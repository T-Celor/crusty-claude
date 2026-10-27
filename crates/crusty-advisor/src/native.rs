//! The deterministic advisor used when the Laya model server is absent.

use std::path::Path;

use async_trait::async_trait;
use crusty_core::advisor::SecretCandidate;
use crusty_core::advisor::{AdviceSource, Advisor, Intent, Judgement, McpServerSummary, Routing, Scores};
use crusty_core::config::LayaConfig;
use crusty_core::permission::ApprovalRequest;
use crusty_core::tool::PlannedChange;

use crate::guard::{self, Sensitive};
use crate::{rank, route, secret, triage};

/// Write-sensitive globs always judged, whatever the configuration says.
pub const DEFAULT_SENSITIVE_PATHS: &[&str] = &[
    "**/.env",
    "**/.env.*",
    "**/.gitignore",
    "**/.gitattributes",
    "**/.github/workflows/**",
    "**/.gitlab-ci.yml",
    "**/.circleci/**",
    "**/azure-pipelines.yml",
    "**/Jenkinsfile",
    "**/Dockerfile*",
    "**/docker-compose*.yml",
    "**/*.pem",
    "**/*.key",
    "**/id_rsa*",
    "**/.npmrc",
    "**/.pypirc",
    "**/.ssh/**",
    "**/.git/**",
    "**/.crusty/**",
    "**/.claude/**",
    "**/.mcp.json",
];

/// Deterministic advisor. Built from the Laya configuration: a switched-off guard
/// (`guard.commands` or `guard.files` false) abstains for that question.
pub struct NativeAdvisor {
    sensitive: Sensitive,
    commands: bool,
    files: bool,
}

impl NativeAdvisor {
    /// Uses [`DEFAULT_SENSITIVE_PATHS`] plus `cfg.guard.sensitive_paths`. Invalid globs are
    /// skipped with a warning.
    pub fn new(cfg: &LayaConfig) -> Self {
        let patterns = DEFAULT_SENSITIVE_PATHS.iter().map(|p| p.to_string()).chain(cfg.guard.sensitive_paths.iter().cloned());
        NativeAdvisor { sensitive: Sensitive::new(patterns), commands: cfg.guard.commands, files: cfg.guard.files }
    }
}

#[async_trait]
impl Advisor for NativeAdvisor {
    fn name(&self) -> &str {
        "native"
    }

    async fn judge_command(&self, command: &str, cwd: &Path, _goal: &str) -> Judgement {
        if !self.commands {
            return Judgement::abstain();
        }
        guard::judge_command(command, cwd)
    }

    async fn judge_write(&self, change: &PlannedChange, _goal: &str) -> Judgement {
        if !self.files {
            return Judgement::abstain();
        }
        guard::judge_write(change, &self.sensitive)
    }

    async fn secret_probability(&self, candidates: &[SecretCandidate]) -> Scores {
        Scores { values: candidates.iter().map(secret::probability).collect(), source: AdviceSource::Native }
    }

    async fn relevance(&self, query: &str, items: &[String]) -> Scores {
        Scores { values: rank::bm25_scores(query, items), source: AdviceSource::Native }
    }

    async fn error_scores(&self, blocks: &[String]) -> Scores {
        Scores { values: blocks.iter().map(|b| triage::block_score(b)).collect(), source: AdviceSource::Native }
    }

    async fn route(&self, prompt: &str) -> Routing {
        route::route(prompt)
    }

    async fn intent(&self, prompt: &str, servers: &[McpServerSummary]) -> Intent {
        route::intent(prompt, servers)
    }

    async fn auto_approve(&self, request: &ApprovalRequest, _goal: &str) -> Judgement {
        route::auto_approve(request)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crusty_core::advisor::Verdict;
    use crusty_core::permission::RiskLevel;

    #[tokio::test]
    async fn answers_are_native_and_guarded() {
        let mut cfg = LayaConfig::default();
        cfg.guard.sensitive_paths = vec!["[".into(), "secrets/**".into()];
        let a = NativeAdvisor::new(&cfg);
        assert_eq!(a.name(), "native");
        let j = a.judge_command("rm -rf /", Path::new("."), "").await;
        assert_eq!(j.verdict, Verdict::Block);
        assert_eq!(j.source, AdviceSource::Native);
        let w = PlannedChange { path: "secrets/a.txt".into(), before: None, after: "x".into() };
        let j = a.judge_write(&w, "").await;
        assert_eq!(j.verdict, Verdict::Review);
        assert_eq!(j.source, AdviceSource::Native);
        let s = a.secret_probability(&[SecretCandidate { value: "AKIAIOSFODNN7EXAMPLE".into(), context: String::new() }]).await;
        assert_eq!(s.values.len(), 1);
        assert_eq!(s.source, AdviceSource::Native);
        let r = a.relevance("parse config", &["fn parse_config".into(), "render".into()]).await;
        assert_eq!(r.values.len(), 2);
        let e = a.error_scores(&["error[E0308]: mismatched".into()]).await;
        assert!(e.values[0] > 0.5);
        assert_eq!(a.route("Rename this variable").await.tier, crusty_core::advisor::Tier::Trivial);
        let req = ApprovalRequest {
            id: "1".into(),
            agent_id: "a".into(),
            tool: "Bash".into(),
            summary: "Bash(ls)".into(),
            risk: RiskLevel::Low,
            reason: String::new(),
            suggested_rule: None,
            preview: String::new(),
        };
        assert_eq!(a.auto_approve(&req, "").await.verdict, Verdict::Allow);
    }

    #[tokio::test]
    async fn disabled_guard_abstains() {
        let mut cfg = LayaConfig::default();
        cfg.guard.commands = false;
        cfg.guard.files = false;
        let a = NativeAdvisor::new(&cfg);
        assert!(a.judge_command("rm -rf /", Path::new("."), "").await.is_abstention());
        let w = PlannedChange { path: ".env".into(), before: None, after: "x".into() };
        assert!(a.judge_write(&w, "").await.is_abstention());
    }
}
