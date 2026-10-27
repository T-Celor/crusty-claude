//! Builds a session from the configuration: providers, tools, optimizer pipeline, code
//! graph, MCP servers, permissions, hooks, memory, transcript — then runs the UI.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use crusty_agent::{
    ChannelApprover, Controller, PolicyBundle, Runtime, RuntimeParts, command_list, display_path, reload_memory, replay, session_start_hook,
};
use crusty_core::config::{Config, OptimizerMode};
use crusty_core::permission::FixedApprover;
use crusty_core::{
    AppEvent, ApprovalDecision, Approver, CancellationToken, EventSink, Message, ModelRef, OriginalStore, PermissionMode, Provider, Tool,
    UiCommand,
};
use crusty_graph::{CodeGraph, GraphTool};
use crusty_mcp::McpManager;
use crusty_pipeline::Pipeline;
use crusty_providers::{ProviderRegistry, http_client};
use crusty_session::commands::CustomCommand;
use crusty_session::hooks::ShellHooks;
use crusty_session::transcript::{self, Transcript};
use crusty_tools::{FileToolsState, PermissionEngine, file_tools};
use tokio::sync::mpsc::{self, UnboundedReceiver};

use crate::{Cli, ModeArg};

/// Configuration with the command-line overrides applied.
pub struct Loaded {
    pub config: Config,
    pub workspace: PathBuf,
    pub home: PathBuf,
    pub warnings: Vec<String>,
    pub sources: Vec<PathBuf>,
    /// Tools removed from every agent.
    pub hidden_tools: Vec<String>,
    /// The user trusts this project's own configuration.
    pub trusted: bool,
}

pub fn init_logging(cli: &Cli) -> Option<tracing_appender::non_blocking::WorkerGuard> {
    use tracing_subscriber::EnvFilter;
    let level = if cli.debug { "debug" } else { "warn" };
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(level));
    if cli.print && cli.verbose || cli.command.is_some() {
        let _ = tracing_subscriber::fmt().with_env_filter(filter).with_writer(std::io::stderr).try_init();
        return None;
    }
    // The interactive UI owns the terminal: logs go to a file.
    let dir = crusty_core::paths::crusty_home().join("logs");
    std::fs::create_dir_all(&dir).ok()?;
    let (writer, guard) = tracing_appender::non_blocking(tracing_appender::rolling::never(dir, "crusty.log"));
    let _ = tracing_subscriber::fmt().with_env_filter(filter).with_ansi(false).with_writer(writer).try_init();
    Some(guard)
}

pub fn load_config(cli: &Cli) -> Result<Loaded> {
    let workspace = match &cli.cwd {
        Some(p) => p.clone(),
        None => std::env::current_dir()?,
    };
    let workspace = workspace.canonicalize().with_context(|| format!("project directory {} not found", workspace.display()))?;
    let home = crusty_core::paths::home_dir();
    let trusted = crusty_core::trust::is_trusted(&workspace);
    let (mut config, report) = Config::load(&workspace, cli.config.as_deref(), trusted)?;
    let mut warnings = report.warnings;
    if !trusted && workspace.join(".mcp.json").exists() && config.mcp.read_mcp_json != Some(false) {
        config.mcp.read_mcp_json = Some(false);
        warnings.push("this project's .mcp.json is ignored until you trust the project (`crusty trust`)".into());
    }
    let defs = crusty_session::agents::load_agent_files(&workspace, &home, trusted);
    warnings.extend(crusty_session::agents::merge_into_config(&mut config, &defs));

    if let Some(m) = &cli.model {
        let r = ModelRef::parse(m, &config.orchestrator.model.provider).with_context(|| format!("bad --model `{m}`"))?;
        config.orchestrator.model.provider = r.provider;
        config.orchestrator.model.model = r.model;
    }
    if let Some(e) = &cli.effort {
        config.orchestrator.model.effort = Some(e.clone());
    }
    for dm in &cli.domain_model {
        let (domain, model) = dm.split_once('=').with_context(|| format!("--domain-model expects DOMAIN=PROVIDER:MODEL, got `{dm}`"))?;
        let r = ModelRef::parse(model, &config.executor.provider).with_context(|| format!("bad model in `{dm}`"))?;
        let entry = config.domains.entry(domain.to_owned()).or_default();
        entry.model.provider = Some(r.provider);
        entry.model.model = Some(r.model);
        if entry.description.is_empty() {
            entry.description = format!("{domain} executor");
        }
    }
    if cli.dangerously_skip_permissions {
        config.permissions.mode = PermissionMode::BypassPermissions;
    } else if let Some(m) = &cli.permission_mode {
        config.permissions.mode = PermissionMode::parse(m).with_context(|| format!("unknown permission mode `{m}`"))?;
    }
    config.permissions.allow.extend(cli.allowed_tools.iter().cloned());
    let mut hidden_tools = Vec::new();
    for rule in &cli.disallowed_tools {
        config.permissions.deny.push(rule.clone());
        if !rule.contains('(') {
            hidden_tools.push(rule.clone());
        }
    }
    config.permissions.additional_dirs.extend(cli.add_dir.iter().cloned());
    if cli.no_optimizer {
        config.optimizer.enabled = false;
    }
    if let Some(mode) = cli.optimizer_mode {
        config.optimizer.mode = match mode {
            ModeArg::Active => OptimizerMode::Active,
            ModeArg::Observe => OptimizerMode::Observe,
        };
    }
    warnings.extend(config.validate()?);
    Ok(Loaded { config, workspace, home, warnings, sources: report.sources, hidden_tools, trusted })
}

/// A ready session and what must be cleaned up after it.
pub struct Session {
    pub rt: Arc<Runtime>,
    pub events_rx: UnboundedReceiver<AppEvent>,
    pub approver: Option<Arc<ChannelApprover>>,
    pub mcp: McpManager,
    pub graph: Option<Arc<CodeGraph>>,
    pub custom_commands: Vec<CustomCommand>,
    pub messages: Vec<Message>,
    pub resumed: bool,
    pub background: CancellationToken,
    pub warnings: Vec<String>,
}

fn policy_bundle() -> PolicyBundle {
    PolicyBundle {
        name: "ponytail".into(),
        version: crusty_optimizer::PONYTAIL_VERSION.into(),
        texts: ["lite", "full", "ultra"].into_iter().map(|l| (l.to_owned(), crusty_optimizer::ponytail_prompt(l))).collect(),
    }
}

/// The conversation a session starts from.
struct StartPoint {
    session_id: String,
    messages: Vec<Message>,
    /// Transcript to append to, for a resumed session.
    transcript: Option<PathBuf>,
    warnings: Vec<String>,
}

/// A new conversation, or the one named by --continue / --resume.
fn resolve_session(cli: &Cli, dir: &std::path::Path) -> Result<StartPoint> {
    let path = if cli.continue_session {
        Some(transcript::latest(dir).context("no previous session to continue in this project")?)
    } else if let Some(id) = cli.resume.as_deref().filter(|id| !id.is_empty()) {
        Some(transcript::find(dir, id).with_context(|| format!("no session starting with `{id}`"))?)
    } else {
        None
    };
    match path {
        Some(p) => {
            let loaded = transcript::load(&p).with_context(|| format!("cannot read {}", p.display()))?;
            Ok(StartPoint { session_id: loaded.session_id, messages: loaded.messages, transcript: Some(p), warnings: loaded.warnings })
        }
        None => Ok(StartPoint { session_id: transcript::new_session_id(), messages: Vec::new(), transcript: None, warnings: Vec::new() }),
    }
}

pub async fn build_session(cli: &Cli, loaded: Loaded, interactive: bool) -> Result<Session> {
    let Loaded { config, workspace, home, mut warnings, hidden_tools, .. } = loaded;
    let config = Arc::new(config);
    let (tx, events_rx) = mpsc::unbounded_channel();
    let events = EventSink::new(tx);

    let registry = ProviderRegistry::from_config(&config);
    let providers: BTreeMap<String, Arc<dyn Provider>> =
        registry.ids().into_iter().filter_map(|id| registry.get(id).map(|p| (id.to_owned(), p))).collect();
    let store = Arc::new(OriginalStore::default());
    let pipeline = Arc::new(Pipeline::new(&config, events.clone(), store.clone()));
    pipeline.set_counters(providers.clone());

    let mut tools: Vec<Arc<dyn Tool>> = file_tools(&FileToolsState::default());
    tools.extend(crusty_shell::shell_tools(Arc::new(crusty_shell::BackgroundShells::default())));
    tools.extend(crusty_web::web_tools());
    let graph = config.optimizer.graphify.enabled.then(|| Arc::new(CodeGraph::new(&workspace, config.optimizer.graphify.clone())));
    if let Some(g) = &graph {
        tools.push(Arc::new(GraphTool::new(g.clone())));
    }
    // Everything that can fail runs before MCP servers are started, so an error never leaves
    // their processes behind.
    let permissions = Arc::new(PermissionEngine::from_config(&config.permissions).map_err(anyhow::Error::msg)?);
    let channel = interactive.then(|| Arc::new(ChannelApprover::new(events.clone())));
    let approver: Arc<dyn Approver> = match &channel {
        Some(a) => a.clone(),
        None => Arc::new(FixedApprover(ApprovalDecision::DenyWithFeedback(
            "not approved: this is a non-interactive run; allow it with --allowed-tools or a permissions.allow rule".into(),
        ))),
    };
    let sessions_dir = crusty_core::paths::sessions_dir(&workspace, config.session.dir.as_deref());
    let StartPoint { session_id, messages, transcript: transcript_path, warnings: load_warnings } = resolve_session(cli, &sessions_dir)?;
    warnings.extend(load_warnings);
    let hooks = Arc::new(ShellHooks::from_config(&config.hooks, &workspace, &session_id).map_err(anyhow::Error::msg)?);

    let servers = match crusty_mcp::merged_servers(&config.mcp, &workspace, cli.mcp_config.as_deref()) {
        Ok(s) => s,
        Err(e) => {
            warnings.push(format!("MCP configuration ignored: {e}"));
            BTreeMap::new()
        }
    };
    let mcp = McpManager::connect_all(servers, &events, Duration::from_secs(30)).await;
    tools.extend(mcp.tools());

    let rt = Runtime::new(RuntimeParts {
        config: config.clone(),
        providers,
        tools,
        permissions,
        approver,
        hooks,
        optimizer: pipeline.clone(),
        filters: pipeline,
        events: events.clone(),
        policy: policy_bundle(),
        workspace: workspace.clone(),
        home: home.clone(),
        session_id: session_id.clone(),
        store,
    });
    rt.update_settings(|s| {
        s.hidden_tools = hidden_tools;
        s.append_system.clone_from(&cli.append_system_prompt);
        s.system_override.clone_from(&cli.system_prompt);
        s.max_turns = cli.max_turns;
    });
    reload_memory(&rt);
    rt.update_context(|c| {
        c.mcp_status = mcp
            .status()
            .iter()
            .map(|s| match &s.error {
                Some(e) => format!("{} ({}): not connected — {e}", s.name, s.transport),
                None => format!("{} ({}): {} tools (named mcp__{}__*)", s.name, s.transport, s.tools, s.name),
            })
            .collect();
    });
    let transcript = match &transcript_path {
        Some(p) => Transcript::open_append(p),
        None => Transcript::create(&sessions_dir, &session_id, &workspace),
    };
    match transcript {
        Ok(t) => rt.set_transcript(Some(t)),
        Err(e) => warnings.push(format!("transcript disabled: {e}")),
    }
    let background = CancellationToken::new();
    crusty_proxy::spawn_health_monitor(
        rt.proxy_upstreams.clone(),
        http_client(10),
        events,
        Duration::from_secs(config.proxy.health_interval_secs.max(1)),
        background.clone(),
    );
    let custom_commands = crusty_session::commands::load_commands(&workspace, &home);
    Ok(Session {
        rt,
        events_rx,
        approver: channel,
        mcp,
        graph,
        custom_commands,
        resumed: transcript_path.is_some(),
        messages,
        background,
        warnings,
    })
}

/// Indexes the code graph and gives its repository map to the orchestrator.
pub async fn index_graph(rt: Arc<Runtime>, graph: Arc<CodeGraph>) {
    if !rt.config.optimizer.graphify.auto_index {
        return;
    }
    let events = rt.events.clone();
    let budget = rt.config.optimizer.graphify.repo_map_tokens;
    let result = tokio::task::spawn_blocking(move || graph.refresh(&events).map(|_| graph.repo_map(budget))).await;
    match result {
        Ok(Ok(map)) => rt.update_context(|c| c.repo_map = map),
        Ok(Err(e)) => rt.events.warn(format!("code graph not built: {e}")),
        Err(e) => rt.events.warn(format!("code graph indexing failed: {e}")),
    }
}

/// Emits what the UI shows when a session starts.
pub async fn announce(rt: &Runtime, warnings: &[String]) {
    for w in warnings {
        rt.events.warn(w.clone());
    }
    session_start_hook(rt).await;
    let s = rt.settings();
    rt.events.emit(AppEvent::SessionStarted {
        session_id: rt.session_id(),
        cwd: display_path(&rt.workspace),
        orchestrator: format!("{}:{}", s.orchestrator.provider, s.orchestrator.model),
        permission_mode: rt.permissions.mode(),
    });
    if let Some((_, level)) = rt.policy_text() {
        rt.events.emit(AppEvent::PolicyApplied {
            agent_id: crusty_agent::ORCHESTRATOR_ID.into(),
            policy: rt.policy.name.clone(),
            version: rt.policy.version.clone(),
            level,
        });
    }
}

pub fn print_sessions(loaded: &Loaded) {
    let dir = crusty_core::paths::sessions_dir(&loaded.workspace, loaded.config.session.dir.as_deref());
    let sessions = transcript::list_sessions(&dir);
    if sessions.is_empty() {
        println!("no saved sessions for {}", display_path(&loaded.workspace));
    }
    for s in sessions {
        println!("{}  {:>4} msgs  {}", s.id, s.messages, s.title_or_first_prompt);
    }
}

pub async fn run_interactive(cli: &Cli) -> Result<()> {
    let loaded = load_config(cli)?;
    if cli.resume.as_deref() == Some("") {
        print_sessions(&loaded);
        return Ok(());
    }
    if !std::io::IsTerminal::is_terminal(&std::io::stdout()) {
        bail!("the interactive UI needs a terminal; use --print for scripts");
    }
    let session = build_session(cli, loaded, true).await?;
    let rt = session.rt.clone();
    announce(&rt, &session.warnings).await;
    if let Some(g) = session.graph.clone() {
        tokio::spawn(index_graph(rt.clone(), g));
    }
    if session.resumed {
        replay(&rt, &session.messages);
    }
    let approver = session.approver.clone().context("interactive session without approver")?;
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let controller = Controller::new(rt.clone(), approver, session.messages, session.custom_commands.clone());
    let controller = tokio::spawn(controller.run(cmd_rx));
    let s = rt.settings();
    let opts = crusty_tui::TuiOptions {
        config: rt.config.clone(),
        session_id: rt.session_id(),
        cwd: display_path(&rt.workspace),
        orchestrator: format!("{}:{}", s.orchestrator.provider, s.orchestrator.model),
        permission_mode: rt.permissions.mode(),
        metrics: rt.events.metrics().clone(),
        commands: command_list(&session.custom_commands),
        initial_prompt: cli.prompt.clone(),
    };
    let ui = crusty_tui::run(opts, session.events_rx, cmd_tx.clone()).await;
    let _ = cmd_tx.send(UiCommand::Quit);
    let _ = controller.await;
    session.background.cancel();
    session.mcp.shutdown().await;
    ui.context("terminal UI failed")
}
