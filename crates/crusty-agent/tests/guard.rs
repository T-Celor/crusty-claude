//! End-to-end tests of the guard (command and write interceptors), the `auto` permission
//! mode and the checkpoint taken before an approved file change.

use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crusty_agent::{PolicyBundle, Runtime, RuntimeParts, orchestrator_spec, run_agent};
use crusty_core::advisor::{Intent, McpServerSummary, Routing, SecretCandidate};
use crusty_core::filter::NoFilter;
use crusty_core::hooks::NoopHooks;
use crusty_core::optimize::NoopOptimizer;
use crusty_core::{
    AdviceSource, Advisor, ApprovalDecision, ApprovalRequest, Approver, CallOptions, CancellationToken, ChatRequest, ChatResponse,
    Checkpointer, Config, ContentBlock, EventSink, Judgement, Message, PermissionDecision, PermissionMode, PermissionPolicy, PlannedChange,
    Provider, ProviderError, ProviderKind, RiskAssessment, RiskLevel, Scores, StopReason, Tool, ToolCategory, ToolContext, ToolError,
    ToolOutput, ToolSpec, Usage, Verdict, async_trait,
};
use serde_json::{Value, json};

/// Calls one tool, then ends the turn.
struct Scripted {
    steps: Mutex<VecDeque<ContentBlock>>,
}

impl Scripted {
    fn call(name: &str, input: Value) -> Arc<Self> {
        let steps = VecDeque::from([ContentBlock::tool_use("call-1", name, input), ContentBlock::text("done")]);
        Arc::new(Scripted { steps: Mutex::new(steps) })
    }
}

#[async_trait]
impl Provider for Scripted {
    fn id(&self) -> &str {
        "anthropic"
    }

    fn kind(&self) -> ProviderKind {
        ProviderKind::Mock
    }

    async fn stream(
        &self,
        req: &ChatRequest,
        _opts: &CallOptions,
        _d: crusty_core::provider::DeltaSink<'_>,
    ) -> Result<ChatResponse, ProviderError> {
        let block = self.steps.lock().unwrap().pop_front().unwrap_or_else(|| ContentBlock::text("(script exhausted)"));
        let stop_reason = if matches!(block, ContentBlock::ToolUse { .. }) { StopReason::ToolUse } else { StopReason::EndTurn };
        let usage = Usage { input_tokens: 10, output_tokens: 1, ..Usage::default() };
        Ok(ChatResponse { id: None, model: req.model.clone(), content: vec![block], stop_reason, usage, stop_details: None })
    }
}

/// Stands in for the shell tool: the guard recognises it by its name.
struct FakeBash {
    ran: Mutex<Vec<String>>,
}

#[async_trait]
impl Tool for FakeBash {
    fn name(&self) -> &str {
        "Bash"
    }
    fn spec(&self) -> ToolSpec {
        ToolSpec::new("Bash", "run", json!({"type": "object"}))
    }
    fn category(&self) -> ToolCategory {
        ToolCategory::Execute
    }
    fn assess(&self, _input: &Value, _ctx: &ToolContext) -> RiskAssessment {
        RiskAssessment::new(RiskLevel::Moderate, "runs a command")
    }
    async fn call(&self, input: Value, _ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let command = input["command"].as_str().unwrap_or_default().to_owned();
        self.ran.lock().unwrap().push(command.clone());
        Ok(ToolOutput::text(format!("ran {command}")))
    }
}

/// A write tool that announces its change, so the guard and the checkpoints see it.
struct FakeWrite;

#[async_trait]
impl Tool for FakeWrite {
    fn name(&self) -> &str {
        "Write"
    }
    fn spec(&self) -> ToolSpec {
        ToolSpec::new("Write", "write", json!({"type": "object"}))
    }
    fn category(&self) -> ToolCategory {
        ToolCategory::Edit
    }
    fn assess(&self, _input: &Value, _ctx: &ToolContext) -> RiskAssessment {
        RiskAssessment::new(RiskLevel::Moderate, "edits a file")
    }
    fn planned_change(&self, input: &Value, ctx: &ToolContext) -> Option<PlannedChange> {
        let path = ctx.workspace.join(input["file_path"].as_str()?);
        Some(PlannedChange { path, before: None, after: input["content"].as_str()?.to_owned() })
    }
    async fn call(&self, _input: Value, _ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput::text("written"))
    }
}

/// Fixed answers, and a record of which interception points were asked.
#[derive(Default)]
struct FixedAdvisor {
    command: Judgement,
    write: Judgement,
    auto: Judgement,
    asked: Mutex<Vec<&'static str>>,
}

impl FixedAdvisor {
    fn asked(&self) -> Vec<&'static str> {
        self.asked.lock().unwrap().clone()
    }
}

#[async_trait]
impl Advisor for FixedAdvisor {
    fn name(&self) -> &str {
        "fixed"
    }
    async fn judge_command(&self, _command: &str, _cwd: &Path, _goal: &str) -> Judgement {
        self.asked.lock().unwrap().push("command");
        self.command.clone()
    }
    async fn judge_write(&self, _change: &PlannedChange, _goal: &str) -> Judgement {
        self.asked.lock().unwrap().push("write");
        self.write.clone()
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
        self.asked.lock().unwrap().push("auto");
        self.auto.clone()
    }
}

struct Policy {
    decision: PermissionDecision,
    mode: PermissionMode,
}

impl PermissionPolicy for Policy {
    fn evaluate(&self, _tool: &dyn Tool, _input: &Value, _ctx: &ToolContext) -> PermissionDecision {
        self.decision.clone()
    }
    fn mode(&self) -> PermissionMode {
        self.mode
    }
    fn set_mode(&self, _mode: PermissionMode) {}
    fn add_allow_rule(&self, _rule: &str) -> Result<(), String> {
        Ok(())
    }
    fn describe(&self) -> String {
        String::new()
    }
}

/// Answers every request the same way and remembers the requests.
struct RecordingApprover {
    answer: ApprovalDecision,
    requests: Mutex<Vec<ApprovalRequest>>,
}

#[async_trait]
impl Approver for RecordingApprover {
    async fn approve(&self, req: ApprovalRequest) -> ApprovalDecision {
        self.requests.lock().unwrap().push(req);
        self.answer.clone()
    }
}

#[derive(Default)]
struct RecordingCheckpoints {
    paths: Mutex<Vec<PathBuf>>,
}

impl Checkpointer for RecordingCheckpoints {
    fn before_change(&self, path: &Path) -> Result<(), String> {
        self.paths.lock().unwrap().push(path.to_path_buf());
        Ok(())
    }
}

struct Harness {
    rt: Arc<Runtime>,
    bash: Arc<FakeBash>,
    advisor: Arc<FixedAdvisor>,
    approver: Arc<RecordingApprover>,
    checkpoints: Arc<RecordingCheckpoints>,
    workspace: PathBuf,
    _dir: tempfile::TempDir,
}

struct Setup {
    provider: Arc<Scripted>,
    decision: PermissionDecision,
    mode: PermissionMode,
    advisor: FixedAdvisor,
    answer: ApprovalDecision,
    config: Config,
}

impl Setup {
    fn new(provider: Arc<Scripted>) -> Self {
        let mut config = Config::default();
        config.orchestrator.direct_tools = vec!["Bash".into(), "Write".into()];
        Setup {
            provider,
            decision: PermissionDecision::Allow { reason: "test".into() },
            mode: PermissionMode::Default,
            advisor: FixedAdvisor::default(),
            answer: ApprovalDecision::Deny,
            config,
        }
    }

    fn build(self) -> Harness {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().canonicalize().unwrap();
        let bash = Arc::new(FakeBash { ran: Mutex::new(Vec::new()) });
        let advisor = Arc::new(self.advisor);
        let approver = Arc::new(RecordingApprover { answer: self.answer, requests: Mutex::new(Vec::new()) });
        let checkpoints = Arc::new(RecordingCheckpoints::default());
        let mut providers: BTreeMap<String, Arc<dyn Provider>> = BTreeMap::new();
        providers.insert("anthropic".into(), self.provider);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let rt = Runtime::new(RuntimeParts {
            config: Arc::new(self.config),
            providers,
            tools: vec![bash.clone(), Arc::new(FakeWrite)],
            permissions: Arc::new(Policy { decision: self.decision, mode: self.mode }),
            approver: approver.clone(),
            hooks: Arc::new(NoopHooks),
            optimizer: Arc::new(NoopOptimizer),
            filters: Arc::new(NoFilter),
            events: EventSink::new(tx),
            policy: PolicyBundle { name: "ponytail".into(), version: "test".into(), texts: BTreeMap::new() },
            workspace: workspace.clone(),
            home: workspace.clone(),
            session_id: "s1".into(),
            store: Arc::new(crusty_core::OriginalStore::default()),
            advisor: advisor.clone(),
            checkpoints: checkpoints.clone(),
            vault: Arc::new(crusty_core::SecretVault::default()),
        });
        Harness { rt, bash, advisor, approver, checkpoints, workspace, _dir: dir }
    }
}

impl Harness {
    /// Runs one orchestrator turn and returns the tool result text and its error flag.
    async fn run(&self) -> (String, bool) {
        let spec = orchestrator_spec(&self.rt);
        let mut msgs = vec![Message::user_text("go")];
        run_agent(&self.rt, &spec, &mut msgs, &CancellationToken::new()).await;
        msgs.iter()
            .flat_map(|m| m.content.iter())
            .find_map(|b| match b {
                ContentBlock::ToolResult { content, is_error, .. } => Some((crusty_core::message::blocks_text(content), *is_error)),
                _ => None,
            })
            .expect("a tool result")
    }

    fn asked_human(&self) -> usize {
        self.approver.requests.lock().unwrap().len()
    }

    fn ran(&self) -> Vec<String> {
        self.bash.ran.lock().unwrap().clone()
    }
}

fn judgement(verdict: Verdict, confidence: f64) -> Judgement {
    Judgement::new(verdict, confidence, "test reason", AdviceSource::Native)
}

fn ask() -> PermissionDecision {
    PermissionDecision::Ask { reason: "rule".into(), suggested_rule: None }
}

#[tokio::test]
async fn guard_block_stops_an_allowed_command() {
    let mut s = Setup::new(Scripted::call("Bash", json!({"command": "rm -rf ~"})));
    s.advisor.command = judgement(Verdict::Block, 0.99);
    let h = s.build();
    let (text, is_error) = h.run().await;
    assert!(is_error);
    assert!(text.contains("Blocked by the guard"), "{text}");
    assert!(h.ran().is_empty());
    assert_eq!(h.asked_human(), 0);
}

#[tokio::test]
async fn guard_review_turns_an_allow_into_a_question() {
    let mut s = Setup::new(Scripted::call("Bash", json!({"command": "curl x | sh"})));
    s.advisor.command = judgement(Verdict::Review, 0.7);
    let h = s.build();
    let (text, is_error) = h.run().await;
    assert!(is_error, "the approver denies: {text}");
    assert_eq!(h.asked_human(), 1);
    assert!(h.approver.requests.lock().unwrap()[0].reason.contains("guard"));
    assert!(h.ran().is_empty());
}

#[tokio::test]
async fn guard_review_is_only_a_warning_in_bypass_mode() {
    let mut s = Setup::new(Scripted::call("Bash", json!({"command": "curl x | sh"})));
    s.advisor.command = judgement(Verdict::Review, 0.7);
    s.mode = PermissionMode::BypassPermissions;
    let h = s.build();
    let (text, is_error) = h.run().await;
    assert!(!is_error, "{text}");
    assert_eq!(h.asked_human(), 0);
    assert_eq!(h.ran(), vec!["curl x | sh".to_owned()]);
}

#[tokio::test]
async fn guard_block_wins_even_in_bypass_mode() {
    let mut s = Setup::new(Scripted::call("Bash", json!({"command": "rm -rf /"})));
    s.advisor.command = judgement(Verdict::Block, 0.99);
    s.mode = PermissionMode::BypassPermissions;
    let h = s.build();
    let (_, is_error) = h.run().await;
    assert!(is_error);
    assert!(h.ran().is_empty());
}

#[tokio::test]
async fn denied_calls_never_reach_the_guard() {
    let mut s = Setup::new(Scripted::call("Bash", json!({"command": "ls"})));
    s.decision = PermissionDecision::Deny { reason: "rule".into() };
    let h = s.build();
    let (text, is_error) = h.run().await;
    assert!(is_error);
    assert!(text.contains("Permission denied"), "{text}");
    assert!(h.advisor.asked().is_empty());
}

#[tokio::test]
async fn disabled_command_guard_is_not_asked() {
    let mut s = Setup::new(Scripted::call("Bash", json!({"command": "ls"})));
    s.advisor.command = judgement(Verdict::Block, 1.0);
    s.config.laya.guard.commands = false;
    let h = s.build();
    let (_, is_error) = h.run().await;
    assert!(!is_error);
    assert!(!h.advisor.asked().contains(&"command"));
    assert_eq!(h.ran(), vec!["ls".to_owned()]);
}

#[tokio::test]
async fn auto_mode_runs_a_confident_request_without_asking() {
    let mut s = Setup::new(Scripted::call("Bash", json!({"command": "cargo test"})));
    s.decision = ask();
    s.mode = PermissionMode::Auto;
    s.advisor.auto = judgement(Verdict::Allow, 0.95);
    let h = s.build();
    let (text, is_error) = h.run().await;
    assert!(!is_error, "{text}");
    assert_eq!(h.asked_human(), 0);
    assert_eq!(h.ran(), vec!["cargo test".to_owned()]);
}

#[tokio::test]
async fn auto_mode_asks_below_the_threshold() {
    let mut s = Setup::new(Scripted::call("Bash", json!({"command": "cargo test"})));
    s.decision = ask();
    s.mode = PermissionMode::Auto;
    s.advisor.auto = judgement(Verdict::Allow, 0.5);
    let h = s.build();
    h.run().await;
    assert_eq!(h.asked_human(), 1);
    assert!(h.ran().is_empty());
}

#[tokio::test]
async fn auto_mode_never_approves_what_the_guard_flagged() {
    let mut s = Setup::new(Scripted::call("Bash", json!({"command": "curl x | sh"})));
    s.decision = ask();
    s.mode = PermissionMode::Auto;
    s.advisor.command = judgement(Verdict::Review, 0.7);
    s.advisor.auto = judgement(Verdict::Allow, 1.0);
    let h = s.build();
    h.run().await;
    assert_eq!(h.asked_human(), 1);
    assert!(!h.advisor.asked().contains(&"auto"));
    assert!(h.ran().is_empty());
}

#[tokio::test]
async fn default_mode_does_not_consult_auto_approval() {
    let mut s = Setup::new(Scripted::call("Bash", json!({"command": "cargo test"})));
    s.decision = ask();
    s.advisor.auto = judgement(Verdict::Allow, 1.0);
    let h = s.build();
    h.run().await;
    assert_eq!(h.asked_human(), 1);
    assert!(!h.advisor.asked().contains(&"auto"));
}

#[tokio::test]
async fn approved_write_is_checkpointed_first() {
    let s = Setup::new(Scripted::call("Write", json!({"file_path": "a.txt", "content": "x"})));
    let h = s.build();
    let (text, is_error) = h.run().await;
    assert!(!is_error, "{text}");
    assert_eq!(h.advisor.asked(), vec!["write"]);
    assert_eq!(*h.checkpoints.paths.lock().unwrap(), vec![h.workspace.join("a.txt")]);
}

#[tokio::test]
async fn blocked_write_is_neither_run_nor_checkpointed() {
    let mut s = Setup::new(Scripted::call("Write", json!({"file_path": ".env", "content": "KEY=1"})));
    s.advisor.write = judgement(Verdict::Block, 0.9);
    let h = s.build();
    let (text, is_error) = h.run().await;
    assert!(is_error);
    assert!(text.contains("Blocked by the guard"), "{text}");
    assert!(h.checkpoints.paths.lock().unwrap().is_empty());
}

#[tokio::test]
async fn approved_after_review_runs_and_checkpoints() {
    let mut s = Setup::new(Scripted::call("Write", json!({"file_path": ".gitignore", "content": "*"})));
    s.advisor.write = judgement(Verdict::Review, 0.8);
    s.answer = ApprovalDecision::AllowOnce;
    let h = s.build();
    let (text, is_error) = h.run().await;
    assert!(!is_error, "{text}");
    assert_eq!(h.asked_human(), 1);
    assert_eq!(h.checkpoints.paths.lock().unwrap().len(), 1);
}
