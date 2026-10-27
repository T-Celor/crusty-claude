//! Shell-command lifecycle hooks with Claude Code's stdin/stdout/exit-code protocol.

use std::collections::BTreeMap;
use std::env;
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::time::Duration;

use async_trait::async_trait;
use crusty_core::config::HookMatcher;
use crusty_core::hooks::{HookEvent, HookOutcome, HookRunner};
use regex::Regex;
use serde_json::{Map, Value};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::time::timeout;
use tracing::warn;

/// Runs the configured shell commands for each lifecycle event.
#[derive(Debug)]
pub struct ShellHooks {
    /// Event, anchored matcher regex, and the hook itself, in config order.
    hooks: Vec<(HookEvent, Regex, HookMatcher)>,
    cwd: PathBuf,
    session_id: String,
}

impl ShellHooks {
    /// Builds the runner from the `[hooks]` config table. Unknown event names and invalid
    /// matcher regexes are errors. An empty matcher matches every tool. A non-empty matcher must
    /// match the whole tool name, so `Edit|Write` matches `Edit` but not `MultiEdit`.
    pub fn from_config(map: &BTreeMap<String, Vec<HookMatcher>>, cwd: &Path, session_id: &str) -> Result<Self, String> {
        let mut hooks = Vec::new();
        for (name, matchers) in map {
            let event = HookEvent::parse(name).ok_or_else(|| format!("unknown hook event `{name}`"))?;
            for matcher in matchers {
                let pattern = if matcher.matcher.is_empty() { ".*".to_owned() } else { format!("^(?:{})$", matcher.matcher) };
                let regex = Regex::new(&pattern).map_err(|e| format!("invalid matcher `{}` for {name}: {e}", matcher.matcher))?;
                hooks.push((event, regex, matcher.clone()));
            }
        }
        Ok(ShellHooks { hooks, cwd: cwd.to_path_buf(), session_id: session_id.to_owned() })
    }

    /// The JSON written to a hook's stdin: the payload fields plus the core fields. The core
    /// fields win on a clash.
    fn stdin_json(&self, event: HookEvent, tool: Option<&str>, payload: &Value) -> String {
        let mut map = match payload {
            Value::Object(map) => map.clone(),
            _ => Map::new(),
        };
        map.insert("hook_event_name".into(), event.name().into());
        map.insert("session_id".into(), self.session_id.clone().into());
        map.insert("cwd".into(), self.cwd.display().to_string().into());
        if let Some(tool) = tool {
            map.insert("tool_name".into(), tool.into());
        }
        Value::Object(map).to_string()
    }
}

#[async_trait]
impl HookRunner for ShellHooks {
    async fn run(&self, event: HookEvent, tool: Option<&str>, payload: &Value) -> HookOutcome {
        let input = self.stdin_json(event, tool, payload);
        let tool_event = matches!(event, HookEvent::PreToolUse | HookEvent::PostToolUse);
        let mut outcome = HookOutcome::default();
        let mut messages = Vec::new();
        let mut contexts = Vec::new();
        for (hook_event, regex, matcher) in &self.hooks {
            if *hook_event != event || (tool_event && !regex.is_match(tool.unwrap_or_default())) {
                continue;
            }
            let reply = match execute(matcher, &self.cwd, &input).await {
                Ok(output) => interpret(event, &matcher.command, &output),
                // A PreToolUse hook that cannot finish must not let the tool run unchecked.
                Err(error) if event == HookEvent::PreToolUse => {
                    Reply { block: Some(format!("PreToolUse hook `{}` failed or timed out: {error}", matcher.command)), ..Reply::default() }
                }
                Err(error) => {
                    warn!(command = %matcher.command, %error, "hook did not finish; ignoring it");
                    Reply::default()
                }
            };
            if let Some(message) = reply.block {
                outcome.block = true;
                messages.push(message);
            }
            if let Some(context) = reply.context {
                contexts.push(context);
            }
            if reply.updated_input.is_some() {
                outcome.updated_input = reply.updated_input;
            }
        }
        if !messages.is_empty() {
            outcome.message = Some(messages.join("\n"));
        }
        if !contexts.is_empty() {
            outcome.additional_context = Some(contexts.join("\n"));
        }
        outcome
    }
}

/// What one hook asked for.
#[derive(Debug, Default)]
struct Reply {
    block: Option<String>,
    context: Option<String>,
    updated_input: Option<Value>,
}

/// Runs one hook command with `input` on stdin, killed if it exceeds its timeout.
async fn execute(matcher: &HookMatcher, cwd: &Path, input: &str) -> Result<Output, String> {
    let (program, flag) = shell();
    let mut child = Command::new(&program)
        .arg(flag)
        .arg(&matcher.command)
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("could not start `{}`: {e}", program.display()))?;
    let stdin = child.stdin.take();
    let finished = async move {
        if let Some(mut stdin) = stdin {
            // A hook may exit without reading its input; the broken pipe is not an error.
            let _ = stdin.write_all(input.as_bytes()).await;
        }
        child.wait_with_output().await
    };
    match timeout(Duration::from_secs(matcher.timeout_secs), finished).await {
        Ok(result) => result.map_err(|e| format!("could not wait for hook: {e}")),
        Err(_) => Err(format!("timed out after {} s", matcher.timeout_secs)),
    }
}

/// Exit 0 reads stdout, exit 2 blocks with stderr. For `PreToolUse` every other status (including
/// a signal) also blocks, so a crashing guard cannot let the tool run. Other events log and ignore it.
fn interpret(event: HookEvent, command: &str, output: &Output) -> Reply {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    match output.status.code() {
        Some(0) => interpret_stdout(event, stdout.trim()),
        Some(2) => {
            let message = stderr.trim();
            let block = if message.is_empty() { format!("blocked by hook `{command}`") } else { message.to_owned() };
            Reply { block: Some(block), ..Reply::default() }
        }
        _ if event == HookEvent::PreToolUse => {
            let message = stderr.trim();
            let block = if message.is_empty() { format!("hook exited with {}", output.status) } else { message.to_owned() };
            Reply { block: Some(block), ..Reply::default() }
        }
        code => {
            warn!(%command, ?code, stderr = %stderr.trim(), "hook exited with an error; ignoring it");
            Reply::default()
        }
    }
}

/// A JSON object on stdout is a decision. Plain text is context, but only for the events
/// whose context is read: `UserPromptSubmit` and `SessionStart`.
fn interpret_stdout(event: HookEvent, stdout: &str) -> Reply {
    if stdout.is_empty() {
        return Reply::default();
    }
    match serde_json::from_str::<Value>(stdout) {
        Ok(Value::Object(object)) => from_json(event, &object),
        _ => match event {
            HookEvent::UserPromptSubmit | HookEvent::SessionStart => Reply { context: Some(stdout.to_owned()), ..Reply::default() },
            _ => Reply::default(),
        },
    }
}

fn from_json(event: HookEvent, object: &Map<String, Value>) -> Reply {
    let text = |value: Option<&Value>| value.and_then(Value::as_str).map(str::to_owned);
    let mut reply = Reply::default();
    if object.get("decision").and_then(Value::as_str) == Some("block") {
        reply.block = Some(text(object.get("reason")).unwrap_or_else(|| "blocked by hook".into()));
    }
    // `continue: false` stops only the events that can be stopped.
    let stoppable = matches!(event, HookEvent::PreToolUse | HookEvent::UserPromptSubmit | HookEvent::Stop | HookEvent::SubagentStop);
    if stoppable && object.get("continue").and_then(Value::as_bool) == Some(false) {
        reply.block = Some(text(object.get("stopReason")).unwrap_or_else(|| "stopped by hook".into()));
    }
    if let Some(specific) = object.get("hookSpecificOutput").and_then(Value::as_object) {
        if specific.get("permissionDecision").and_then(Value::as_str) == Some("deny") {
            reply.block = Some(text(specific.get("permissionDecisionReason")).unwrap_or_else(|| "denied by hook".into()));
        }
        reply.context = text(specific.get("additionalContext"));
        reply.updated_input = specific.get("updatedInput").filter(|v| v.is_object()).cloned();
    }
    reply
}

/// `(program, flag)` for running a command string: `bash -c` when bash is on PATH, otherwise
/// `sh -c` on Unix and `cmd /C` on Windows.
fn shell() -> (PathBuf, &'static str) {
    if let Some(bash) = find_on_path("bash") {
        (bash, "-c")
    } else if cfg!(unix) {
        (PathBuf::from("sh"), "-c")
    } else {
        (PathBuf::from("cmd"), "/C")
    }
}

/// First executable named `program` on PATH. On Windows the System32 directory is skipped:
/// its `bash.exe` is the WSL launcher, which cannot run Windows-side hook commands.
fn find_on_path(program: &str) -> Option<PathBuf> {
    let paths = env::var_os("PATH")?;
    env::split_paths(&paths).filter(|dir| !(cfg!(windows) && is_system32(dir))).find_map(|dir| {
        let plain = dir.join(program);
        if cfg!(windows) {
            let exe = dir.join(format!("{program}.exe"));
            if exe.is_file() {
                return Some(exe);
            }
        }
        plain.is_file().then_some(plain)
    })
}

fn is_system32(dir: &Path) -> bool {
    dir.to_string_lossy().to_ascii_lowercase().contains("system32")
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use serde_json::json;

    use super::*;

    fn matcher(pattern: &str, command: &str) -> HookMatcher {
        HookMatcher { matcher: pattern.into(), command: command.into(), timeout_secs: 10 }
    }

    fn config(event: &str, matchers: Vec<HookMatcher>) -> BTreeMap<String, Vec<HookMatcher>> {
        BTreeMap::from([(event.to_owned(), matchers)])
    }

    /// Returns false (and prints a note) when bash is missing, so the test is skipped.
    fn bash_available() -> bool {
        if find_on_path("bash").is_some() {
            return true;
        }
        eprintln!("skipping: bash not found on PATH");
        false
    }

    fn hooks(map: &BTreeMap<String, Vec<HookMatcher>>) -> ShellHooks {
        ShellHooks::from_config(map, Path::new("."), "sess-1").unwrap()
    }

    #[test]
    fn rejects_unknown_event_and_bad_regex() {
        let unknown = config("NoSuchEvent", vec![matcher("", "true")]);
        assert!(ShellHooks::from_config(&unknown, Path::new("."), "s").unwrap_err().contains("unknown hook event"));
        let bad = config("PreToolUse", vec![matcher("(", "true")]);
        assert!(ShellHooks::from_config(&bad, Path::new("."), "s").unwrap_err().contains("invalid matcher"));
    }

    #[tokio::test]
    async fn exit_zero_json_block_blocks() {
        if !bash_available() {
            return;
        }
        let runner = hooks(&config("PreToolUse", vec![matcher("", r#"echo '{"decision":"block","reason":"nope"}'"#)]));
        let outcome = runner.run(HookEvent::PreToolUse, Some("Bash"), &json!({"tool_input": {}})).await;
        assert!(outcome.block);
        assert_eq!(outcome.message.as_deref(), Some("nope"));
    }

    #[tokio::test]
    async fn exit_two_blocks_with_stderr() {
        if !bash_available() {
            return;
        }
        let runner = hooks(&config("PreToolUse", vec![matcher("", "echo forbidden >&2; exit 2")]));
        let outcome = runner.run(HookEvent::PreToolUse, Some("Bash"), &json!({})).await;
        assert!(outcome.block);
        assert_eq!(outcome.message.as_deref(), Some("forbidden"));
    }

    #[tokio::test]
    async fn pre_tool_use_blocks_on_any_status_other_than_zero_and_two() {
        if !bash_available() {
            return;
        }
        let runner = hooks(&config("PreToolUse", vec![matcher("", "echo oops >&2; exit 1")]));
        let outcome = runner.run(HookEvent::PreToolUse, Some("Bash"), &json!({})).await;
        assert!(outcome.block);
        assert_eq!(outcome.message.as_deref(), Some("oops"));

        // A signal has no exit code; the hook is still a failed guard and blocks.
        let killed = hooks(&config("PreToolUse", vec![matcher("", "kill -9 $$")]));
        let outcome = killed.run(HookEvent::PreToolUse, Some("Bash"), &json!({})).await;
        assert!(outcome.block);
        assert!(outcome.message.as_deref().is_some_and(|m| m.starts_with("hook exited with")));

        // Other events keep ignoring a failing hook.
        let prompt = hooks(&config("UserPromptSubmit", vec![matcher("", "exit 1")]));
        let outcome = prompt.run(HookEvent::UserPromptSubmit, None, &json!({"prompt": "hi"})).await;
        assert!(!outcome.block);
    }

    #[tokio::test]
    async fn plain_stdout_is_context_only_for_prompt_and_session_events() {
        if !bash_available() {
            return;
        }
        let runner = hooks(&config("UserPromptSubmit", vec![matcher("", "echo extra context")]));
        let outcome = runner.run(HookEvent::UserPromptSubmit, None, &json!({"prompt": "hi"})).await;
        assert_eq!(outcome.additional_context.as_deref(), Some("extra context"));
        assert!(!outcome.block);

        let pre = hooks(&config("PreToolUse", vec![matcher("", "echo ignored")]));
        let outcome = pre.run(HookEvent::PreToolUse, Some("Bash"), &json!({})).await;
        assert_eq!(outcome.additional_context, None);
    }

    #[tokio::test]
    async fn matcher_filters_by_tool_name() {
        if !bash_available() {
            return;
        }
        let runner = hooks(&config("PreToolUse", vec![matcher("Bash|Edit", "exit 2")]));
        let read = runner.run(HookEvent::PreToolUse, Some("Read"), &json!({})).await;
        assert!(!read.block);
        let edit = runner.run(HookEvent::PreToolUse, Some("Edit"), &json!({})).await;
        assert!(edit.block);
        let multi = runner.run(HookEvent::PreToolUse, Some("MultiEdit"), &json!({})).await;
        assert!(!multi.block);
    }

    #[tokio::test]
    async fn pre_tool_use_timeout_blocks() {
        if !bash_available() {
            return;
        }
        let slow = HookMatcher { matcher: String::new(), command: "sleep 5".into(), timeout_secs: 1 };
        let runner = hooks(&config("PreToolUse", vec![slow]));
        let started = Instant::now();
        let outcome = runner.run(HookEvent::PreToolUse, Some("Bash"), &json!({})).await;
        assert!(outcome.block);
        assert_eq!(outcome.message.as_deref(), Some("PreToolUse hook `sleep 5` failed or timed out: timed out after 1 s"));
        assert!(started.elapsed() < Duration::from_secs(4));
    }

    #[tokio::test]
    async fn updated_input_is_returned() {
        if !bash_available() {
            return;
        }
        let command = r#"echo '{"hookSpecificOutput":{"hookEventName":"PreToolUse","updatedInput":{"command":"ls -la"}}}'"#;
        let runner = hooks(&config("PreToolUse", vec![matcher("Bash", command)]));
        let outcome = runner.run(HookEvent::PreToolUse, Some("Bash"), &json!({"command": "ls"})).await;
        assert_eq!(outcome.updated_input, Some(json!({"command": "ls -la"})));
        assert!(!outcome.block);
    }

    #[tokio::test]
    async fn stdin_carries_event_and_session_fields() {
        if !bash_available() {
            return;
        }
        let runner = hooks(&config("UserPromptSubmit", vec![matcher("", "grep -o UserPromptSubmit")]));
        let outcome = runner.run(HookEvent::UserPromptSubmit, None, &json!({"prompt": "x"})).await;
        assert_eq!(outcome.additional_context.as_deref(), Some("UserPromptSubmit"));
    }

    #[test]
    fn json_deny_and_continue_false_block() {
        let deny = from_json(
            HookEvent::PreToolUse,
            json!({"hookSpecificOutput": {"permissionDecision": "deny", "permissionDecisionReason": "no"}}).as_object().unwrap(),
        );
        assert_eq!(deny.block.as_deref(), Some("no"));
        let stop = from_json(HookEvent::PreToolUse, json!({"continue": false, "stopReason": "halt"}).as_object().unwrap());
        assert_eq!(stop.block.as_deref(), Some("halt"));
        let post = from_json(HookEvent::PostToolUse, json!({"continue": false, "stopReason": "halt"}).as_object().unwrap());
        assert_eq!(post.block, None);
    }
}
