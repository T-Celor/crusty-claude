//! Provider-neutral conversation model.
//!
//! The JSON shape follows the Anthropic Messages API, because it is the richest of the
//! supported wire formats: the Anthropic provider and the HTTP proxy serialize these types
//! as they are, other providers map them. Unknown block types and unknown fields are kept
//! verbatim (`ContentBlock::Unknown`, `extra` maps) so a proxied request loses nothing.

use serde::de::Error as _;
use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Map, Value};

/// Fields a block or request carries that this crate does not model (`cache_control`,
/// `citations`, ...). They are written back unchanged.
pub type Extra = Map<String, Value>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
    /// Mid-conversation operator message (supported by recent Claude models).
    System,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ImageSource {
    Base64 { media_type: String, data: String },
    Url { url: String },
}

#[derive(Debug, Clone, PartialEq)]
pub enum ContentBlock {
    Text {
        text: String,
        extra: Extra,
    },
    Image {
        source: ImageSource,
        extra: Extra,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
        extra: Extra,
    },
    ToolResult {
        tool_use_id: String,
        content: Vec<ContentBlock>,
        is_error: bool,
        extra: Extra,
    },
    Thinking {
        thinking: String,
        signature: Option<String>,
        extra: Extra,
    },
    RedactedThinking {
        data: String,
        extra: Extra,
    },
    /// Any block type this crate does not know (documents, server tool results, ...).
    Unknown(Value),
}

impl ContentBlock {
    pub fn text(text: impl Into<String>) -> Self {
        ContentBlock::Text { text: text.into(), extra: Extra::new() }
    }

    pub fn image_png_base64(data: impl Into<String>) -> Self {
        ContentBlock::Image { source: ImageSource::Base64 { media_type: "image/png".into(), data: data.into() }, extra: Extra::new() }
    }

    pub fn tool_use(id: impl Into<String>, name: impl Into<String>, input: Value) -> Self {
        ContentBlock::ToolUse { id: id.into(), name: name.into(), input, extra: Extra::new() }
    }

    pub fn tool_result(tool_use_id: impl Into<String>, content: Vec<ContentBlock>, is_error: bool) -> Self {
        ContentBlock::ToolResult { tool_use_id: tool_use_id.into(), content, is_error, extra: Extra::new() }
    }

    /// Wire name of the block type.
    pub fn type_name(&self) -> &str {
        match self {
            ContentBlock::Text { .. } => "text",
            ContentBlock::Image { .. } => "image",
            ContentBlock::ToolUse { .. } => "tool_use",
            ContentBlock::ToolResult { .. } => "tool_result",
            ContentBlock::Thinking { .. } => "thinking",
            ContentBlock::RedactedThinking { .. } => "redacted_thinking",
            ContentBlock::Unknown(v) => v.get("type").and_then(Value::as_str).unwrap_or("unknown"),
        }
    }

    /// Text of a text block, `None` for other blocks.
    pub fn as_text(&self) -> Option<&str> {
        match self {
            ContentBlock::Text { text, .. } => Some(text),
            _ => None,
        }
    }

    pub fn extra_mut(&mut self) -> Option<&mut Extra> {
        match self {
            ContentBlock::Text { extra, .. }
            | ContentBlock::Image { extra, .. }
            | ContentBlock::ToolUse { extra, .. }
            | ContentBlock::ToolResult { extra, .. }
            | ContentBlock::Thinking { extra, .. }
            | ContentBlock::RedactedThinking { extra, .. } => Some(extra),
            ContentBlock::Unknown(Value::Object(map)) => Some(map),
            ContentBlock::Unknown(_) => None,
        }
    }
}

/// Concatenated text of the text blocks, one per line.
pub fn blocks_text(blocks: &[ContentBlock]) -> String {
    let mut out = String::new();
    for b in blocks {
        if let Some(t) = b.as_text() {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(t);
        }
    }
    out
}

fn write_extra<M: SerializeMap>(m: &mut M, extra: &Extra) -> Result<(), M::Error> {
    for (k, v) in extra {
        if k != "type" {
            m.serialize_entry(k, v)?;
        }
    }
    Ok(())
}

impl Serialize for ContentBlock {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        if let ContentBlock::Unknown(v) = self {
            return v.serialize(s);
        }
        let mut m = s.serialize_map(None)?;
        m.serialize_entry("type", self.type_name())?;
        match self {
            ContentBlock::Text { text, extra } => {
                m.serialize_entry("text", text)?;
                write_extra(&mut m, extra)?;
            }
            ContentBlock::Image { source, extra } => {
                m.serialize_entry("source", source)?;
                write_extra(&mut m, extra)?;
            }
            ContentBlock::ToolUse { id, name, input, extra } => {
                m.serialize_entry("id", id)?;
                m.serialize_entry("name", name)?;
                m.serialize_entry("input", input)?;
                write_extra(&mut m, extra)?;
            }
            ContentBlock::ToolResult { tool_use_id, content, is_error, extra } => {
                m.serialize_entry("tool_use_id", tool_use_id)?;
                m.serialize_entry("content", content)?;
                if *is_error {
                    m.serialize_entry("is_error", &true)?;
                }
                write_extra(&mut m, extra)?;
            }
            ContentBlock::Thinking { thinking, signature, extra } => {
                m.serialize_entry("thinking", thinking)?;
                if let Some(sig) = signature {
                    m.serialize_entry("signature", sig)?;
                }
                write_extra(&mut m, extra)?;
            }
            ContentBlock::RedactedThinking { data, extra } => {
                m.serialize_entry("data", data)?;
                write_extra(&mut m, extra)?;
            }
            ContentBlock::Unknown(_) => unreachable!("handled above"),
        }
        m.end()
    }
}

#[derive(Deserialize)]
struct TextRepr {
    text: String,
    #[serde(flatten)]
    extra: Extra,
}

#[derive(Deserialize)]
struct ImageRepr {
    source: ImageSource,
    #[serde(flatten)]
    extra: Extra,
}

#[derive(Deserialize)]
struct ToolUseRepr {
    id: String,
    name: String,
    #[serde(default)]
    input: Value,
    #[serde(flatten)]
    extra: Extra,
}

#[derive(Deserialize)]
struct ToolResultRepr {
    tool_use_id: String,
    #[serde(default, deserialize_with = "de_blocks")]
    content: Vec<ContentBlock>,
    #[serde(default)]
    is_error: bool,
    #[serde(flatten)]
    extra: Extra,
}

#[derive(Deserialize)]
struct ThinkingRepr {
    #[serde(default)]
    thinking: String,
    #[serde(default)]
    signature: Option<String>,
    #[serde(flatten)]
    extra: Extra,
}

#[derive(Deserialize)]
struct RedactedRepr {
    data: String,
    #[serde(flatten)]
    extra: Extra,
}

impl ContentBlock {
    /// Parses one block. A known type with an unexpected shape is kept as `Unknown`, so
    /// nothing is lost when a request only passes through.
    pub fn from_value(value: Value) -> Self {
        let Value::Object(mut obj) = value else {
            return ContentBlock::Unknown(value);
        };
        let ty = match obj.get("type").and_then(Value::as_str) {
            Some(t) => t.to_owned(),
            None => return ContentBlock::Unknown(Value::Object(obj)),
        };
        let original = obj.clone();
        obj.remove("type");
        let body = Value::Object(obj);
        let parsed = match ty.as_str() {
            "text" => serde_json::from_value::<TextRepr>(body).map(|r| ContentBlock::Text { text: r.text, extra: r.extra }),
            "image" => serde_json::from_value::<ImageRepr>(body).map(|r| ContentBlock::Image { source: r.source, extra: r.extra }),
            "tool_use" => serde_json::from_value::<ToolUseRepr>(body).map(|r| ContentBlock::ToolUse {
                id: r.id,
                name: r.name,
                input: r.input,
                extra: r.extra,
            }),
            "tool_result" => serde_json::from_value::<ToolResultRepr>(body).map(|r| ContentBlock::ToolResult {
                tool_use_id: r.tool_use_id,
                content: r.content,
                is_error: r.is_error,
                extra: r.extra,
            }),
            "thinking" => serde_json::from_value::<ThinkingRepr>(body).map(|r| ContentBlock::Thinking {
                thinking: r.thinking,
                signature: r.signature,
                extra: r.extra,
            }),
            "redacted_thinking" => {
                serde_json::from_value::<RedactedRepr>(body).map(|r| ContentBlock::RedactedThinking { data: r.data, extra: r.extra })
            }
            _ => return ContentBlock::Unknown(Value::Object(original)),
        };
        parsed.unwrap_or(ContentBlock::Unknown(Value::Object(original)))
    }
}

impl<'de> Deserialize<'de> for ContentBlock {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(ContentBlock::from_value(Value::deserialize(d)?))
    }
}

/// Accepts a plain string (one text block), an array of blocks, or null.
pub fn de_blocks<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<ContentBlock>, D::Error> {
    match Value::deserialize(d)? {
        Value::Null => Ok(Vec::new()),
        Value::String(s) => Ok(vec![ContentBlock::text(s)]),
        Value::Array(items) => Ok(items.into_iter().map(ContentBlock::from_value).collect()),
        other => Err(D::Error::custom(format!("expected string or array of content blocks, got {other}"))),
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    #[serde(deserialize_with = "de_blocks")]
    pub content: Vec<ContentBlock>,
}

impl Message {
    pub fn user_text(text: impl Into<String>) -> Self {
        Message { role: Role::User, content: vec![ContentBlock::text(text)] }
    }

    pub fn assistant_text(text: impl Into<String>) -> Self {
        Message { role: Role::Assistant, content: vec![ContentBlock::text(text)] }
    }

    pub fn text(&self) -> String {
        blocks_text(&self.content)
    }

    pub fn tool_uses(&self) -> impl Iterator<Item = (&str, &str, &Value)> {
        self.content.iter().filter_map(|b| match b {
            ContentBlock::ToolUse { id, name, input, .. } => Some((id.as_str(), name.as_str(), input)),
            _ => None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub input_schema: Value,
    /// `type` of server tools, `cache_control`, `eager_input_streaming`, ...
    #[serde(flatten)]
    pub extra: Extra,
}

impl ToolSpec {
    pub fn new(name: impl Into<String>, description: impl Into<String>, input_schema: Value) -> Self {
        ToolSpec { name: name.into(), description: description.into(), input_schema, extra: Extra::new() }
    }
}

/// One model call. Serializes to an Anthropic Messages request body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatRequest {
    pub model: String,
    pub max_tokens: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty", deserialize_with = "de_blocks")]
    pub system: Vec<ContentBlock>,
    pub messages: Vec<Message>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
    /// `thinking`, `output_config`, `metadata`, `tool_choice`, `stop_sequences`, ...
    #[serde(flatten)]
    pub extra: Extra,
}

/// Beta that lets a request carry `thinking.block_binding`.
pub const THINKING_BINDING_BETA: &str = "thinking-binding-controls-2026-08-01";

impl ChatRequest {
    /// Asks the API to drop, instead of rejecting, thinking blocks whose earlier
    /// conversation was edited (compression, compaction, a rebuilt system prompt). Only
    /// applies when the request has a `thinking` object. Transports must then send
    /// `THINKING_BINDING_BETA`. Returns whether the field is set.
    pub fn tolerate_history_edits(&mut self) -> bool {
        match self.extra.get_mut("thinking") {
            Some(Value::Object(t)) => {
                t.insert("block_binding".into(), serde_json::json!({"prefix_mismatch_behavior": "drop_block"}));
                true
            }
            _ => false,
        }
    }

    /// Whether `thinking.block_binding` is set (the transport needs the beta header).
    pub fn has_block_binding(&self) -> bool {
        self.extra.get("thinking").and_then(|t| t.get("block_binding")).is_some()
    }

    pub fn new(model: impl Into<String>, max_tokens: u32) -> Self {
        ChatRequest {
            model: model.into(),
            max_tokens,
            system: Vec::new(),
            messages: Vec::new(),
            tools: Vec::new(),
            temperature: None,
            stream: None,
            extra: Extra::new(),
        }
    }

    pub fn system_text(&self) -> String {
        blocks_text(&self.system)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub cache_read_input_tokens: u64,
    #[serde(default)]
    pub cache_creation_input_tokens: u64,
}

impl Usage {
    /// Whole prompt size: uncached input plus cache reads and writes.
    pub fn total_input(&self) -> u64 {
        self.input_tokens + self.cache_read_input_tokens + self.cache_creation_input_tokens
    }

    pub fn total(&self) -> u64 {
        self.total_input() + self.output_tokens
    }

    pub fn add(&mut self, other: &Usage) {
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
        self.cache_read_input_tokens += other.cache_read_input_tokens;
        self.cache_creation_input_tokens += other.cache_creation_input_tokens;
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
    StopSequence,
    PauseTurn,
    Refusal,
    Other(String),
}

impl StopReason {
    pub fn parse(s: &str) -> Self {
        match s {
            "end_turn" | "stop" => StopReason::EndTurn,
            "tool_use" | "tool_calls" | "function_call" => StopReason::ToolUse,
            "max_tokens" | "length" => StopReason::MaxTokens,
            "stop_sequence" => StopReason::StopSequence,
            "pause_turn" => StopReason::PauseTurn,
            "refusal" | "content_filter" => StopReason::Refusal,
            other => StopReason::Other(other.to_owned()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatResponse {
    #[serde(default)]
    pub id: Option<String>,
    pub model: String,
    pub content: Vec<ContentBlock>,
    pub stop_reason: StopReason,
    #[serde(default)]
    pub usage: Usage,
    /// Present when `stop_reason` is `refusal`.
    #[serde(default)]
    pub stop_details: Option<Value>,
}

impl ChatResponse {
    pub fn text(&self) -> String {
        blocks_text(&self.content)
    }
}

/// Incremental output while a response streams, for live display only. The final
/// `ChatResponse` is authoritative.
#[derive(Debug, Clone, PartialEq)]
pub enum StreamDelta {
    Text(String),
    Thinking(String),
    ToolUseStart { id: String, name: String },
    ToolInput { id: String, partial_json: String },
    Usage(Usage),
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn known_blocks_round_trip_with_extra_fields() {
        let v = json!({"type": "text", "text": "hi", "cache_control": {"type": "ephemeral"}});
        let b: ContentBlock = serde_json::from_value(v.clone()).unwrap();
        assert!(matches!(&b, ContentBlock::Text { text, extra } if text == "hi" && extra.contains_key("cache_control")));
        assert_eq!(serde_json::to_value(&b).unwrap(), v);
    }

    #[test]
    fn unknown_blocks_pass_through() {
        let v = json!({"type": "document", "source": {"type": "text", "data": "x"}});
        let b: ContentBlock = serde_json::from_value(v.clone()).unwrap();
        assert!(matches!(b, ContentBlock::Unknown(_)));
        assert_eq!(serde_json::to_value(&b).unwrap(), v);
    }

    #[test]
    fn malformed_known_block_is_kept_verbatim() {
        let v = json!({"type": "tool_use", "name": "x"});
        let b: ContentBlock = serde_json::from_value(v.clone()).unwrap();
        assert_eq!(serde_json::to_value(&b).unwrap(), v);
    }

    #[test]
    fn tool_result_accepts_string_content() {
        let v = json!({"type": "tool_result", "tool_use_id": "t1", "content": "ok"});
        let b: ContentBlock = serde_json::from_value(v).unwrap();
        match b {
            ContentBlock::ToolResult { content, is_error, .. } => {
                assert_eq!(blocks_text(&content), "ok");
                assert!(!is_error);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn request_keeps_unknown_top_level_fields() {
        let v = json!({
            "model": "m", "max_tokens": 10, "system": "be brief",
            "messages": [{"role": "user", "content": "hello"}],
            "thinking": {"type": "adaptive"}, "metadata": {"user_id": "u"}
        });
        let r: ChatRequest = serde_json::from_value(v).unwrap();
        assert_eq!(r.system_text(), "be brief");
        assert!(r.extra.contains_key("thinking"));
        let back = serde_json::to_value(&r).unwrap();
        assert_eq!(back["metadata"]["user_id"], "u");
        assert_eq!(back["messages"][0]["content"][0]["text"], "hello");
    }

    #[test]
    fn history_edit_tolerance_needs_thinking() {
        let mut r = ChatRequest::new("m", 10);
        assert!(!r.tolerate_history_edits());
        r.extra.insert("thinking".into(), json!({"type": "adaptive"}));
        assert!(r.tolerate_history_edits());
        assert!(r.has_block_binding());
        assert_eq!(serde_json::to_value(&r).unwrap()["thinking"]["block_binding"]["prefix_mismatch_behavior"], "drop_block");
    }

    #[test]
    fn stop_reason_maps_openai_names() {
        assert_eq!(StopReason::parse("tool_calls"), StopReason::ToolUse);
        assert_eq!(StopReason::parse("length"), StopReason::MaxTokens);
    }
}
