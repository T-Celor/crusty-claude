//! Tool-output filters (RTK, HTML→Markdown) as seen by tools, and the one policy that
//! decides whether a filtered text replaces the original. Implementations live in
//! `crusty-optimizer`; tools only see `ContentFilter`.

use std::time::Instant;

use crate::config::OptimizerMode;
use crate::event::{AppEvent, Stage, StageMetrics, StageStatus};
use crate::tokens::estimate_text;
use crate::tool::ToolContext;

/// A shortened text and the transform that produced it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Filtered {
    pub text: String,
    /// e.g. `rtk:git-status`.
    pub transformer: String,
}

/// A web page converted for the model.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HtmlDoc {
    pub markdown: String,
    pub title: Option<String>,
    pub canonical_url: Option<String>,
    pub description: Option<String>,
    pub published: Option<String>,
}

pub trait ContentFilter: Send + Sync {
    /// Shortens a shell command's combined output. `None` when no filter applies.
    fn command_output(&self, command: &str, output: &str, exit_code: Option<i32>) -> Option<Filtered>;

    /// Converts an HTML page. `None` when conversion is disabled or failed.
    fn html_to_markdown(&self, html: &str, url: &str) -> Option<HtmlDoc>;
}

/// No filtering: every text is used as it is.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoFilter;

impl ContentFilter for NoFilter {
    fn command_output(&self, _command: &str, _output: &str, _exit_code: Option<i32>) -> Option<Filtered> {
        None
    }

    fn html_to_markdown(&self, _html: &str, _url: &str) -> Option<HtmlDoc> {
        None
    }
}

/// Decides between `original` and `filtered`, records the measurement, keeps the original
/// retrievable, and returns the text to give the model.
///
/// The filtered text is used only when the optimizer is enabled and active and the
/// estimated gain reaches `optimizer.minimum_estimated_gain`.
pub fn choose_filtered(
    ctx: &ToolContext,
    stage: Stage,
    content_kind: &str,
    original: &str,
    filtered: Option<Filtered>,
    started: Instant,
) -> String {
    let Some(filtered) = filtered else {
        return original.to_owned();
    };
    let opt = &ctx.config.optimizer;
    let before = estimate_text(original);
    let after = estimate_text(&filtered.text);
    let mut m = StageMetrics::new(stage, content_kind, &filtered.transformer);
    m.input_bytes = original.len() as u64;
    m.output_bytes = filtered.text.len() as u64;
    m.tokens_before_est = before;
    m.tokens_after_est = after;
    m.session_id.clone_from(&ctx.session_id);
    m.agent_id.clone_from(&ctx.agent_id);
    let gain = if before == 0 { 0.0 } else { 1.0 - after as f64 / before as f64 };
    let use_filtered = if !opt.enabled {
        m.detail = "optimizer disabled".into();
        false
    } else if gain < opt.minimum_estimated_gain {
        m.detail = format!("gain {:.0}% below minimum", gain * 100.0);
        false
    } else if opt.mode == OptimizerMode::Observe {
        m.status = StageStatus::Observed;
        m.detail = format!("observe: would save {:.0}%", gain * 100.0);
        false
    } else {
        m.status = StageStatus::Applied;
        m.detail = format!("-{:.0}%", gain * 100.0);
        true
    };
    let text = if use_filtered {
        let mut text = filtered.text;
        if opt.preserve_original {
            let handle = ctx.store.put(original);
            text.push_str(&format!(
                "\n[crusty: output shortened by {} ({after}/{before} est. tokens); full original: Retrieve(handle=\"{handle}\")]",
                filtered.transformer
            ));
            m.original_ref = Some(handle);
        }
        text
    } else {
        original.to_owned()
    };
    m.duration_us = started.elapsed().as_micros() as u64;
    ctx.events.emit(AppEvent::Optimizer(m));
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn ctx(mutate: impl FnOnce(&mut crate::Config)) -> ToolContext {
        let mut c = ToolContext::for_tests(std::env::temp_dir());
        let mut cfg = crate::Config::default();
        mutate(&mut cfg);
        c.config = Arc::new(cfg);
        c
    }

    fn f(text: &str) -> Option<Filtered> {
        Some(Filtered { text: text.into(), transformer: "rtk:test".into() })
    }

    #[test]
    fn applies_and_keeps_original() {
        let c = ctx(|_| {});
        let original = "line\n".repeat(100);
        let out = choose_filtered(&c, Stage::Rtk, "tool_output", &original, f("line (x100)"), Instant::now());
        assert!(out.starts_with("line (x100)"));
        let handle = out.split("handle=\"").nth(1).unwrap().trim_end_matches("\")]");
        assert_eq!(c.store.get(handle).unwrap().as_str(), original);
        assert_eq!(c.events.metrics().snapshot().stages[&Stage::Rtk].applied, 1);
    }

    #[test]
    fn small_gain_keeps_original() {
        let c = ctx(|_| {});
        let out = choose_filtered(&c, Stage::Rtk, "tool_output", "abcdefgh", f("abcdefg"), Instant::now());
        assert_eq!(out, "abcdefgh");
    }

    #[test]
    fn observe_mode_measures_without_applying() {
        let c = ctx(|cfg| cfg.optimizer.mode = OptimizerMode::Observe);
        let original = "x".repeat(400);
        let out = choose_filtered(&c, Stage::Rtk, "tool_output", &original, f("x"), Instant::now());
        assert_eq!(out, original);
        let t = &c.events.metrics().snapshot().stages[&Stage::Rtk];
        assert_eq!((t.applied, t.observed), (0, 1));
    }
}
