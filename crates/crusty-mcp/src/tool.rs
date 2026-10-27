//! The `crusty_core::Tool` adapter for one tool of one MCP server.

use std::time::Duration;

use async_trait::async_trait;
use crusty_core::ToolSpec;
use crusty_core::message::blocks_text;
use crusty_core::permission::{RiskAssessment, RiskLevel};
use crusty_core::tool::{Tool, ToolCategory, ToolContext, ToolError, ToolOutput, preview_of};
use rmcp::model::CallToolRequestParams;
use rmcp::service::{Peer, RoleClient};
use serde_json::Value;

use crate::convert;

/// One remote tool. Each server is its own trust boundary, so calls are asked unless a rule
/// such as `mcp__<server>` allows them.
pub struct McpTool {
    server: String,
    tool_name: String,
    spec: ToolSpec,
    peer: Peer<RoleClient>,
    timeout: Duration,
}

impl McpTool {
    /// `tool_name` is the server's own name for the tool; `spec.name` is the name the model sees.
    pub fn new(server: &str, tool_name: String, spec: ToolSpec, peer: Peer<RoleClient>, timeout: Duration) -> Self {
        McpTool { server: server.to_owned(), tool_name, spec, peer, timeout }
    }
}

#[async_trait]
impl Tool for McpTool {
    fn name(&self) -> &str {
        &self.spec.name
    }

    fn spec(&self) -> ToolSpec {
        self.spec.clone()
    }

    fn category(&self) -> ToolCategory {
        ToolCategory::Other
    }

    fn is_read_only(&self) -> bool {
        false
    }

    fn assess(&self, _input: &Value, _ctx: &ToolContext) -> RiskAssessment {
        RiskAssessment::new(RiskLevel::Moderate, format!("MCP tool from server {} (separate trust boundary)", self.server))
    }

    async fn call(&self, input: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let Value::Object(arguments) = input else {
            return Err(ToolError::InvalidInput(format!("MCP tool `{}` expects a JSON object", self.spec.name)));
        };
        let mut params = CallToolRequestParams::new(self.tool_name.clone());
        params.arguments = Some(arguments);
        let request = self.peer.call_tool(params);
        let outcome = tokio::select! {
            () = ctx.cancel.cancelled() => return Err(ToolError::Cancelled),
            outcome = tokio::time::timeout(self.timeout, request) => outcome,
        };
        let result = match outcome {
            Err(_) => return Err(ToolError::Timeout(self.timeout.as_secs())),
            Ok(Err(e)) => return Err(ToolError::Failed(format!("MCP server `{}`: {e}", self.server))),
            Ok(Ok(result)) => result,
        };
        let (content, is_error) = convert::call_result(&result);
        let preview = preview_of(&blocks_text(&content));
        Ok(ToolOutput { content, is_error, preview, exit_code: None })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::model::{CallToolResponse, CallToolResult, ListToolsResult, PaginatedRequestParams};
    use rmcp::service::{RequestContext, RoleServer};
    use rmcp::{ErrorData, ServerHandler, ServiceExt};
    use serde_json::json;

    /// Echoes `arguments.text` back as `echo: <text>`.
    #[derive(Clone)]
    struct EchoServer;

    impl ServerHandler for EchoServer {
        async fn list_tools(
            &self,
            _request: Option<PaginatedRequestParams>,
            _context: RequestContext<RoleServer>,
        ) -> Result<ListToolsResult, ErrorData> {
            serde_json::from_value(json!({"tools": [
                {"name": "echo", "description": "Echo", "inputSchema": {"type": "object"}}
            ]}))
            .map_err(|e| ErrorData::internal_error(e.to_string(), None))
        }

        async fn call_tool(
            &self,
            request: CallToolRequestParams,
            _context: RequestContext<RoleServer>,
        ) -> Result<CallToolResponse, ErrorData> {
            let text = request.arguments.as_ref().and_then(|args| args.get("text")).and_then(Value::as_str).unwrap_or_default().to_owned();
            serde_json::from_value::<CallToolResult>(json!({"content": [{"type": "text", "text": format!("echo: {text}")}]}))
                .map(CallToolResponse::from)
                .map_err(|e| ErrorData::internal_error(e.to_string(), None))
        }
    }

    /// A live client session talking to `EchoServer` over an in-memory pipe.
    async fn echo_session() -> rmcp::service::RunningService<RoleClient, ()> {
        let (server_io, client_io) = tokio::io::duplex(1 << 16);
        tokio::spawn(async move {
            if let Ok(running) = EchoServer.serve(server_io).await {
                let _ = running.waiting().await;
            }
        });
        ().serve(client_io).await.expect("client initialises over the in-memory pipe")
    }

    fn echo_tool(peer: Peer<RoleClient>) -> McpTool {
        let spec = ToolSpec::new("mcp__test__echo", "[MCP test] Echo", json!({"type": "object"}));
        McpTool::new("test", "echo".to_owned(), spec, peer, Duration::from_secs(10))
    }

    #[tokio::test]
    async fn non_object_input_is_invalid() {
        let session = echo_session().await;
        let tool = echo_tool(session.peer().clone());
        let ctx = ToolContext::for_tests(std::env::temp_dir());
        let err = tool.call(json!("not an object"), &ctx).await.unwrap_err();
        assert!(matches!(err, ToolError::InvalidInput(_)));
    }

    #[tokio::test]
    async fn object_input_reaches_the_server() {
        let session = echo_session().await;
        let tool = echo_tool(session.peer().clone());
        let ctx = ToolContext::for_tests(std::env::temp_dir());
        let out = tool.call(json!({"text": "hi"}), &ctx).await.expect("call succeeds");
        assert!(!out.is_error);
        assert_eq!(out.text_content(), "echo: hi");
        assert_eq!(out.preview, "echo: hi");
    }

    #[tokio::test]
    async fn identity_and_trust_boundary() {
        let session = echo_session().await;
        let tool = echo_tool(session.peer().clone());
        assert_eq!(tool.name(), "mcp__test__echo");
        assert_eq!(tool.category(), ToolCategory::Other);
        assert!(!tool.is_read_only());
        let assessment = tool.assess(&json!({}), &ToolContext::for_tests(std::env::temp_dir()));
        assert_eq!(assessment.level, RiskLevel::Moderate);
        assert!(assessment.reason.contains("separate trust boundary"));
    }
}
