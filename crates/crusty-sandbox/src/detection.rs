//! Chooses the confinement backend available on this machine.

use std::path::PathBuf;

use crusty_core::config::{SandboxConfig, SandboxMode};
use crusty_core::event::SandboxInfo;

use crate::Policy;

/// The mechanism that confines commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    /// `bwrap` (bubblewrap): mount and PID namespaces.
    Bubblewrap,
    /// Landlock LSM, applied by the `__sandbox-exec` helper process.
    Landlock,
    /// macOS `sandbox-exec` with an SBPL profile.
    Seatbelt,
    /// Windows job object: process-tree cleanup only, no file or network isolation.
    WindowsJob,
    /// No confinement available.
    None,
}

impl Backend {
    /// Name reported in [`SandboxInfo::backend`].
    pub fn name(self) -> &'static str {
        match self {
            Backend::Bubblewrap => "bubblewrap",
            Backend::Landlock => "landlock",
            Backend::Seatbelt => "seatbelt",
            Backend::WindowsJob => "windows-job",
            Backend::None => "none",
        }
    }
}

/// What this machine can enforce for a backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Detection {
    /// The chosen backend.
    pub backend: Backend,
    /// Whether commands can be confined at all with this backend.
    pub available: bool,
    /// Whether file writes are confined.
    pub confines_writes: bool,
    /// Whether network access can be blocked.
    pub blocks_network: bool,
    /// Human-readable explanation, shown to the user.
    pub detail: String,
}

impl Detection {
    fn unavailable(detail: impl Into<String>) -> Self {
        Detection { backend: Backend::None, available: false, confines_writes: false, blocks_network: false, detail: detail.into() }
    }
}

/// Detects the backend using the executables on `PATH`.
pub fn detect(cfg: &SandboxConfig) -> Detection {
    detect_with(cfg, find_on_path)
}

/// Like [`detect`], with a custom executable lookup (for tests).
pub fn detect_with(cfg: &SandboxConfig, path_lookup: impl Fn(&str) -> Option<PathBuf>) -> Detection {
    if cfg.mode == SandboxMode::Off {
        return Detection::unavailable("disabled");
    }
    platform_detection(&path_lookup)
}

/// Summarises what [`crate::wrap`] will enforce, for the UI.
pub fn describe(det: &Detection, policy: &Policy) -> SandboxInfo {
    SandboxInfo {
        backend: det.backend.name().to_string(),
        mode: policy.mode.to_string(),
        network_blocked: !policy.network && det.blocks_network && det.available,
        available: det.available,
        detail: det.detail.clone(),
    }
}

fn find_on_path(name: &str) -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths).map(|dir| dir.join(name)).find(|candidate| candidate.is_file())
}

#[cfg(target_os = "linux")]
fn platform_detection(lookup: &dyn Fn(&str) -> Option<PathBuf>) -> Detection {
    if let Some(path) = lookup("bwrap") {
        return Detection {
            backend: Backend::Bubblewrap,
            available: true,
            confines_writes: true,
            blocks_network: true,
            detail: format!("bubblewrap at {}", path.display()),
        };
    }
    let (fs, net) = crate::landlock_exec::probe();
    if !fs {
        return Detection::unavailable("no sandbox backend: bwrap is not on PATH and the kernel has no Landlock");
    }
    Detection {
        backend: Backend::Landlock,
        available: true,
        confines_writes: true,
        blocks_network: net,
        detail: if net {
            "landlock (filesystem and TCP network rules)".to_string()
        } else {
            "landlock filesystem rules only: kernel lacks ABI v4 network rules".to_string()
        },
    }
}

#[cfg(target_os = "macos")]
fn platform_detection(_lookup: &dyn Fn(&str) -> Option<PathBuf>) -> Detection {
    if std::path::Path::new("/usr/bin/sandbox-exec").is_file() {
        Detection {
            backend: Backend::Seatbelt,
            available: true,
            confines_writes: true,
            blocks_network: true,
            detail: "seatbelt via /usr/bin/sandbox-exec".to_string(),
        }
    } else {
        Detection::unavailable("/usr/bin/sandbox-exec is missing")
    }
}

#[cfg(windows)]
fn platform_detection(_lookup: &dyn Fn(&str) -> Option<PathBuf>) -> Detection {
    Detection {
        backend: Backend::WindowsJob,
        available: true,
        confines_writes: false,
        blocks_network: false,
        detail: "process tree only: no filesystem or network isolation on Windows".to_string(),
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn platform_detection(_lookup: &dyn Fn(&str) -> Option<PathBuf>) -> Detection {
    Detection::unavailable("no sandbox backend for this operating system")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn off_mode_is_disabled() {
        let cfg = SandboxConfig { mode: SandboxMode::Off, ..SandboxConfig::default() };
        let det = detect_with(&cfg, |_| None);
        assert_eq!(det.backend, Backend::None);
        assert!(!det.available);
        assert_eq!(det.detail, "disabled");
    }

    #[test]
    fn availability_matches_backend() {
        let det = detect_with(&SandboxConfig::default(), |_| None);
        assert_eq!(det.available, det.backend != Backend::None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn bwrap_on_path_wins() {
        let det = detect_with(&SandboxConfig::default(), |name| (name == "bwrap").then(|| PathBuf::from("/usr/bin/bwrap")));
        assert_eq!(det.backend, Backend::Bubblewrap);
        assert!(det.available && det.blocks_network && det.confines_writes);
    }

    #[cfg(windows)]
    #[test]
    fn windows_reports_job_object_only() {
        let det = detect_with(&SandboxConfig::default(), |_| None);
        assert_eq!(det.backend, Backend::WindowsJob);
        assert!(det.available && !det.confines_writes && !det.blocks_network);
        assert!(det.detail.contains("no filesystem or network isolation"));
    }

    #[test]
    fn describe_reports_network_block() {
        let det = Detection {
            backend: Backend::Bubblewrap,
            available: true,
            confines_writes: true,
            blocks_network: true,
            detail: "bubblewrap".into(),
        };
        let policy = Policy::from_config(
            &SandboxConfig { network: false, ..SandboxConfig::default() },
            std::path::Path::new("/ws"),
            std::path::Path::new("/ws"),
        );
        let info = describe(&det, &policy);
        assert_eq!(info.backend, "bubblewrap");
        assert_eq!(info.mode, "workspace-write");
        assert!(info.network_blocked && info.available);
    }
}
