//! Local token counts. Exact for OpenAI models (tiktoken encodings); `None` for everything else.

use crusty_core::message::{ChatRequest, ContentBlock};
use tiktoken_rs::{CoreBPE, cl100k_base_singleton, o200k_base_singleton};

/// Tokens each chat message adds on top of its content (OpenAI chat format).
const TOKENS_PER_MESSAGE: u64 = 3;
/// Tokens that prime the assistant's reply.
const REPLY_PRIMING: u64 = 3;

/// Encoding of an OpenAI model id: `o200k_base` for gpt-4o, gpt-4.1, gpt-5 and the o-series,
/// `cl100k_base` for gpt-4 and gpt-3.5.
fn encoding(model: &str) -> Option<&'static CoreBPE> {
    let model = model.to_ascii_lowercase();
    if ["gpt-4o", "gpt-4.1", "gpt-5", "o1", "o3", "o4"].iter().any(|p| model.starts_with(p)) {
        Some(o200k_base_singleton())
    } else if ["gpt-4", "gpt-3.5"].iter().any(|p| model.starts_with(p)) {
        Some(cl100k_base_singleton())
    } else {
        None
    }
}

/// Token count of an OpenAI chat request: 3 per message, 3 for the reply, the text of every
/// message and system block, and the JSON of each tool spec. Images are not counted.
pub fn count_openai(model: &str, req: &ChatRequest) -> Option<u64> {
    let bpe = encoding(model)?;
    let count = |text: &str| bpe.encode_with_special_tokens(text).len() as u64;
    let mut total = REPLY_PRIMING;
    if !req.system.is_empty() {
        total += TOKENS_PER_MESSAGE + count(&blocks_text(&req.system));
    }
    for message in &req.messages {
        total += TOKENS_PER_MESSAGE + count(&blocks_text(&message.content));
    }
    for tool in &req.tools {
        total += count(&serde_json::to_string(tool).ok()?);
    }
    Some(total)
}

/// Text the tokenizer sees in these blocks: text, thinking, tool calls and tool results.
fn blocks_text(blocks: &[ContentBlock]) -> String {
    let mut out = String::new();
    for block in blocks {
        match block {
            ContentBlock::Text { text, .. } => out.push_str(text),
            ContentBlock::Thinking { thinking, .. } => out.push_str(thinking),
            ContentBlock::ToolUse { name, input, .. } => {
                out.push_str(name);
                out.push_str(&input.to_string());
            }
            ContentBlock::ToolResult { content, .. } => out.push_str(&blocks_text(content)),
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crusty_core::ToolSpec;
    use crusty_core::message::Message;
    use serde_json::json;

    #[test]
    fn non_openai_models_have_no_count() {
        let req = ChatRequest::new("llama3", 10);
        assert_eq!(count_openai("llama3", &req), None);
        assert_eq!(count_openai("gemini-2.5-pro", &req), None);
    }

    #[test]
    fn one_hello_message_costs_seven_tokens() {
        // "hello" is one token in both encodings: 3 (message) + 1 + 3 (reply priming).
        let mut req = ChatRequest::new("gpt-4o", 10);
        req.messages.push(Message::user_text("hello"));
        assert_eq!(count_openai("gpt-4o", &req), Some(7));
        assert_eq!(count_openai("gpt-4", &req), Some(7));
    }

    #[test]
    fn tool_specs_count_their_json() {
        let tool = ToolSpec::new("t", "d", json!({"type": "object"}));
        let mut req = ChatRequest::new("gpt-4.1", 10);
        req.tools.push(tool.clone());
        let json = serde_json::to_string(&tool).expect("json");
        let expected = REPLY_PRIMING + encoding("gpt-4.1").map(|bpe| bpe.encode_with_special_tokens(&json).len() as u64).unwrap_or(0);
        assert_eq!(count_openai("gpt-4.1", &req), Some(expected));
    }
}
