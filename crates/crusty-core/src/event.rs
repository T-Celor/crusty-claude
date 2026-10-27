//! Typed events from the engine to the UI, telemetry and transcript, and commands back
//! from the UI. Every number the TUI shows comes from these events (see `metrics`).

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::mpsc;

use crate::message::Usage;
use crate::metrics::Metrics;
use crate::permission::{ApprovalDecision, ApprovalRequest, PermissionMode};

/// Optimization stages, in pipeline order. Also the order of the TUI side panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    Ponytail,
    Rtk,
    HtmlToMarkdown,
    Graphify,
    Headroom,
    ContextGate,
    Pxpipe,
    Proxy,
}

impl Stage {
    pub const ALL: [Stage; 8] = [
        Stage::Ponytail,
        Stage::Rtk,
        Stage::HtmlToMarkdown,
        Stage::Graphify,
        Stage::Headroom,
        Stage::ContextGate,
        Stage::Pxpipe,
        Stage::Proxy,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Stage::Ponytail => "Ponytail",
            Stage::Rtk => "RTK",
            Stage::HtmlToMarkdown => "HTML→MD",
            Stage::Graphify => "Graphify",
            Stage::Headroom => "Headroom",
            Stage::ContextGate => "Gate",
            Stage::Pxpipe => "pxpipe",
            Stage::Proxy => "Proxy",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StageStatus {
    /// The transformed content was used.
    Applied,
    /// Observe mode: computed and measured, the original was used.
    Observed,
    /// Not worth it or not applicable (`detail` says why).
    Skipped,
    /// The stage failed or was unavailable and the safe path was used instead.
    Fallback,
    Failed,
}

/// Measurement of one stage run. Sizes are measured; `*_est` tokens are estimates
/// (4 chars per token, images by pixel area); `*_real` tokens come from a provider.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StageMetrics {
    pub stage: Stage,
    /// What was processed: `tool_output:cargo`, `html`, `request`, ...
    pub content_kind: String,
    /// Which transform ran: `rtk:git-status`, `headroom:json-crush`, ...
    pub transformer: String,
    pub status: StageStatus,
    pub detail: String,
    pub input_bytes: u64,
    pub output_bytes: u64,
    pub tokens_before_est: u64,
    pub tokens_after_est: u64,
    pub tokens_before_real: Option<u64>,
    pub tokens_after_real: Option<u64>,
    pub duration_us: u64,
    pub warnings: Vec<String>,
    /// Handle of the original in the `OriginalStore`, when it was kept.
    pub original_ref: Option<String>,
    pub session_id: String,
    pub agent_id: String,
}

impl StageMetrics {
    pub fn new(stage: Stage, content_kind: impl Into<String>, transformer: impl Into<String>) -> Self {
        StageMetrics {
            stage,
            content_kind: content_kind.into(),
            transformer: transformer.into(),
            status: StageStatus::Skipped,
            detail: String::new(),
            input_bytes: 0,
            output_bytes: 0,
            tokens_before_est: 0,
            tokens_after_est: 0,
            tokens_before_real: None,
            tokens_after_real: None,
            duration_us: 0,
            warnings: Vec::new(),
            original_ref: None,
            session_id: String::new(),
            agent_id: String::new(),
        }
    }

    /// Estimated tokens saved (negative when the stage grew the content).
    pub fn saved_est(&self) -> i64 {
        self.tokens_before_est as i64 - self.tokens_after_est as i64
    }
}

/// One routing decision of the context gate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct GateInfo {
    pub session: String,
    pub agent_id: String,
    pub policy: String,
    /// `direct` or `pxpipe`.
    pub route: String,
    pub reason: String,
    pub zone: Option<String>,
    pub est: u64,
    /// Estimate scaled by the session's calibration.
    pub measure: u64,
    pub calibrated: bool,
    /// Tokens the pxpipe dry run would save.
    pub saving: Option<i64>,
    pub delta: Option<f64>,
    pub text_cost: Option<f64>,
    pub pxpipe_cost: Option<f64>,
    pub predicted_after: Option<u64>,
    pub dryrun_ms: Option<u64>,
    /// Share of precision-critical tokens (hashes, ids, secrets) in the content pxpipe would image.
    pub verbatim_risk: Option<f64>,
    /// False in observe mode: the decision was logged, the direct route was used.
    pub applied: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    Pending,
    InProgress,
    Completed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TodoItem {
    pub content: String,
    pub status: TodoStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", content = "detail", rename_all = "snake_case")]
pub enum AgentStatus {
    Idle,
    Thinking,
    Streaming,
    Tool(String),
    WaitingApproval,
    Delegating,
    Compacting,
    Done,
    Failed(String),
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum AppEvent {
    SessionStarted {
        session_id: String,
        cwd: String,
        orchestrator: String,
        permission_mode: PermissionMode,
    },
    AgentSpawned {
        agent_id: String,
        parent: Option<String>,
        domain: String,
        provider: String,
        model: String,
        task: String,
    },
    AgentStatus {
        agent_id: String,
        status: AgentStatus,
    },
    AgentFinished {
        agent_id: String,
        ok: bool,
        summary: String,
        usage: Usage,
        cost_usd: f64,
        turns: u32,
    },
    /// The user's message, as accepted for a turn.
    UserMessage {
        text: String,
    },
    TextDelta {
        agent_id: String,
        text: String,
    },
    ThinkingDelta {
        agent_id: String,
        text: String,
    },
    /// Complete assistant text of one model response.
    AssistantText {
        agent_id: String,
        text: String,
    },
    ToolStarted {
        agent_id: String,
        call_id: String,
        tool: String,
        summary: String,
    },
    ToolFinished {
        agent_id: String,
        call_id: String,
        tool: String,
        ok: bool,
        preview: String,
        duration_ms: u64,
        exit_code: Option<i32>,
    },
    ApprovalRequested(ApprovalRequest),
    ApprovalResolved {
        id: String,
        allowed: bool,
    },
    PlanUpdated {
        agent_id: String,
        items: Vec<TodoItem>,
    },
    /// Real usage reported by a provider for one call.
    Usage {
        agent_id: String,
        provider: String,
        model: String,
        usage: Usage,
        cost_usd: f64,
    },
    ModelFallback {
        agent_id: String,
        from: String,
        to: String,
        reason: String,
    },
    Optimizer(StageMetrics),
    Gate(GateInfo),
    /// Real tokens of the original request (provider count) and of the optimized one
    /// (provider usage), credited to the stage that changed the request.
    MeasuredSaving {
        stage: Stage,
        tokens_before: u64,
        tokens_after: u64,
    },
    /// Real prompt size of an answer the gate routed, to compare with its prediction.
    GateFeedback {
        session: String,
        route: String,
        real_context: u64,
        predicted_after: Option<u64>,
    },
    PolicyApplied {
        agent_id: String,
        policy: String,
        version: String,
        level: String,
    },
    GraphIndexed {
        files: u64,
        symbols: u64,
        edges: u64,
        communities: u64,
        duration_ms: u64,
    },
    ProxyStatus {
        name: String,
        url: String,
        up: bool,
        latency_ms: Option<u64>,
        stats: Option<Value>,
    },
    ContextStatus {
        agent_id: String,
        est_tokens: u64,
        window: u64,
    },
    PermissionModeChanged {
        mode: PermissionMode,
    },
    ModelChanged {
        target: String,
        model: String,
    },
    CommandOutput {
        command: String,
        output: String,
    },
    Info {
        message: String,
    },
    Warning {
        message: String,
    },
    Error {
        message: String,
    },
    /// The top-level turn ended; the UI accepts input again.
    TurnFinished {
        ok: bool,
    },
    /// The conversation was reset (`/clear`) or replaced (`/resume`).
    ConversationReset {
        session_id: String,
    },
    Shutdown,
}

/// Commands from the UI to the session controller.
#[derive(Debug, Clone, PartialEq)]
pub enum UiCommand {
    /// A prompt, a `/command`, a `!shell` line or a `#memory` note.
    Submit(String),
    /// Stop the running turn (Esc).
    Interrupt,
    Approval {
        id: String,
        decision: ApprovalDecision,
    },
    SetPermissionMode(PermissionMode),
    Quit,
}

/// Where events go. Cloning shares the channel and the metrics. `emit` never blocks.
#[derive(Clone)]
pub struct EventSink {
    tx: Option<mpsc::UnboundedSender<AppEvent>>,
    metrics: Arc<Metrics>,
}

impl EventSink {
    pub fn new(tx: mpsc::UnboundedSender<AppEvent>) -> Self {
        EventSink { tx: Some(tx), metrics: Arc::new(Metrics::default()) }
    }

    /// A sink with no consumer; metrics are still recorded.
    pub fn noop() -> Self {
        EventSink { tx: None, metrics: Arc::new(Metrics::default()) }
    }

    pub fn emit(&self, event: AppEvent) {
        self.metrics.record(&event);
        if let Some(tx) = &self.tx {
            // A closed receiver means the UI is gone; the engine keeps working.
            let _ = tx.send(event);
        }
    }

    pub fn metrics(&self) -> &Arc<Metrics> {
        &self.metrics
    }

    pub fn info(&self, message: impl Into<String>) {
        self.emit(AppEvent::Info { message: message.into() });
    }

    pub fn warn(&self, message: impl Into<String>) {
        self.emit(AppEvent::Warning { message: message.into() });
    }

    pub fn error(&self, message: impl Into<String>) {
        self.emit(AppEvent::Error { message: message.into() });
    }
}

impl std::fmt::Debug for EventSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventSink").field("connected", &self.tx.is_some()).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_serialize_with_tag() {
        let e = AppEvent::Info { message: "x".into() };
        let v = serde_json::to_value(&e).unwrap();
        assert_eq!(v["event"], "info");
    }

    #[tokio::test]
    async fn sink_forwards_and_records() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let sink = EventSink::new(tx);
        let mut m = StageMetrics::new(Stage::Rtk, "tool_output", "rtk:test");
        m.status = StageStatus::Applied;
        m.tokens_before_est = 100;
        m.tokens_after_est = 40;
        sink.emit(AppEvent::Optimizer(m));
        assert!(matches!(rx.recv().await, Some(AppEvent::Optimizer(_))));
        let snap = sink.metrics().snapshot();
        assert_eq!(snap.stages[&Stage::Rtk].saved_est(), 60);
    }
}
