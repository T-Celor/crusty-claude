//! Helpers shared by the HTTP providers: base URLs, environment credentials, error
//! mapping and the cancellable SSE body loop.

use std::pin::pin;

use crusty_core::config::ProviderConfig;
use crusty_core::{CallOptions, CancellationToken, ProviderError};
use futures::StreamExt;
use serde_json::Value;

use crate::sse::{SseEvent, SseParser};

/// Largest error body read before giving up on a parsed message.
const ERROR_BODY_LIMIT: usize = 64 * 1024;
/// Characters of a non-JSON error body kept in the error message.
const ERROR_TEXT_LIMIT: usize = 500;

/// Base URL of a call: the per-call override wins, trailing slashes are dropped.
pub(crate) fn resolve_base(opts: &CallOptions, cfg: &ProviderConfig) -> Result<String, ProviderError> {
    let base = opts.base_url_override.as_deref().unwrap_or(cfg.base_url.as_str()).trim_end_matches('/');
    if base.is_empty() {
        return Err(ProviderError::Config("base_url is empty".into()));
    }
    Ok(base.to_owned())
}

/// Value of the named environment variable, `None` when unset, empty or unnamed.
pub(crate) fn env_value(name: Option<&str>) -> Option<String> {
    let name = name?;
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

pub(crate) fn map_reqwest(err: reqwest::Error) -> ProviderError {
    if err.is_timeout() { ProviderError::Timeout } else { ProviderError::Network(err.to_string()) }
}

/// Reads at most `ERROR_BODY_LIMIT` bytes of a response body, lossily decoded.
async fn read_capped(resp: reqwest::Response) -> String {
    let mut stream = pin!(resp.bytes_stream());
    let mut buf = Vec::new();
    while let Some(Ok(chunk)) = stream.next().await {
        let room = ERROR_BODY_LIMIT - buf.len();
        buf.extend_from_slice(&chunk[..chunk.len().min(room)]);
        if buf.len() >= ERROR_BODY_LIMIT {
            break;
        }
    }
    String::from_utf8_lossy(&buf).into_owned()
}

/// `error.message` of a JSON error body, else the start of the raw text.
fn error_message(body: &str) -> String {
    serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| v.pointer("/error/message").and_then(Value::as_str).map(str::to_owned))
        .unwrap_or_else(|| body.chars().take(ERROR_TEXT_LIMIT).collect())
}

/// Passes a 2xx response through; maps any other status to an error.
pub(crate) async fn ensure_success(resp: reqwest::Response) -> Result<reqwest::Response, ProviderError> {
    let status = resp.status().as_u16();
    if (200..300).contains(&status) {
        return Ok(resp);
    }
    let message = error_message(&read_capped(resp).await);
    Err(match status {
        401 | 403 => ProviderError::Auth(message),
        _ => ProviderError::Http { status, message },
    })
}

/// Feeds every SSE event of a response to `handle`, stopping on cancellation or on the
/// first error that `handle` returns.
pub(crate) async fn for_each_sse<F>(resp: reqwest::Response, cancel: &CancellationToken, mut handle: F) -> Result<(), ProviderError>
where
    F: FnMut(SseEvent) -> Result<(), ProviderError>,
{
    let mut stream = pin!(resp.bytes_stream());
    let mut parser = SseParser::new();
    loop {
        let next = tokio::select! {
            _ = cancel.cancelled() => return Err(ProviderError::Cancelled),
            next = stream.next() => next,
        };
        match next {
            None => break,
            Some(Ok(chunk)) => {
                for event in parser.push(chunk.as_ref()) {
                    handle(event)?;
                }
            }
            Some(Err(err)) => return Err(map_reqwest(err)),
        }
    }
    for event in parser.finish() {
        handle(event)?;
    }
    Ok(())
}

/// Error for a stream that closed before its protocol end marker. Not retried here: the
/// stream had started, so the engine's model fallback decides what happens next.
pub(crate) fn truncated_stream() -> ProviderError {
    ProviderError::Network("stream ended before the end of the response".into())
}

/// Parses a tool call's accumulated argument string. Empty input is `{}`; invalid JSON is
/// kept under `__crusty_invalid_json` so the engine can report it as a tool error.
pub(crate) fn parse_tool_input(raw: &str) -> Value {
    if raw.trim().is_empty() {
        return Value::Object(serde_json::Map::new());
    }
    serde_json::from_str(raw).unwrap_or_else(|_| serde_json::json!({ "__crusty_invalid_json": raw }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_input_parsing() {
        assert_eq!(parse_tool_input(""), serde_json::json!({}));
        assert_eq!(parse_tool_input("{\"a\":1}"), serde_json::json!({"a": 1}));
        assert_eq!(parse_tool_input("{\"a\":"), serde_json::json!({"__crusty_invalid_json": "{\"a\":"}));
    }

    #[test]
    fn error_message_prefers_json_field() {
        assert_eq!(error_message("{\"error\":{\"message\":\"nope\"}}"), "nope");
        assert_eq!(error_message("plain"), "plain");
        assert_eq!(error_message(&"x".repeat(900)).chars().count(), ERROR_TEXT_LIMIT);
    }

    #[test]
    fn base_url_trims_slashes_and_rejects_empty() {
        let cfg = ProviderConfig { base_url: "https://h.test/api//".into(), ..Default::default() };
        assert_eq!(resolve_base(&CallOptions::default(), &cfg).ok().as_deref(), Some("https://h.test/api"));
        let empty = ProviderConfig::default();
        assert!(matches!(resolve_base(&CallOptions::default(), &empty), Err(ProviderError::Config(_))));
    }
}
