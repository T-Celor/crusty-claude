//! OpenAI Chat Completions and compatible servers (OpenRouter, Ollama, Gemini's OpenAI
//! endpoint, vLLM, LM Studio).

use std::collections::BTreeMap;

use async_trait::async_trait;
use crusty_core::config::ProviderConfig;
use crusty_core::message::{
    ChatRequest, ChatResponse, ContentBlock, ImageSource, Message, Role, StopReason, StreamDelta, Usage, blocks_text,
};
use crusty_core::provider::{CallOptions, DeltaSink, Provider, ProviderError, ProviderKind};
use reqwest::RequestBuilder;
use serde_json::{Value, json};

use crate::common::{ensure_success, env_value, for_each_sse, map_reqwest, parse_tool_input, resolve_base, truncated_stream};
use crate::retry::with_retries;

const DEFAULT_RETRIES: u32 = 3;
const OPENAI_HOST: &str = "openai.com";
const IMAGE_OMITTED: &str = "[image omitted]";

/// OpenAI Chat Completions provider.
pub struct OpenAiProvider {
    id: String,
    cfg: ProviderConfig,
    http: reqwest::Client,
    api_key: Option<String>,
}

impl OpenAiProvider {
    pub fn new(id: impl Into<String>, cfg: ProviderConfig, http: reqwest::Client) -> Self {
        OpenAiProvider { id: id.into(), cfg, http, api_key: None }
    }

    /// Uses `key` instead of the environment variable (tests and explicit overrides).
    pub fn with_api_key(mut self, key: Option<String>) -> Self {
        self.api_key = key;
        self
    }

    /// Bearer key when one is configured; keyless servers (Ollama, LM Studio) send none.
    fn decorate(&self, mut rb: RequestBuilder, extra: &[(String, String)]) -> RequestBuilder {
        let key = self.api_key.clone().or_else(|| env_value(self.cfg.api_key_env.as_deref()));
        if let Some(key) = key {
            rb = rb.bearer_auth(key);
        }
        for (name, value) in self.cfg.headers.iter().chain(extra.iter().map(|(k, v)| (k, v))) {
            rb = rb.header(name.as_str(), value.as_str());
        }
        rb
    }

    async fn send_chat(&self, req: &ChatRequest, opts: &CallOptions) -> Result<reqwest::Response, ProviderError> {
        let base = resolve_base(opts, &self.cfg)?;
        let body = to_openai_body(req, &base);
        let rb = self.http.post(format!("{base}/chat/completions")).json(&body);
        let resp = self.decorate(rb, &opts.extra_headers).send().await.map_err(map_reqwest)?;
        ensure_success(resp).await
    }

    fn max_retries(&self) -> u32 {
        self.cfg.max_retries.unwrap_or(DEFAULT_RETRIES)
    }
}

#[async_trait]
impl Provider for OpenAiProvider {
    fn id(&self) -> &str {
        &self.id
    }

    fn kind(&self) -> ProviderKind {
        ProviderKind::Openai
    }

    async fn stream(&self, req: &ChatRequest, opts: &CallOptions, on_delta: DeltaSink<'_>) -> Result<ChatResponse, ProviderError> {
        let resp = with_retries(self.max_retries(), &opts.cancel, |_| self.send_chat(req, opts)).await?;
        let mut acc = Accumulator::default();
        for_each_sse(resp, &opts.cancel, |event| acc.apply(&event.data, on_delta)).await?;
        if !acc.complete() {
            return Err(truncated_stream());
        }
        Ok(acc.finish(&req.model, on_delta))
    }

    async fn list_models(&self) -> Result<Vec<String>, ProviderError> {
        let base = resolve_base(&CallOptions::default(), &self.cfg)?;
        let resp = self.decorate(self.http.get(format!("{base}/models")), &[]).send().await.map_err(map_reqwest)?;
        let resp = ensure_success(resp).await?;
        let v: Value = resp.json().await.map_err(|e| ProviderError::Parse(e.to_string()))?;
        Ok(v["data"].as_array().map(|items| items.iter().filter_map(|m| m["id"].as_str().map(str::to_owned)).collect()).unwrap_or_default())
    }
}

/// Maps a provider-neutral request to a Chat Completions body. Anthropic-only extras
/// (thinking, fallbacks, metadata, tool_choice) are not sent.
pub(crate) fn to_openai_body(req: &ChatRequest, base_url: &str) -> Value {
    let mut messages = Vec::new();
    let system = req.system_text();
    if !system.is_empty() {
        messages.push(json!({ "role": "system", "content": system }));
    }
    for msg in &req.messages {
        match msg.role {
            Role::System => messages.push(json!({ "role": "system", "content": msg.text() })),
            Role::User => user_messages(msg, &mut messages),
            Role::Assistant => assistant_message(msg, &mut messages),
        }
    }

    let mut body = json!({
        "model": req.model,
        "messages": messages,
        "stream": true,
        "stream_options": { "include_usage": true },
    });
    let max_key = if base_url.contains("api.openai.com") { "max_completion_tokens" } else { "max_tokens" };
    body[max_key] = json!(req.max_tokens);
    if let Some(t) = req.temperature {
        body["temperature"] = json!(t);
    }
    if base_url.contains(OPENAI_HOST)
        && let Some(effort) = req.extra.get("output_config").and_then(|c| c["effort"].as_str())
    {
        let mapped = if matches!(effort, "xhigh" | "max") { "high" } else { effort };
        body["reasoning_effort"] = json!(mapped);
    }
    let tools: Vec<Value> = req
        .tools
        .iter()
        .filter(|t| !t.input_schema.is_null())
        .map(|t| json!({ "type": "function", "function": { "name": t.name, "description": t.description, "parameters": t.input_schema } }))
        .collect();
    if !tools.is_empty() {
        body["tools"] = Value::Array(tools);
    }
    body
}

/// Tool results become `tool` messages placed before the user's own parts.
fn user_messages(msg: &Message, out: &mut Vec<Value>) {
    for block in &msg.content {
        if let ContentBlock::ToolResult { tool_use_id, content, .. } = block {
            out.push(json!({ "role": "tool", "tool_call_id": tool_use_id, "content": tool_result_text(content) }));
        }
    }
    let parts: Vec<Value> = msg.content.iter().filter_map(user_part).collect();
    if parts.is_empty() {
        return;
    }
    if parts.iter().all(|p| p["type"] == "text") {
        let text: Vec<&str> = parts.iter().filter_map(|p| p["text"].as_str()).collect();
        out.push(json!({ "role": "user", "content": text.join("\n") }));
    } else {
        out.push(json!({ "role": "user", "content": parts }));
    }
}

fn user_part(block: &ContentBlock) -> Option<Value> {
    match block {
        ContentBlock::Text { text, .. } => Some(json!({ "type": "text", "text": text })),
        ContentBlock::Image { source, .. } => {
            let url = match source {
                ImageSource::Base64 { media_type, data } => format!("data:{media_type};base64,{data}"),
                ImageSource::Url { url } => url.clone(),
            };
            Some(json!({ "type": "image_url", "image_url": { "url": url } }))
        }
        _ => None,
    }
}

fn tool_result_text(content: &[ContentBlock]) -> String {
    let parts: Vec<String> = content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text, .. } => Some(text.clone()),
            ContentBlock::Image { .. } => Some(IMAGE_OMITTED.to_owned()),
            _ => None,
        })
        .collect();
    parts.join("\n")
}

fn assistant_message(msg: &Message, out: &mut Vec<Value>) {
    let text = blocks_text(&msg.content);
    let tool_calls: Vec<Value> = msg
        .tool_uses()
        .map(|(id, name, input)| json!({ "id": id, "type": "function", "function": { "name": name, "arguments": input.to_string() } }))
        .collect();
    if text.is_empty() && tool_calls.is_empty() {
        return;
    }
    let mut entry = json!({ "role": "assistant", "content": if text.is_empty() { Value::Null } else { json!(text) } });
    if !tool_calls.is_empty() {
        entry["tool_calls"] = Value::Array(tool_calls);
    }
    out.push(entry);
}

#[derive(Default)]
struct ToolAcc {
    id: String,
    name: String,
    args: String,
}

/// Rebuilds a response from Chat Completions stream chunks.
#[derive(Default)]
struct Accumulator {
    id: Option<String>,
    model: Option<String>,
    text: String,
    tools: BTreeMap<u64, ToolAcc>,
    finish_reason: Option<String>,
    usage: Usage,
    /// Set by the `[DONE]` sentinel.
    done: bool,
}

impl Accumulator {
    /// True once the server sent `[DONE]` or a choice with a finish reason.
    fn complete(&self) -> bool {
        self.done || self.finish_reason.is_some()
    }

    /// Applies one `data:` payload. `[DONE]` marks the end; empty payloads carry nothing.
    fn apply(&mut self, data: &str, sink: DeltaSink<'_>) -> Result<(), ProviderError> {
        let data = data.trim();
        if data.is_empty() {
            return Ok(());
        }
        if data == "[DONE]" {
            self.done = true;
            return Ok(());
        }
        let chunk: Value = serde_json::from_str(data).map_err(|e| ProviderError::Parse(e.to_string()))?;
        if !chunk["error"].is_null() {
            return Err(ProviderError::Http {
                status: 500,
                message: chunk["error"]["message"].as_str().unwrap_or("stream error").to_owned(),
            });
        }
        if self.id.is_none() {
            self.id = chunk["id"].as_str().map(str::to_owned);
        }
        if self.model.is_none() {
            self.model = chunk["model"].as_str().map(str::to_owned);
        }
        if let Some(usage) = chunk.get("usage").filter(|u| !u.is_null()) {
            self.usage = map_usage(usage);
        }
        let choice = &chunk["choices"][0];
        if choice.is_null() {
            return Ok(());
        }
        let delta = &choice["delta"];
        if let Some(text) = delta["content"].as_str().filter(|t| !t.is_empty()) {
            self.text.push_str(text);
            sink(StreamDelta::Text(text.to_owned()));
        }
        let reasoning = delta["reasoning_content"].as_str().or(delta["reasoning"].as_str());
        if let Some(reasoning) = reasoning.filter(|r| !r.is_empty()) {
            sink(StreamDelta::Thinking(reasoning.to_owned()));
        }
        if let Some(calls) = delta["tool_calls"].as_array() {
            for call in calls {
                self.apply_tool_call(call, sink);
            }
        }
        if let Some(reason) = choice["finish_reason"].as_str() {
            self.finish_reason = Some(reason.to_owned());
        }
        Ok(())
    }

    fn apply_tool_call(&mut self, call: &Value, sink: DeltaSink<'_>) {
        let index = call["index"].as_u64().unwrap_or(0);
        let acc = self.tools.entry(index).or_insert_with(|| {
            let id = call["id"].as_str().map_or_else(|| format!("call_{index}"), str::to_owned);
            let name = call["function"]["name"].as_str().unwrap_or_default().to_owned();
            sink(StreamDelta::ToolUseStart { id: id.clone(), name: name.clone() });
            ToolAcc { id, name, args: String::new() }
        });
        // A real id may arrive after the generated `call_<index>` fallback.
        if acc.id == format!("call_{index}")
            && let Some(id) = call["id"].as_str().filter(|i| !i.is_empty())
        {
            acc.id = id.to_owned();
        }
        if let Some(name) = call["function"]["name"].as_str().filter(|n| !n.is_empty())
            && acc.name.is_empty()
        {
            acc.name = name.to_owned();
        }
        if let Some(part) = call["function"]["arguments"].as_str().filter(|a| !a.is_empty()) {
            acc.args.push_str(part);
            sink(StreamDelta::ToolInput { id: acc.id.clone(), partial_json: part.to_owned() });
        }
    }

    fn finish(self, default_model: &str, sink: DeltaSink<'_>) -> ChatResponse {
        sink(StreamDelta::Usage(self.usage));
        let has_tools = !self.tools.is_empty();
        let mut content = Vec::new();
        if !self.text.is_empty() {
            content.push(ContentBlock::text(self.text));
        }
        for acc in self.tools.into_values() {
            content.push(ContentBlock::tool_use(acc.id, acc.name, parse_tool_input(&acc.args)));
        }
        // Ollama and some compatible servers report "stop" even when they called tools.
        let stop_reason = match self.finish_reason.as_deref() {
            Some("stop") if has_tools => StopReason::ToolUse,
            Some(reason) => StopReason::parse(reason),
            None if has_tools => StopReason::ToolUse,
            None => StopReason::EndTurn,
        };
        ChatResponse {
            id: self.id,
            model: self.model.unwrap_or_else(|| default_model.to_owned()),
            content,
            stop_reason,
            usage: self.usage,
            stop_details: None,
        }
    }
}

/// Splits cached prompt tokens out of the reported prompt size.
fn map_usage(u: &Value) -> Usage {
    let prompt = u["prompt_tokens"].as_u64().unwrap_or(0);
    let cached = u["prompt_tokens_details"]["cached_tokens"].as_u64().unwrap_or(0);
    Usage {
        input_tokens: prompt.saturating_sub(cached),
        output_tokens: u["completion_tokens"].as_u64().unwrap_or(0),
        cache_read_input_tokens: cached,
        cache_creation_input_tokens: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crusty_core::ToolSpec;
    use crusty_core::message::Extra;

    #[test]
    fn maps_system_tools_and_results() {
        let mut req = ChatRequest::new("gpt-x", 64);
        req.system = vec![ContentBlock::text("be brief")];
        req.tools.push(ToolSpec::new("read", "read a file", json!({"type": "object"})));
        req.messages.push(Message::user_text("start"));
        req.messages.push(Message {
            role: Role::Assistant,
            content: vec![ContentBlock::text("calling"), ContentBlock::tool_use("c1", "read", json!({"path": "a"}))],
        });
        req.messages.push(Message {
            role: Role::User,
            content: vec![ContentBlock::tool_result("c1", vec![ContentBlock::text("file body")], false), ContentBlock::text("thanks")],
        });
        let body = to_openai_body(&req, "https://api.openai.com/v1");
        assert_eq!(body["messages"][0], json!({"role": "system", "content": "be brief"}));
        assert_eq!(body["messages"][2]["tool_calls"][0]["function"]["arguments"], json!("{\"path\":\"a\"}"));
        assert_eq!(body["messages"][3], json!({"role": "tool", "tool_call_id": "c1", "content": "file body"}));
        assert_eq!(body["messages"][4], json!({"role": "user", "content": "thanks"}));
        assert_eq!(body["tools"][0]["function"]["name"], json!("read"));
        assert_eq!(body["max_completion_tokens"], json!(64));
        assert!(body.get("max_tokens").is_none());
    }

    #[test]
    fn max_tokens_key_depends_on_host() {
        let req = ChatRequest::new("m", 9);
        let body = to_openai_body(&req, "http://localhost:11434/v1");
        assert_eq!(body["max_tokens"], json!(9));
        assert!(body.get("max_completion_tokens").is_none());
    }

    #[test]
    fn images_become_content_parts() {
        let mut req = ChatRequest::new("m", 1);
        req.messages.push(Message { role: Role::User, content: vec![ContentBlock::text("look"), ContentBlock::image_png_base64("AAAA")] });
        let body = to_openai_body(&req, "http://x");
        assert_eq!(body["messages"][0]["content"][1]["image_url"]["url"], json!("data:image/png;base64,AAAA"));
    }

    fn sink(_: StreamDelta) {}

    #[test]
    fn streams_tool_arguments_split_across_chunks_with_cached_usage() {
        let mut acc = Accumulator::default();
        let chunks = [
            r#"{"id":"r1","model":"gpt-x","choices":[{"delta":{"content":"Hi"}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_a","function":{"name":"read","arguments":"{\"pa"}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"th\":\"x\"}"}}]}}]}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
            r#"{"choices":[],"usage":{"prompt_tokens":100,"completion_tokens":20,"prompt_tokens_details":{"cached_tokens":60}}}"#,
            "[DONE]",
        ];
        for c in chunks {
            acc.apply(c, &sink).unwrap_or_else(|e| panic!("{e}"));
        }
        let resp = acc.finish("fallback", &sink);
        assert_eq!(resp.id.as_deref(), Some("r1"));
        assert_eq!(resp.stop_reason, StopReason::ToolUse);
        assert_eq!(resp.usage, Usage { input_tokens: 40, output_tokens: 20, cache_read_input_tokens: 60, cache_creation_input_tokens: 0 });
        assert_eq!(resp.content[0], ContentBlock::text("Hi"));
        assert!(matches!(&resp.content[1], ContentBlock::ToolUse { id, name, input, .. }
            if id == "call_a" && name == "read" && input == &json!({"path": "x"})));
    }

    #[test]
    fn in_stream_error_is_reported() {
        let mut acc = Accumulator::default();
        let err = acc.apply(r#"{"error":{"message":"quota"}}"#, &sink).err();
        assert_eq!(err, Some(ProviderError::Http { status: 500, message: "quota".into() }));
    }

    #[test]
    fn extra_effort_maps_for_openai_only() {
        let mut req = ChatRequest::new("o", 1);
        let mut extra = Extra::new();
        extra.insert("output_config".into(), json!({"effort": "max"}));
        req.extra = extra;
        assert_eq!(to_openai_body(&req, "https://api.openai.com/v1")["reasoning_effort"], json!("high"));
        assert!(to_openai_body(&req, "http://localhost:11434").get("reasoning_effort").is_none());
    }
}
