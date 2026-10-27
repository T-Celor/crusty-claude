//! Finds tool results that later results have made obsolete, so the context can drop them.

use std::collections::{BTreeSet, HashMap};

use crusty_core::message::blocks_text;
use crusty_core::{ContentBlock, Message};
use serde_json::Value;

/// Tools whose result describes a path's content or listing.
const READ_TOOLS: &[&str] = &["Read", "Grep", "Glob", "LS"];
/// Tools that change a file.
const EDIT_TOOLS: &[&str] = &["Edit", "Write", "MultiEdit"];

/// One tool result of the conversation, with the tool that produced it.
struct ToolResultRef {
    message: usize,
    block: usize,
    name: String,
    key: Option<String>,
    text: String,
}

/// The path a tool input refers to (`file_path`, else `path`).
fn path_of(input: &Value) -> Option<String> {
    input.get("file_path").or_else(|| input.get("path")).and_then(Value::as_str).map(str::to_owned)
}

/// Tool results superseded by a later message, as `(message index, block index)`, sorted.
///
/// A result is superseded when:
/// - it comes from `Read`, `Grep`, `Glob` or `LS` on a path that a later one of those reads again;
/// - it is a `Read` of a file that a later `Edit`, `Write` or `MultiEdit` changed;
/// - its non-empty text is identical to a later tool result.
///
/// The last `keep_recent` messages are never reported. Superseding results may sit anywhere
/// after the stale one, including those protected messages.
pub fn stale_tool_results(messages: &[Message], keep_recent: usize) -> Vec<(usize, usize)> {
    // Pass 1: tool_use id to (name, path), and every edit with its message index.
    let mut uses: HashMap<String, (String, Option<String>)> = HashMap::new();
    let mut edits: Vec<(usize, String)> = Vec::new();
    for (mi, m) in messages.iter().enumerate() {
        for block in &m.content {
            if let ContentBlock::ToolUse { id, name, input, .. } = block {
                let path = path_of(input);
                if let (true, Some(p)) = (EDIT_TOOLS.contains(&name.as_str()), &path) {
                    edits.push((mi, p.clone()));
                }
                uses.insert(id.clone(), (name.clone(), path));
            }
        }
    }

    // Pass 2: every tool result with what produced it.
    let mut results: Vec<ToolResultRef> = Vec::new();
    for (mi, m) in messages.iter().enumerate() {
        for (bi, block) in m.content.iter().enumerate() {
            if let ContentBlock::ToolResult { tool_use_id, content, .. } = block {
                let (name, key) = uses.get(tool_use_id).cloned().unwrap_or_default();
                results.push(ToolResultRef { message: mi, block: bi, name, key, text: blocks_text(content) });
            }
        }
    }

    let protected_from = messages.len().saturating_sub(keep_recent);
    let mut stale = BTreeSet::new();
    for r in &results {
        if r.message >= protected_from {
            continue;
        }
        let read_like = READ_TOOLS.contains(&r.name.as_str());
        let reread = read_like
            && r.key.is_some()
            && results.iter().any(|o| o.message > r.message && READ_TOOLS.contains(&o.name.as_str()) && o.key == r.key);
        let edited = r.name == "Read" && r.key.as_ref().is_some_and(|k| edits.iter().any(|(emi, p)| *emi > r.message && p == k));
        let duplicate = !r.text.is_empty() && results.iter().any(|o| o.message > r.message && o.text == r.text);
        if reread || edited || duplicate {
            stale.insert((r.message, r.block));
        }
    }
    stale.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crusty_core::Role;
    use serde_json::json;

    fn call(id: &str, name: &str, input: Value) -> Message {
        Message { role: Role::Assistant, content: vec![ContentBlock::tool_use(id, name, input)] }
    }

    fn reply(id: &str, text: &str) -> Message {
        Message { role: Role::User, content: vec![ContentBlock::tool_result(id, vec![ContentBlock::text(text)], false)] }
    }

    #[test]
    fn superseded_reads_edits_and_duplicates() {
        let messages = vec![
            call("r1", "Read", json!({"file_path": "a.rs"})), // 0
            reply("r1", "alpha"),                             // 1: a.rs re-read at 7
            call("r2", "Read", json!({"file_path": "b.rs"})), // 2
            reply("r2", "beta"),                              // 3: b.rs edited at 4
            call("e1", "Edit", json!({"file_path": "b.rs"})), // 4
            reply("e1", "edited"),                            // 5
            call("r3", "Read", json!({"file_path": "a.rs"})), // 6
            reply("r3", "alpha"),                             // 7: a.rs re-read at 9
            call("r4", "Read", json!({"file_path": "a.rs"})), // 8
            reply("r4", "delta"),                             // 9
        ];
        assert_eq!(stale_tool_results(&messages, 0), vec![(1, 0), (3, 0), (7, 0)]);
        // The last two messages are protected: (7,0) is still a candidate, but (9,0) never is.
        assert_eq!(stale_tool_results(&messages, 2), vec![(1, 0), (3, 0), (7, 0)]);
        assert_eq!(stale_tool_results(&messages, 3), vec![(1, 0), (3, 0)]);
    }

    #[test]
    fn keep_recent_protects_the_tail() {
        let messages = vec![
            call("r1", "Read", json!({"file_path": "x"})),
            reply("r1", "a"),
            call("r2", "Read", json!({"file_path": "x"})),
            reply("r2", "b"),
        ];
        assert_eq!(stale_tool_results(&messages, 1), vec![(1, 0)]);
        assert_eq!(stale_tool_results(&messages, 2), vec![(1, 0)]);
        assert!(stale_tool_results(&messages, 3).is_empty());
        assert!(stale_tool_results(&messages, 100).is_empty());
    }

    #[test]
    fn duplicate_results_from_any_tool() {
        let messages = vec![
            call("b1", "Bash", json!({"command": "cargo test"})),
            reply("b1", "same output"),
            call("b2", "Bash", json!({"command": "cargo test"})),
            reply("b2", "same output"),
            call("b3", "Bash", json!({"command": "ls"})),
            reply("b3", "different"),
        ];
        assert_eq!(stale_tool_results(&messages, 0), vec![(1, 0)]);
    }

    #[test]
    fn empty_duplicates_and_writes_are_not_reported() {
        let messages = vec![
            call("e1", "Bash", json!({"command": "true"})),
            reply("e1", ""),
            call("e2", "Bash", json!({"command": "true"})),
            reply("e2", ""),
            call("w1", "Write", json!({"file_path": "a.rs", "content": "x"})),
            reply("w1", "written"),
        ];
        assert!(stale_tool_results(&messages, 0).is_empty());
    }

    #[test]
    fn grep_result_superseded_by_later_read_of_same_path() {
        let messages = vec![
            call("g1", "Grep", json!({"pattern": "fn", "path": "src/lib.rs"})),
            reply("g1", "12: fn a"),
            call("r1", "Read", json!({"file_path": "src/lib.rs"})),
            reply("r1", "full file"),
        ];
        assert_eq!(stale_tool_results(&messages, 0), vec![(1, 0)]);
    }

    #[test]
    fn unknown_ids_are_ignored() {
        let messages = vec![reply("ghost", "orphan"), reply("ghost2", "orphan")];
        assert_eq!(stale_tool_results(&messages, 0), vec![(0, 0)]);
    }
}
