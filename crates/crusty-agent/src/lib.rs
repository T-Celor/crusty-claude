//! The orchestrator/executor engine of Crusty-Claude.
//!
//! The main agent is always the orchestrator: it plans and delegates with the `Task` tool
//! to executor agents of configurable domains (coder, reviewer, designer, …), each with its
//! own model. Every agent runs the same loop (`engine::run_agent`); every model request
//! goes through the optimizer pipeline and every tool call through the permission engine.

pub mod approval;
pub mod commands;
pub mod compact;
pub mod controller;
pub mod delegate;
pub mod engine;
pub mod print;
pub mod prompts;
pub mod runtime;

use std::path::{Path, PathBuf};

pub use approval::ChannelApprover;
pub use commands::{command_list, reload_memory};
pub use controller::{Controller, replay, session_start_hook};
pub use engine::{AgentOutcome, AgentSpec, ORCHESTRATOR_ID, StopKind, orchestrator_spec, run_agent};
pub use print::{PrintOutcome, run_print};
pub use runtime::{PolicyBundle, Runtime, RuntimeParts, RuntimeSettings, SessionContext};

/// A path without the Windows verbatim prefix (`\\?\`), for display.
pub fn display_path(p: &Path) -> String {
    let s = p.display().to_string();
    s.strip_prefix(r"\\?\").map(str::to_owned).unwrap_or(s)
}

/// Finds an executable on PATH (adds `.exe` on Windows).
pub fn find_in_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let names: Vec<String> =
        if cfg!(windows) { vec![format!("{name}.exe"), format!("{name}.cmd"), name.to_owned()] } else { vec![name.to_owned()] };
    std::env::split_paths(&path).find_map(|dir| names.iter().map(|n| dir.join(n)).find(|p| p.is_file()))
}

/// Transcript directory of the runtime's project.
pub fn sessions_dir(rt: &Runtime) -> PathBuf {
    crusty_core::paths::sessions_dir(&rt.workspace, rt.config.session.dir.as_deref())
}
