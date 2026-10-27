//! The agent loop shared by the orchestrator and every executor: build the request, run
//! it through the optimizer, call the model (with fallbacks), gate and run the tools,
//! repeat until the model stops.

use std::sync::{Arc, Mutex};
use std::time::Instant;

use crusty_core::config::ModelProfile;
use crusty_core::event::{AgentStatus, TodoItem, TodoStatus};
use crusty_core::hooks::HookEvent;
use crusty_core::tokens::estimate_request;
use crusty_core::tool::PlannedChange;
use crusty_core::vault::StreamRehydrator;
use crusty_core::{
    AppEvent, ApprovalDecision, ApprovalRequest, CallOptions, CancellationToken, ChatRequest, ChatResponse, ContentBlock, Judgement,
    Message, ModelRef, OptimizeContext, OptimizeOutcome, PermissionDecision, PermissionMode, Provider, ProviderError, ProviderKind,
    RiskLevel, Role, Route, StopReason, StreamDelta, Tool, ToolContext, ToolError, ToolOutput, ToolSpec, Usage, Verdict,
};
use crusty_session::transcript::Entry;
use futures::future::{BoxFuture, join_all};
use serde_json::{Value, json};

use crate::runtime::Runtime;
use crate::{compact, delegate, prompts};

pub const ORCHESTRATOR_ID: &str = "orchestrator";
pub const TASK_TOOL: &str = "Task";
/// Loads deferred MCP servers; handled by the engine (it needs the runtime).
pub const TOOL_SEARCH: &str = "ToolSearch";
/// Key the providers put in a tool input whose JSON could not be parsed.
const INVALID_JSON_KEY: &str = "__crusty_invalid_json";
/// Tools that write a local file: a masked secret may go into them without asking.
const LOCAL_EDITORS: [&str; 4] = ["Write", "Edit", "MultiEdit", "NotebookEdit"];

/// One agent: who it is, which model, which tools.
#[derive(Debug, Clone)]
pub struct AgentSpec {
    pub id: String,
    pub parent: Option<String>,
    /// `orchestrator` or a domain name.
    pub domain: String,
    pub profile: ModelProfile,
    pub system: String,
    /// Tool names this agent may call (`Task` included for the orchestrator).
    pub tools: Vec<String>,
    pub depth: u32,
    pub max_steps: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub enum StopKind {
    Done,
    StepLimit,
    Budget(String),
    Refusal(String),
    MaxTokens,
    Cancelled,
    Error(String),
}

impl StopKind {
    pub fn label(&self) -> String {
        match self {
            StopKind::Done => "done".into(),
            StopKind::StepLimit => "step limit reached".into(),
            StopKind::Budget(b) => format!("budget exceeded ({b})"),
            StopKind::Refusal(r) => format!("refused ({r})"),
            StopKind::MaxTokens => "output token limit reached".into(),
            StopKind::Cancelled => "cancelled".into(),
            StopKind::Error(e) => format!("error: {e}"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct AgentOutcome {
    /// Text of the last model response.
    pub text: String,
    pub usage: Usage,
    pub cost_usd: f64,
    pub turns: u32,
    pub stop: StopKind,
    /// `provider:model` that answered last.
    pub model: String,
}

/// The orchestrator as configured right now.
pub fn orchestrator_spec(rt: &Runtime) -> AgentSpec {
    let settings = rt.settings();
    let mut tools: Vec<String> = rt.config.orchestrator.direct_tools.clone();
    if !tools.iter().any(|t| t == TASK_TOOL) {
        tools.push(TASK_TOOL.into());
    }
    AgentSpec {
        id: ORCHESTRATOR_ID.into(),
        parent: None,
        domain: ORCHESTRATOR_ID.into(),
        profile: settings.orchestrator.clone(),
        system: prompts::orchestrator_prompt(rt),
        tools,
        depth: 0,
        max_steps: settings.max_turns.unwrap_or(rt.config.orchestrator.max_steps),
    }
}

/// Claude models that take adaptive thinking and `output_config.effort` (4.6 and later).
pub fn modern_claude(model: &str) -> bool {
    let Some(rest) = model.strip_prefix("claude-") else {
        return false;
    };
    let mut parts = rest.split('-');
    // Old names start with the version (`claude-3-5-sonnet-…`), current ones with the family.
    if parts.next().is_none_or(|family| family.parse::<u32>().is_ok()) {
        return false;
    }
    // A date suffix (`claude-opus-4-20250514`) is not a minor version.
    let mut version = parts.map(|p| if p.len() <= 2 { p.parse::<u32>().ok() } else { None });
    match (version.next().flatten(), version.next().flatten()) {
        (Some(major), minor) => major >= 5 || (major == 4 && minor.unwrap_or(0) >= 6),
        _ => false,
    }
}

fn tool_specs(rt: &Runtime, spec: &AgentSpec, kind: ProviderKind) -> Vec<ToolSpec> {
    let hidden = rt.settings().hidden_tools;
    let server_search = kind == ProviderKind::Anthropic && rt.config.web.search_backend == "anthropic";
    let mut specs = Vec::new();
    for name in &spec.tools {
        if hidden.contains(name) || rt.is_deferred_tool(name) {
            continue;
        }
        if name == TASK_TOOL {
            specs.push(delegate::task_spec(rt));
        } else if name == TOOL_SEARCH {
            if !rt.deferred_servers().is_empty() {
                specs.push(tool_search_spec());
            }
        } else if name == "WebSearch" && server_search {
            let mut s = ToolSpec::new("web_search", "", Value::Null);
            s.extra.insert("type".into(), json!("web_search_20260209"));
            specs.push(s);
        } else if let Some(t) = rt.tools.get(name) {
            specs.push(t.spec());
        }
    }
    specs.sort_by(|a, b| a.name.cmp(&b.name));
    specs
}

fn build_request(rt: &Runtime, spec: &AgentSpec, messages: &[Message]) -> ChatRequest {
    let profile = &spec.profile;
    let info = rt.config.model_info(&profile.model);
    let kind = rt.provider(&profile.provider).map(|p| p.kind()).unwrap_or_default();
    let mut req = ChatRequest::new(&profile.model, profile.max_output_tokens.min(info.max_output.max(1024)));
    req.system.push(ContentBlock::text(&spec.system));
    if let Some((policy, _)) = rt.policy_text() {
        req.system.push(ContentBlock::text(policy));
    }
    req.messages = messages.to_vec();
    req.tools = tool_specs(rt, spec, kind);
    req.temperature = profile.temperature;
    let modern = kind == ProviderKind::Anthropic && modern_claude(&profile.model);
    if modern && profile.thinking.as_deref() != Some("off") {
        let mut thinking = json!({"type": "adaptive"});
        if rt.config.tui.show_thinking {
            thinking["display"] = json!("summarized");
        }
        req.extra.insert("thinking".into(), thinking);
        // Compression, compaction and a rebuilt system prompt edit earlier turns; without this
        // the API rejects (400) every later thinking block on enforced accounts.
        req.tolerate_history_edits();
    }
    if let Some(effort) = &profile.effort
        && (modern || kind == ProviderKind::Openai)
    {
        req.extra.insert("output_config".into(), json!({"effort": effort}));
    }
    if kind == ProviderKind::Anthropic {
        req.extra.insert("metadata".into(), json!({"user_id": format!("crusty-{}", rt.session_id())}));
    }
    req
}

fn optimize_context(rt: &Runtime, spec: &AgentSpec, model: &ModelRef) -> OptimizeContext {
    let provider = rt.provider(&model.provider);
    let info = rt.config.model_info(&model.model);
    OptimizeContext {
        session_id: format!("{}:{}", rt.session_id(), spec.id),
        agent_id: spec.id.clone(),
        provider_id: model.provider.clone(),
        provider_kind: provider.as_ref().map(|p| p.kind()).unwrap_or_default(),
        model: model.model.clone(),
        context_window: info.context_window,
        supports_images: info.vision && provider.map(|p| p.supports_images(&model.model)).unwrap_or(false),
    }
}

fn status(rt: &Runtime, agent_id: &str, status: AgentStatus) {
    rt.events.emit(AppEvent::AgentStatus { agent_id: agent_id.to_owned(), status });
}

fn lock_rehydrator(m: &Mutex<StreamRehydrator>) -> std::sync::MutexGuard<'_, StreamRehydrator> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Server of an MCP tool (`mcp__<server>__<tool>`).
pub fn mcp_server(tool: &str) -> Option<&str> {
    tool.strip_prefix("mcp__")?.split_once("__").map(|(server, _)| server).filter(|s| !s.is_empty())
}

fn tool_search_spec() -> ToolSpec {
    ToolSpec::new(
        TOOL_SEARCH,
        "Load deferred MCP tools. Their servers are hidden until needed: `select:<server>` loads one server; any other query matches its words against the names and descriptions of the deferred tools and loads the servers of the matches. The loaded tools are available from your next step.",
        json!({"type": "object", "properties": {"query": {"type": "string"}}, "required": ["query"]}),
    )
}

/// Loads the deferred servers `query` names (`select:<server>`) or matches, and lists the
/// tools that are now available.
fn tool_search(rt: &Runtime, input: &Value) -> ToolOutput {
    let query = input.get("query").and_then(Value::as_str).unwrap_or("").trim().to_owned();
    if let Some(server) = query.strip_prefix("select:").map(str::trim) {
        let names: Vec<String> = rt.tools.keys().filter(|n| mcp_server(n) == Some(server)).cloned().collect();
        if names.is_empty() {
            return ToolOutput::error(format!("no MCP server `{server}`"));
        }
        rt.load_deferred(server);
        return ToolOutput::text(format!("Loaded MCP server `{server}`. Its tools, available from your next step:\n{}", names.join("\n")));
    }
    let words: Vec<String> = query.to_lowercase().split_whitespace().map(str::to_owned).collect();
    let hits: Vec<(String, String)> = rt
        .tools
        .iter()
        .filter(|(name, _)| rt.is_deferred_tool(name))
        .filter_map(|(name, tool)| {
            let spec = tool.spec();
            let haystack = format!("{name} {}", spec.description).to_lowercase();
            words.iter().any(|w| haystack.contains(w.as_str())).then(|| (name.clone(), spec.description))
        })
        .collect();
    if hits.is_empty() {
        return ToolOutput::text("no deferred tool matches");
    }
    let mut lines = Vec::new();
    for (name, description) in &hits {
        if let Some(server) = mcp_server(name) {
            rt.load_deferred(server);
        }
        lines.push(format!("- {name}: {description}"));
    }
    ToolOutput::text(format!("Loaded the servers of these tools; they are available from your next step:\n{}", lines.join("\n")))
}

/// What answered a model call.
struct Answer {
    resp: ChatResponse,
    model: ModelRef,
    /// The optimizer's proxy route failed and the request went direct.
    via_failed: bool,
}

/// One streamed call. The conversation keeps placeholders; the screen shows the real values.
/// A fresh rehydrator per call, so a retried call does not repeat text it already showed.
async fn stream_attempt(
    rt: &Runtime,
    spec: &AgentSpec,
    provider: &dyn Provider,
    req: &ChatRequest,
    opts: &CallOptions,
) -> Result<ChatResponse, ProviderError> {
    let rehydrator = Mutex::new(StreamRehydrator::default());
    let show_thinking = rt.config.tui.show_thinking;
    let on_delta = |d: StreamDelta| match d {
        StreamDelta::Text(text) => {
            let shown = lock_rehydrator(&rehydrator).push(&rt.vault, &text);
            if !shown.is_empty() {
                rt.events.emit(AppEvent::TextDelta { agent_id: spec.id.clone(), text: shown });
            }
        }
        StreamDelta::Thinking(text) if show_thinking => rt.events.emit(AppEvent::ThinkingDelta { agent_id: spec.id.clone(), text }),
        _ => {}
    };
    let result = provider.stream(req, opts, &on_delta).await;
    let tail = lock_rehydrator(&rehydrator).finish(&rt.vault);
    if !tail.is_empty() {
        rt.events.emit(AppEvent::TextDelta { agent_id: spec.id.clone(), text: tail });
    }
    result
}

/// A fallback model's request, built for that model and sent through the optimizer like the
/// primary's (DLP masking included).
async fn optimized_fallback(
    rt: &Runtime,
    spec: &AgentSpec,
    messages: &[Message],
    mref: &ModelRef,
) -> (ChatRequest, OptimizeOutcome, OptimizeContext) {
    let mut fallback = spec.clone();
    fallback.profile.provider.clone_from(&mref.provider);
    fallback.profile.model.clone_from(&mref.model);
    let mut req = build_request(rt, &fallback, messages);
    let octx = optimize_context(rt, &fallback, mref);
    let outcome = rt.optimizer.optimize(&mut req, &octx).await;
    (req, outcome, octx)
}

/// Calls the model, falling back to the profile's fallback models when one is unavailable,
/// and to the direct route when an optimizer proxy cannot be reached. A fallback model gets
/// a request built and optimized for it (its own thinking, tools and format), not the primary's.
async fn call_model(
    rt: &Runtime,
    spec: &AgentSpec,
    messages: &[Message],
    req: &ChatRequest,
    outcome: &OptimizeOutcome,
    octx: &OptimizeContext,
    cancel: &CancellationToken,
) -> Result<Answer, ProviderError> {
    let mut candidates = vec![spec.profile.model_ref()];
    candidates.extend(spec.profile.fallback.iter().filter_map(|f| ModelRef::parse(f, &spec.profile.provider)));
    let mut last_err = ProviderError::Config(format!("provider `{}` is not configured", spec.profile.provider));
    for (i, mref) in candidates.iter().enumerate() {
        let Some(provider) = rt.provider(&mref.provider) else {
            last_err = ProviderError::Config(format!("provider `{}` is not configured", mref.provider));
            continue;
        };
        let owned;
        let (req, outcome, octx) = if i == 0 {
            (req, outcome, octx)
        } else {
            owned = optimized_fallback(rt, spec, messages, mref).await;
            (&owned.0, &owned.1, &owned.2)
        };
        let via = match &outcome.route {
            Route::Via { base_url, .. } => Some(base_url.clone()),
            _ => None,
        };
        let direct = rt.provider_route(&mref.provider);
        let opts =
            CallOptions { base_url_override: via.clone().or_else(|| direct.clone()), cancel: cancel.clone(), extra_headers: Vec::new() };
        let mut result = stream_attempt(rt, spec, provider.as_ref(), req, &opts).await;
        let mut via_failed = false;
        if via.is_some() && matches!(result, Err(ProviderError::Network(_) | ProviderError::Timeout)) {
            rt.optimizer.record_route_failure(octx, outcome);
            rt.events.warn("optimizer proxy unreachable; request sent direct");
            via_failed = true;
            let opts = CallOptions { base_url_override: direct, ..opts };
            result = stream_attempt(rt, spec, provider.as_ref(), req, &opts).await;
        }
        match result {
            Ok(resp) => return Ok(Answer { resp, model: mref.clone(), via_failed }),
            Err(ProviderError::Cancelled) => return Err(ProviderError::Cancelled),
            Err(e) if e.is_unavailable() && i + 1 < candidates.len() => {
                rt.events.emit(AppEvent::ModelFallback {
                    agent_id: spec.id.clone(),
                    from: mref.to_string(),
                    to: candidates[i + 1].to_string(),
                    reason: e.to_string(),
                });
                last_err = e;
            }
            Err(e) => return Err(e),
        }
    }
    Err(last_err)
}

/// Runs an agent until it stops. `messages` is the agent's conversation, updated in place.
pub fn run_agent<'a>(
    rt: &'a Arc<Runtime>,
    spec: &'a AgentSpec,
    messages: &'a mut Vec<Message>,
    cancel: &'a CancellationToken,
) -> BoxFuture<'a, AgentOutcome> {
    Box::pin(async move {
        let mut usage = Usage::default();
        let mut cost = 0.0;
        let mut turns = 0;
        let mut text = String::new();
        let mut model_label = spec.profile.model_ref().to_string();
        // Message count when compaction last failed: it is retried only once history grew.
        let mut compact_failed_at: Option<usize> = None;
        let stop = loop {
            if cancel.is_cancelled() {
                break StopKind::Cancelled;
            }
            if turns >= spec.max_steps {
                break StopKind::StepLimit;
            }
            let mut req = build_request(rt, spec, messages);
            let window = rt.config.model_info(&spec.profile.model).context_window;
            let est = estimate_request(&req, true);
            let session_cfg = &rt.config.session;
            let may_compact = compact_failed_at.is_none_or(|n| messages.len() > n + 2);
            if session_cfg.auto_compact && may_compact && window > 0 && est as f64 > session_cfg.compact_threshold * window as f64 {
                status(rt, &spec.id, AgentStatus::Compacting);
                match compact::compact(rt, spec, messages, None, cancel).await {
                    Ok(report) => {
                        usage.add(&report.usage);
                        cost += report.cost_usd;
                        compact_failed_at = None;
                        rt.events.info(format!("{}: context compacted ({} messages summarised)", spec.id, report.replaced));
                    }
                    Err(e) => {
                        compact_failed_at = Some(messages.len());
                        rt.events.warn(format!("{}: compaction skipped: {e}", spec.id));
                    }
                }
                req = build_request(rt, spec, messages);
            }
            rt.events.emit(AppEvent::ContextStatus { agent_id: spec.id.clone(), est_tokens: estimate_request(&req, true), window });

            let mref = spec.profile.model_ref();
            let octx = optimize_context(rt, spec, &mref);
            let mut outcome = rt.optimizer.optimize(&mut req, &octx).await;
            status(rt, &spec.id, AgentStatus::Thinking);
            let answer = match call_model(rt, spec, messages, &req, &outcome, &octx, cancel).await {
                Ok(a) => a,
                Err(ProviderError::Cancelled) => break StopKind::Cancelled,
                Err(e) => break StopKind::Error(e.to_string()),
            };
            if answer.via_failed {
                outcome.route = Route::Direct;
            }
            let (resp, answered_by) = (answer.resp, answer.model);
            turns += 1;
            model_label = answered_by.to_string();
            let call_cost = rt.config.cost(&answered_by.model, &resp.usage);
            usage.add(&resp.usage);
            cost += call_cost;
            rt.events.emit(AppEvent::Usage {
                agent_id: spec.id.clone(),
                provider: answered_by.provider.clone(),
                model: answered_by.model.clone(),
                usage: resp.usage,
                cost_usd: call_cost,
            });
            // A fallback model answered a different request: it says nothing about the optimizer's.
            if answered_by == mref {
                rt.optimizer.record_usage(&octx, &outcome, &resp.usage);
            }

            text = resp.text();
            if !text.trim().is_empty() {
                rt.events.emit(AppEvent::AssistantText { agent_id: spec.id.clone(), text: rt.vault.rehydrate(&text).into_owned() });
            }
            let assistant = Message { role: Role::Assistant, content: resp.content.clone() };
            rt.log(&Entry::Message { agent_id: spec.id.clone(), message: assistant.clone(), ts: chrono::Utc::now() });
            messages.push(assistant);

            let calls: Vec<(String, String, Value)> = resp
                .content
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::ToolUse { id, name, input, .. } => Some((id.clone(), name.clone(), input.clone())),
                    _ => None,
                })
                .collect();
            if let Some(reason) = over_budget(&spec.profile, &usage, cost) {
                answer_unrun(rt, spec, messages, &calls, "Not run: the agent's budget is exhausted.");
                break StopKind::Budget(reason);
            }
            match &resp.stop_reason {
                StopReason::Refusal => {
                    answer_unrun(rt, spec, messages, &calls, "Not run: the response was a refusal.");
                    let detail =
                        resp.stop_details.as_ref().and_then(|d| d.get("category")).and_then(Value::as_str).unwrap_or("unspecified");
                    break StopKind::Refusal(detail.to_owned());
                }
                StopReason::PauseTurn => continue,
                _ if !calls.is_empty() => {
                    let results = execute_tool_calls(rt, spec, calls, cancel).await;
                    let user = Message { role: Role::User, content: results };
                    rt.log(&Entry::Message { agent_id: spec.id.clone(), message: user.clone(), ts: chrono::Utc::now() });
                    messages.push(user);
                    if cancel.is_cancelled() {
                        break StopKind::Cancelled;
                    }
                }
                StopReason::MaxTokens => break StopKind::MaxTokens,
                _ => {
                    let event = if spec.depth == 0 { HookEvent::Stop } else { HookEvent::SubagentStop };
                    let hook = rt.hooks.run(event, None, &json!({"agent_id": spec.id, "last_message": text})).await;
                    match hook.message.filter(|_| hook.block) {
                        Some(msg) => {
                            let user = Message::user_text(format!("[{} hook] {msg}", event.name()));
                            rt.log(&Entry::Message { agent_id: spec.id.clone(), message: user.clone(), ts: chrono::Utc::now() });
                            messages.push(user);
                        }
                        None => break StopKind::Done,
                    }
                }
            }
        };
        let final_status = match &stop {
            StopKind::Done => AgentStatus::Done,
            StopKind::Cancelled => AgentStatus::Cancelled,
            other => AgentStatus::Failed(other.label()),
        };
        status(rt, &spec.id, final_status);
        AgentOutcome { text, usage, cost_usd: cost, turns, stop, model: model_label }
    })
}

/// Every tool_use must get a tool_result: calls the agent will not run are answered with an
/// error, so the conversation stays valid for the next request.
fn answer_unrun(rt: &Runtime, spec: &AgentSpec, messages: &mut Vec<Message>, calls: &[(String, String, Value)], why: &str) {
    if calls.is_empty() {
        return;
    }
    let user = Message { role: Role::User, content: calls.iter().map(|(id, _, _)| error_result(id, why)).collect() };
    rt.log(&Entry::Message { agent_id: spec.id.clone(), message: user.clone(), ts: chrono::Utc::now() });
    messages.push(user);
}

fn over_budget(profile: &ModelProfile, usage: &Usage, cost: f64) -> Option<String> {
    if let Some(max) = profile.token_budget
        && usage.total() > max
    {
        return Some(format!("{} tokens > {max}", usage.total()));
    }
    if let Some(max) = profile.cost_budget_usd
        && cost > max
    {
        return Some(format!("${cost:.4} > ${max:.4}"));
    }
    None
}

enum Prepared {
    Done(ContentBlock),
    Run { id: String, name: String, input: Value, tool: Option<Arc<dyn Tool>> },
}

fn error_result(id: &str, message: impl Into<String>) -> ContentBlock {
    ContentBlock::tool_result(id, vec![ContentBlock::text(message)], true)
}

/// Bounded description of a tool input for the approval dialog.
fn input_preview(name: &str, input: &Value) -> String {
    let field = |k: &str| input.get(k).and_then(Value::as_str).unwrap_or("");
    let text = match name {
        "Bash" => field("command").to_owned(),
        "Edit" => format!("{}\n--- old\n{}\n+++ new\n{}", field("file_path"), field("old_string"), field("new_string")),
        "Write" => format!("{}\n{}", field("file_path"), field("content")),
        _ => serde_json::to_string_pretty(input).unwrap_or_default(),
    };
    // Commands are shown whole (up to a large cap): the risky part may be at the end.
    let cap = if name == "Bash" { 8000 } else { 1500 };
    let mut out: String = text.chars().take(cap).collect();
    if text.chars().count() > cap {
        out.push_str("\n…");
    }
    out
}

/// Validation, hooks and permissions of one call, before anything runs.
async fn prepare(rt: &Runtime, spec: &AgentSpec, id: String, name: String, mut input: Value, ctx: &ToolContext) -> Prepared {
    let hidden = rt.settings().hidden_tools.contains(&name);
    let server_tool = name == "web_search";
    if server_tool {
        // Server-side tools run at the provider; a client-side call means nothing to do.
        return Prepared::Done(error_result(&id, "web_search runs server-side"));
    }
    if hidden || !spec.tools.contains(&name) {
        let hint = if spec.depth == 0 { " Delegate it with Task to an executor domain that has it." } else { "" };
        return Prepared::Done(error_result(&id, format!("Tool `{name}` is not available to {}.{hint}", spec.domain)));
    }
    if rt.is_deferred_tool(&name) {
        return Prepared::Done(error_result(&id, format!("Tool `{name}` is not loaded yet: load its server with ToolSearch first.")));
    }
    if let Some(raw) = input.get(INVALID_JSON_KEY) {
        let raw: String = raw.as_str().unwrap_or("").chars().take(200).collect();
        return Prepared::Done(error_result(
            &id,
            format!(
                "INVALID_JSON: the input of this call was not valid JSON (it may have been cut by the output limit). Re-issue the call. Received: {raw}"
            ),
        ));
    }
    if name == TASK_TOOL || name == TOOL_SEARCH {
        return Prepared::Run { id, name, input, tool: None };
    }
    let Some(tool) = rt.tools.get(&name).cloned() else {
        return Prepared::Done(error_result(&id, format!("Unknown tool `{name}`")));
    };
    // The model only sees placeholders for values the DLP stage masked; the tool, the
    // hooks, the permission engine and the user see the real ones. The guard judges the
    // masked input, so real values never reach the Laya server.
    let model_input = input.clone();
    let restored = rt.vault.rehydrate_json(&mut input) > 0;
    let hook =
        rt.hooks.run(HookEvent::PreToolUse, Some(&name), &json!({"tool_name": name, "tool_input": input, "agent_id": spec.id})).await;
    if hook.block {
        return Prepared::Done(error_result(&id, format!("Blocked by PreToolUse hook: {}", hook.message.unwrap_or_default())));
    }
    // A hook rewrite is built from the real values: when a secret was restored, the guard keeps
    // judging the model's masked input so the secret never reaches the Laya server.
    let judged = if restored { model_input } else { hook.updated_input.clone().unwrap_or(model_input) };
    if let Some(updated) = hook.updated_input {
        input = updated;
    }
    let change = tool.planned_change(&input, ctx);
    let judged_change = tool.planned_change(&judged, ctx);
    let mode = rt.permissions.mode();
    let mut decision = rt.permissions.evaluate(tool.as_ref(), &input, ctx);
    // A guard review must reach a human: auto mode may not approve it.
    let mut guarded = false;
    // Whether a guard judged this call; auto mode runs judged calls and reads without asking.
    let mut judged_call = false;
    if !matches!(decision, PermissionDecision::Deny { .. })
        && let Some(judgement) = guard(rt, &name, &judged, judged_change.as_ref(), ctx).await
    {
        judged_call = true;
        match judgement.verdict {
            Verdict::Block => {
                return Prepared::Done(error_result(&id, format!("Blocked by the guard ({}): {}", judgement.source, judgement.reason)));
            }
            Verdict::Review if mode == PermissionMode::BypassPermissions => {
                rt.events.warn(format!("guard flagged `{name}` ({}), not asked in bypassPermissions mode", judgement.reason));
            }
            Verdict::Review => {
                guarded = true;
                if let PermissionDecision::Allow { .. } = decision {
                    decision = PermissionDecision::Ask {
                        reason: format!("guard ({}): {}", judgement.source, judgement.reason),
                        suggested_rule: None,
                    };
                }
            }
            Verdict::Allow => {}
        }
    }
    // A masked secret put into a tool that does not write a local file would leave the machine:
    // the human decides, whatever the mode or the rules.
    // File editors are exempt only for a file inside the project (not a network share, not a
    // synced folder elsewhere).
    let local_write = LOCAL_EDITORS.contains(&name.as_str())
        && change.as_ref().is_some_and(|c| rt.allowed_roots.iter().any(|root| c.path.starts_with(root)));
    if restored && !local_write && !matches!(decision, PermissionDecision::Deny { .. }) {
        decision = PermissionDecision::Ask { reason: format!("inserts a masked secret into {name}"), suggested_rule: None };
        guarded = true;
    }
    // Auto mode runs only what a guard judged, and what only reads without touching the network.
    if mode == PermissionMode::Auto
        && !judged_call
        && !guarded
        && matches!(decision, PermissionDecision::Allow { .. })
        && !auto_free(tool.as_ref(), &name)
    {
        decision = PermissionDecision::Ask { reason: format!("auto mode asks: no guard judged `{name}`"), suggested_rule: None };
    }
    match decision {
        PermissionDecision::Allow { .. } => run_prepared(rt, id, name, input, tool, change.as_ref()),
        PermissionDecision::Deny { reason } => Prepared::Done(error_result(&id, format!("Permission denied: {reason}"))),
        PermissionDecision::Ask { reason, suggested_rule } => {
            let request = ApprovalRequest {
                id: uuid::Uuid::new_v4().to_string(),
                agent_id: spec.id.clone(),
                tool: name.clone(),
                summary: tool.summarize(&input),
                risk: tool.assess(&input, ctx).level,
                reason,
                suggested_rule: suggested_rule.clone(),
                preview: input_preview(&name, &input),
            };
            if !guarded && mode == PermissionMode::Auto && request.risk <= RiskLevel::Moderate {
                let judgement = rt.advisor.auto_approve(&request, &ctx.goal).await;
                if judgement.verdict == Verdict::Allow && judgement.confidence >= rt.config.laya.auto_mode.threshold {
                    rt.events.info(format!(
                        "auto: `{}` ran without asking ({}, {} confidence {:.2})",
                        request.summary, judgement.reason, judgement.source, judgement.confidence
                    ));
                    return run_prepared(rt, id, name, input, tool, change.as_ref());
                }
            }
            status(rt, &spec.id, AgentStatus::WaitingApproval);
            // An interrupt must not leave the turn waiting for an answer nobody will give.
            let decision = tokio::select! {
                d = rt.approver.approve(request) => d,
                () = ctx.cancel.cancelled() => ApprovalDecision::Deny,
            };
            match decision {
                ApprovalDecision::AllowOnce => run_prepared(rt, id, name, input, tool, change.as_ref()),
                ApprovalDecision::AllowAlways => {
                    if let Some(rule) = suggested_rule
                        && let Err(e) = rt.permissions.add_allow_rule(&rule)
                    {
                        rt.events.warn(format!("could not add rule `{rule}`: {e}"));
                    }
                    run_prepared(rt, id, name, input, tool, change.as_ref())
                }
                ApprovalDecision::Deny => Prepared::Done(error_result(&id, "The user denied this tool call.")),
                ApprovalDecision::DenyWithFeedback(f) => {
                    Prepared::Done(error_result(&id, format!("The user denied this tool call and said: {f}")))
                }
            }
        }
    }
}

/// Tools that the auto mode runs without a guard judgement: reads that do not reach outside.
fn auto_free(tool: &dyn Tool, name: &str) -> bool {
    tool.is_read_only() && !matches!(name, "WebFetch" | "WebSearch") && !name.starts_with("mcp__")
}

/// Asks the advisor about a shell command or a file change, when the guard covers it.
async fn guard(rt: &Runtime, name: &str, input: &Value, change: Option<&PlannedChange>, ctx: &ToolContext) -> Option<Judgement> {
    let cfg = &rt.config.laya.guard;
    if let Some(change) = change {
        return if cfg.files { Some(rt.advisor.judge_write(change, &ctx.goal).await) } else { None };
    }
    if name == "Bash" && cfg.commands {
        let command = input.get("command").and_then(Value::as_str)?;
        return Some(rt.advisor.judge_command(command, &ctx.workspace, &ctx.goal).await);
    }
    None
}

/// Approved call: snapshots the file it will change for `/rewind`, then lets it run.
fn run_prepared(rt: &Runtime, id: String, name: String, input: Value, tool: Arc<dyn Tool>, change: Option<&PlannedChange>) -> Prepared {
    if let Some(change) = change
        && let Err(e) = rt.checkpoints.before_change(&change.path)
    {
        rt.events.warn(format!("no checkpoint for {}: {e}", change.path.display()));
    }
    Prepared::Run { id, name, input, tool: Some(tool) }
}

fn todos_from_input(input: &Value) -> Option<Vec<TodoItem>> {
    let items = input.get("todos")?.as_array()?;
    items
        .iter()
        .map(|t| {
            let status = match t.get("status")?.as_str()? {
                "in_progress" => TodoStatus::InProgress,
                "completed" => TodoStatus::Completed,
                _ => TodoStatus::Pending,
            };
            Some(TodoItem { content: t.get("content")?.as_str()?.to_owned(), status })
        })
        .collect()
}

async fn run_call(
    rt: &Arc<Runtime>,
    spec: &AgentSpec,
    id: String,
    name: String,
    input: Value,
    tool: Option<Arc<dyn Tool>>,
    cancel: &CancellationToken,
) -> ContentBlock {
    let ctx = rt.tool_context(&spec.id, cancel.child_token());
    let summary = match &tool {
        Some(t) => t.summarize(&input),
        None => delegate::summarize(&input),
    };
    rt.events.emit(AppEvent::ToolStarted { agent_id: spec.id.clone(), call_id: id.clone(), tool: name.clone(), summary });
    status(rt, &spec.id, if tool.is_none() { AgentStatus::Delegating } else { AgentStatus::Tool(name.clone()) });
    let started = Instant::now();
    let result: Result<ToolOutput, ToolError> = match &tool {
        Some(t) => t.call(input.clone(), &ctx).await,
        None if name == TOOL_SEARCH => Ok(tool_search(rt, &input)),
        None => delegate::run_task(rt, spec, &input, cancel).await,
    };
    let mut output = match result {
        Ok(o) => o,
        Err(e) => ToolOutput::error(e.to_string()),
    };
    // Hook text joins the result after the optimizer has shortened it, so Triage and Rerank
    // never drop what the hook added.
    let mut hook_blocks = Vec::new();
    if tool.is_some() {
        let payload = json!({"tool_name": name, "tool_input": input, "tool_response": output.text_content(), "agent_id": spec.id});
        let hook = rt.hooks.run(HookEvent::PostToolUse, Some(&name), &payload).await;
        if hook.block {
            hook_blocks.push(ContentBlock::text(format!("[PostToolUse hook] {}", hook.message.unwrap_or_default())));
        }
        if let Some(extra) = hook.additional_context {
            hook_blocks.push(ContentBlock::text(extra));
        }
        let goal = rt.goal(&spec.id);
        let octx = optimize_context(rt, spec, &spec.profile.model_ref());
        rt.optimizer.after_tool(&name, &input, &mut output, &goal, &octx).await;
    }
    output.content.extend(hook_blocks);
    if name == "TodoWrite"
        && !output.is_error
        && let Some(items) = todos_from_input(&input)
    {
        rt.set_plan(&spec.id, items);
    }
    rt.events.emit(AppEvent::ToolFinished {
        agent_id: spec.id.clone(),
        call_id: id.clone(),
        tool: name,
        ok: !output.is_error,
        preview: output.preview.clone(),
        duration_ms: started.elapsed().as_millis() as u64,
        exit_code: output.exit_code,
    });
    ContentBlock::tool_result(id, output.content, output.is_error)
}

/// Runs the calls of one model response and returns their results in call order.
/// Read-only calls and delegations run concurrently; any state-changing call makes the
/// whole batch sequential.
async fn execute_tool_calls(
    rt: &Arc<Runtime>,
    spec: &AgentSpec,
    calls: Vec<(String, String, Value)>,
    cancel: &CancellationToken,
) -> Vec<ContentBlock> {
    let ctx = rt.tool_context(&spec.id, cancel.child_token());
    let mut prepared = Vec::with_capacity(calls.len());
    for (id, name, input) in calls {
        if cancel.is_cancelled() {
            prepared.push(Prepared::Done(error_result(&id, "Cancelled by the user.")));
            continue;
        }
        prepared.push(prepare(rt, spec, id, name, input, &ctx).await);
    }
    let parallel = prepared.iter().all(|p| match p {
        Prepared::Done(_) => true,
        Prepared::Run { tool: None, .. } => true,
        Prepared::Run { tool: Some(t), .. } => t.is_read_only(),
    });
    let mut done: Vec<Option<ContentBlock>> = Vec::with_capacity(prepared.len());
    let mut pending = Vec::new();
    for (i, p) in prepared.into_iter().enumerate() {
        match p {
            Prepared::Done(block) => done.push(Some(block)),
            Prepared::Run { id, name, input, tool } => {
                done.push(None);
                pending.push((i, id, name, input, tool));
            }
        }
    }
    if parallel {
        let futures = pending
            .into_iter()
            .map(|(i, id, name, input, tool)| async move { (i, run_call(rt, spec, id, name, input, tool, cancel).await) });
        for (i, block) in join_all(futures).await {
            done[i] = Some(block);
        }
    } else {
        for (i, id, name, input, tool) in pending {
            let block = if cancel.is_cancelled() {
                error_result(&id, "Cancelled by the user.")
            } else {
                run_call(rt, spec, id, name, input, tool, cancel).await
            };
            done[i] = Some(block);
        }
    }
    done.into_iter().flatten().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modern_claude_detection() {
        assert!(modern_claude("claude-opus-5-5"));
        assert!(modern_claude("claude-sonnet-4-6"));
        assert!(modern_claude("claude-fable-5"));
        assert!(!modern_claude("claude-haiku-4-5"));
        assert!(!modern_claude("claude-3-opus"));
        assert!(!modern_claude("claude-3-5-sonnet-20241022"));
        assert!(!modern_claude("claude-3-7-sonnet-20250219"));
        assert!(!modern_claude("claude-opus-4-20250514"));
        assert!(modern_claude("claude-opus-4-6"));
        assert!(!modern_claude("gpt-5"));
    }

    #[test]
    fn todos_are_parsed() {
        let input = json!({"todos": [{"content": "a", "status": "completed"}, {"content": "b", "status": "in_progress"}]});
        let items = todos_from_input(&input).unwrap();
        assert_eq!(items[1].status, TodoStatus::InProgress);
        assert!(todos_from_input(&json!({"todos": [{"status": "x"}]})).is_none());
    }

    #[test]
    fn budget_checks() {
        let mut p = ModelProfile { token_budget: Some(10), ..ModelProfile::default() };
        let u = Usage { input_tokens: 8, output_tokens: 5, ..Usage::default() };
        assert!(over_budget(&p, &u, 0.0).is_some());
        p.token_budget = None;
        p.cost_budget_usd = Some(1.0);
        assert!(over_budget(&p, &u, 0.5).is_none());
        assert!(over_budget(&p, &u, 1.5).is_some());
    }
}
