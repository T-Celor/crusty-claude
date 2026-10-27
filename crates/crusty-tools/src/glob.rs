//! The `Glob` tool (find files by name) and the directory walker shared with Grep and LS.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use crusty_core::message::ToolSpec;
use crusty_core::permission::{RiskAssessment, RiskLevel};
use crusty_core::tool::{Tool, ToolCategory, ToolContext, ToolError, ToolOutput, input_opt_str, input_str};
use globset::GlobMatcher;
use serde_json::{Value, json};

use crate::sandbox::{compile_glob, is_protected_raw, resolve_path};
use crate::{display_rel, search_subject, to_slashes};

/// Results returned at most.
const MAX_RESULTS: usize = 1000;

/// Walker over `base`: honours `.gitignore` even outside a git repository, includes hidden
/// files, and never enters `.git`.
pub(crate) fn walker(base: &Path) -> ignore::WalkBuilder {
    let mut builder = ignore::WalkBuilder::new(base);
    builder.hidden(false).require_git(false).filter_entry(|entry| entry.file_name().to_str() != Some(".git"));
    builder
}

/// Risk of a search: critical only when it is rooted at a protected path.
pub(crate) fn search_risk(ctx: &ToolContext, input: &Value) -> RiskAssessment {
    match input_opt_str(input, "path") {
        Some(raw) if is_protected_raw(ctx, raw) => RiskAssessment::new(RiskLevel::Critical, "searches a protected path"),
        _ => RiskAssessment::new(RiskLevel::Low, "searches files"),
    }
}

/// The directory a search starts from: `path` if given, else the workspace.
pub(crate) fn search_base(ctx: &ToolContext, input: &Value) -> Result<PathBuf, ToolError> {
    match input_opt_str(input, "path") {
        Some(raw) => resolve_path(ctx, raw),
        None => Ok(ctx.workspace.clone()),
    }
}

/// Finds files under `base` whose path relative to `base` matches, with their mtimes.
fn find_files(base: &Path, matcher: &GlobMatcher) -> Vec<(PathBuf, SystemTime)> {
    walker(base)
        .build()
        .flatten()
        .filter(|entry| entry.file_type().is_some_and(|t| t.is_file()))
        .filter(|entry| entry.path().strip_prefix(base).is_ok_and(|rel| matcher.is_match(to_slashes(rel))))
        .map(|entry| {
            let mtime = entry.metadata().ok().and_then(|m| m.modified().ok()).unwrap_or(UNIX_EPOCH);
            (entry.path().to_path_buf(), mtime)
        })
        .collect()
}

/// Lists files whose names match a glob, newest first.
pub struct GlobTool;

#[async_trait]
impl Tool for GlobTool {
    fn name(&self) -> &str {
        "Glob"
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "Glob",
            "Finds files by glob pattern (for example `**/*.rs`), respecting .gitignore. Results are sorted by modification time, newest first.",
            json!({
                "type": "object",
                "properties": {
                    "pattern": {"type": "string", "description": "Glob pattern, relative to the search directory."},
                    "path": {"type": "string", "description": "Directory to search (default: the workspace)."}
                },
                "required": ["pattern"]
            }),
        )
    }

    fn category(&self) -> ToolCategory {
        ToolCategory::Read
    }

    fn assess(&self, input: &Value, ctx: &ToolContext) -> RiskAssessment {
        search_risk(ctx, input)
    }

    fn permission_subject(&self, input: &Value) -> Option<String> {
        search_subject(input)
    }

    async fn call(&self, input: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let matcher = compile_glob(input_str(&input, "pattern")?).map_err(|e| ToolError::InvalidInput(format!("invalid pattern: {e}")))?;
        let base = search_base(ctx, &input)?;
        let mut files =
            tokio::task::spawn_blocking(move || find_files(&base, &matcher)).await.map_err(|e| ToolError::Failed(e.to_string()))?;
        files.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        if files.is_empty() {
            return Ok(ToolOutput::text("No files matched"));
        }
        let total = files.len();
        let mut lines: Vec<String> = files.iter().take(MAX_RESULTS).map(|(path, _)| display_rel(path, &ctx.workspace)).collect();
        if total > MAX_RESULTS {
            lines.push(format!("[showing first {MAX_RESULTS} of {total} files]"));
        }
        Ok(ToolOutput::text(lines.join("\n")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::context;
    use std::time::Duration;
    use tempfile::tempdir;

    #[tokio::test]
    async fn matches_nested_files_by_pattern() {
        let dir = tempdir().unwrap();
        let ctx = context(&dir);
        std::fs::create_dir_all(dir.path().join("src/a")).unwrap();
        std::fs::write(dir.path().join("src/a/lib.rs"), "").unwrap();
        std::fs::write(dir.path().join("notes.txt"), "").unwrap();
        let out = GlobTool.call(json!({"pattern": "**/*.rs"}), &ctx).await.unwrap();
        assert_eq!(out.text_content(), "src/a/lib.rs");
    }

    #[tokio::test]
    async fn sorts_newest_first() {
        let dir = tempdir().unwrap();
        let ctx = context(&dir);
        std::fs::write(dir.path().join("old.txt"), "").unwrap();
        std::thread::sleep(Duration::from_millis(30));
        std::fs::write(dir.path().join("new.txt"), "").unwrap();
        let out = GlobTool.call(json!({"pattern": "*.txt"}), &ctx).await.unwrap();
        assert_eq!(out.text_content(), "new.txt\nold.txt");
    }

    #[tokio::test]
    async fn respects_gitignore_without_a_git_repo() {
        let dir = tempdir().unwrap();
        let ctx = context(&dir);
        std::fs::write(dir.path().join(".gitignore"), "secret.log\n").unwrap();
        std::fs::write(dir.path().join("secret.log"), "").unwrap();
        std::fs::write(dir.path().join("keep.log"), "").unwrap();
        let out = GlobTool.call(json!({"pattern": "*.log"}), &ctx).await.unwrap();
        assert_eq!(out.text_content(), "keep.log");
    }

    #[tokio::test]
    async fn reports_no_matches() {
        let dir = tempdir().unwrap();
        let ctx = context(&dir);
        let out = GlobTool.call(json!({"pattern": "*.nothing"}), &ctx).await.unwrap();
        assert_eq!(out.text_content(), "No files matched");
    }
}
