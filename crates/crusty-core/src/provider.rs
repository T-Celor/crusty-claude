//! The model-provider contract. Implementations live in `crusty-providers`.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::message::{ChatRequest, ChatResponse, StreamDelta, Usage};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    /// Anthropic Messages API (also any Anthropic-compatible proxy).
    #[default]
    Anthropic,
    /// OpenAI Chat Completions and compatible servers (OpenRouter, Ollama, Gemini, vLLM, ...).
    Openai,
    /// Scripted offline provider for tests and demos.
    Mock,
}

/// `provider:model`, as written in configs and on the command line.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ModelRef {
    pub provider: String,
    pub model: String,
}

impl ModelRef {
    pub fn new(provider: impl Into<String>, model: impl Into<String>) -> Self {
        ModelRef { provider: provider.into(), model: model.into() }
    }

    /// Parses `provider:model`. A bare model name uses `default_provider`.
    pub fn parse(s: &str, default_provider: &str) -> Option<Self> {
        let s = s.trim();
        if s.is_empty() {
            return None;
        }
        match s.split_once(':') {
            Some((p, m)) if !p.is_empty() && !m.is_empty() => Some(ModelRef::new(p, m)),
            Some(_) => None,
            None => Some(ModelRef::new(default_provider, s)),
        }
    }
}

impl std::fmt::Display for ModelRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.provider, self.model)
    }
}

/// Per-call options that are not part of the request body.
#[derive(Debug, Clone, Default)]
pub struct CallOptions {
    /// Send this call to another base URL (a token-saving proxy picked by the optimizer).
    pub base_url_override: Option<String>,
    pub cancel: CancellationToken,
    /// Extra non-secret headers for this call.
    pub extra_headers: Vec<(String, String)>,
}

#[derive(Debug, Clone, thiserror::Error, PartialEq)]
pub enum ProviderError {
    #[error("HTTP {status}: {message}")]
    Http { status: u16, message: String },
    #[error("network error: {0}")]
    Network(String),
    #[error("request timed out")]
    Timeout,
    #[error("cancelled")]
    Cancelled,
    #[error("invalid response: {0}")]
    Parse(String),
    #[error("authentication: {0}")]
    Auth(String),
    #[error("configuration: {0}")]
    Config(String),
}

impl ProviderError {
    /// Worth retrying the same call after a backoff.
    pub fn is_retryable(&self) -> bool {
        match self {
            ProviderError::Http { status, .. } => matches!(status, 408 | 409 | 429 | 500..=599),
            ProviderError::Network(_) | ProviderError::Timeout => true,
            _ => false,
        }
    }

    /// The model cannot serve right now: a fallback model may be tried.
    pub fn is_unavailable(&self) -> bool {
        match self {
            ProviderError::Http { status, .. } => matches!(status, 404 | 429 | 500..=599),
            ProviderError::Network(_) | ProviderError::Timeout => true,
            _ => false,
        }
    }
}

/// Callback receiving stream deltas for live display.
pub type DeltaSink<'a> = &'a (dyn Fn(StreamDelta) + Send + Sync);

#[async_trait]
pub trait Provider: Send + Sync {
    /// Name of the provider in the config (`anthropic`, `ollama`, ...).
    fn id(&self) -> &str;

    fn kind(&self) -> ProviderKind;

    /// Whether image blocks can be sent to this model.
    fn supports_images(&self, _model: &str) -> bool {
        true
    }

    /// Runs one model call, streaming deltas to `on_delta`, and returns the full response.
    async fn stream(&self, req: &ChatRequest, opts: &CallOptions, on_delta: DeltaSink<'_>) -> Result<ChatResponse, ProviderError>;

    /// Real input-token count for `req`, when the provider offers a counting endpoint.
    async fn count_tokens(&self, _req: &ChatRequest) -> Option<u64> {
        None
    }

    async fn list_models(&self) -> Result<Vec<String>, ProviderError> {
        Ok(Vec::new())
    }
}

/// Usage plus where it came from, as reported by a provider.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderUsage {
    pub provider: String,
    pub model: String,
    pub usage: Usage,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_ref_parsing() {
        assert_eq!(ModelRef::parse("openai:gpt-x", "anthropic"), Some(ModelRef::new("openai", "gpt-x")));
        assert_eq!(ModelRef::parse("claude-opus-5-5", "anthropic"), Some(ModelRef::new("anthropic", "claude-opus-5-5")));
        assert_eq!(ModelRef::parse(":x", "a"), None);
        assert_eq!(ModelRef::parse("  ", "a"), None);
    }

    #[test]
    fn retry_classification() {
        assert!(ProviderError::Http { status: 529, message: String::new() }.is_retryable());
        assert!(!ProviderError::Http { status: 400, message: String::new() }.is_retryable());
        assert!(ProviderError::Http { status: 404, message: String::new() }.is_unavailable());
    }
}
