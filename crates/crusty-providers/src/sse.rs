//! Incremental Server-Sent Events parser over raw bytes.
//!
//! Bytes are buffered until a newline, so chunk boundaries may fall anywhere, including
//! inside a line or a multi-byte UTF-8 sequence.

/// One dispatched event. `event` is `None` when the stream named no event type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
}

#[derive(Debug, Default)]
pub struct SseParser {
    buf: Vec<u8>,
    event: Option<String>,
    data: Vec<String>,
}

impl SseParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feeds raw bytes and returns the events completed by them.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<SseEvent> {
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        let mut start = 0;
        while let Some(pos) = self.buf[start..].iter().position(|&b| b == b'\n') {
            let end = start + pos;
            let line = String::from_utf8_lossy(trim_cr(&self.buf[start..end])).into_owned();
            start = end + 1;
            self.line(&line, &mut out);
        }
        self.buf.drain(..start);
        out
    }

    /// Ends the stream. A final event without a terminating blank line is still dispatched.
    pub fn finish(&mut self) -> Vec<SseEvent> {
        let mut out = Vec::new();
        if !self.buf.is_empty() {
            let rest = std::mem::take(&mut self.buf);
            let line = String::from_utf8_lossy(trim_cr(&rest)).into_owned();
            self.line(&line, &mut out);
        }
        out.extend(self.dispatch());
        out
    }

    fn line(&mut self, line: &str, out: &mut Vec<SseEvent>) {
        if line.is_empty() {
            out.extend(self.dispatch());
            return;
        }
        if line.starts_with(':') {
            return;
        }
        let (field, value) = match line.split_once(':') {
            Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
            None => (line, ""),
        };
        match field {
            "data" => self.data.push(value.to_owned()),
            "event" => self.event = Some(value.to_owned()),
            _ => {}
        }
    }

    fn dispatch(&mut self) -> Option<SseEvent> {
        let event = self.event.take();
        if self.data.is_empty() {
            return None;
        }
        let data = self.data.join("\n");
        self.data.clear();
        Some(SseEvent { event, data })
    }
}

fn trim_cr(line: &[u8]) -> &[u8] {
    match line.last() {
        Some(b'\r') => &line[..line.len() - 1],
        _ => line,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(event: Option<&str>, data: &str) -> SseEvent {
        SseEvent { event: event.map(str::to_owned), data: data.to_owned() }
    }

    #[test]
    fn parses_events_split_at_every_byte() {
        let input = b"event: message_start\ndata: {\"a\":1}\n\nevent: ping\ndata: x\n\n";
        for split in 0..input.len() {
            let mut p = SseParser::new();
            let mut events = p.push(&input[..split]);
            events.extend(p.push(&input[split..]));
            events.extend(p.finish());
            assert_eq!(events, vec![ev(Some("message_start"), "{\"a\":1}"), ev(Some("ping"), "x")], "split at {split}");
        }
    }

    #[test]
    fn handles_crlf_comments_and_optional_space() {
        let mut p = SseParser::new();
        let events = p.push(b": keep-alive\r\ndata:no-space\r\n\r\ndata: yes\r\n\r\n");
        assert_eq!(events, vec![ev(None, "no-space"), ev(None, "yes")]);
    }

    #[test]
    fn joins_multi_line_data() {
        let mut p = SseParser::new();
        let events = p.push(b"data: first\ndata: second\n\n");
        assert_eq!(events, vec![ev(None, "first\nsecond")]);
    }

    #[test]
    fn utf8_split_inside_a_character() {
        let bytes = "data: héllo\n\n".as_bytes();
        let cut = bytes.iter().position(|&b| b == 0xC3).map_or(0, |i| i + 1);
        let mut p = SseParser::new();
        let mut events = p.push(&bytes[..cut]);
        events.extend(p.push(&bytes[cut..]));
        assert_eq!(events, vec![ev(None, "héllo")]);
    }

    #[test]
    fn blank_line_without_data_dispatches_nothing() {
        let mut p = SseParser::new();
        assert!(p.push(b"event: orphan\n\n").is_empty());
        assert_eq!(p.push(b"data: next\n\n"), vec![ev(None, "next")]);
    }

    #[test]
    fn finish_dispatches_unterminated_event() {
        let mut p = SseParser::new();
        assert!(p.push(b"data: tail").is_empty());
        assert_eq!(p.finish(), vec![ev(None, "tail")]);
    }
}
