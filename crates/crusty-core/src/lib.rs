//! Shared contracts of Crusty-Claude.
//!
//! Every other crate depends on this one and on nothing else of the workspace except
//! where noted, so the provider, tool, optimizer, UI and engine crates can evolve and be
//! tested independently. The binary wires concrete implementations together.

pub mod advisor;
pub mod checkpoint;
pub mod config;
pub mod event;
pub mod filter;
pub mod hooks;
pub mod message;
pub mod metrics;
pub mod optimize;
pub mod paths;
pub mod permission;
pub mod provider;
pub mod store;
pub mod tokens;
pub mod tool;
pub mod trust;
pub mod vault;

pub use advisor::{AdviceSource, Advisor, Judgement, NoAdvisor, Scores, Verdict};
pub use checkpoint::{CheckpointSummary, Checkpointer, NoCheckpoints, RewindReport, RewindScope};
pub use config::Config;
pub use event::{AdviceInfo, AdvicePoint, AppEvent, EventSink, SandboxInfo, Stage, StageMetrics, StageStatus, UiCommand};
pub use filter::{ContentFilter, Filtered, HtmlDoc, choose_filtered};
pub use message::{ChatRequest, ChatResponse, ContentBlock, Message, Role, StopReason, StreamDelta, ToolSpec, Usage};
pub use optimize::{OptimizeContext, OptimizeOutcome, RequestOptimizer, Route};
pub use permission::{
    ApprovalDecision, ApprovalRequest, Approver, PermissionDecision, PermissionMode, PermissionPolicy, RiskAssessment, RiskLevel,
};
pub use provider::{CallOptions, ModelRef, Provider, ProviderError, ProviderKind};
pub use store::OriginalStore;
pub use tool::{PlannedChange, Tool, ToolCategory, ToolContext, ToolError, ToolOutput};
pub use vault::{RedactionKind, SecretVault};

/// Re-exported so implementors use the same version.
pub use async_trait::async_trait;
pub use tokio_util::sync::CancellationToken;
