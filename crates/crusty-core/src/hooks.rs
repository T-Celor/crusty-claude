//! Lifecycle hooks (Claude Code compatible names). The shell runner lives in
//! `crusty-session`.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum HookEvent {
    PreToolUse,
    PostToolUse,
    UserPromptSubmit,
    Stop,
    SubagentStop,
    SessionStart,
    SessionEnd,
    PreCompact,
    Notification,
}

impl HookEvent {
    pub const ALL: [HookEvent; 9] = [
        HookEvent::PreToolUse,
        HookEvent::PostToolUse,
        HookEvent::UserPromptSubmit,
        HookEvent::Stop,
        HookEvent::SubagentStop,
        HookEvent::SessionStart,
        HookEvent::SessionEnd,
        HookEvent::PreCompact,
        HookEvent::Notification,
    ];

    pub fn name(self) -> &'static str {
        match self {
            HookEvent::PreToolUse => "PreToolUse",
            HookEvent::PostToolUse => "PostToolUse",
            HookEvent::UserPromptSubmit => "UserPromptSubmit",
            HookEvent::Stop => "Stop",
            HookEvent::SubagentStop => "SubagentStop",
            HookEvent::SessionStart => "SessionStart",
            HookEvent::SessionEnd => "SessionEnd",
            HookEvent::PreCompact => "PreCompact",
            HookEvent::Notification => "Notification",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        HookEvent::ALL.into_iter().find(|e| e.name().eq_ignore_ascii_case(s))
    }
}

/// What the hooks of one event decided, merged across all matching hooks.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct HookOutcome {
    /// Stop the action (tool call, prompt, stop) and tell the model `message`.
    pub block: bool,
    pub message: Option<String>,
    /// Text added to the model's context.
    pub additional_context: Option<String>,
    /// Replacement tool input (PreToolUse only).
    pub updated_input: Option<Value>,
}

#[async_trait]
pub trait HookRunner: Send + Sync {
    /// `tool` is the tool name for tool events, matched against each hook's matcher.
    async fn run(&self, event: HookEvent, tool: Option<&str>, payload: &Value) -> HookOutcome;
}

#[derive(Debug, Default, Clone, Copy)]
pub struct NoopHooks;

#[async_trait]
impl HookRunner for NoopHooks {
    async fn run(&self, _event: HookEvent, _tool: Option<&str>, _payload: &Value) -> HookOutcome {
        HookOutcome::default()
    }
}
