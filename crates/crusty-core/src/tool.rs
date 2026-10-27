//! The tool contract. Built-in tools live in `crusty-tools`, MCP tools in `crusty-mcp`,
//! delegation (`Task`) in `crusty-agent`. Every call goes through the permission engine
//! before `call` runs; tools never decide on their own that they are allowed.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::advisor::{Advisor, NoAdvisor};
use crate::checkpoint::{Checkpointer, NoCheckpoints};
use crate::config::Config;
use crate::event::EventSink;
use crate::filter::{ContentFilter, NoFilter};
use crate::message::{ContentBlock, ToolSpec, blocks_text};
use crate::permission::RiskAssessment;
use crate::store::OriginalStore;

/// What a tool does, used by permission modes (`plan` denies `Edit` and `Execute`,
/// `acceptEdits` auto-allows `Edit` inside the workspace).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCategory {
    Read,
    Edit,
    Execute,
    Network,
    Delegate,
    Other,
}

/// Everything a tool call may use. Cheap to clone.
#[derive(Clone)]
pub struct ToolContext {
    pub agent_id: String,
    pub session_id: String,
    /// Canonical project root; commands run here.
    pub workspace: PathBuf,
    /// Canonical roots file tools may touch: the workspace plus configured extra dirs.
    pub allowed_roots: Vec<PathBuf>,
    pub cancel: CancellationToken,
    pub events: EventSink,
    pub store: Arc<OriginalStore>,
    pub config: Arc<Config>,
    /// RTK and HTML→Markdown filters (see `filter::choose_filtered`).
    pub filters: Arc<dyn ContentFilter>,
    /// Laya or the native heuristics: reranking, log triage.
    pub advisor: Arc<dyn Advisor>,
    /// File snapshots for `/rewind`; file-changing tools call it before writing.
    pub checkpoints: Arc<dyn Checkpointer>,
    /// What the calling agent is working on (the user's prompt, or the delegated task),
    /// used to rank search results and log blocks.
    pub goal: Arc<str>,
}

impl ToolContext {
    /// A context for tests: workspace `dir`, default config, no event consumer, no
    /// advisor, no checkpoints.
    pub fn for_tests(dir: impl Into<PathBuf>) -> Self {
        let dir = dir.into();
        let dir = dir.canonicalize().unwrap_or(dir);
        ToolContext {
            agent_id: "test".into(),
            session_id: "test".into(),
            workspace: dir.clone(),
            allowed_roots: vec![dir],
            cancel: CancellationToken::new(),
            events: EventSink::noop(),
            store: Arc::new(OriginalStore::default()),
            config: Arc::new(Config::default()),
            filters: Arc::new(NoFilter),
            advisor: Arc::new(NoAdvisor),
            checkpoints: Arc::new(NoCheckpoints),
            goal: Arc::from(""),
        }
    }
}

/// The file change a call would make, computed before it runs (for the write guard and
/// the approval preview).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedChange {
    pub path: PathBuf,
    /// Current content; `None` when the file does not exist yet.
    pub before: Option<String>,
    pub after: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ToolOutput {
    /// Text and image blocks returned to the model.
    pub content: Vec<ContentBlock>,
    pub is_error: bool,
    /// Short line for the UI.
    pub preview: String,
    pub exit_code: Option<i32>,
}

impl ToolOutput {
    pub fn text(text: impl Into<String>) -> Self {
        let text = text.into();
        ToolOutput { preview: preview_of(&text), content: vec![ContentBlock::text(text)], is_error: false, exit_code: None }
    }

    pub fn error(text: impl Into<String>) -> Self {
        ToolOutput { is_error: true, ..ToolOutput::text(text) }
    }

    pub fn with_exit_code(mut self, code: Option<i32>) -> Self {
        self.exit_code = code;
        self
    }

    pub fn text_content(&self) -> String {
        blocks_text(&self.content)
    }
}

/// First non-empty line, at most 120 characters.
pub fn preview_of(text: &str) -> String {
    let line = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim();
    let mut out: String = line.chars().take(120).collect();
    if line.chars().count() > 120 {
        out.push('…');
    }
    out
}

#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum ToolError {
    #[error("invalid input: {0}")]
    InvalidInput(String),
    #[error("denied: {0}")]
    Denied(String),
    #[error("{0}")]
    Io(String),
    #[error("cancelled")]
    Cancelled,
    #[error("timed out after {0} s")]
    Timeout(u64),
    #[error("{0}")]
    Failed(String),
}

impl From<std::io::Error> for ToolError {
    fn from(e: std::io::Error) -> Self {
        ToolError::Io(e.to_string())
    }
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;

    /// Name, description and JSON schema sent to the model.
    fn spec(&self) -> ToolSpec;

    fn category(&self) -> ToolCategory;

    /// Never changes state, so it may run concurrently with other read-only calls.
    fn is_read_only(&self) -> bool {
        matches!(self.category(), ToolCategory::Read)
    }

    /// Risk of this concrete call, from its input.
    fn assess(&self, input: &Value, ctx: &ToolContext) -> RiskAssessment;

    /// The part of the input that permission rules match: the command for `Bash`, the path
    /// for file tools, `domain:<host>` for `WebFetch`.
    fn permission_subject(&self, _input: &Value) -> Option<String> {
        None
    }

    /// One line for the UI, e.g. `Bash(cargo test)`.
    fn summarize(&self, input: &Value) -> String {
        match self.permission_subject(input) {
            Some(s) => format!("{}({})", self.name(), preview_of(&s)),
            None => self.name().to_owned(),
        }
    }

    /// For file-changing tools: the change this call would make, without making it.
    /// `None` when the tool changes no file or the change cannot be computed (the call
    /// itself will then report the error).
    fn planned_change(&self, _input: &Value, _ctx: &ToolContext) -> Option<PlannedChange> {
        None
    }

    async fn call(&self, input: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError>;
}

/// Reads a required string field of a tool input.
pub fn input_str<'a>(input: &'a Value, key: &str) -> Result<&'a str, ToolError> {
    input.get(key).and_then(Value::as_str).ok_or_else(|| ToolError::InvalidInput(format!("missing string field `{key}`")))
}

/// Reads an optional string field of a tool input.
pub fn input_opt_str<'a>(input: &'a Value, key: &str) -> Option<&'a str> {
    input.get(key).and_then(Value::as_str)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_takes_first_non_empty_line() {
        assert_eq!(preview_of("\n\n  hello \nworld"), "hello");
        assert_eq!(preview_of(&"x".repeat(200)).chars().count(), 121);
    }

    #[test]
    fn input_helpers() {
        let v = serde_json::json!({"a": "b"});
        assert_eq!(input_str(&v, "a").unwrap(), "b");
        assert!(input_str(&v, "z").is_err());
        assert_eq!(input_opt_str(&v, "z"), None);
    }
}
