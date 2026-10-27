//! Token estimates. They are estimates: 4 characters per text token, images priced by
//! pixel area the way Anthropic bills them. Real counts come from providers.

use base64::Engine as _;
use serde_json::Value;

use crate::message::{ChatRequest, ContentBlock, ImageSource, Message};

/// Flat cost of an image whose size cannot be read.
pub const IMAGE_FLAT_TOKENS: u64 = 1600;

/// Characters per text token used by every estimate.
pub const CHARS_PER_TOKEN: u64 = 4;

pub fn estimate_text(s: &str) -> u64 {
    (s.chars().count() as u64).div_ceil(CHARS_PER_TOKEN)
}

/// Tokens a base64 PNG costs (width × height / 750), or the flat amount if the header
/// cannot be read.
pub fn png_tokens(b64: &str) -> u64 {
    let head_len = b64.len().min(32);
    // 32 base64 chars decode to 24 bytes: the signature plus the IHDR width and height.
    let Ok(head) = base64::engine::general_purpose::STANDARD.decode(&b64.as_bytes()[..head_len - head_len % 4]) else {
        return IMAGE_FLAT_TOKENS;
    };
    if head.len() < 24 || head[..4] != [0x89, b'P', b'N', b'G'] {
        return IMAGE_FLAT_TOKENS;
    }
    let w = u32::from_be_bytes([head[16], head[17], head[18], head[19]]) as u64;
    let h = u32::from_be_bytes([head[20], head[21], head[22], head[23]]) as u64;
    (w * h).div_ceil(750)
}

pub fn image_tokens(width: u32, height: u32) -> u64 {
    (width as u64 * height as u64).div_ceil(750)
}

fn chars_in_value(v: &Value) -> u64 {
    match v {
        Value::String(s) => s.chars().count() as u64,
        Value::Array(items) => items.iter().map(chars_in_value).sum(),
        Value::Object(map) => map.values().map(chars_in_value).sum(),
        _ => 0,
    }
}

/// (text characters, image tokens) of some blocks.
fn block_parts(blocks: &[ContentBlock], png_size: bool) -> (u64, u64) {
    let mut chars = 0;
    let mut images = 0;
    for b in blocks {
        match b {
            ContentBlock::Text { text, .. } => chars += text.chars().count() as u64,
            ContentBlock::Image { source, .. } => {
                images += match source {
                    ImageSource::Base64 { media_type, data } if png_size && media_type == "image/png" => png_tokens(data),
                    _ => IMAGE_FLAT_TOKENS,
                }
            }
            ContentBlock::ToolUse { name, input, .. } => chars += name.len() as u64 + chars_in_value(input),
            ContentBlock::ToolResult { content, .. } => {
                let (c, i) = block_parts(content, png_size);
                chars += c;
                images += i;
            }
            ContentBlock::Thinking { thinking, .. } => chars += thinking.chars().count() as u64,
            ContentBlock::RedactedThinking { data, .. } => chars += data.len() as u64 / 4,
            ContentBlock::Unknown(v) => chars += chars_in_value(v),
        }
    }
    (chars, images)
}

pub fn estimate_blocks(blocks: &[ContentBlock], png_size: bool) -> u64 {
    let (chars, images) = block_parts(blocks, png_size);
    chars.div_ceil(CHARS_PER_TOKEN) + images
}

pub fn estimate_messages(messages: &[Message], png_size: bool) -> u64 {
    let mut chars = 0;
    let mut images = 0;
    for m in messages {
        let (c, i) = block_parts(&m.content, png_size);
        chars += c;
        images += i;
    }
    chars.div_ceil(CHARS_PER_TOKEN) + images
}

/// Estimated prompt size of a request: system, messages and tool definitions.
/// With `png_size`, PNG images are priced by their real dimensions.
pub fn estimate_request(req: &ChatRequest, png_size: bool) -> u64 {
    let (mut chars, mut images) = block_parts(&req.system, png_size);
    for m in &req.messages {
        let (c, i) = block_parts(&m.content, png_size);
        chars += c;
        images += i;
    }
    for t in &req.tools {
        chars += t.name.len() as u64 + t.description.chars().count() as u64 + chars_in_value(&t.input_schema);
    }
    chars.div_ceil(CHARS_PER_TOKEN) + images
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png_header(w: u32, h: u32) -> String {
        let mut bytes = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 13, b'I', b'H', b'D', b'R'];
        bytes.extend_from_slice(&w.to_be_bytes());
        bytes.extend_from_slice(&h.to_be_bytes());
        bytes.extend_from_slice(&[8, 0, 0, 0, 0]);
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    #[test]
    fn text_estimate_rounds_up() {
        assert_eq!(estimate_text(""), 0);
        assert_eq!(estimate_text("abcde"), 2);
    }

    #[test]
    fn png_is_priced_by_area() {
        assert_eq!(png_tokens(&png_header(750, 2)), 2);
        assert_eq!(png_tokens("not-a-png"), IMAGE_FLAT_TOKENS);
    }

    #[test]
    fn request_estimate_counts_everything() {
        let mut req = ChatRequest::new("m", 10);
        req.system = vec![ContentBlock::text("abcd")];
        req.messages.push(Message::user_text("abcdefgh"));
        req.messages.push(Message { role: crate::message::Role::User, content: vec![ContentBlock::image_png_base64(png_header(1500, 1))] });
        assert_eq!(estimate_request(&req, false), 3 + IMAGE_FLAT_TOKENS);
        assert_eq!(estimate_request(&req, true), 3 + 2);
    }
}
