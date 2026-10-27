//! The seam between the ACP server and the engine. The CLI implements [`AcpBackend`];
//! this crate knows nothing about providers, tools or sessions beyond these traits.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use crusty_core::AppEvent;
use crusty_core::permission::{ApprovalDecision, ApprovalRequest};
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

/// Why a prompt turn ended. [`StopReason::as_str`] is the ACP `stopReason` value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    /// The model finished its answer.
    EndTurn,
    /// The client cancelled the turn.
    Cancelled,
    /// The model hit its output limit.
    MaxTokens,
    /// The model refused to answer.
    Refusal,
}

impl StopReason {
    /// The wire name used in the `session/prompt` response.
    pub fn as_str(self) -> &'static str {
        match self {
            StopReason::EndTurn => "end_turn",
            StopReason::Cancelled => "cancelled",
            StopReason::MaxTokens => "max_tokens",
            StopReason::Refusal => "refusal",
        }
    }
}

/// A session that a backend created or loaded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionHandle {
    /// Backend-assigned id, sent to the client as `sessionId`.
    pub id: String,
}

/// An MCP server the client asked a session to use.
#[derive(Debug, Clone, PartialEq)]
pub struct McpServerSpec {
    /// The `name` field of the ACP server object.
    pub name: String,
    /// The whole ACP server object, untouched, for the backend to interpret.
    pub config: serde_json::Value,
}

/// One block of a user prompt, already decoded from ACP content blocks.
#[derive(Debug, Clone, PartialEq)]
pub enum PromptPart {
    /// A `text` block.
    Text(String),
    /// A `resource_link` (no text) or an embedded `resource` (optional inline text).
    Resource {
        /// Usually a `file://` URI.
        uri: String,
        /// Inline contents of an embedded resource, when the client sent them.
        text: Option<String>,
    },
    /// An `image` block: MIME type and base64 data.
    Image {
        /// For example `image/png`.
        mime_type: String,
        /// Base64-encoded bytes.
        data: String,
    },
}

/// Asks the client to approve a tool call. Implemented by the server for each prompt.
#[async_trait]
pub trait ApprovalBridge: Send + Sync {
    /// Sends the request and waits for the answer. A cancelled turn, a refused or
    /// malformed answer and a missing client all come back as [`ApprovalDecision::Deny`].
    async fn request(&self, request: ApprovalRequest) -> ApprovalDecision;
}

/// The engine as seen by the ACP server.
#[async_trait]
pub trait AcpBackend: Send + Sync {
    /// Whether `session/load` is supported. Advertised in `initialize`.
    fn can_load_session(&self) -> bool {
        false
    }

    /// Creates a session rooted at `cwd`.
    async fn new_session(&self, cwd: PathBuf, mcp_servers: Vec<McpServerSpec>) -> Result<SessionHandle, String>;

    /// Restores a session by id. The default refuses.
    async fn load_session(&self, _id: &str, _cwd: PathBuf) -> Result<SessionHandle, String> {
        Err("unsupported".to_string())
    }

    /// Runs one turn. The backend sends engine events on `events` and may stop early when
    /// `cancel` fires. Dropping `events` (by returning) ends the turn's event stream.
    async fn prompt(
        &self,
        session: &str,
        prompt: Vec<PromptPart>,
        events: UnboundedSender<AppEvent>,
        approvals: Arc<dyn ApprovalBridge>,
        cancel: CancellationToken,
    ) -> Result<StopReason, String>;

    /// Switches the session's mode.
    async fn set_mode(&self, session: &str, mode_id: &str) -> Result<(), String>;

    /// Available modes as `(id, name)`; the first one is the default.
    fn modes(&self) -> Vec<(String, String)>;
}
