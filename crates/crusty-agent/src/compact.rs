//! Conversation compaction: older messages are summarised by the agent's own model and
//! replaced by the summary, keeping the recent turns verbatim.

use std::fmt::Write as _;

use crusty_core::hooks::HookEvent;
use crusty_core::{CallOptions, CancellationToken, ChatRequest, ContentBlock, Message, Role};
use crusty_session::transcript::Entry;
use serde_json::json;

use crate::engine::AgentSpec;
use crate::runtime::Runtime;

#[derive(Debug, Clone, PartialEq)]
pub struct CompactReport {
    pub replaced: usize,
    pub summary_chars: usize,
    /// Spend of the summarising call, counted in the agent's budget.
    pub usage: crusty_core::Usage,
    pub cost_usd: f64,
}

const SUMMARY_PROMPT: &str = "\
Summarise the conversation below so another agent can continue the work without it. Keep: the user's goals and constraints, decisions taken and why, files read or changed (paths), commands run and their results, open problems and the next steps. Drop pleasantries and repeated tool output. Write plain prose and bullet points, no preamble.";

/// Index where the kept tail starts: a user message that is not only tool results, so no
/// tool_use is separated from its tool_result. `None` when nothing can be dropped.
pub fn split_point(messages: &[Message], keep: usize) -> Option<usize> {
    let start = messages.len().checked_sub(keep.max(1))?;
    (1..=start).rev().find(|&i| {
        let m = &messages[i];
        m.role == Role::User && !m.content.iter().all(|b| matches!(b, ContentBlock::ToolResult { .. }))
    })
}

fn render(messages: &[Message]) -> String {
    let mut out = String::new();
    for m in messages {
        let role = match m.role {
            Role::User => "User",
            Role::Assistant => "Assistant",
            Role::System => "System",
        };
        let _ = writeln!(out, "## {role}");
        for b in &m.content {
            match b {
                ContentBlock::Text { text, .. } => {
                    let _ = writeln!(out, "{text}");
                }
                ContentBlock::ToolUse { name, input, .. } => {
                    let input: String = input.to_string().chars().take(300).collect();
                    let _ = writeln!(out, "[tool call {name}: {input}]");
                }
                ContentBlock::ToolResult { content, is_error, .. } => {
                    let text: String = crusty_core::message::blocks_text(content).chars().take(2000).collect();
                    let _ = writeln!(out, "[tool result{}]\n{text}", if *is_error { " (error)" } else { "" });
                }
                _ => {}
            }
        }
    }
    out
}

pub async fn compact(
    rt: &Runtime,
    spec: &AgentSpec,
    messages: &mut Vec<Message>,
    instructions: Option<&str>,
    cancel: &CancellationToken,
) -> Result<CompactReport, String> {
    let keep = rt.config.session.keep_recent_on_compact;
    let split = split_point(messages, keep).ok_or("not enough history to compact")?;
    let hook = rt.hooks.run(HookEvent::PreCompact, None, &json!({"agent_id": spec.id, "messages": messages.len()})).await;
    if hook.block {
        return Err(format!("blocked by PreCompact hook: {}", hook.message.unwrap_or_default()));
    }
    let provider = rt.provider(&spec.profile.provider).ok_or("provider not configured")?;
    let mut prompt = format!("{SUMMARY_PROMPT}\n");
    if let Some(extra) = instructions.filter(|s| !s.trim().is_empty()) {
        let _ = write!(prompt, "\nAlso: {extra}\n");
    }
    let _ = write!(prompt, "\n<conversation>\n{}</conversation>", render(&messages[..split]));
    let mut req = ChatRequest::new(&spec.profile.model, 8_000);
    req.messages.push(Message::user_text(prompt));
    // Nothing leaves the process unmasked; the summary keeps the placeholders. A no-op when DLP is off.
    crusty_dlp::redact_request(&mut req, &rt.vault, rt.advisor.as_ref(), &rt.config.laya.dlp).await;
    let opts =
        CallOptions { base_url_override: rt.provider_route(&spec.profile.provider), cancel: cancel.clone(), extra_headers: Vec::new() };
    let resp = provider.stream(&req, &opts, &|_| {}).await.map_err(|e| e.to_string())?;
    let summary = resp.text();
    if summary.trim().is_empty() {
        return Err("the model returned an empty summary".into());
    }
    let cost_usd = rt.config.cost(&spec.profile.model, &resp.usage);
    rt.events.emit(crusty_core::AppEvent::Usage {
        agent_id: spec.id.clone(),
        provider: spec.profile.provider.clone(),
        model: spec.profile.model.clone(),
        usage: resp.usage,
        cost_usd,
    });
    let mut tail = messages.split_off(split);
    let kept = tail.len();
    let header = ContentBlock::text(format!("[Conversation summary — earlier messages were compacted]\n{summary}"));
    // The tail starts with a user message: the summary joins it, so roles keep alternating.
    tail[0].content.insert(0, header);
    *messages = tail;
    if spec.depth == 0 {
        rt.log(&Entry::Compaction { summary: summary.clone(), replaced: split, kept, ts: chrono::Utc::now() });
        // `/rewind <id> conversation` to a checkpoint from before this point would cut the wrong messages.
        rt.mark_compaction(rt.checkpoints.list().iter().map(|c| c.id).max().unwrap_or(0));
    }
    Ok(CompactReport { replaced: split, summary_chars: summary.len(), usage: resp.usage, cost_usd })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool_result() -> Message {
        Message { role: Role::User, content: vec![ContentBlock::tool_result("t", vec![ContentBlock::text("ok")], false)] }
    }

    #[test]
    fn split_never_separates_tool_pairs() {
        let msgs = vec![
            Message::user_text("a"),
            Message::assistant_text("b"),
            Message::user_text("c"),
            Message::assistant_text("d"),
            tool_result(),
            Message::assistant_text("e"),
        ];
        assert_eq!(split_point(&msgs, 2), Some(2));
        assert_eq!(split_point(&msgs[..2], 2), None);
    }

    #[test]
    fn render_includes_tool_calls() {
        let msgs = vec![Message { role: Role::Assistant, content: vec![ContentBlock::tool_use("1", "Read", json!({"file_path": "x"}))] }];
        assert!(render(&msgs).contains("[tool call Read"));
    }
}
