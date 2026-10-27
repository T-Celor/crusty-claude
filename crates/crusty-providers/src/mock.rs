//! Scripted offline provider for tests and demos. It makes no network calls.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use crusty_core::message::{ChatRequest, ChatResponse, ContentBlock, Role, StopReason, StreamDelta, Usage};
use crusty_core::provider::{CallOptions, DeltaSink, Provider, ProviderError, ProviderKind};
use crusty_core::tokens::{estimate_request, estimate_text};
use serde_json::Value;

/// Longest echo returned by an unscripted mock.
const ECHO_LIMIT: usize = 200;
const DONE_REPLY: &str = "(mock) done";

/// One scripted reply.
#[derive(Debug, Clone)]
pub enum MockTurn {
    Text(String),
    ToolUse { name: String, input: Value },
    Error(ProviderError),
}

/// Provider that replies from a script, or echoes the last user text when unscripted.
pub struct MockProvider {
    id: String,
    script: Option<Mutex<VecDeque<MockTurn>>>,
    requests: Mutex<Vec<ChatRequest>>,
    tool_calls: AtomicU64,
}

impl MockProvider {
    /// Unscripted: replies `(mock) <last user text>`.
    pub fn new(id: impl Into<String>) -> Self {
        MockProvider { id: id.into(), script: None, requests: Mutex::new(Vec::new()), tool_calls: AtomicU64::new(0) }
    }

    /// Scripted: each call pops one turn; once the script is exhausted it replies `(mock) done`.
    pub fn scripted(id: impl Into<String>, turns: Vec<MockTurn>) -> Self {
        MockProvider { script: Some(Mutex::new(turns.into())), ..MockProvider::new(id) }
    }

    /// Every request received so far, in order.
    pub fn requests(&self) -> Vec<ChatRequest> {
        lock(&self.requests).clone()
    }

    fn next_turn(&self, req: &ChatRequest) -> MockTurn {
        match &self.script {
            Some(queue) => lock(queue).pop_front().unwrap_or_else(|| MockTurn::Text(DONE_REPLY.into())),
            None => {
                let last = req.messages.iter().rev().find(|m| m.role == Role::User && !m.text().is_empty());
                let echo: String = last.map(|m| m.text()).unwrap_or_default().chars().take(ECHO_LIMIT).collect();
                MockTurn::Text(format!("(mock) {echo}"))
            }
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[async_trait]
impl Provider for MockProvider {
    fn id(&self) -> &str {
        &self.id
    }

    fn kind(&self) -> ProviderKind {
        ProviderKind::Mock
    }

    async fn stream(&self, req: &ChatRequest, opts: &CallOptions, on_delta: DeltaSink<'_>) -> Result<ChatResponse, ProviderError> {
        if opts.cancel.is_cancelled() {
            return Err(ProviderError::Cancelled);
        }
        lock(&self.requests).push(req.clone());
        let (content, stop_reason, output) = match self.next_turn(req) {
            MockTurn::Text(text) => {
                on_delta(StreamDelta::Text(text.clone()));
                let output = crusty_core::tokens::estimate_text(&text);
                (vec![ContentBlock::text(text)], StopReason::EndTurn, output)
            }
            MockTurn::ToolUse { name, input } => {
                let n = self.tool_calls.fetch_add(1, Ordering::Relaxed);
                let id = format!("mock_tool_{n}");
                let raw = input.to_string();
                on_delta(StreamDelta::ToolUseStart { id: id.clone(), name: name.clone() });
                on_delta(StreamDelta::ToolInput { id: id.clone(), partial_json: raw.clone() });
                (vec![ContentBlock::tool_use(id, name, input)], StopReason::ToolUse, estimate_text(&raw))
            }
            MockTurn::Error(err) => return Err(err),
        };
        let usage = Usage { input_tokens: estimate_request(req, false), output_tokens: output, ..Usage::default() };
        on_delta(StreamDelta::Usage(usage));
        Ok(ChatResponse { id: None, model: req.model.clone(), content, stop_reason, usage, stop_details: None })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crusty_core::Message;
    use serde_json::json;

    fn sink(_: StreamDelta) {}

    #[tokio::test]
    async fn unscripted_echoes_last_user_text_truncated() {
        let mock = MockProvider::new("m");
        let mut req = ChatRequest::new("x", 10);
        req.messages.push(Message::user_text("a".repeat(300)));
        let resp = mock.stream(&req, &CallOptions::default(), &sink).await.unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(resp.text(), format!("(mock) {}", "a".repeat(200)));
    }

    #[tokio::test]
    async fn scripted_turns_pop_in_order_then_finish() {
        let mock = MockProvider::scripted(
            "m",
            vec![MockTurn::ToolUse { name: "read".into(), input: json!({"p": 1}) }, MockTurn::Text("done now".into())],
        );
        let req = ChatRequest::new("x", 10);
        let opts = CallOptions::default();
        let first = mock.stream(&req, &opts, &sink).await.unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(first.stop_reason, StopReason::ToolUse);
        assert!(matches!(&first.content[0], ContentBlock::ToolUse { id, .. } if id == "mock_tool_0"));
        let second = mock.stream(&req, &opts, &sink).await.unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(second.text(), "done now");
        let third = mock.stream(&req, &opts, &sink).await.unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(third.text(), DONE_REPLY);
        assert_eq!(mock.requests().len(), 3);
    }

    #[tokio::test]
    async fn scripted_error_is_returned() {
        let mock = MockProvider::scripted("m", vec![MockTurn::Error(ProviderError::Timeout)]);
        let out = mock.stream(&ChatRequest::new("x", 1), &CallOptions::default(), &sink).await;
        assert_eq!(out.err(), Some(ProviderError::Timeout));
    }
}
