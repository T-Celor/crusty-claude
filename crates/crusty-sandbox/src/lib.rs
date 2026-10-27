//! OS-level confinement of commands: Landlock or bubblewrap on Linux, Seatbelt on macOS, and a job object on Windows.
//!
//! Flow: [`detect`] picks the backend, [`Policy::from_config`] resolves the paths, [`wrap`]
//! turns a command into the one to spawn. On Linux with Landlock, the wrapped command re-runs
//! this binary as `__sandbox-exec`, which calls [`run_landlocked`].
//!
//! Limitations: Landlock is allow-list only, so a protected path inside a writable root stays
//! writable there (reported as a note). Bubblewrap and Seatbelt cannot be checked on the
//! build host; only the argument vectors and profile text are unit-tested.

mod command;
mod detection;
mod policy;

#[cfg(windows)]
mod job;
#[cfg(target_os = "linux")]
mod landlock_exec;

pub use command::{Wrapped, wrap};
pub use detection::{Backend, Detection, describe, detect, detect_with};
pub use policy::Policy;

#[cfg(windows)]
pub use job::JobGuard;
#[cfg(target_os = "linux")]
pub use landlock_exec::run_landlocked;

use std::path::PathBuf;

/// Why a command could not be confined.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SandboxError {
    /// No sandbox backend works here and `fail_if_unavailable` is set.
    #[error("sandbox unavailable: {0}")]
    Unavailable(String),
    /// A path is not UTF-8 or contains a NUL or line break, so it cannot go into a profile or argument list safely.
    #[error("refusing sandbox path {0:?}: not UTF-8 or contains a control character")]
    InvalidPath(PathBuf),
}
