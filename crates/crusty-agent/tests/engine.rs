//! End-to-end tests of the orchestrator/executor loop with a scripted provider.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};

use crusty_agent::{ORCHESTRATOR_ID, PolicyBundle, Runtime, RuntimeParts, StopKind, orchestrator_spec, run_agent};
use crusty_core::filter::NoFilter;
use crusty_core::hooks::NoopHooks;
use crusty_core::optimize::NoopOptimizer;
use crusty_core::permission::FixedApprover;
use crusty_core::{
    AppEvent, ApprovalDecision, CallOptions, CancellationToken, ChatRequest, ChatResponse, Config, ContentBlock, EventSink, Message,
    PermissionDecision, PermissionMode, PermissionPolicy, Provider, ProviderError, ProviderKind, RiskAssessment, RiskLevel, Role,
    StopReason, Tool, ToolCategory, ToolContext, ToolError, ToolOutput, ToolSpec, Usage, async_trait,
};
use serde_json::{Value, json};

use crusty_agent::{ChannelApprover, Controller};
use crusty_core::advisor::{
    AdviceSource, Advisor, Intent, IntentKind, Judgement, McpServerSummary, Routing, Scores, SecretCandidate, Tier,
};
use crusty_core::checkpoint::{CheckpointSummary, Checkpointer};
use crusty_core::hooks::{HookEvent, HookOutcome, HookRunner};
use crusty_core::optimize::{OptimizeContext, OptimizeOutcome, RequestOptimizer};
use crusty_core::tool::PlannedChange;
use crusty_core::vault::RedactionKind;
use crusty_core::{ApprovalRequest, UiCommand};
use crusty_core::{SecretVault, StreamDelta};

enum Step {
    Text(&'static str),
    Call(&'static str, Value),
    Fail(u16),
    /// Streams some text, then fails.
    PartialFail(&'static str, u16),
}

struct Scripted {
    steps: Mutex<VecDeque<Step>>,
    seen: Mutex<Vec<ChatRequest>>,
}

impl Scripted {
    fn new(steps: Vec<Step>) -> Arc<Self> {
        Arc::new(Scripted { steps: Mutex::new(steps.into()), seen: Mutex::new(Vec::new()) })
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
        d: crusty_core::provider::DeltaSink<'_>,
    ) -> Result<ChatResponse, ProviderError> {
        self.seen.lock().unwrap().push(req.clone());
        let step = self.steps.lock().unwrap().pop_front().unwrap_or(Step::Text("(script exhausted)"));
        let usage = Usage { input_tokens: 100, output_tokens: 10, ..Usage::default() };
        let (content, stop_reason) = match step {
            Step::Text(t) => (vec![ContentBlock::text(t)], StopReason::EndTurn),
            Step::Call(name, input) => (vec![ContentBlock::tool_use(format!("call-{name}"), name, input)], StopReason::ToolUse),
            Step::Fail(status) => return Err(ProviderError::Http { status, message: "scripted".into() }),
            Step::PartialFail(text, status) => {
                d(StreamDelta::Text(text.to_owned()));
                return Err(ProviderError::Http { status, message: "scripted".into() });
            }
        };
        Ok(ChatResponse { id: None, model: req.model.clone(), content, stop_reason, usage, stop_details: None })
    }
}

struct Echo;

#[async_trait]
impl Tool for Echo {
    fn name(&self) -> &str {
        "Echo"
    }
    fn spec(&self) -> ToolSpec {
        ToolSpec::new("Echo", "echo", json!({"type": "object"}))
    }
    fn category(&self) -> ToolCategory {
        ToolCategory::Read
    }
    fn assess(&self, _input: &Value, _ctx: &ToolContext) -> RiskAssessment {
        RiskAssessment::new(RiskLevel::Low, "read")
    }
    async fn call(&self, input: Value, _ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput::text(format!("echo {input}")))
    }
}

struct Policy(PermissionDecision, PermissionMode);

impl PermissionPolicy for Policy {
    fn evaluate(&self, _tool: &dyn Tool, _input: &Value, _ctx: &ToolContext) -> PermissionDecision {
        self.0.clone()
    }
    fn mode(&self) -> PermissionMode {
        self.1
    }
    fn set_mode(&self, _mode: PermissionMode) {}
    fn add_allow_rule(&self, _rule: &str) -> Result<(), String> {
        Ok(())
    }
    fn describe(&self) -> String {
        String::new()
    }
}

/// Services a test runtime is built with; `default_services` gives the no-op ones.
struct Services {
    tools: Vec<Arc<dyn Tool>>,
    optimizer: Arc<dyn RequestOptimizer>,
    advisor: Arc<dyn Advisor>,
    checkpoints: Arc<dyn Checkpointer>,
    hooks: Arc<dyn HookRunner>,
    vault: Arc<SecretVault>,
    mode: PermissionMode,
}

fn default_services() -> Services {
    Services {
        tools: vec![Arc::new(Echo)],
        optimizer: Arc::new(NoopOptimizer),
        advisor: Arc::new(crusty_core::NoAdvisor),
        checkpoints: Arc::new(crusty_core::NoCheckpoints),
        hooks: Arc::new(NoopHooks),
        vault: Arc::new(SecretVault::default()),
        mode: PermissionMode::Default,
    }
}

fn runtime(
    provider: Arc<Scripted>,
    config: Config,
    decision: PermissionDecision,
) -> (Arc<Runtime>, tokio::sync::mpsc::UnboundedReceiver<AppEvent>) {
    runtime_with(provider, config, decision, default_services())
}

fn runtime_with(
    provider: Arc<Scripted>,
    config: Config,
    decision: PermissionDecision,
    services: Services,
) -> (Arc<Runtime>, tokio::sync::mpsc::UnboundedReceiver<AppEvent>) {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let dir = std::env::temp_dir();
    let mut providers: BTreeMap<String, Arc<dyn Provider>> = BTreeMap::new();
    providers.insert("anthropic".into(), provider);
    let rt = Runtime::new(RuntimeParts {
        config: Arc::new(config),
        providers,
        tools: services.tools,
        permissions: Arc::new(Policy(decision, services.mode)),
        approver: Arc::new(FixedApprover(ApprovalDecision::Deny)),
        hooks: services.hooks,
        optimizer: services.optimizer,
        filters: Arc::new(NoFilter),
        events: EventSink::new(tx),
        policy: PolicyBundle {
            name: "ponytail".into(),
            version: "test".into(),
            texts: BTreeMap::from([("full".into(), "# Ponytail test".into())]),
        },
        workspace: dir.clone(),
        home: dir,
        session_id: "s1".into(),
        store: Arc::new(crusty_core::OriginalStore::default()),
        advisor: services.advisor,
        checkpoints: services.checkpoints,
        vault: services.vault,
    });
    (rt, rx)
}

fn allow() -> PermissionDecision {
    PermissionDecision::Allow { reason: "test".into() }
}

fn config_with_echo() -> Config {
    let mut c = Config::default();
    c.orchestrator.direct_tools = vec!["Echo".into(), "Task".into()];
    c
}

/// Advisor with fixed answers: the tier it routes to, and the relevance of every MCP server.
struct FixedAdvisor {
    tier: Tier,
    relevance: Vec<f64>,
}

#[async_trait]
impl Advisor for FixedAdvisor {
    fn name(&self) -> &str {
        "fixed"
    }
    async fn judge_command(&self, _command: &str, _cwd: &std::path::Path, _goal: &str) -> Judgement {
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
        Routing { tier: self.tier, confidence: 0.9, source: AdviceSource::Laya }
    }
    async fn intent(&self, _prompt: &str, _servers: &[McpServerSummary]) -> Intent {
        Intent { kind: IntentKind::Action, confidence: 0.9, server_relevance: self.relevance.clone(), source: AdviceSource::Laya }
    }
    async fn auto_approve(&self, _request: &ApprovalRequest, _goal: &str) -> Judgement {
        Judgement::abstain()
    }
}

/// An MCP tool: `mcp__docs__search` style names are deferred by intent detection.
struct Named(&'static str);

#[async_trait]
impl Tool for Named {
    fn name(&self) -> &str {
        self.0
    }
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(self.0, "search the docs", json!({"type": "object"}))
    }
    fn category(&self) -> ToolCategory {
        ToolCategory::Read
    }
    fn assess(&self, _input: &Value, _ctx: &ToolContext) -> RiskAssessment {
        RiskAssessment::new(RiskLevel::Low, "read")
    }
    async fn call(&self, _input: Value, _ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput::text("found"))
    }
}

/// A `Write` tool whose change lands at `file_path`, or inside the project by default.
struct ProjectWrite;

#[async_trait]
impl Tool for ProjectWrite {
    fn name(&self) -> &str {
        "Write"
    }
    fn spec(&self) -> ToolSpec {
        ToolSpec::new("Write", "write a file", json!({"type": "object"}))
    }
    fn category(&self) -> ToolCategory {
        ToolCategory::Read
    }
    fn assess(&self, _input: &Value, _ctx: &ToolContext) -> RiskAssessment {
        RiskAssessment::new(RiskLevel::Low, "write")
    }
    fn planned_change(&self, input: &Value, ctx: &ToolContext) -> Option<PlannedChange> {
        let path = match input.get("file_path").and_then(Value::as_str) {
            Some(p) => std::path::PathBuf::from(p),
            None => ctx.workspace.join("notes.txt"),
        };
        Some(PlannedChange { path, before: None, after: String::new() })
    }
    async fn call(&self, _input: Value, _ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput::text("found"))
    }
}

#[tokio::test]
async fn masked_secret_written_outside_the_project_is_asked() {
    let (vault, placeholder) = secret_vault();
    let outside = std::env::temp_dir().join("crusty-outside-project").join("leak.txt");
    let p = Scripted::new(vec![
        Step::Call("Write", json!({"content": placeholder, "file_path": outside.display().to_string()})),
        Step::Text("ok"),
    ]);
    let services = Services { tools: vec![Arc::new(ProjectWrite)], vault, ..default_services() };
    let (rt, _rx) = runtime_with(p, config_with(&["Write"]), allow(), services);
    let spec = orchestrator_spec(&rt);
    let mut msgs = vec![Message::user_text("x")];
    run_agent(&rt, &spec, &mut msgs, &CancellationToken::new()).await;
    // The harness approver denies, so a call that is asked comes back as an error.
    let (is_error, _) = first_result(&msgs);
    assert!(is_error, "a secret written outside the project must be asked");
}

/// Records the tools whose results went through `after_tool`.
#[derive(Default)]
struct Recording(Mutex<Vec<String>>);

#[async_trait]
impl RequestOptimizer for Recording {
    async fn optimize(&self, _req: &mut ChatRequest, _ctx: &OptimizeContext) -> OptimizeOutcome {
        OptimizeOutcome::default()
    }
    async fn after_tool(&self, tool: &str, _input: &Value, _output: &mut ToolOutput, _goal: &str, _ctx: &OptimizeContext) {
        self.0.lock().unwrap().push(tool.to_owned());
    }
}

/// One checkpoint, taken before the first prompt (conversation index 0).
struct Snapshots;

impl Checkpointer for Snapshots {
    fn before_change(&self, _path: &std::path::Path) -> Result<(), String> {
        Ok(())
    }
    fn begin(&self, _label: &str, _message_index: usize) -> Option<u32> {
        Some(1)
    }
    fn list(&self) -> Vec<CheckpointSummary> {
        vec![CheckpointSummary { id: 1, label: "first".into(), files: 0, created: "now".into(), message_index: 0 }]
    }
}

/// A controller over one conversation; commands and prompts go through its UI channel.
fn spawn_controller(rt: &Arc<Runtime>) -> (tokio::sync::mpsc::UnboundedSender<UiCommand>, tokio::task::JoinHandle<()>) {
    let approver = Arc::new(ChannelApprover::new(rt.events.clone()));
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let ctl = Controller::new(rt.clone(), approver, Vec::new(), Vec::new());
    (tx, tokio::spawn(ctl.run(rx)))
}

async fn wait_for(rx: &mut tokio::sync::mpsc::UnboundedReceiver<AppEvent>, done: impl Fn(&AppEvent) -> bool) {
    while let Some(ev) = rx.recv().await {
        if done(&ev) {
            return;
        }
    }
    panic!("event channel closed before the expected event");
}

#[tokio::test]
async fn router_applies_tier_unless_pinned() {
    let p = Scripted::new(vec![Step::Text("ok")]);
    let services = Services { advisor: Arc::new(FixedAdvisor { tier: Tier::Hard, relevance: Vec::new() }), ..default_services() };
    let (rt, mut rx) = runtime_with(p.clone(), config_with_echo(), allow(), services);
    let (tx, task) = spawn_controller(&rt);
    tx.send(UiCommand::Submit("refactor the parser".into())).unwrap();
    wait_for(&mut rx, |e| matches!(e, AppEvent::TurnFinished { .. })).await;
    assert_eq!(rt.settings().orchestrator.model, "claude-opus-5-5");
    assert_eq!(rt.settings().routed_tier.as_deref(), Some("hard"));
    let asked = p.seen.lock().unwrap()[0].model.clone();
    assert_eq!(asked, "claude-opus-5-5");
    tx.send(UiCommand::Quit).unwrap();
    task.await.unwrap();

    // A pinned model is left alone.
    let p = Scripted::new(vec![Step::Text("ok")]);
    let services = Services { advisor: Arc::new(FixedAdvisor { tier: Tier::Hard, relevance: Vec::new() }), ..default_services() };
    let (rt, mut rx) = runtime_with(p.clone(), config_with_echo(), allow(), services);
    rt.update_settings(|s| s.orchestrator_pinned = true);
    let before = rt.settings().orchestrator.model;
    let (tx, task) = spawn_controller(&rt);
    tx.send(UiCommand::Submit("refactor the parser".into())).unwrap();
    wait_for(&mut rx, |e| matches!(e, AppEvent::TurnFinished { .. })).await;
    assert_eq!(rt.settings().orchestrator.model, before);
    let asked = p.seen.lock().unwrap()[0].model.clone();
    assert_eq!(asked, before);
    tx.send(UiCommand::Quit).unwrap();
    task.await.unwrap();
}

#[tokio::test]
async fn deferred_mcp_tools_load_through_tool_search() {
    let mut config = config_with_echo();
    config.orchestrator.direct_tools = vec!["Echo".into(), "Task".into(), "ToolSearch".into(), "mcp__docs__search".into()];
    let p = Scripted::new(vec![Step::Call("ToolSearch", json!({"query": "select:docs"})), Step::Text("done")]);
    let tools: Vec<Arc<dyn Tool>> = vec![Arc::new(Echo), Arc::new(Named("mcp__docs__search"))];
    let services = Services { tools, advisor: Arc::new(FixedAdvisor { tier: Tier::Routine, relevance: vec![0.0] }), ..default_services() };
    let (rt, mut rx) = runtime_with(p.clone(), config, allow(), services);
    let (tx, task) = spawn_controller(&rt);
    tx.send(UiCommand::Submit("list the things".into())).unwrap();
    wait_for(&mut rx, |e| matches!(e, AppEvent::TurnFinished { .. })).await;
    tx.send(UiCommand::Quit).unwrap();
    task.await.unwrap();

    let seen = p.seen.lock().unwrap();
    let names = |r: &ChatRequest| r.tools.iter().map(|t| t.name.clone()).collect::<Vec<String>>();
    let first = names(&seen[0]);
    assert!(first.iter().any(|n| n == "ToolSearch"));
    assert!(!first.iter().any(|n| n == "mcp__docs__search"));
    assert!(names(&seen[1]).iter().any(|n| n == "mcp__docs__search"));
}

#[tokio::test]
async fn after_tool_shortens_tool_output() {
    let rec = Arc::new(Recording::default());
    let p = Scripted::new(vec![Step::Call("Echo", json!({})), Step::Text("ok")]);
    let services = Services { optimizer: rec.clone(), ..default_services() };
    let (rt, _rx) = runtime_with(p, config_with_echo(), allow(), services);
    let spec = orchestrator_spec(&rt);
    let mut msgs = vec![Message::user_text("x")];
    run_agent(&rt, &spec, &mut msgs, &CancellationToken::new()).await;
    assert_eq!(*rec.0.lock().unwrap(), vec!["Echo".to_string()]);
}

#[tokio::test]
async fn rewind_truncates_the_conversation() {
    let p = Scripted::new(vec![Step::Text("one"), Step::Text("two")]);
    let services = Services { checkpoints: Arc::new(Snapshots), ..default_services() };
    let (rt, mut rx) = runtime_with(p.clone(), config_with_echo(), allow(), services);
    let (tx, task) = spawn_controller(&rt);
    tx.send(UiCommand::Submit("first".into())).unwrap();
    wait_for(&mut rx, |e| matches!(e, AppEvent::TurnFinished { .. })).await;
    tx.send(UiCommand::Submit("/rewind 1 conversation".into())).unwrap();
    wait_for(&mut rx, |e| matches!(e, AppEvent::Rewound { .. })).await;
    tx.send(UiCommand::Submit("second".into())).unwrap();
    wait_for(&mut rx, |e| matches!(e, AppEvent::TurnFinished { .. })).await;
    tx.send(UiCommand::Quit).unwrap();
    task.await.unwrap();
    // Only the new prompt is left before the second request.
    let len = p.seen.lock().unwrap()[1].messages.len();
    assert_eq!(len, 1);
}

#[tokio::test]
async fn orchestrator_delegates_and_finishes() {
    let p = Scripted::new(vec![
        Step::Call("Task", json!({"domain": "coder", "description": "do it", "prompt": "write the thing"})),
        Step::Text("executor report: done"),
        Step::Text("all good"),
    ]);
    let (rt, mut rx) = runtime(p.clone(), config_with_echo(), allow());
    let spec = orchestrator_spec(&rt);
    let mut msgs = vec![Message::user_text("build it")];
    let out = run_agent(&rt, &spec, &mut msgs, &CancellationToken::new()).await;
    assert_eq!(out.stop, StopKind::Done);
    assert_eq!(out.text, "all good");
    let seen = p.seen.lock().unwrap();
    // executor ran on the coder domain's model with the delegated prompt and the policy injected
    assert_eq!(seen[1].model, "claude-sonnet-5-5");
    assert_eq!(seen[1].messages[0].text(), "write the thing");
    assert!(seen[1].system.iter().any(|b| b.as_text() == Some("# Ponytail test")));
    assert!(seen[0].system.iter().any(|b| b.as_text() == Some("# Ponytail test")));
    // the executor's report came back to the orchestrator as the Task result
    let result = &msgs[2].content[0];
    match result {
        ContentBlock::ToolResult { content, is_error, .. } => {
            assert!(!is_error);
            assert!(crusty_core::message::blocks_text(content).contains("executor report: done"));
        }
        other => panic!("unexpected {other:?}"),
    }
    let mut spawned = false;
    while let Ok(ev) = rx.try_recv() {
        if let AppEvent::AgentSpawned { domain, .. } = ev {
            spawned = domain == "coder";
        }
    }
    assert!(spawned);
}

#[tokio::test]
async fn orchestrator_cannot_use_executor_tools() {
    let p = Scripted::new(vec![Step::Call("Bash", json!({"command": "ls"})), Step::Text("ok")]);
    let (rt, _rx) = runtime(p, config_with_echo(), allow());
    let spec = orchestrator_spec(&rt);
    let mut msgs = vec![Message::user_text("x")];
    run_agent(&rt, &spec, &mut msgs, &CancellationToken::new()).await;
    let text = msgs[2].content.iter().find_map(|b| match b {
        ContentBlock::ToolResult { content, is_error: true, .. } => Some(crusty_core::message::blocks_text(content)),
        _ => None,
    });
    assert!(text.unwrap().contains("Delegate it with Task"));
}

#[tokio::test]
async fn denied_and_asked_calls_return_errors() {
    for decision in
        [PermissionDecision::Deny { reason: "nope".into() }, PermissionDecision::Ask { reason: "ask".into(), suggested_rule: None }]
    {
        let p = Scripted::new(vec![Step::Call("Echo", json!({"a": 1})), Step::Text("ok")]);
        let (rt, _rx) = runtime(p, config_with_echo(), decision);
        let spec = orchestrator_spec(&rt);
        let mut msgs = vec![Message::user_text("x")];
        run_agent(&rt, &spec, &mut msgs, &CancellationToken::new()).await;
        assert!(matches!(&msgs[2].content[0], ContentBlock::ToolResult { is_error: true, .. }));
    }
}

#[tokio::test]
async fn allowed_tool_runs() {
    let p = Scripted::new(vec![Step::Call("Echo", json!({"a": 1})), Step::Text("ok")]);
    let (rt, _rx) = runtime(p, config_with_echo(), allow());
    let spec = orchestrator_spec(&rt);
    let mut msgs = vec![Message::user_text("x")];
    let out = run_agent(&rt, &spec, &mut msgs, &CancellationToken::new()).await;
    assert_eq!(out.turns, 2);
    match &msgs[2].content[0] {
        ContentBlock::ToolResult { content, is_error: false, .. } => {
            assert!(crusty_core::message::blocks_text(content).starts_with("echo"))
        }
        other => panic!("unexpected {other:?}"),
    }
    assert_eq!(msgs[2].role, Role::User);
}

#[tokio::test]
async fn invalid_tool_json_is_reported() {
    let p = Scripted::new(vec![Step::Call("Echo", json!({"__crusty_invalid_json": "{\"a"})), Step::Text("ok")]);
    let (rt, _rx) = runtime(p, config_with_echo(), allow());
    let spec = orchestrator_spec(&rt);
    let mut msgs = vec![Message::user_text("x")];
    run_agent(&rt, &spec, &mut msgs, &CancellationToken::new()).await;
    match &msgs[2].content[0] {
        ContentBlock::ToolResult { content, is_error: true, .. } => {
            assert!(crusty_core::message::blocks_text(content).starts_with("INVALID_JSON"))
        }
        other => panic!("unexpected {other:?}"),
    }
}

#[tokio::test]
async fn unavailable_model_falls_back() {
    let p = Scripted::new(vec![Step::Fail(529), Step::Text("from fallback")]);
    let mut c = config_with_echo();
    c.orchestrator.model.fallback = vec!["anthropic:claude-sonnet-5-5".into()];
    let (rt, mut rx) = runtime(p.clone(), c, allow());
    let spec = orchestrator_spec(&rt);
    let mut msgs = vec![Message::user_text("x")];
    let out = run_agent(&rt, &spec, &mut msgs, &CancellationToken::new()).await;
    assert_eq!(out.text, "from fallback");
    assert_eq!(out.model, "anthropic:claude-sonnet-5-5");
    assert_eq!(p.seen.lock().unwrap()[1].model, "claude-sonnet-5-5");
    let mut fell_back = false;
    while let Ok(ev) = rx.try_recv() {
        fell_back |= matches!(ev, AppEvent::ModelFallback { .. });
    }
    assert!(fell_back);
}

#[tokio::test]
async fn budget_and_step_limits_stop_the_agent() {
    let p = Scripted::new(vec![Step::Call("Echo", json!({})), Step::Call("Echo", json!({})), Step::Call("Echo", json!({}))]);
    let mut c = config_with_echo();
    c.orchestrator.max_steps = 2;
    let (rt, _rx) = runtime(p, c, allow());
    let spec = orchestrator_spec(&rt);
    let mut msgs = vec![Message::user_text("x")];
    assert_eq!(run_agent(&rt, &spec, &mut msgs, &CancellationToken::new()).await.stop, StopKind::StepLimit);

    let p = Scripted::new(vec![Step::Call("Echo", json!({})), Step::Text("never")]);
    let mut c = config_with_echo();
    c.orchestrator.model.token_budget = Some(50);
    let (rt, _rx) = runtime(p, c, allow());
    let spec = orchestrator_spec(&rt);
    let mut msgs = vec![Message::user_text("x")];
    assert!(matches!(run_agent(&rt, &spec, &mut msgs, &CancellationToken::new()).await.stop, StopKind::Budget(_)));
}

#[tokio::test]
async fn cancelled_before_start() {
    let p = Scripted::new(vec![Step::Text("never")]);
    let (rt, _rx) = runtime(p, config_with_echo(), allow());
    let spec = orchestrator_spec(&rt);
    let cancel = CancellationToken::new();
    cancel.cancel();
    let mut msgs = vec![Message::user_text("x")];
    assert_eq!(run_agent(&rt, &spec, &mut msgs, &cancel).await.stop, StopKind::Cancelled);
    assert_eq!(spec.id, ORCHESTRATOR_ID);
}

#[tokio::test]
async fn review_loops_are_limited() {
    let review = || Step::Call("Task", json!({"domain": "reviewer", "description": "review", "prompt": "review it"}));
    let mut steps = Vec::new();
    for _ in 0..4 {
        steps.push(review());
        steps.push(Step::Text("looks fine"));
    }
    steps.push(Step::Text("end"));
    let p = Scripted::new(steps);
    let mut c = config_with_echo();
    c.orchestrator.max_review_loops = 1;
    let (rt, _rx) = runtime(p, c, allow());
    let spec = orchestrator_spec(&rt);
    let mut msgs = vec![Message::user_text("x")];
    run_agent(&rt, &spec, &mut msgs, &CancellationToken::new()).await;
    let denied = msgs.iter().flat_map(|m| &m.content).any(|b| match b {
        ContentBlock::ToolResult { content, is_error: true, .. } => {
            crusty_core::message::blocks_text(content).contains("review loop limit")
        }
        _ => false,
    });
    assert!(denied);
}

/// `(is_error, text)` of the first tool result of a run (`msgs[2]`).
fn first_result(msgs: &[Message]) -> (bool, String) {
    match &msgs[2].content[0] {
        ContentBlock::ToolResult { content, is_error, .. } => (*is_error, crusty_core::message::blocks_text(content)),
        other => panic!("unexpected {other:?}"),
    }
}

fn config_with(tools: &[&str]) -> Config {
    let mut c = Config::default();
    c.orchestrator.direct_tools = tools.iter().map(|t| (*t).to_owned()).collect();
    c
}

/// A vault holding one masked secret, and its placeholder.
fn secret_vault() -> (Arc<SecretVault>, String) {
    let vault = Arc::new(SecretVault::default());
    let placeholder = vault.placeholder("AKIAIOSFODNN7EXAMPLE", RedactionKind::Secret);
    (vault, placeholder)
}

/// Records every shell command the guard was shown.
#[derive(Default)]
struct JudgingCommands(Mutex<Vec<String>>);

#[async_trait]
impl Advisor for JudgingCommands {
    fn name(&self) -> &str {
        "judging"
    }
    async fn judge_command(&self, command: &str, _cwd: &std::path::Path, _goal: &str) -> Judgement {
        self.0.lock().unwrap().push(command.to_owned());
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
        Routing { tier: Tier::Routine, confidence: 0.0, source: AdviceSource::None }
    }
    async fn intent(&self, _prompt: &str, _servers: &[McpServerSummary]) -> Intent {
        Intent { kind: IntentKind::Action, confidence: 0.0, server_relevance: Vec::new(), source: AdviceSource::None }
    }
    async fn auto_approve(&self, _request: &ApprovalRequest, _goal: &str) -> Judgement {
        Judgement::abstain()
    }
}

/// Adds a rewrite to every request, as the real pipeline does.
struct Marking;

#[async_trait]
impl RequestOptimizer for Marking {
    async fn optimize(&self, req: &mut ChatRequest, _ctx: &OptimizeContext) -> OptimizeOutcome {
        req.system.push(ContentBlock::text("OPTIMIZED"));
        OptimizeOutcome::default()
    }
}

/// Replaces every tool result with a short line, as Triage and Rerank do.
struct Shrinking;

#[async_trait]
impl RequestOptimizer for Shrinking {
    async fn optimize(&self, _req: &mut ChatRequest, _ctx: &OptimizeContext) -> OptimizeOutcome {
        OptimizeOutcome::default()
    }
    async fn after_tool(&self, _tool: &str, _input: &Value, output: &mut ToolOutput, _goal: &str, _ctx: &OptimizeContext) {
        output.content = vec![ContentBlock::text("short")];
    }
}

/// Adds context to every PostToolUse event.
struct PostHook;

#[async_trait]
impl HookRunner for PostHook {
    async fn run(&self, event: HookEvent, _tool: Option<&str>, _payload: &Value) -> HookOutcome {
        match event {
            HookEvent::PostToolUse => HookOutcome { additional_context: Some("HOOK CONTEXT".into()), ..HookOutcome::default() },
            _ => HookOutcome::default(),
        }
    }
}

/// Records the sessions whose optimizer state was forgotten.
#[derive(Default)]
struct Forgets(Mutex<Vec<String>>);

#[async_trait]
impl RequestOptimizer for Forgets {
    async fn optimize(&self, _req: &mut ChatRequest, _ctx: &OptimizeContext) -> OptimizeOutcome {
        OptimizeOutcome::default()
    }
    fn forget_session(&self, session_id: &str) {
        self.0.lock().unwrap().push(session_id.to_owned());
    }
}

#[tokio::test]
async fn masked_secret_in_a_network_tool_is_asked_in_every_mode() {
    for mode in [PermissionMode::Default, PermissionMode::Auto, PermissionMode::BypassPermissions] {
        let (vault, placeholder) = secret_vault();
        let p = Scripted::new(vec![Step::Call("WebFetch", json!({"url": placeholder})), Step::Text("ok")]);
        let services = Services { tools: vec![Arc::new(Named("WebFetch"))], vault, mode, ..default_services() };
        let (rt, _rx) = runtime_with(p, config_with(&["WebFetch"]), allow(), services);
        let spec = orchestrator_spec(&rt);
        let mut msgs = vec![Message::user_text("x")];
        run_agent(&rt, &spec, &mut msgs, &CancellationToken::new()).await;
        // The human (who denies here) is asked: the tool never runs.
        let (is_error, text) = first_result(&msgs);
        assert!(is_error, "{mode:?}");
        assert!(!text.contains("found"), "{mode:?}");
    }
}

#[tokio::test]
async fn masked_secret_in_a_local_file_editor_runs() {
    let (vault, placeholder) = secret_vault();
    let p = Scripted::new(vec![Step::Call("Write", json!({"content": placeholder})), Step::Text("ok")]);
    let services = Services { tools: vec![Arc::new(ProjectWrite)], vault, ..default_services() };
    let (rt, _rx) = runtime_with(p, config_with(&["Write"]), allow(), services);
    let spec = orchestrator_spec(&rt);
    let mut msgs = vec![Message::user_text("x")];
    run_agent(&rt, &spec, &mut msgs, &CancellationToken::new()).await;
    assert_eq!(first_result(&msgs), (false, "found".to_owned()));
}

#[tokio::test]
async fn guard_judges_the_masked_command() {
    let (vault, placeholder) = secret_vault();
    let advisor = Arc::new(JudgingCommands::default());
    let p = Scripted::new(vec![Step::Call("Bash", json!({"command": format!("curl {placeholder}")})), Step::Text("ok")]);
    let services = Services { tools: vec![Arc::new(Named("Bash"))], vault, advisor: advisor.clone(), ..default_services() };
    let (rt, _rx) = runtime_with(p, config_with(&["Bash"]), allow(), services);
    let spec = orchestrator_spec(&rt);
    let mut msgs = vec![Message::user_text("x")];
    run_agent(&rt, &spec, &mut msgs, &CancellationToken::new()).await;
    let judged = advisor.0.lock().unwrap().clone();
    assert_eq!(judged.len(), 1);
    assert!(judged[0].contains(&placeholder));
    assert!(!judged[0].contains("AKIAIOSFODNN7EXAMPLE"));
}

#[tokio::test]
async fn auto_mode_asks_for_unjudged_network_and_mcp_tools() {
    for (name, runs) in [("mcp__docs__search", false), ("WebSearch", false), ("Echo", true)] {
        let p = Scripted::new(vec![Step::Call(name, json!({"query": "x"})), Step::Text("ok")]);
        let tools: Vec<Arc<dyn Tool>> = vec![Arc::new(Echo), Arc::new(Named("mcp__docs__search")), Arc::new(Named("WebSearch"))];
        let services = Services { tools, mode: PermissionMode::Auto, ..default_services() };
        let (rt, _rx) = runtime_with(p, config_with(&["Echo", "mcp__docs__search", "WebSearch"]), allow(), services);
        let spec = orchestrator_spec(&rt);
        let mut msgs = vec![Message::user_text("x")];
        run_agent(&rt, &spec, &mut msgs, &CancellationToken::new()).await;
        let (is_error, _) = first_result(&msgs);
        assert_eq!(!is_error, runs, "{name}");
    }
}

#[tokio::test]
async fn hook_context_survives_the_optimizer_shortening_the_result() {
    let p = Scripted::new(vec![Step::Call("Echo", json!({})), Step::Text("ok")]);
    let services = Services { optimizer: Arc::new(Shrinking), hooks: Arc::new(PostHook), ..default_services() };
    let (rt, _rx) = runtime_with(p, config_with_echo(), allow(), services);
    let spec = orchestrator_spec(&rt);
    let mut msgs = vec![Message::user_text("x")];
    run_agent(&rt, &spec, &mut msgs, &CancellationToken::new()).await;
    let (_, text) = first_result(&msgs);
    assert!(text.contains("short") && text.contains("HOOK CONTEXT"), "{text}");
}

#[tokio::test]
async fn fallback_request_goes_through_the_optimizer() {
    let p = Scripted::new(vec![Step::Fail(529), Step::Text("from fallback")]);
    let mut c = config_with_echo();
    c.orchestrator.model.fallback = vec!["anthropic:claude-sonnet-5-5".into()];
    let services = Services { optimizer: Arc::new(Marking), ..default_services() };
    let (rt, _rx) = runtime_with(p.clone(), c, allow(), services);
    let spec = orchestrator_spec(&rt);
    let mut msgs = vec![Message::user_text("x")];
    run_agent(&rt, &spec, &mut msgs, &CancellationToken::new()).await;
    let seen = p.seen.lock().unwrap();
    assert!(seen[1].system.iter().any(|b| b.as_text() == Some("OPTIMIZED")));
}

#[tokio::test]
async fn partial_text_of_a_failed_attempt_is_shown_once() {
    let p = Scripted::new(vec![Step::PartialFail("half", 529), Step::Text("from fallback")]);
    let mut c = config_with_echo();
    c.orchestrator.model.fallback = vec!["anthropic:claude-sonnet-5-5".into()];
    let (rt, mut rx) = runtime(p, c, allow());
    let spec = orchestrator_spec(&rt);
    let mut msgs = vec![Message::user_text("x")];
    run_agent(&rt, &spec, &mut msgs, &CancellationToken::new()).await;
    let mut shown = String::new();
    while let Ok(ev) = rx.try_recv() {
        if let AppEvent::TextDelta { text, .. } = ev {
            shown.push_str(&text);
        }
    }
    // The failed attempt's text is shown once; the fallback's answer comes as a whole message.
    assert_eq!(shown, "half");
}

#[tokio::test]
async fn compaction_summary_request_is_masked() {
    let p = Scripted::new(vec![Step::Text("summary")]);
    let mut c = config_with_echo();
    c.session.keep_recent_on_compact = 2;
    let (rt, _rx) = runtime(p.clone(), c, allow());
    let spec = orchestrator_spec(&rt);
    let mut msgs = vec![
        Message::user_text("key AKIAIOSFODNN7EXAMPLE"),
        Message::assistant_text("ok"),
        Message::user_text("next"),
        Message::assistant_text("done"),
    ];
    crusty_agent::compact::compact(&rt, &spec, &mut msgs, None, &CancellationToken::new()).await.unwrap();
    let seen = p.seen.lock().unwrap();
    let sent = seen[0].messages[0].text();
    assert!(!sent.contains("AKIAIOSFODNN7EXAMPLE"));
    assert!(sent.contains("SECRET_"));
}

#[tokio::test]
async fn conversation_rewind_is_refused_after_compaction() {
    let p = Scripted::new(vec![Step::Text("summary"), Step::Text("one")]);
    let services = Services { checkpoints: Arc::new(Snapshots), ..default_services() };
    let mut c = config_with_echo();
    c.session.keep_recent_on_compact = 2;
    let (rt, mut rx) = runtime_with(p, c, allow(), services);
    let spec = orchestrator_spec(&rt);
    let mut msgs = vec![Message::user_text("a"), Message::assistant_text("b"), Message::user_text("c"), Message::assistant_text("d")];
    crusty_agent::compact::compact(&rt, &spec, &mut msgs, None, &CancellationToken::new()).await.unwrap();
    assert_eq!(rt.compacted_through_checkpoint(), 1);
    let (tx, task) = spawn_controller(&rt);
    tx.send(UiCommand::Submit("/rewind 1 conversation".into())).unwrap();
    wait_for(&mut rx, |e| matches!(e, AppEvent::Error { .. })).await;
    tx.send(UiCommand::Quit).unwrap();
    task.await.unwrap();
}

#[tokio::test]
async fn clear_forgets_deferred_servers_and_optimizer_state() {
    let forgets = Arc::new(Forgets::default());
    let services = Services { optimizer: forgets.clone(), ..default_services() };
    let (rt, mut rx) = runtime_with(Scripted::new(vec![]), config_with_echo(), allow(), services);
    rt.set_deferred_servers(std::collections::BTreeSet::from(["docs".to_owned()]));
    let (tx, task) = spawn_controller(&rt);
    tx.send(UiCommand::Submit("/clear".into())).unwrap();
    wait_for(&mut rx, |e| matches!(e, AppEvent::ConversationReset { .. })).await;
    tx.send(UiCommand::Quit).unwrap();
    task.await.unwrap();
    assert!(rt.deferred_servers().is_empty());
    let forgotten = forgets.0.lock().unwrap().clone();
    assert_eq!(forgotten.len(), 2);
    assert_eq!(forgotten[0], "s1");
}
