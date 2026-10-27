//! Application state and the handling of engine events and key presses.
//!
//! Everything here is pure (no terminal access), so it is unit-tested directly. The
//! event loop in `lib.rs` feeds it events and keys and draws it with `ui.rs`.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use crusty_core::checkpoint::{CheckpointSummary, RewindScope};
use crusty_core::config::Config;
use crusty_core::event::{AgentStatus, AppEvent, TodoItem, UiCommand};
use crusty_core::metrics::{Metrics, MetricsSnapshot};
use crusty_core::permission::{ApprovalDecision, ApprovalRequest, PermissionMode};

use crate::TuiOptions;
use crate::complete;
use crate::input::Editor;

/// Executor and tool runs kept for the Exec tab.
const EXEC_LOG_MAX: usize = 200;
/// Window in which a second Ctrl+C exits.
const DOUBLE_CTRL_C: Duration = Duration::from_secs(2);
/// Lines per PageUp/PageDown.
const PAGE: usize = 10;
/// Maximum rows of the slash-command completion popup.
pub const COMPLETION_ROWS: usize = 8;
/// Window in which a second Esc on an empty input opens the rewind picker.
const DOUBLE_ESC: Duration = Duration::from_millis(500);
/// Recently mentioned files kept for ranking.
const RECENT_MAX: usize = 20;

/// One row of the completion popup. `insert` is the whole input text after accepting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionItem {
    /// Shown in the popup: `/name` or a relative path.
    pub label: String,
    /// Description shown after the label (commands only).
    pub detail: String,
    /// Input text after the item is accepted.
    pub insert: String,
    /// Whether the item is a file (remembered as recently mentioned).
    pub file: bool,
}

/// The checkpoint picker opened by `CheckpointList`, newest first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RewindPicker {
    /// Checkpoints, sorted by id descending.
    pub items: Vec<CheckpointSummary>,
    /// Index into `items` of the highlighted row.
    pub selected: usize,
}

/// Short name of a rewind scope for messages.
pub fn scope_label(scope: RewindScope) -> &'static str {
    match scope {
        RewindScope::Code => "code",
        RewindScope::Conversation => "conversation",
        RewindScope::Both => "both",
    }
}

/// The main tabs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    /// The conversation.
    Chat,
    /// Table of tool runs.
    Exec,
    /// Info, warnings, errors and command outputs.
    Log,
}

/// Progress of one tool call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolStatus {
    Running,
    Ok(Duration),
    Failed(Duration),
}

/// One item of the conversation.
#[derive(Debug, Clone, PartialEq)]
pub enum Entry {
    /// A prompt the user sent.
    User { text: String },
    /// Assistant text. `label` is the domain of an executor, or `orchestrator`.
    Assistant { agent_id: String, label: String, text: String },
    /// Model reasoning (hidden when thinking display is off).
    Thinking { agent_id: String, text: String },
    /// A tool call, updated in place when it finishes.
    Tool { call_id: String, agent_id: String, summary: String, status: ToolStatus, preview: String, exit_code: Option<i32> },
    /// Neutral information.
    Info(String),
    /// A warning.
    Warning(String),
    /// An error.
    Error(String),
    /// A slash command and its output.
    Command { command: String, output: String },
    /// An executor delegated by the orchestrator.
    Delegation { agent_id: String, domain: String, model: String, task: String },
}

/// One row of the Exec tab.
#[derive(Debug, Clone, PartialEq)]
pub struct ExecRun {
    pub call_id: String,
    pub agent_id: String,
    pub summary: String,
    pub status: ToolStatus,
    pub exit_code: Option<i32>,
    /// Unix time the call started, in seconds.
    pub at_secs: u64,
}

/// What the side panel shows about one executor agent.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentView {
    pub id: String,
    pub domain: String,
    pub model: String,
    pub status: AgentStatus,
    /// Input plus output tokens reported by the provider.
    pub tokens: u64,
    pub cost_usd: f64,
    pub task: String,
}

/// The whole UI state.
#[derive(Debug)]
pub struct App {
    config: Arc<Config>,
    metrics: Arc<Metrics>,
    /// Metrics as of the last tick; every number in the side panel comes from here.
    pub(crate) snapshot: MetricsSnapshot,
    session_id: String,
    cwd: String,
    pub(crate) orchestrator: String,
    commands: Vec<(String, String)>,
    pub(crate) mode: PermissionMode,
    pub(crate) entries: Vec<Entry>,
    /// Entry index of the assistant text being streamed, per agent.
    open_text: HashMap<String, usize>,
    /// Entry index of the thinking being streamed, per agent.
    open_thinking: HashMap<String, usize>,
    pub(crate) agents: Vec<AgentView>,
    orchestrator_plan: Option<Vec<TodoItem>>,
    latest_plan: Vec<TodoItem>,
    pub(crate) exec_log: VecDeque<ExecRun>,
    pub(crate) approvals: VecDeque<ApprovalRequest>,
    /// Text typed for "deny with feedback" on the front approval; `Some` while typing.
    pub(crate) feedback: Option<String>,
    pub(crate) context_pct: Option<u64>,
    pub(crate) busy: bool,
    pub(crate) tab: Tab,
    pub(crate) show_panel: bool,
    pub(crate) show_thinking: bool,
    /// Lines scrolled up from the bottom; 0 follows the newest output.
    pub(crate) scroll_back: usize,
    pub(crate) input: Editor,
    /// Set by Esc to hide the completion popup until the input changes.
    completion_off: bool,
    last_ctrl_c: Option<Instant>,
    quit: bool,
    redraw: bool,
    /// Counts ticks; drives the spinners.
    pub(crate) spinner: usize,
    /// Relative paths of the workspace, for `@` completion.
    files: Vec<String>,
    /// Files mentioned or accepted recently, newest first.
    recent_files: Vec<String>,
    /// How often each slash command was submitted in this session.
    command_usage: HashMap<String, u32>,
    /// Highlighted row of the completion popup.
    pub(crate) completion_sel: usize,
    /// Time of the last Esc on an empty input, for the double-Esc rewind.
    last_esc: Option<Instant>,
    /// The checkpoint picker, while open.
    pub(crate) rewind: Option<RewindPicker>,
    /// Id of the newest checkpoint created in this session.
    pub(crate) last_checkpoint: Option<u32>,
    /// Set by Ctrl+R: the caller rebuilds the file index.
    index_refresh: bool,
}

impl App {
    /// The state at startup, configured from `opts`.
    pub fn new(opts: &TuiOptions) -> App {
        let tui = &opts.config.tui;
        App {
            config: Arc::clone(&opts.config),
            metrics: Arc::clone(&opts.metrics),
            snapshot: opts.metrics.snapshot(),
            session_id: opts.session_id.clone(),
            cwd: opts.cwd.clone(),
            orchestrator: opts.orchestrator.clone(),
            commands: opts.commands.clone(),
            mode: opts.permission_mode,
            entries: Vec::new(),
            open_text: HashMap::new(),
            open_thinking: HashMap::new(),
            agents: Vec::new(),
            orchestrator_plan: None,
            latest_plan: Vec::new(),
            exec_log: VecDeque::new(),
            approvals: VecDeque::new(),
            feedback: None,
            context_pct: None,
            busy: false,
            tab: Tab::Chat,
            show_panel: tui.show_optimizer_panel,
            show_thinking: tui.show_thinking,
            scroll_back: 0,
            input: Editor::new(),
            completion_off: false,
            last_ctrl_c: None,
            quit: false,
            redraw: false,
            spinner: 0,
            files: Vec::new(),
            recent_files: Vec::new(),
            command_usage: HashMap::new(),
            completion_sel: 0,
            last_esc: None,
            rewind: None,
            last_checkpoint: None,
            index_refresh: false,
        }
    }

    /// Replaces the file index used by `@` completion.
    pub fn set_file_index(&mut self, files: Vec<String>) {
        self.files = files;
    }

    /// Returns true once after Ctrl+R, when the file index must be rebuilt.
    pub fn take_index_refresh(&mut self) -> bool {
        std::mem::take(&mut self.index_refresh)
    }

    /// The most recent submitted prompt that starts with the typed text, as shown dimmed
    /// after the cursor. `None` while the popup is open or the cursor is not at the end.
    pub fn history_suggestion(&self) -> Option<&str> {
        let text = self.input.text();
        if text.is_empty() || text.contains('\n') || self.input.cursor() != text.chars().count() {
            return None;
        }
        if !self.completion_items().is_empty() {
            return None;
        }
        self.input.history().iter().rev().find(|h| h.len() > text.len() && h.starts_with(text)).map(String::as_str)
    }

    /// The configuration the UI was started with.
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// The working directory of the session.
    pub fn cwd(&self) -> &str {
        &self.cwd
    }

    /// The session id, shortened to 8 characters for the header.
    pub fn short_session(&self) -> String {
        self.session_id.chars().take(8).collect()
    }

    /// Whether the user or the engine asked to exit.
    pub fn should_quit(&self) -> bool {
        self.quit
    }

    /// Whether a turn is running.
    pub fn busy(&self) -> bool {
        self.busy
    }

    /// Whether the screen must be redrawn on every tick (spinners are moving).
    pub fn is_animating(&self) -> bool {
        self.busy || self.agents.iter().any(|a| is_running(&a.status))
    }

    /// Agents that are not finished, failed or cancelled.
    pub fn running_agents(&self) -> usize {
        self.agents.iter().filter(|a| is_running(&a.status)).count()
    }

    /// The plan to show: the orchestrator's, else the latest one from an executor.
    pub fn plan(&self) -> &[TodoItem] {
        self.orchestrator_plan.as_deref().unwrap_or(&self.latest_plan)
    }

    /// Whether the agent was spawned as an executor (not the orchestrator).
    pub fn is_executor(&self, agent_id: &str) -> bool {
        self.agents.iter().any(|a| a.id == agent_id)
    }

    /// Returns true when the caller must clear the terminal before the next draw (Ctrl+L).
    pub fn take_redraw_request(&mut self) -> bool {
        std::mem::take(&mut self.redraw)
    }

    /// Refreshes the metrics snapshot and advances the spinners. Returns true when any
    /// metric changed, so the caller knows a redraw is needed.
    pub fn on_tick(&mut self) -> bool {
        self.spinner = self.spinner.wrapping_add(1);
        let snap = self.metrics.snapshot();
        let changed = snap != self.snapshot;
        self.snapshot = snap;
        changed
    }

    /// Rows of the completion popup. Slash commands for `/prefix` (most used first), files
    /// for an `@query` token. Empty when the popup is dismissed with Esc or nothing matches.
    pub fn completion_items(&self) -> Vec<CompletionItem> {
        if self.completion_off {
            return Vec::new();
        }
        if let Some(prefix) = self.completion_prefix() {
            let mut matches: Vec<&(String, String)> = self.commands.iter().filter(|(n, _)| n.starts_with(prefix)).collect();
            matches.sort_by_key(|(n, _)| std::cmp::Reverse(self.command_usage.get(n.as_str()).copied().unwrap_or(0)));
            return matches
                .into_iter()
                .map(|(n, d)| CompletionItem { label: format!("/{n}"), detail: d.clone(), insert: format!("/{n} "), file: false })
                .collect();
        }
        if let Some((start, query)) = self.file_token() {
            let text = self.input.text();
            return complete::rank_files(&self.files, query, &self.recent_files, COMPLETION_ROWS)
                .into_iter()
                .map(|p| CompletionItem { insert: format!("{}@{p} ", &text[..start]), label: p, detail: String::new(), file: true })
                .collect();
        }
        Vec::new()
    }

    fn completion_prefix(&self) -> Option<&str> {
        let rest = self.input.text().strip_prefix('/')?;
        (!rest.contains(char::is_whitespace)).then_some(rest)
    }

    /// The last whitespace-separated token when it starts with `@`: its byte offset and the
    /// query after the `@`.
    fn file_token(&self) -> Option<(usize, &str)> {
        let text = self.input.text();
        let start = text.char_indices().rev().find(|(_, c)| c.is_whitespace()).map_or(0, |(i, c)| i + c.len_utf8());
        let query = text[start..].strip_prefix('@')?;
        Some((start, query))
    }

    /// Moves the popup highlight by `step`, wrapping around `len` rows.
    fn move_selection(&mut self, step: isize, len: usize) {
        let len = len as isize;
        self.completion_sel = (self.completion_sel as isize + step).rem_euclid(len) as usize;
    }

    /// Puts the highlighted popup row into the input and remembers it if it is a file.
    fn accept_selected(&mut self, items: &[CompletionItem]) {
        let Some(item) = items.get(self.completion_sel).or(items.first()) else {
            return;
        };
        if item.file {
            self.remember_file(&item.label.clone());
        }
        self.input.set_text(&item.insert);
    }

    /// Moves a file to the front of the recently mentioned list.
    fn remember_file(&mut self, path: &str) {
        self.recent_files.retain(|p| p != path);
        self.recent_files.insert(0, path.to_string());
        self.recent_files.truncate(RECENT_MAX);
    }

    /// Keys while the checkpoint picker is open: ↑/↓ select, `c` code, `v` conversation,
    /// Enter both, Esc closes.
    fn handle_rewind_key(&mut self, key: KeyEvent) -> Vec<UiCommand> {
        let Some(picker) = self.rewind.as_mut() else {
            return Vec::new();
        };
        let last = picker.items.len().saturating_sub(1);
        let scope = match key.code {
            KeyCode::Up => {
                picker.selected = picker.selected.saturating_sub(1);
                return Vec::new();
            }
            KeyCode::Down => {
                picker.selected = (picker.selected + 1).min(last);
                return Vec::new();
            }
            KeyCode::Char('c') => "code",
            KeyCode::Char('v') => "conversation",
            KeyCode::Enter => "both",
            KeyCode::Esc => {
                self.rewind = None;
                return Vec::new();
            }
            _ => return Vec::new(),
        };
        let id = picker.items.get(picker.selected).map(|i| i.id);
        self.rewind = None;
        match id {
            Some(id) => self.submit_text(&format!("/rewind {id} {scope}")),
            None => Vec::new(),
        }
    }

    /// Applies one engine event to the state.
    pub fn handle_event(&mut self, ev: AppEvent) {
        match ev {
            AppEvent::SessionStarted { session_id, cwd, orchestrator, permission_mode } => {
                self.session_id = session_id;
                self.cwd = cwd;
                self.orchestrator = orchestrator;
                self.mode = permission_mode;
            }
            AppEvent::AgentSpawned { agent_id, parent: _, domain, provider: _, model, task } => {
                self.agents.push(AgentView {
                    id: agent_id.clone(),
                    domain: domain.clone(),
                    model: model.clone(),
                    status: AgentStatus::Idle,
                    tokens: 0,
                    cost_usd: 0.0,
                    task: task.clone(),
                });
                self.entries.push(Entry::Delegation { agent_id, domain, model, task });
            }
            AppEvent::AgentStatus { agent_id, status } => {
                if let Some(a) = self.agent_mut(&agent_id) {
                    a.status = status;
                }
            }
            AppEvent::AgentFinished { agent_id, ok, summary, usage, cost_usd, turns: _ } => {
                if let Some(a) = self.agent_mut(&agent_id) {
                    a.status = if ok { AgentStatus::Done } else { AgentStatus::Failed(summary) };
                    // Usage events already add up per call; the final totals only raise them,
                    // so nothing is counted twice.
                    a.tokens = a.tokens.max(usage.input_tokens + usage.output_tokens);
                    a.cost_usd = a.cost_usd.max(cost_usd);
                }
            }
            AppEvent::UserMessage { text } => {
                let repeats = matches!(self.entries.last(), Some(Entry::User { text: t }) if *t == text);
                if !repeats {
                    self.entries.push(Entry::User { text });
                }
            }
            AppEvent::TextDelta { agent_id, text } => {
                if let Some(&i) = self.open_text.get(&agent_id) {
                    if let Some(Entry::Assistant { text: buf, .. }) = self.entries.get_mut(i) {
                        buf.push_str(&text);
                    }
                } else {
                    let label = self.label_for(&agent_id);
                    self.entries.push(Entry::Assistant { agent_id: agent_id.clone(), label, text });
                    self.open_text.insert(agent_id, self.entries.len() - 1);
                }
            }
            AppEvent::ThinkingDelta { agent_id, text } => {
                if let Some(&i) = self.open_thinking.get(&agent_id) {
                    if let Some(Entry::Thinking { text: buf, .. }) = self.entries.get_mut(i) {
                        buf.push_str(&text);
                    }
                } else {
                    self.entries.push(Entry::Thinking { agent_id: agent_id.clone(), text });
                    self.open_thinking.insert(agent_id, self.entries.len() - 1);
                }
            }
            AppEvent::AssistantText { agent_id, text } => {
                self.open_thinking.remove(&agent_id);
                match self.open_text.remove(&agent_id) {
                    Some(i) => {
                        if let Some(Entry::Assistant { text: buf, .. }) = self.entries.get_mut(i) {
                            *buf = text;
                        }
                    }
                    None if !text.is_empty() => {
                        let label = self.label_for(&agent_id);
                        self.entries.push(Entry::Assistant { agent_id, label, text });
                    }
                    None => {}
                }
            }
            AppEvent::ToolStarted { agent_id, call_id, tool, summary } => {
                let summary = if summary.is_empty() { tool } else { summary };
                self.exec_log.push_back(ExecRun {
                    call_id: call_id.clone(),
                    agent_id: agent_id.clone(),
                    summary: summary.clone(),
                    status: ToolStatus::Running,
                    exit_code: None,
                    at_secs: now_secs(),
                });
                while self.exec_log.len() > EXEC_LOG_MAX {
                    self.exec_log.pop_front();
                }
                self.entries.push(Entry::Tool {
                    call_id,
                    agent_id,
                    summary,
                    status: ToolStatus::Running,
                    preview: String::new(),
                    exit_code: None,
                });
            }
            AppEvent::ToolFinished { call_id, ok, preview, duration_ms, exit_code, .. } => {
                let duration = Duration::from_millis(duration_ms);
                let status = if ok { ToolStatus::Ok(duration) } else { ToolStatus::Failed(duration) };
                for entry in self.entries.iter_mut().rev() {
                    if let Entry::Tool { call_id: id, status: s, preview: p, exit_code: e, .. } = entry
                        && *id == call_id
                    {
                        *s = status.clone();
                        *p = preview.clone();
                        *e = exit_code;
                        break;
                    }
                }
                if let Some(run) = self.exec_log.iter_mut().rev().find(|r| r.call_id == call_id) {
                    run.status = status;
                    run.exit_code = exit_code;
                }
            }
            AppEvent::ApprovalRequested(req) => self.approvals.push_back(req),
            AppEvent::ApprovalResolved { id, .. } => {
                self.approvals.retain(|r| r.id != id);
                if self.approvals.is_empty() {
                    self.feedback = None;
                }
            }
            AppEvent::PlanUpdated { agent_id, items } => {
                if self.is_executor(&agent_id) {
                    self.latest_plan = items;
                } else {
                    self.orchestrator_plan = Some(items);
                }
            }
            AppEvent::Usage { agent_id, usage, cost_usd, .. } => {
                if let Some(a) = self.agent_mut(&agent_id) {
                    a.tokens += usage.input_tokens + usage.output_tokens;
                    a.cost_usd += cost_usd;
                }
            }
            AppEvent::ModelFallback { from, to, reason, .. } => {
                self.entries.push(Entry::Warning(format!("model fallback {from} -> {to}: {reason}")));
            }
            AppEvent::PermissionModeChanged { mode } => self.mode = mode,
            AppEvent::ModelChanged { target, model } => {
                if target == "orchestrator" {
                    self.orchestrator = model;
                }
            }
            AppEvent::ContextStatus { agent_id, est_tokens, window } => {
                if agent_id == "orchestrator" && window > 0 {
                    self.context_pct = Some(est_tokens.saturating_mul(100) / window);
                }
            }
            AppEvent::CommandOutput { command, output } => {
                // The command entry was pushed on submit; it takes the first output.
                match self.entries.last_mut() {
                    Some(Entry::Command { command: c, output: o }) if o.is_empty() => {
                        *c = command;
                        *o = output;
                    }
                    _ => self.entries.push(Entry::Command { command, output }),
                }
            }
            AppEvent::Info { message } => self.entries.push(Entry::Info(message)),
            AppEvent::Warning { message } => self.entries.push(Entry::Warning(message)),
            AppEvent::Error { message } => self.entries.push(Entry::Error(message)),
            AppEvent::TurnFinished { .. } => {
                self.busy = false;
                self.open_text.clear();
                self.open_thinking.clear();
            }
            AppEvent::ConversationReset { session_id } => {
                self.session_id = session_id;
                self.entries.clear();
                self.open_text.clear();
                self.open_thinking.clear();
                self.agents.clear();
                self.orchestrator_plan = None;
                self.latest_plan.clear();
                self.exec_log.clear();
                self.approvals.clear();
                self.feedback = None;
                self.busy = false;
                self.scroll_back = 0;
                self.context_pct = None;
            }
            AppEvent::CheckpointCreated { id, .. } => self.last_checkpoint = Some(id),
            AppEvent::CheckpointList { mut items } => {
                if items.is_empty() {
                    self.entries.push(Entry::Info("no checkpoints yet".into()));
                } else {
                    items.sort_by_key(|i| std::cmp::Reverse(i.id));
                    self.rewind = Some(RewindPicker { items, selected: 0 });
                }
            }
            AppEvent::Rewound { id, scope, files_restored, failures } => {
                self.entries
                    .push(Entry::Info(format!("rewound to checkpoint {id} ({}): {files_restored} files restored", scope_label(scope))));
                self.entries.extend(failures.into_iter().map(|f| Entry::Warning(format!("could not restore {f}"))));
            }
            AppEvent::Shutdown => self.quit = true,
            // Optimizer, gate, proxy, graph and policy events reach the screen through the
            // metrics snapshot, so they need no state of their own here.
            _ => {}
        }
    }

    /// Handles one key press and returns the commands to send to the session.
    pub fn handle_key(&mut self, key: KeyEvent) -> Vec<UiCommand> {
        if !self.approvals.is_empty() {
            return self.handle_approval_key(key);
        }
        if self.rewind.is_some() {
            return self.handle_rewind_key(key);
        }
        let before = self.input.text().to_string();
        let cmds = self.edit_key(key);
        if self.input.text() != before {
            self.completion_sel = 0;
        }
        cmds
    }

    /// Editing, history, completion and submit keys.
    fn edit_key(&mut self, key: KeyEvent) -> Vec<UiCommand> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        let items = self.completion_items();
        let popup = !items.is_empty();
        match key.code {
            KeyCode::Enter if shift || alt => self.input.newline(),
            KeyCode::Enter
                if popup && items.get(self.completion_sel).is_some_and(|i| i.insert.trim_end() != self.input.text().trim_end()) =>
            {
                self.accept_selected(&items);
            }
            KeyCode::Enter => return self.submit_input(),
            KeyCode::Char('j') if ctrl => self.input.newline(),
            KeyCode::Char('c') if ctrl => return self.ctrl_c(),
            KeyCode::Char('d') if ctrl => {
                if self.input.is_empty() {
                    self.quit = true;
                    return vec![UiCommand::Quit];
                }
                self.input.delete();
            }
            KeyCode::Char('a') if ctrl => self.input.home(),
            KeyCode::Char('e') if ctrl => self.input.end(),
            KeyCode::Char('u') if ctrl => self.input.clear(),
            KeyCode::Char('w') if ctrl => self.input.delete_word_back(),
            KeyCode::Char('t') if ctrl => self.show_panel = !self.show_panel,
            KeyCode::Char('o') if ctrl => self.show_thinking = !self.show_thinking,
            KeyCode::Char('l') if ctrl => self.redraw = true,
            KeyCode::Char('r') if ctrl => self.index_refresh = true,
            KeyCode::Char(c) if !ctrl && !alt => {
                self.input.insert_char(c);
                self.completion_off = false;
            }
            KeyCode::Backspace => self.input.backspace(),
            KeyCode::Delete => self.input.delete(),
            KeyCode::Left => self.input.left(),
            KeyCode::Right => match self.history_suggestion().map(str::to_string) {
                Some(full) => self.input.set_text(&full),
                None => self.input.right(),
            },
            KeyCode::Home => self.input.home(),
            KeyCode::End => self.input.end(),
            KeyCode::Up if popup => self.move_selection(-1, items.len()),
            KeyCode::Down if popup => self.move_selection(1, items.len()),
            KeyCode::Up => {
                if self.input.on_first_line() {
                    self.input.history_prev();
                } else {
                    self.input.up();
                }
            }
            KeyCode::Down => {
                if self.input.on_last_line() {
                    self.input.history_next();
                } else {
                    self.input.down();
                }
            }
            KeyCode::Tab if popup => self.complete(&items),
            KeyCode::BackTab if popup => self.move_selection(-1, items.len()),
            KeyCode::BackTab => return vec![UiCommand::SetPermissionMode(self.mode.next())],
            KeyCode::Esc if popup => self.completion_off = true,
            KeyCode::Esc => {
                if self.busy {
                    self.last_esc = None;
                    return vec![UiCommand::Interrupt];
                }
                if self.input.is_empty() {
                    let now = Instant::now();
                    if self.last_esc.is_some_and(|prev| now.duration_since(prev) <= DOUBLE_ESC) {
                        self.last_esc = None;
                        return self.submit_text("/rewind");
                    }
                    self.last_esc = Some(now);
                }
                self.completion_off = true;
            }
            KeyCode::PageUp => self.scroll_back += PAGE,
            KeyCode::PageDown => self.scroll_back = self.scroll_back.saturating_sub(PAGE),
            KeyCode::F(1) => self.tab = Tab::Chat,
            KeyCode::F(2) => self.tab = Tab::Exec,
            KeyCode::F(3) => self.tab = Tab::Log,
            _ => {}
        }
        Vec::new()
    }

    /// Handles pasted text: into the input, or into the feedback being typed.
    pub fn handle_paste(&mut self, text: &str) {
        let clean = text.replace("\r\n", "\n").replace('\r', "\n");
        if let Some(fb) = self.feedback.as_mut() {
            fb.push_str(&clean.replace('\n', " "));
        } else if self.approvals.is_empty() {
            self.input.insert_str(&clean);
            self.completion_off = false;
        }
    }

    /// Submits the input line (Enter). Blank input is ignored.
    fn submit_input(&mut self) -> Vec<UiCommand> {
        let text = self.input.text().trim_end().to_string();
        if text.trim().is_empty() {
            return Vec::new();
        }
        self.input.clear();
        self.input.push_history(&text);
        self.completion_off = false;
        self.submit_text(&text)
    }

    /// Records a prompt or `/command` as sent and returns the command for the session.
    /// Used for typed input and for the initial prompt. Blank text returns nothing.
    pub fn submit_text(&mut self, text: &str) -> Vec<UiCommand> {
        let text = text.trim_end();
        if text.is_empty() {
            return Vec::new();
        }
        for path in text.split_whitespace().filter_map(|w| w.strip_prefix('@')) {
            self.remember_file(path);
        }
        if let Some(rest) = text.strip_prefix('/') {
            if let Some(name) = rest.split_whitespace().next() {
                *self.command_usage.entry(name.to_string()).or_insert(0) += 1;
            }
            self.entries.push(Entry::Command { command: text.to_string(), output: String::new() });
        } else {
            self.busy = true;
            self.scroll_back = 0;
            self.entries.push(Entry::User { text: text.to_string() });
        }
        vec![UiCommand::Submit(text.to_string())]
    }

    /// Ctrl+C: clear the input, else interrupt a turn, else exit on a second press.
    fn ctrl_c(&mut self) -> Vec<UiCommand> {
        if !self.input.is_empty() {
            self.input.clear();
            return Vec::new();
        }
        if self.busy {
            return vec![UiCommand::Interrupt];
        }
        let now = Instant::now();
        if let Some(prev) = self.last_ctrl_c
            && now.duration_since(prev) <= DOUBLE_CTRL_C
        {
            self.quit = true;
            return vec![UiCommand::Quit];
        }
        self.last_ctrl_c = Some(now);
        self.entries.push(Entry::Info("Press Ctrl+C again to exit".into()));
        Vec::new()
    }

    /// Tab with the popup open: extends a `/prefix` to the common prefix of several
    /// commands, else accepts the highlighted row.
    fn complete(&mut self, items: &[CompletionItem]) {
        if items.len() > 1
            && let Some(prefix) = self.completion_prefix().map(str::to_string)
        {
            let names: Vec<String> = items.iter().map(|i| i.label.trim_start_matches('/').to_string()).collect();
            let common = common_prefix(&names);
            if common.len() > prefix.len() {
                self.input.set_text(&format!("/{common}"));
                return;
            }
        }
        self.accept_selected(items);
    }

    /// Keys while an approval is pending: they go to the approval modal only.
    fn handle_approval_key(&mut self, key: KeyEvent) -> Vec<UiCommand> {
        if let Some(fb) = self.feedback.as_mut() {
            match key.code {
                KeyCode::Enter => {
                    let text = self.feedback.take().unwrap_or_default();
                    let decision = if text.trim().is_empty() { ApprovalDecision::Deny } else { ApprovalDecision::DenyWithFeedback(text) };
                    return self.resolve_front(decision);
                }
                KeyCode::Esc => self.feedback = None,
                KeyCode::Backspace => {
                    fb.pop();
                }
                KeyCode::Char(c) => fb.push(c),
                _ => {}
            }
            return Vec::new();
        }
        match key.code {
            KeyCode::Char('y') => self.resolve_front(ApprovalDecision::AllowOnce),
            KeyCode::Char('a') => {
                let has_rule = self.approvals.front().is_some_and(|r| r.suggested_rule.is_some());
                if has_rule { self.resolve_front(ApprovalDecision::AllowAlways) } else { Vec::new() }
            }
            KeyCode::Char('n') | KeyCode::Esc => self.resolve_front(ApprovalDecision::Deny),
            KeyCode::Char('f') => {
                self.feedback = Some(String::new());
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    /// Answers the front approval with `decision` and removes it from the queue.
    fn resolve_front(&mut self, decision: ApprovalDecision) -> Vec<UiCommand> {
        self.feedback = None;
        match self.approvals.pop_front() {
            Some(req) => vec![UiCommand::Approval { id: req.id, decision }],
            None => Vec::new(),
        }
    }

    fn agent_mut(&mut self, agent_id: &str) -> Option<&mut AgentView> {
        self.agents.iter_mut().find(|a| a.id == agent_id)
    }

    /// Domain of an executor, or `orchestrator`.
    fn label_for(&self, agent_id: &str) -> String {
        self.agents.iter().find(|a| a.id == agent_id).map_or_else(|| "orchestrator".to_string(), |a| a.domain.clone())
    }
}

/// Whether an agent is still working (not finished, failed or cancelled).
pub fn is_running(status: &AgentStatus) -> bool {
    !matches!(status, AgentStatus::Done | AgentStatus::Failed(_) | AgentStatus::Cancelled)
}

/// The longest prefix shared by all names.
fn common_prefix(names: &[String]) -> String {
    let Some((first, rest)) = names.split_first() else {
        return String::new();
    };
    let mut prefix = first.clone();
    for name in rest {
        let shared = prefix.chars().zip(name.chars()).take_while(|(a, b)| a == b).count();
        prefix = prefix.chars().take(shared).collect();
    }
    prefix
}

/// Unix time in seconds (0 if the clock is before 1970).
fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyEvent;
    use crusty_core::message::Usage;
    use crusty_core::permission::RiskLevel;

    fn opts() -> TuiOptions {
        TuiOptions {
            config: Arc::new(Config::default()),
            session_id: "0123456789abcdef".into(),
            cwd: "/work".into(),
            orchestrator: "anthropic:opus".into(),
            permission_mode: PermissionMode::Default,
            metrics: Arc::new(Metrics::default()),
            commands: vec![
                ("help".into(), "show help".into()),
                ("history".into(), "show history".into()),
                ("clear".into(), "clear".into()),
            ],
            initial_prompt: None,
        }
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    fn type_str(app: &mut App, s: &str) {
        for c in s.chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
    }

    fn request(id: &str, rule: Option<&str>) -> ApprovalRequest {
        ApprovalRequest {
            id: id.into(),
            agent_id: "orchestrator".into(),
            tool: "Bash".into(),
            summary: "Bash(rm -rf target)".into(),
            risk: RiskLevel::High,
            reason: "deletes files".into(),
            suggested_rule: rule.map(str::to_string),
            preview: "rm -rf target".into(),
        }
    }

    #[test]
    fn enter_submits_and_clears() {
        let mut app = App::new(&opts());
        type_str(&mut app, "hello");
        let cmds = app.handle_key(key(KeyCode::Enter));
        assert_eq!(cmds, vec![UiCommand::Submit("hello".into())]);
        assert!(app.input.is_empty());
        assert!(app.busy());
        assert!(app.handle_key(key(KeyCode::Enter)).is_empty());
    }

    #[test]
    fn shift_enter_inserts_newline() {
        let mut app = App::new(&opts());
        type_str(&mut app, "a");
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT));
        type_str(&mut app, "b");
        assert_eq!(app.input.text(), "a\nb");
        assert!(app.handle_key(key(KeyCode::Enter)).len() == 1);
    }

    #[test]
    fn history_up_and_down_recall_prompts() {
        let mut app = App::new(&opts());
        for s in ["one", "two"] {
            type_str(&mut app, s);
            app.handle_key(key(KeyCode::Enter));
        }
        app.handle_key(key(KeyCode::Up));
        assert_eq!(app.input.text(), "two");
        app.handle_key(key(KeyCode::Up));
        assert_eq!(app.input.text(), "one");
        app.handle_key(key(KeyCode::Down));
        assert_eq!(app.input.text(), "two");
    }

    #[test]
    fn tab_completes_slash_command() {
        let mut app = App::new(&opts());
        type_str(&mut app, "/he");
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.input.text(), "/help ");
        assert!(app.completion_items().is_empty());
    }

    #[test]
    fn at_popup_filters_and_tab_accepts_file() {
        let mut app = App::new(&opts());
        app.set_file_index(vec!["src/main.rs".into(), "README.md".into()]);
        type_str(&mut app, "open @ma");
        let items = app.completion_items();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].label, "src/main.rs");
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.input.text(), "open @src/main.rs ");
    }

    #[test]
    fn slash_popup_ranks_by_session_usage() {
        let mut app = App::new(&opts());
        app.submit_text("/history");
        type_str(&mut app, "/");
        let names: Vec<String> = app.completion_items().into_iter().map(|i| i.label).collect();
        assert_eq!(names[0], "/history");
    }

    #[test]
    fn right_accepts_history_suggestion() {
        let mut app = App::new(&opts());
        type_str(&mut app, "hello world");
        app.handle_key(key(KeyCode::Enter));
        type_str(&mut app, "hel");
        assert_eq!(app.history_suggestion(), Some("hello world"));
        app.handle_key(key(KeyCode::Right));
        assert_eq!(app.input.text(), "hello world");
    }

    #[test]
    fn double_esc_on_empty_input_sends_rewind() {
        let mut app = App::new(&opts());
        assert!(app.handle_key(key(KeyCode::Esc)).is_empty());
        assert_eq!(app.handle_key(key(KeyCode::Esc)), vec![UiCommand::Submit("/rewind".into())]);
    }

    #[test]
    fn rewind_picker_navigates_and_submits_scope() {
        let mut app = App::new(&opts());
        let cp = |id: u32| CheckpointSummary {
            id,
            label: format!("prompt {id}"),
            files: 1,
            created: "2026-10-27T10:00:00Z".into(),
            message_index: 0,
        };
        app.handle_event(AppEvent::CheckpointList { items: vec![cp(1), cp(2)] });
        assert!(app.rewind.is_some());
        app.handle_key(key(KeyCode::Down));
        assert_eq!(app.handle_key(key(KeyCode::Char('c'))), vec![UiCommand::Submit("/rewind 1 code".into())]);
        assert!(app.rewind.is_none());
    }

    #[test]
    fn backtab_cycles_permission_mode() {
        let mut app = App::new(&opts());
        let cmds = app.handle_key(key(KeyCode::BackTab));
        assert_eq!(cmds, vec![UiCommand::SetPermissionMode(PermissionMode::AcceptEdits)]);
    }

    #[test]
    fn esc_interrupts_only_while_busy() {
        let mut app = App::new(&opts());
        assert!(app.handle_key(key(KeyCode::Esc)).is_empty());
        type_str(&mut app, "go");
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.handle_key(key(KeyCode::Esc)), vec![UiCommand::Interrupt]);
    }

    #[test]
    fn double_ctrl_c_quits() {
        let mut app = App::new(&opts());
        assert!(app.handle_key(ctrl('c')).is_empty());
        assert!(app.entries.iter().any(|e| matches!(e, Entry::Info(m) if m.contains("Press Ctrl+C again"))));
        assert_eq!(app.handle_key(ctrl('c')), vec![UiCommand::Quit]);
        assert!(app.should_quit());
    }

    #[test]
    fn ctrl_c_clears_input_first() {
        let mut app = App::new(&opts());
        type_str(&mut app, "x");
        assert!(app.handle_key(ctrl('c')).is_empty());
        assert!(app.input.is_empty());
        assert!(!app.should_quit());
    }

    #[test]
    fn approval_keys_answer_and_pop() {
        let mut app = App::new(&opts());
        app.handle_event(AppEvent::ApprovalRequested(request("r1", Some("Bash(rm *)"))));
        app.handle_event(AppEvent::ApprovalRequested(request("r2", None)));
        assert_eq!(
            app.handle_key(key(KeyCode::Char('y'))),
            vec![UiCommand::Approval { id: "r1".into(), decision: ApprovalDecision::AllowOnce }]
        );
        // r2 has no suggested rule, so "a" does nothing.
        assert!(app.handle_key(key(KeyCode::Char('a'))).is_empty());
        assert_eq!(
            app.handle_key(key(KeyCode::Char('n'))),
            vec![UiCommand::Approval { id: "r2".into(), decision: ApprovalDecision::Deny }]
        );
        assert!(app.approvals.is_empty());
    }

    #[test]
    fn approval_always_and_feedback() {
        let mut app = App::new(&opts());
        app.handle_event(AppEvent::ApprovalRequested(request("r1", Some("Bash(rm *)"))));
        assert_eq!(
            app.handle_key(key(KeyCode::Char('a'))),
            vec![UiCommand::Approval { id: "r1".into(), decision: ApprovalDecision::AllowAlways }]
        );
        app.handle_event(AppEvent::ApprovalRequested(request("r2", None)));
        app.handle_key(key(KeyCode::Char('f')));
        type_str(&mut app, "use cargo");
        app.handle_key(key(KeyCode::Backspace));
        app.handle_key(key(KeyCode::Char('o')));
        assert_eq!(
            app.handle_key(key(KeyCode::Enter)),
            vec![UiCommand::Approval { id: "r2".into(), decision: ApprovalDecision::DenyWithFeedback("use cargo".into()) }]
        );
    }

    #[test]
    fn approval_feedback_escape_cancels_typing() {
        let mut app = App::new(&opts());
        app.handle_event(AppEvent::ApprovalRequested(request("r1", None)));
        app.handle_key(key(KeyCode::Char('f')));
        app.handle_key(key(KeyCode::Esc));
        assert!(app.feedback.is_none());
        assert_eq!(app.approvals.len(), 1);
        assert_eq!(app.handle_key(key(KeyCode::Esc)), vec![UiCommand::Approval { id: "r1".into(), decision: ApprovalDecision::Deny }]);
    }

    #[test]
    fn ctrl_t_toggles_panel() {
        let mut app = App::new(&opts());
        let before = app.show_panel;
        app.handle_key(ctrl('t'));
        assert_eq!(app.show_panel, !before);
    }

    #[test]
    fn text_delta_then_assistant_text_is_not_duplicated() {
        let mut app = App::new(&opts());
        app.handle_event(AppEvent::TextDelta { agent_id: "orchestrator".into(), text: "Hel".into() });
        app.handle_event(AppEvent::TextDelta { agent_id: "orchestrator".into(), text: "lo".into() });
        app.handle_event(AppEvent::AssistantText { agent_id: "orchestrator".into(), text: "Hello".into() });
        let texts: Vec<&str> = app
            .entries
            .iter()
            .filter_map(|e| match e {
                Entry::Assistant { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(texts, vec!["Hello"]);
    }

    #[test]
    fn tool_started_then_finished_updates_entry() {
        let mut app = App::new(&opts());
        app.handle_event(AppEvent::ToolStarted {
            agent_id: "orchestrator".into(),
            call_id: "c1".into(),
            tool: "Bash".into(),
            summary: "Bash(cargo test)".into(),
        });
        app.handle_event(AppEvent::ToolFinished {
            agent_id: "orchestrator".into(),
            call_id: "c1".into(),
            tool: "Bash".into(),
            ok: false,
            preview: "boom".into(),
            duration_ms: 1200,
            exit_code: Some(2),
        });
        assert_eq!(app.entries.len(), 1);
        match &app.entries[0] {
            Entry::Tool { status, preview, exit_code, .. } => {
                assert_eq!(*status, ToolStatus::Failed(Duration::from_millis(1200)));
                assert_eq!(preview, "boom");
                assert_eq!(*exit_code, Some(2));
            }
            other => panic!("unexpected entry {other:?}"),
        }
        assert_eq!(app.exec_log.len(), 1);
        assert_eq!(app.exec_log[0].exit_code, Some(2));
    }

    #[test]
    fn agents_spawn_update_and_finish() {
        let mut app = App::new(&opts());
        app.handle_event(AppEvent::AgentSpawned {
            agent_id: "a1".into(),
            parent: None,
            domain: "rust".into(),
            provider: "anthropic".into(),
            model: "claude-sonnet-5-5".into(),
            task: "fix bug".into(),
        });
        app.handle_event(AppEvent::AgentStatus { agent_id: "a1".into(), status: AgentStatus::Streaming });
        app.handle_event(AppEvent::Usage {
            agent_id: "a1".into(),
            provider: "anthropic".into(),
            model: "claude-sonnet-5-5".into(),
            usage: Usage { input_tokens: 1000, output_tokens: 200, ..Usage::default() },
            cost_usd: 0.01,
        });
        assert!(app.is_animating());
        app.handle_event(AppEvent::AgentFinished {
            agent_id: "a1".into(),
            ok: true,
            summary: "done".into(),
            usage: Usage { input_tokens: 1000, output_tokens: 200, ..Usage::default() },
            cost_usd: 0.01,
            turns: 2,
        });
        assert_eq!(app.agents[0].status, AgentStatus::Done);
        assert_eq!(app.agents[0].tokens, 1200);
        assert_eq!(app.running_agents(), 0);
        assert!(matches!(app.entries[0], Entry::Delegation { .. }));
    }

    #[test]
    fn plan_prefers_orchestrator_plan() {
        let mut app = App::new(&opts());
        app.handle_event(AppEvent::AgentSpawned {
            agent_id: "a1".into(),
            parent: None,
            domain: "docs".into(),
            provider: "p".into(),
            model: "m".into(),
            task: "t".into(),
        });
        let item = |s: &str| TodoItem { content: s.into(), status: crusty_core::event::TodoStatus::Pending };
        app.handle_event(AppEvent::PlanUpdated { agent_id: "a1".into(), items: vec![item("sub")] });
        assert_eq!(app.plan(), &[item("sub")]);
        app.handle_event(AppEvent::PlanUpdated { agent_id: "orchestrator".into(), items: vec![item("main")] });
        assert_eq!(app.plan(), &[item("main")]);
    }

    #[test]
    fn conversation_reset_clears_entries() {
        let mut app = App::new(&opts());
        type_str(&mut app, "hi");
        app.handle_key(key(KeyCode::Enter));
        app.handle_event(AppEvent::ConversationReset { session_id: "new".into() });
        assert!(app.entries.is_empty());
        assert!(!app.busy());
    }

    #[test]
    fn conversation_reset_clears_context_plan_and_agents() {
        let mut app = App::new(&opts());
        app.handle_event(AppEvent::ContextStatus { agent_id: "orchestrator".into(), est_tokens: 500, window: 1000 });
        app.handle_event(AppEvent::AgentSpawned {
            agent_id: "a1".into(),
            parent: None,
            domain: "rust".into(),
            provider: "p".into(),
            model: "m".into(),
            task: "t".into(),
        });
        let item = TodoItem { content: "step".into(), status: crusty_core::event::TodoStatus::Pending };
        app.handle_event(AppEvent::PlanUpdated { agent_id: "orchestrator".into(), items: vec![item] });
        assert_eq!(app.context_pct, Some(50));
        app.handle_event(AppEvent::ConversationReset { session_id: "new".into() });
        assert_eq!(app.context_pct, None);
        assert!(app.plan().is_empty());
        assert!(app.agents.is_empty());
    }

    #[test]
    fn turn_finished_clears_busy() {
        let mut app = App::new(&opts());
        type_str(&mut app, "hi");
        app.handle_key(key(KeyCode::Enter));
        assert!(app.busy());
        app.handle_event(AppEvent::TurnFinished { ok: true });
        assert!(!app.busy());
    }

    #[test]
    fn command_output_fills_its_entry() {
        let mut app = App::new(&opts());
        type_str(&mut app, "/help");
        app.handle_key(key(KeyCode::Enter));
        app.handle_event(AppEvent::CommandOutput { command: "/help".into(), output: "text".into() });
        assert_eq!(app.entries.len(), 1);
        assert!(matches!(&app.entries[0], Entry::Command { output, .. } if output == "text"));
    }

    #[test]
    fn common_prefix_of_names() {
        let names = vec!["history".to_string(), "help".to_string()];
        assert_eq!(common_prefix(&names), "h");
    }
}
