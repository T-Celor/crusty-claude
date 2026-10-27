//! Masking of a whole outgoing request.

use crusty_core::advisor::{AdviceSource, Advisor, Scores, SecretCandidate};
use crusty_core::config::DlpConfig;
use crusty_core::message::{ChatRequest, ContentBlock};
use crusty_core::vault::SecretVault;
use serde_json::Value;

use crate::detect::{CERTAIN, Finding, scan};
use crate::mask::{MaskCounts, mask_text};

/// Bytes of text kept on each side of an ambiguous value for the advisor.
const CONTEXT_BYTES: usize = 60;

/// What [`redact_request`] masked.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct RedactionReport {
    /// Distinct secret values masked in the request.
    pub secrets: u64,
    /// Distinct personal-data values masked in the request.
    pub pii: u64,
    /// Ambiguous findings sent to the advisor.
    pub ambiguous_checked: usize,
    /// Ambiguous findings masked after the advisor's decision.
    pub ambiguous_masked: usize,
    /// Who decided the ambiguous findings; `None` when the advisor abstained or was not asked.
    pub source: AdviceSource,
}

/// Masks secrets and personal data in `req` in place.
///
/// Covers system text, text blocks, tool-result text and every string inside tool-use
/// inputs and in object keys. Thinking blocks, images and unknown blocks are left untouched
/// (see [`visit_blocks`]). Ambiguous findings are sent to `advisor` once; those with a
/// probability at or above `cfg.ambiguous_threshold` are masked. When the advisor abstains
/// they are masked if their own confidence is at or above `cfg.ambiguous_threshold`. The output is deterministic for a given
/// vault, so masking a request twice changes nothing the second time.
pub async fn redact_request(req: &mut ChatRequest, vault: &SecretVault, advisor: &dyn Advisor, cfg: &DlpConfig) -> RedactionReport {
    let mut report = RedactionReport::default();
    if !cfg.enabled {
        return report;
    }

    // Pass 1: scan every string and collect the ambiguous findings for the advisor.
    let mut found: Vec<Vec<Finding>> = Vec::new();
    let mut pending: Vec<(usize, usize)> = Vec::new();
    let mut candidates: Vec<SecretCandidate> = Vec::new();
    visit_request(req, &mut |text: &mut String| {
        let findings = scan(text, cfg);
        let slot = found.len();
        for (i, f) in findings.iter().enumerate() {
            if f.confidence < CERTAIN {
                pending.push((slot, i));
                candidates.push(SecretCandidate {
                    value: text.get(f.start..f.end).unwrap_or_default().to_owned(),
                    context: context(text, f.start, f.end),
                });
            }
        }
        found.push(findings);
    });

    // Pass 2: decide the ambiguous findings, then mask.
    let mut keep: Vec<Vec<bool>> = found.iter().map(|fs| fs.iter().map(|f| f.confidence >= CERTAIN).collect()).collect();
    if !candidates.is_empty() {
        report.ambiguous_checked = candidates.len();
        let scores = advisor.secret_probability(&candidates).await;
        report.source = scores.source;
        decide(&scores, &found, &pending, cfg, &mut keep, &mut report);
    }

    let mut slot_no = 0;
    let mut counts = MaskCounts::default();
    visit_request(req, &mut |text: &mut String| {
        let slot = slot_no;
        slot_no += 1;
        let selected: Vec<Finding> = match (found.get(slot), keep.get(slot)) {
            (Some(fs), Some(ks)) => fs.iter().zip(ks).filter(|(_, k)| **k).map(|(f, _)| f.clone()).collect(),
            _ => Vec::new(),
        };
        if selected.is_empty() {
            return;
        }
        let (masked, c) = mask_text(text, &selected, vault);
        *text = masked;
        counts.merge(c);
    });

    report.secrets = counts.secrets.len() as u64;
    report.pii = counts.pii.len() as u64;
    report
}

/// Marks the ambiguous findings the advisor (or the fallback rule) wants masked.
fn decide(
    scores: &Scores,
    found: &[Vec<Finding>],
    pending: &[(usize, usize)],
    cfg: &DlpConfig,
    keep: &mut [Vec<bool>],
    report: &mut RedactionReport,
) {
    let probs = scores.for_items(pending.len());
    for (k, &(slot, i)) in pending.iter().enumerate() {
        let masked = match probs {
            Some(p) => p.get(k).is_some_and(|&p| p >= cfg.ambiguous_threshold),
            None => found.get(slot).and_then(|fs| fs.get(i)).is_some_and(|f| f.confidence >= cfg.ambiguous_threshold),
        };
        if let Some(flag) = keep.get_mut(slot).and_then(|ks| ks.get_mut(i)).filter(|_| masked) {
            *flag = true;
            report.ambiguous_masked += 1;
        }
    }
}

/// The text around the value at `start..end`, without the value itself.
fn context(text: &str, start: usize, end: usize) -> String {
    let mut lo = start.saturating_sub(CONTEXT_BYTES);
    while !text.is_char_boundary(lo) {
        lo -= 1;
    }
    let mut hi = (end + CONTEXT_BYTES).min(text.len());
    while !text.is_char_boundary(hi) {
        hi += 1;
    }
    let before = text.get(lo..start).unwrap_or_default();
    let after = text.get(end..hi).unwrap_or_default();
    format!("{before}{after}")
}

/// Calls `f` on every string the request sends that DLP covers, in a fixed order.
fn visit_request(req: &mut ChatRequest, f: &mut dyn FnMut(&mut String)) {
    visit_blocks(&mut req.system, f);
    for m in &mut req.messages {
        visit_blocks(&mut m.content, f);
    }
}

/// Thinking and redacted-thinking blocks are not changed: their signatures bind the exact
/// bytes, so editing them would invalidate the request. Images are not text. Neither can
/// hold an unmasked secret the model has seen, because the model only ever received masked
/// input; a secret that detection missed is a detection gap, not something this stage can fix
/// there. Unknown blocks are passed through unchanged for the same reason.
fn visit_blocks(blocks: &mut [ContentBlock], f: &mut dyn FnMut(&mut String)) {
    for block in blocks {
        match block {
            ContentBlock::Text { text, .. } => f(text),
            ContentBlock::ToolUse { input, .. } => visit_value(input, f),
            ContentBlock::ToolResult { content, .. } => visit_blocks(content, f),
            ContentBlock::Image { .. }
            | ContentBlock::Thinking { .. }
            | ContentBlock::RedactedThinking { .. }
            | ContentBlock::Unknown(_) => {}
        }
    }
}

/// Every string in a JSON value, object keys included. Keys are rebuilt in order after masking.
fn visit_value(v: &mut Value, f: &mut dyn FnMut(&mut String)) {
    match v {
        Value::String(s) => f(s),
        Value::Array(items) => items.iter_mut().for_each(|x| visit_value(x, f)),
        Value::Object(map) => {
            let taken = std::mem::take(map);
            for (mut key, mut value) in taken {
                f(&mut key);
                visit_value(&mut value, f);
                map.insert(key, value);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}
