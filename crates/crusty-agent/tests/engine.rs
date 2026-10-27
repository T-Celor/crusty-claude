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

enum Step {
    Text(&'static str),
    Call(&'static str, Value),
    Fail(u16),
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
        _d: crusty_core::provider::DeltaSink<'_>,
    ) -> Result<ChatResponse, ProviderError> {
        self.seen.lock().unwrap().push(req.clone());
        let step = self.steps.lock().unwrap().pop_front().unwrap_or(Step::Text("(script exhausted)"));
        let usage = Usage { input_tokens: 100, output_tokens: 10, ..Usage::default() };
        let (content, stop_reason) = match step {
            Step::Text(t) => (vec![ContentBlock::text(t)], StopReason::EndTurn),
            Step::Call(name, input) => (vec![ContentBlock::tool_use(format!("call-{name}"), name, input)], StopReason::ToolUse),
            Step::Fail(status) => return Err(ProviderError::Http { status, message: "scripted".into() }),
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

struct Policy(PermissionDecision);

impl PermissionPolicy for Policy {
    fn evaluate(&self, _tool: &dyn Tool, _input: &Value, _ctx: &ToolContext) -> PermissionDecision {
        self.0.clone()
    }
    fn mode(&self) -> PermissionMode {
        PermissionMode::Default
    }
    fn set_mode(&self, _mode: PermissionMode) {}
    fn add_allow_rule(&self, _rule: &str) -> Result<(), String> {
        Ok(())
    }
    fn describe(&self) -> String {
        String::new()
    }
}

fn runtime(
    provider: Arc<Scripted>,
    config: Config,
    decision: PermissionDecision,
) -> (Arc<Runtime>, tokio::sync::mpsc::UnboundedReceiver<AppEvent>) {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let dir = std::env::temp_dir();
    let mut providers: BTreeMap<String, Arc<dyn Provider>> = BTreeMap::new();
    providers.insert("anthropic".into(), provider);
    let rt = Runtime::new(RuntimeParts {
        config: Arc::new(config),
        providers,
        tools: vec![Arc::new(Echo)],
        permissions: Arc::new(Policy(decision)),
        approver: Arc::new(FixedApprover(ApprovalDecision::Deny)),
        hooks: Arc::new(NoopHooks),
        optimizer: Arc::new(NoopOptimizer),
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
