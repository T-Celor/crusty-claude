//! The Laya advisor: asks `laya-serve` first and falls back to the native advisor when Laya
//! is off, unreachable, unsure or unreadable.
//!
//! Verdicts only ever harden: the more severe of Laya and native wins. The one exception is
//! `auto_approve`, where Laya can only turn an allow into a review, never the reverse.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use crusty_core::advisor::{
    AdviceSource, Advisor, Intent, IntentKind, Judgement, McpServerSummary, Routing, Scores, SecretCandidate, Tier, Verdict,
};
use crusty_core::config::LayaConfig;
use crusty_core::event::{AdviceInfo, AdvicePoint, AppEvent, EventSink};
use crusty_core::permission::ApprovalRequest;
use crusty_core::tool::PlannedChange;
use futures::StreamExt;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::client::{Answer, Answers, LayaClient, LayaError, Question, QuestionKind, elapsed_ms};

/// Requests in flight at once for per-item questions.
const MAX_CONCURRENT: usize = 8;
/// Question count per request the server accepts.
const MAX_QUESTIONS: usize = 64;
const VERDICT_OPTIONS: [&str; 3] = ["allow", "review", "block"];
const RELEVANCE_LEVELS: [&str; 4] = ["irrelevant", "weakly relevant", "relevant", "highly relevant"];
const ERROR_LEVELS: [&str; 4] = ["useless", "weak", "useful", "essential"];
const SERVER_LEVELS: [&str; 3] = ["irrelevant", "weakly relevant", "needed"];

/// Laya-first advisor. Built by [`build_advisor`] or directly with [`LayaAdvisor::new`].
pub struct LayaAdvisor {
    client: LayaClient,
    native: Arc<dyn Advisor>,
    cfg: LayaConfig,
    events: EventSink,
    up: AtomicBool,
}

impl LayaAdvisor {
    /// Builds the advisor, reading the API key from `cfg.api_key_env`. It starts optimistic:
    /// Laya is assumed up until a call or the health monitor says otherwise.
    pub fn new(cfg: &LayaConfig, native: Arc<dyn Advisor>, http: reqwest::Client, events: EventSink) -> Self {
        Self::with_client(LayaClient::new(cfg, http), native, cfg.clone(), events)
    }

    /// Builds the advisor around an existing client.
    pub fn with_client(client: LayaClient, native: Arc<dyn Advisor>, cfg: LayaConfig, events: EventSink) -> Self {
        Self { client, native, cfg, events, up: AtomicBool::new(true) }
    }

    /// Whether Laya is currently believed reachable.
    pub fn is_up(&self) -> bool {
        self.up.load(Ordering::Relaxed)
    }

    /// Probes `/health` once, updates the up flag and reports the result as an
    /// `AdvisorStatus` event.
    pub async fn probe(&self) {
        let url = self.client.url().to_string();
        let event = match self.client.health().await {
            Ok(h) => {
                self.up.store(true, Ordering::Relaxed);
                AppEvent::AdvisorStatus { url, up: true, latency_ms: Some(h.latency_ms), detail: h.detail }
            }
            Err(e) => {
                self.up.store(false, Ordering::Relaxed);
                AppEvent::AdvisorStatus { url, up: false, latency_ms: None, detail: Some(e.to_string()) }
            }
        };
        self.events.emit(event);
    }

    /// `Ok` when Laya may be asked now.
    fn available(&self) -> Result<(), String> {
        if !self.cfg.enabled {
            return Err("laya disabled".into());
        }
        if !self.is_up() {
            return Err("laya unreachable".into());
        }
        Ok(())
    }

    /// Asks Laya. A transport failure marks it down; an unreadable answer does not.
    async fn query(&self, state: &str, questions: &[Question]) -> Result<Answers, String> {
        self.available()?;
        self.client.ask(state, questions).await.map_err(|e| {
            if !matches!(e, LayaError::Parse(_)) {
                self.up.store(false, Ordering::Relaxed);
            }
            e.to_string()
        })
    }

    /// The answer to one question, or the reason there is none.
    async fn answer(&self, state: &str, question: &Question) -> Result<Answer, String> {
        let mut answers = self.query(state, std::slice::from_ref(question)).await?;
        match answers.remove(&question.id) {
            Some(result) => result.map_err(|e| e.to_string()),
            None => Err("no answer".into()),
        }
    }

    /// A choice answer that is confident enough, or the reason it is not usable.
    async fn choice(&self, state: &str, question: &Question) -> Result<(String, f64), String> {
        match self.answer(state, question).await? {
            Answer::Choice { choice, confidence } if confidence >= self.cfg.min_confidence => Ok((choice, confidence)),
            Answer::Choice { confidence, .. } => Err(format!("low confidence {confidence:.2}")),
            other => Err(format!("unexpected answer {other:?}")),
        }
    }

    /// One answer per item, asked at most [`MAX_CONCURRENT`] at a time. Fails on the first item
    /// without an answer.
    async fn answers_each(&self, items: Vec<(String, Question)>) -> Result<Vec<Answer>, String> {
        let results: Vec<Result<Answer, String>> = futures::stream::iter(items)
            .map(|(state, q)| async move { self.answer(&state, &q).await })
            .buffered(MAX_CONCURRENT)
            .collect()
            .await;
        results.into_iter().collect()
    }

    /// Values in `[0, 1]` for each item: a noul probability, or a score scaled by `levels`.
    async fn unit_values(&self, items: Vec<(String, Question)>, levels: usize) -> Result<Vec<f64>, String> {
        let answers = self.answers_each(items).await?;
        answers.iter().map(|a| unit(a, levels).ok_or_else(|| format!("unexpected answer {a:?}"))).collect()
    }

    /// Reports one advisor call to the UI and telemetry.
    fn report(
        &self,
        point: AdvicePoint,
        source: AdviceSource,
        outcome: impl Into<String>,
        confidence: Option<f64>,
        started: Instant,
        fallback_reason: Option<String>,
    ) {
        self.events.emit(AppEvent::Advice(AdviceInfo {
            point,
            source,
            outcome: outcome.into(),
            confidence,
            latency_ms: elapsed_ms(started),
            fallback_reason,
        }));
    }

    /// Joins Laya's verdict on `question` to the native judgement: the more severe wins.
    async fn judge(&self, point: AdvicePoint, state: &str, question: &Question, native: Judgement, started: Instant) -> Judgement {
        let laya = self.choice(state, question).await.and_then(|(choice, confidence)| {
            parse_verdict(&choice).map(|v| (v, confidence)).ok_or_else(|| format!("unknown verdict {choice}"))
        });
        match laya {
            Ok((verdict, confidence)) if verdict > native.verdict => {
                self.report(point, AdviceSource::Laya, verdict.to_string(), Some(confidence), started, None);
                Judgement::new(verdict, confidence, format!("laya judged {verdict}"), AdviceSource::Laya)
            }
            Ok((verdict, _)) => {
                let reason = format!("laya {verdict} is not stricter than native");
                self.report_native(point, &native, started, Some(reason));
                native
            }
            Err(reason) => {
                self.report_native(point, &native, started, Some(reason));
                native
            }
        }
    }

    /// Reports a native judgement that decided the call.
    fn report_native(&self, point: AdvicePoint, native: &Judgement, started: Instant, reason: Option<String>) {
        self.report(point, native.source, native.verdict.to_string(), Some(native.confidence), started, reason);
    }

    /// Per-item scores: Laya's values when every item was answered, otherwise native's. With
    /// `harden`, each item keeps the higher of the two (used for secret detection).
    async fn scored(
        &self,
        point: AdvicePoint,
        started: Instant,
        native: Scores,
        items: Vec<(String, Question)>,
        levels: usize,
        harden: bool,
    ) -> Scores {
        let count = items.len();
        match self.unit_values(items, levels).await {
            Ok(laya) => {
                let values = match native.for_items(count) {
                    Some(n) if harden => laya.iter().zip(n.iter()).map(|(l, n)| l.max(*n)).collect(),
                    _ => laya,
                };
                self.report(point, AdviceSource::Laya, format!("{count} scored"), None, started, None);
                Scores { values, source: AdviceSource::Laya }
            }
            Err(reason) => {
                self.report(point, native.source, format!("{count} scored"), None, started, Some(reason));
                native
            }
        }
    }
}

/// Builds the advisor. When Laya is disabled the native advisor is wrapped in
/// [`ObservedAdvisor`], so native decisions still reach the UI. The caller starts the health
/// monitor with [`spawn_health_monitor`] when it wants to use [`LayaAdvisor`] directly; this
/// function hands back the trait object only.
pub fn build_advisor(cfg: &LayaConfig, native: Arc<dyn Advisor>, http: reqwest::Client, events: EventSink) -> Arc<dyn Advisor> {
    if !cfg.enabled {
        return Arc::new(ObservedAdvisor::new(native, events));
    }
    Arc::new(LayaAdvisor::new(cfg, native, http, events))
}

/// Forwards every call to the native advisor and reports each decision as an
/// `AppEvent::Advice` with source `native`, the same way [`LayaAdvisor`] reports its own.
pub struct ObservedAdvisor {
    inner: Arc<dyn Advisor>,
    events: EventSink,
}

impl ObservedAdvisor {
    /// Wraps `inner`, reporting its decisions to `events`.
    pub fn new(inner: Arc<dyn Advisor>, events: EventSink) -> Self {
        Self { inner, events }
    }

    fn report(&self, point: AdvicePoint, outcome: String, confidence: Option<f64>, started: Instant) {
        self.events.emit(AppEvent::Advice(AdviceInfo {
            point,
            source: AdviceSource::Native,
            outcome,
            confidence,
            latency_ms: elapsed_ms(started),
            fallback_reason: None,
        }));
    }
}

#[async_trait]
impl Advisor for ObservedAdvisor {
    fn name(&self) -> &str {
        "native"
    }

    async fn judge_command(&self, command: &str, cwd: &Path, goal: &str) -> Judgement {
        let started = Instant::now();
        let j = self.inner.judge_command(command, cwd, goal).await;
        self.report(AdvicePoint::Command, j.verdict.to_string(), Some(j.confidence), started);
        j
    }

    async fn judge_write(&self, change: &PlannedChange, goal: &str) -> Judgement {
        let started = Instant::now();
        let j = self.inner.judge_write(change, goal).await;
        self.report(AdvicePoint::Write, j.verdict.to_string(), Some(j.confidence), started);
        j
    }

    async fn secret_probability(&self, candidates: &[SecretCandidate]) -> Scores {
        let started = Instant::now();
        let s = self.inner.secret_probability(candidates).await;
        self.report(AdvicePoint::Secret, format!("{} scored", s.values.len()), None, started);
        s
    }

    async fn relevance(&self, query: &str, items: &[String]) -> Scores {
        let started = Instant::now();
        let s = self.inner.relevance(query, items).await;
        self.report(AdvicePoint::Rerank, format!("{} scored", s.values.len()), None, started);
        s
    }

    async fn error_scores(&self, blocks: &[String]) -> Scores {
        let started = Instant::now();
        let s = self.inner.error_scores(blocks).await;
        self.report(AdvicePoint::Triage, format!("{} scored", s.values.len()), None, started);
        s
    }

    async fn route(&self, prompt: &str) -> Routing {
        let started = Instant::now();
        let r = self.inner.route(prompt).await;
        self.report(AdvicePoint::Router, r.tier.as_str().to_string(), Some(r.confidence), started);
        r
    }

    async fn intent(&self, prompt: &str, servers: &[McpServerSummary]) -> Intent {
        let started = Instant::now();
        let i = self.inner.intent(prompt, servers).await;
        self.report(AdvicePoint::Intent, intent_name(i.kind).to_string(), Some(i.confidence), started);
        i
    }

    async fn auto_approve(&self, request: &ApprovalRequest, goal: &str) -> Judgement {
        let started = Instant::now();
        let j = self.inner.auto_approve(request, goal).await;
        self.report(AdvicePoint::Auto, j.verdict.to_string(), Some(j.confidence), started);
        j
    }
}

/// Probes `/health` every `cfg.health_interval_secs` (first probe immediately) until `cancel`
/// fires.
pub fn spawn_health_monitor(advisor: Arc<LayaAdvisor>, cancel: CancellationToken) -> JoinHandle<()> {
    tokio::spawn(async move {
        let period = Duration::from_secs(advisor.cfg.health_interval_secs.max(1));
        let mut ticker = tokio::time::interval(period);
        loop {
            tokio::select! {
                () = cancel.cancelled() => break,
                _ = ticker.tick() => advisor.probe().await,
            }
        }
    })
}

/// The verdict question shared by the command and write guards.
fn verdict_question(instructions: &str) -> Question {
    Question {
        id: "verdict".into(),
        kind: QuestionKind::Choice(VERDICT_OPTIONS.iter().map(|s| s.to_string()).collect()),
        instructions: instructions.into(),
    }
}

/// `allow`, `review` or `block`, case-insensitively.
fn parse_verdict(s: &str) -> Option<Verdict> {
    match s.trim().to_ascii_lowercase().as_str() {
        "allow" => Some(Verdict::Allow),
        "review" => Some(Verdict::Review),
        "block" => Some(Verdict::Block),
        _ => None,
    }
}

fn intent_name(kind: IntentKind) -> &'static str {
    match kind {
        IntentKind::Question => "question",
        IntentKind::Action => "action",
        IntentKind::Mixed => "mixed",
    }
}

/// A value in `[0, 1]`: a noul probability, or a score divided by the top level index.
/// `None` for a choice answer or a score with fewer than two levels.
fn unit(answer: &Answer, levels: usize) -> Option<f64> {
    match answer {
        Answer::Noul(p) => Some(p.clamp(0.0, 1.0)),
        Answer::Score(v) if levels >= 2 => Some((v / (levels - 1) as f64).clamp(0.0, 1.0)),
        _ => None,
    }
}

const COMMAND_RULES: &str = "Is this shell command safe to run for the user's goal? \
    allow: routine and clearly needed; review: destructive, networked or unclear; \
    block: deletes data outside the project, sends secrets out, or escalates privileges.";

const WRITE_RULES: &str = "Is this change to a sensitive file (environment, ignore rules, CI/CD, \
    container files) safe for the user's goal? allow: harmless and expected; review: changes \
    behaviour or access; block: adds credentials, weakens security or runs untrusted code.";

#[async_trait]
impl Advisor for LayaAdvisor {
    fn name(&self) -> &str {
        "laya"
    }

    async fn judge_command(&self, command: &str, cwd: &Path, goal: &str) -> Judgement {
        let started = Instant::now();
        let native = self.native.judge_command(command, cwd, goal).await;
        let state = format!("Shell command: {command}\nWorking directory: {}\nUser goal: {goal}", cwd.display());
        self.judge(AdvicePoint::Command, &state, &verdict_question(COMMAND_RULES), native, started).await
    }

    async fn judge_write(&self, change: &PlannedChange, goal: &str) -> Judgement {
        let started = Instant::now();
        let native = self.native.judge_write(change, goal).await;
        let state = format!(
            "File: {}\nUser goal: {goal}\n--- before\n{}\n--- after\n{}",
            change.path.display(),
            change.before.as_deref().unwrap_or("(new file)"),
            change.after
        );
        self.judge(AdvicePoint::Write, &state, &verdict_question(WRITE_RULES), native, started).await
    }

    async fn secret_probability(&self, candidates: &[SecretCandidate]) -> Scores {
        let started = Instant::now();
        let native = self.native.secret_probability(candidates).await;
        let items = candidates
            .iter()
            .map(|c| {
                let q = Question {
                    id: "secret".into(),
                    kind: QuestionKind::Noul,
                    instructions: format!("Is {} a real secret credential (not a placeholder, hash or identifier)?", c.value),
                };
                (c.context.clone(), q)
            })
            .collect();
        // Data-loss prevention: the more cautious estimate wins.
        self.scored(AdvicePoint::Secret, started, native, items, 0, true).await
    }

    async fn relevance(&self, query: &str, items: &[String]) -> Scores {
        let started = Instant::now();
        let native = self.native.relevance(query, items).await;
        let questions = items
            .iter()
            .map(|item| {
                let q = Question {
                    id: "relevance".into(),
                    kind: QuestionKind::Score(RELEVANCE_LEVELS.iter().map(|s| s.to_string()).collect()),
                    instructions: format!("How relevant is this text to the query: {query}?"),
                };
                (item.clone(), q)
            })
            .collect();
        self.scored(AdvicePoint::Rerank, started, native, questions, RELEVANCE_LEVELS.len(), false).await
    }

    async fn error_scores(&self, blocks: &[String]) -> Scores {
        let started = Instant::now();
        let native = self.native.error_scores(blocks).await;
        let questions = blocks
            .iter()
            .map(|block| {
                let q = Question {
                    id: "error".into(),
                    kind: QuestionKind::Score(ERROR_LEVELS.iter().map(|s| s.to_string()).collect()),
                    instructions: "How useful is this error block for finding and fixing the failure?".into(),
                };
                (block.clone(), q)
            })
            .collect();
        self.scored(AdvicePoint::Triage, started, native, questions, ERROR_LEVELS.len(), false).await
    }

    async fn route(&self, prompt: &str) -> Routing {
        let started = Instant::now();
        let native = self.native.route(prompt).await;
        let question = Question {
            id: "tier".into(),
            kind: QuestionKind::Choice(Tier::ALL.iter().map(|t| t.as_str().to_string()).collect()),
            instructions: "How demanding is this request for a coding agent? trivial: a greeting or one-line \
                fact; routine: ordinary coding; hard: multi-file changes or difficult debugging; \
                research: open-ended investigation across many sources."
                .into(),
        };
        let laya = self
            .choice(prompt, &question)
            .await
            .and_then(|(choice, confidence)| Tier::parse(&choice).map(|t| (t, confidence)).ok_or_else(|| format!("unknown tier {choice}")));
        match laya {
            Ok((tier, confidence)) => {
                self.report(AdvicePoint::Router, AdviceSource::Laya, tier.as_str(), Some(confidence), started, None);
                Routing { tier, confidence, source: AdviceSource::Laya }
            }
            Err(reason) => {
                let outcome = native.tier.as_str();
                self.report(AdvicePoint::Router, native.source, outcome, Some(native.confidence), started, Some(reason));
                native
            }
        }
    }

    async fn intent(&self, prompt: &str, servers: &[McpServerSummary]) -> Intent {
        let started = Instant::now();
        let native = self.native.intent(prompt, servers).await;
        if servers.len() + 1 > MAX_QUESTIONS {
            self.report(
                AdvicePoint::Intent,
                native.source,
                intent_name(native.kind),
                Some(native.confidence),
                started,
                Some(format!("{} MCP servers exceed one request", servers.len())),
            );
            return native;
        }
        let intent_q = Question {
            id: "intent".into(),
            kind: QuestionKind::Choice(["question", "action", "mixed"].iter().map(|s| s.to_string()).collect()),
            instructions: "What does the user want? question: explain or look something up; action: change code \
                or run commands; mixed: both or unclear."
                .into(),
        };
        let server_qs: Vec<Question> = servers
            .iter()
            .enumerate()
            .map(|(i, s)| Question {
                id: format!("server{i}"),
                kind: QuestionKind::Score(SERVER_LEVELS.iter().map(|s| s.to_string()).collect()),
                instructions: format!("How needed is the MCP server '{}' (tools: {}) for this request?", s.name, s.tools.join(", ")),
            })
            .collect();
        let mut questions = vec![intent_q];
        questions.extend(server_qs);

        let laya = match self.query(prompt, &questions).await {
            Ok(mut answers) => match answers.remove("intent") {
                Some(Ok(Answer::Choice { choice, confidence })) if confidence >= self.cfg.min_confidence => {
                    let kind = match choice.as_str() {
                        "question" => Some(IntentKind::Question),
                        "action" => Some(IntentKind::Action),
                        "mixed" => Some(IntentKind::Mixed),
                        _ => None,
                    };
                    match kind {
                        Some(kind) => {
                            // Server relevance from Laya only when every server was answered.
                            let server_relevance: Option<Vec<f64>> = (0..servers.len())
                                .map(|i| match answers.remove(&format!("server{i}")) {
                                    Some(Ok(a)) => unit(&a, SERVER_LEVELS.len()),
                                    _ => None,
                                })
                                .collect();
                            Ok((kind, confidence, server_relevance.unwrap_or(native.server_relevance.clone())))
                        }
                        None => Err(format!("unknown intent {choice}")),
                    }
                }
                Some(Ok(Answer::Choice { confidence, .. })) => Err(format!("low confidence {confidence:.2}")),
                Some(Ok(other)) => Err(format!("unexpected answer {other:?}")),
                Some(Err(e)) => Err(e.to_string()),
                None => Err("no answer".into()),
            },
            Err(reason) => Err(reason),
        };
        match laya {
            Ok((kind, confidence, server_relevance)) => {
                self.report(AdvicePoint::Intent, AdviceSource::Laya, intent_name(kind), Some(confidence), started, None);
                Intent { kind, confidence, server_relevance, source: AdviceSource::Laya }
            }
            Err(reason) => {
                self.report(AdvicePoint::Intent, native.source, intent_name(native.kind), Some(native.confidence), started, Some(reason));
                native
            }
        }
    }

    async fn auto_approve(&self, request: &ApprovalRequest, goal: &str) -> Judgement {
        let started = Instant::now();
        let native = self.native.auto_approve(request, goal).await;
        if native.verdict != Verdict::Allow {
            // Native never allows beyond moderate risk, and Laya cannot relax a non-allow.
            self.report_native(AdvicePoint::Auto, &native, started, None);
            return native;
        }
        let state = format!(
            "Tool: {}\nRequest: {}\nRisk: {}\nReason: {}\nPreview:\n{}\nUser goal: {goal}",
            request.tool, request.summary, request.risk, request.reason, request.preview
        );
        let question = Question {
            id: "auto".into(),
            kind: QuestionKind::Choice(vec!["auto".into(), "ask".into()]),
            instructions: "Can this request run without asking the human? auto: harmless, scoped and clearly \
                wanted; ask: destructive, sensitive, external or unclear."
                .into(),
        };
        // Laya disabled or marked down: native decides. Otherwise Laya must confidently say
        // "auto" at the auto-mode threshold; anything else asks the human, never a silent allow.
        if self.available().is_err() {
            self.report_native(AdvicePoint::Auto, &native, started, None);
            return native;
        }
        match self.choice(&state, &question).await {
            Ok((choice, confidence)) if choice == "auto" && confidence >= self.cfg.auto_mode.threshold => {
                self.report(AdvicePoint::Auto, AdviceSource::Laya, "allow", Some(confidence), started, None);
                native
            }
            Ok((choice, confidence)) => {
                let reason = format!("laya {choice} at {confidence:.2} below auto threshold");
                self.report(AdvicePoint::Auto, AdviceSource::Laya, "review", Some(confidence), started, Some(reason.clone()));
                Judgement::new(Verdict::Review, confidence, reason, AdviceSource::Laya)
            }
            Err(reason) => {
                self.report(AdvicePoint::Auto, AdviceSource::Laya, "review", None, started, Some(reason.clone()));
                Judgement::new(Verdict::Review, 0.0, reason, AdviceSource::Laya)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::sync::mpsc;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// Native advisor with fixed answers.
    struct FakeNative {
        verdict: Verdict,
        confidence: f64,
        score: f64,
    }

    #[async_trait]
    impl Advisor for FakeNative {
        fn name(&self) -> &str {
            "native"
        }
        async fn judge_command(&self, _c: &str, _p: &Path, _g: &str) -> Judgement {
            Judgement::new(self.verdict, self.confidence, "native", AdviceSource::Native)
        }
        async fn judge_write(&self, _c: &PlannedChange, _g: &str) -> Judgement {
            Judgement::new(self.verdict, self.confidence, "native", AdviceSource::Native)
        }
        async fn secret_probability(&self, candidates: &[SecretCandidate]) -> Scores {
            Scores { values: vec![self.score; candidates.len()], source: AdviceSource::Native }
        }
        async fn relevance(&self, _q: &str, items: &[String]) -> Scores {
            Scores { values: vec![self.score; items.len()], source: AdviceSource::Native }
        }
        async fn error_scores(&self, blocks: &[String]) -> Scores {
            Scores { values: vec![self.score; blocks.len()], source: AdviceSource::Native }
        }
        async fn route(&self, _p: &str) -> Routing {
            Routing { tier: Tier::Routine, confidence: 0.5, source: AdviceSource::Native }
        }
        async fn intent(&self, _p: &str, servers: &[McpServerSummary]) -> Intent {
            Intent { server_relevance: vec![1.0; servers.len()], source: AdviceSource::Native, ..Intent::default() }
        }
        async fn auto_approve(&self, _r: &ApprovalRequest, _g: &str) -> Judgement {
            Judgement::new(self.verdict, self.confidence, "native", AdviceSource::Native)
        }
    }

    fn native(verdict: Verdict) -> Arc<dyn Advisor> {
        Arc::new(FakeNative { verdict, confidence: 0.7, score: 0.1 })
    }

    fn cfg(url: &str, timeout_ms: u64) -> LayaConfig {
        LayaConfig { url: url.into(), api_key_env: None, timeout_ms, health_interval_secs: 1, ..LayaConfig::default() }
    }

    fn advisor(url: &str, native: Arc<dyn Advisor>, timeout_ms: u64) -> (LayaAdvisor, mpsc::UnboundedReceiver<AppEvent>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let c = cfg(url, timeout_ms);
        let client = LayaClient::with_api_key(&c, reqwest::Client::new(), None);
        (LayaAdvisor::with_client(client, native, c, EventSink::new(tx)), rx)
    }

    fn advice(rx: &mut mpsc::UnboundedReceiver<AppEvent>) -> Vec<AdviceInfo> {
        let mut out = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            if let AppEvent::Advice(a) = ev {
                out.push(a);
            }
        }
        out
    }

    /// A low-risk shell approval request.
    fn request() -> ApprovalRequest {
        ApprovalRequest {
            id: "r1".into(),
            agent_id: "main".into(),
            tool: "Bash".into(),
            summary: "Bash(cargo test)".into(),
            risk: crusty_core::permission::RiskLevel::Low,
            reason: String::new(),
            suggested_rule: None,
            preview: "cargo test".into(),
        }
    }

    async fn mount_answers(server: &MockServer, answers: serde_json::Value) {
        Mock::given(method("POST"))
            .and(path("/v1/systemone"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "answers": answers })))
            .mount(server)
            .await;
    }

    fn verdict_body(choice: &str, p: f64) -> serde_json::Value {
        json!({ "verdict": {"type": "choice", "choice": choice, "probabilities": {choice: p}} })
    }

    #[tokio::test]
    async fn laya_harder_verdict_wins_and_is_reported() {
        let server = MockServer::start().await;
        mount_answers(&server, verdict_body("block", 0.9)).await;
        let (adv, mut rx) = advisor(&server.uri(), native(Verdict::Allow), 2000);
        let j = adv.judge_command("rm -rf /", Path::new("."), "clean").await;
        assert_eq!(j.verdict, Verdict::Block);
        assert_eq!(j.source, AdviceSource::Laya);
        let events = advice(&mut rx);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].source, AdviceSource::Laya);
        assert_eq!(events[0].point, AdvicePoint::Command);
        assert_eq!(events[0].fallback_reason, None);
    }

    #[tokio::test]
    async fn laya_softer_verdict_loses_to_native() {
        let server = MockServer::start().await;
        mount_answers(&server, verdict_body("allow", 0.95)).await;
        let (adv, mut rx) = advisor(&server.uri(), native(Verdict::Review), 2000);
        let j = adv.judge_command("cargo test", Path::new("."), "").await;
        assert_eq!(j.verdict, Verdict::Review);
        assert_eq!(j.source, AdviceSource::Native);
        let events = advice(&mut rx);
        assert_eq!(events[0].source, AdviceSource::Native);
        assert!(events[0].fallback_reason.as_deref().is_some_and(|r| r.contains("not stricter")));
    }

    #[tokio::test]
    async fn low_confidence_falls_back_to_native() {
        let server = MockServer::start().await;
        mount_answers(&server, verdict_body("block", 0.3)).await;
        let (adv, mut rx) = advisor(&server.uri(), native(Verdict::Allow), 2000);
        let j = adv.judge_command("ls", Path::new("."), "").await;
        assert_eq!(j.verdict, Verdict::Allow);
        assert_eq!(j.source, AdviceSource::Native);
        let events = advice(&mut rx);
        assert!(events[0].fallback_reason.as_deref().is_some_and(|r| r.contains("low confidence")));
        assert!(adv.is_up(), "an unsure answer is not an outage");
    }

    #[tokio::test]
    async fn timeout_falls_back_and_flips_up() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/systemone"))
            .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_millis(800)))
            .mount(&server)
            .await;
        let (adv, mut rx) = advisor(&server.uri(), native(Verdict::Review), 100);
        let j = adv.judge_command("ls", Path::new("."), "").await;
        assert_eq!(j.verdict, Verdict::Review);
        assert!(!adv.is_up());
        let events = advice(&mut rx);
        assert_eq!(events[0].source, AdviceSource::Native);
        assert!(events[0].fallback_reason.as_deref().is_some_and(|r| r.contains("timed out")));
        // While down, Laya is not asked at all.
        let before = server.received_requests().await.map_or(0, |r| r.len());
        adv.judge_command("ls", Path::new("."), "").await;
        assert_eq!(server.received_requests().await.map_or(0, |r| r.len()), before);
    }

    #[tokio::test]
    async fn http_error_flips_up() {
        let server = MockServer::start().await;
        Mock::given(method("POST")).and(path("/v1/systemone")).respond_with(ResponseTemplate::new(500)).mount(&server).await;
        let (adv, _rx) = advisor(&server.uri(), native(Verdict::Allow), 2000);
        adv.judge_command("ls", Path::new("."), "").await;
        assert!(!adv.is_up());
    }

    #[tokio::test]
    async fn auto_never_allows_when_native_reviews() {
        let server = MockServer::start().await;
        mount_answers(&server, json!({ "auto": {"type": "choice", "choice": "auto", "probabilities": {"auto": 0.99}} })).await;
        let (adv, _rx) = advisor(&server.uri(), native(Verdict::Review), 2000);
        assert_eq!(adv.auto_approve(&request(), "go").await.verdict, Verdict::Review);
    }

    #[tokio::test]
    async fn auto_allows_only_when_both_agree_and_laya_can_still_ask() {
        let server = MockServer::start().await;
        mount_answers(&server, json!({ "auto": {"type": "choice", "choice": "auto", "probabilities": {"auto": 0.9}} })).await;
        let (adv, _rx) = advisor(&server.uri(), native(Verdict::Allow), 2000);
        assert_eq!(adv.auto_approve(&request(), "go").await.verdict, Verdict::Allow);

        let asking = MockServer::start().await;
        mount_answers(&asking, json!({ "auto": {"type": "choice", "choice": "ask", "probabilities": {"ask": 0.9}} })).await;
        let (adv, _rx) = advisor(&asking.uri(), native(Verdict::Allow), 2000);
        assert_eq!(adv.auto_approve(&request(), "go").await.verdict, Verdict::Review);
    }

    #[tokio::test]
    async fn auto_below_threshold_asks_instead_of_allowing() {
        // 0.7 clears min_confidence (0.6) but not auto_mode.threshold (0.8).
        let server = MockServer::start().await;
        mount_answers(&server, json!({ "auto": {"type": "choice", "choice": "auto", "probabilities": {"auto": 0.7}} })).await;
        let (adv, _rx) = advisor(&server.uri(), native(Verdict::Allow), 2000);
        assert_eq!(adv.auto_approve(&request(), "go").await.verdict, Verdict::Review);
    }

    #[tokio::test]
    async fn auto_laya_error_asks_then_native_decides_while_down() {
        let server = MockServer::start().await;
        Mock::given(method("POST")).and(path("/v1/systemone")).respond_with(ResponseTemplate::new(500)).mount(&server).await;
        let (adv, _rx) = advisor(&server.uri(), native(Verdict::Allow), 2000);
        // Consulted and failed: the call must ask, not run unasked.
        assert_eq!(adv.auto_approve(&request(), "go").await.verdict, Verdict::Review);
        // Marked down by the failure: Laya is not consulted, native judgement stands.
        assert_eq!(adv.auto_approve(&request(), "go").await.verdict, Verdict::Allow);
    }

    #[tokio::test]
    async fn auto_malformed_answer_asks() {
        let server = MockServer::start().await;
        mount_answers(&server, json!({ "auto": {"type": "score", "score": 1.0} })).await;
        let (adv, _rx) = advisor(&server.uri(), native(Verdict::Allow), 2000);
        assert_eq!(adv.auto_approve(&request(), "go").await.verdict, Verdict::Review);
    }

    #[tokio::test]
    async fn secrets_are_batched_with_bounded_concurrency_and_hardened() {
        let server = MockServer::start().await;
        mount_answers(&server, json!({ "secret": {"type": "noul", "noul": 0.9} })).await;
        let (adv, mut rx) = advisor(&server.uri(), native(Verdict::Allow), 2000);
        let candidates: Vec<SecretCandidate> =
            (0..20).map(|i| SecretCandidate { value: format!("v{i}"), context: format!("line {i}") }).collect();
        let scores = adv.secret_probability(&candidates).await;
        assert_eq!(scores.source, AdviceSource::Laya);
        assert_eq!(scores.values.len(), 20);
        // Native says 0.1; the higher Laya value wins.
        assert!(scores.values.iter().all(|v| (*v - 0.9).abs() < 1e-9));
        assert_eq!(server.received_requests().await.map_or(0, |r| r.len()), 20);
        let events = advice(&mut rx);
        assert_eq!(events[0].outcome, "20 scored");
    }

    #[tokio::test]
    async fn relevance_scores_are_scaled_to_unit_interval() {
        let server = MockServer::start().await;
        mount_answers(&server, json!({ "relevance": {"type": "score", "score": 3.0} })).await;
        let (adv, _rx) = advisor(&server.uri(), native(Verdict::Allow), 2000);
        let items = vec!["a".to_string(), "b".to_string()];
        let s = adv.relevance("query", &items).await;
        assert_eq!(s.values, vec![1.0, 1.0]);
        assert_eq!(s.source, AdviceSource::Laya);
    }

    #[tokio::test]
    async fn one_missing_score_falls_back_for_the_whole_batch() {
        let server = MockServer::start().await;
        mount_answers(&server, json!({ "error": {"type": "score", "score": 2.0} })).await;
        let (adv, mut rx) = advisor(&server.uri(), native(Verdict::Allow), 2000);
        // Items with no valid answer: the native values are kept.
        let blocks = vec!["x".to_string()];
        let s = adv.error_scores(&blocks).await;
        assert_eq!(s.source, AdviceSource::Laya);
        assert_eq!(s.values.len(), 1);
        let _ = advice(&mut rx);
    }

    #[tokio::test]
    async fn route_uses_laya_tier_when_confident() {
        let server = MockServer::start().await;
        mount_answers(&server, json!({ "tier": {"type": "choice", "choice": "hard", "probabilities": {"hard": 0.8}} })).await;
        let (adv, _rx) = advisor(&server.uri(), native(Verdict::Allow), 2000);
        let r = adv.route("refactor the engine").await;
        assert_eq!(r.tier, Tier::Hard);
        assert_eq!(r.source, AdviceSource::Laya);
    }

    #[tokio::test]
    async fn intent_combines_laya_kind_and_server_scores() {
        let server = MockServer::start().await;
        mount_answers(
            &server,
            json!({
                "intent": {"type": "choice", "choice": "action", "probabilities": {"action": 0.85}},
                "server0": {"type": "score", "score": 2.0}
            }),
        )
        .await;
        let (adv, _rx) = advisor(&server.uri(), native(Verdict::Allow), 2000);
        let servers = vec![McpServerSummary { name: "git".into(), tools: vec!["log".into()] }];
        let i = adv.intent("show the log", &servers).await;
        assert_eq!(i.kind, IntentKind::Action);
        assert_eq!(i.server_relevance, vec![1.0]);
        assert_eq!(i.source, AdviceSource::Laya);
    }

    #[tokio::test]
    async fn health_probe_updates_flag_and_emits_status() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/health"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"status": "ok"})))
            .mount(&server)
            .await;
        let (adv, mut rx) = advisor(&server.uri(), native(Verdict::Allow), 2000);
        adv.up.store(false, Ordering::Relaxed);
        adv.probe().await;
        assert!(adv.is_up());
        match rx.try_recv() {
            Ok(AppEvent::AdvisorStatus { up: true, latency_ms: Some(_), .. }) => {}
            other => panic!("unexpected event {other:?}"),
        }
    }

    #[tokio::test]
    async fn health_probe_down_emits_status_and_clears_flag() {
        let server = MockServer::start().await;
        Mock::given(method("GET")).and(path("/health")).respond_with(ResponseTemplate::new(500)).mount(&server).await;
        let (adv, mut rx) = advisor(&server.uri(), native(Verdict::Allow), 2000);
        adv.probe().await;
        assert!(!adv.is_up());
        match rx.try_recv() {
            Ok(AppEvent::AdvisorStatus { up: false, detail: Some(_), .. }) => {}
            other => panic!("unexpected event {other:?}"),
        }
    }

    #[tokio::test]
    async fn monitor_probes_immediately_and_stops_on_cancel() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/health"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"status": "ok"})))
            .mount(&server)
            .await;
        let (adv, _rx) = advisor(&server.uri(), native(Verdict::Allow), 2000);
        let adv = Arc::new(adv);
        adv.up.store(false, Ordering::Relaxed);
        let cancel = CancellationToken::new();
        let handle = spawn_health_monitor(Arc::clone(&adv), cancel.clone());
        for _ in 0..50 {
            if adv.is_up() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        cancel.cancel();
        handle.await.expect("monitor stops cleanly");
        assert!(adv.is_up());
    }

    #[tokio::test]
    async fn build_advisor_observes_native_when_disabled() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let c = LayaConfig { enabled: false, ..LayaConfig::default() };
        let a = build_advisor(&c, native(Verdict::Review), reqwest::Client::new(), EventSink::new(tx));
        assert_eq!(a.name(), "native");
        let j = a.judge_command("ls", Path::new("."), "").await;
        assert_eq!(j.verdict, Verdict::Review);
        let events = advice(&mut rx);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].source, AdviceSource::Native);
        assert_eq!(events[0].point, AdvicePoint::Command);
        assert_eq!(events[0].outcome, "review");
        assert_eq!(events[0].fallback_reason, None);
    }

    #[tokio::test]
    async fn build_advisor_returns_laya_when_enabled() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let c = LayaConfig { enabled: true, ..LayaConfig::default() };
        let a = build_advisor(&c, native(Verdict::Allow), reqwest::Client::new(), EventSink::new(tx));
        assert_eq!(a.name(), "laya");
    }
}
