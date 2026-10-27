//! The `Write` tool, and the atomic file write shared by every file-changing tool.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use crusty_core::message::ToolSpec;
use crusty_core::permission::{RiskAssessment, RiskLevel};
use crusty_core::tool::{Tool, ToolCategory, ToolContext, ToolError, ToolOutput, input_str};
use serde_json::{Value, json};

use crate::file_subject;
use crate::sandbox::{absolute_path, display_path, is_protected_raw, is_write_protected, resolve_path};
use crate::tracker::FileTracker;

/// Risk of changing the file named by `key` in `input`: critical for protected and
/// write-protected paths.
pub(crate) fn file_edit_risk(ctx: &ToolContext, input: &Value, key: &str) -> RiskAssessment {
    match input_str(input, key) {
        Ok(raw) if is_protected_raw(ctx, raw) => RiskAssessment::new(RiskLevel::Critical, "changes a protected path"),
        Ok(raw) if is_write_protected(ctx, &absolute_path(ctx, raw)) => {
            RiskAssessment::new(RiskLevel::Critical, "changes a write-protected path")
        }
        _ => RiskAssessment::new(RiskLevel::Moderate, "changes a file"),
    }
}

/// Writes `bytes` to `path` through a temporary file in the same directory, creating missing
/// parent directories, so a reader never sees a half-written file.
pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), ToolError> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let dir = path.parent().ok_or_else(|| ToolError::Io(format!("{} has no parent directory", display_path(path))))?;
    std::fs::create_dir_all(dir)?;
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    let temp = dir.join(format!(".{name}.{}.{}.tmp", std::process::id(), COUNTER.fetch_add(1, Ordering::Relaxed)));
    let result = std::fs::write(&temp, bytes).and_then(|()| std::fs::rename(&temp, path));
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result.map_err(ToolError::from)
}

/// Creates or overwrites a file. An existing file must have been read in this session.
pub struct WriteTool {
    tracker: Arc<FileTracker>,
}

impl WriteTool {
    pub fn new(tracker: Arc<FileTracker>) -> Self {
        WriteTool { tracker }
    }
}

#[async_trait]
impl Tool for WriteTool {
    fn name(&self) -> &str {
        "Write"
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "Write",
            "Writes a file, replacing its content. An existing file must be Read first in this session. Parent directories are created.",
            json!({
                "type": "object",
                "properties": {
                    "file_path": {"type": "string", "description": "Path of the file, absolute or relative to the workspace."},
                    "content": {"type": "string", "description": "The complete new content of the file."}
                },
                "required": ["file_path", "content"]
            }),
        )
    }

    fn category(&self) -> ToolCategory {
        ToolCategory::Edit
    }

    fn assess(&self, input: &Value, ctx: &ToolContext) -> RiskAssessment {
        file_edit_risk(ctx, input, "file_path")
    }

    fn permission_subject(&self, input: &Value) -> Option<String> {
        file_subject(input, "file_path")
    }

    async fn call(&self, input: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let path = resolve_path(ctx, input_str(&input, "file_path")?)?;
        let content = input_str(&input, "content")?;
        if path.is_dir() {
            return Err(ToolError::InvalidInput(format!("{} is a directory", display_path(&path))));
        }
        self.tracker.check_can_write(&path)?;
        write_atomic(&path, content.as_bytes())?;
        self.tracker.record_write(&path);
        Ok(ToolOutput::text(format!("Wrote {} lines to {}", content.lines().count(), display_path(&path))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::context;
    use tempfile::tempdir;

    #[tokio::test]
    async fn creates_new_file_in_new_directory() {
        let dir = tempdir().unwrap();
        let ctx = context(&dir);
        let tool = WriteTool::new(Arc::new(FileTracker::new()));
        let out = tool.call(json!({"file_path": "deep/new/f.txt", "content": "a\nb\n"}), &ctx).await.unwrap();
        assert!(out.text_content().starts_with("Wrote 2 lines to"));
        assert_eq!(std::fs::read_to_string(dir.path().join("deep/new/f.txt")).unwrap(), "a\nb\n");
    }

    #[tokio::test]
    async fn refuses_unread_existing_file() {
        let dir = tempdir().unwrap();
        let ctx = context(&dir);
        std::fs::write(dir.path().join("f.txt"), "old").unwrap();
        let tool = WriteTool::new(Arc::new(FileTracker::new()));
        let result = tool.call(json!({"file_path": "f.txt", "content": "new"}), &ctx).await;
        assert!(matches!(result, Err(ToolError::Failed(_))));
        assert_eq!(std::fs::read_to_string(dir.path().join("f.txt")).unwrap(), "old");
    }

    #[test]
    fn protected_path_is_critical() {
        let dir = tempdir().unwrap();
        let ctx = context(&dir);
        let risk = file_edit_risk(&ctx, &json!({"file_path": ".env"}), "file_path");
        assert_eq!(risk.level, RiskLevel::Critical);
    }

    #[test]
    fn write_protected_path_is_critical_but_ordinary_file_is_not() {
        let dir = tempdir().unwrap();
        let ctx = context(&dir);
        for path in [".crusty/settings.json", ".mcp.json", ".git/hooks/pre-commit"] {
            let risk = file_edit_risk(&ctx, &json!({"file_path": path}), "file_path");
            assert_eq!(risk.level, RiskLevel::Critical, "{path}");
        }
        assert_eq!(file_edit_risk(&ctx, &json!({"file_path": "src/main.rs"}), "file_path").level, RiskLevel::Moderate);
    }
}
