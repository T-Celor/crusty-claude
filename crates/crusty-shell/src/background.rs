//! Background shells: commands started with `run_in_background`, read with `BashOutput` and
//! stopped with `KillShell`.

use std::collections::{HashMap, VecDeque};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use crusty_core::message::ToolSpec;
use crusty_core::permission::{RiskAssessment, RiskLevel};
use crusty_core::tool::{Tool, ToolCategory, ToolContext, ToolError, ToolOutput, input_opt_str, input_str};
use crusty_core::{CancellationToken, async_trait};
use regex::Regex;
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::{Child, Command};
use tokio::sync::watch;

use crate::bash::BashTool;
use crate::sandbox::{Keep, Launch, Protected, SandboxSetup, settle};

/// Newest output kept per background shell, in bytes.
const OUTPUT_LIMIT: usize = 1024 * 1024;

/// Background shells that may run at once.
const MAX_RUNNING: usize = 8;

/// Where a background shell stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellStatus {
    Running,
    Exited(Option<i32>),
    Killed,
}

#[derive(Default)]
struct Output {
    bytes: VecDeque<u8>,
    /// Bytes ever received, so a read cursor survives trimming.
    total: u64,
    read_pos: u64,
}

struct Shell {
    output: Mutex<Output>,
    status: Mutex<ShellStatus>,
    cancel: CancellationToken,
    /// Set to `true` once the process is gone and its status recorded.
    done: watch::Sender<bool>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Shell {
    fn append(&self, bytes: &[u8]) {
        let mut out = lock(&self.output);
        out.bytes.extend(bytes);
        out.total += bytes.len() as u64;
        let excess = out.bytes.len().saturating_sub(OUTPUT_LIMIT);
        out.bytes.drain(..excess);
    }

    /// Up to `max` bytes of output since the last call (on a character boundary), how many bytes
    /// were trimmed away before it could be read, and whether more output is left for the next call.
    fn take_new(&self, max: usize) -> (String, u64, bool) {
        let mut out = lock(&self.output);
        let buf_start = out.total - out.bytes.len() as u64;
        let skipped = buf_start.saturating_sub(out.read_pos);
        let from = (out.read_pos.max(buf_start) - buf_start) as usize;
        let mut end = out.bytes.len().min(from.saturating_add(max));
        // Never split a UTF-8 sequence: `max` is at least 4 bytes, so this always makes progress.
        if end < out.bytes.len() {
            while end > from && (out.bytes[end] & 0xC0) == 0x80 {
                end -= 1;
            }
        }
        let text = String::from_utf8_lossy(&out.bytes.range(from..end).copied().collect::<Vec<u8>>()).into_owned();
        out.read_pos = buf_start + end as u64;
        (text, skipped, end < out.bytes.len())
    }

    fn status(&self) -> ShellStatus {
        *lock(&self.status)
    }

    fn status_text(&self) -> String {
        match self.status() {
            ShellStatus::Running => "running".into(),
            ShellStatus::Exited(Some(code)) => format!("exited {code}"),
            ShellStatus::Exited(None) => "exited (terminated by signal)".into(),
            ShellStatus::Killed => "killed".into(),
        }
    }
}

/// Registry of background shells, shared by the tools that start and read them.
#[derive(Default)]
pub struct BackgroundShells {
    next_id: AtomicU64,
    shells: Mutex<HashMap<String, Arc<Shell>>>,
}

impl BackgroundShells {
    /// Starts `command` in the background and returns its id (`bash_<n>`). Its stdout and
    /// stderr are read continuously into a bounded buffer until it exits or is killed; it is
    /// killed after `max_secs` seconds. `launch` confines the child once spawned, and
    /// `protected` (from [`Launch::protect`]) is restored when it ends. Fails when
    /// `MAX_RUNNING` shells are already running.
    pub fn spawn(&self, mut command: Command, max_secs: u64, launch: &Launch, protected: Option<Protected>) -> Result<String, ToolError> {
        let mut shells = lock(&self.shells);
        if shells.values().filter(|s| s.status() == ShellStatus::Running).count() >= MAX_RUNNING {
            return Err(ToolError::InvalidInput(format!(
                "at most {MAX_RUNNING} background shells can run at once; stop one with KillShell first"
            )));
        }
        let mut child = command.stdout(Stdio::piped()).stderr(Stdio::piped()).stdin(Stdio::null()).kill_on_drop(true).spawn()?;
        let keep = launch.adopt(&child)?;
        let id = format!("bash_{}", self.next_id.fetch_add(1, Ordering::Relaxed) + 1);
        let (done, _) = watch::channel(false);
        let shell = Arc::new(Shell {
            output: Mutex::new(Output::default()),
            status: Mutex::new(ShellStatus::Running),
            cancel: CancellationToken::new(),
            done,
        });
        if let Some(out) = child.stdout.take() {
            tokio::spawn(pump(out, shell.clone()));
        }
        if let Some(err) = child.stderr.take() {
            tokio::spawn(pump(err, shell.clone()));
        }
        tokio::spawn(supervise(child, shell.clone(), keep, protected));
        let mut done = shell.done.subscribe();
        let killer = shell.clone();
        tokio::spawn(async move {
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(max_secs)) => killer.cancel.cancel(),
                _ = done.wait_for(|finished| *finished) => {}
            }
        });
        shells.insert(id.clone(), shell);
        Ok(id)
    }

    fn get(&self, id: &str) -> Option<Arc<Shell>> {
        lock(&self.shells).get(id).cloned()
    }
}

async fn pump<R: AsyncRead + Unpin>(mut reader: R, shell: Arc<Shell>) {
    let mut chunk = [0u8; 4096];
    while let Ok(n) = reader.read(&mut chunk).await {
        if n == 0 {
            break;
        }
        shell.append(&chunk[..n]);
    }
}

/// Waits for the child and records its status. `_keep` lives until the child is gone, then
/// protected files are restored and the fact is appended to the output.
async fn supervise(mut child: Child, shell: Arc<Shell>, _keep: Keep, protected: Option<Protected>) {
    let status = tokio::select! {
        result = child.wait() => ShellStatus::Exited(result.ok().and_then(|s| s.code())),
        _ = shell.cancel.cancelled() => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            ShellStatus::Killed
        }
    };
    if let Some(settled) = settle(protected) {
        shell.append(format!("\n{}\n", settled.message).as_bytes());
    }
    *lock(&shell.status) = status;
    shell.done.send_replace(true);
}

fn shell_by_id(shells: &BackgroundShells, id: &str) -> Result<Arc<Shell>, ToolError> {
    shells.get(id).ok_or_else(|| ToolError::InvalidInput(format!("unknown bash_id `{id}`")))
}

/// Reads the output of a background shell: what was printed since the last read, optionally
/// only the lines matching a regex, and its status.
pub struct BashOutputTool {
    shells: Arc<BackgroundShells>,
}

#[async_trait]
impl Tool for BashOutputTool {
    fn name(&self) -> &str {
        "BashOutput"
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "BashOutput",
            "Returns the output a background shell printed since the last read, and its status.",
            json!({
                "type": "object",
                "properties": {
                    "bash_id": {"type": "string", "description": "Id returned when the background shell started."},
                    "filter": {"type": "string", "description": "Optional regex; only matching lines are returned."}
                },
                "required": ["bash_id"]
            }),
        )
    }

    fn category(&self) -> ToolCategory {
        ToolCategory::Read
    }

    fn assess(&self, _input: &Value, _ctx: &ToolContext) -> RiskAssessment {
        RiskAssessment::new(RiskLevel::Low, "reads the output of a background shell")
    }

    fn permission_subject(&self, input: &Value) -> Option<String> {
        input_opt_str(input, "bash_id").map(str::to_owned)
    }

    async fn call(&self, input: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let id = input_str(&input, "bash_id")?;
        let shell = shell_by_id(&self.shells, id)?;
        let filter = input_opt_str(&input, "filter")
            .map(Regex::new)
            .transpose()
            .map_err(|e| ToolError::InvalidInput(format!("invalid filter regex: {e}")))?;
        let (text, skipped, more) = shell.take_new(ctx.config.security.max_tool_output_bytes.max(4));
        let shown = match &filter {
            Some(re) => text.lines().filter(|l| re.is_match(l)).collect::<Vec<_>>().join("\n"),
            None => text,
        };
        let mut body = String::new();
        if skipped > 0 {
            body.push_str(&format!("[{skipped} bytes dropped: output exceeded the buffer]\n"));
        }
        body.push_str(&shown);
        if !body.is_empty() && !body.ends_with('\n') {
            body.push('\n');
        }
        if more {
            body.push_str("[output truncated, read again for more]\n");
        }
        body.push_str(&format!("[status: {}]", shell.status_text()));
        Ok(ToolOutput::text(body))
    }
}

/// Stops a background shell.
pub struct KillShellTool {
    shells: Arc<BackgroundShells>,
}

#[async_trait]
impl Tool for KillShellTool {
    fn name(&self) -> &str {
        "KillShell"
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "KillShell",
            "Stops a background shell started with Bash and run_in_background.",
            json!({
                "type": "object",
                "properties": {"shell_id": {"type": "string", "description": "Id of the background shell (bash_<n>)."}},
                "required": ["shell_id"]
            }),
        )
    }

    fn category(&self) -> ToolCategory {
        ToolCategory::Execute
    }

    fn assess(&self, _input: &Value, _ctx: &ToolContext) -> RiskAssessment {
        RiskAssessment::new(RiskLevel::Moderate, "stops a background process")
    }

    fn permission_subject(&self, input: &Value) -> Option<String> {
        input_opt_str(input, "shell_id").map(str::to_owned)
    }

    async fn call(&self, input: Value, _ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let id = input_str(&input, "shell_id")?;
        let shell = shell_by_id(&self.shells, id)?;
        if shell.status() != ShellStatus::Running {
            return Ok(ToolOutput::text(format!("{id} already stopped ({}).", shell.status_text())));
        }
        let mut done = shell.done.subscribe();
        shell.cancel.cancel();
        let _ = done.wait_for(|finished| *finished).await;
        Ok(ToolOutput::text(format!("Stopped {id} ({}).", shell.status_text())))
    }
}

/// The Bash, BashOutput and KillShell tools, sharing one registry of background shells. Bash
/// commands are confined by `sandbox` when it is given.
pub fn shell_tools(shells: Arc<BackgroundShells>, sandbox: Option<Arc<SandboxSetup>>) -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(BashTool::new(shells.clone(), sandbox)),
        Arc::new(BashOutputTool { shells: shells.clone() }),
        Arc::new(KillShellTool { shells }),
    ]
}
