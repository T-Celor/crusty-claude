//! Anthropic Messages API (and Anthropic-compatible proxies).

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use crusty_core::config::ProviderConfig;
use crusty_core::message::{ChatRequest, ChatResponse, ContentBlock, StopReason, StreamDelta, Usage};
use crusty_core::provider::{CallOptions, DeltaSink, Provider, ProviderError, ProviderKind};
use reqwest::RequestBuilder;
use reqwest::header::CONTENT_TYPE;
use serde_json::{Value, json};

use crate::bedrock;
use crate::common::{
    EnvFn, ensure_success, for_each_sse, map_reqwest, non_blank, parse_tool_input, process_env, resolve_base, truncated_stream,
};
use crate::retry::with_retries;
use crate::sse::SseEvent;
use crate::vertex::{self, TokenSource, VERTEX_VERSION};

const DEFAULT_RETRIES: u32 = 3;
/// From this `anthropic-version` on, the API rejects sampling parameters.
const TEMPERATURE_REMOVED_FROM: &str = "2026-10-21";
const FALLBACK_MODELS: [&str; 4] = ["claude-fable-5-1", "claude-opus-5-5", "claude-opus-5", "claude-sonnet-5-5"];
const FALLBACK_BETA: &str = "server-side-fallback-2026-07-01";
const OAUTH_BETA: &str = "oauth-2025-04-20";
/// Maximum `cache_control` markers the API accepts per request.
const MAX_CACHE_MARKERS: usize = 4;
const API_HOST: &str = "api.anthropic.com";

/// How a call authenticates.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Credential {
    ApiKey(String),
    Bearer(String),
    /// Ready-made headers: Bedrock bearer or SigV4, Vertex OAuth.
    Headers(Vec<(String, String)>),
    None,
}

/// Where a Messages call goes and how it authenticates. Request bodies and stream handling
/// are shared by every endpoint.
pub(crate) enum Endpoint {
    /// `api.anthropic.com` or an Anthropic-compatible proxy: API key or bearer.
    FirstParty,
    /// Bedrock Mantle: bearer key or SigV4.
    Bedrock,
    /// Vertex `streamRawPredict`: OAuth bearer from the token source.
    Vertex(Arc<dyn TokenSource>),
}

impl Endpoint {
    fn kind(&self) -> ProviderKind {
        match self {
            Endpoint::FirstParty => ProviderKind::Anthropic,
            Endpoint::Bedrock => ProviderKind::Bedrock,
            Endpoint::Vertex(_) => ProviderKind::Vertex,
        }
    }
}

/// Anthropic Messages provider.
pub struct AnthropicProvider {
    id: String,
    cfg: ProviderConfig,
    http: reqwest::Client,
    api_key: Option<String>,
    endpoint: Endpoint,
    env: EnvFn,
}

impl AnthropicProvider {
    pub fn new(id: impl Into<String>, cfg: ProviderConfig, http: reqwest::Client) -> Self {
        Self::with_endpoint(id, cfg, http, Endpoint::FirstParty, process_env())
    }

    /// Provider on `endpoint` that reads configuration values through `env`.
    pub(crate) fn with_endpoint(id: impl Into<String>, cfg: ProviderConfig, http: reqwest::Client, endpoint: Endpoint, env: EnvFn) -> Self {
        AnthropicProvider { id: id.into(), cfg, http, api_key: None, endpoint, env }
    }

    /// Uses `key` instead of the environment variable (tests and explicit overrides).
    pub fn with_api_key(mut self, key: Option<String>) -> Self {
        self.api_key = key;
        self
    }

    /// Credentials of one call, for the endpoint it goes to. `method`, `url` and `body` are
    /// what SigV4 signs.
    async fn credential(&self, base: &str, method: &str, url: &str, body: &[u8]) -> Result<Credential, ProviderError> {
        match &self.endpoint {
            Endpoint::FirstParty => self.first_party_credential(base),
            Endpoint::Bedrock => Ok(Credential::Headers(bedrock::auth_headers(&self.cfg, &*self.env, method, url, body)?)),
            Endpoint::Vertex(tokens) => {
                let token = tokens.access_token().await?;
                Ok(Credential::Headers(vec![("authorization".to_owned(), format!("Bearer {token}"))]))
            }
        }
    }

    fn env_var(&self, name: Option<&str>) -> Option<String> {
        name.and_then(|n| non_blank((self.env)(n)))
    }

    fn first_party_credential(&self, base: &str) -> Result<Credential, ProviderError> {
        if let Some(key) = self.api_key.clone() {
            return Ok(Credential::ApiKey(key));
        }
        if let Some(key) = self.env_var(self.cfg.api_key_env.as_deref()) {
            return Ok(Credential::ApiKey(key));
        }
        if let Some(token) = self.env_var(self.cfg.auth_token_env.as_deref()) {
            return Ok(Credential::Bearer(token));
        }
        if is_anthropic_host(base) {
            return Err(ProviderError::Auth(format!(
                "set {} (or {}) to call {API_HOST}",
                self.cfg.api_key_env.as_deref().unwrap_or("ANTHROPIC_API_KEY"),
                self.cfg.auth_token_env.as_deref().unwrap_or("ANTHROPIC_AUTH_TOKEN"),
            )));
        }
        Ok(Credential::None)
    }

    /// Adds version, credential, configured headers and the merged `anthropic-beta` header.
    fn decorate(&self, mut rb: RequestBuilder, cred: &Credential, mut betas: Vec<String>, extra: &[(String, String)]) -> RequestBuilder {
        // Vertex carries the version in the body only.
        if !matches!(self.endpoint, Endpoint::Vertex(_)) {
            rb = rb.header("anthropic-version", self.cfg.anthropic_version.as_str());
        }
        match cred {
            Credential::ApiKey(key) => rb = rb.header("x-api-key", key.as_str()),
            Credential::Bearer(token) => {
                rb = rb.bearer_auth(token);
                betas.push(OAUTH_BETA.to_owned());
            }
            Credential::Headers(headers) => {
                for (name, value) in headers {
                    rb = rb.header(name.as_str(), value.as_str());
                }
            }
            Credential::None => {}
        }
        let custom = self.cfg.headers.iter().chain(extra.iter().map(|(k, v)| (k, v)));
        for (name, value) in custom {
            if name.eq_ignore_ascii_case("anthropic-beta") {
                betas.extend(value.split(',').map(str::trim).filter(|b| !b.is_empty()).map(str::to_owned));
            } else {
                rb = rb.header(name.as_str(), value.as_str());
            }
        }
        betas.sort();
        betas.dedup();
        if !betas.is_empty() {
            rb = rb.header("anthropic-beta", betas.join(","));
        }
        rb
    }

    /// Base URL of a call for this endpoint.
    fn base(&self, opts: &CallOptions) -> Result<String, ProviderError> {
        match &self.endpoint {
            Endpoint::FirstParty => resolve_base(opts, &self.cfg),
            Endpoint::Bedrock => bedrock::base_url(opts, &self.cfg, &*self.env),
            Endpoint::Vertex(_) => vertex::base_url(opts, &self.cfg, &*self.env),
        }
    }

    /// URL of a Messages call. Bedrock and first-party share `{base}/v1/messages`.
    fn messages_url(&self, base: &str, model: &str) -> Result<String, ProviderError> {
        match &self.endpoint {
            Endpoint::Vertex(_) => vertex::messages_url(base, &self.cfg, &*self.env, model),
            _ => Ok(endpoint(base, "/messages")),
        }
    }

    /// URL of the token-counting call.
    fn count_url(&self, base: &str) -> Result<String, ProviderError> {
        match &self.endpoint {
            Endpoint::Vertex(_) => vertex::count_url(base, &self.cfg, &*self.env),
            _ => Ok(endpoint(base, "/messages/count_tokens")),
        }
    }

    /// Applies the endpoint's body rewrites: the Bedrock model id, or Vertex's version and
    /// (for Messages calls, `drop_model`) the model that moves into the URL.
    pub(crate) fn shape_body(&self, body: &mut Value, model: &str, drop_model: bool) {
        match &self.endpoint {
            Endpoint::FirstParty => {}
            Endpoint::Bedrock => body["model"] = json!(bedrock::model_id(model)),
            Endpoint::Vertex(_) => {
                if let Some(obj) = body.as_object_mut() {
                    obj.entry("anthropic_version").or_insert_with(|| json!(VERTEX_VERSION));
                    if drop_model {
                        obj.remove("model");
                    }
                }
            }
        }
    }

    /// POSTs a JSON body with this endpoint's credentials. The status is left to the caller.
    async fn post_json(
        &self,
        base: &str,
        url: &str,
        body: &Value,
        betas: Vec<String>,
        extra: &[(String, String)],
    ) -> Result<reqwest::Response, ProviderError> {
        let bytes = serde_json::to_vec(body).map_err(|e| ProviderError::Parse(e.to_string()))?;
        let cred = self.credential(base, "POST", url, &bytes).await?;
        let rb = self.http.post(url).header(CONTENT_TYPE, "application/json").body(bytes);
        self.decorate(rb, &cred, betas, extra).send().await.map_err(map_reqwest)
    }

    async fn send_messages(&self, req: &ChatRequest, opts: &CallOptions) -> Result<reqwest::Response, ProviderError> {
        let base = self.base(opts)?;
        let first_party = matches!(self.endpoint, Endpoint::FirstParty);
        let mut cfg = self.cfg.clone();
        // Server-side fallbacks and eager tool streaming are first-party features.
        cfg.server_fallbacks &= first_party;
        let eager_default = first_party && opts.base_url_override.is_none() && self.cfg.via_proxy.is_none();
        let (mut body, betas) = build_body(req, &cfg, eager_default);
        self.shape_body(&mut body, &req.model, true);
        let url = self.messages_url(&base, &req.model)?;
        let resp = self.post_json(&base, &url, &body, betas, &opts.extra_headers).await?;
        ensure_success(resp).await
    }

    fn max_retries(&self) -> u32 {
        self.cfg.max_retries.unwrap_or(DEFAULT_RETRIES)
    }
}

#[async_trait]
impl Provider for AnthropicProvider {
    fn id(&self) -> &str {
        &self.id
    }

    fn kind(&self) -> ProviderKind {
        self.endpoint.kind()
    }

    async fn stream(&self, req: &ChatRequest, opts: &CallOptions, on_delta: DeltaSink<'_>) -> Result<ChatResponse, ProviderError> {
        let resp = with_retries(self.max_retries(), &opts.cancel, |_| self.send_messages(req, opts)).await?;
        let mut acc = Accumulator::default();
        for_each_sse(resp, &opts.cancel, |event| acc.apply(&event, on_delta)).await?;
        if !acc.complete {
            return Err(truncated_stream());
        }
        Ok(acc.finish(on_delta))
    }

    async fn count_tokens(&self, req: &ChatRequest) -> Option<u64> {
        let base = self.base(&CallOptions::default()).ok()?;
        let value = serde_json::to_value(req).ok()?;
        let mut body = json!({ "model": req.model, "messages": value["messages"] });
        if !req.system.is_empty() {
            body["system"] = value["system"].clone();
        }
        if !req.tools.is_empty() {
            body["tools"] = value["tools"].clone();
        }
        self.shape_body(&mut body, &req.model, false);
        let url = self.count_url(&base).ok()?;
        let resp = self.post_json(&base, &url, &body, Vec::new(), &[]).await.ok()?;
        let resp = ensure_success(resp).await.ok()?;
        let v: Value = resp.json().await.ok()?;
        v["input_tokens"].as_u64()
    }

    async fn list_models(&self) -> Result<Vec<String>, ProviderError> {
        // Only the first-party API has a model list in this crate.
        if !matches!(self.endpoint, Endpoint::FirstParty) {
            return Ok(Vec::new());
        }
        let base = self.base(&CallOptions::default())?;
        let url = endpoint(&base, "/models");
        let cred = self.credential(&base, "GET", &url, &[]).await?;
        let resp = self.decorate(self.http.get(&url), &cred, Vec::new(), &[]).send().await.map_err(map_reqwest)?;
        let resp = ensure_success(resp).await?;
        let v: Value = resp.json().await.map_err(|e| ProviderError::Parse(e.to_string()))?;
        Ok(v["data"].as_array().map(|items| items.iter().filter_map(|m| m["id"].as_str().map(str::to_owned)).collect()).unwrap_or_default())
    }
}

/// `{base}/v1{path}`, or `{base}{path}` when the base already ends in `/v1`.
fn endpoint(base: &str, path: &str) -> String {
    if base.ends_with("/v1") { format!("{base}{path}") } else { format!("{base}/v1{path}") }
}

fn is_anthropic_host(base: &str) -> bool {
    url::Url::parse(base).ok().and_then(|u| u.host_str().map(|h| h == API_HOST)).unwrap_or(false)
}

/// Serializes the request and applies the Anthropic-specific rewrites. Returns the body and
/// the `anthropic-beta` values the rewrites need.
fn build_body(req: &ChatRequest, cfg: &ProviderConfig, eager_default: bool) -> (Value, Vec<String>) {
    let mut body = serde_json::to_value(req).unwrap_or(Value::Null);
    let mut betas = Vec::new();
    body["stream"] = json!(true);

    if cfg.anthropic_version.as_str() >= TEMPERATURE_REMOVED_FROM
        && let Some(obj) = body.as_object_mut()
    {
        obj.remove("temperature");
    }

    if cfg.server_fallbacks && FALLBACK_MODELS.contains(&req.model.as_str()) && body.get("fallbacks").is_none() {
        body["fallbacks"] = json!("default");
        betas.push(FALLBACK_BETA.to_owned());
    }

    if cfg.eager_input_streaming.unwrap_or(eager_default)
        && let Some(tools) = body.get_mut("tools").and_then(Value::as_array_mut)
    {
        for obj in tools.iter_mut().filter_map(Value::as_object_mut) {
            if obj.contains_key("input_schema") {
                obj.insert("eager_input_streaming".into(), json!(true));
            }
        }
    }

    // `thinking.block_binding` is rejected without its beta.
    if req.has_block_binding() {
        betas.push(crusty_core::message::THINKING_BINDING_BETA.to_owned());
    }

    add_cache_markers(&mut body);
    (body, betas)
}

/// Marks the last tool, the last system block and the last content block of the last
/// message with an ephemeral `cache_control`, without exceeding `MAX_CACHE_MARKERS`.
fn add_cache_markers(body: &mut Value) {
    let mut budget = MAX_CACHE_MARKERS.saturating_sub(count_markers(body));
    let tool = body.get_mut("tools").and_then(Value::as_array_mut).and_then(|a| a.last_mut());
    mark(tool, &mut budget);
    let system = body.get_mut("system").and_then(Value::as_array_mut).and_then(|a| a.last_mut());
    mark(system, &mut budget);
    let block = body
        .get_mut("messages")
        .and_then(Value::as_array_mut)
        .and_then(|m| m.last_mut())
        .and_then(|m| m.get_mut("content"))
        .and_then(Value::as_array_mut)
        .and_then(|c| c.last_mut());
    mark(block, &mut budget);
}

fn mark(target: Option<&mut Value>, budget: &mut usize) {
    if *budget == 0 {
        return;
    }
    if let Some(obj) = target.and_then(Value::as_object_mut)
        && !obj.contains_key("cache_control")
    {
        obj.insert("cache_control".into(), json!({ "type": "ephemeral" }));
        *budget -= 1;
    }
}

fn count_markers(v: &Value) -> usize {
    match v {
        Value::Object(map) => usize::from(map.contains_key("cache_control")) + map.values().map(count_markers).sum::<usize>(),
        Value::Array(items) => items.iter().map(count_markers).sum(),
        _ => 0,
    }
}

/// Maps an in-stream `error` object to the HTTP-style error the retry rules understand.
fn stream_error(error: &Value) -> ProviderError {
    let message = error["message"].as_str().unwrap_or("stream error").to_owned();
    let status = match error["type"].as_str() {
        Some("overloaded_error") => 529,
        Some("rate_limit_error") => 429,
        _ => 500,
    };
    ProviderError::Http { status, message }
}

fn index_of(data: &Value) -> u64 {
    data["index"].as_u64().unwrap_or(0)
}

fn merge_usage(usage: &mut Usage, v: &Value) {
    if let Some(n) = v["input_tokens"].as_u64() {
        usage.input_tokens = n;
    }
    if let Some(n) = v["output_tokens"].as_u64() {
        usage.output_tokens = n;
    }
    if let Some(n) = v["cache_read_input_tokens"].as_u64() {
        usage.cache_read_input_tokens = n;
    }
    if let Some(n) = v["cache_creation_input_tokens"].as_u64() {
        usage.cache_creation_input_tokens = n;
    }
}

/// Rebuilds a response from Messages stream events.
#[derive(Default)]
struct Accumulator {
    id: Option<String>,
    model: String,
    usage: Usage,
    stop_reason: String,
    stop_details: Option<Value>,
    blocks: BTreeMap<u64, ContentBlock>,
    tool_json: BTreeMap<u64, String>,
    /// Set by `message_stop` or by a `message_delta` carrying a stop reason.
    complete: bool,
}

impl Accumulator {
    fn apply(&mut self, event: &SseEvent, sink: DeltaSink<'_>) -> Result<(), ProviderError> {
        let data: Value = serde_json::from_str(&event.data).map_err(|e| ProviderError::Parse(e.to_string()))?;
        let kind = data["type"].as_str().or(event.event.as_deref()).unwrap_or_default();
        match kind {
            "message_start" => {
                let msg = &data["message"];
                self.id = msg["id"].as_str().map(str::to_owned);
                if let Some(model) = msg["model"].as_str() {
                    self.model = model.to_owned();
                }
                merge_usage(&mut self.usage, &msg["usage"]);
            }
            "content_block_start" => self.block_start(&data, sink),
            "content_block_delta" => self.block_delta(&data, sink),
            "content_block_stop" => self.block_stop(index_of(&data)),
            "message_delta" => {
                let delta = &data["delta"];
                if let Some(reason) = delta["stop_reason"].as_str() {
                    self.stop_reason = reason.to_owned();
                    self.complete = true;
                }
                if !delta["stop_details"].is_null() {
                    self.stop_details = Some(delta["stop_details"].clone());
                }
                merge_usage(&mut self.usage, &data["usage"]);
            }
            "message_stop" => self.complete = true,
            "error" => return Err(stream_error(&data["error"])),
            _ => {}
        }
        Ok(())
    }

    fn block_start(&mut self, data: &Value, sink: DeltaSink<'_>) {
        let index = index_of(data);
        let block = ContentBlock::from_value(data["content_block"].clone());
        if let ContentBlock::ToolUse { id, name, .. } = &block {
            sink(StreamDelta::ToolUseStart { id: id.clone(), name: name.clone() });
            self.tool_json.insert(index, String::new());
        }
        self.blocks.insert(index, block);
    }

    fn block_delta(&mut self, data: &Value, sink: DeltaSink<'_>) {
        let index = index_of(data);
        let delta = &data["delta"];
        match delta["type"].as_str() {
            Some("text_delta") => {
                let text = delta["text"].as_str().unwrap_or_default();
                if let Some(ContentBlock::Text { text: buf, .. }) = self.blocks.get_mut(&index) {
                    buf.push_str(text);
                }
                sink(StreamDelta::Text(text.to_owned()));
            }
            Some("input_json_delta") => {
                let part = delta["partial_json"].as_str().unwrap_or_default();
                self.tool_json.entry(index).or_default().push_str(part);
                let id = match self.blocks.get(&index) {
                    Some(ContentBlock::ToolUse { id, .. }) => id.clone(),
                    _ => String::new(),
                };
                sink(StreamDelta::ToolInput { id, partial_json: part.to_owned() });
            }
            Some("thinking_delta") => {
                let text = delta["thinking"].as_str().unwrap_or_default();
                if let Some(ContentBlock::Thinking { thinking, .. }) = self.blocks.get_mut(&index) {
                    thinking.push_str(text);
                }
                sink(StreamDelta::Thinking(text.to_owned()));
            }
            Some("signature_delta") => {
                if let Some(ContentBlock::Thinking { signature, .. }) = self.blocks.get_mut(&index) {
                    *signature = delta["signature"].as_str().map(str::to_owned);
                }
            }
            Some("citations_delta") => {
                if let Some(ContentBlock::Text { extra, .. }) = self.blocks.get_mut(&index) {
                    let list = extra.entry("citations").or_insert_with(|| Value::Array(Vec::new()));
                    if let Some(list) = list.as_array_mut() {
                        list.push(delta["citation"].clone());
                    }
                }
            }
            _ => {}
        }
    }

    fn block_stop(&mut self, index: u64) {
        if let Some(raw) = self.tool_json.remove(&index) {
            self.set_tool_input(index, &raw);
        }
    }

    fn set_tool_input(&mut self, index: u64, raw: &str) {
        if let Some(ContentBlock::ToolUse { input, .. }) = self.blocks.get_mut(&index) {
            *input = parse_tool_input(raw);
        }
    }

    fn finish(mut self, sink: DeltaSink<'_>) -> ChatResponse {
        for (index, raw) in std::mem::take(&mut self.tool_json) {
            self.set_tool_input(index, &raw);
        }
        sink(StreamDelta::Usage(self.usage));
        ChatResponse {
            id: self.id,
            model: self.model,
            content: self.blocks.into_values().collect(),
            stop_reason: StopReason::parse(&self.stop_reason),
            usage: self.usage,
            stop_details: self.stop_details,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> ProviderConfig {
        ProviderConfig { anthropic_version: "2023-06-01".into(), ..Default::default() }
    }

    fn request(model: &str) -> ChatRequest {
        let mut req = ChatRequest::new(model, 100);
        req.temperature = Some(0.5);
        req.messages.push(crusty_core::Message::user_text("hi"));
        req
    }

    #[test]
    fn endpoint_handles_v1_suffix() {
        assert_eq!(endpoint("https://x.test", "/messages"), "https://x.test/v1/messages");
        assert_eq!(endpoint("https://x.test/v1", "/messages"), "https://x.test/v1/messages");
    }

    #[test]
    fn temperature_depends_on_api_version() {
        let (kept, _) = build_body(&request("m"), &cfg(), false);
        assert_eq!(kept["temperature"], json!(0.5));
        let newer = ProviderConfig { anthropic_version: "2026-10-21".into(), ..cfg() };
        let (dropped, _) = build_body(&request("m"), &newer, false);
        assert!(dropped.get("temperature").is_none());
    }

    #[test]
    fn server_fallbacks_only_for_listed_models() {
        let (body, betas) = build_body(&request("claude-opus-5-5"), &cfg(), false);
        assert_eq!(body["fallbacks"], json!("default"));
        assert_eq!(betas, vec![FALLBACK_BETA.to_owned()]);
        let (body, betas) = build_body(&request("claude-haiku-5-5"), &cfg(), false);
        assert!(body.get("fallbacks").is_none());
        assert!(betas.is_empty());
    }

    #[test]
    fn eager_input_streaming_follows_default_and_override() {
        let mut req = request("m");
        req.tools.push(crusty_core::ToolSpec::new("t", "d", json!({"type": "object"})));
        let (on, _) = build_body(&req, &cfg(), true);
        assert_eq!(on["tools"][0]["eager_input_streaming"], json!(true));
        let (off, _) = build_body(&req, &cfg(), false);
        assert!(off["tools"][0].get("eager_input_streaming").is_none());
    }

    #[test]
    fn cache_markers_never_exceed_four() {
        let mut req = request("m");
        req.system = vec![ContentBlock::text("sys")];
        req.tools = vec![crusty_core::ToolSpec::new("t", "d", json!({"type": "object"}))];
        req.messages.push(crusty_core::Message::assistant_text("a"));
        req.messages.push(crusty_core::Message::user_text("b"));
        let (body, _) = build_body(&req, &cfg(), false);
        assert_eq!(count_markers(&body), 3);
        assert!(body["messages"][1]["content"][0].get("cache_control").is_none());
        assert_eq!(body["messages"][2]["content"][0]["cache_control"]["type"], json!("ephemeral"));
    }

    #[test]
    fn accumulator_rebuilds_tool_use_and_usage() {
        let sink = |_: StreamDelta| {};
        let mut acc = Accumulator::default();
        let events = [
            (
                "message_start",
                r#"{"type":"message_start","message":{"id":"msg_1","model":"m","usage":{"input_tokens":7,"output_tokens":1}}}"#,
            ),
            (
                "content_block_start",
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"tu_1","name":"read","input":{}}}"#,
            ),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"path\":"}}"#,
            ),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"\"a.rs\"}"}}"#,
            ),
            ("content_block_stop", r#"{"type":"content_block_stop","index":0}"#),
            ("message_delta", r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":42}}"#),
        ];
        for (name, data) in events {
            acc.apply(&SseEvent { event: Some(name.into()), data: data.into() }, &sink).unwrap_or_else(|e| panic!("{e}"));
        }
        let resp = acc.finish(&sink);
        assert_eq!(resp.id.as_deref(), Some("msg_1"));
        assert_eq!(resp.stop_reason, StopReason::ToolUse);
        assert_eq!(resp.usage.input_tokens, 7);
        assert_eq!(resp.usage.output_tokens, 42);
        assert!(matches!(&resp.content[0], ContentBlock::ToolUse { input, .. } if input == &json!({"path": "a.rs"})));
    }

    #[test]
    fn invalid_tool_json_is_marked() {
        let sink = |_: StreamDelta| {};
        let mut acc = Accumulator::default();
        acc.blocks.insert(0, ContentBlock::tool_use("t", "x", json!({})));
        acc.tool_json.insert(0, "{oops".into());
        let resp = acc.finish(&sink);
        assert!(matches!(&resp.content[0], ContentBlock::ToolUse { input, .. } if input["__crusty_invalid_json"] == json!("{oops")));
    }

    #[test]
    fn stream_error_maps_overload_to_529() {
        let err = stream_error(&json!({"type": "overloaded_error", "message": "busy"}));
        assert_eq!(err, ProviderError::Http { status: 529, message: "busy".into() });
    }
}
