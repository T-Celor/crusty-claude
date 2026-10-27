//! Session-wide services shared by the orchestrator and every executor.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use crusty_core::config::{ModelOverride, ModelProfile, PonytailConfig, ProxyUpstream, ResolvedDomain};
use crusty_core::event::TodoItem;
use crusty_core::filter::ContentFilter;
use crusty_core::hooks::HookRunner;
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
}

/// Prompt material gathered at session start.
#[derive(Debug, Clone, Default)]
pub struct SessionContext {
    pub memory: String,
    pub memory_files: Vec<PathBuf>,
    pub repo_map: Option<String>,
    pub mcp_status: Vec<String>,
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
    /// Latest plan per agent, from TodoWrite calls.
    pub plans: Mutex<BTreeMap<String, Vec<TodoItem>>>,
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
            policy: parts.policy,
            workspace,
            allowed_roots,
            home: parts.home,
            session_id: RwLock::new(parts.session_id),
            settings: RwLock::new(settings),
            context: RwLock::new(SessionContext::default()),
            transcript: Mutex::new(None),
            proxy_upstreams: Arc::new(RwLock::new(config.proxy.upstreams.clone())),
            plans: Mutex::new(BTreeMap::new()),
            review_count: AtomicU32::new(0),
            agent_slots: Mutex::new(BTreeMap::new()),
            max_slots: slots,
            next_agent: AtomicU32::new(1),
            config,
        })
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
        }
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
