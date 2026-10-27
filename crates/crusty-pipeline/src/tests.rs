use super::*;
use crusty_core::{ContentBlock, Message, Role};
use tokio::sync::mpsc;

fn ctx(window: u64) -> OptimizeContext {
    OptimizeContext {
        session_id: "s".into(),
        agent_id: "orchestrator".into(),
        provider_id: "anthropic".into(),
        provider_kind: ProviderKind::Anthropic,
        model: "claude-opus-5-5".into(),
        context_window: window,
        supports_images: true,
    }
}

/// Words only (no digits or hex) so the verbatim guard stays quiet; long lines fill rows.
fn dense_block() -> String {
    let line = "let value = compute_result(input, options).map_err(Error::from)?; ".repeat(6);
    (0..40).map(|_| line.clone()).collect::<Vec<_>>().join("\n")
}

fn log_block() -> String {
    let mut s = String::new();
    for _ in 0..300 {
        s.push_str("INFO request handled in the usual way without any surprise\n");
    }
    s.push_str("ERROR connection refused by upstream\n");
    s
}

fn request_with_old_results(block: impl Fn() -> String, old: usize) -> ChatRequest {
    let mut req = ChatRequest::new("claude-opus-5-5", 1000);
    req.messages.push(Message::user_text("start"));
    for i in 0..old {
        let id = format!("t{i}");
        req.messages
            .push(Message { role: Role::Assistant, content: vec![ContentBlock::tool_use(&id, "Read", serde_json::json!({"n": i}))] });
        req.messages.push(Message {
            role: Role::User,
            content: vec![ContentBlock::tool_result(&id, vec![ContentBlock::text(format!("{i}\n{}", block()))], false)],
        });
    }
    for _ in 0..5 {
        req.messages.push(Message::assistant_text("ok"));
        req.messages.push(Message::user_text("next"));
    }
    req
}

fn pipeline(mutate: impl FnOnce(&mut Config)) -> (Pipeline, mpsc::UnboundedReceiver<AppEvent>) {
    let mut config = Config::default();
    mutate(&mut config);
    let (tx, rx) = mpsc::unbounded_channel();
    (Pipeline::new(&config, EventSink::new(tx), Arc::new(OriginalStore::default())), rx)
}

fn has_image(req: &ChatRequest) -> bool {
    req.messages.iter().flat_map(|m| &m.content).any(|b| match b {
        ContentBlock::ToolResult { content, .. } => content.iter().any(|c| matches!(c, ContentBlock::Image { .. })),
        ContentBlock::Image { .. } => true,
        _ => false,
    })
}

fn stage(out: &OptimizeOutcome, s: Stage) -> Option<&StageMetrics> {
    out.stages.iter().find(|m| m.stage == s)
}

#[tokio::test]
async fn disabled_optimizer_passes_through() {
    let (p, _rx) = pipeline(|c| c.optimizer.enabled = false);
    let mut req = request_with_old_results(log_block, 4);
    let original = req.clone();
    let out = p.optimize(&mut req, &ctx(1_000_000)).await;
    assert_eq!(req, original);
    assert!(out.stages.is_empty());
    assert_eq!(out.route, Route::Direct);
}

#[tokio::test]
async fn headroom_compresses_old_logs_and_keeps_originals() {
    let (p, _rx) = pipeline(|c| c.optimizer.headroom.batch_tokens = 100);
    let mut req = request_with_old_results(log_block, 4);
    let out = p.optimize(&mut req, &ctx(1_000_000)).await;
    let m = stage(&out, Stage::Headroom).unwrap();
    assert_eq!(m.status, StageStatus::Applied, "{}", m.detail);
    assert!(out.est_after < out.est_before);
    let text = serde_json::to_string(&req).unwrap();
    assert!(text.contains("ERROR connection refused"), "errors survive compression");
    assert!(!p.store.is_empty());
}

#[tokio::test]
async fn observe_mode_measures_without_changing() {
    let (p, _rx) = pipeline(|c| {
        c.optimizer.mode = OptimizerMode::Observe;
        c.optimizer.headroom.batch_tokens = 100;
    });
    let mut req = request_with_old_results(log_block, 4);
    let original = req.clone();
    let out = p.optimize(&mut req, &ctx(1_000_000)).await;
    assert_eq!(req, original);
    assert_eq!(stage(&out, Stage::Headroom).unwrap().status, StageStatus::Observed);
}

#[tokio::test]
async fn small_requests_go_direct() {
    let (p, mut rx) = pipeline(|_| {});
    let mut req = request_with_old_results(log_block, 1);
    let out = p.optimize(&mut req, &ctx(1_000_000)).await;
    let gate = out.gate.clone().unwrap();
    assert_eq!(gate.route, "direct");
    assert_eq!(gate.reason, "zone_low");
    assert!(stage(&out, Stage::Pxpipe).is_none());
    let mut saw_gate = false;
    while let Ok(ev) = rx.try_recv() {
        saw_gate |= matches!(ev, AppEvent::Gate(_));
    }
    assert!(saw_gate);
}

#[tokio::test]
async fn near_limit_requests_are_imaged_by_native_pxpipe() {
    let (p, _rx) = pipeline(|c| {
        c.optimizer.headroom.enabled = false;
        c.optimizer.context_gate.min_saving_tokens = 100;
        c.optimizer.context_gate.window = Some(10_000);
        c.optimizer.context_gate.dryrun_ms = 300_000;
    });
    let mut req = request_with_old_results(dense_block, 6);
    let out = p.optimize(&mut req, &ctx(1_000_000)).await;
    let gate = out.gate.clone().unwrap();
    assert_eq!(gate.route, "pxpipe", "reason {}", gate.reason);
    let m = stage(&out, Stage::Pxpipe).unwrap();
    assert_eq!(m.status, StageStatus::Applied, "{}", m.detail);
    assert!(has_image(&req));
    assert!(out.est_after < out.est_before);
}

#[tokio::test]
async fn models_outside_the_allowlist_stay_text() {
    let (p, _rx) = pipeline(|c| {
        c.optimizer.headroom.enabled = false;
        c.optimizer.context_gate.window = Some(10_000);
        c.optimizer.context_gate.dryrun_ms = 300_000;
    });
    let mut req = request_with_old_results(dense_block, 6);
    let mut c = ctx(1_000_000);
    c.model = "claude-haiku-5-5".into();
    let out = p.optimize(&mut req, &c).await;
    assert_eq!(out.gate.unwrap().reason, "model_not_allowed");
    assert!(!has_image(&req));
}

#[tokio::test]
async fn external_pxpipe_is_a_route() {
    let (p, _rx) = pipeline(|c| {
        c.optimizer.headroom.enabled = false;
        c.optimizer.context_gate.min_saving_tokens = 100;
        c.optimizer.context_gate.window = Some(10_000);
        c.optimizer.context_gate.dryrun_ms = 300_000;
        c.optimizer.pxpipe.mode = "external".into();
    });
    let mut req = request_with_old_results(dense_block, 6);
    let original = req.clone();
    let out = p.optimize(&mut req, &ctx(1_000_000)).await;
    assert_eq!(out.route, Route::Via { name: "pxpipe".into(), base_url: "http://127.0.0.1:47821".into() });
    assert_eq!(req, original);
}

#[tokio::test]
async fn observe_mode_never_reroutes_to_external_headroom() {
    let (p, _rx) = pipeline(|c| {
        c.optimizer.mode = OptimizerMode::Observe;
        c.optimizer.headroom.mode = "external".into();
    });
    let mut req = request_with_old_results(log_block, 2);
    let out = p.optimize(&mut req, &ctx(1_000_000)).await;
    assert_eq!(out.route, Route::Direct);
    assert_eq!(stage(&out, Stage::Headroom).unwrap().status, StageStatus::Observed);
    p.set_mode(OptimizerMode::Active).unwrap();
    let out = p.optimize(&mut req, &ctx(1_000_000)).await;
    assert!(matches!(out.route, Route::Via { .. }));
}

#[tokio::test]
async fn direct_gate_decisions_are_not_counted_as_applied() {
    let (p, _rx) = pipeline(|_| {});
    let mut req = request_with_old_results(log_block, 1);
    let out = p.optimize(&mut req, &ctx(1_000_000)).await;
    assert_eq!(stage(&out, Stage::ContextGate).unwrap().status, StageStatus::Skipped);
}

#[tokio::test]
async fn stages_switch_at_runtime() {
    let (p, _rx) = pipeline(|c| c.optimizer.headroom.batch_tokens = 100);
    p.set_stage_enabled(Stage::Headroom, false).unwrap();
    assert!(p.set_stage_enabled(Stage::Ponytail, false).is_err());
    let mut req = request_with_old_results(log_block, 4);
    let out = p.optimize(&mut req, &ctx(1_000_000)).await;
    assert!(stage(&out, Stage::Headroom).is_none());
    p.set_mode(OptimizerMode::Observe).unwrap();
    assert_eq!(p.mode(), OptimizerMode::Observe);
    assert!(p.describe().contains("headroom off"));
}

#[tokio::test]
async fn usage_feeds_the_gate_and_measured_savings() {
    let (p, mut rx) = pipeline(|_| {});
    let c = ctx(1_000_000);
    let mut applied = StageMetrics::new(Stage::Headroom, "request", "headroom:log-crush");
    applied.status = StageStatus::Applied;
    applied.tokens_before_est = 1000;
    applied.tokens_after_est = 400;
    let outcome = OptimizeOutcome {
        gate: Some(crusty_core::event::GateInfo { route: "direct".into(), est: 1000, applied: true, ..Default::default() }),
        stages: vec![applied],
        real_before: Some(1200),
        ..OptimizeOutcome::default()
    };
    p.record_usage(&c, &outcome, &Usage { input_tokens: 500, ..Usage::default() });
    let (mut feedback, mut measured) = (false, false);
    while let Ok(ev) = rx.try_recv() {
        match ev {
            AppEvent::GateFeedback { real_context, .. } => feedback = real_context == 500,
            AppEvent::MeasuredSaving { stage, tokens_before, tokens_after } => {
                measured = stage == Stage::Headroom && tokens_before == 1200 && tokens_after == 500
            }
            _ => {}
        }
    }
    assert!(feedback && measured);
}

#[test]
fn filters_follow_switches() {
    let (p, _rx) = pipeline(|_| {});
    let html = "<html><head><title>T</title></head><body><h1>Hello</h1><p>world</p></body></html>";
    assert!(p.html_to_markdown(html, "https://x.test").is_some());
    p.set_stage_enabled(Stage::HtmlToMarkdown, false).unwrap();
    assert!(p.html_to_markdown(html, "https://x.test").is_none());
    p.set_stage_enabled(Stage::Rtk, false).unwrap();
    assert!(p.command_output("git status", "On branch main\n", Some(0)).is_none());
}
