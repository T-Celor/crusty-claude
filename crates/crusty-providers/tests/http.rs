//! HTTP-level tests against a local wiremock server. No test reaches the real network.

use std::time::Duration;

use crusty_core::config::ProviderConfig;
use crusty_core::message::{ChatRequest, ContentBlock, StopReason, StreamDelta};
use crusty_core::provider::{CallOptions, Provider, ProviderError, ProviderKind};
use crusty_core::{CancellationToken, Message};
use crusty_providers::{AnthropicProvider, OpenAiProvider, http_client};
use serde_json::json;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const ANTHROPIC_TEXT_SSE: &str = "event: message_start\n\
data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"model\":\"claude-haiku-5-5\",\"usage\":{\"input_tokens\":12,\"output_tokens\":1}}}\n\n\
event: content_block_start\n\
data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}\n\n\
event: content_block_stop\n\
data: {\"type\":\"content_block_stop\",\"index\":0}\n\n\
event: message_delta\n\
data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":5}}\n\n\
event: message_stop\n\
data: {\"type\":\"message_stop\"}\n\n";

fn anthropic_cfg(base: &str) -> ProviderConfig {
    ProviderConfig { base_url: base.to_owned(), max_retries: Some(2), ..Default::default() }
}

fn provider(base: &str) -> AnthropicProvider {
    AnthropicProvider::new("anthropic", anthropic_cfg(base), http_client(30)).with_api_key(Some("test-key".into()))
}

fn request(model: &str) -> ChatRequest {
    let mut req = ChatRequest::new(model, 256);
    req.messages.push(Message::user_text("hi"));
    req
}

fn sse(body: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_raw(body.to_owned(), "text/event-stream")
}

fn noop(_: StreamDelta) {}

#[tokio::test]
async fn anthropic_streams_text_and_usage() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header("x-api-key", "test-key"))
        .respond_with(sse(ANTHROPIC_TEXT_SSE))
        .mount(&server)
        .await;
    let resp = provider(&server.uri())
        .stream(&request("claude-haiku-5-5"), &CallOptions::default(), &noop)
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(resp.text(), "Hello");
    assert_eq!(resp.id.as_deref(), Some("msg_1"));
    assert_eq!(resp.stop_reason, StopReason::EndTurn);
    assert_eq!(resp.usage.input_tokens, 12);
    assert_eq!(resp.usage.output_tokens, 5);
}

#[tokio::test]
async fn anthropic_rebuilds_tool_use_from_fragments() {
    let tool_sse = "event: content_block_start\n\
data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"tu_9\",\"name\":\"read\",\"input\":{}}}\n\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"path\\\":\"}}\n\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"\\\"a.rs\\\"}\"}}\n\n\
event: content_block_stop\n\
data: {\"type\":\"content_block_stop\",\"index\":1}\n\n\
event: content_block_start\n\
data: {\"type\":\"content_block_start\",\"index\":2,\"content_block\":{\"type\":\"tool_use\",\"id\":\"tu_bad\",\"name\":\"read\",\"input\":{}}}\n\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":2,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{oops\"}}\n\n\
event: content_block_stop\n\
data: {\"type\":\"content_block_stop\",\"index\":2}\n\n\
event: message_delta\n\
data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":7}}\n\n";
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/v1/messages")).respond_with(sse(tool_sse)).mount(&server).await;
    let resp = provider(&server.uri())
        .stream(&request("claude-haiku-5-5"), &CallOptions::default(), &noop)
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(resp.stop_reason, StopReason::ToolUse);
    assert!(matches!(&resp.content[0], ContentBlock::ToolUse { input, .. } if input == &json!({"path": "a.rs"})));
    assert!(matches!(&resp.content[1], ContentBlock::ToolUse { input, .. } if input["__crusty_invalid_json"] == json!("{oops")));
}

#[tokio::test]
async fn client_error_is_not_retried() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({"error": {"message": "bad field"}})))
        .expect(1)
        .mount(&server)
        .await;
    let out = provider(&server.uri()).stream(&request("m"), &CallOptions::default(), &noop).await;
    assert_eq!(out.err(), Some(ProviderError::Http { status: 400, message: "bad field".into() }));
}

#[tokio::test]
async fn overloaded_then_success_retries_once() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(529).set_body_json(json!({"error": {"message": "overloaded"}})))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST")).and(path("/v1/messages")).respond_with(sse(ANTHROPIC_TEXT_SSE)).mount(&server).await;
    let resp = provider(&server.uri()).stream(&request("m"), &CallOptions::default(), &noop).await.unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(resp.text(), "Hello");
    assert_eq!(server.received_requests().await.map_or(0, |r| r.len()), 2);
}

#[tokio::test]
async fn unauthorized_maps_to_auth() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({"error": {"message": "invalid key"}})))
        .mount(&server)
        .await;
    let out = provider(&server.uri()).stream(&request("m"), &CallOptions::default(), &noop).await;
    assert_eq!(out.err(), Some(ProviderError::Auth("invalid key".into())));
}

#[tokio::test]
async fn error_event_mid_stream_is_returned_not_retried() {
    let body = "event: message_start\n\
data: {\"type\":\"message_start\",\"message\":{\"id\":\"m\",\"model\":\"x\"}}\n\n\
event: error\n\
data: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}\n\n";
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/v1/messages")).respond_with(sse(body)).expect(1).mount(&server).await;
    let out = provider(&server.uri()).stream(&request("m"), &CallOptions::default(), &noop).await;
    assert_eq!(out.err(), Some(ProviderError::Http { status: 529, message: "Overloaded".into() }));
}

#[tokio::test]
async fn base_url_ending_in_v1_is_not_doubled() {
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/v1/messages")).respond_with(sse(ANTHROPIC_TEXT_SSE)).expect(1).mount(&server).await;
    let base = format!("{}/v1", server.uri());
    let resp = provider(&base).stream(&request("m"), &CallOptions::default(), &noop).await.unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(resp.text(), "Hello");
}

#[tokio::test]
async fn missing_key_for_official_host_is_auth_error() {
    let cfg = ProviderConfig {
        base_url: "https://api.anthropic.com".into(),
        api_key_env: Some("CRUSTY_TEST_UNSET_KEY_VAR".into()),
        ..Default::default()
    };
    let p = AnthropicProvider::new("anthropic", cfg, http_client(5));
    let out = p.stream(&request("m"), &CallOptions::default(), &noop).await;
    assert!(matches!(out, Err(ProviderError::Auth(_))));
}

#[tokio::test]
async fn cancellation_returns_cancelled() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(sse(ANTHROPIC_TEXT_SSE).set_delay(Duration::from_secs(5)))
        .mount(&server)
        .await;
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        trigger.cancel();
    });
    let opts = CallOptions { cancel, ..Default::default() };
    let out = provider(&server.uri()).stream(&request("m"), &opts, &noop).await;
    assert_eq!(out.err(), Some(ProviderError::Cancelled));
}

#[tokio::test]
async fn count_tokens_reads_input_tokens_and_fails_quietly() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages/count_tokens"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"input_tokens": 42})))
        .mount(&server)
        .await;
    assert_eq!(provider(&server.uri()).count_tokens(&request("m")).await, Some(42));
    let down = provider("http://127.0.0.1:9").count_tokens(&request("m")).await;
    assert_eq!(down, None);
}

#[tokio::test]
async fn openai_streams_tool_call_with_cached_usage() {
    let sse_body = "data: {\"id\":\"c1\",\"model\":\"gpt-x\",\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_z\",\"function\":{\"name\":\"read\",\"arguments\":\"{\\\"p\"}}]}}]}\n\n\
data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\":1}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\n\
data: {\"choices\":[],\"usage\":{\"prompt_tokens\":50,\"completion_tokens\":9,\"prompt_tokens_details\":{\"cached_tokens\":20}}}\n\n\
data: [DONE]\n\n";
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/chat/completions")).respond_with(sse(sse_body)).mount(&server).await;
    let cfg = ProviderConfig { kind: ProviderKind::Openai, base_url: server.uri(), ..Default::default() };
    let p = OpenAiProvider::new("ollama", cfg, http_client(30));
    let resp = p.stream(&request("gpt-x"), &CallOptions::default(), &noop).await.unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(resp.stop_reason, StopReason::ToolUse);
    assert_eq!(resp.usage.input_tokens, 30);
    assert_eq!(resp.usage.cache_read_input_tokens, 20);
    assert!(matches!(&resp.content[0], ContentBlock::ToolUse { id, input, .. } if id == "call_z" && input == &json!({"p": 1})));
}

fn truncated() -> ProviderError {
    ProviderError::Network("stream ended before the end of the response".into())
}

#[tokio::test]
async fn anthropic_stream_without_message_stop_is_an_error() {
    let body = "event: message_start\n\
data: {\"type\":\"message_start\",\"message\":{\"id\":\"m\",\"model\":\"x\"}}\n\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hel\"}}\n\n";
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/v1/messages")).respond_with(sse(body)).expect(1).mount(&server).await;
    let out = provider(&server.uri()).stream(&request("m"), &CallOptions::default(), &noop).await;
    assert_eq!(out.err(), Some(truncated()));
}

#[tokio::test]
async fn openai_stream_without_done_or_finish_is_an_error() {
    let body = "data: {\"id\":\"c2\",\"model\":\"gpt-x\",\"choices\":[{\"delta\":{\"content\":\"Hel\"}}]}\n\n";
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/chat/completions")).respond_with(sse(body)).expect(1).mount(&server).await;
    let cfg = ProviderConfig { kind: ProviderKind::Openai, base_url: server.uri(), ..Default::default() };
    let out = OpenAiProvider::new("ollama", cfg, http_client(30)).stream(&request("gpt-x"), &CallOptions::default(), &noop).await;
    assert_eq!(out.err(), Some(truncated()));
}

#[tokio::test]
async fn openai_stop_finish_with_tool_calls_is_tool_use() {
    let body = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_s\",\"function\":{\"name\":\"read\",\"arguments\":\"{}\"}}]}}]}\n\n\
data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
data: [DONE]\n\n";
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/chat/completions")).respond_with(sse(body)).mount(&server).await;
    let cfg = ProviderConfig { kind: ProviderKind::Openai, base_url: server.uri(), ..Default::default() };
    let resp = OpenAiProvider::new("ollama", cfg, http_client(30))
        .stream(&request("gpt-x"), &CallOptions::default(), &noop)
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(resp.stop_reason, StopReason::ToolUse);
}

#[tokio::test]
async fn openai_real_tool_id_arriving_later_replaces_fallback() {
    let body = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"name\":\"read\",\"arguments\":\"\"}}]}}]}\n\n\
data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_real\",\"function\":{\"arguments\":\"{}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\n\
data: [DONE]\n\n";
    let server = MockServer::start().await;
    Mock::given(method("POST")).and(path("/chat/completions")).respond_with(sse(body)).mount(&server).await;
    let cfg = ProviderConfig { kind: ProviderKind::Openai, base_url: server.uri(), ..Default::default() };
    let resp = OpenAiProvider::new("ollama", cfg, http_client(30))
        .stream(&request("gpt-x"), &CallOptions::default(), &noop)
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    assert!(matches!(&resp.content[0], ContentBlock::ToolUse { id, name, .. } if id == "call_real" && name == "read"));
}
