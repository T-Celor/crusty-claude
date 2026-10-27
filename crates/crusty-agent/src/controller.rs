//! The session controller: turns UI commands into orchestrator turns, slash commands,
//! `!shell` lines and `#memory` notes. One orchestrator turn runs at a time; prompts typed
//! meanwhile are queued.

use std::collections::VecDeque;
use std::sync::Arc;

use crusty_core::hooks::HookEvent;
use crusty_core::{AppEvent, CancellationToken, ContentBlock, Message, Role, UiCommand};
use crusty_session::commands::CustomCommand;
use crusty_session::transcript::{self, Entry, Transcript};
use serde_json::json;
use tokio::sync::Mutex as AsyncMutex;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::task::JoinHandle;

use crate::approval::ChannelApprover;
use crate::engine::{ORCHESTRATOR_ID, StopKind, orchestrator_spec, run_agent};
use crate::runtime::Runtime;
use crate::{commands, sessions_dir};

pub struct Controller {
    pub(crate) rt: Arc<Runtime>,
    pub(crate) approver: Arc<ChannelApprover>,
    pub(crate) convo: Arc<AsyncMutex<Vec<Message>>>,
    pub(crate) turn: Option<(JoinHandle<()>, CancellationToken)>,
    pub(crate) queue: VecDeque<String>,
    pub(crate) custom_commands: Vec<CustomCommand>,
    pub(crate) quit: bool,
}

/// Runs the SessionStart hook and keeps its context for the prompts.
pub async fn session_start_hook(rt: &Runtime) {
    let hook = rt.hooks.run(HookEvent::SessionStart, None, &json!({"session_id": rt.session_id()})).await;
    if let Some(extra) = hook.additional_context.filter(|s| !s.trim().is_empty()) {
        rt.update_context(|c| {
            c.memory.push_str("\n\n# Session start context\n");
            c.memory.push_str(&extra);
        });
    }
}

/// Builds the user message of a prompt, running the UserPromptSubmit hook.
/// `None` when a hook blocked the prompt.
pub async fn prepare_prompt(rt: &Runtime, prompt: &str) -> Option<Message> {
    let hook = rt.hooks.run(HookEvent::UserPromptSubmit, None, &json!({"prompt": prompt})).await;
    if hook.block {
        rt.events.warn(format!("prompt blocked by UserPromptSubmit hook: {}", hook.message.unwrap_or_default()));
        return None;
    }
    let mut msg = Message::user_text(prompt);
    if let Some(extra) = hook.additional_context.filter(|s| !s.trim().is_empty()) {
        msg.content.push(ContentBlock::text(extra));
    }
    Some(msg)
}

/// Replays a loaded conversation to the UI (text only).
pub fn replay(rt: &Runtime, messages: &[Message]) {
    for m in messages {
        let text = m.text();
        if text.trim().is_empty() {
            continue;
        }
        match m.role {
            Role::User => rt.events.emit(AppEvent::UserMessage { text }),
            _ => rt.events.emit(AppEvent::AssistantText { agent_id: ORCHESTRATOR_ID.into(), text }),
        }
    }
}

async fn wait_turn(turn: &mut Option<(JoinHandle<()>, CancellationToken)>) -> Result<(), tokio::task::JoinError> {
    match turn {
        Some((handle, _)) => handle.await,
        None => futures::future::pending().await,
    }
}

impl Controller {
    pub fn new(rt: Arc<Runtime>, approver: Arc<ChannelApprover>, messages: Vec<Message>, custom_commands: Vec<CustomCommand>) -> Self {
        Controller {
            rt,
            approver,
            convo: Arc::new(AsyncMutex::new(messages)),
            turn: None,
            queue: VecDeque::new(),
            custom_commands,
            quit: false,
        }
    }

    pub fn busy(&self) -> bool {
        self.turn.is_some()
    }

    /// Processes UI commands until `Quit` or until the UI goes away.
    pub async fn run(mut self, mut rx: UnboundedReceiver<UiCommand>) {
        while !self.quit {
            tokio::select! {
                cmd = rx.recv() => match cmd {
                    Some(cmd) => self.handle(cmd).await,
                    None => break,
                },
                done = wait_turn(&mut self.turn) => {
                    self.turn = None;
                    if let Err(e) = done
                        && e.is_panic()
                    {
                        self.rt.events.error("the turn crashed (internal error); the conversation is kept");
                        self.rt.events.emit(AppEvent::TurnFinished { ok: false });
                    }
                    if let Some(next) = self.queue.pop_front() {
                        self.submit(next).await;
                    }
                }
            }
        }
        self.shutdown().await;
    }

    async fn shutdown(&mut self) {
        if let Some((handle, cancel)) = self.turn.take() {
            cancel.cancel();
            self.approver.deny_all();
            let _ = handle.await;
        }
        self.rt.hooks.run(HookEvent::SessionEnd, None, &json!({"session_id": self.rt.session_id()})).await;
        self.rt.events.emit(AppEvent::Shutdown);
    }

    pub(crate) async fn handle(&mut self, cmd: UiCommand) {
        match cmd {
            UiCommand::Submit(text) => self.submit(text).await,
            UiCommand::Interrupt => self.interrupt(),
            UiCommand::Approval { id, decision } => {
                if !self.approver.resolve(&id, decision) {
                    self.rt.events.warn("that approval is no longer pending");
                }
            }
            UiCommand::SetPermissionMode(mode) => {
                self.rt.permissions.set_mode(mode);
                self.rt.events.emit(AppEvent::PermissionModeChanged { mode });
            }
            UiCommand::Quit => self.quit = true,
        }
    }

    pub(crate) fn interrupt(&mut self) {
        if let Some((_, cancel)) = &self.turn {
            cancel.cancel();
            self.approver.deny_all();
            if !self.queue.is_empty() {
                self.rt.events.info(format!("interrupted; {} queued prompt(s) dropped", self.queue.len()));
                self.queue.clear();
            } else {
                self.rt.events.info("interrupted");
            }
        }
    }

    pub(crate) async fn submit(&mut self, text: String) {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return;
        }
        if let Some(rest) = trimmed.strip_prefix('/') {
            let (name, args) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
            let (name, args) = (name.to_owned(), args.trim().to_owned());
            if self.busy() && commands::needs_idle(&name, &self.custom_commands) {
                self.queue.push_back(text);
                self.rt.events.info(format!("/{name} queued until the current turn ends"));
                return;
            }
            commands::handle(self, &name, &args).await;
        } else if self.busy() {
            self.queue.push_back(text);
            self.rt.events.info("message queued until the current turn ends");
        } else if let Some(command) = trimmed.strip_prefix('!') {
            self.shell(command.trim().to_owned());
        } else if let Some(note) = trimmed.strip_prefix('#').filter(|n| !n.starts_with('#')) {
            self.remember(note.trim());
        } else {
            self.start_turn(trimmed.to_owned()).await;
        }
    }

    /// `#note`: appended to the project memory file and to the prompts.
    fn remember(&mut self, note: &str) {
        if note.is_empty() {
            return;
        }
        match crusty_session::memory::append_memory(&self.rt.workspace, note) {
            Ok(path) => {
                commands::reload_memory(&self.rt);
                self.rt.events.info(format!("added to memory: {}", crate::display_path(&path)));
            }
            Err(e) => self.rt.events.error(format!("cannot write memory: {e}")),
        }
    }

    /// Runs a long command (`!cmd`, `/compact`, `/graph rebuild`) like a turn: in the
    /// background, interruptible with Esc, with prompts typed meanwhile queued.
    pub(crate) fn spawn_job<F>(&mut self, job: impl FnOnce(CancellationToken) -> F)
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        let cancel = CancellationToken::new();
        let rt = self.rt.clone();
        let fut = job(cancel.clone());
        let handle = tokio::spawn(async move {
            fut.await;
            rt.events.emit(AppEvent::TurnFinished { ok: true });
        });
        self.turn = Some((handle, cancel));
    }

    /// `!command`: run by the user's own request in the workspace (the user typed it, so no
    /// approval), output added to the conversation so the orchestrator sees it.
    fn shell(&mut self, command: String) {
        let Some(bash) = self.rt.tools.get("Bash").cloned() else {
            self.rt.events.error("the Bash tool is not available");
            return;
        };
        let rt = self.rt.clone();
        let convo = self.convo.clone();
        self.spawn_job(move |cancel| async move {
            let ctx = rt.tool_context("user", cancel);
            let output = match bash.call(json!({"command": command}), &ctx).await {
                Ok(o) => o.text_content(),
                Err(e) => format!("error: {e}"),
            };
            rt.events.emit(AppEvent::CommandOutput { command: format!("!{command}"), output: output.clone() });
            // The output is data: a closing tag inside it must not end the wrapper.
            let output = output.replace("</bash-output>", "<\\/bash-output>");
            let msg = Message::user_text(format!("<bash-input>{command}</bash-input>\n<bash-output>{output}</bash-output>"));
            rt.log(&Entry::Message { agent_id: ORCHESTRATOR_ID.into(), message: msg.clone(), ts: chrono::Utc::now() });
            convo.lock().await.push(msg);
        });
    }

    /// Starts an orchestrator turn in the background.
    pub(crate) async fn start_turn(&mut self, prompt: String) {
        let Some(message) = prepare_prompt(&self.rt, &prompt).await else {
            return;
        };
        self.rt.reset_turn_counters();
        self.rt.events.emit(AppEvent::UserMessage { text: prompt });
        let rt = self.rt.clone();
        let convo = self.convo.clone();
        let cancel = CancellationToken::new();
        let token = cancel.clone();
        let handle = tokio::spawn(async move {
            let mut messages = convo.lock().await;
            rt.log(&Entry::Message { agent_id: ORCHESTRATOR_ID.into(), message: message.clone(), ts: chrono::Utc::now() });
            messages.push(message);
            let spec = orchestrator_spec(&rt);
            let outcome = run_agent(&rt, &spec, &mut messages, &token).await;
            rt.log(&Entry::Usage {
                input_tokens: outcome.usage.total_input(),
                output_tokens: outcome.usage.output_tokens,
                cost_usd: outcome.cost_usd,
                ts: chrono::Utc::now(),
            });
            match &outcome.stop {
                StopKind::Done => {}
                StopKind::Cancelled => {}
                StopKind::Error(e) => rt.events.error(format!("turn failed: {e}")),
                other => rt.events.warn(format!("turn stopped: {}", other.label())),
            }
            rt.events.emit(AppEvent::TurnFinished { ok: outcome.stop == StopKind::Done });
        });
        self.turn = Some((handle, cancel));
    }

    /// Starts a fresh conversation with a new transcript.
    pub(crate) async fn reset(&mut self) {
        let id = transcript::new_session_id();
        self.convo.lock().await.clear();
        self.rt.clear_plans();
        self.rt.set_session_id(id.clone());
        let dir = sessions_dir(&self.rt);
        match Transcript::create(&dir, &id, &self.rt.workspace) {
            Ok(t) => self.rt.set_transcript(Some(t)),
            Err(e) => {
                self.rt.set_transcript(None);
                self.rt.events.warn(format!("transcript disabled: {e}"));
            }
        }
        self.rt.events.emit(AppEvent::ConversationReset { session_id: id });
    }

    /// Replaces the conversation with a saved session.
    pub(crate) async fn resume(&mut self, path: &std::path::Path) -> Result<(), String> {
        let loaded = transcript::load(path).map_err(|e| e.to_string())?;
        for w in &loaded.warnings {
            self.rt.events.warn(w.clone());
        }
        let transcript = Transcript::open_append(path).map_err(|e| e.to_string())?;
        self.rt.set_session_id(loaded.session_id.clone());
        self.rt.set_transcript(Some(transcript));
        self.rt.clear_plans();
        self.rt.events.emit(AppEvent::ConversationReset { session_id: loaded.session_id.clone() });
        replay(&self.rt, &loaded.messages);
        *self.convo.lock().await = loaded.messages;
        Ok(())
    }
}
