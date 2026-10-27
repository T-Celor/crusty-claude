//! The `Edit` and `MultiEdit` tools: exact string replacement with uniqueness checks.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use crusty_core::message::ToolSpec;
use crusty_core::permission::RiskAssessment;
use crusty_core::tool::{Tool, ToolCategory, ToolContext, ToolError, ToolOutput, input_str};
use serde_json::{Value, json};

use crate::file_subject;
use crate::opt_bool;
use crate::sandbox::{display_path, resolve_path};
use crate::tracker::FileTracker;
use crate::write::{file_edit_risk, write_atomic};

/// Longest diff returned to the model, in lines.
const MAX_DIFF_LINES: usize = 60;

/// One exact replacement inside a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Replacement {
    old: String,
    new: String,
    replace_all: bool,
}

fn parse_replacement(input: &Value) -> Result<Replacement, ToolError> {
    Ok(Replacement {
        old: input_str(input, "old_string")?.to_owned(),
        new: input_str(input, "new_string")?.to_owned(),
        replace_all: opt_bool(input, "replace_all")?,
    })
}

/// Applies one replacement, returning the new text and how many places changed.
fn apply_one(text: &str, r: &Replacement) -> Result<(String, usize), String> {
    if r.old.is_empty() {
        return Err("old_string must not be empty".into());
    }
    if r.old == r.new {
        return Err("old_string and new_string are identical".into());
    }
    match text.matches(&r.old).count() {
        0 => Err("old_string not found".into()),
        n if n > 1 && !r.replace_all => Err(format!("found {n} matches; add context or use replace_all")),
        n if r.replace_all => Ok((text.replace(&r.old, &r.new), n)),
        _ => Ok((text.replacen(&r.old, &r.new, 1), 1)),
    }
}

/// Applies `replacements` in order to `text`. Any failure fails the whole list.
fn apply_replacements(text: &str, replacements: &[Replacement]) -> Result<(String, usize), ToolError> {
    let mut current = text.to_owned();
    let mut changed = 0;
    for (index, r) in replacements.iter().enumerate() {
        let (next, n) = apply_one(&current, r)
            .map_err(|e| ToolError::Failed(if replacements.len() == 1 { e } else { format!("edit {}: {e}", index + 1) }))?;
        current = next;
        changed += n;
    }
    Ok((current, changed))
}

/// Whether every line break in `text` is CRLF.
fn uses_crlf(text: &str) -> bool {
    text.contains("\r\n") && text.matches('\n').count() == text.matches("\r\n").count()
}

fn unified_diff(before: &str, after: &str, label: &str) -> String {
    let diff = similar::TextDiff::from_lines(before, after);
    let mut unified = diff.unified_diff();
    unified.context_radius(3).header(&format!("a/{label}"), &format!("b/{label}"));
    let text = unified.to_string();
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() <= MAX_DIFF_LINES {
        return text;
    }
    let mut cut = lines[..MAX_DIFF_LINES].join("\n");
    cut.push_str(&format!("\n… {} more diff lines", lines.len() - MAX_DIFF_LINES));
    cut
}

/// Applies `replacements` to the file at `path` and writes it back, keeping its line endings.
pub(crate) fn edit_file(tracker: &FileTracker, path: &Path, replacements: &[Replacement]) -> Result<ToolOutput, ToolError> {
    let label = display_path(path);
    if !path.exists() {
        return Err(ToolError::Failed(format!("{label} does not exist; use Write to create it")));
    }
    tracker.check_can_write(path)?;
    let raw = String::from_utf8(std::fs::read(path)?).map_err(|_| ToolError::Failed(format!("{label} is not UTF-8 text")))?;

    let crlf = uses_crlf(&raw);
    let before = if crlf { raw.replace("\r\n", "\n") } else { raw.clone() };
    let normalised: Vec<Replacement> = replacements
        .iter()
        .map(|r| Replacement {
            old: if crlf { r.old.replace("\r\n", "\n") } else { r.old.clone() },
            new: if crlf { r.new.replace("\r\n", "\n") } else { r.new.clone() },
            replace_all: r.replace_all,
        })
        .collect();
    let (after, changed) = apply_replacements(&before, &normalised)?;

    let bytes = if crlf { after.replace('\n', "\r\n") } else { after.clone() };
    write_atomic(path, bytes.as_bytes())?;
    tracker.record_write(path);

    let diff = unified_diff(&before, &after, &label);
    Ok(ToolOutput::text(format!("Edited {label}: {changed} replacement(s)\n{diff}")))
}

/// Replaces an exact string in a file. The string must occur once unless `replace_all` is set.
pub struct EditTool {
    tracker: Arc<FileTracker>,
}

impl EditTool {
    pub fn new(tracker: Arc<FileTracker>) -> Self {
        EditTool { tracker }
    }
}

#[async_trait]
impl Tool for EditTool {
    fn name(&self) -> &str {
        "Edit"
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "Edit",
            "Replaces old_string with new_string in a file that was Read in this session. old_string must match exactly once \
             unless replace_all is true. Include enough surrounding context to make it unique.",
            json!({
                "type": "object",
                "properties": {
                    "file_path": {"type": "string"},
                    "old_string": {"type": "string", "description": "Exact text to replace."},
                    "new_string": {"type": "string", "description": "Replacement text."},
                    "replace_all": {"type": "boolean", "description": "Replace every occurrence (default false)."}
                },
                "required": ["file_path", "old_string", "new_string"]
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
        let replacement = parse_replacement(&input)?;
        edit_file(&self.tracker, &path, std::slice::from_ref(&replacement))
    }
}

/// Applies several replacements to one file in order. All succeed or the file is unchanged.
pub struct MultiEditTool {
    tracker: Arc<FileTracker>,
}

impl MultiEditTool {
    pub fn new(tracker: Arc<FileTracker>) -> Self {
        MultiEditTool { tracker }
    }
}

#[async_trait]
impl Tool for MultiEditTool {
    fn name(&self) -> &str {
        "MultiEdit"
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "MultiEdit",
            "Applies several Edit replacements to one file, in order. If any replacement fails, the file is not changed.",
            json!({
                "type": "object",
                "properties": {
                    "file_path": {"type": "string"},
                    "edits": {
                        "type": "array",
                        "minItems": 1,
                        "items": {
                            "type": "object",
                            "properties": {
                                "old_string": {"type": "string"},
                                "new_string": {"type": "string"},
                                "replace_all": {"type": "boolean"}
                            },
                            "required": ["old_string", "new_string"]
                        }
                    }
                },
                "required": ["file_path", "edits"]
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
        let edits =
            input.get("edits").and_then(Value::as_array).ok_or_else(|| ToolError::InvalidInput("missing array field `edits`".into()))?;
        if edits.is_empty() {
            return Err(ToolError::InvalidInput("`edits` must not be empty".into()));
        }
        let replacements = edits.iter().map(parse_replacement).collect::<Result<Vec<_>, _>>()?;
        edit_file(&self.tracker, &path, &replacements)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::context;
    use crusty_core::tool::ToolContext;
    use tempfile::tempdir;

    /// Writes `content` to `f.txt` in the workspace and marks it as read.
    fn setup(ctx: &ToolContext, tracker: &FileTracker, content: &[u8]) -> std::path::PathBuf {
        let path = ctx.workspace.join("f.txt");
        std::fs::write(&path, content).unwrap();
        tracker.record_read(&path);
        path
    }

    #[tokio::test]
    async fn replaces_a_unique_match_and_shows_a_diff() {
        let dir = tempdir().unwrap();
        let ctx = context(&dir);
        let tracker = Arc::new(FileTracker::new());
        let path = setup(&ctx, &tracker, b"alpha\nbeta\ngamma\n");
        let tool = EditTool::new(tracker);
        let out = tool.call(json!({"file_path": "f.txt", "old_string": "beta", "new_string": "delta"}), &ctx).await.unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "alpha\ndelta\ngamma\n");
        assert!(out.text_content().contains("-beta"));
        assert!(out.text_content().contains("+delta"));
    }

    #[tokio::test]
    async fn missing_and_ambiguous_matches_are_errors() {
        let dir = tempdir().unwrap();
        let ctx = context(&dir);
        let tracker = Arc::new(FileTracker::new());
        setup(&ctx, &tracker, b"x x\n");
        let tool = EditTool::new(tracker);
        let missing = tool.call(json!({"file_path": "f.txt", "old_string": "zzz", "new_string": "y"}), &ctx).await;
        assert!(matches!(missing, Err(ToolError::Failed(m)) if m.contains("not found")));
        let ambiguous = tool.call(json!({"file_path": "f.txt", "old_string": "x", "new_string": "y"}), &ctx).await;
        assert!(matches!(ambiguous, Err(ToolError::Failed(m)) if m.contains("found 2 matches")));
    }

    #[tokio::test]
    async fn replace_all_changes_every_occurrence() {
        let dir = tempdir().unwrap();
        let ctx = context(&dir);
        let tracker = Arc::new(FileTracker::new());
        let path = setup(&ctx, &tracker, b"x x x\n");
        let tool = EditTool::new(tracker);
        tool.call(json!({"file_path": "f.txt", "old_string": "x", "new_string": "y", "replace_all": true}), &ctx).await.unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "y y y\n");
    }

    #[tokio::test]
    async fn identical_strings_are_rejected() {
        let dir = tempdir().unwrap();
        let ctx = context(&dir);
        let tracker = Arc::new(FileTracker::new());
        setup(&ctx, &tracker, b"same\n");
        let tool = EditTool::new(tracker);
        let result = tool.call(json!({"file_path": "f.txt", "old_string": "same", "new_string": "same"}), &ctx).await;
        assert!(matches!(result, Err(ToolError::Failed(m)) if m.contains("identical")));
    }

    #[tokio::test]
    async fn crlf_line_endings_are_preserved() {
        let dir = tempdir().unwrap();
        let ctx = context(&dir);
        let tracker = Arc::new(FileTracker::new());
        let path = setup(&ctx, &tracker, b"a\r\nb\r\nc\r\n");
        let tool = EditTool::new(tracker);
        tool.call(json!({"file_path": "f.txt", "old_string": "a\nb", "new_string": "x\ny"}), &ctx).await.unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"x\r\ny\r\nc\r\n");
    }

    #[tokio::test]
    async fn edit_requires_a_prior_read() {
        let dir = tempdir().unwrap();
        let ctx = context(&dir);
        std::fs::write(ctx.workspace.join("f.txt"), "a").unwrap();
        let tool = EditTool::new(Arc::new(FileTracker::new()));
        let result = tool.call(json!({"file_path": "f.txt", "old_string": "a", "new_string": "b"}), &ctx).await;
        assert!(matches!(result, Err(ToolError::Failed(_))));
    }

    #[tokio::test]
    async fn multi_edit_is_all_or_nothing() {
        let dir = tempdir().unwrap();
        let ctx = context(&dir);
        let tracker = Arc::new(FileTracker::new());
        let path = setup(&ctx, &tracker, b"one two\n");
        let tool = MultiEditTool::new(tracker);
        let input = json!({
            "file_path": "f.txt",
            "edits": [
                {"old_string": "one", "new_string": "1"},
                {"old_string": "absent", "new_string": "2"}
            ]
        });
        let result = tool.call(input, &ctx).await;
        assert!(matches!(result, Err(ToolError::Failed(m)) if m.starts_with("edit 2:")));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "one two\n");
    }

    #[tokio::test]
    async fn multi_edit_applies_in_order() {
        let dir = tempdir().unwrap();
        let ctx = context(&dir);
        let tracker = Arc::new(FileTracker::new());
        let path = setup(&ctx, &tracker, b"one two\n");
        let tool = MultiEditTool::new(tracker);
        let input = json!({
            "file_path": "f.txt",
            "edits": [
                {"old_string": "one", "new_string": "1"},
                {"old_string": "two", "new_string": "2"}
            ]
        });
        tool.call(input, &ctx).await.unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "1 2\n");
    }
}
