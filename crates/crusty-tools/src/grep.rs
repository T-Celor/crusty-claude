//! The `Grep` tool: regex search over workspace files, in three output modes.

use std::path::Path;

use async_trait::async_trait;
use crusty_core::message::ToolSpec;
use crusty_core::permission::RiskAssessment;
use crusty_core::tool::{Tool, ToolCategory, ToolContext, ToolError, ToolOutput, input_opt_str, input_str};
use globset::GlobMatcher;
use regex::{Regex, RegexBuilder};
use serde_json::{Value, json};

use crate::glob::{search_base, search_risk, walker};
use crate::sandbox::compile_glob;
use crate::{display_rel, is_binary, opt_bool, search_subject, to_slashes};

/// Files larger than this are skipped.
const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;
const DEFAULT_HEAD_LIMIT: usize = 250;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutputMode {
    Files,
    Content,
    Count,
}

/// Parsed search request.
struct Query {
    regex: Regex,
    multiline: bool,
    mode: OutputMode,
    before: usize,
    after: usize,
    numbers: bool,
    head_limit: usize,
    glob: Option<GlobMatcher>,
    extensions: Option<Vec<String>>,
}

/// Count argument: absent means `default`; `0` is allowed.
fn count_arg(input: &Value, key: &str, default: usize) -> Result<usize, ToolError> {
    Ok(match crate::opt_u64(input, key)? {
        Some(n) => usize::try_from(n).unwrap_or(usize::MAX),
        None => default,
    })
}

/// Extensions for a `type` name such as `rs` or `py`. Unknown names are used as extensions.
fn type_extensions(name: &str) -> Vec<String> {
    let known: &[&str] = match name {
        "rs" => &["rs"],
        "py" => &["py", "pyi"],
        "js" => &["js", "mjs", "cjs", "jsx"],
        "ts" => &["ts", "mts", "cts", "tsx"],
        "go" => &["go"],
        "java" => &["java"],
        "kt" => &["kt", "kts"],
        "c" => &["c", "h"],
        "cpp" => &["cc", "cpp", "cxx", "hh", "hpp", "h"],
        "cs" => &["cs"],
        "rb" => &["rb"],
        "php" => &["php"],
        "sh" => &["sh", "bash"],
        "html" => &["html", "htm"],
        "css" => &["css"],
        "sql" => &["sql"],
        "swift" => &["swift"],
        "md" => &["md", "markdown"],
        "json" => &["json"],
        "toml" => &["toml"],
        "yaml" => &["yaml", "yml"],
        "xml" => &["xml"],
        "txt" => &["txt"],
        other => return vec![other.to_owned()],
    };
    known.iter().map(|ext| (*ext).to_owned()).collect()
}

impl Query {
    fn from_input(input: &Value) -> Result<Self, ToolError> {
        let multiline = opt_bool(input, "multiline")?;
        let regex = RegexBuilder::new(input_str(input, "pattern")?)
            .case_insensitive(opt_bool(input, "-i")?)
            .multi_line(multiline)
            .dot_matches_new_line(multiline)
            .build()
            .map_err(|e| ToolError::InvalidInput(format!("invalid regex: {e}")))?;
        let mode = match input_opt_str(input, "output_mode").unwrap_or("files_with_matches") {
            "files_with_matches" => OutputMode::Files,
            "content" => OutputMode::Content,
            "count" => OutputMode::Count,
            other => return Err(ToolError::InvalidInput(format!("unknown output_mode `{other}`"))),
        };
        let context = count_arg(input, "-C", 0)?;
        let head_limit = match count_arg(input, "head_limit", DEFAULT_HEAD_LIMIT)? {
            0 => usize::MAX,
            n => n,
        };
        let glob = input_opt_str(input, "glob")
            .map(compile_glob)
            .transpose()
            .map_err(|e| ToolError::InvalidInput(format!("invalid glob: {e}")))?;
        Ok(Query {
            regex,
            multiline,
            mode,
            before: count_arg(input, "-B", context)?,
            after: count_arg(input, "-A", context)?,
            numbers: input.get("-n").and_then(Value::as_bool).unwrap_or(true),
            head_limit,
            glob,
            extensions: input_opt_str(input, "type").map(type_extensions),
        })
    }

    /// Whether a file is worth reading, given the glob, type and size filters.
    fn accepts(&self, path: &Path, base: &Path, size: Option<u64>) -> bool {
        if size.is_some_and(|len| len > MAX_FILE_BYTES) {
            return false;
        }
        if let Some(glob) = &self.glob
            && !glob.is_match(to_slashes(path.strip_prefix(base).unwrap_or(path)))
        {
            return false;
        }
        match &self.extensions {
            Some(exts) => path.extension().and_then(|e| e.to_str()).is_some_and(|e| exts.iter().any(|x| x.eq_ignore_ascii_case(e))),
            None => true,
        }
    }
}

/// Which lines of `text` match. In multiline mode a match marks every line it spans.
fn matched_lines(text: &str, lines: &[&str], query: &Query) -> Vec<bool> {
    if !query.multiline {
        return lines.iter().map(|line| query.regex.is_match(line)).collect();
    }
    let mut flags = vec![false; lines.len()];
    if lines.is_empty() {
        return flags;
    }
    let starts: Vec<usize> = std::iter::once(0).chain(text.match_indices('\n').map(|(i, _)| i + 1)).collect();
    let last = lines.len() - 1;
    for m in query.regex.find_iter(text) {
        let first = (starts.partition_point(|&s| s <= m.start()) - 1).min(last);
        let last_byte = m.end().max(m.start() + 1) - 1;
        let line_of_end = (starts.partition_point(|&s| s <= last_byte) - 1).min(last);
        if first <= line_of_end {
            for flag in &mut flags[first..=line_of_end] {
                *flag = true;
            }
        }
    }
    flags
}

/// Output lines for one file in content mode: matches with `:`, context with `-`, and `--`
/// between separated groups.
fn render_content(rel: &str, lines: &[&str], matched: &[bool], query: &Query) -> Vec<String> {
    let mut show = vec![false; lines.len()];
    for (i, _) in matched.iter().enumerate().filter(|(_, m)| **m) {
        let lo = i.saturating_sub(query.before);
        let hi = (i + query.after).min(lines.len() - 1);
        for flag in &mut show[lo..=hi] {
            *flag = true;
        }
    }
    let mut out = Vec::new();
    let mut previous: Option<usize> = None;
    for (i, line) in lines.iter().enumerate().filter(|(i, _)| show[*i]) {
        if previous.is_some_and(|p| i > p + 1) {
            out.push("--".to_owned());
        }
        previous = Some(i);
        let sep = if matched[i] { ':' } else { '-' };
        out.push(if query.numbers { format!("{rel}{sep}{}{sep}{line}", i + 1) } else { format!("{rel}{sep}{line}") });
    }
    out
}

/// Runs the search synchronously. Results are sorted by path and capped at `head_limit`.
fn search(base: &Path, workspace: &Path, query: &Query) -> String {
    let mut results: Vec<(String, Vec<String>)> = Vec::new();
    for entry in walker(base).build().flatten() {
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let path = entry.path();
        let size = entry.metadata().ok().map(|m| m.len());
        if !query.accepts(path, base, size) {
            continue;
        }
        let Ok(bytes) = std::fs::read(path) else {
            continue;
        };
        if is_binary(&bytes) {
            continue;
        }
        let Ok(text) = std::str::from_utf8(&bytes) else {
            continue;
        };
        let lines: Vec<&str> = text.lines().collect();
        let matched = matched_lines(text, &lines, query);
        let count = matched.iter().filter(|m| **m).count();
        if count == 0 {
            continue;
        }
        let rel = display_rel(path, workspace);
        let rendered = match query.mode {
            OutputMode::Files => vec![rel.clone()],
            OutputMode::Count => vec![format!("{rel}:{count}")],
            OutputMode::Content => render_content(&rel, &lines, &matched, query),
        };
        results.push((rel, rendered));
    }
    results.sort_by(|a, b| a.0.cmp(&b.0));

    let total: usize = results.iter().map(|(_, lines)| lines.len()).sum();
    let shown: Vec<String> = results.into_iter().flat_map(|(_, lines)| lines).take(query.head_limit).collect();
    if shown.is_empty() {
        return "No matches found".to_owned();
    }
    let mut out = shown.join("\n");
    if total > query.head_limit {
        out.push_str(&format!("\n[showing first {} of {total} entries]", query.head_limit));
    }
    out
}

/// Searches file contents with a regex.
pub struct GrepTool;

#[async_trait]
impl Tool for GrepTool {
    fn name(&self) -> &str {
        "Grep"
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "Grep",
            "Searches file contents with a regular expression, respecting .gitignore. output_mode selects \
             files_with_matches (default), content (path:line:text with optional context) or count.",
            json!({
                "type": "object",
                "properties": {
                    "pattern": {"type": "string", "description": "Regular expression."},
                    "path": {"type": "string", "description": "File or directory to search (default: the workspace)."},
                    "glob": {"type": "string", "description": "Only search files whose path matches this glob, e.g. `*.ts`."},
                    "type": {"type": "string", "description": "File type, e.g. rs, py, js, ts, go, md, json."},
                    "output_mode": {"type": "string", "enum": ["files_with_matches", "content", "count"]},
                    "-i": {"type": "boolean", "description": "Case-insensitive."},
                    "-n": {"type": "boolean", "description": "Show line numbers in content mode (default true)."},
                    "-A": {"type": "integer", "minimum": 0, "description": "Lines of context after each match."},
                    "-B": {"type": "integer", "minimum": 0, "description": "Lines of context before each match."},
                    "-C": {"type": "integer", "minimum": 0, "description": "Lines of context before and after."},
                    "multiline": {"type": "boolean", "description": "Let patterns span lines."},
                    "head_limit": {"type": "integer", "minimum": 0, "description": "Maximum entries returned (default 250, 0 for no limit)."}
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
        let query = Query::from_input(&input)?;
        let base = search_base(ctx, &input)?;
        let workspace = ctx.workspace.clone();
        let text =
            tokio::task::spawn_blocking(move || search(&base, &workspace, &query)).await.map_err(|e| ToolError::Failed(e.to_string()))?;
        Ok(ToolOutput::text(text))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::context;
    use tempfile::tempdir;

    fn write(dir: &tempfile::TempDir, name: &str, body: &str) {
        let path = dir.path().join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    #[tokio::test]
    async fn files_mode_lists_matching_files_sorted() {
        let dir = tempdir().unwrap();
        let ctx = context(&dir);
        write(&dir, "b.txt", "needle\n");
        write(&dir, "a.txt", "hay\nneedle\n");
        write(&dir, "c.txt", "hay\n");
        let out = GrepTool.call(json!({"pattern": "needle"}), &ctx).await.unwrap();
        assert_eq!(out.text_content(), "a.txt\nb.txt");
    }

    #[tokio::test]
    async fn content_mode_shows_line_numbers_and_context() {
        let dir = tempdir().unwrap();
        let ctx = context(&dir);
        write(&dir, "f.txt", "one\ntwo\nthree\nfour\nfive\n");
        let out = GrepTool.call(json!({"pattern": "three", "output_mode": "content", "-C": 1}), &ctx).await.unwrap();
        assert_eq!(out.text_content(), "f.txt-2-two\nf.txt:3:three\nf.txt-4-four");
    }

    #[tokio::test]
    async fn count_mode_and_case_insensitive() {
        let dir = tempdir().unwrap();
        let ctx = context(&dir);
        write(&dir, "f.txt", "Foo\nfoo\nbar\n");
        let out = GrepTool.call(json!({"pattern": "foo", "output_mode": "count", "-i": true}), &ctx).await.unwrap();
        assert_eq!(out.text_content(), "f.txt:2");
    }

    #[tokio::test]
    async fn type_and_glob_filters_narrow_the_search() {
        let dir = tempdir().unwrap();
        let ctx = context(&dir);
        write(&dir, "a.rs", "todo\n");
        write(&dir, "b.py", "todo\n");
        let by_type = GrepTool.call(json!({"pattern": "todo", "type": "rs"}), &ctx).await.unwrap();
        assert_eq!(by_type.text_content(), "a.rs");
        let by_glob = GrepTool.call(json!({"pattern": "todo", "glob": "*.py"}), &ctx).await.unwrap();
        assert_eq!(by_glob.text_content(), "b.py");
    }

    #[tokio::test]
    async fn gitignored_and_binary_files_are_skipped() {
        let dir = tempdir().unwrap();
        let ctx = context(&dir);
        write(&dir, ".gitignore", "ignored.txt\n");
        write(&dir, "ignored.txt", "needle\n");
        write(&dir, "kept.txt", "needle\n");
        std::fs::write(dir.path().join("blob.bin"), [0u8, b'n', b'e', b'e', b'd', b'l', b'e']).unwrap();
        let out = GrepTool.call(json!({"pattern": "needle"}), &ctx).await.unwrap();
        assert_eq!(out.text_content(), "kept.txt");
    }

    #[tokio::test]
    async fn head_limit_truncates_with_note() {
        let dir = tempdir().unwrap();
        let ctx = context(&dir);
        write(&dir, "f.txt", "x\nx\nx\nx\n");
        let out = GrepTool.call(json!({"pattern": "x", "output_mode": "content", "-n": false, "head_limit": 2}), &ctx).await.unwrap();
        assert_eq!(out.text_content(), "f.txt:x\nf.txt:x\n[showing first 2 of 4 entries]");
    }

    #[tokio::test]
    async fn invalid_regex_is_an_input_error() {
        let dir = tempdir().unwrap();
        let ctx = context(&dir);
        let result = GrepTool.call(json!({"pattern": "("}), &ctx).await;
        assert!(matches!(result, Err(ToolError::InvalidInput(_))));
    }

    #[test]
    fn multiline_match_spans_lines() {
        let query = Query::from_input(&json!({"pattern": "a.b", "multiline": true, "output_mode": "count"})).unwrap();
        let text = "a\nb\nc\n";
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(matched_lines(text, &lines, &query), vec![true, true, false]);
    }
}
