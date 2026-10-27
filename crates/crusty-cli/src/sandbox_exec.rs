//! Sandbox glue for the CLI: the setup that Bash tools share, and the hidden `__sandbox-exec`
//! entry point that the Landlock backend re-enters to confine a command.

use std::sync::Arc;

use anyhow::{Result, bail};
use crusty_core::config::{Config, SandboxMode};
use crusty_shell::SandboxSetup;

/// The sandbox setup for Bash tools, or `None` when `sandbox.mode` is `off`.
pub fn setup(config: &Config) -> Option<Arc<SandboxSetup>> {
    if config.sandbox.mode == SandboxMode::Off {
        return None;
    }
    // An empty path only matters to the Landlock backend, which then fails loudly at spawn.
    let self_exe = std::env::current_exe().unwrap_or_default();
    let detection = crusty_sandbox::detect(&config.sandbox);
    Some(Arc::new(SandboxSetup { config: config.sandbox.clone(), detection, self_exe }))
}

/// Joins a fresh kill-on-close job, then runs `program args` as its child and returns its exit
/// code. Used by the Windows job backend: the helper is in the job before the command starts,
/// so the whole tree ends with it. Only Windows has jobs; elsewhere this is an error.
pub fn run_in_job(program: &str, args: &[String]) -> Result<i32> {
    #[cfg(windows)]
    {
        let job = crusty_sandbox::JobGuard::join_current_process()?;
        let status = std::process::Command::new(program).args(args).status()?;
        // Closing a kill-on-close job that contains this process would kill it before it
        // reports the child's code: exit while the job handle is still open.
        let code = status.code().unwrap_or(1);
        let _keep_open = &job;
        std::process::exit(code)
    }
    #[cfg(not(windows))]
    {
        let _ = (program, args);
        bail!("__job-exec is only available on Windows")
    }
}

/// Runs `program args` confined by the Landlock policy in `policy_json` and returns its exit
/// code. Only Linux has Landlock; elsewhere this is an error.
pub fn run(policy_json: &str, program: &str, args: &[String]) -> Result<i32> {
    #[cfg(target_os = "linux")]
    {
        Ok(crusty_sandbox::run_landlocked(policy_json, program, args)?)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (policy_json, program, args);
        bail!("__sandbox-exec is only available on Linux (Landlock)")
    }
}
