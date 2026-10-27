//! Confines file tools to the allowed roots and flags protected paths.

use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};

use crusty_core::tool::{ToolContext, ToolError};
use globset::{GlobBuilder, GlobMatcher, GlobSet, GlobSetBuilder};

/// Removes `.` and resolves `..` without touching the disk. A `..` never climbs above a root.
pub fn normalize_lexically(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => match out.components().next_back() {
                Some(Component::Normal(_)) => {
                    out.pop();
                }
                Some(Component::RootDir | Component::Prefix(_)) => {}
                _ => out.push(".."),
            },
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Joins `raw` to the workspace when relative, then normalises it lexically.
fn join_to_workspace(ctx: &ToolContext, raw: &str) -> Result<PathBuf, ToolError> {
    if raw.trim().is_empty() {
        return Err(ToolError::InvalidInput("path is empty".into()));
    }
    let path = Path::new(raw);
    let full = if path.is_absolute() { path.to_path_buf() } else { ctx.workspace.join(path) };
    Ok(normalize_lexically(&full))
}

/// Resolves symlinks in the longest existing prefix of `path` and re-appends the rest, so
/// files that do not exist yet can still be located.
fn canonicalize_lenient(path: &Path) -> Result<PathBuf, ToolError> {
    let mut existing = path.to_path_buf();
    let mut missing: Vec<OsString> = Vec::new();
    loop {
        match existing.canonicalize() {
            Ok(mut resolved) => {
                for part in missing.iter().rev() {
                    resolved.push(part);
                }
                return Ok(resolved);
            }
            Err(_) => {
                // A dangling symlink looks missing, but writing through it would leave the sandbox.
                if existing.symlink_metadata().is_ok() {
                    return Err(ToolError::Denied(format!("{} is a broken symbolic link", display_path(&existing))));
                }
                let (Some(name), Some(parent)) = (existing.file_name(), existing.parent()) else {
                    return Err(ToolError::Io(format!("cannot resolve {}", display_path(path))));
                };
                missing.push(name.to_os_string());
                existing = parent.to_path_buf();
            }
        }
    }
}

/// Resolves what the model named to the location a tool will touch, and refuses anything
/// outside the allowed roots when `permissions.workspace_only` is set.
pub fn resolve_path(ctx: &ToolContext, raw: &str) -> Result<PathBuf, ToolError> {
    let resolved = canonicalize_lenient(&join_to_workspace(ctx, raw)?)?;
    if ctx.config.permissions.workspace_only && !ctx.allowed_roots.iter().any(|root| resolved.starts_with(root)) {
        return Err(ToolError::Denied(format!("{} is outside the workspace", display_path(&resolved))));
    }
    Ok(resolved)
}

/// Best-effort absolute location of `raw`, without the workspace check. Used to match rules
/// and to assess risk before a call runs.
pub fn absolute_path(ctx: &ToolContext, raw: &str) -> PathBuf {
    match join_to_workspace(ctx, raw) {
        Ok(path) => canonicalize_lenient(&path).unwrap_or(path),
        Err(_) => PathBuf::from(raw),
    }
}

/// Path text for the model and the UI: the Windows verbatim prefix is removed.
pub fn display_path(path: &Path) -> String {
    let text = path.to_string_lossy();
    let text: &str = &text;
    if let Some(unc) = text.strip_prefix(r"\\?\UNC\") {
        return format!(r"\\{unc}");
    }
    text.strip_prefix(r"\\?\").unwrap_or(text).to_owned()
}

/// Whether `path` matches a configured protected pattern, checked against both the
/// workspace-relative and the absolute form. A malformed pattern fails closed.
pub fn is_protected(ctx: &ToolContext, path: &Path) -> bool {
    matches_patterns(&ctx.config.security.protected_paths, ctx, path)
}

/// Whether `path` matches a `write_protected_paths` pattern. Same rules as `is_protected`.
pub fn is_write_protected(ctx: &ToolContext, path: &Path) -> bool {
    matches_patterns(&ctx.config.security.write_protected_paths, ctx, path)
}

fn matches_patterns(patterns: &[String], ctx: &ToolContext, path: &Path) -> bool {
    let Ok(set) = build_set(patterns) else {
        return true;
    };
    let absolute = display_path(path).replace('\\', "/");
    let relative = path.strip_prefix(&ctx.workspace).ok().map(|rel| rel.to_string_lossy().replace('\\', "/"));
    set.is_match(&absolute) || relative.is_some_and(|rel| set.is_match(&rel))
}

/// Whether the raw path named in a tool input is protected.
pub fn is_protected_raw(ctx: &ToolContext, raw: &str) -> bool {
    is_protected(ctx, &absolute_path(ctx, raw))
}

/// Compiles a gitignore-style pattern: `*` does not cross `/`, and a pattern without a slash
/// matches at any depth (`*.md` means `**/*.md`).
pub fn compile_glob(pattern: &str) -> Result<GlobMatcher, globset::Error> {
    let pattern = pattern.trim().trim_start_matches("./");
    let full = if pattern.contains('/') || pattern.starts_with("**") { pattern.to_owned() } else { format!("**/{pattern}") };
    Ok(GlobBuilder::new(&full).literal_separator(true).build()?.compile_matcher())
}

fn build_set(patterns: &[String]) -> Result<GlobSet, globset::Error> {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        builder.add(GlobBuilder::new(pattern).build()?);
    }
    builder.build()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::context;
    use tempfile::tempdir;

    #[test]
    fn traversal_and_outside_paths_are_denied() {
        let dir = tempdir().unwrap();
        let ctx = context(&dir);
        assert!(matches!(resolve_path(&ctx, "../x"), Err(ToolError::Denied(_))));
        let outside = tempdir().unwrap();
        let file = outside.path().join("secret.txt");
        assert!(matches!(resolve_path(&ctx, file.to_str().unwrap()), Err(ToolError::Denied(_))));
    }

    #[test]
    fn new_file_in_new_subdirectory_is_allowed() {
        let dir = tempdir().unwrap();
        let ctx = context(&dir);
        let path = resolve_path(&ctx, "new/deeper/file.txt").unwrap();
        assert!(path.starts_with(&ctx.workspace));
        assert!(path.ends_with("file.txt"));
    }

    #[cfg(unix)]
    #[test]
    fn symlink_escaping_the_workspace_is_denied() {
        let dir = tempdir().unwrap();
        let outside = tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("link")).unwrap();
        let ctx = context(&dir);
        assert!(matches!(resolve_path(&ctx, "link/x.txt"), Err(ToolError::Denied(_))));
    }

    #[test]
    fn protected_paths_are_flagged() {
        let dir = tempdir().unwrap();
        let ctx = context(&dir);
        assert!(is_protected_raw(&ctx, ".env"));
        assert!(is_protected_raw(&ctx, "config/.env.local"));
        assert!(!is_protected_raw(&ctx, "src/main.rs"));
    }

    #[test]
    fn write_protected_paths_are_flagged_and_fail_closed() {
        let dir = tempdir().unwrap();
        let mut ctx = context(&dir);
        assert!(is_write_protected(&ctx, &ctx.workspace.join(".crusty/config.toml")));
        assert!(is_write_protected(&ctx, &ctx.workspace.join(".mcp.json")));
        assert!(!is_write_protected(&ctx, &ctx.workspace.join(".env")));
        assert!(!is_write_protected(&ctx, &ctx.workspace.join("src/main.rs")));
        let mut config = (*ctx.config).clone();
        config.security.write_protected_paths = vec!["[".to_owned()];
        ctx.config = std::sync::Arc::new(config);
        assert!(is_write_protected(&ctx, &ctx.workspace.join("src/main.rs")));
    }

    #[test]
    fn display_path_strips_verbatim_prefix() {
        assert_eq!(display_path(Path::new(r"\\?\C:\a\b")), r"C:\a\b");
        assert_eq!(display_path(Path::new(r"\\?\UNC\srv\share\x")), r"\\srv\share\x");
        assert_eq!(display_path(Path::new("/tmp/x")), "/tmp/x");
    }

    #[test]
    fn normalize_resolves_dots() {
        assert_eq!(normalize_lexically(Path::new("a/./b/../c")), PathBuf::from("a/c"));
    }

    #[test]
    fn glob_without_slash_matches_any_depth() {
        assert!(compile_glob("*.md").unwrap().is_match("docs/a/readme.md"));
        assert!(!compile_glob("src/*.rs").unwrap().is_match("src/a/main.rs"));
    }
}
