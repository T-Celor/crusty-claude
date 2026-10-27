//! Session-wide services shared by the orchestrator and every executor.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use crusty_core::advisor::Advisor;
use crusty_core::checkpoint::Checkpointer;
use crusty_core::config::{ModelOverride, ModelProfile, PonytailConfig, ProxyUpstream, ResolvedDomain};
use crusty_core::event::TodoItem;
use crusty_core::filter::ContentFilter;
use crusty_core::hooks::HookRunner;
use crusty_core::vault::SecretVault;
use crusty_core::{
    Approver, CancellationToken, Config, EventSink, OriginalStore, PermissionPolicy, Provider, RequestOptimizer, Tool, ToolContext,
};
use crusty_session::transcript::{Entry, Transcript};
use tokio::sync::Semaphore;

/// Engineering policy texts injected into every agent (Ponytail), one per level.
#[derive(Debug, Clone, Default)]
pub struct PolicyBundle {
    pub name: String,
    pub version: String,
    /// level (`lite`, `full`, `ultra`) → system prompt text.
    pub texts: BTreeMap<String, String>,
}

/// Settings the user can change while the session runs.
#[derive(Debug, Clone)]
pub struct RuntimeSettings {
    pub orchestrator: ModelProfile,
    /// `/domain` changes, applied on top of the configured domains.
    pub domain_overrides: BTreeMap<String, ModelOverride>,
    pub ponytail: PonytailConfig,
    /// provider id → base URL of a proxy all its calls go through (`/proxy route`).
    pub provider_routes: BTreeMap<String, String>,
    /// Tools removed from every agent (`--disallowed-tools` without specifier).
    pub hidden_tools: Vec<String>,
    /// Extra system prompt (`--append-system-prompt`).
    pub append_system: Option<String>,
    /// Replaces the orchestrator's base prompt (`--system-prompt`).
    pub system_override: Option<String>,
    /// Max model calls of the orchestrator per turn (`--max-turns`), else config.
    pub max_turns: Option<u32>,
    /// The user chose the orchestrator model (`--model`, `/model`): the router leaves it alone.
    pub orchestrator_pinned: bool,
    /// Tier the router picked for this session, shown by `/model` and the side panel.
    pub routed_tier: Option<String>,
}

/// Prompt material gathered at session start.
#[derive(Debug, Clone, Default)]
pub struct SessionContext {
    pub memory: String,
    pub memory_files: Vec<PathBuf>,
    pub repo_map: Option<String>,
    pub mcp_status: Vec<String>,
    /// `name: description` of every skill offered to the agents (`/skills`).
    pub skills: Vec<String>,
    /// `name version (source)` of every loaded plugin (`/plugins`).
    pub plugins: Vec<String>,
}

/// Everything an agent run needs. Built once per session by the binary.
pub struct Runtime {
    pub config: Arc<Config>,
    pub providers: BTreeMap<String, Arc<dyn Provider>>,
    pub tools: BTreeMap<String, Arc<dyn Tool>>,
    pub permissions: Arc<dyn PermissionPolicy>,
    pub approver: Arc<dyn Approver>,
    pub hooks: Arc<dyn HookRunner>,
    pub optimizer: Arc<dyn RequestOptimizer>,
    pub filters: Arc<dyn ContentFilter>,
    pub events: EventSink,
    pub store: Arc<OriginalStore>,
    pub advisor: Arc<dyn Advisor>,
    pub checkpoints: Arc<dyn Checkpointer>,
    /// Placeholders of values masked by the DLP stage, restored in tool inputs and shown text.
    pub vault: Arc<SecretVault>,
    /// What each agent works on (its prompt or task), given to tools as `ToolContext::goal`.
    pub goals: Mutex<BTreeMap<String, Arc<str>>>,
    pub policy: PolicyBundle,
    /// Canonical project root.
    pub workspace: PathBuf,
    pub allowed_roots: Vec<PathBuf>,
    pub home: PathBuf,
    pub session_id: RwLock<String>,
    pub settings: RwLock<RuntimeSettings>,
    pub context: RwLock<SessionContext>,
    pub transcript: Mutex<Option<Transcript>>,
    /// Shared with the proxy health monitor.
    pub proxy_upstreams: Arc<RwLock<Vec<ProxyUpstream>>>,
    /// MCP servers whose tools are hidden from the model until `ToolSearch` loads them
    /// (intent detection found them irrelevant to the prompt).
    pub deferred_servers: RwLock<BTreeSet<String>>,
    /// Latest plan per agent, from TodoWrite calls.
    pub plans: Mutex<BTreeMap<String, Vec<TodoItem>>>,
    /// Highest checkpoint id that existed at the latest compaction (0 when none): checkpoints up
    /// to it cannot rewind the conversation any more.
    compacted_through_checkpoint: AtomicU32,
    /// Reviewer delegations in the current user turn.
    pub review_count: AtomicU32,
    /// Parallel executors per delegation depth (one pool per depth, so nested delegation cannot
    /// starve its own parents).
    agent_slots: Mutex<BTreeMap<u32, Arc<Semaphore>>>,
    max_slots: usize,
    next_agent: AtomicU32,
}

/// What `Runtime::new` needs that has no sensible default.
pub struct RuntimeParts {
    pub config: Arc<Config>,
    pub providers: BTreeMap<String, Arc<dyn Provider>>,
    pub tools: Vec<Arc<dyn Tool>>,
    pub permissions: Arc<dyn PermissionPolicy>,
    pub approver: Arc<dyn Approver>,
    pub hooks: Arc<dyn HookRunner>,
    pub optimizer: Arc<dyn RequestOptimizer>,
    pub filters: Arc<dyn ContentFilter>,
    pub events: EventSink,
    pub policy: PolicyBundle,
    pub workspace: PathBuf,
    pub home: PathBuf,
    pub session_id: String,
    /// Shared with the optimizer so `Retrieve` finds what Headroom and RTK shortened.
    pub store: Arc<OriginalStore>,
    pub advisor: Arc<dyn Advisor>,
    pub checkpoints: Arc<dyn Checkpointer>,
    /// Shared with the DLP stage of the optimizer.
    pub vault: Arc<SecretVault>,
}

fn read<T>(lock: &RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    lock.read().unwrap_or_else(|e| e.into_inner())
}

fn write<T>(lock: &RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    lock.write().unwrap_or_else(|e| e.into_inner())
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Runtime {
    pub fn new(parts: RuntimeParts) -> Arc<Self> {
        let config = parts.config;
        let workspace = parts.workspace.canonicalize().unwrap_or(parts.workspace);
        let mut allowed_roots = vec![workspace.clone()];
        for d in &config.permissions.additional_dirs {
            let d = if d.is_absolute() { d.clone() } else { workspace.join(d) };
            if let Ok(c) = d.canonicalize() {
                allowed_roots.push(c);
            }
        }
        let settings = RuntimeSettings {
            orchestrator: config.orchestrator.model.clone(),
            domain_overrides: BTreeMap::new(),
            ponytail: config.optimizer.ponytail.clone(),
            provider_routes: BTreeMap::new(),
            hidden_tools: Vec::new(),
            append_system: None,
            system_override: None,
            max_turns: None,
            orchestrator_pinned: false,
            routed_tier: None,
        };
        let slots = config.orchestrator.max_parallel_agents.max(1);
        Arc::new(Runtime {
            providers: parts.providers,
            tools: parts.tools.into_iter().map(|t| (t.name().to_owned(), t)).collect(),
            permissions: parts.permissions,
            approver: parts.approver,
            hooks: parts.hooks,
            optimizer: parts.optimizer,
            filters: parts.filters,
            events: parts.events,
            store: parts.store,
            advisor: parts.advisor,
            checkpoints: parts.checkpoints,
            vault: parts.vault,
            goals: Mutex::new(BTreeMap::new()),
            policy: parts.policy,
            workspace,
            allowed_roots,
            home: parts.home,
            session_id: RwLock::new(parts.session_id),
            settings: RwLock::new(settings),
            context: RwLock::new(SessionContext::default()),
            transcript: Mutex::new(None),
            proxy_upstreams: Arc::new(RwLock::new(config.proxy.upstreams.clone())),
            deferred_servers: RwLock::new(BTreeSet::new()),
            plans: Mutex::new(BTreeMap::new()),
            compacted_through_checkpoint: AtomicU32::new(0),
            review_count: AtomicU32::new(0),
            agent_slots: Mutex::new(BTreeMap::new()),
            max_slots: slots,
            next_agent: AtomicU32::new(1),
            config,
        })
    }

    /// MCP servers currently deferred (their tools are not sent to the model).
    pub fn deferred_servers(&self) -> BTreeSet<String> {
        read(&self.deferred_servers).clone()
    }

    /// Replaces the deferred MCP servers (intent detection, once per prompt).
    pub fn set_deferred_servers(&self, servers: BTreeSet<String>) {
        *write(&self.deferred_servers) = servers;
    }

    /// Loads a deferred server's tools (`ToolSearch`). False when it was not deferred.
    pub fn load_deferred(&self, server: &str) -> bool {
        write(&self.deferred_servers).remove(server)
    }

    /// Whether a tool belongs to a deferred MCP server (`mcp__<server>__<tool>`).
    pub fn is_deferred_tool(&self, tool: &str) -> bool {
        let Some(rest) = tool.strip_prefix("mcp__") else {
            return false;
        };
        read(&self.deferred_servers).iter().any(|s| rest.strip_prefix(s.as_str()).is_some_and(|t| t.starts_with("__")))
    }

    pub fn session_id(&self) -> String {
        read(&self.session_id).clone()
    }

    pub fn set_session_id(&self, id: String) {
        *write(&self.session_id) = id;
    }

    pub fn settings(&self) -> RuntimeSettings {
        read(&self.settings).clone()
    }

    pub fn update_settings(&self, f: impl FnOnce(&mut RuntimeSettings)) {
        f(&mut write(&self.settings));
    }

    pub fn context(&self) -> SessionContext {
        read(&self.context).clone()
    }

    pub fn update_context(&self, f: impl FnOnce(&mut SessionContext)) {
        f(&mut write(&self.context));
    }

    pub fn set_transcript(&self, t: Option<Transcript>) {
        *lock(&self.transcript) = t;
    }

    /// Appends to the transcript; a write failure is reported once, never fatal.
    pub fn log(&self, entry: &Entry) {
        let mut guard = lock(&self.transcript);
        if let Some(t) = guard.as_mut()
            && let Err(e) = t.append(entry)
        {
            self.events.warn(format!("transcript write failed, persistence disabled: {e}"));
            *guard = None;
        }
    }

    pub fn set_plan(&self, agent_id: &str, items: Vec<TodoItem>) {
        lock(&self.plans).insert(agent_id.to_owned(), items);
    }

    pub fn plan(&self, agent_id: &str) -> Option<Vec<TodoItem>> {
        lock(&self.plans).get(agent_id).cloned()
    }

    /// Records the newest checkpoint id at a compaction of the conversation.
    pub fn mark_compaction(&self, newest_checkpoint: u32) {
        self.compacted_through_checkpoint.store(newest_checkpoint, Ordering::Relaxed);
    }

    /// Checkpoints up to this id were taken before the latest compaction.
    pub fn compacted_through_checkpoint(&self) -> u32 {
        self.compacted_through_checkpoint.load(Ordering::Relaxed)
    }

    /// Drops what the optimizer and the deferred MCP set keep about the conversations `ids`
    /// (`/clear`, `/resume`, `/rewind`).
    pub fn forget_conversation(&self, ids: &[&str]) {
        self.set_deferred_servers(BTreeSet::new());
        for id in ids {
            self.optimizer.forget_session(id);
        }
    }

    /// `domain-<n>` ids, unique in the session.
    pub fn next_agent_id(&self, domain: &str) -> String {
        format!("{domain}-{}", self.next_agent.fetch_add(1, Ordering::Relaxed))
    }

    pub fn reset_turn_counters(&self) {
        self.review_count.store(0, Ordering::Relaxed);
    }

    /// The executor pool of one delegation depth.
    pub fn agent_slots(&self, depth: u32) -> Arc<Semaphore> {
        lock(&self.agent_slots).entry(depth).or_insert_with(|| Arc::new(Semaphore::new(self.max_slots))).clone()
    }

    /// Forgets every agent's plan (new or resumed conversation).
    pub fn clear_plans(&self) {
        lock(&self.plans).clear();
    }

    /// A domain with configured and runtime overrides applied.
    pub fn domain(&self, name: &str) -> Option<ResolvedDomain> {
        let mut d = self.config.domain(name)?;
        if let Some(o) = read(&self.settings).domain_overrides.get(name) {
            d.model = d.model.apply(o);
        }
        Some(d)
    }

    pub fn domain_names(&self) -> Vec<String> {
        self.config.domain_names()
    }

    pub fn provider(&self, id: &str) -> Option<Arc<dyn Provider>> {
        self.providers.get(id).cloned()
    }

    /// Proxy base URL the calls of `provider_id` go through, if any.
    pub fn provider_route(&self, provider_id: &str) -> Option<String> {
        if let Some(url) = read(&self.settings).provider_routes.get(provider_id) {
            return Some(url.clone());
        }
        let name = self.config.providers.get(provider_id)?.via_proxy.as_ref()?;
        read(&self.proxy_upstreams).iter().find(|u| &u.name == name && u.enabled).map(|u| u.url.clone())
    }

    pub fn tool_context(&self, agent_id: &str, cancel: CancellationToken) -> ToolContext {
        ToolContext {
            agent_id: agent_id.to_owned(),
            session_id: self.session_id(),
            workspace: self.workspace.clone(),
            allowed_roots: self.allowed_roots.clone(),
            cancel,
            events: self.events.clone(),
            store: self.store.clone(),
            config: self.config.clone(),
            filters: self.filters.clone(),
            advisor: self.advisor.clone(),
            checkpoints: self.checkpoints.clone(),
            goal: self.goal(agent_id),
        }
    }

    /// Records what `agent_id` works on.
    pub fn set_goal(&self, agent_id: &str, goal: &str) {
        lock(&self.goals).insert(agent_id.to_owned(), Arc::from(goal));
    }

    pub fn goal(&self, agent_id: &str) -> Arc<str> {
        lock(&self.goals).get(agent_id).cloned().unwrap_or_else(|| Arc::from(""))
    }

    /// Ponytail text for the current level, `None` when disabled.
    pub fn policy_text(&self) -> Option<(String, String)> {
        let p = read(&self.settings).ponytail.clone();
        if !p.enabled {
            return None;
        }
        let text = self.policy.texts.get(&p.level).or_else(|| self.policy.texts.get("full"))?;
        Some((text.clone(), p.level))
    }
}
