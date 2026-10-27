//! The `Retrieve` tool: fetches the original of content an optimizer shortened.

use async_trait::async_trait;
use crusty_core::message::ToolSpec;
use crusty_core::permission::{RiskAssessment, RiskLevel};
use crusty_core::tool::{Tool, ToolCategory, ToolContext, ToolError, ToolOutput, input_str};
use serde_json::{Value, json};

use crate::opt_u64;
use crate::read::{DEFAULT_LINE_LIMIT, render_lines};

/// Returns numbered lines of an original kept by the session's `OriginalStore`.
pub struct RetrieveTool;

#[async_trait]
impl Tool for RetrieveTool {
    fn name(&self) -> &str {
        "Retrieve"
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "Retrieve",
            "Returns the original, unshortened text behind a handle such as orig_1a2b3c4d5e6f. Use offset and limit to page through it.",
            json!({
                "type": "object",
                "properties": {
                    "handle": {"type": "string", "description": "Handle printed by a shortened tool result."},
                    "offset": {"type": "integer", "minimum": 1, "description": "1-based line to start from."},
                    "limit": {"type": "integer", "minimum": 1, "description": "Number of lines to return (default 2000)."}
                },
                "required": ["handle"]
            }),
        )
    }

    fn category(&self) -> ToolCategory {
        ToolCategory::Read
    }

    fn assess(&self, _input: &Value, _ctx: &ToolContext) -> RiskAssessment {
        RiskAssessment::new(RiskLevel::Low, "reads a stored original")
    }

    async fn call(&self, input: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let handle = input_str(&input, "handle")?;
        let original = ctx.store.get(handle).ok_or_else(|| {
            ToolError::Failed(format!("unknown handle `{handle}`: originals are kept for this session only, within a size budget"))
        })?;
        let offset = opt_u64(&input, "offset")?.unwrap_or(1);
        let limit = opt_u64(&input, "limit")?.unwrap_or(DEFAULT_LINE_LIMIT);
        Ok(ToolOutput::text(render_lines(original.as_str(), offset, limit)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::context;
    use tempfile::tempdir;

    #[tokio::test]
    async fn returns_numbered_original() {
        let dir = tempdir().unwrap();
        let ctx = context(&dir);
        let handle = ctx.store.put("alpha\nbeta");
        let out = RetrieveTool.call(json!({"handle": handle}), &ctx).await.unwrap();
        assert_eq!(out.text_content(), "     1\talpha\n     2\tbeta");
    }

    #[tokio::test]
    async fn unknown_handle_is_an_error() {
        let dir = tempdir().unwrap();
        let ctx = context(&dir);
        let result = RetrieveTool.call(json!({"handle": "orig_000000000000"}), &ctx).await;
        assert!(matches!(result, Err(ToolError::Failed(m)) if m.contains("session only")));
    }
}
