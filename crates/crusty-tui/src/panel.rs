//! Side panel: live optimizer stages, agents and plan. Every number comes from the metrics
//! snapshot taken on the last tick, or from configuration. Estimates are labelled "est.".

use crusty_core::event::{AdvicePoint, AgentStatus, Stage, TodoStatus};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph, Wrap};
use serde_json::Value;

use crate::app::{App, is_running};
use crate::theme;
use crate::ui::rows;

/// Formats a token count: `12345` gives `12.3k`, `1200000` gives `1.2M`.
pub fn fmt_tokens(n: u64) -> String {
    if n < 1_000 {
        n.to_string()
    } else if n < 1_000_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    }
}

/// Formats a saved-token count with a sign: positive savings are `-12.3k`, growth is `+1.0k`.
pub fn fmt_saved(v: i64) -> String {
    match v {
        0 => "0".to_string(),
        v if v > 0 => format!("-{}", fmt_tokens(v as u64)),
        v => format!("+{}", fmt_tokens(v.unsigned_abs())),
    }
}

/// Formats a saved share (0.0 to 1.0) as a signed percentage: 0.61 gives `-61%`.
pub fn fmt_ratio(ratio: f64) -> String {
    let pct = (ratio * 100.0).round() as i64;
    match pct {
        0 => "0%".to_string(),
        p if p > 0 => format!("-{p}%"),
        p => format!("+{}%", -p),
    }
}

/// Draws the side panel into `area`.
pub fn draw(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let sections: Vec<(&'static str, Vec<Line<'static>>)> = vec![
        (" Optimizer (live) ", optimizer_lines(app)),
        (" Agents ", agent_lines(app)),
        (" Plan ", plan_lines(app)),
        (" Advisor ", advisor_lines(app)),
        (" Sandbox ", sandbox_lines(app)),
        (" DLP ", dlp_lines(app)),
        (" Checkpoints ", checkpoint_lines(app)),
    ];
    let width = area.width.saturating_sub(2) as usize;
    let constraints: Vec<Constraint> =
        sections.iter().map(|(_, lines)| Constraint::Length(height(lines, width))).chain(std::iter::once(Constraint::Min(0))).collect();
    let areas = Layout::vertical(constraints).split(area);
    for ((title, lines), part) in sections.into_iter().zip(areas.iter()) {
        block(frame, *part, title, lines);
    }
}

/// Lines of the "Advisor" block: Laya health, then per-point answers.
fn advisor_lines(app: &App) -> Vec<Line<'static>> {
    let adv = &app.snapshot.advisor;
    let mut out = Vec::new();
    if adv.url.is_empty() {
        out.push(Line::styled("  Laya: native", theme::dim()));
    } else if adv.up {
        let latency = adv.latency_ms.map_or(String::new(), |ms| format!(" {ms}ms"));
        out.push(Line::from(vec![Span::raw("  Laya: "), Span::styled("● up", theme::OK), Span::raw(latency)]));
    } else {
        out.push(Line::from(vec![Span::raw("  Laya: "), Span::styled("✕ down", theme::ERR)]));
    }
    for point in AdvicePoint::ALL {
        if let Some(t) = adv.points.get(&point) {
            out.push(Line::raw(format!(
                "  {:<9} laya {} · native {} · abstain {} · review/block {}",
                point.label(),
                t.laya,
                t.native,
                t.abstained,
                t.escalations
            )));
        }
    }
    if out.len() == 1 {
        out.push(Line::styled("  (no advice yet)", theme::dim()));
    }
    if let Some(reason) = &adv.last_fallback_reason {
        out.push(Line::styled(format!("  last fallback: {reason}"), theme::warn()));
    }
    out
}

/// Lines of the "Sandbox" block: backend, mode and network, or a warning when unsandboxed.
fn sandbox_lines(app: &App) -> Vec<Line<'static>> {
    let Some(info) = &app.snapshot.sandbox else {
        return vec![Line::styled("  (not reported)", theme::dim())];
    };
    let mut out = Vec::new();
    if info.backend == "none" || info.mode == "off" {
        out.push(Line::styled("⚠ unsandboxed", theme::ERR));
    } else {
        let network = if info.network_blocked { "off" } else { "on" };
        out.push(Line::raw(format!("  {} · {} · network {network}", info.backend, info.mode)));
    }
    if !info.available {
        out.push(Line::styled(format!("  backend unavailable: {}", info.detail), theme::warn()));
    }
    out
}

/// Lines of the "DLP" block: values masked in this session.
fn dlp_lines(app: &App) -> Vec<Line<'static>> {
    let snap = &app.snapshot;
    if snap.redacted_secrets == 0 && snap.redacted_pii == 0 {
        return vec![Line::styled("  nothing masked yet", theme::dim())];
    }
    vec![Line::raw(format!("  secrets masked {} · PII masked {}", snap.redacted_secrets, snap.redacted_pii))]
}

/// Lines of the "Checkpoints" block: count, rewinds and the newest id.
fn checkpoint_lines(app: &App) -> Vec<Line<'static>> {
    let last = app.last_checkpoint.map_or_else(|| "–".to_string(), |id| format!("#{id}"));
    vec![Line::raw(format!("  {} saved · {} rewinds · last {last}", app.snapshot.checkpoints, app.snapshot.rewinds))]
}

fn block(frame: &mut Frame<'_>, area: Rect, title: &'static str, lines: Vec<Line<'static>>) {
    let widget = Paragraph::new(lines).wrap(Wrap { trim: false }).block(Block::bordered().title(title));
    frame.render_widget(widget, area);
}

/// Outer height of a bordered block holding `lines`, given the inner width.
fn height(lines: &[Line<'static>], inner_width: usize) -> u16 {
    (rows(lines, inner_width as u16) + 2).min(u16::MAX as usize) as u16
}

/// Whether a stage is enabled in the configuration.
fn stage_enabled(app: &App, stage: Stage) -> bool {
    let cfg = app.config();
    let opt = &cfg.optimizer;
    match stage {
        Stage::Ponytail => opt.ponytail.enabled,
        Stage::Rtk => opt.enabled && opt.rtk.enabled,
        Stage::HtmlToMarkdown => opt.enabled && opt.html_to_markdown.enabled,
        Stage::Graphify => opt.enabled && opt.graphify.enabled,
        Stage::Headroom => opt.enabled && opt.headroom.enabled,
        Stage::ContextGate => opt.enabled && opt.context_gate.enabled,
        Stage::Pxpipe => opt.enabled && opt.pxpipe.enabled,
        Stage::Proxy => cfg.proxy.upstreams.iter().any(|u| u.enabled),
        Stage::Triage => opt.enabled && cfg.laya.triage.enabled,
        Stage::Rerank => opt.enabled && cfg.laya.rerank.enabled,
        Stage::Prune => opt.enabled && cfg.laya.pruner.enabled,
        Stage::Dlp => cfg.laya.dlp.enabled,
    }
}

/// Lines of the "Optimizer (live)" block.
fn optimizer_lines(app: &App) -> Vec<Line<'static>> {
    let snap = &app.snapshot;
    let cfg = app.config();
    let mut out = Vec::new();
    for stage in Stage::ALL {
        let t = snap.stages.get(&stage).cloned().unwrap_or_default();
        let (dot, color) = if !stage_enabled(app, stage) {
            ("○", theme::MUTED)
        } else if t.fallback + t.failed > 0 {
            ("✕", theme::ERR)
        } else if t.applied > 0 {
            ("●", theme::OK)
        } else {
            ("◌", theme::MUTED)
        };
        let mut spans = vec![
            Span::styled(format!("{dot} "), color),
            Span::raw(format!("{:<10}", stage.label())),
            Span::raw(format!(" runs {:>4}", t.runs)),
        ];
        if t.real_samples > 0 {
            let saved_real = t.tokens_before_real as i64 - t.tokens_after_real as i64;
            let ratio = t.ratio_real().map_or_else(String::new, |r| format!(" {}", fmt_ratio(r)));
            spans.push(Span::styled(format!(" measured {}{ratio}", fmt_saved(saved_real)), theme::OK));
        } else if t.tokens_before_est > 0 {
            spans.push(Span::raw(format!(" saved {} ({} est.)", fmt_saved(t.saved_est()), fmt_ratio(t.ratio_est()))));
        }
        out.push(Line::from(spans));
    }

    let ponytail_label = snap.policy_label.clone().unwrap_or_else(|| format!("ponytail {}", cfg.optimizer.ponytail.level));
    out.push(Line::styled(format!("  {} · {} agents", ponytail_label, snap.policy_injections), theme::dim()));

    let g = &snap.gate;
    let last = g
        .last
        .as_ref()
        .map_or_else(|| "last: –".to_string(), |l| format!("last: {} ({}, zone {}) ", l.route, l.reason, l.zone.as_deref().unwrap_or("-")));
    out.push(Line::styled(format!("  Gate {last}· direct {} / pxpipe {}", g.direct, g.pxpipe), theme::dim()));
    let pred = g.prediction_abs_error_sum.checked_div(g.predictions).map_or_else(|| "–".to_string(), |avg| format!("{avg} tok"));
    out.push(Line::styled(format!("  dry runs {} · pred err avg {pred}", g.dryruns), theme::dim()));

    if let Some(graph) = &snap.graph {
        out.push(Line::styled(
            format!("  {} files · {} sym · {} communities", graph.files, graph.symbols, graph.communities),
            theme::dim(),
        ));
    }

    for (name, p) in &snap.proxies {
        if p.up {
            let latency = p.latency_ms.map_or(String::new(), |ms| format!(" {ms}ms"));
            let stats = match &p.stats {
                Some(Value::Object(map)) => {
                    map.iter().filter_map(|(k, v)| v.as_f64().map(|n| format!(" · {k} {n}"))).take(2).collect::<String>()
                }
                _ => String::new(),
            };
            out.push(Line::from(vec![
                Span::styled(format!("  {name} "), theme::dim()),
                Span::styled("● up", theme::OK),
                Span::raw(format!("{latency}{stats}")),
            ]));
        } else {
            out.push(Line::from(vec![Span::styled(format!("  {name} "), theme::dim()), Span::styled("✕ down", theme::ERR)]));
        }
    }

    let total = &snap.total_usage;
    out.push(Line::raw(format!("  saved ≈ {} tok (est.)", fmt_saved(snap.total_saved_est()))));
    if let Some(real) = snap.total_saved_real() {
        out.push(Line::styled(format!("  measured ≈ {} tok (real counts)", fmt_saved(real)), theme::OK));
    }
    if cfg.tui.show_token_metrics {
        out.push(Line::raw(format!(
            "  real in {} · out {} · cache {}",
            fmt_tokens(total.input_tokens),
            fmt_tokens(total.output_tokens),
            fmt_tokens(total.cache_read_input_tokens)
        )));
        out.push(Line::raw(format!("  cost ${:.4}", snap.total_cost_usd)));
    }
    let observed: i64 = snap.stages.values().map(|t| t.observed_saving_est).sum();
    if observed > 0 {
        out.push(Line::styled(format!("  observe: would save {} tok (est.)", fmt_tokens(observed as u64)), theme::dim()));
    }
    out
}

/// Lines of the "Agents" block: one per executor.
fn agent_lines(app: &App) -> Vec<Line<'static>> {
    if app.agents.is_empty() {
        return vec![Line::styled("  (none)", theme::dim())];
    }
    app.agents
        .iter()
        .map(|a| {
            let (icon, color) = match &a.status {
                AgentStatus::Done => ("✓", theme::OK),
                AgentStatus::Failed(_) => ("✕", theme::ERR),
                AgentStatus::Cancelled => ("■", theme::MUTED),
                _ if is_running(&a.status) => (SPINNER[app.spinner % SPINNER.len()], theme::ACCENT),
                _ => ("·", theme::MUTED),
            };
            let short = a.model.strip_prefix("claude-").unwrap_or(&a.model);
            Line::from(vec![
                Span::styled(format!("{icon} "), color),
                Span::raw(format!("{:<12} {:<14} {:>7} ${:.4}", a.domain, short, fmt_tokens(a.tokens), a.cost_usd)),
            ])
        })
        .collect()
}

/// Lines of the "Plan" block.
fn plan_lines(app: &App) -> Vec<Line<'static>> {
    if app.plan().is_empty() {
        return vec![Line::styled("  (no plan)", theme::dim())];
    }
    app.plan()
        .iter()
        .map(|item| {
            let mark = match item.status {
                TodoStatus::Pending => "☐",
                TodoStatus::InProgress => "◐",
                TodoStatus::Completed => "☑",
            };
            Line::raw(format!("{mark} {}", item.content))
        })
        .collect()
}

/// Braille spinner frames.
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_format_matches_spec() {
        assert_eq!(fmt_tokens(12_345), "12.3k");
        assert_eq!(fmt_tokens(1_200_000), "1.2M");
        assert_eq!(fmt_tokens(999), "999");
    }

    #[test]
    fn ratio_and_saved_format() {
        assert_eq!(fmt_ratio(0.61), "-61%");
        assert_eq!(fmt_ratio(-0.1), "+10%");
        assert_eq!(fmt_saved(12_300), "-12.3k");
        assert_eq!(fmt_saved(-1_000), "+1.0k");
    }
}
