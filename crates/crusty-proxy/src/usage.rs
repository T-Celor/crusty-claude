//! Reads the token usage of a proxied answer while its bytes stream through.

use crusty_core::Usage;
use serde_json::Value;

/// Longest SSE line parsed for usage. Longer lines are content, not usage, and are skipped.
const MAX_SSE_LINE: usize = 256 * 1024;
/// Largest JSON answer buffered to find its `usage` field.
const MAX_JSON_BODY: usize = 2 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Sse,
    Json,
    Off,
}

/// Taps the chunks of one upstream answer and extracts its usage. It never changes the bytes.
#[derive(Debug)]
pub struct UsageWatcher {
    mode: Mode,
    /// SSE: the current partial line. JSON: the whole body (up to `MAX_JSON_BODY`).
    buf: Vec<u8>,
    /// The current line (SSE) or body (JSON) is over its limit and is being dropped.
    overflow: bool,
    /// SSE: usage from the first `message_start`, output tokens updated by `message_delta`.
    usage: Option<Usage>,
}

impl UsageWatcher {
    /// Chooses the parser from the answer's content type. Other types are not parsed.
    pub fn new(content_type: Option<&str>) -> Self {
        let media = content_type.unwrap_or("").split(';').next().unwrap_or("").trim().to_ascii_lowercase();
        let mode = match media.as_str() {
            "text/event-stream" => Mode::Sse,
            "application/json" => Mode::Json,
            _ => Mode::Off,
        };
        UsageWatcher { mode, buf: Vec::new(), overflow: false, usage: None }
    }

    /// Feeds the next chunk of the answer, in order.
    pub fn feed(&mut self, chunk: &[u8]) {
        match self.mode {
            Mode::Sse => self.feed_sse(chunk),
            Mode::Json => self.feed_json(chunk),
            Mode::Off => {}
        }
    }

    /// Ends the answer and returns its usage, if it carried any.
    pub fn finish(mut self) -> Option<Usage> {
        match self.mode {
            Mode::Sse => {
                if !self.overflow && !self.buf.is_empty() {
                    let line = std::mem::take(&mut self.buf);
                    self.handle_sse_line(&line);
                }
                self.usage
            }
            Mode::Json => {
                if self.overflow {
                    return None;
                }
                let body: Value = serde_json::from_slice(&self.buf).ok()?;
                serde_json::from_value(body.get("usage")?.clone()).ok()
            }
            Mode::Off => None,
        }
    }

    fn feed_sse(&mut self, chunk: &[u8]) {
        for piece in chunk.split_inclusive(|b| *b == b'\n') {
            let ends_line = piece.last() == Some(&b'\n');
            let text = if ends_line { &piece[..piece.len() - 1] } else { piece };
            if !self.overflow {
                if self.buf.len() + text.len() > MAX_SSE_LINE {
                    self.overflow = true;
                    self.buf.clear();
                } else {
                    self.buf.extend_from_slice(text);
                }
            }
            if ends_line {
                if !self.overflow {
                    let line = std::mem::take(&mut self.buf);
                    self.handle_sse_line(&line);
                }
                self.buf.clear();
                self.overflow = false;
            }
        }
    }

    fn handle_sse_line(&mut self, line: &[u8]) {
        let line = if line.last() == Some(&b'\r') { &line[..line.len() - 1] } else { line };
        let Some(data) = line.strip_prefix(b"data:") else {
            return;
        };
        let data = data.strip_prefix(b" ").unwrap_or(data);
        if contains(data, b"\"message_start\"") {
            if self.usage.is_none() {
                let start = serde_json::from_slice::<Value>(data).ok();
                self.usage = start
                    .as_ref()
                    .and_then(|v| v.get("message"))
                    .and_then(|m| m.get("usage"))
                    .and_then(|u| serde_json::from_value::<Usage>(u.clone()).ok());
            }
        } else if contains(data, b"\"message_delta\"") {
            let output = serde_json::from_slice::<Value>(data)
                .ok()
                .and_then(|v| v.get("usage").and_then(|u| u.get("output_tokens")).and_then(Value::as_u64));
            if let (Some(usage), Some(output)) = (self.usage.as_mut(), output) {
                usage.output_tokens = output;
            }
        }
    }

    fn feed_json(&mut self, chunk: &[u8]) {
        if self.overflow {
            return;
        }
        if self.buf.len() + chunk.len() > MAX_JSON_BODY {
            self.overflow = true;
            self.buf = Vec::new();
        } else {
            self.buf.extend_from_slice(chunk);
        }
    }
}

/// Whether `needle` occurs in `hay`.
fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SSE: &str = "event: message_start\n\
data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":10,\"cache_read_input_tokens\":5,\"cache_creation_input_tokens\":2,\"output_tokens\":1}}}\n\
\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"delta\":{\"text\":\"hello\"}}\n\
\n\
event: message_delta\n\
data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":42}}\n\
\n";

    fn expected() -> Usage {
        Usage { input_tokens: 10, output_tokens: 42, cache_read_input_tokens: 5, cache_creation_input_tokens: 2 }
    }

    #[test]
    fn sse_split_at_every_byte_gives_same_usage() {
        let bytes = SSE.as_bytes();
        for cut in 1..bytes.len() {
            let mut w = UsageWatcher::new(Some("text/event-stream; charset=utf-8"));
            w.feed(&bytes[..cut]);
            w.feed(&bytes[cut..]);
            assert_eq!(w.finish(), Some(expected()), "cut at {cut}");
        }
    }

    #[test]
    fn sse_without_trailing_newline_still_counts() {
        let trimmed = SSE.trim_end();
        let mut w = UsageWatcher::new(Some("text/event-stream"));
        w.feed(trimmed.as_bytes());
        assert_eq!(w.finish(), Some(expected()));
    }

    #[test]
    fn sse_crlf_lines_are_parsed() {
        let crlf = SSE.replace('\n', "\r\n");
        let mut w = UsageWatcher::new(Some("text/event-stream"));
        w.feed(crlf.as_bytes());
        assert_eq!(w.finish(), Some(expected()));
    }

    #[test]
    fn sse_overlong_line_is_skipped() {
        let mut w = UsageWatcher::new(Some("text/event-stream"));
        let padding = "x".repeat(MAX_SSE_LINE + 10);
        w.feed(format!("data: {{\"type\":\"message_start\",\"pad\":\"{padding}\"}}\n").as_bytes());
        w.feed(b"data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":1}}}\n");
        // The overlong line was dropped, so the short one that follows is the first usage seen.
        assert_eq!(w.finish().map(|u| u.input_tokens), Some(1));
    }

    #[test]
    fn sse_without_message_start_has_no_usage() {
        let mut w = UsageWatcher::new(Some("text/event-stream"));
        w.feed(b"data: {\"type\":\"message_stop\"}\n\n");
        assert_eq!(w.finish(), None);
    }

    #[test]
    fn json_body_usage_is_read() {
        let mut w = UsageWatcher::new(Some("application/json"));
        let body = br#"{"id":"m","content":[],"usage":{"input_tokens":3,"output_tokens":7}}"#;
        w.feed(&body[..10]);
        w.feed(&body[10..]);
        assert_eq!(w.finish(), Some(Usage { input_tokens: 3, output_tokens: 7, ..Usage::default() }));
    }

    #[test]
    fn json_over_limit_gives_no_usage() {
        let mut w = UsageWatcher::new(Some("application/json"));
        let big = vec![b' '; MAX_JSON_BODY + 1];
        w.feed(&big);
        assert_eq!(w.finish(), None);
    }

    #[test]
    fn other_content_types_are_not_parsed() {
        let mut w = UsageWatcher::new(Some("text/plain"));
        w.feed(SSE.as_bytes());
        assert_eq!(w.finish(), None);
        assert_eq!(UsageWatcher::new(None).finish(), None);
    }
}
