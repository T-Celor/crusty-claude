//! pxpipe: bulky old context rendered as monospace PNG pages.
//!
//! An image costs width × height / 750 tokens whatever text it holds, so dense text gets
//! cheaper as an image. The rendering is lossy for byte-exact values, which is why the
//! verbatim risk and the factsheet travel with every imaged block.
//!
//! Glyphs come from noto-sans-mono-bitmap first (Latin), then from GNU Unifont for the rest of
//! the Basic Multilingual Plane. Unifont glyphs are drawn unscaled and left-aligned: halfwidth
//! ones take one cell, fullwidth ones two. Right-to-left scripts, combining marks and scripts
//! that need shaping are drawn by neither font correctly, so blocks containing them stay text.

use std::collections::HashSet;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use crusty_core::config::PxpipeConfig;
use crusty_core::message::{ChatRequest, ContentBlock, Role, blocks_text};
use crusty_core::tokens::{estimate_text, image_tokens};
use noto_sans_mono_bitmap::{FontWeight, RasterHeight, RasterizedChar, get_raster, get_raster_width};
use unifont::Glyph as UnifontGlyph;

const FONT_WEIGHT: FontWeight = FontWeight::Regular;
const FONT_SIZE: RasterHeight = RasterHeight::Size16;
/// Glyph height in pixels: one text row.
const CELL_H: usize = RasterHeight::Size16.val();
/// Prefixes of API keys and tokens that must never be paraphrased.
const KEY_PREFIXES: [&str; 5] = ["sk-", "ghp_", "gho_", "AKIA", "xox"];
/// Minimum length of a base64-looking run that counts as precision-critical.
const BASE64_MIN_LEN: usize = 20;
/// Minimum length of a hex run that counts as precision-critical.
const HEX_MIN_LEN: usize = 8;
/// Digit runs at least this long are precision-critical.
const DIGIT_RUN_MIN: usize = 6;
/// Spaces a tab is rendered as.
const TAB_WIDTH: usize = 4;
/// Most image pages one request may carry, across all blocks; blocks beyond it stay text.
const MAX_PAGES_PER_REQUEST: usize = 20;
/// Bounds of a page side in pixels. Anthropic downsizes larger images.
const MIN_PAGE_SIDE: u32 = 64;
const MAX_PAGE_SIDE: u32 = 1568;

/// One rendered page of text.
#[derive(Debug, Clone, PartialEq)]
pub struct RenderedPage {
    /// Grayscale 8-bit PNG.
    pub png: Vec<u8>,
    /// Pixel width.
    pub width: u32,
    /// Pixel height.
    pub height: u32,
    /// Characters drawn on the page.
    pub chars: usize,
}

/// Outcome of [`PxpipeRenderer::transform`].
#[derive(Debug, Clone, PartialEq)]
pub struct PxpipeResult {
    /// The transformed request when `applied`, otherwise the original.
    pub request: ChatRequest,
    /// Whether the transformed request is returned.
    pub applied: bool,
    /// Why the transform was or was not applied.
    pub reason: String,
    /// Estimated tokens saved: text removed minus images and notes added.
    pub saving_est: i64,
    /// Estimated text tokens taken out of the request.
    pub text_tokens_removed: u64,
    /// Estimated image tokens added.
    pub image_tokens_added: u64,
    /// Image pages added.
    pub pages: usize,
    /// Blocks replaced by images.
    pub blocks_imaged: usize,
    /// Share of precision-critical tokens in the imaged text.
    pub verbatim_risk: f64,
}

/// Applies pxpipe to the old blocks of a request.
#[derive(Debug, Clone)]
pub struct PxpipeRenderer {
    cfg: PxpipeConfig,
}

/// Running totals of one transform.
#[derive(Debug, Default)]
struct Totals {
    text_tokens: u64,
    image_tokens: u64,
    note_tokens: u64,
    pages: usize,
    blocks: usize,
    critical: usize,
    tokens: usize,
}

impl PxpipeRenderer {
    /// A renderer with the given configuration. Page sides are clamped to 64..=1568 pixels,
    /// since Anthropic downsizes larger images and the saving estimate would be wrong.
    pub fn new(mut cfg: PxpipeConfig) -> Self {
        cfg.page_width = cfg.page_width.clamp(MIN_PAGE_SIDE, MAX_PAGE_SIDE);
        cfg.page_height = cfg.page_height.clamp(MIN_PAGE_SIDE, MAX_PAGE_SIDE);
        PxpipeRenderer { cfg }
    }

    /// Images the eligible old blocks of `req` if that saves tokens. Eligible blocks are text
    /// of tool results (text-only content) and user text, in messages before the last
    /// `protect_recent_messages`, with at least `min_block_chars` characters.
    pub fn transform(&self, req: &ChatRequest) -> PxpipeResult {
        if !self.cfg.enabled {
            return skipped(req, "disabled");
        }
        let boundary = req.messages.len().saturating_sub(self.cfg.protect_recent_messages);
        let mut request = req.clone();
        let mut t = Totals::default();
        for msg in request.messages.iter_mut().take(boundary) {
            let user_text = msg.role == Role::User;
            let content = std::mem::take(&mut msg.content);
            msg.content = content.into_iter().flat_map(|b| self.image_block(b, user_text, &mut t)).collect();
        }
        let saving = t.text_tokens as i64 - t.image_tokens as i64 - t.note_tokens as i64;
        let (applied, reason) = if t.blocks == 0 {
            (false, "no_eligible_blocks")
        } else if saving <= 0 {
            (false, "no_saving")
        } else {
            (true, "imaged")
        };
        PxpipeResult {
            request: if applied { request } else { req.clone() },
            applied,
            reason: reason.to_owned(),
            saving_est: saving,
            text_tokens_removed: t.text_tokens,
            image_tokens_added: t.image_tokens,
            pages: t.pages,
            blocks_imaged: t.blocks,
            verbatim_risk: ratio(t.critical, t.tokens),
        }
    }

    /// Replaces an eligible block with its note and images; any other block is returned as is.
    fn image_block(&self, block: ContentBlock, user_text: bool, t: &mut Totals) -> Vec<ContentBlock> {
        match block {
            ContentBlock::Text { text, extra } if user_text => match self.render_note(&text, t) {
                Some(blocks) => blocks,
                None => vec![ContentBlock::Text { text, extra }],
            },
            ContentBlock::ToolResult { tool_use_id, content, is_error, extra } if content.iter().all(|b| b.as_text().is_some()) => {
                let text = blocks_text(&content);
                let content = match self.render_note(&text, t) {
                    Some(blocks) => blocks,
                    None => content,
                };
                vec![ContentBlock::ToolResult { tool_use_id, content, is_error, extra }]
            }
            other => vec![other],
        }
    }

    /// Note and page images of one text, or `None` when it stays text.
    fn render_note(&self, text: &str, t: &mut Totals) -> Option<Vec<ContentBlock>> {
        let chars = text.chars().count();
        if chars < self.cfg.min_block_chars || !fully_renderable(text) {
            return None;
        }
        let pages = render_pages(text, self.cfg.page_width, self.cfg.page_height);
        if pages.is_empty() || t.pages + pages.len() > MAX_PAGES_PER_REQUEST {
            return None;
        }
        let facts = factsheet(text, self.cfg.factsheet_max);
        let exact = if facts.is_empty() { "none detected".to_owned() } else { facts.join(", ") };
        let note = format!(
            "[pxpipe: {chars} chars of earlier context rendered as monospace images ({} page(s)). Exact values: {exact}]",
            pages.len()
        );

        t.text_tokens += estimate_text(text);
        t.note_tokens += estimate_text(&note);
        t.blocks += 1;
        t.pages += pages.len();
        let (critical, total) = critical_stats(text);
        t.critical += critical;
        t.tokens += total;

        let mut blocks = vec![ContentBlock::text(note)];
        for page in pages {
            t.image_tokens += image_tokens(page.width, page.height);
            blocks.push(ContentBlock::image_png_base64(STANDARD.encode(&page.png)));
        }
        Some(blocks)
    }
}

/// The original request, reported as not transformed.
fn skipped(req: &ChatRequest, reason: &str) -> PxpipeResult {
    PxpipeResult {
        request: req.clone(),
        applied: false,
        reason: reason.to_owned(),
        saving_est: 0,
        text_tokens_removed: 0,
        image_tokens_added: 0,
        pages: 0,
        blocks_imaged: 0,
        verbatim_risk: 0.0,
    }
}

/// True when every character of `text` has a glyph, after the tab and newline handling of
/// [`render_pages`]. Text that fails this is kept as text rather than imaged.
fn fully_renderable(text: &str) -> bool {
    text.chars().all(|c| matches!(c, '\r' | '\n' | '\t') || ink_of(c).is_some())
}

/// The ink of one character: a noto glyph, or a Unifont glyph where noto has none.
enum Ink {
    Noto(RasterizedChar),
    Unifont(&'static UnifontGlyph),
}

impl Ink {
    /// Cells the character advances: two for a fullwidth Unifont glyph, otherwise one.
    fn cells(&self) -> usize {
        match self {
            Ink::Noto(_) => 1,
            Ink::Unifont(glyph) if glyph.is_fullwidth() => 2,
            Ink::Unifont(_) => 1,
        }
    }
}

/// Glyph of `c`, noto first so Latin output is unchanged. `None` for characters that are
/// not drawn (see [`unshaped`]) and for characters neither font has.
fn ink_of(c: char) -> Option<Ink> {
    if unshaped(c) {
        return None;
    }
    get_raster(c, FONT_WEIGHT, FONT_SIZE).map(Ink::Noto).or_else(|| unifont::get_glyph(c).map(Ink::Unifont))
}

/// Characters that cannot be drawn correctly one glyph at a time: right-to-left scripts
/// (drawn in logical order they read backwards, and there is no shaping), combining and
/// zero-width marks (drawn as spacing glyphs), and scripts that need shaping (Indic, Thai,
/// Lao, Myanmar, Khmer, Hangul conjoining jamo). Blocks containing them stay text.
fn unshaped(c: char) -> bool {
    matches!(
        c as u32,
        0x0300..=0x036F
            | 0x0483..=0x0489
            | 0x0590..=0x08FF
            | 0x0900..=0x0DFF
            | 0x0E00..=0x0EFF
            | 0x1000..=0x109F
            | 0x1100..=0x11FF
            | 0x1780..=0x17FF
            | 0x1AB0..=0x1AFF
            | 0x1DC0..=0x1DFF
            | 0x200B..=0x200F
            | 0x202A..=0x202E
            | 0x2060..=0x206F
            | 0x20D0..=0x20FF
            | 0xFB1D..=0xFDFF
            | 0xFE00..=0xFE0F
            | 0xFE20..=0xFE2F
            | 0xFE70..=0xFEFF
    )
}

/// Renders `text` as monospace pages of `width` × `height` pixels, wrapping at the columns.
/// Only the last page is cropped to the rows it uses. Returns no pages when encoding fails,
/// so a caller never drops text that has no image. A character without a glyph is drawn as
/// `?` here for direct callers; [`PxpipeRenderer::transform`] never images such text.
pub fn render_pages(text: &str, width: u32, height: u32) -> Vec<RenderedPage> {
    let cell_w = get_raster_width(FONT_WEIGHT, FONT_SIZE).max(1);
    let cols = (width as usize / cell_w).max(1);
    let rows = (height as usize / CELL_H).max(1);
    let mut canvas = Canvas::new(cols, rows, cell_w);
    for ch in text.chars() {
        match ch {
            '\r' => {}
            '\n' => canvas.newline(),
            '\t' => {
                for _ in 0..TAB_WIDTH {
                    canvas.put(' ');
                }
            }
            c => canvas.put(c),
        }
    }
    canvas.finish()
}

/// Page being drawn. Grayscale: white background, ink drawn as `255 - intensity`.
struct Canvas {
    cols: usize,
    rows: usize,
    cell_w: usize,
    px_w: usize,
    pixels: Vec<u8>,
    /// Cursor, in cells. `row` may reach `rows` (or beyond after blank lines): the page
    /// is then closed before the next glyph is drawn.
    row: usize,
    col: usize,
    used_rows: usize,
    chars: usize,
    pages: Vec<RenderedPage>,
    failed: bool,
}

impl Canvas {
    fn new(cols: usize, rows: usize, cell_w: usize) -> Self {
        let px_w = cols * cell_w;
        Canvas {
            cols,
            rows,
            cell_w,
            px_w,
            pixels: vec![255; px_w * rows * CELL_H],
            row: 0,
            col: 0,
            used_rows: 0,
            chars: 0,
            pages: Vec::new(),
            failed: false,
        }
    }

    fn newline(&mut self) {
        self.row += 1;
        self.col = 0;
    }

    fn put(&mut self, ch: char) {
        let ink = ink_of(ch).or_else(|| ink_of('?'));
        let cells = ink.as_ref().map_or(1, Ink::cells);
        if self.col + cells > self.cols {
            self.newline();
        }
        while self.row >= self.rows {
            self.close_page(self.rows);
            self.row -= self.rows;
        }
        if let Some(ink) = &ink {
            self.draw(ink, cells);
        }
        self.col += cells;
        self.used_rows = self.used_rows.max(self.row + 1);
        self.chars += 1;
    }

    /// Draws `ink` at the cursor, spanning `cells` cells. Unifont glyphs are not scaled.
    fn draw(&mut self, ink: &Ink, cells: usize) {
        let base_x = self.col * self.cell_w;
        let base_y = self.row * CELL_H;
        match ink {
            Ink::Noto(glyph) => {
                for (gy, line) in glyph.raster().iter().take(CELL_H).enumerate() {
                    let start = (base_y + gy) * self.px_w + base_x;
                    for (gx, &ink) in line.iter().take(self.cell_w).enumerate() {
                        if let Some(px) = self.pixels.get_mut(start + gx) {
                            *px = (*px).min(255 - ink);
                        }
                    }
                }
            }
            Ink::Unifont(glyph) => {
                let span = cells * self.cell_w;
                for gy in 0..CELL_H {
                    for gx in 0..glyph.get_width().min(span) {
                        let index = (base_y + gy) * self.px_w + base_x + gx;
                        if let Some(px) = self.pixels.get_mut(index).filter(|_| glyph.get_pixel(gx, gy)) {
                            *px = 0;
                        }
                    }
                }
            }
        }
    }

    /// Emits the current page with `rows_used` rows and starts a blank one.
    fn close_page(&mut self, rows_used: usize) {
        let height = rows_used * CELL_H;
        let mut pixels = std::mem::replace(&mut self.pixels, vec![255; self.px_w * self.rows * CELL_H]);
        pixels.truncate(self.px_w * height);
        match encode_png(&pixels, self.px_w as u32, height as u32) {
            Some(png) => self.pages.push(RenderedPage { png, width: self.px_w as u32, height: height as u32, chars: self.chars }),
            None => self.failed = true,
        }
        self.used_rows = 0;
        self.chars = 0;
    }

    fn finish(mut self) -> Vec<RenderedPage> {
        if self.chars > 0 {
            let used = self.used_rows;
            self.close_page(used);
        }
        if self.failed { Vec::new() } else { self.pages }
    }
}

fn encode_png(pixels: &[u8], width: u32, height: u32) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut encoder = png::Encoder::new(&mut out, width, height);
    encoder.set_color(png::ColorType::Grayscale);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().ok()?;
    writer.write_image_data(pixels).ok()?;
    writer.finish().ok()?;
    Some(out)
}

/// Share of whitespace-separated tokens of `text` that are precision-critical (hashes,
/// UUIDs, base64-looking runs, long digit runs, key-like tokens). Byte-exact values in this
/// share do not survive imaging reliably.
pub fn verbatim_risk(text: &str) -> f64 {
    let (critical, total) = critical_stats(text);
    ratio(critical, total)
}

/// The distinct precision-critical tokens of `text` in order of appearance, at most `max`.
/// They are copied as text next to the images.
pub fn factsheet(text: &str, max: usize) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for tok in text.split_whitespace().map(clean) {
        if out.len() >= max {
            break;
        }
        if is_critical(tok) && seen.insert(tok) {
            out.push(tok.to_owned());
        }
    }
    out
}

fn ratio(part: usize, total: usize) -> f64 {
    if total == 0 { 0.0 } else { part as f64 / total as f64 }
}

/// (critical tokens, all tokens) of `text`.
fn critical_stats(text: &str) -> (usize, usize) {
    let mut critical = 0;
    let mut total = 0;
    for tok in text.split_whitespace() {
        total += 1;
        if is_critical(clean(tok)) {
            critical += 1;
        }
    }
    (critical, total)
}

/// A token without the punctuation around it.
fn clean(tok: &str) -> &str {
    tok.trim_matches(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '=' | '_' | '-')))
}

fn is_critical(tok: &str) -> bool {
    is_hex_run(tok) || is_uuid(tok) || is_key(tok) || is_base64ish(tok) || has_digit_run(tok, DIGIT_RUN_MIN)
}

fn is_hex_run(tok: &str) -> bool {
    tok.len() >= HEX_MIN_LEN && tok.bytes().all(|b| b.is_ascii_hexdigit())
}

fn is_uuid(tok: &str) -> bool {
    const LENGTHS: [usize; 5] = [8, 4, 4, 4, 12];
    let parts: Vec<&str> = tok.split('-').collect();
    parts.len() == LENGTHS.len() && parts.iter().zip(LENGTHS).all(|(p, len)| p.len() == len && p.bytes().all(|b| b.is_ascii_hexdigit()))
}

fn is_key(tok: &str) -> bool {
    tok.len() >= HEX_MIN_LEN && KEY_PREFIXES.iter().any(|p| tok.starts_with(p))
}

/// Long run of base64 characters that is not a plain word: it needs a digit or a
/// base64-only symbol, so long hyphenated identifiers stay prose.
fn is_base64ish(tok: &str) -> bool {
    tok.len() >= BASE64_MIN_LEN
        && tok.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'=' | b'_' | b'-'))
        && tok.bytes().any(|b| b.is_ascii_digit() || matches!(b, b'+' | b'/' | b'='))
}

fn has_digit_run(tok: &str, min: usize) -> bool {
    let mut run = 0;
    for b in tok.bytes() {
        if b.is_ascii_digit() {
            run += 1;
            if run >= min {
                return true;
            }
        } else {
            run = 0;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crusty_core::message::Message;
    use crusty_core::tokens::image_tokens;

    const PAGE: u32 = 1568;

    fn png_dims(png: &[u8]) -> (u32, u32) {
        assert_eq!(&png[..8], &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A], "PNG signature");
        let w = u32::from_be_bytes([png[16], png[17], png[18], png[19]]);
        let h = u32::from_be_bytes([png[20], png[21], png[22], png[23]]);
        (w, h)
    }

    /// Dense hex text as one long paragraph, so every row is filled.
    fn dense(chars: usize) -> String {
        let mut s = String::new();
        let mut i: u32 = 1;
        while s.len() < chars {
            s.push_str(&format!("{:08x} ", i.wrapping_mul(2_654_435_761)));
            i += 1;
        }
        s
    }

    fn prose(chars: usize) -> String {
        "the quick brown fox jumps over the lazy dog ".repeat(chars / 44 + 1)
    }

    /// Ten messages: a user message with a tool result, then the protected recent ones.
    fn request_with_result(text: &str) -> ChatRequest {
        let mut req = ChatRequest::new("claude-sonnet-5-5", 1000);
        req.messages.push(Message::user_text("start"));
        req.messages
            .push(Message { role: Role::User, content: vec![ContentBlock::tool_result("t1", vec![ContentBlock::text(text)], false)] });
        for i in 0..8 {
            req.messages.push(Message::assistant_text(format!("recent {i}")));
        }
        req
    }

    #[test]
    fn page_is_a_png_with_the_expected_size() {
        let pages = render_pages("hello", PAGE, PAGE);
        assert_eq!(pages.len(), 1);
        let p = &pages[0];
        let cell_w = get_raster_width(FONT_WEIGHT, FONT_SIZE) as u32;
        assert_eq!(png_dims(&p.png), (p.width, p.height));
        assert_eq!(p.width, (PAGE / cell_w) * cell_w);
        assert_eq!(p.height, 16, "one row, cropped to the rows used");
        assert_eq!(p.chars, 5);
    }

    #[test]
    fn last_page_is_cropped_and_full_pages_are_not() {
        // 99 lines fill 98 rows; the 99th starts a second, cropped page.
        let text = "x\n".repeat(99);
        let pages = render_pages(&text, PAGE, PAGE);
        assert_eq!(pages.len(), 2);
        assert_eq!(pages[0].height, 98 * 16);
        assert_eq!(pages[1].height, 16);
    }

    #[test]
    fn empty_text_has_no_pages() {
        assert!(render_pages("", PAGE, PAGE).is_empty());
    }

    #[test]
    fn every_character_is_drawn_once() {
        let text = dense(100_000);
        let pages = render_pages(&text, PAGE, PAGE);
        let drawn: usize = pages.iter().map(|p| p.chars).sum();
        assert_eq!(drawn, text.chars().count());
    }

    #[test]
    fn dense_old_result_is_imaged_with_positive_saving() {
        let text = dense(10_000);
        let req = request_with_result(&text);
        let cfg = PxpipeConfig::default();
        let r = PxpipeRenderer::new(cfg).transform(&req);
        assert!(r.applied, "reason: {}", r.reason);
        assert_eq!(r.reason, "imaged");
        assert_eq!(r.blocks_imaged, 1);
        assert!(r.pages >= 1);
        assert!(r.saving_est > 0);
        assert!(r.image_tokens_added < r.text_tokens_removed);
        assert_eq!(r.verbatim_risk, 1.0, "hex text is all precision-critical");

        // The tool result now holds a note and the images; recent messages are untouched.
        let ContentBlock::ToolResult { content, .. } = &r.request.messages[1].content[0] else {
            panic!("tool result expected");
        };
        assert!(content[0].as_text().is_some_and(|n| n.starts_with("[pxpipe: ")));
        assert!(content.iter().skip(1).all(|b| matches!(b, ContentBlock::Image { .. })));
        assert_eq!(r.request.messages[2..], req.messages[2..]);
    }

    #[test]
    fn short_blocks_stay_text() {
        let req = request_with_result("short result");
        let r = PxpipeRenderer::new(PxpipeConfig::default()).transform(&req);
        assert!(!r.applied);
        assert_eq!(r.reason, "no_eligible_blocks");
        assert_eq!(r.request, req);
    }

    #[test]
    fn disabled_renderer_changes_nothing() {
        let req = request_with_result(&dense(10_000));
        let r = PxpipeRenderer::new(PxpipeConfig { enabled: false, ..PxpipeConfig::default() }).transform(&req);
        assert!(!r.applied);
        assert_eq!(r.reason, "disabled");
        assert_eq!(r.request, req);
    }

    #[test]
    fn verbatim_risk_is_high_for_hex_and_zero_for_prose() {
        assert!(verbatim_risk(&dense(2_000)) > 0.9);
        assert_eq!(verbatim_risk(&prose(2_000)), 0.0);
        assert_eq!(verbatim_risk(""), 0.0);
    }

    #[test]
    fn factsheet_extracts_hashes_and_ids() {
        let text = "fixed in 9fceb02d0ae598e95dc970b74767f19372d61af8 for user 123e4567-e89b-12d3-a456-426614174000, \
                    call 1234567 and the rest of the prose.";
        let facts = factsheet(text, 10);
        assert!(facts.contains(&"9fceb02d0ae598e95dc970b74767f19372d61af8".to_owned()));
        assert!(facts.contains(&"123e4567-e89b-12d3-a456-426614174000".to_owned()));
        assert!(facts.contains(&"1234567".to_owned()));
        assert!(!facts.iter().any(|f| f == "prose"));
    }

    #[test]
    fn factsheet_is_distinct_and_capped() {
        let facts = factsheet("0123456789 0123456789 abcdef012345 abcdef012345", 1);
        assert_eq!(facts, vec!["0123456789".to_owned()]);
        let facts = factsheet("0123456789 0123456789 abcdef012345 abcdef012345", 10);
        assert_eq!(facts.len(), 2);
    }

    #[test]
    fn image_tokens_follow_area_over_750() {
        let pages = render_pages(&dense(3_000), PAGE, PAGE);
        for p in &pages {
            assert_eq!(image_tokens(p.width, p.height), (p.width as u64 * p.height as u64).div_ceil(750));
        }
        assert_eq!(image_tokens(750, 2), 2);
    }

    #[test]
    fn text_outside_the_font_stays_text() {
        let text = format!("{}\u{1F600}", dense(10_000));
        assert!(!fully_renderable(&text));
        let r = PxpipeRenderer::new(PxpipeConfig::default()).transform(&request_with_result(&text));
        assert_eq!((r.applied, r.blocks_imaged), (false, 0));
    }

    #[test]
    fn unifont_scripts_are_renderable_and_drawn() {
        let text = "中文 かなカナ 한국어 Привет Ελληνικά ┌─┐ → ←\n".repeat(50);
        assert!(fully_renderable(&text));
        assert!(!render_pages(&text, PAGE, PAGE).is_empty());
    }

    #[test]
    fn fullwidth_glyph_takes_two_cells() {
        // "中a": the 'a' must sit where a plain "a" would sit two cells to the right.
        let cw = get_raster_width(FONT_WEIGHT, FONT_SIZE);
        let mut wide = Canvas::new(8, 1, cw);
        wide.put('中');
        wide.put('a');
        assert_eq!(wide.col, 3);
        let mut plain = Canvas::new(8, 1, cw);
        plain.put('a');
        for y in 0..CELL_H {
            let row_wide = &wide.pixels[y * wide.px_w..(y + 1) * wide.px_w];
            let row_plain = &plain.pixels[y * plain.px_w..(y + 1) * plain.px_w];
            assert_eq!(&row_wide[2 * cw..3 * cw], &row_plain[..cw], "row {y}");
        }
    }

    #[test]
    fn rtl_marks_and_unassigned_characters_stay_text() {
        assert!(!fully_renderable("مرحبا"));
        assert!(!fully_renderable("שלום"));
        assert!(!fully_renderable("e\u{0301}"));
        assert!(!fully_renderable("\u{10FFFD}"), "unassigned plane 16 code point");
        assert!(!fully_renderable("\u{1F600}"));
        assert!(fully_renderable("plain ASCII and Ü ß"));
    }

    #[test]
    fn pages_per_request_are_capped() {
        // Short pages (10 rows) make the cap reachable with a small text.
        let cfg = PxpipeConfig { page_height: 160, ..PxpipeConfig::default() };
        let text = dense(60_000);
        assert!(render_pages(&text, cfg.page_width, cfg.page_height).len() > MAX_PAGES_PER_REQUEST);
        let r = PxpipeRenderer::new(cfg).transform(&request_with_result(&text));
        assert_eq!((r.applied, r.blocks_imaged, r.pages), (false, 0, 0));
    }

    #[test]
    fn page_sides_are_clamped() {
        let big = PxpipeRenderer::new(PxpipeConfig { page_width: 10_000, page_height: 1, ..PxpipeConfig::default() });
        assert_eq!((big.cfg.page_width, big.cfg.page_height), (MAX_PAGE_SIDE, MIN_PAGE_SIDE));
    }

    #[test]
    fn large_block_renders_all_of_its_text() {
        let text = dense(100_000);
        let pages = render_pages(&text, PAGE, PAGE);
        assert!(!pages.is_empty());
        assert!(pages.iter().all(|p| p.height as usize <= PAGE as usize));
    }
}
