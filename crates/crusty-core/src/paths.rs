//! Well-known locations.

use std::path::{Path, PathBuf};

/// The user's home directory (`.` when unknown).
pub fn home_dir() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("."))
}

/// `$CRUSTY_HOME`, else `~/.crusty`.
pub fn crusty_home() -> PathBuf {
    if let Some(p) = std::env::var_os("CRUSTY_HOME").filter(|p| !p.is_empty()) {
        return PathBuf::from(p);
    }
    dirs::home_dir().unwrap_or_else(|| PathBuf::from(".")).join(".crusty")
}

/// Directory name for a project's sessions: the path with separators flattened.
pub fn project_key(project: &Path) -> String {
    let s = project.to_string_lossy();
    let key: String = s.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' { c } else { '-' }).collect();
    let key = key.trim_matches('-').to_owned();
    if key.is_empty() { "root".into() } else { key }
}

/// Where a project's transcripts live.
pub fn sessions_dir(project: &Path, configured: Option<&Path>) -> PathBuf {
    configured.map(Path::to_path_buf).unwrap_or_else(|| crusty_home().join("projects")).join(project_key(project))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_key_is_filesystem_safe() {
        let k = project_key(Path::new("/home/me/my project"));
        assert_eq!(k, "home-me-my-project");
        assert_eq!(project_key(Path::new("/")), "root");
    }
}
