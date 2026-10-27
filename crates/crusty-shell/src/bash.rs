//! The `Bash` tool: runs one shell command in the workspace with a secret-free environment.

use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crusty_core::async_trait;
use crusty_core::event::{AppEvent, Stage, StageMetrics, StageStatus};
use crusty_core::filter::choose_filtered;
use crusty_core::message::ToolSpec;
use crusty_core::permission::{RiskAssessment, RiskLevel};
use crusty_core::tokens::estimate_text;
use crusty_core::tool::{Tool, ToolCategory, ToolContext, ToolError, ToolOutput, input_opt_str, input_str};
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;

use crate::background::BackgroundShells;
use crate::env::scrubbed_env;
use crate::risk::{ClassifyEnv, classify_command_in};
use crate::rtk_external::{find_binary, rtk_rewrite};
use crate::sandbox::{self, SandboxSetup};
use crate::shell::{ShellSpec, detect_shell};

/// Output kept in memory per stream while a command runs. Beyond this, bytes are counted
/// but discarded.
// shortcut: 64 MiB per stream is held in memory and the rest is only counted, upgrade to spilling to a temp file
const RETAIN_LIMIT: usize = 64 * 1024 * 1024;

/// Runs shell commands for the model. Background commands are registered with `shells`, and
/// every command is confined by `sandbox` when it is given.
pub struct BashTool {
    shells: Arc<BackgroundShells>,
    sandbox: Option<Arc<SandboxSetup>>,
}

impl BashTool {
    /// A Bash tool whose background commands live in `shells`, confined by `sandbox` when given.
    pub fn new(shells: Arc<BackgroundShells>, sandbox: Option<Arc<SandboxSetup>>) -> Self {
        BashTool { shells, sandbox }
    }
}

/// Whether the call asks to run outside the sandbox (`dangerouslyDisableSandbox`).
fn wants_unsandboxed(input: &Value) -> bool {
    input.get("dangerouslyDisableSandbox").and_then(Value::as_bool).unwrap_or(false)
}

#[async_trait]
impl Tool for BashTool {
    fn name(&self) -> &str {
        "Bash"
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "Bash",
            "Runs a shell command in the project directory and returns its output and exit code. \
             Set run_in_background to start a long-running command and read it later with BashOutput.",
            json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string", "description": "The command to run."},
                    "timeout": {"type": "integer", "description": "Timeout in milliseconds."},
                    "description": {"type": "string", "description": "What the command does, in a few words."},
                    "run_in_background": {"type": "boolean", "description": "Start the command and return at once."},
                    "dangerouslyDisableSandbox": {"type": "boolean", "description": "Run the command outside the sandbox. Asked for approval every time, and refused when unsandboxed runs are disabled."}
                },
                "required": ["command"]
            }),
        )
    }

    fn category(&self) -> ToolCategory {
        ToolCategory::Execute
    }

    fn assess(&self, input: &Value, ctx: &ToolContext) -> RiskAssessment {
        let mut risk = match input_str(input, "command") {
            Ok(command) => classify_command_in(command, &classify_env(ctx)),
            Err(_) => RiskAssessment::new(RiskLevel::High, "no command given"),
        };
        if wants_unsandboxed(input) && ctx.config.sandbox.allow_unsandboxed_requests {
            // Critical is always asked, whatever the mode and rules.
            if risk.level < RiskLevel::Critical {
                risk = RiskAssessment::new(RiskLevel::Critical, "runs outside the sandbox");
            } else {
                risk.reason = format!("{}; runs outside the sandbox", risk.reason);
            }
        }
        risk
    }

    fn permission_subject(&self, input: &Value) -> Option<String> {
        input_opt_str(input, "command").map(str::to_owned)
    }

    fn summarize(&self, input: &Value) -> String {
        let line = input_opt_str(input, "command").and_then(|c| c.lines().next()).unwrap_or("").trim();
        if line.chars().count() > 80 { format!("Bash({}…)", line.chars().take(79).collect::<String>()) } else { format!("Bash({line})") }
    }

    async fn call(&self, input: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let command = input_str(&input, "command")?;
        if command.trim().is_empty() {
            return Err(ToolError::InvalidInput("command must not be empty".into()));
        }
        let unsandboxed = wants_unsandboxed(&input);
        if unsandboxed && !ctx.config.sandbox.allow_unsandboxed_requests {
            return Ok(ToolOutput::error("unsandboxed runs are disabled by [sandbox] allow_unsandboxed_requests"));
        }
        let limits = &ctx.config.security;
        let timeout_ms = match input.get("timeout") {
            None | Some(Value::Null) => limits.bash_default_timeout_secs.saturating_mul(1000),
            Some(v) => v.as_u64().ok_or_else(|| ToolError::InvalidInput("timeout must be a whole number of milliseconds".into()))?,
        }
        .min(limits.bash_max_timeout_secs.saturating_mul(1000))
        .max(1);
        let background = input.get("run_in_background").and_then(Value::as_bool).unwrap_or(false);
        let shell = detect_shell(std::env::var("CRUSTY_SHELL").ok().as_deref());

        let rtk = &ctx.config.optimizer.rtk;
        // Commands above Moderate run as written: the external filter never sees them.
        let risky = classify_command_in(command, &classify_env(ctx)).level > RiskLevel::Moderate;
        let external = (rtk.enabled && rtk.prefer_external && !background && !risky)
            .then(|| find_binary(&rtk.external_binary).and_then(|_| rtk_rewrite(command, &rtk.external_binary)))
            .flatten();
        let run = external.as_deref().unwrap_or(command);

        let launch =
            sandbox::launch(self.sandbox.as_deref(), ctx, unsandboxed, &shell.program.to_string_lossy(), &shell_argv(&shell, run))?;
        let mut cmd = build_command(&launch.program, &launch.args, &ctx.workspace, process_env(), &limits.secret_env_patterns);
        let protected = launch.protect();
        if background {
            let id = self.shells.spawn(cmd, limits.bash_max_timeout_secs, &launch, protected)?;
            return Ok(ToolOutput::text(format!("Started background shell {id}. Use BashOutput with bash_id={id}.")));
        }

        let started = Instant::now();
        let mut child = cmd.spawn()?;
        let _keep = launch.adopt(&child)?;
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let outcome = tokio::select! {
            done = async { tokio::join!(read_capped(stdout), read_capped(stderr), child.wait()) } => Some(done),
            _ = tokio::time::sleep(Duration::from_millis(timeout_ms)) => None,
            _ = ctx.cancel.cancelled() => None,
        };
        let Some((out, err, status)) = outcome else {
            let _ = child.kill().await;
            sandbox::settle(protected);
            return Err(if ctx.cancel.is_cancelled() { ToolError::Cancelled } else { ToolError::Timeout(timeout_ms.div_ceil(1000)) });
        };
        let settled = sandbox::settle(protected);
        let code = status?.code();

        let mut body = String::from_utf8_lossy(&out.bytes).into_owned();
        if !err.bytes.is_empty() {
            body.push_str("\n[stderr]\n");
            body.push_str(&String::from_utf8_lossy(&err.bytes));
        }
        let discarded = out.dropped + err.dropped;
        if discarded > 0 {
            body.push_str(&format!("\n[{discarded} bytes discarded beyond the retention limit]"));
        }
        if body.is_empty() {
            body.push_str("(no output)");
        }
        let cap = limits.max_tool_output_bytes;
        let mut text = if body.len() > cap {
            let total = body.len();
            let head = truncate_bytes(&body, cap).to_owned();
            let handle = ctx.store.put(body);
            format!("{head}\n[output truncated: showing {cap} of {total} bytes; full output: Retrieve(handle=\"{handle}\")]")
        } else {
            body
        };
        match code {
            Some(0) => {}
            Some(n) => text.push_str(&format!("\n[exit code {n}]")),
            None => text.push_str("\n[terminated by signal]"),
        }

        let word = command.split_whitespace().next().unwrap_or("command");
        if external.is_some() {
            let est = estimate_text(&text);
            let mut m = StageMetrics::new(Stage::Rtk, format!("tool_output:{word}"), format!("rtk-external:{word}"));
            m.status = StageStatus::Applied;
            m.detail = "external rtk (savings not measured)".into();
            m.input_bytes = text.len() as u64;
            m.output_bytes = text.len() as u64;
            m.tokens_before_est = est;
            m.tokens_after_est = est;
            m.session_id.clone_from(&ctx.session_id);
            m.agent_id.clone_from(&ctx.agent_id);
            m.duration_us = started.elapsed().as_micros() as u64;
            ctx.events.emit(AppEvent::Optimizer(m));
        } else if rtk.enabled {
            let filtered = ctx.filters.command_output(command, &text, code);
            text = choose_filtered(ctx, Stage::Rtk, &format!("tool_output:{word}"), &text, filtered, started);
        }

        let mut is_error = code != Some(0);
        if let Some(settled) = settled {
            text.push('\n');
            text.push_str(&settled.message);
            is_error |= !settled.restored.is_empty();
        }
        Ok(ToolOutput { is_error, ..ToolOutput::text(text) }.with_exit_code(code))
    }
}

/// The classifier's view of a call: its working directory and the configured protected paths.
fn classify_env(ctx: &ToolContext) -> ClassifyEnv<'_> {
    ClassifyEnv {
        workspace: Some(ctx.workspace.as_path()),
        protected: ctx.config.security.protected_paths.as_slice(),
        write_protected: ctx.config.security.write_protected_paths.as_slice(),
    }
}

/// The shell's arguments followed by `run`: what is passed to `shell.program`.
fn shell_argv(shell: &ShellSpec, run: &str) -> Vec<String> {
    shell.args_before_command.iter().cloned().chain([run.to_owned()]).collect()
}

/// Builds the command `program args` with an explicit environment: `vars` minus the names
/// matching `secret_patterns`, plus `CRUSTY=1`.
pub(crate) fn build_command(
    program: &str,
    args: &[String],
    workspace: &Path,
    vars: impl Iterator<Item = (String, String)>,
    secret_patterns: &[String],
) -> Command {
    let mut cmd = Command::new(program);
    cmd.args(args)
        .current_dir(workspace)
        .env_clear()
        .envs(scrubbed_env(vars, secret_patterns))
        .env("CRUSTY", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    cmd
}

/// The current process environment, skipping variables that are not valid Unicode.
fn process_env() -> impl Iterator<Item = (String, String)> {
    std::env::vars_os().filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
}

#[derive(Default)]
struct Captured {
    bytes: Vec<u8>,
    dropped: u64,
}

/// Reads a stream to its end, keeping up to `RETAIN_LIMIT` bytes.
async fn read_capped<R: AsyncRead + Unpin>(reader: Option<R>) -> Captured {
    let Some(mut reader) = reader else {
        return Captured::default();
    };
    let mut out = Captured::default();
    let mut chunk = [0u8; 8192];
    loop {
        match reader.read(&mut chunk).await {
            Ok(0) | Err(_) => return out,
            Ok(n) => {
                let keep = n.min(RETAIN_LIMIT.saturating_sub(out.bytes.len()));
                out.bytes.extend_from_slice(&chunk[..keep]);
                out.dropped += (n - keep) as u64;
            }
        }
    }
}

/// The longest prefix of at most `cap` bytes that ends on a character boundary.
fn truncate_bytes(s: &str, cap: usize) -> &str {
    let mut end = cap.min(s.len());
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::background::shell_tools;
    use crusty_core::Config;

    fn tool() -> BashTool {
        BashTool::new(Arc::new(BackgroundShells::default()), None)
    }

    fn ctx_in(dir: &Path) -> ToolContext {
        ToolContext::for_tests(dir)
    }

    fn sh() -> ShellSpec {
        detect_shell(None)
    }

    #[tokio::test]
    async fn runs_echo_and_returns_stdout() {
        let dir = tempfile::tempdir().unwrap();
        let out = tool().call(json!({"command": "echo hello"}), &ctx_in(dir.path())).await.unwrap();
        assert_eq!(out.text_content().trim(), "hello");
        assert!(!out.is_error);
        assert_eq!(out.exit_code, Some(0));
        assert_eq!(out.preview, "hello");
    }

    #[tokio::test]
    async fn non_zero_exit_is_an_error_with_code() {
        let dir = tempfile::tempdir().unwrap();
        let out = tool().call(json!({"command": "echo oops >&2; exit 3"}), &ctx_in(dir.path())).await.unwrap();
        assert!(out.is_error);
        assert_eq!(out.exit_code, Some(3));
        let text = out.text_content();
        assert!(text.contains("[stderr]\noops"), "{text}");
        assert!(text.contains("[exit code 3]"), "{text}");
    }

    #[tokio::test]
    async fn timeout_kills_the_command() {
        let dir = tempfile::tempdir().unwrap();
        let err = tool().call(json!({"command": "exec sleep 5", "timeout": 300}), &ctx_in(dir.path())).await.unwrap_err();
        assert!(matches!(err, ToolError::Timeout(1)), "{err:?}");
    }

    #[tokio::test]
    async fn cancellation_stops_the_command() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx_in(dir.path());
        let token = ctx.cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            token.cancel();
        });
        let err = tool().call(json!({"command": "exec sleep 5"}), &ctx).await.unwrap_err();
        assert_eq!(err, ToolError::Cancelled);
    }

    #[tokio::test]
    async fn empty_command_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let err = tool().call(json!({"command": "  "}), &ctx_in(dir.path())).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidInput(_)));
    }

    #[tokio::test]
    async fn long_output_is_truncated_and_kept_whole() {
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = ctx_in(dir.path());
        let mut config = Config::default();
        config.security.max_tool_output_bytes = 64;
        ctx.config = Arc::new(config);
        let command = "i=0; while [ $i -lt 300 ]; do printf a; i=$((i+1)); done";
        let out = tool().call(json!({"command": command}), &ctx).await.unwrap();
        let text = out.text_content();
        assert!(text.starts_with(&"a".repeat(64)), "{text}");
        assert!(text.contains("Retrieve(handle=\"orig_"), "{text}");
        assert_eq!(ctx.store.len(), 1);
        let handle = text.split("handle=\"").nth(1).unwrap().split('"').next().unwrap();
        assert_eq!(ctx.store.get(handle).unwrap().len(), 300);
    }

    #[tokio::test]
    async fn child_sees_no_secrets_and_gets_crusty_marker() {
        let dir = tempfile::tempdir().unwrap();
        let shell = sh();
        let patterns: Vec<String> = Config::default().security.secret_env_patterns;
        let path = std::env::var("PATH").unwrap_or_default();
        let vars = vec![
            ("PATH".to_owned(), path),
            ("ANTHROPIC_API_KEY".to_owned(), "sk-test".to_owned()),
            ("GITHUB_TOKEN".to_owned(), "ghp-test".to_owned()),
        ];
        let mut cmd = build_command(
            &shell.program.to_string_lossy(),
            &shell_argv(&shell, "echo \"[$ANTHROPIC_API_KEY][$GITHUB_TOKEN][$CRUSTY]\""),
            dir.path(),
            vars.into_iter(),
            &patterns,
        );
        let output = cmd.output().await.unwrap();
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "[][][1]");
    }

    #[tokio::test]
    async fn background_shell_start_read_and_kill() {
        let dir = tempfile::tempdir().unwrap();
        let shells = Arc::new(BackgroundShells::default());
        let tools = shell_tools(shells.clone(), None);
        let ctx = ctx_in(dir.path());
        let started =
            tools[0].call(json!({"command": "echo ready; exec sleep 30", "run_in_background": true}), &ctx).await.unwrap().text_content();
        let id = started.split_whitespace().nth(3).unwrap().trim_end_matches('.').to_owned();
        assert!(id.starts_with("bash_"), "{started}");

        let mut seen = String::new();
        for _ in 0..250 {
            seen = tools[1].call(json!({"bash_id": id}), &ctx).await.unwrap().text_content();
            if seen.contains("ready") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(seen.contains("ready"), "{seen}");
        assert!(seen.contains("[status: running]"), "{seen}");

        let killed = tools[2].call(json!({"shell_id": id}), &ctx).await.unwrap().text_content();
        assert!(killed.contains("killed"), "{killed}");
        let after = tools[1].call(json!({"bash_id": id}), &ctx).await.unwrap().text_content();
        assert!(after.contains("[status: killed]"), "{after}");
    }

    #[test]
    fn summary_is_first_line_capped() {
        let long = format!("{}\nsecond", "x".repeat(200));
        let s = tool().summarize(&json!({"command": long}));
        assert!(s.starts_with("Bash(xxx") && s.ends_with("…)"), "{s}");
        assert_eq!(s.chars().count(), "Bash(".len() + 80 + 1);
        assert_eq!(tool().summarize(&json!({"command": "ls -la"})), "Bash(ls -la)");
    }

    #[test]
    fn assess_uses_the_command() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx_in(dir.path());
        assert_eq!(tool().assess(&json!({"command": "rm -rf /"}), &ctx).level, RiskLevel::Critical);
        assert_eq!(tool().assess(&json!({}), &ctx).level, RiskLevel::High);
    }

    #[test]
    fn unsandboxed_request_is_always_critical() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx_in(dir.path());
        let risk = tool().assess(&json!({"command": "ls", "dangerouslyDisableSandbox": true}), &ctx);
        assert_eq!(risk.level, RiskLevel::Critical);
        assert_eq!(risk.reason, "runs outside the sandbox");
        let critical = tool().assess(&json!({"command": "rm -rf /", "dangerouslyDisableSandbox": true}), &ctx);
        assert_eq!(critical.level, RiskLevel::Critical);
        assert!(critical.reason.ends_with("runs outside the sandbox"));
    }

    #[tokio::test]
    async fn unsandboxed_request_is_refused_when_not_allowed() {
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = ctx_in(dir.path());
        let mut config = Config::default();
        config.sandbox.allow_unsandboxed_requests = false;
        ctx.config = Arc::new(config);
        let out = tool().call(json!({"command": "echo hi", "dangerouslyDisableSandbox": true}), &ctx).await.unwrap();
        assert!(out.is_error);
        assert!(out.text_content().contains("allow_unsandboxed_requests"), "{}", out.text_content());
    }

    #[tokio::test]
    async fn bubblewrap_setup_wraps_the_call_and_fails_when_unavailable() {
        use crate::sandbox::SandboxSetup;
        use crusty_sandbox::{Backend, Detection};
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx_in(dir.path());
        let setup = SandboxSetup {
            config: crusty_core::config::SandboxConfig { fail_if_unavailable: true, ..Default::default() },
            detection: Detection {
                backend: Backend::None,
                available: false,
                confines_writes: false,
                blocks_network: false,
                detail: "test".into(),
            },
            self_exe: std::path::PathBuf::from("/x"),
        };
        let err = BashTool::new(Arc::new(BackgroundShells::default()), Some(Arc::new(setup)))
            .call(json!({"command": "echo hi"}), &ctx)
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Failed(_)), "{err:?}");
    }

    #[test]
    fn wrapped_command_line_for_bubblewrap() {
        use crate::sandbox::{SandboxSetup, launch};
        use crusty_sandbox::{Backend, Detection};
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx_in(dir.path());
        let setup = SandboxSetup {
            config: crusty_core::config::SandboxConfig::default(),
            detection: Detection {
                backend: Backend::Bubblewrap,
                available: true,
                confines_writes: true,
                blocks_network: true,
                detail: "bwrap".into(),
            },
            self_exe: std::path::PathBuf::from("/x"),
        };
        let argv = shell_argv(&sh(), "echo hi");
        let l = launch(Some(&setup), &ctx, false, "/bin/bash", &argv).unwrap();
        assert_eq!(l.program, "bwrap");
        assert_eq!(l.args.last().map(String::as_str), Some("echo hi"));
        assert!(l.args.contains(&"--bind".to_owned()));
    }

    #[test]
    fn off_mode_leaves_command_untouched() {
        use crate::sandbox::{SandboxSetup, launch};
        use crusty_sandbox::{Backend, Detection};
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx_in(dir.path());
        let mut config = crusty_core::config::SandboxConfig::default();
        config.mode = crusty_core::config::SandboxMode::Off;
        let setup = SandboxSetup {
            config,
            detection: Detection {
                backend: Backend::Bubblewrap,
                available: true,
                confines_writes: true,
                blocks_network: true,
                detail: "bwrap".into(),
            },
            self_exe: std::path::PathBuf::from("/x"),
        };
        let argv = shell_argv(&sh(), "echo hi");
        let l = launch(Some(&setup), &ctx, false, "/bin/bash", &argv).unwrap();
        assert_eq!(l.program, "/bin/bash");
        assert_eq!(l.args, argv);
    }
}
