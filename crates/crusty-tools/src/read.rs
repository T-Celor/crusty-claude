//! The `Read` tool: text with line numbers, or an image the model can see.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use crusty_core::message::{ContentBlock, Extra, ImageSource, ToolSpec};
use crusty_core::permission::{RiskAssessment, RiskLevel};
use crusty_core::tool::{Tool, ToolCategory, ToolContext, ToolError, ToolOutput, input_str};
use serde_json::{Value, json};

use crate::sandbox::{display_path, is_protected_raw, resolve_path};
use crate::tracker::FileTracker;
use crate::{file_subject, is_binary, opt_u64};

/// Lines returned when `limit` is not given.
pub(crate) const DEFAULT_LINE_LIMIT: u64 = 2000;
/// Largest `limit` a caller can ask for.
const MAX_LINE_LIMIT: u64 = 5000;
/// Text files above this size are refused rather than read whole.
const MAX_TEXT_BYTES: u64 = 50 * 1024 * 1024;
/// Longer lines are cut to this many characters and marked with `…`.
const MAX_LINE_CHARS: usize = 2000;
const MAX_IMAGE_BYTES: u64 = 5 * 1024 * 1024;

/// Reads files for the model and records each read, so later edits can be checked.
pub struct ReadTool {
    tracker: Arc<FileTracker>,
}

impl ReadTool {
    pub fn new(tracker: Arc<FileTracker>) -> Self {
        ReadTool { tracker }
    }
}

#[async_trait]
impl Tool for ReadTool {
    fn name(&self) -> &str {
        "Read"
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "Read",
            "Reads a file from the workspace and returns its lines numbered from 1. Use offset and limit for large files. \
             Image files (png, jpg, gif, webp) are returned as images.",
            json!({
                "type": "object",
                "properties": {
                    "file_path": {"type": "string", "description": "Path of the file, absolute or relative to the workspace."},
                    "offset": {"type": "integer", "minimum": 1, "description": "1-based line to start from."},
                    "limit": {"type": "integer", "minimum": 1, "description": "Number of lines to return (default 2000)."}
                },
                "required": ["file_path"]
            }),
        )
    }

    fn category(&self) -> ToolCategory {
        ToolCategory::Read
    }

    fn assess(&self, input: &Value, ctx: &ToolContext) -> RiskAssessment {
        if input_str(input, "file_path").is_ok_and(|raw| is_protected_raw(ctx, raw)) {
            RiskAssessment::new(RiskLevel::Critical, "reads a protected file")
        } else {
            RiskAssessment::new(RiskLevel::Low, "reads a file")
        }
    }

    fn permission_subject(&self, input: &Value) -> Option<String> {
        file_subject(input, "file_path")
    }

    async fn call(&self, input: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let path = resolve_path(ctx, input_str(&input, "file_path")?)?;
        let max_bytes = ctx.config.security.max_tool_output_bytes;
        let job_path = path.clone();
        // Disk reads can block for a long time, so they run off the async runtime.
        let output = tokio::task::spawn_blocking(move || match image_media_type(&job_path) {
            Some(media_type) => read_image(&job_path, media_type),
            None => read_text(&job_path, &input, max_bytes),
        })
        .await
        .map_err(|e| ToolError::Failed(format!("read task failed: {e}")))??;
        self.tracker.record_read(&path);
        Ok(output)
    }
}

/// Metadata of a regular file. FIFOs, devices and directories are refused before any read.
fn regular_file_metadata(path: &Path) -> Result<std::fs::Metadata, ToolError> {
    let meta = std::fs::metadata(path)?;
    if !meta.is_file() {
        return Err(ToolError::Failed(format!("{} is not a regular file", display_path(path))));
    }
    Ok(meta)
}

fn image_media_type(path: &Path) -> Option<&'static str> {
    match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        _ => None,
    }
}

fn read_image(path: &Path, media_type: &str) -> Result<ToolOutput, ToolError> {
    let name = display_path(path);
    if regular_file_metadata(path)?.len() > MAX_IMAGE_BYTES {
        return Err(ToolError::Failed(format!("{name} is larger than 5 MiB")));
    }
    let bytes = std::fs::read(path)?;
    Ok(ToolOutput {
        content: vec![
            ContentBlock::Image {
                source: ImageSource::Base64 { media_type: media_type.to_owned(), data: STANDARD.encode(&bytes) },
                extra: Extra::new(),
            },
            ContentBlock::text(format!("Image {name} ({} bytes)", bytes.len())),
        ],
        is_error: false,
        preview: format!("image {name}"),
        exit_code: None,
    })
}

fn read_text(path: &Path, input: &Value, max_bytes: usize) -> Result<ToolOutput, ToolError> {
    let meta = regular_file_metadata(path)?;
    if meta.len() > MAX_TEXT_BYTES {
        return Err(ToolError::Failed(format!(
            "{} is larger than 50 MiB; read it in parts with offset and limit, or search it with Grep",
            display_path(path)
        )));
    }
    let bytes = std::fs::read(path)?;
    if bytes.is_empty() {
        return Ok(ToolOutput::text("<system-reminder>file exists but is empty</system-reminder>"));
    }
    if is_binary(&bytes) {
        return Err(ToolError::Failed(format!("{} is a binary file and cannot be shown as text", display_path(path))));
    }
    let offset = opt_u64(input, "offset")?.unwrap_or(1);
    let limit = opt_u64(input, "limit")?.unwrap_or(DEFAULT_LINE_LIMIT).min(MAX_LINE_LIMIT);
    let text = render_lines(&String::from_utf8_lossy(&bytes), offset, limit)?;
    Ok(ToolOutput::text(cap_output(text, max_bytes)))
}

/// Keeps `text` within `max_bytes`. A longer text is cut at the last line that fits, followed
/// by a note saying how to read the rest.
fn cap_output(text: String, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text;
    }
    const NOTE: &str = "\n[truncated: use offset/limit]";
    let mut budget = max_bytes.saturating_sub(NOTE.len());
    while !text.is_char_boundary(budget) {
        budget -= 1;
    }
    let cut = text[..budget].rfind('\n').unwrap_or(0);
    format!("{}{NOTE}", &text[..cut])
}

/// Numbers the lines of `text` in `cat -n` style, starting at the 1-based line `offset` and
/// returning at most `limit` lines. A truncated range ends with a note saying what was shown.
pub(crate) fn render_lines(text: &str, offset: u64, limit: u64) -> Result<String, ToolError> {
    if offset == 0 {
        return Err(ToolError::InvalidInput("offset starts at 1".into()));
    }
    if limit == 0 {
        return Err(ToolError::InvalidInput("limit must be at least 1".into()));
    }
    let lines: Vec<&str> = text.lines().collect();
    let total = lines.len();
    if total == 0 {
        return Ok(String::new());
    }
    let start = usize::try_from(offset).unwrap_or(usize::MAX);
    if start > total {
        return Err(ToolError::InvalidInput(format!("offset {offset} is past the end of the file ({total} lines)")));
    }
    let count = usize::try_from(limit).unwrap_or(usize::MAX);
    // `end` is the last shown line, 1-based, and the exclusive end of the slice.
    let end = (start - 1).saturating_add(count).min(total);
    let body: Vec<String> = lines[start - 1..end]
        .iter()
        .enumerate()
        .map(|(i, line)| format!("{:>6}\t{}", start + i, truncate_chars(line, MAX_LINE_CHARS)))
        .collect();
    let mut out = body.join("\n");
    if end < total {
        out.push_str(&format!("\n[showing lines {start}-{end} of {total}]"));
    }
    Ok(out)
}

fn truncate_chars(line: &str, max: usize) -> String {
    if line.chars().count() <= max {
        return line.to_owned();
    }
    let mut cut: String = line.chars().take(max).collect();
    cut.push('…');
    cut
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::context;
    use tempfile::tempdir;

    fn tool() -> (ReadTool, Arc<FileTracker>) {
        let tracker = Arc::new(FileTracker::new());
        (ReadTool::new(tracker.clone()), tracker)
    }

    #[tokio::test]
    async fn numbers_lines_and_records_the_read() {
        let dir = tempdir().unwrap();
        let ctx = context(&dir);
        std::fs::write(dir.path().join("a.txt"), "one\ntwo\n").unwrap();
        let (read, tracker) = tool();
        let out = read.call(json!({"file_path": "a.txt"}), &ctx).await.unwrap();
        assert_eq!(out.text_content(), "     1\tone\n     2\ttwo");
        assert!(tracker.check_can_write(&ctx.workspace.join("a.txt")).is_ok());
    }

    #[tokio::test]
    async fn offset_and_limit_report_truncation() {
        let dir = tempdir().unwrap();
        let ctx = context(&dir);
        let body: String = (1..=10).map(|n| format!("line {n}\n")).collect();
        std::fs::write(dir.path().join("a.txt"), body).unwrap();
        let (read, _) = tool();
        let out = read.call(json!({"file_path": "a.txt", "offset": 4, "limit": 2}), &ctx).await.unwrap();
        assert_eq!(out.text_content(), "     4\tline 4\n     5\tline 5\n[showing lines 4-5 of 10]");
    }

    #[test]
    fn long_lines_are_cut() {
        let text = "x".repeat(2500);
        let rendered = render_lines(&text, 1, 10).unwrap();
        assert!(rendered.ends_with('…'));
        assert_eq!(rendered.chars().filter(|c| *c == 'x').count(), 2000);
    }

    #[test]
    fn offset_past_end_is_an_error() {
        assert!(render_lines("a\nb", 3, 10).is_err());
        assert!(render_lines("a\nb", 0, 10).is_err());
    }

    #[tokio::test]
    async fn empty_file_gets_a_note() {
        let dir = tempdir().unwrap();
        let ctx = context(&dir);
        std::fs::write(dir.path().join("empty.txt"), "").unwrap();
        let (read, _) = tool();
        let out = read.call(json!({"file_path": "empty.txt"}), &ctx).await.unwrap();
        assert!(out.text_content().contains("file exists but is empty"));
    }

    #[tokio::test]
    async fn binary_file_is_refused() {
        let dir = tempdir().unwrap();
        let ctx = context(&dir);
        std::fs::write(dir.path().join("b.bin"), [0u8, 1, 2, 3]).unwrap();
        let (read, _) = tool();
        assert!(matches!(read.call(json!({"file_path": "b.bin"}), &ctx).await, Err(ToolError::Failed(_))));
    }

    #[tokio::test]
    async fn image_is_returned_as_image_block() {
        let dir = tempdir().unwrap();
        let ctx = context(&dir);
        std::fs::write(dir.path().join("pic.png"), [0x89u8, b'P', b'N', b'G']).unwrap();
        let (read, _) = tool();
        let out = read.call(json!({"file_path": "pic.png"}), &ctx).await.unwrap();
        match &out.content[0] {
            ContentBlock::Image { source: ImageSource::Base64 { media_type, .. }, .. } => assert_eq!(media_type, "image/png"),
            other => panic!("expected image block, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn protected_file_is_critical_risk() {
        let dir = tempdir().unwrap();
        let ctx = context(&dir);
        let (read, _) = tool();
        assert_eq!(read.assess(&json!({"file_path": ".env"}), &ctx).level, RiskLevel::Critical);
        assert_eq!(read.assess(&json!({"file_path": "src/lib.rs"}), &ctx).level, RiskLevel::Low);
    }

    #[tokio::test]
    async fn directories_and_non_regular_files_are_refused() {
        let dir = tempdir().unwrap();
        let ctx = context(&dir);
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        let (read, _) = tool();
        assert!(matches!(read.call(json!({"file_path": "sub"}), &ctx).await, Err(ToolError::Failed(_))));
    }

    #[test]
    fn output_is_capped_at_a_line_boundary() {
        let text: String = (1..=100).map(|n| format!("line {n:03}\n")).collect();
        let capped = cap_output(text.clone(), 200);
        assert!(capped.len() <= 200);
        assert!(capped.ends_with("[truncated: use offset/limit]"));
        let kept = capped.trim_end_matches("\n[truncated: use offset/limit]");
        assert!(text.starts_with(&format!("{kept}\n")));
        assert_eq!(cap_output("short".to_owned(), 200), "short");
    }

    #[test]
    fn line_limit_is_capped() {
        let body: String = (1..=6000).map(|n| format!("{n}\n")).collect();
        let rendered = render_lines(&body, 1, MAX_LINE_LIMIT.min(9999)).unwrap();
        assert!(rendered.ends_with("[showing lines 1-5000 of 6000]"));
    }
}
