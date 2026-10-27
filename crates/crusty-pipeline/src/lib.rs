//! The request optimization pipeline, in order: Headroom (compress old tool output) →
//! Context Gate (gain function: is imaging worth it?) → pxpipe (image the bulky old
//! context, only when the gate says so). Tool-output filters (RTK, HTML→Markdown) run
//! when tools produce output, through the same object.
//!
//! Every stage emits a `StageMetrics` event; estimates are labelled as estimates and real
//! token counts come only from the provider (usage, and `count_tokens` when enabled).

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use crusty_core::advisor::{Advisor, NoAdvisor};
use crusty_core::config::{Config, LayaConfig, OptimizerConfig, OptimizerMode};
use crusty_core::event::{AppEvent, Stage, StageMetrics, StageStatus};
use crusty_core::filter::{ContentFilter, Filtered, HtmlDoc};
use crusty_core::tokens::estimate_request;
use crusty_core::tool::ToolOutput;
use crusty_core::vault::SecretVault;
use crusty_core::{
    ChatRequest, EventSink, OptimizeContext, OptimizeOutcome, OriginalStore, Provider, ProviderKind, RequestOptimizer, Route, Usage,
    async_trait,
};
use crusty_gate::{ContextGate, DryRun, GateInput, GateRoute, PreDecision, PxpipeRenderer, PxpipeResult};
use crusty_optimizer::{Headroom, convert_html, filter_command};

use crusty_dlp::redact_request;
use prune::PruneSessions;

/// Stages this pipeline can switch at runtime.
const SWITCHABLE: [Stage; 9] = [
    Stage::Rtk,
    Stage::HtmlToMarkdown,
    Stage::Triage,
    Stage::Rerank,
    Stage::Dlp,
    Stage::Prune,
    Stage::Headroom,
    Stage::ContextGate,
    Stage::Pxpipe,
];

pub struct Pipeline {
    cfg: OptimizerConfig,
    mode: Mutex<OptimizerMode>,
    switches: BTreeMap<Stage, AtomicBool>,
    headroom: Headroom,
    gate: ContextGate,
    pxpipe: PxpipeRenderer,
    events: EventSink,
    store: Arc<OriginalStore>,
    /// Providers used for `count_tokens` measurements.
    counters: RwLock<BTreeMap<String, Arc<dyn Provider>>>,
    /// Laya settings of the DLP, prune, triage and rerank stages.
    laya: LayaConfig,
    /// Judges ambiguous secrets, relevance and error blocks; abstains by default.
    advisor: Arc<dyn Advisor>,
    /// Value to placeholder table of the DLP stage.
    vault: Arc<SecretVault>,
    prune: Mutex<PruneSessions>,
}

fn share_saved(before: u64, after: u64) -> f64 {
    if before == 0 { 0.0 } else { 1.0 - after as f64 / before as f64 }
}

impl Pipeline {
    pub fn new(config: &Config, events: EventSink, store: Arc<OriginalStore>) -> Self {
        let cfg = config.optimizer.clone();
        let switches = SWITCHABLE
            .into_iter()
            .map(|s| {
                let on = match s {
                    Stage::Rtk => cfg.rtk.enabled,
                    Stage::HtmlToMarkdown => cfg.html_to_markdown.enabled,
                    Stage::Headroom => cfg.headroom.enabled,
                    Stage::ContextGate => cfg.context_gate.enabled,
                    Stage::Dlp => config.laya.dlp.enabled,
                    Stage::Prune => config.laya.pruner.enabled,
                    Stage::Triage => config.laya.triage.enabled,
                    Stage::Rerank => config.laya.rerank.enabled,
                    _ => cfg.pxpipe.enabled,
                };
                (s, AtomicBool::new(on))
            })
            .collect();
        Pipeline {
            mode: Mutex::new(cfg.mode),
            switches,
            headroom: Headroom::new(&cfg.headroom),
            gate: ContextGate::new(cfg.context_gate.clone()),
            pxpipe: PxpipeRenderer::new(cfg.pxpipe.clone()),
            events,
            store,
            counters: RwLock::new(BTreeMap::new()),
            laya: config.laya.clone(),
            advisor: Arc::new(NoAdvisor),
            vault: Arc::new(SecretVault::default()),
            prune: Mutex::new(PruneSessions::default()),
            cfg,
        }
    }

    /// Uses `advisor` for DLP ambiguity, prune relevance, triage and rerank. The default
    /// abstains, so every stage then keeps its native rule.
    pub fn with_advisor(mut self, advisor: Arc<dyn Advisor>) -> Self {
        self.advisor = advisor;
        self
    }

    /// Uses `vault` for DLP placeholders, so the engine can restore values in tool inputs.
    pub fn with_vault(mut self, vault: Arc<SecretVault>) -> Self {
        self.vault = vault;
        self
    }

    /// Providers whose counting endpoint measures the real size of original requests.
    pub fn set_counters(&self, providers: BTreeMap<String, Arc<dyn Provider>>) {
        *self.counters.write().unwrap_or_else(|e| e.into_inner()) = providers;
    }

    /// The current mode: active rewrites requests, observe only measures them.
    pub fn mode(&self) -> OptimizerMode {
        *self.mode.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn on(&self, stage: Stage) -> bool {
        self.cfg.enabled && self.switches.get(&stage).is_some_and(|s| s.load(Ordering::Relaxed))
    }

    fn metric(&self, stage: Stage, kind: &str, transformer: &str, ctx: &OptimizeContext) -> StageMetrics {
        let mut m = StageMetrics::new(stage, kind, transformer);
        m.session_id.clone_from(&ctx.session_id);
        m.agent_id.clone_from(&ctx.agent_id);
        m
    }

    fn emit(&self, out: &mut OptimizeOutcome, m: StageMetrics) {
        self.events.emit(AppEvent::Optimizer(m.clone()));
        out.stages.push(m);
    }

    fn headroom_stage(&self, req: &mut ChatRequest, ctx: &OptimizeContext, mode: OptimizerMode, out: &mut OptimizeOutcome) {
        let started = Instant::now();
        let mut candidate = req.clone();
        let report = self.headroom.compress(&mut candidate, &ctx.session_id, &self.store);
        let transformer = if report.transforms.is_empty() { "headroom:none".to_owned() } else { report.transforms.join("+") };
        let mut m = self.metric(Stage::Headroom, "request", &transformer, ctx);
        m.tokens_before_est = report.tokens_before;
        m.tokens_after_est = report.tokens_after;
        m.input_bytes = report.tokens_before * 4;
        m.output_bytes = report.tokens_after * 4;
        let gain = share_saved(report.tokens_before, report.tokens_after);
        if report.blocks_compressed + report.duplicates_removed == 0 {
            m.detail = format!("nothing old enough to compress (frozen boundary {})", report.frozen_boundary);
        } else if gain < self.cfg.minimum_estimated_gain {
            m.detail = format!("gain {:.0}% below minimum", gain * 100.0);
        } else if mode == OptimizerMode::Observe {
            m.status = StageStatus::Observed;
            m.detail = format!("observe: would save {:.0}% ({} blocks)", gain * 100.0, report.blocks_compressed);
        } else {
            m.status = StageStatus::Applied;
            m.detail = format!("-{:.0}% · {} blocks · {} duplicates", gain * 100.0, report.blocks_compressed, report.duplicates_removed);
            *req = candidate;
            req.tolerate_history_edits();
        }
        m.duration_us = started.elapsed().as_micros() as u64;
        self.emit(out, m);
    }

    /// DLP stage, first in the pipeline. It is a privacy control: it runs even when the
    /// optimizer is disabled or in observe mode, and only its switch governs it. The proxy
    /// agent is skipped because it cannot restore placeholders in the answer it forwards to
    /// an external client, which would then see them.
    async fn dlp_stage(&self, req: &mut ChatRequest, ctx: &OptimizeContext, out: &mut OptimizeOutcome) {
        let switched_on = self.switches.get(&Stage::Dlp).is_some_and(|s| s.load(Ordering::Relaxed));
        if ctx.agent_id == "proxy" || !switched_on {
            return;
        }
        let started = Instant::now();
        let report = redact_request(req, &self.vault, self.advisor.as_ref(), &self.laya.dlp).await;
        let masked = report.secrets + report.pii;
        let mut m = self.metric(Stage::Dlp, "request", "dlp:mask", ctx);
        m.status = if masked > 0 { StageStatus::Applied } else { StageStatus::Skipped };
        m.detail =
            format!("{} secrets · {} personal values masked · {} ambiguous checked", report.secrets, report.pii, report.ambiguous_checked);
        m.duration_us = started.elapsed().as_micros() as u64;
        if masked > 0 {
            self.events.emit(AppEvent::Redacted { agent_id: ctx.agent_id.clone(), secrets: report.secrets, pii: report.pii });
        }
        self.emit(out, m);
    }

    async fn render(&self, req: &ChatRequest) -> Result<PxpipeResult, String> {
        let renderer = self.pxpipe.clone();
        let snapshot = req.clone();
        let budget = Duration::from_millis(self.cfg.context_gate.dryrun_ms.max(1));
        match tokio::time::timeout(budget, tokio::task::spawn_blocking(move || renderer.transform(&snapshot))).await {
            Ok(Ok(r)) => Ok(r),
            Ok(Err(e)) => Err(format!("renderer failed: {e}")),
            Err(_) => Err(format!("dry run exceeded {} ms", budget.as_millis())),
        }
    }

    async fn gate_and_pxpipe(&self, req: &mut ChatRequest, ctx: &OptimizeContext, mode: OptimizerMode, out: &mut OptimizeOutcome) {
        let started = Instant::now();
        let est = estimate_request(req, true);
        let px_on = self.on(Stage::Pxpipe);
        let allowed = px_on
            && ctx.supports_images
            && ctx.provider_kind == ProviderKind::Anthropic
            && self.cfg.pxpipe.allowed_models.iter().any(|m| m == &ctx.model);
        let window = self.cfg.context_gate.window.unwrap_or(ctx.context_window).max(1);
        let input = GateInput { session: &ctx.session_id, est, window, pxpipe_allowed: allowed };
        let mut dry_result = None;
        let decision = match self.gate.pre_decide(&input, Instant::now()) {
            PreDecision::Final(d) => d,
            PreDecision::NeedDryRun { measure, zone } => {
                let t0 = Instant::now();
                // The dry run uses the in-process renderer, also when pxpipe runs as an external
                // proxy: it estimates what imaging would save on this exact request.
                let dry = match self.render(req).await {
                    Ok(r) => {
                        let d = DryRun::Measured {
                            saving: r.saving_est,
                            verbatim_risk: r.verbatim_risk,
                            applied: r.applied,
                            reason: r.reason.clone(),
                        };
                        dry_result = Some(r);
                        d
                    }
                    Err(e) => DryRun::Failed(e),
                };
                self.gate.finish(&input, measure, zone, dry, Some(t0.elapsed().as_millis() as u64), Instant::now())
            }
        };
        let to_pxpipe = decision.route == GateRoute::Pxpipe;
        let applied = !(to_pxpipe && mode == OptimizerMode::Observe);
        let info = decision.to_info(&ctx.session_id, &ctx.agent_id, &self.cfg.context_gate.policy, applied);
        self.events.emit(AppEvent::Gate(info.clone()));
        out.gate = Some(info);
        let mut m = self.metric(Stage::ContextGate, "request", &format!("gate:{}", self.cfg.context_gate.policy), ctx);
        // The gate itself changes nothing: it counts as applied only when it sends the request
        // to pxpipe.
        m.status = match (to_pxpipe, applied) {
            (true, true) => StageStatus::Applied,
            (true, false) => StageStatus::Observed,
            (false, _) => StageStatus::Skipped,
        };
        m.tokens_before_est = est;
        m.tokens_after_est = est;
        m.detail = format!("{} · {}{}", decision.route, decision.reason, decision.zone.map(|z| format!(" · zone {z}")).unwrap_or_default());
        m.duration_us = started.elapsed().as_micros() as u64;
        self.emit(out, m);
        if !to_pxpipe {
            return;
        }

        let started = Instant::now();
        let external = self.cfg.pxpipe.mode == "external";
        let mut m = self.metric(Stage::Pxpipe, "request", if external { "pxpipe:external" } else { "pxpipe:native" }, ctx);
        m.tokens_before_est = est;
        if mode == OptimizerMode::Observe {
            let saving = decision.saving.unwrap_or(0).max(0) as u64;
            m.status = StageStatus::Observed;
            m.tokens_after_est = est.saturating_sub(saving);
            m.detail = "observe: gate chose pxpipe, text sent".into();
        } else if external {
            out.route = Route::Via { name: "pxpipe".into(), base_url: self.cfg.pxpipe.external_url.clone() };
            m.status = StageStatus::Applied;
            m.tokens_after_est = est;
            m.detail = format!("routed to external pxpipe at {} (its savings are measured by pxpipe)", self.cfg.pxpipe.external_url);
        } else {
            let result = match dry_result {
                Some(r) => Ok(r),
                None => self.render(req).await,
            };
            match result {
                Ok(r) if r.applied => {
                    m.status = StageStatus::Applied;
                    m.detail = format!("{} pages · {} blocks imaged · verbatim risk {:.3}", r.pages, r.blocks_imaged, r.verbatim_risk);
                    *req = r.request;
                    req.tolerate_history_edits();
                    m.tokens_after_est = estimate_request(req, true);
                    self.gate.record_pxpipe_success();
                }
                Ok(r) => {
                    m.tokens_after_est = est;
                    m.detail = format!("not applied: {}", r.reason);
                }
                Err(e) => {
                    self.gate.record_pxpipe_failure(&ctx.session_id, Instant::now());
                    m.status = StageStatus::Fallback;
                    m.tokens_after_est = est;
                    m.detail = format!("{e}; text sent");
                }
            }
        }
        m.input_bytes = est * 4;
        m.output_bytes = m.tokens_after_est * 4;
        m.duration_us = started.elapsed().as_micros() as u64;
        self.emit(out, m);
    }
}

#[async_trait]
impl RequestOptimizer for Pipeline {
    async fn optimize(&self, req: &mut ChatRequest, ctx: &OptimizeContext) -> OptimizeOutcome {
        let mut out = OptimizeOutcome::default();
        self.dlp_stage(req, ctx, &mut out).await;
        let est_before = estimate_request(req, true);
        out.est_before = est_before;
        out.est_after = est_before;
        if !self.cfg.enabled {
            return out;
        }
        let mode = self.mode();
        if self.cfg.measure_with_count_tokens && ctx.provider_kind == ProviderKind::Anthropic {
            let counter = self.counters.read().unwrap_or_else(|e| e.into_inner()).get(&ctx.provider_id).cloned();
            if let Some(p) = counter {
                out.real_before = p.count_tokens(req).await;
            }
        }
        if self.on(Stage::Prune) {
            self.prune_stage(req, ctx, mode, &mut out).await;
        }
        if self.on(Stage::Headroom) {
            if self.cfg.headroom.mode == "external" {
                let url = self.cfg.headroom.external_url.clone();
                let mut m = self.metric(Stage::Headroom, "request", "headroom:external", ctx);
                m.tokens_before_est = est_before;
                m.tokens_after_est = est_before;
                if mode == OptimizerMode::Observe {
                    m.status = StageStatus::Observed;
                    m.detail = format!("observe: would send through external Headroom at {url}");
                    self.emit(&mut out, m);
                } else {
                    m.status = StageStatus::Applied;
                    m.detail = format!(
                        "sent through external Headroom at {url} (its savings are measured by Headroom; chain the gate after it with `crusty proxy`)"
                    );
                    self.emit(&mut out, m);
                    out.route = Route::Via { name: "headroom".into(), base_url: url };
                    return out;
                }
            } else {
                self.headroom_stage(req, ctx, mode, &mut out);
            }
        }
        if self.on(Stage::ContextGate) {
            self.gate_and_pxpipe(req, ctx, mode, &mut out).await;
        }
        out.est_after = estimate_request(req, true);
        out
    }

    async fn after_tool(&self, tool: &str, _input: &serde_json::Value, output: &mut ToolOutput, goal: &str, ctx: &OptimizeContext) {
        self.shorten_tool_output(tool, output, goal, ctx).await;
    }

    fn forget_session(&self, session_id: &str) {
        self.prune.lock().unwrap_or_else(|e| e.into_inner()).forget(session_id);
    }

    fn record_usage(&self, ctx: &OptimizeContext, outcome: &OptimizeOutcome, usage: &Usage) {
        let real = usage.total_input();
        if real == 0 {
            return;
        }
        if let Some(g) = &outcome.gate {
            // Imaged context reached the model when the native renderer applied, or when the
            // request really went through the external pxpipe route.
            let external = self.cfg.pxpipe.mode == "external";
            let imaged = g.route == "pxpipe" && g.applied && (!external || matches!(outcome.route, Route::Via { .. }));
            let route = if imaged { GateRoute::Pxpipe } else { GateRoute::Direct };
            let predicted = g.predicted_after.filter(|_| route == GateRoute::Pxpipe);
            self.gate.record_usage(&ctx.session_id, route, g.est, real, predicted);
            self.events.emit(AppEvent::GateFeedback {
                session: ctx.session_id.clone(),
                route: route.to_string(),
                real_context: real,
                predicted_after: predicted,
            });
        }
        if let Some(before) = outcome.real_before {
            // The measurement covers the whole pipeline; with several stages it is shared out
            // in proportion to each stage's estimated saving.
            let changed: Vec<(Stage, i64)> = outcome
                .stages
                .iter()
                .filter(|m| m.status == StageStatus::Applied && m.saved_est() > 0)
                .map(|m| (m.stage, m.saved_est()))
                .collect();
            let total_est: i64 = changed.iter().map(|(_, s)| s).sum();
            let saved = before as f64 - real as f64;
            for (stage, est) in changed {
                let share = est as f64 / total_est as f64;
                let tokens_before = (before as f64 * share).round() as u64;
                let tokens_after = (tokens_before as f64 - saved * share).max(0.0).round() as u64;
                self.events.emit(AppEvent::MeasuredSaving { stage, tokens_before, tokens_after });
            }
        }
    }

    fn record_route_failure(&self, ctx: &OptimizeContext, outcome: &OptimizeOutcome) {
        if let Route::Via { name, base_url } = &outcome.route {
            if name == "pxpipe" {
                self.gate.record_pxpipe_failure(&ctx.session_id, Instant::now());
            }
            let stage = if name == "pxpipe" { Stage::Pxpipe } else { Stage::Headroom };
            let mut m = self.metric(stage, "request", &format!("{name}:external"), ctx);
            m.status = StageStatus::Fallback;
            m.detail = format!("{base_url} unreachable; sent direct");
            self.events.emit(AppEvent::Optimizer(m));
        }
    }

    fn set_stage_enabled(&self, stage: Stage, enabled: bool) -> Result<(), String> {
        match self.switches.get(&stage) {
            Some(s) => {
                s.store(enabled, Ordering::Relaxed);
                Ok(())
            }
            None => Err(match stage {
                Stage::Ponytail => "Ponytail is a policy: use /ponytail off".into(),
                Stage::Graphify => "the code graph is configured with optimizer.graphify; query it with /graph".into(),
                _ => "proxies are managed with /proxy".into(),
            }),
        }
    }

    fn set_mode(&self, mode: OptimizerMode) -> Result<(), String> {
        *self.mode.lock().unwrap_or_else(|e| e.into_inner()) = mode;
        Ok(())
    }

    fn describe(&self) -> String {
        if !self.cfg.enabled {
            return "optimizer: disabled (no stage runs; requests are sent as they are)".into();
        }
        let flag = |s: Stage| if self.on(s) { "on" } else { "off" };
        format!(
            "optimizer: {:?} · dlp {} · prune {} · triage {} · rerank {} · rtk {} · html→md {} · headroom {} ({}) · gate {} ({} policy) · pxpipe {} ({}) · count_tokens {}",
            self.mode(),
            flag(Stage::Dlp),
            flag(Stage::Prune),
            flag(Stage::Triage),
            flag(Stage::Rerank),
            flag(Stage::Rtk),
            flag(Stage::HtmlToMarkdown),
            flag(Stage::Headroom),
            self.cfg.headroom.mode,
            flag(Stage::ContextGate),
            self.cfg.context_gate.policy,
            flag(Stage::Pxpipe),
            self.cfg.pxpipe.mode,
            if self.cfg.measure_with_count_tokens { "on" } else { "off" }
        )
    }
}

impl ContentFilter for Pipeline {
    fn command_output(&self, command: &str, output: &str, exit_code: Option<i32>) -> Option<Filtered> {
        if !self.on(Stage::Rtk) {
            return None;
        }
        filter_command(command, output, exit_code, &self.cfg.rtk)
    }

    fn html_to_markdown(&self, html: &str, url: &str) -> Option<HtmlDoc> {
        // Conversion is needed to read pages at all, so it ignores optimizer.enabled.
        if !self.switches.get(&Stage::HtmlToMarkdown).is_some_and(|s| s.load(Ordering::Relaxed)) {
            return None;
        }
        convert_html(html, url, self.cfg.html_to_markdown.max_chars)
    }
}

mod prune;
mod tool_output;

#[cfg(test)]
mod tests;
