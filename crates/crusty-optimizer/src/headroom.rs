//! Native port of the Headroom idea: compresses old tool output in a request, cache-aware.
//!
//! The compressed region `messages[0..frozen]` only grows in batches, and its output is a
//! pure function of the history. A provider prompt cache therefore keeps hitting between
//! turns, and the compressed prefix changes only when a batch of new tokens has accumulated.

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::sync::{LazyLock, Mutex, MutexGuard};

use crusty_core::config::HeadroomConfig;
use crusty_core::message::{ChatRequest, ContentBlock, Message, Role, blocks_text};
use crusty_core::store::OriginalStore;
use crusty_core::tokens::{estimate_request, estimate_text};
use regex::Regex;
use serde_json::Value;
use sha2::{Digest, Sha256};

/// Sessions whose frozen boundary is remembered; the oldest inserted is forgotten first.
const MAX_SESSIONS: usize = 2000;
/// Strings longer than this (in characters) keep their head and tail.
const LONG_STRING: usize = 1000;
const STRING_HEAD: usize = 400;
const STRING_TAIL: usize = 200;
/// Failing items kept from the middle of a JSON array; the rest are counted in the omission note.
const MAX_ANOMALIES: usize = 50;
/// How deep `is_anomalous` looks into nested objects and arrays.
const MAX_ANOMALY_DEPTH: usize = 4;

// Literal patterns: a compile failure is a bug caught by the tests.
static ANOMALY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)error|fail|exception|warn|denied|invalid").expect("valid anomaly pattern"));
static IMPORTANT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)error|warning|fail|panic|exception|traceback").expect("valid important pattern"));

/// What one `Headroom::compress` call did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HeadroomReport {
    pub tokens_before: u64,
    pub tokens_after: u64,
    pub blocks_compressed: usize,
    pub duplicates_removed: usize,
    /// Transform applied to each changed block, e.g. `headroom:json-crush`.
    pub transforms: Vec<String>,
    /// Messages `[0..frozen_boundary]` were compressed; the rest were left alone.
    pub frozen_boundary: usize,
}

#[derive(Debug, Default)]
struct Boundaries {
    frozen: HashMap<String, usize>,
    order: VecDeque<String>,
}

impl Boundaries {
    fn remember(&mut self, session: &str, frozen: usize) {
        if let Some(slot) = self.frozen.get_mut(session) {
            *slot = frozen;
            return;
        }
        self.frozen.insert(session.to_owned(), frozen);
        self.order.push_back(session.to_owned());
        while self.order.len() > MAX_SESSIONS {
            if let Some(old) = self.order.pop_front() {
                self.frozen.remove(&old);
            }
        }
    }
}

/// Context compressor. Keeps one frozen boundary per session.
#[derive(Debug)]
pub struct Headroom {
    cfg: HeadroomConfig,
    boundaries: Mutex<Boundaries>,
}

impl Headroom {
    pub fn new(cfg: &HeadroomConfig) -> Self {
        Headroom { cfg: cfg.clone(), boundaries: Mutex::new(Boundaries::default()) }
    }

    /// Compresses the request in place. Originals of changed blocks go to `store`.
    ///
    /// Blocks in the recent `protect_recent_messages` messages, system, tools, tool uses,
    /// thinking, assistant text and images are never touched.
    pub fn compress(&self, req: &mut ChatRequest, session: &str, store: &OriginalStore) -> HeadroomReport {
        let tokens_before = estimate_request(req, false);
        let candidate = req.messages.len().saturating_sub(self.cfg.protect_recent_messages);
        let frozen = self.advance(session, &req.messages, candidate);
        let mut report = HeadroomReport { tokens_before, frozen_boundary: frozen, ..Default::default() };

        // Duplicates are matched only against results inside the frozen region, so a block
        // compressed once never changes form when new results are appended after it.
        let last_positions = last_tool_result_positions(&req.messages[..frozen]);
        for mi in 0..frozen {
            let role = req.messages[mi].role;
            for (bi, block) in req.messages[mi].content.iter_mut().enumerate() {
                match block {
                    ContentBlock::ToolResult { content, .. } => {
                        let Some(text) = text_only(content) else { continue };
                        let duplicate = last_positions.get(&digest(&text)).is_some_and(|last| *last > (mi, bi));
                        if let Some(new) = self.rewrite(&text, duplicate, store, &mut report) {
                            *content = vec![ContentBlock::text(new)];
                        }
                    }
                    ContentBlock::Text { text, .. } if role == Role::User => {
                        if let Some(new) = self.rewrite(text, false, store, &mut report) {
                            *text = new;
                        }
                    }
                    _ => {}
                }
            }
        }
        report.tokens_after = estimate_request(req, false);
        report
    }

    /// Moves the session's boundary to `candidate` only when the tokens between the old
    /// boundary and `candidate` reach `batch_tokens`. Returns the boundary to use.
    fn advance(&self, session: &str, messages: &[Message], candidate: usize) -> usize {
        // A stored boundary past the candidate (history shrank, e.g. after compaction) is reset
        // to it, so the protected recent tail is never compressed.
        let candidate = candidate.min(messages.len());
        let mut state = self.lock();
        let current = state.frozen.get(session).copied().unwrap_or(0).min(candidate);
        let frozen = if candidate > current && eligible_tokens(&messages[current..candidate]) >= self.cfg.batch_tokens {
            candidate
        } else {
            current
        };
        let frozen = frozen.min(candidate);
        state.remember(session, frozen);
        frozen
    }

    /// The compressed text of one eligible block, or `None` when it stays as it is.
    fn rewrite(&self, original: &str, duplicate: bool, store: &OriginalStore, report: &mut HeadroomReport) -> Option<String> {
        if estimate_text(original) < self.cfg.min_block_tokens {
            return None;
        }
        // `put` is idempotent by content hash, so re-putting the same original on later turns
        // adds nothing to the store.
        if duplicate {
            let handle = store.put(original);
            report.duplicates_removed += 1;
            report.transforms.push("headroom:duplicate".to_owned());
            return Some(format!("[headroom: duplicate of a later tool result; original: Retrieve(handle=\"{handle}\")]"));
        }
        let (kind, compact) = match serde_json::from_str::<Value>(original.trim()) {
            Ok(value) => {
                let crushed = json_crush(value, self.cfg.json_keep_items);
                ("headroom:json-crush", serde_json::to_string(&crushed).ok()?)
            }
            Err(_) => ("headroom:log-crush", self.log_crush(original)),
        };
        let before = estimate_text(original);
        let after = estimate_text(&compact);
        if after * 10 > before * 9 {
            return None;
        }
        let handle = store.put(original);
        report.blocks_compressed += 1;
        report.transforms.push(kind.to_owned());
        Some(format!("{compact}\n[headroom: compressed {before}→{after} est. tokens; original: Retrieve(handle=\"{handle}\")]"))
    }

    /// Collapses identical consecutive lines and keeps the head, the tail and every
    /// important line in between.
    fn log_crush(&self, text: &str) -> String {
        let mut runs: Vec<(&str, usize)> = Vec::new();
        for line in text.lines() {
            match runs.last_mut() {
                Some((prev, n)) if *prev == line => *n += 1,
                _ => runs.push((line, 1)),
            }
        }
        let total = runs.len();
        let head = self.cfg.keep_head_lines.min(total);
        let tail_start = total.saturating_sub(self.cfg.keep_tail_lines).max(head);
        let mut out = Vec::new();
        let mut omitted = 0usize;
        for run in &runs[..head] {
            out.push(render_run(*run));
        }
        for run in &runs[head..tail_start] {
            if IMPORTANT.is_match(run.0) {
                flush_omitted(&mut out, &mut omitted);
                out.push(render_run(*run));
            } else {
                omitted += run.1;
            }
        }
        flush_omitted(&mut out, &mut omitted);
        for run in &runs[tail_start..] {
            out.push(render_run(*run));
        }
        out.join("\n")
    }

    fn lock(&self) -> MutexGuard<'_, Boundaries> {
        self.boundaries.lock().unwrap_or_else(|e| e.into_inner())
    }
}

fn render_run((line, count): (&str, usize)) -> String {
    if count > 1 { format!("{line}  (×{count})") } else { line.to_owned() }
}

fn flush_omitted(out: &mut Vec<String>, omitted: &mut usize) {
    if *omitted > 0 {
        out.push(format!("… {omitted} lines omitted …"));
        *omitted = 0;
    }
}

/// Text of a block whose content is only text blocks; `None` when it holds images or others.
fn text_only(content: &[ContentBlock]) -> Option<String> {
    content.iter().all(|b| matches!(b, ContentBlock::Text { .. })).then(|| blocks_text(content))
}

fn digest(text: &str) -> [u8; 32] {
    Sha256::digest(text.as_bytes()).into()
}

/// Position of the last text-only tool result for each content digest.
fn last_tool_result_positions(messages: &[Message]) -> HashMap<[u8; 32], (usize, usize)> {
    let mut last = HashMap::new();
    for (mi, message) in messages.iter().enumerate() {
        for (bi, block) in message.content.iter().enumerate() {
            if let ContentBlock::ToolResult { content, .. } = block
                && let Some(text) = text_only(content)
            {
                last.insert(digest(&text), (mi, bi));
            }
        }
    }
    last
}

/// Estimated tokens of the text that compression may rewrite.
fn eligible_tokens(messages: &[Message]) -> u64 {
    messages
        .iter()
        .map(|m| {
            m.content
                .iter()
                .map(|b| match b {
                    ContentBlock::ToolResult { content, .. } => text_only(content).map_or(0, |t| estimate_text(&t)),
                    ContentBlock::Text { text, .. } if m.role == Role::User => estimate_text(text),
                    _ => 0,
                })
                .sum::<u64>()
        })
        .sum()
}

fn json_crush(value: Value, keep: usize) -> Value {
    match value {
        Value::Array(items) => Value::Array(crush_array(items, keep)),
        Value::Object(map) => Value::Object(map.into_iter().map(|(k, v)| (k, json_crush(v, keep))).collect()),
        Value::String(s) => Value::String(crush_string(s)),
        other => other,
    }
}

/// Keeps `keep` items at each end and up to `MAX_ANOMALIES` suspicious items from the middle.
fn crush_array(items: Vec<Value>, keep: usize) -> Vec<Value> {
    let len = items.len();
    if len <= 2 * keep + 1 {
        return items.into_iter().map(|v| json_crush(v, keep)).collect();
    }
    let tail_start = len - keep;
    let mut out = Vec::new();
    let mut middle = Vec::new();
    let mut tail = Vec::new();
    for (i, item) in items.into_iter().enumerate() {
        if i < keep {
            out.push(json_crush(item, keep));
        } else if i >= tail_start {
            tail.push(json_crush(item, keep));
        } else {
            middle.push(item);
        }
    }
    let mut omitted = 0usize;
    let mut omitted_failing = 0usize;
    let mut keys = BTreeSet::new();
    let mut kept_failing = 0usize;
    for item in middle {
        let failing = is_anomalous(&item);
        if failing && kept_failing < MAX_ANOMALIES {
            kept_failing += 1;
            out.push(json_crush(item, keep));
            continue;
        }
        omitted += 1;
        if failing {
            omitted_failing += 1;
        }
        if let Value::Object(map) = &item {
            keys.extend(map.keys().cloned());
        }
    }
    if omitted > 0 {
        let failing_part = if omitted_failing > 0 { format!(", including {omitted_failing} with errors") } else { String::new() };
        let keys_part =
            if keys.is_empty() { String::new() } else { format!(" (keys: {})", keys.into_iter().collect::<Vec<_>>().join(", ")) };
        out.push(Value::String(format!("… {omitted} items omitted{failing_part}{keys_part} …")));
    }
    out.extend(tail);
    out
}

/// An item that reports a failure: a key or string value naming one, or a status/code that is
/// not a success.
fn is_anomalous(item: &Value) -> bool {
    is_anomalous_at(item, 0)
}

/// Recurses into nested objects and arrays up to `MAX_ANOMALY_DEPTH`.
fn is_anomalous_at(item: &Value, depth: usize) -> bool {
    if depth > MAX_ANOMALY_DEPTH {
        return false;
    }
    match item {
        Value::Object(map) => map.iter().any(|(k, v)| {
            ANOMALY.is_match(k)
                || v.as_str().is_some_and(|s| ANOMALY.is_match(s))
                || ((k == "status" || k == "code") && !is_success(v))
                || is_anomalous_at(v, depth + 1)
        }),
        Value::Array(items) => items.iter().any(|v| is_anomalous_at(v, depth + 1)),
        Value::String(s) => ANOMALY.is_match(s),
        _ => false,
    }
}

fn is_success(v: &Value) -> bool {
    match v {
        Value::Number(n) => n.as_u64() == Some(200),
        Value::String(s) => matches!(s.to_ascii_lowercase().as_str(), "ok" | "success" | "200"),
        _ => false,
    }
}

fn crush_string(s: String) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= LONG_STRING {
        return s;
    }
    let head: String = chars[..STRING_HEAD].iter().collect();
    let tail: String = chars[chars.len() - STRING_TAIL..].iter().collect();
    format!("{head}… {} chars omitted …{tail}", chars.len() - STRING_HEAD - STRING_TAIL)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crusty_core::message::Extra;
    use serde_json::json;

    fn cfg() -> HeadroomConfig {
        HeadroomConfig { protect_recent_messages: 0, min_block_tokens: 50, batch_tokens: 1, ..HeadroomConfig::default() }
    }

    fn big_log(seed: usize) -> String {
        (0..200).map(|i| format!("line {seed} {i} the quick brown fox jumps over the lazy dog\n")).collect()
    }

    fn result_msg(id: &str, text: &str) -> Message {
        Message { role: Role::User, content: vec![ContentBlock::tool_result(id, vec![ContentBlock::text(text)], false)] }
    }

    fn request(messages: Vec<Message>) -> ChatRequest {
        let mut req = ChatRequest::new("m", 100);
        req.messages = messages;
        req
    }

    fn conversation(n: usize) -> Vec<Message> {
        (0..n)
            .map(|i| if i % 2 == 0 { result_msg(&format!("t{i}"), &big_log(i)) } else { Message::assistant_text(format!("ok {i}")) })
            .collect()
    }

    fn handle_of(text: &str) -> String {
        text.split("Retrieve(handle=\"").nth(1).unwrap_or("").split('"').next().unwrap_or("").to_owned()
    }

    #[test]
    fn recent_messages_are_untouched() {
        let original = conversation(10);
        let mut req = request(original.clone());
        let h = Headroom::new(&HeadroomConfig { protect_recent_messages: 6, ..cfg() });
        let store = OriginalStore::default();
        let report = h.compress(&mut req, "s", &store);
        assert_eq!(report.frozen_boundary, 4);
        assert_eq!(req.messages[4..], original[4..]);
        assert_ne!(req.messages[0], original[0]);
        assert!(report.tokens_after < report.tokens_before);
    }

    #[test]
    fn boundary_waits_for_batch_tokens_then_advances() {
        let original = conversation(10);
        let store = OriginalStore::default();
        let waiting = Headroom::new(&HeadroomConfig { protect_recent_messages: 6, batch_tokens: 1_000_000, ..cfg() });
        let mut req = request(original.clone());
        assert_eq!(waiting.compress(&mut req, "s", &store).frozen_boundary, 0);
        assert_eq!(req.messages, original);

        let advancing = Headroom::new(&HeadroomConfig { protect_recent_messages: 6, batch_tokens: 100, ..cfg() });
        let mut req = request(original);
        assert_eq!(advancing.compress(&mut req, "s", &store).frozen_boundary, 4);
    }

    #[test]
    fn output_is_identical_for_the_same_history() {
        let h = Headroom::new(&HeadroomConfig { protect_recent_messages: 2, ..cfg() });
        let store = OriginalStore::default();
        let mut first = request(conversation(8));
        let mut second = request(conversation(8));
        let r1 = h.compress(&mut first, "s", &store);
        let r2 = h.compress(&mut second, "s", &store);
        assert_eq!(first, second);
        assert_eq!(r1.frozen_boundary, r2.frozen_boundary);
        assert_eq!(serde_json::to_string(&first).unwrap(), serde_json::to_string(&second).unwrap());
    }

    #[test]
    fn json_crush_keeps_ends_and_failing_items() {
        let items: Vec<Value> = (0..100)
            .map(|i| {
                if i == 50 {
                    json!({"id": i, "status": "failed", "payload": "p".repeat(40)})
                } else {
                    json!({"id": i, "status": "ok", "payload": "p".repeat(40)})
                }
            })
            .collect();
        let text = serde_json::to_string(&items).unwrap();
        let h = Headroom::new(&cfg());
        let store = OriginalStore::default();
        let mut req = request(vec![result_msg("t1", &text), Message::user_text("next question")]);
        let report = h.compress(&mut req, "s", &store);
        assert_eq!(report.transforms, vec!["headroom:json-crush".to_owned()]);
        let ContentBlock::ToolResult { content, .. } = &req.messages[0].content[0] else { panic!("tool result expected") };
        let body = content[0].as_text().unwrap().split("\n[headroom:").next().unwrap();
        let crushed: Value = serde_json::from_str(body).unwrap();
        let arr = crushed.as_array().unwrap();
        assert_eq!(arr[0]["id"], 0);
        assert_eq!(arr.last().unwrap()["id"], 99);
        assert!(arr.iter().any(|v| v["id"] == 50), "failing item kept");
        assert!(arr.iter().any(|v| v.as_str().is_some_and(|s| s.contains("items omitted"))));
    }

    #[test]
    fn long_json_strings_keep_head_and_tail() {
        let text = serde_json::to_string(&json!({"body": "a".repeat(3000)})).unwrap();
        let crushed = json_crush(serde_json::from_str(&text).unwrap(), 5);
        let s = crushed["body"].as_str().unwrap();
        assert!(s.starts_with(&"a".repeat(400)));
        assert!(s.ends_with(&"a".repeat(200)));
        assert!(s.contains("2400 chars omitted"));
    }

    #[test]
    fn log_crush_keeps_error_line_in_the_middle() {
        let mut text = String::new();
        for i in 0..300 {
            if i == 150 {
                text.push_str("ERROR: boom while linking\n");
            } else {
                text.push_str(&format!("ok line number {i} processed with plenty of filler text here\n"));
            }
        }
        let h = Headroom::new(&cfg());
        let store = OriginalStore::default();
        let mut req = request(vec![result_msg("t1", &text), Message::user_text("go on")]);
        let report = h.compress(&mut req, "s", &store);
        assert_eq!(report.transforms, vec!["headroom:log-crush".to_owned()]);
        let ContentBlock::ToolResult { content, .. } = &req.messages[0].content[0] else { panic!("tool result expected") };
        let body = content[0].as_text().unwrap();
        assert!(body.contains("ERROR: boom while linking"));
        assert!(body.contains("lines omitted"));
    }

    #[test]
    fn duplicate_tool_results_are_replaced_and_original_is_retrievable() {
        let text = big_log(7);
        let messages = vec![
            result_msg("a", &text),
            Message::assistant_text("reading again"),
            result_msg("b", &text),
            Message::user_text("summarize"),
            Message::assistant_text("done"),
            Message::user_text("thanks"),
        ];
        let h = Headroom::new(&HeadroomConfig { protect_recent_messages: 2, ..cfg() });
        let store = OriginalStore::default();
        let mut req = request(messages);
        let report = h.compress(&mut req, "s", &store);
        assert_eq!(report.duplicates_removed, 1);
        let ContentBlock::ToolResult { content, .. } = &req.messages[0].content[0] else { panic!("tool result expected") };
        let body = content[0].as_text().unwrap();
        assert!(body.starts_with("[headroom: duplicate of a later tool result"));
        let handle = handle_of(body);
        assert_eq!(store.get(&handle).unwrap().as_str(), text);
        // The later occurrence is the reference, so it is compressed on its own, not replaced.
        let ContentBlock::ToolResult { content: later, .. } = &req.messages[2].content[0] else { panic!("tool result expected") };
        assert!(later[0].as_text().unwrap().contains("lines omitted"));
    }

    #[test]
    fn appended_results_do_not_change_frozen_blocks() {
        let text = big_log(9);
        let base = vec![
            result_msg("a", &text),
            Message::assistant_text("r"),
            Message::user_text("q"),
            Message::assistant_text("s"),
            Message::user_text("last"),
        ];
        let h = Headroom::new(&HeadroomConfig { protect_recent_messages: 2, ..cfg() });
        let store = OriginalStore::default();
        let mut first = request(base.clone());
        assert_eq!(h.compress(&mut first, "s", &store).frozen_boundary, 3);
        let mut appended = base;
        appended.push(result_msg("c", &text));
        let mut second = request(appended);
        assert_eq!(h.compress(&mut second, "s", &store).frozen_boundary, 3);
        assert_eq!(first.messages[..3], second.messages[..3]);
    }

    #[test]
    fn stored_boundary_is_clamped_to_the_protected_tail() {
        let h = Headroom::new(&cfg());
        assert_eq!(h.advance("s", &conversation(10), 10), 10);
        // History shrank: the stored boundary must follow the new, smaller candidate.
        assert_eq!(h.advance("s", &conversation(4), 4), 4);
        assert_eq!(h.advance("s", &conversation(4), 2), 2);
    }

    #[test]
    fn json_crush_keeps_every_failure_up_to_fifty_and_counts_the_rest() {
        let items: Vec<Value> = (0..200)
            .map(|i| match i {
                7 => json!({"id": i, "meta": {"deep": [{"x": "exception"}]}}),
                _ if i % 2 == 0 => json!({"id": i, "status": "failed"}),
                _ => json!({"id": i, "status": "ok"}),
            })
            .collect();
        let crushed = json_crush(Value::Array(items), 5);
        let arr = crushed.as_array().unwrap();
        assert_eq!(arr.len(), 5 + 50 + 1 + 5);
        assert!(arr.iter().any(|v| v["id"] == 7), "nested failure detected");
        assert!(arr.iter().any(|v| v.as_str().is_some_and(|s| s.contains("140 items omitted, including 46 with errors"))));
    }

    #[test]
    fn originals_of_compressed_blocks_are_retrievable() {
        let text = big_log(3);
        let h = Headroom::new(&cfg());
        let store = OriginalStore::default();
        let mut req = request(vec![result_msg("t1", &text), Message::user_text("next")]);
        h.compress(&mut req, "s", &store);
        let ContentBlock::ToolResult { content, .. } = &req.messages[0].content[0] else { panic!("tool result expected") };
        let handle = handle_of(content[0].as_text().unwrap());
        assert_eq!(store.get(&handle).unwrap().as_str(), text);
    }

    #[test]
    fn tool_use_thinking_and_images_are_never_touched() {
        let big = "reasoning ".repeat(500);
        let input = json!({"command": "x".repeat(4000)});
        let messages = vec![
            Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::Thinking { thinking: big.clone(), signature: None, extra: Extra::new() },
                    ContentBlock::tool_use("t9", "Bash", input.clone()),
                    ContentBlock::text(big.clone()),
                    ContentBlock::image_png_base64("aGVsbG8="),
                ],
            },
            Message::user_text("next"),
        ];
        let original = messages.clone();
        let h = Headroom::new(&cfg());
        let store = OriginalStore::default();
        let mut req = request(messages);
        let report = h.compress(&mut req, "s", &store);
        assert_eq!(report.blocks_compressed, 0);
        assert_eq!(req.messages[0], original[0]);
    }

    #[test]
    fn user_text_in_frozen_region_is_compressed() {
        let text = big_log(5);
        let h = Headroom::new(&cfg());
        let store = OriginalStore::default();
        let mut req = request(vec![Message::user_text(text.clone()), Message::assistant_text("ok")]);
        let report = h.compress(&mut req, "s", &store);
        assert_eq!(report.blocks_compressed, 1);
        assert!(req.messages[0].text().contains("lines omitted"));
    }

    #[test]
    fn boundaries_are_bounded() {
        let h = Headroom::new(&cfg());
        for i in 0..(MAX_SESSIONS + 1) {
            h.lock().remember(&format!("s{i}"), 1);
        }
        let state = h.lock();
        assert_eq!(state.order.len(), MAX_SESSIONS);
        assert!(!state.frozen.contains_key("s0"));
    }
}
