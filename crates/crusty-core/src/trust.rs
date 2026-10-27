//! Projects the user trusts with their own configuration (`.crusty/config.toml` settings
//! that redirect traffic or run commands, and `.mcp.json`). A cloned repository is
//! untrusted until the user runs `crusty trust` in it.

use std::collections::BTreeSet;
use std::io;
use std::path::{Path, PathBuf};

pub fn trust_file() -> PathBuf {
    crate::paths::crusty_home().join("trusted_projects.json")
}

fn key(project: &Path) -> String {
    project.canonicalize().unwrap_or_else(|_| project.to_path_buf()).to_string_lossy().into_owned()
}

fn read(file: &Path) -> BTreeSet<String> {
    std::fs::read_to_string(file).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

pub fn is_trusted_in(file: &Path, project: &Path) -> bool {
    read(file).contains(&key(project))
}

pub fn set_trusted_in(file: &Path, project: &Path, trusted: bool) -> io::Result<()> {
    let mut set = read(file);
    let k = key(project);
    if trusted {
        set.insert(k);
    } else {
        set.remove(&k);
    }
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = file.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(&set).map_err(io::Error::other)?)?;
    std::fs::rename(tmp, file)
}

/// Whether the user trusts `project` (stored in `~/.crusty/trusted_projects.json`).
pub fn is_trusted(project: &Path) -> bool {
    is_trusted_in(&trust_file(), project)
}

pub fn set_trusted(project: &Path, trusted: bool) -> io::Result<()> {
    set_trusted_in(&trust_file(), project, trusted)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trust_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("t.json");
        let project = dir.path().join("p");
        std::fs::create_dir_all(&project).unwrap();
        assert!(!is_trusted_in(&file, &project));
        set_trusted_in(&file, &project, true).unwrap();
        assert!(is_trusted_in(&file, &project));
        set_trusted_in(&file, &project, false).unwrap();
        assert!(!is_trusted_in(&file, &project));
    }
}
