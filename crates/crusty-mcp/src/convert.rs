//! Pure conversions from rmcp model values to crusty-core values. No I/O, no connections.

use crusty_core::ToolSpec;
use crusty_core::message::{ContentBlock, ImageSource};
use rmcp::model::{CallToolResult, ContentBlock as McpContent, ResourceContents, Tool as McpToolDef};
use serde::Serialize;
use serde_json::{Value, json};

/// Longest tool name the model API accepts.
const MAX_NAME_LEN: usize = 64;
/// Kept characters before the suffix when a name is too long: `prefix_` plus 8 hex digits.
const PREFIX_LEN: usize = MAX_NAME_LEN - 9;

/// Replaces every character outside `[A-Za-z0-9_-]` with `_`.
pub fn sanitize(text: &str) -> String {
    text.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' }).collect()
}

/// 64-bit FNV-1a: stable across runs and platforms, unlike `DefaultHasher`.
fn fnv1a(text: &str) -> u64 {
    text.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3))
}

/// The name the model sees: `mcp__<server>__<tool>`, sanitised, at most 64 characters.
/// Longer names keep a 55-character prefix and 8 hex digits of a hash of the full name.
pub fn tool_name(server: &str, tool: &str) -> String {
    let full = sanitize(&format!("mcp__{server}__{tool}"));
    if full.len() <= MAX_NAME_LEN {
        return full;
    }
    format!("{}_{:08x}", &full[..PREFIX_LEN], fnv1a(&full) as u32)
}

/// Spec for the model: description prefixed with the server name, schema as the server sent it.
pub fn tool_spec(server: &str, tool: &McpToolDef) -> ToolSpec {
    let description = tool.description.as_deref().unwrap_or_default();
    let schema = if tool.input_schema.is_empty() { json!({"type": "object"}) } else { Value::Object((*tool.input_schema).clone()) };
    ToolSpec::new(tool_name(server, &tool.name), format!("[MCP {server}] {description}"), schema)
}

/// Converts a tool result into content blocks and its error flag. Structured content, when
/// present, is appended as pretty JSON.
pub fn call_result(result: &CallToolResult) -> (Vec<ContentBlock>, bool) {
    let mut blocks: Vec<ContentBlock> = result.content.iter().map(block).collect();
    if let Some(structured) = &result.structured_content {
        blocks.push(ContentBlock::text(json_text(structured)));
    }
    (blocks, result.is_error.unwrap_or(false))
}

fn block(content: &McpContent) -> ContentBlock {
    match content {
        McpContent::Text(text) => ContentBlock::text(text.text.clone()),
        McpContent::Image(image) => ContentBlock::Image {
            source: ImageSource::Base64 { media_type: image.mime_type.clone(), data: image.data.clone() },
            extra: Default::default(),
        },
        McpContent::Resource(embedded) => ContentBlock::text(resource_text(&embedded.resource)),
        other => ContentBlock::text(json_text(other)),
    }
}

fn resource_text(resource: &ResourceContents) -> String {
    match resource {
        ResourceContents::TextResourceContents { uri, text, .. } => format!("{uri}\n{text}"),
        // Binary payloads are not sent to the model; the URI and size tell it what exists.
        ResourceContents::BlobResourceContents { uri, mime_type, blob, .. } => format!(
            "{uri} ({}, {} base64 characters, binary content not shown)",
            mime_type.as_deref().unwrap_or("unknown type"),
            blob.len()
        ),
        other => json_text(other),
    }
}

fn json_text<T: Serialize + ?Sized>(value: &T) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|e| format!("<unserialisable content: {e}>"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(value: Value) -> CallToolResult {
        serde_json::from_value(value).expect("valid CallToolResult JSON")
    }

    #[test]
    fn names_are_sanitised_and_capped() {
        assert_eq!(tool_name("git hub", "create.issue"), "mcp__git_hub__create_issue");
        let long = tool_name("server", &"x".repeat(100));
        assert_eq!(long.len(), MAX_NAME_LEN);
        assert_eq!(long, tool_name("server", &"x".repeat(100)));
        assert_ne!(long, tool_name("server", &"x".repeat(99)));
        assert!(long.starts_with("mcp__server__xxx"));
    }

    #[test]
    fn spec_prefixes_description_and_defaults_schema() {
        let def: McpToolDef = serde_json::from_value(json!({"name": "ping", "inputSchema": {}})).expect("tool");
        let spec = tool_spec("api", &def);
        assert_eq!(spec.name, "mcp__api__ping");
        assert_eq!(spec.description, "[MCP api] ");
        assert_eq!(spec.input_schema, json!({"type": "object"}));
    }

    #[test]
    fn text_and_image_convert() {
        let (blocks, is_error) = call_result(&result(json!({"content": [
            {"type": "text", "text": "hello"},
            {"type": "image", "data": "AAAA", "mimeType": "image/png"}
        ]})));
        assert!(!is_error);
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0], ContentBlock::text("hello"));
        assert_eq!(
            blocks[1],
            ContentBlock::Image {
                source: ImageSource::Base64 { media_type: "image/png".into(), data: "AAAA".into() },
                extra: Default::default(),
            }
        );
    }

    #[test]
    fn resource_error_flag_and_structured_content() {
        let (blocks, is_error) = call_result(&result(json!({
            "content": [{"type": "resource", "resource": {"uri": "file:///a.txt", "mimeType": "text/plain", "text": "body"}}],
            "structuredContent": {"ok": true},
            "isError": true
        })));
        assert!(is_error);
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0], ContentBlock::text("file:///a.txt\nbody"));
        assert_eq!(blocks[1], ContentBlock::text("{\n  \"ok\": true\n}"));
    }

    #[test]
    fn blob_resource_is_described_not_inlined() {
        let (blocks, _) = call_result(&result(json!({"content": [
            {"type": "resource", "resource": {"uri": "file:///a.bin", "mimeType": "application/octet-stream", "blob": "QUJD"}}
        ]})));
        let ContentBlock::Text { text, .. } = &blocks[0] else { panic!("expected a text block") };
        assert!(text.starts_with("file:///a.bin"));
        assert!(!text.contains("QUJD"));
    }
}
