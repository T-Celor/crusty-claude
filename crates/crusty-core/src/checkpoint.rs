//! File checkpoints for `/rewind`. File-changing tools call `before_change` before they
//! touch a file; the store (`crusty-session`) keeps the content the file had when the
//! current checkpoint (one per user prompt) began, once per file and checkpoint.
//! Changes made by shell commands are not tracked.

use std::path::Path;

use serde::{Deserialize, Serialize};

pub trait Checkpointer: Send + Sync {
    /// Records the current content of `path` (or that it does not exist) for the current
    /// checkpoint, unless it was already recorded. An error means the snapshot was not
    /// taken; the tool reports it and still runs.
    fn before_change(&self, path: &Path) -> Result<(), String>;

    /// Starts the checkpoint of a new user prompt and returns its id; `None` when
    /// checkpoints are disabled.
    fn begin(&self, _label: &str, _message_index: usize) -> Option<u32> {
        None
    }

    /// Checkpoints of the session, oldest first.
    fn list(&self) -> Vec<CheckpointSummary> {
        Vec::new()
    }

    /// Restores every file touched since checkpoint `id` began to its content at that
    /// moment, then drops the later checkpoints.
    fn rewind(&self, _id: u32) -> Result<RewindReport, String> {
        Err("checkpoints are disabled".into())
    }
}

/// Result of a code rewind.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RewindReport {
    /// Files written back or deleted.
    pub files_restored: usize,
    /// Files that could not be restored, with the reason.
    pub failures: Vec<String>,
}

/// Takes no snapshot (checkpoints disabled, tests).
#[derive(Debug, Clone, Copy, Default)]
pub struct NoCheckpoints;

impl Checkpointer for NoCheckpoints {
    fn before_change(&self, _path: &Path) -> Result<(), String> {
        Ok(())
    }
}

/// One checkpoint as listed by `/rewind`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointSummary {
    /// 1-based, in conversation order.
    pub id: u32,
    /// The prompt that started it, on one line.
    pub label: String,
    /// Files changed after it began.
    pub files: usize,
    /// RFC 3339.
    pub created: String,
    /// Index of the prompt's message in the conversation, for conversation rewinds.
    pub message_index: usize,
}

/// What `/rewind` restores.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RewindScope {
    /// Files only; the conversation continues.
    Code,
    /// Conversation only; files stay as they are.
    Conversation,
    #[default]
    Both,
}

impl RewindScope {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "code" | "files" => Some(RewindScope::Code),
            "conversation" | "chat" | "convo" => Some(RewindScope::Conversation),
            "both" | "all" => Some(RewindScope::Both),
            _ => None,
        }
    }

    pub fn restores_code(self) -> bool {
        matches!(self, RewindScope::Code | RewindScope::Both)
    }

    pub fn restores_conversation(self) -> bool {
        matches!(self, RewindScope::Conversation | RewindScope::Both)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scopes_parse() {
        assert_eq!(RewindScope::parse("files"), Some(RewindScope::Code));
        assert!(RewindScope::Both.restores_code() && RewindScope::Both.restores_conversation());
        assert!(!RewindScope::Code.restores_conversation());
        assert_eq!(RewindScope::parse("x"), None);
    }
}
