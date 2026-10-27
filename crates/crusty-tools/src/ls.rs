//! The `LS` tool: one directory's entries.

use async_trait::async_trait;
use crusty_core::message::ToolSpec;
use crusty_core::permission::RiskAssessment;
use crusty_core::tool::{Tool, ToolCategory, ToolContext, ToolError, ToolOutput, input_str};
use globset::GlobMatcher;
use serde_json::{Value, json};

use crate::glob::search_risk;
use crate::sandbox::{compile_glob, resolve_path};
use crate::search_subject;

/// Entries returned at most.
const MAX_ENTRIES: usize = 1000;

/// Compiles the `ignore` array of the input: globs matched against entry names.
fn ignore_globs(input: &Value) -> Result<Vec<GlobMatcher>, ToolError> {
    match input.get("ignore") {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| {
                let pattern = item.as_str().ok_or_else(|| ToolError::InvalidInput("`ignore` entries must be strings".into()))?;
                compile_glob(pattern).map_err(|e| ToolError::InvalidInput(format!("invalid ignore glob: {e}")))
            })
            .collect(),
        Some(_) => Err(ToolError::InvalidInput("`ignore` must be an array of globs".into())),
    }
}

/// Lists the entries of one directory, directories marked with a trailing `/`.
pub struct LsTool;

#[async_trait]
impl Tool for LsTool {
    fn name(&self) -> &str {
        "LS"
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "LS",
            "Lists the entries of one directory, sorted by name. Directories end with `/`.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Directory to list, absolute or relative to the workspace."},
                    "ignore": {"type": "array", "items": {"type": "string"}, "description": "Globs of entry names to hide."}
                },
                "required": ["path"]
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
        let dir = resolve_path(ctx, input_str(&input, "path")?)?;
        if !dir.is_dir() {
            return Err(ToolError::InvalidInput(format!("{} is not a directory", crate::display_path(&dir))));
        }
        let ignore = ignore_globs(&input)?;
        let mut entries: Vec<(String, bool)> = Vec::new();
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if ignore.iter().any(|glob| glob.is_match(&name)) {
                continue;
            }
            let is_dir = entry.path().is_dir();
            entries.push((name, is_dir));
        }
        if entries.is_empty() {
            return Ok(ToolOutput::text("(empty directory)"));
        }
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        let total = entries.len();
        let mut lines: Vec<String> =
            entries.iter().take(MAX_ENTRIES).map(|(name, is_dir)| if *is_dir { format!("{name}/") } else { name.clone() }).collect();
        if total > MAX_ENTRIES {
            lines.push(format!("[showing first {MAX_ENTRIES} of {total} entries]"));
        }
        Ok(ToolOutput::text(lines.join("\n")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::context;
    use tempfile::tempdir;

    #[tokio::test]
    async fn lists_sorted_entries_with_directory_suffix() {
        let dir = tempdir().unwrap();
        let ctx = context(&dir);
        std::fs::create_dir(dir.path().join("zdir")).unwrap();
        std::fs::write(dir.path().join("b.txt"), "").unwrap();
        std::fs::write(dir.path().join("a.txt"), "").unwrap();
        let out = LsTool.call(json!({"path": "."}), &ctx).await.unwrap();
        assert_eq!(out.text_content(), "a.txt\nb.txt\nzdir/");
    }

    #[tokio::test]
    async fn ignore_globs_hide_entries() {
        let dir = tempdir().unwrap();
        let ctx = context(&dir);
        std::fs::write(dir.path().join("a.log"), "").unwrap();
        std::fs::write(dir.path().join("a.txt"), "").unwrap();
        let out = LsTool.call(json!({"path": ".", "ignore": ["*.log"]}), &ctx).await.unwrap();
        assert_eq!(out.text_content(), "a.txt");
    }

    #[tokio::test]
    async fn file_path_and_missing_path_are_errors() {
        let dir = tempdir().unwrap();
        let ctx = context(&dir);
        std::fs::write(dir.path().join("f.txt"), "").unwrap();
        assert!(matches!(LsTool.call(json!({"path": "f.txt"}), &ctx).await, Err(ToolError::InvalidInput(_))));
        assert!(LsTool.call(json!({"path": "missing"}), &ctx).await.is_err());
    }

    #[tokio::test]
    async fn empty_directory_is_reported() {
        let dir = tempdir().unwrap();
        let ctx = context(&dir);
        let out = LsTool.call(json!({"path": "."}), &ctx).await.unwrap();
        assert_eq!(out.text_content(), "(empty directory)");
    }
}
