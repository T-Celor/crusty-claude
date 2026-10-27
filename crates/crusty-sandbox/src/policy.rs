//! The resolved description of what one sandboxed command may read and write.

use std::path::{Path, PathBuf};

use crusty_core::config::{SandboxConfig, SandboxMode};
use serde::{Deserialize, Serialize};

/// Paths under the workspace that stay read-only: VCS hooks, tool config and editor state.
const DEFAULT_PROTECTED: &[&str] = &[".git/hooks", ".git/config", ".crusty", ".claude", ".mcp.json", ".vscode", ".idea"];

/// A resolved sandbox policy. Serialized to JSON to pass it to the Landlock helper.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Policy {
    /// Confinement level.
    pub mode: SandboxMode,
    /// The project root; writable in `workspace-write` mode.
    pub workspace: PathBuf,
    /// Directory the command starts in.
    pub cwd: PathBuf,
    /// Extra writable directories, already resolved against the workspace.
    pub writable_roots: Vec<PathBuf>,
    /// Whether network access is allowed.
    pub network: bool,
    /// Paths that stay read-only even inside a writable root.
    pub protected: Vec<PathBuf>,
    /// Refuse to run instead of running unconfined when no backend works.
    pub fail_if_unavailable: bool,
}

impl Policy {
    /// Resolves `cfg` for a command run in `cwd` inside `workspace`. Relative writable roots
    /// are taken relative to `workspace`, and the default protected paths are added.
    pub fn from_config(cfg: &SandboxConfig, workspace: &Path, cwd: &Path) -> Self {
        Policy {
            mode: cfg.mode,
            workspace: workspace.to_path_buf(),
            cwd: cwd.to_path_buf(),
            // `PathBuf::join` keeps an absolute root as it is.
            writable_roots: cfg.writable_roots.iter().map(|r| workspace.join(r)).collect(),
            network: cfg.network,
            protected: DEFAULT_PROTECTED.iter().map(|p| workspace.join(p)).collect(),
            fail_if_unavailable: cfg.fail_if_unavailable,
        }
    }

    /// Directories the command may write to: the workspace (in `workspace-write` mode only)
    /// followed by the extra writable roots, without duplicates.
    pub fn writable_dirs(&self) -> Vec<&Path> {
        if self.mode != SandboxMode::WorkspaceWrite {
            return Vec::new();
        }
        let mut dirs: Vec<&Path> = vec![self.workspace.as_path()];
        for root in &self.writable_roots {
            if !dirs.contains(&root.as_path()) {
                dirs.push(root);
            }
        }
        dirs
    }

    /// The first protected path that lies inside a writable directory, if any. Landlock cannot
    /// make such a path read-only, so callers must report it.
    pub(crate) fn protected_in_writable(&self) -> Option<&Path> {
        let dirs = self.writable_dirs();
        self.protected.iter().map(PathBuf::as_path).find(|p| dirs.iter().any(|dir| p.starts_with(dir)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_config_resolves_relative_roots_and_adds_defaults() {
        let cfg = SandboxConfig { writable_roots: vec![PathBuf::from("cache")], ..SandboxConfig::default() };
        let ws = Path::new("/work/ws");
        let policy = Policy::from_config(&cfg, ws, ws);
        assert_eq!(policy.writable_roots, vec![ws.join("cache")]);
        assert!(policy.protected.contains(&ws.join(".git/hooks")));
        assert!(policy.protected.contains(&ws.join(".mcp.json")));
        assert_eq!(policy.protected.len(), DEFAULT_PROTECTED.len());
        assert!(policy.network);
    }

    #[test]
    fn writable_dirs_follow_mode() {
        let cfg = SandboxConfig { writable_roots: vec![PathBuf::from("/ws"), PathBuf::from("/cache")], ..SandboxConfig::default() };
        let ws = Path::new("/ws");
        let mut policy = Policy::from_config(&cfg, ws, ws);
        assert_eq!(policy.writable_dirs(), vec![ws, Path::new("/cache")]);
        policy.mode = SandboxMode::ReadOnly;
        assert!(policy.writable_dirs().is_empty());
    }

    #[test]
    fn policy_json_round_trips() {
        let cfg = SandboxConfig {
            network: false,
            writable_roots: vec![PathBuf::from("/cache")],
            fail_if_unavailable: true,
            ..SandboxConfig::default()
        };
        let policy = Policy::from_config(&cfg, Path::new("/ws"), Path::new("/ws/src"));
        let json = serde_json::to_string(&policy).expect("serialize");
        let back: Policy = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, policy);
    }
}
