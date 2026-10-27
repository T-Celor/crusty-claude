//! Permission vocabulary shared by the tool gateway, the agent engine and the UI.
//! The rule engine itself lives in `crusty-tools`.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RiskLevel {
    /// Read allowed files, search symbols: allowed automatically.
    #[default]
    Low,
    /// Edit project files, run tests: allowed by project rules or asked.
    Moderate,
    /// Delete files, install dependencies, change system config: always asked.
    High,
    /// Secrets, privileged commands, data exfiltration: denied unless a rule allows it,
    /// and then still asked.
    Critical,
}

impl std::fmt::Display for RiskLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            RiskLevel::Low => "low",
            RiskLevel::Moderate => "moderate",
            RiskLevel::High => "high",
            RiskLevel::Critical => "critical",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum PermissionMode {
    /// Ask for anything not allowed by a rule.
    #[default]
    Default,
    /// File edits inside the workspace are allowed without asking.
    AcceptEdits,
    /// Read-only: every tool that changes something is denied.
    Plan,
    /// Low and moderate risk run without asking. High risk is still asked unless an allow
    /// rule names it, critical risk is still denied unless an allow rule names it (and then
    /// asked), and deny rules and the workspace sandbox still apply: this mode never
    /// disables protections.
    BypassPermissions,
}

impl PermissionMode {
    pub fn next(self) -> Self {
        match self {
            PermissionMode::Default => PermissionMode::AcceptEdits,
            PermissionMode::AcceptEdits => PermissionMode::Plan,
            PermissionMode::Plan => PermissionMode::BypassPermissions,
            PermissionMode::BypassPermissions => PermissionMode::Default,
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().replace(['-', '_'], "").as_str() {
            "default" => Some(PermissionMode::Default),
            "acceptedits" => Some(PermissionMode::AcceptEdits),
            "plan" => Some(PermissionMode::Plan),
            "bypasspermissions" | "bypass" => Some(PermissionMode::BypassPermissions),
            _ => None,
        }
    }
}

impl std::fmt::Display for PermissionMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            PermissionMode::Default => "default",
            PermissionMode::AcceptEdits => "acceptEdits",
            PermissionMode::Plan => "plan",
            PermissionMode::BypassPermissions => "bypassPermissions",
        })
    }
}

/// How risky one concrete tool call is, decided from its input, not only its name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RiskAssessment {
    pub level: RiskLevel,
    pub reason: String,
}

impl RiskAssessment {
    pub fn new(level: RiskLevel, reason: impl Into<String>) -> Self {
        RiskAssessment { level, reason: reason.into() }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum PermissionDecision {
    Allow {
        reason: String,
    },
    /// Needs the user. `suggested_rule` is what "always allow" would add.
    Ask {
        reason: String,
        suggested_rule: Option<String>,
    },
    Deny {
        reason: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalRequest {
    pub id: String,
    pub agent_id: String,
    pub tool: String,
    /// One line, e.g. `Bash(rm -rf target)`.
    pub summary: String,
    pub risk: RiskLevel,
    pub reason: String,
    pub suggested_rule: Option<String>,
    /// Bounded preview of the input (diff, command, URL).
    pub preview: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "feedback", rename_all = "snake_case")]
pub enum ApprovalDecision {
    AllowOnce,
    /// Allow and add `suggested_rule` to the session's allow rules.
    AllowAlways,
    Deny,
    /// Deny and tell the agent what to do instead.
    DenyWithFeedback(String),
}

impl ApprovalDecision {
    pub fn is_allowed(&self) -> bool {
        matches!(self, ApprovalDecision::AllowOnce | ApprovalDecision::AllowAlways)
    }
}

#[async_trait]
pub trait Approver: Send + Sync {
    async fn approve(&self, req: ApprovalRequest) -> ApprovalDecision;
}

/// The rule engine as the agent engine sees it (implemented by
/// `crusty_tools::PermissionEngine`).
pub trait PermissionPolicy: Send + Sync {
    fn evaluate(&self, tool: &dyn crate::tool::Tool, input: &serde_json::Value, ctx: &crate::tool::ToolContext) -> PermissionDecision;
    fn mode(&self) -> PermissionMode;
    fn set_mode(&self, mode: PermissionMode);
    /// Adds a session allow rule (from "always allow"). Errors on a malformed rule.
    fn add_allow_rule(&self, rule: &str) -> Result<(), String>;
    /// Human-readable list of the active rules, for `/permissions`.
    fn describe(&self) -> String;
}

/// Answers every request the same way: used by non-interactive runs and tests.
pub struct FixedApprover(pub ApprovalDecision);

#[async_trait]
impl Approver for FixedApprover {
    async fn approve(&self, _req: ApprovalRequest) -> ApprovalDecision {
        self.0.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_cycles_and_parses() {
        let mut m = PermissionMode::Default;
        for _ in 0..4 {
            m = m.next();
        }
        assert_eq!(m, PermissionMode::Default);
        assert_eq!(PermissionMode::parse("accept-edits"), Some(PermissionMode::AcceptEdits));
        assert_eq!(PermissionMode::parse("bypassPermissions"), Some(PermissionMode::BypassPermissions));
        assert_eq!(PermissionMode::parse("nope"), None);
    }

    #[test]
    fn risk_is_ordered() {
        assert!(RiskLevel::Critical > RiskLevel::High && RiskLevel::High > RiskLevel::Low);
    }
}
