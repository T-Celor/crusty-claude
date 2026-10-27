//! The advisor: fast local judgements used by the guard, the optimizer, the router and
//! the `auto` permission mode.
//!
//! `crusty-laya` asks a Laya server (`laya-serve`) and falls back to the deterministic
//! heuristics of `crusty-advisor`, which also run alone when Laya is off. Whatever the
//! source, an advisor can only make a security decision stricter: its verdicts escalate
//! the native permission decision (allow → ask → deny), never relax it. The one
//! exception is the `auto` mode, where a confident `Allow` replaces a question to the
//! user for non-critical requests; deny rules, critical risk and the sandbox still apply.
//!
//! Scores are "no opinion" when `values.len()` differs from the number of items asked
//! about: consumers must then keep their native behaviour.

use std::path::Path;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::permission::ApprovalRequest;
use crate::tool::PlannedChange;

/// Ordered from the most to the least permissive, so `max` escalates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    #[default]
    Allow,
    /// A human should look at it: an allowed action becomes a question.
    Review,
    /// Refuse the action.
    Block,
}

impl std::fmt::Display for Verdict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Verdict::Allow => "allow",
            Verdict::Review => "review",
            Verdict::Block => "block",
        })
    }
}

/// Who answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AdviceSource {
    /// The Laya model.
    Laya,
    /// The native heuristics (Laya off, unreachable, or unsure).
    Native,
    /// Nobody: the advisor abstained and the caller keeps its own decision.
    #[default]
    None,
}

impl std::fmt::Display for AdviceSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            AdviceSource::Laya => "laya",
            AdviceSource::Native => "native",
            AdviceSource::None => "none",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Judgement {
    pub verdict: Verdict,
    /// Confidence in `verdict`, 0.0 to 1.0.
    pub confidence: f64,
    pub reason: String,
    pub source: AdviceSource,
}

impl Judgement {
    /// No opinion: `Allow` with zero confidence from nobody, which never changes a decision.
    pub fn abstain() -> Self {
        Judgement::default()
    }

    pub fn new(verdict: Verdict, confidence: f64, reason: impl Into<String>, source: AdviceSource) -> Self {
        Judgement { verdict, confidence: confidence.clamp(0.0, 1.0), reason: reason.into(), source }
    }

    pub fn is_abstention(&self) -> bool {
        self.source == AdviceSource::None
    }
}

/// One score per item asked about, 0.0 to 1.0, or no opinion (see the module docs).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Scores {
    pub values: Vec<f64>,
    pub source: AdviceSource,
}

impl Scores {
    pub fn none() -> Self {
        Scores::default()
    }

    /// The scores, when there is exactly one per item.
    pub fn for_items(&self, items: usize) -> Option<&[f64]> {
        (self.source != AdviceSource::None && self.values.len() == items).then_some(self.values.as_slice())
    }
}

/// How demanding a request is, for the model router.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    /// A greeting, a one-line fact, a typo.
    Trivial,
    /// Ordinary coding work.
    #[default]
    Routine,
    /// Multi-file changes, difficult debugging, architecture.
    Hard,
    /// Open-ended investigation across many sources.
    Research,
}

impl Tier {
    pub const ALL: [Tier; 4] = [Tier::Trivial, Tier::Routine, Tier::Hard, Tier::Research];

    pub fn as_str(self) -> &'static str {
        match self {
            Tier::Trivial => "trivial",
            Tier::Routine => "routine",
            Tier::Hard => "hard",
            Tier::Research => "research",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Tier::ALL.into_iter().find(|t| t.as_str().eq_ignore_ascii_case(s.trim()))
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Routing {
    pub tier: Tier,
    pub confidence: f64,
    pub source: AdviceSource,
}

/// What the user wants from a prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum IntentKind {
    /// Explain, answer, look something up: reading tools are enough.
    Question,
    /// Change code, run commands.
    Action,
    /// Both, or unclear.
    #[default]
    Mixed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Intent {
    pub kind: IntentKind,
    pub confidence: f64,
    /// Relevance of each MCP server to the prompt, in the order given; `1.0` for every
    /// server when the advisor has no opinion.
    pub server_relevance: Vec<f64>,
    pub source: AdviceSource,
}

/// An MCP server as the intent detector sees it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct McpServerSummary {
    pub name: String,
    /// Tool names and the first sentence of their descriptions.
    pub tools: Vec<String>,
}

/// A string that might be a secret, with the line it was found on.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct SecretCandidate {
    pub value: String,
    pub context: String,
}

#[async_trait]
pub trait Advisor: Send + Sync {
    /// `laya`, `native` or `none`, for the UI.
    fn name(&self) -> &str;

    /// Shell command interceptor: `Block` destructive or exfiltrating commands, `Review`
    /// suspicious ones. Runs after the native risk scanner and may only escalate it.
    async fn judge_command(&self, command: &str, cwd: &Path, goal: &str) -> Judgement;

    /// Write guard for sensitive files (`.env`, ignore rules, CI/CD, container files).
    async fn judge_write(&self, change: &PlannedChange, goal: &str) -> Judgement;

    /// Probability that each candidate is a secret (data-loss prevention).
    async fn secret_probability(&self, candidates: &[SecretCandidate]) -> Scores;

    /// Relevance of each item to `query` (reranking search fragments, pruning history).
    async fn relevance(&self, query: &str, items: &[String]) -> Scores;

    /// How useful each error block of a failed build or test log is to fix it.
    async fn error_scores(&self, blocks: &[String]) -> Scores;

    /// Model tier for a prompt.
    async fn route(&self, prompt: &str) -> Routing;

    /// Intent of a prompt and relevance of each MCP server to it.
    async fn intent(&self, prompt: &str, servers: &[McpServerSummary]) -> Intent;

    /// `auto` mode: `Allow` with its confidence when the request can run without a human.
    async fn auto_approve(&self, request: &ApprovalRequest, goal: &str) -> Judgement;
}

/// Abstains everywhere: callers keep their native behaviour. Used by tests and library
/// users that do not configure an advisor.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoAdvisor;

#[async_trait]
impl Advisor for NoAdvisor {
    fn name(&self) -> &str {
        "none"
    }

    async fn judge_command(&self, _command: &str, _cwd: &Path, _goal: &str) -> Judgement {
        Judgement::abstain()
    }

    async fn judge_write(&self, _change: &PlannedChange, _goal: &str) -> Judgement {
        Judgement::abstain()
    }

    async fn secret_probability(&self, _candidates: &[SecretCandidate]) -> Scores {
        Scores::none()
    }

    async fn relevance(&self, _query: &str, _items: &[String]) -> Scores {
        Scores::none()
    }

    async fn error_scores(&self, _blocks: &[String]) -> Scores {
        Scores::none()
    }

    async fn route(&self, _prompt: &str) -> Routing {
        Routing::default()
    }

    async fn intent(&self, _prompt: &str, servers: &[McpServerSummary]) -> Intent {
        Intent { server_relevance: vec![1.0; servers.len()], ..Intent::default() }
    }

    async fn auto_approve(&self, _request: &ApprovalRequest, _goal: &str) -> Judgement {
        Judgement { verdict: Verdict::Review, ..Judgement::abstain() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verdicts_escalate_with_max() {
        assert_eq!(Verdict::Allow.max(Verdict::Review), Verdict::Review);
        assert_eq!(Verdict::Block.max(Verdict::Review), Verdict::Block);
    }

    #[test]
    fn scores_need_one_value_per_item() {
        let s = Scores { values: vec![0.5, 0.2], source: AdviceSource::Native };
        assert!(s.for_items(2).is_some());
        assert!(s.for_items(3).is_none());
        assert!(Scores::none().for_items(0).is_none());
    }

    #[test]
    fn tiers_parse() {
        assert_eq!(Tier::parse(" Hard "), Some(Tier::Hard));
        assert_eq!(Tier::parse("x"), None);
    }

    #[tokio::test]
    async fn no_advisor_keeps_native_behaviour() {
        let a = NoAdvisor;
        assert!(a.judge_command("ls", Path::new("."), "").await.is_abstention());
        let i = a.intent("x", &[McpServerSummary::default(), McpServerSummary::default()]).await;
        assert_eq!(i.server_relevance, vec![1.0, 1.0]);
        assert_eq!(a.route("x").await.confidence, 0.0);
    }
}
