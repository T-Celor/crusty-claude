//! Running totals built only from emitted events, so what the UI shows is what happened.

use std::collections::BTreeMap;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::advisor::AdviceSource;
use crate::event::{AdvicePoint, AppEvent, GateInfo, SandboxInfo, Stage, StageStatus};
use crate::message::Usage;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct StageTotals {
    pub runs: u64,
    pub applied: u64,
    pub observed: u64,
    pub skipped: u64,
    pub fallback: u64,
    pub failed: u64,
    pub input_bytes: u64,
    pub output_bytes: u64,
    /// Only counted for applied runs, so the saving reflects what was really sent.
    pub tokens_before_est: u64,
    pub tokens_after_est: u64,
    /// Same for observe-mode runs: what the stage would have saved.
    pub observed_saving_est: i64,
    pub tokens_before_real: u64,
    pub tokens_after_real: u64,
    pub real_samples: u64,
    pub duration_us: u64,
    pub last_transformer: String,
    pub last_detail: String,
}

impl StageTotals {
    pub fn saved_est(&self) -> i64 {
        self.tokens_before_est as i64 - self.tokens_after_est as i64
    }

    /// Estimated share of tokens removed by applied runs, 0.0 to 1.0.
    pub fn ratio_est(&self) -> f64 {
        if self.tokens_before_est == 0 { 0.0 } else { self.saved_est() as f64 / self.tokens_before_est as f64 }
    }

    /// Measured share removed, when real counts exist.
    pub fn ratio_real(&self) -> Option<f64> {
        (self.real_samples > 0 && self.tokens_before_real > 0)
            .then(|| (self.tokens_before_real as f64 - self.tokens_after_real as f64) / self.tokens_before_real as f64)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct GateTotals {
    pub direct: u64,
    pub pxpipe: u64,
    /// Observe mode: decisions for pxpipe that were not applied.
    pub observed_pxpipe: u64,
    pub dryruns: u64,
    pub dryrun_ms_sum: u64,
    pub predictions: u64,
    pub prediction_abs_error_sum: u64,
    pub real_context_direct_sum: u64,
    pub real_context_pxpipe_sum: u64,
    pub last: Option<GateInfo>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelUsage {
    pub calls: u64,
    pub usage: Usage,
    pub cost_usd: f64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ProxyView {
    pub url: String,
    pub up: bool,
    pub latency_ms: Option<u64>,
    pub stats: Option<Value>,
    pub checks: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct GraphView {
    pub files: u64,
    pub symbols: u64,
    pub edges: u64,
    pub communities: u64,
    pub duration_ms: u64,
}

/// Answers of the advisor at one integration point.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AdviceTotals {
    pub laya: u64,
    pub native: u64,
    pub abstained: u64,
    /// `review` or `block` verdicts.
    pub escalations: u64,
    pub latency_ms_sum: u64,
    pub last_outcome: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AdvisorView {
    pub url: String,
    pub up: bool,
    pub latency_ms: Option<u64>,
    pub detail: Option<String>,
    pub points: BTreeMap<AdvicePoint, AdviceTotals>,
    pub last_fallback_reason: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MetricsSnapshot {
    pub stages: BTreeMap<Stage, StageTotals>,
    pub gate: GateTotals,
    /// Keyed by `provider:model`.
    pub usage_by_model: BTreeMap<String, ModelUsage>,
    pub total_usage: Usage,
    pub total_cost_usd: f64,
    pub policy_injections: u64,
    pub policy_label: Option<String>,
    pub proxies: BTreeMap<String, ProxyView>,
    pub graph: Option<GraphView>,
    pub agents_spawned: u64,
    pub model_fallbacks: u64,
    pub tool_calls: u64,
    pub tool_errors: u64,
    pub advisor: AdvisorView,
    /// Distinct values masked per request, summed.
    pub redacted_secrets: u64,
    pub redacted_pii: u64,
    pub sandbox: Option<SandboxInfo>,
    pub checkpoints: u64,
    pub rewinds: u64,
}

impl MetricsSnapshot {
    /// Estimated tokens saved by every applied stage.
    pub fn total_saved_est(&self) -> i64 {
        self.stages.values().map(StageTotals::saved_est).sum()
    }

    /// Real tokens saved, from the stages that have real measurements.
    pub fn total_saved_real(&self) -> Option<i64> {
        let measured: Vec<&StageTotals> = self.stages.values().filter(|t| t.real_samples > 0).collect();
        (!measured.is_empty()).then(|| measured.iter().map(|t| t.tokens_before_real as i64 - t.tokens_after_real as i64).sum())
    }
}

#[derive(Debug, Default)]
pub struct Metrics {
    inner: Mutex<MetricsSnapshot>,
}

impl Metrics {
    pub fn snapshot(&self) -> MetricsSnapshot {
        self.lock().clone()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, MetricsSnapshot> {
        // A panic while holding the lock leaves plain counters; they are still usable.
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn record(&self, event: &AppEvent) {
        let mut s = self.lock();
        match event {
            AppEvent::Optimizer(m) => {
                let t = s.stages.entry(m.stage).or_default();
                t.runs += 1;
                t.input_bytes += m.input_bytes;
                t.output_bytes += m.output_bytes;
                t.duration_us += m.duration_us;
                t.last_transformer.clone_from(&m.transformer);
                t.last_detail.clone_from(&m.detail);
                match m.status {
                    StageStatus::Applied => {
                        t.applied += 1;
                        t.tokens_before_est += m.tokens_before_est;
                        t.tokens_after_est += m.tokens_after_est;
                        if let (Some(b), Some(a)) = (m.tokens_before_real, m.tokens_after_real) {
                            t.tokens_before_real += b;
                            t.tokens_after_real += a;
                            t.real_samples += 1;
                        }
                    }
                    StageStatus::Observed => {
                        t.observed += 1;
                        t.observed_saving_est += m.saved_est();
                    }
                    StageStatus::Skipped => t.skipped += 1,
                    StageStatus::Fallback => t.fallback += 1,
                    StageStatus::Failed => t.failed += 1,
                }
            }
            AppEvent::Gate(g) => {
                match (g.route.as_str(), g.applied) {
                    ("pxpipe", true) => s.gate.pxpipe += 1,
                    ("pxpipe", false) => s.gate.observed_pxpipe += 1,
                    _ => s.gate.direct += 1,
                }
                if let Some(ms) = g.dryrun_ms {
                    s.gate.dryruns += 1;
                    s.gate.dryrun_ms_sum += ms;
                }
                s.gate.last = Some(g.clone());
            }
            AppEvent::MeasuredSaving { stage, tokens_before, tokens_after } => {
                let t = s.stages.entry(*stage).or_default();
                t.tokens_before_real += tokens_before;
                t.tokens_after_real += tokens_after;
                t.real_samples += 1;
            }
            AppEvent::GateFeedback { route, real_context, predicted_after, .. } => {
                if route == "pxpipe" {
                    s.gate.real_context_pxpipe_sum += real_context;
                    if let Some(p) = predicted_after {
                        s.gate.predictions += 1;
                        s.gate.prediction_abs_error_sum += real_context.abs_diff(*p);
                    }
                } else {
                    s.gate.real_context_direct_sum += real_context;
                }
            }
            AppEvent::Usage { provider, model, usage, cost_usd, .. } => {
                let m = s.usage_by_model.entry(format!("{provider}:{model}")).or_default();
                m.calls += 1;
                m.usage.add(usage);
                m.cost_usd += cost_usd;
                s.total_usage.add(usage);
                s.total_cost_usd += cost_usd;
            }
            AppEvent::PolicyApplied { policy, version, level, .. } => {
                s.policy_injections += 1;
                s.policy_label = Some(format!("{policy} {version} ({level})"));
            }
            AppEvent::GraphIndexed { files, symbols, edges, communities, duration_ms } => {
                s.graph = Some(GraphView {
                    files: *files,
                    symbols: *symbols,
                    edges: *edges,
                    communities: *communities,
                    duration_ms: *duration_ms,
                });
            }
            AppEvent::ProxyStatus { name, url, up, latency_ms, stats } => {
                let p = s.proxies.entry(name.clone()).or_default();
                p.url.clone_from(url);
                p.up = *up;
                p.latency_ms = *latency_ms;
                p.checks += 1;
                if stats.is_some() {
                    p.stats.clone_from(stats);
                }
            }
            AppEvent::AdvisorStatus { url, up, latency_ms, detail } => {
                s.advisor.url.clone_from(url);
                s.advisor.up = *up;
                s.advisor.latency_ms = *latency_ms;
                if detail.is_some() {
                    s.advisor.detail.clone_from(detail);
                }
            }
            AppEvent::Advice(a) => {
                if a.fallback_reason.is_some() {
                    s.advisor.last_fallback_reason.clone_from(&a.fallback_reason);
                }
                let t = s.advisor.points.entry(a.point).or_default();
                match a.source {
                    AdviceSource::Laya => t.laya += 1,
                    AdviceSource::Native => t.native += 1,
                    AdviceSource::None => t.abstained += 1,
                }
                if a.outcome == "review" || a.outcome == "block" {
                    t.escalations += 1;
                }
                t.latency_ms_sum += a.latency_ms;
                t.last_outcome.clone_from(&a.outcome);
            }
            AppEvent::Redacted { secrets, pii, .. } => {
                s.redacted_secrets += secrets;
                s.redacted_pii += pii;
            }
            AppEvent::Sandbox(info) => s.sandbox = Some(info.clone()),
            AppEvent::CheckpointCreated { .. } => s.checkpoints += 1,
            AppEvent::Rewound { .. } => s.rewinds += 1,
            AppEvent::AgentSpawned { .. } => s.agents_spawned += 1,
            AppEvent::ModelFallback { .. } => s.model_fallbacks += 1,
            AppEvent::ToolFinished { ok, .. } => {
                s.tool_calls += 1;
                if !ok {
                    s.tool_errors += 1;
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::StageMetrics;

    #[test]
    fn observed_runs_do_not_count_as_savings() {
        let m = Metrics::default();
        let mut s = StageMetrics::new(Stage::Headroom, "request", "headroom:json");
        s.tokens_before_est = 1000;
        s.tokens_after_est = 300;
        s.status = StageStatus::Observed;
        m.record(&AppEvent::Optimizer(s.clone()));
        let snap = m.snapshot();
        assert_eq!(snap.total_saved_est(), 0);
        assert_eq!(snap.stages[&Stage::Headroom].observed_saving_est, 700);
        s.status = StageStatus::Applied;
        m.record(&AppEvent::Optimizer(s));
        assert_eq!(m.snapshot().total_saved_est(), 700);
    }

    #[test]
    fn usage_accumulates_per_model() {
        let m = Metrics::default();
        let usage = Usage { input_tokens: 10, output_tokens: 5, ..Usage::default() };
        for _ in 0..2 {
            m.record(&AppEvent::Usage { agent_id: "a".into(), provider: "p".into(), model: "x".into(), usage, cost_usd: 0.5 });
        }
        let snap = m.snapshot();
        assert_eq!(snap.usage_by_model["p:x"].calls, 2);
        assert_eq!(snap.total_usage.input_tokens, 20);
        assert!((snap.total_cost_usd - 1.0).abs() < 1e-9);
    }

    #[test]
    fn advice_is_counted_per_point_and_source() {
        use crate::event::AdviceInfo;
        let m = Metrics::default();
        let advice = |source, outcome: &str| {
            AppEvent::Advice(AdviceInfo {
                point: AdvicePoint::Command,
                source,
                outcome: outcome.into(),
                confidence: Some(0.9),
                latency_ms: 10,
                fallback_reason: None,
            })
        };
        m.record(&advice(AdviceSource::Laya, "block"));
        m.record(&advice(AdviceSource::Native, "allow"));
        let t = m.snapshot().advisor.points[&AdvicePoint::Command].clone();
        assert_eq!((t.laya, t.native, t.escalations, t.latency_ms_sum), (1, 1, 1, 20));
    }

    #[test]
    fn real_savings_only_from_measured_stages() {
        let m = Metrics::default();
        assert_eq!(m.snapshot().total_saved_real(), None);
        m.record(&AppEvent::MeasuredSaving { stage: Stage::Headroom, tokens_before: 1000, tokens_after: 600 });
        assert_eq!(m.snapshot().total_saved_real(), Some(400));
    }

    #[test]
    fn gate_feedback_tracks_prediction_error() {
        let m = Metrics::default();
        m.record(&AppEvent::GateFeedback { session: "s".into(), route: "pxpipe".into(), real_context: 900, predicted_after: Some(1000) });
        let g = m.snapshot().gate;
        assert_eq!((g.predictions, g.prediction_abs_error_sum), (1, 100));
    }
}
