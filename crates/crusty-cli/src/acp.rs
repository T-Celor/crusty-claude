//! `crusty acp`: the Agent Client Protocol on stdio, for editors (Zed, JetBrains, Neovim,
//! Emacs). Each ACP session is one engine session; a prompt runs as one orchestrator turn,
//! as `crusty -p` does. Nothing but protocol messages may go to stdout.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use crusty_acp::{AcpBackend, AgentInfo, ApprovalBridge, McpServerSpec, PromptPart, SessionHandle, StopReason, serve};
use crusty_agent::controller::prepare_prompt;
use crusty_agent::{ChannelApprover, ORCHESTRATOR_ID, Runtime, StopKind, orchestrator_spec, run_agent};
use crusty_core::config::McpServerConfig;
use crusty_core::permission::{ApprovalRequest, PermissionMode};
use crusty_core::{AppEvent, CancellationToken, ContentBlock, Message, Role, async_trait};
use crusty_mcp::McpManager;
use crusty_session::transcript::Entry;
use serde_json::Value;
use tokio::sync::Mutex;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use crate::{Cli, wiring};

/// Modes offered to the client: `(id, name)`; the first one is the default.
const MODES: [(&str, &str); 5] = [
    ("default", "Default"),
    ("acceptEdits", "Accept edits"),
    ("plan", "Plan"),
    ("auto", "Auto"),
    ("bypassPermissions", "Bypass permissions"),
];

/// One open ACP session: a built engine session and what its turns need.
struct AcpSession {
    rt: Arc<Runtime>,
    approver: Arc<ChannelApprover>,
    background: CancellationToken,
    /// Taken when the session shuts down, since stopping the servers consumes the manager.
    mcp: Mutex<Option<McpManager>>,
    /// The orchestrator's conversation; held for the whole turn, so turns of one session queue.
    conversation: Mutex<Vec<Message>>,
    /// Engine events; drained by the running turn and held for its length.
    events: Mutex<UnboundedReceiver<AppEvent>>,
}

impl AcpSession {
    /// Stops the background tasks and the MCP servers of this session.
    async fn shutdown(&self) {
        self.background.cancel();
        let mcp = self.mcp.lock().await.take();
        if let Some(mcp) = mcp {
            mcp.shutdown().await;
        }
    }
}

/// The Crusty-Claude engine behind the ACP server.
pub struct CrustyAcp {
    base: Cli,
    sessions: Mutex<HashMap<String, Arc<AcpSession>>>,
}

impl CrustyAcp {
    /// A backend whose sessions take their settings from `base`.
    pub fn new(base: &Cli) -> Self {
        Self { base: base.clone(), sessions: Mutex::new(HashMap::new()) }
    }

    /// Stops every open session. Called when the ACP connection ends.
    pub async fn shutdown(&self) {
        let sessions: Vec<Arc<AcpSession>> = self.sessions.lock().await.drain().map(|(_, s)| s).collect();
        for session in sessions {
            session.shutdown().await;
        }
    }

    /// The command line of one session: `base` rooted at `cwd`, starting fresh.
    fn cli_for(&self, cwd: PathBuf) -> Cli {
        let mut cli = self.base.clone();
        cli.cwd = Some(cwd);
        cli.continue_session = false;
        cli.resume = None;
        cli
    }

    /// Builds an engine session; `mcp_servers` are added to the configured ones.
    async fn start(&self, cli: &Cli, mcp_servers: Vec<McpServerSpec>) -> Result<Arc<AcpSession>, String> {
        let mut loaded = wiring::load_config(cli).map_err(|e| format!("{e:#}"))?;
        let mut skipped = Vec::new();
        for spec in mcp_servers {
            match mcp_server_config(&spec.config) {
                Some(server) => {
                    loaded.config.mcp.servers.insert(spec.name, server);
                }
                None => skipped.push(spec.name),
            }
        }
        let session = wiring::build_session(cli, loaded, true).await.map_err(|e| format!("{e:#}"))?;
        for name in skipped {
            session.rt.events.warn(format!("MCP server `{name}` skipped: unsupported transport"));
        }
        wiring::announce(&session.rt, &session.warnings).await;
        if let Some(graph) = session.graph.clone() {
            tokio::spawn(wiring::index_graph(session.rt.clone(), graph));
        }
        let approver = session.approver.clone().ok_or_else(|| "the session has no approval channel".to_string())?;
        Ok(Arc::new(AcpSession {
            rt: session.rt,
            approver,
            background: session.background,
            mcp: Mutex::new(Some(session.mcp)),
            conversation: Mutex::new(session.messages),
            events: Mutex::new(session.events_rx),
        }))
    }

    /// Stores a started session under its id. A duplicate id keeps the open session.
    async fn register(&self, session: Arc<AcpSession>) -> SessionHandle {
        let id = session.rt.session_id();
        let mut sessions = self.sessions.lock().await;
        if sessions.contains_key(&id) {
            drop(sessions);
            session.shutdown().await;
        } else {
            sessions.insert(id.clone(), session);
        }
        SessionHandle { id }
    }

    async fn get(&self, id: &str) -> Result<Arc<AcpSession>, String> {
        self.sessions.lock().await.get(id).cloned().ok_or_else(|| format!("unknown session `{id}`"))
    }
}

#[async_trait]
impl AcpBackend for CrustyAcp {
    fn can_load_session(&self) -> bool {
        true
    }

    async fn new_session(&self, cwd: PathBuf, mcp_servers: Vec<McpServerSpec>) -> Result<SessionHandle, String> {
        let cli = self.cli_for(cwd);
        let session = self.start(&cli, mcp_servers).await?;
        Ok(self.register(session).await)
    }

    async fn load_session(&self, id: &str, cwd: PathBuf) -> Result<SessionHandle, String> {
        let mut cli = self.cli_for(cwd);
        cli.resume = Some(id.to_owned());
        let session = self.start(&cli, Vec::new()).await?;
        Ok(self.register(session).await)
    }

    async fn prompt(
        &self,
        session_id: &str,
        prompt: Vec<PromptPart>,
        events: UnboundedSender<AppEvent>,
        approvals: Arc<dyn ApprovalBridge>,
        cancel: CancellationToken,
    ) -> Result<StopReason, String> {
        let session = self.get(session_id).await?;
        let mut conversation = session.conversation.lock().await;
        let mut rx = session.events.lock().await;

        let mut blocks = prompt_blocks(&prompt);
        let hooked = prepare_prompt(&session.rt, &Message { role: Role::User, content: blocks.clone() }.text())
            .await
            .ok_or_else(|| "prompt blocked by a UserPromptSubmit hook".to_string())?;
        // prepare_prompt puts the flattened text first, then any context a hook added.
        blocks.extend(hooked.content.into_iter().skip(1));
        let message = Message { role: Role::User, content: blocks };
        session.rt.log(&Entry::Message { agent_id: ORCHESTRATOR_ID.into(), message: message.clone(), ts: chrono::Utc::now() });
        conversation.push(message);

        session.rt.reset_turn_counters();
        let spec = orchestrator_spec(&session.rt);
        let turn = CancellationToken::new();
        let outcome = {
            let run = run_agent(&session.rt, &spec, &mut conversation, &turn);
            tokio::pin!(run);
            let mut cancelled = false;
            loop {
                tokio::select! {
                    outcome = &mut run => break outcome,
                    Some(event) = rx.recv() => {
                        if let AppEvent::ApprovalRequested(request) = &event {
                            answer_approval(approvals.clone(), session.approver.clone(), request.clone());
                        }
                        let _ = events.send(event);
                    }
                    _ = cancel.cancelled(), if !cancelled => {
                        cancelled = true;
                        turn.cancel();
                        session.approver.deny_all();
                    }
                }
            }
        };
        session.rt.log(&Entry::Usage {
            input_tokens: outcome.usage.total_input(),
            output_tokens: outcome.usage.output_tokens,
            cost_usd: outcome.cost_usd,
            ts: chrono::Utc::now(),
        });
        session.rt.events.emit(AppEvent::TurnFinished { ok: outcome.stop == StopKind::Done });
        while let Ok(event) = rx.try_recv() {
            let _ = events.send(event);
        }
        match outcome.stop {
            StopKind::Done => Ok(StopReason::EndTurn),
            StopKind::Cancelled => Ok(StopReason::Cancelled),
            StopKind::MaxTokens => Ok(StopReason::MaxTokens),
            StopKind::Refusal(_) => Ok(StopReason::Refusal),
            other => Err(other.label()),
        }
    }

    async fn set_mode(&self, session_id: &str, mode_id: &str) -> Result<(), String> {
        let session = self.get(session_id).await?;
        let mode = PermissionMode::parse(mode_id).ok_or_else(|| format!("unknown mode `{mode_id}`"))?;
        session.rt.permissions.set_mode(mode);
        session.rt.events.emit(AppEvent::PermissionModeChanged { mode });
        Ok(())
    }

    fn modes(&self) -> Vec<(String, String)> {
        MODES.iter().map(|(id, name)| ((*id).to_owned(), (*name).to_owned())).collect()
    }
}

/// Asks the client about one tool call and hands its answer to the engine's approver.
fn answer_approval(approvals: Arc<dyn ApprovalBridge>, approver: Arc<ChannelApprover>, request: ApprovalRequest) {
    tokio::spawn(async move {
        let id = request.id.clone();
        let decision = approvals.request(request).await;
        approver.resolve(&id, decision);
    });
}

/// The user message of a prompt: text, file contents, file references and images, in order.
fn prompt_blocks(parts: &[PromptPart]) -> Vec<ContentBlock> {
    parts
        .iter()
        .map(|part| match part {
            PromptPart::Text(text) => ContentBlock::text(text.clone()),
            PromptPart::Resource { uri, text: Some(text) } => ContentBlock::text(format!("<file uri=\"{uri}\">\n{text}\n</file>")),
            PromptPart::Resource { uri, text: None } => ContentBlock::text(format!("@{uri}")),
            PromptPart::Image { mime_type, data } => ContentBlock::Image {
                source: crusty_core::message::ImageSource::Base64 { media_type: mime_type.clone(), data: data.clone() },
                extra: Default::default(),
            },
        })
        .collect()
}

/// Reads one ACP `mcpServers` entry: stdio (no `type`), or `http` / `sse` with a `url`.
/// `None` for any other shape.
fn mcp_server_config(value: &Value) -> Option<McpServerConfig> {
    match value.get("type").and_then(Value::as_str) {
        None | Some("stdio") => Some(McpServerConfig {
            command: Some(value.get("command")?.as_str()?.to_owned()),
            args: strings(value, "args"),
            env: name_values(value, "env"),
            ..Default::default()
        }),
        Some(transport @ ("http" | "sse")) => Some(McpServerConfig {
            transport: Some(transport.to_owned()),
            url: Some(value.get("url")?.as_str()?.to_owned()),
            headers: name_values(value, "headers"),
            ..Default::default()
        }),
        Some(_) => None,
    }
}

/// The string items of the array under `key`.
fn strings(value: &Value, key: &str) -> Vec<String> {
    value
        .get(key)
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect())
        .unwrap_or_default()
}

/// The `{name, value}` pairs of the array under `key`.
fn name_values(value: &Value, key: &str) -> BTreeMap<String, String> {
    let items = value.get(key).and_then(Value::as_array).into_iter().flatten();
    items.filter_map(|item| Some((item.get("name")?.as_str()?.to_owned(), item.get("value")?.as_str()?.to_owned()))).collect()
}

/// Serves ACP on stdin and stdout until the client disconnects, then stops every session.
pub async fn run(cli: &Cli) -> Result<i32> {
    let backend = Arc::new(CrustyAcp::new(cli));
    let info = AgentInfo { name: "crusty".into(), version: env!("CARGO_PKG_VERSION").into() };
    let served = serve(tokio::io::stdin(), tokio::io::stdout(), backend.clone(), info).await;
    backend.shutdown().await;
    served?;
    Ok(0)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;
    use std::time::Duration;

    use clap::Parser;
    use crusty_session::transcript;
    use serde_json::json;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines, ReadHalf, WriteHalf};

    use super::*;

    /// The offline mock provider, from the repository's config file plus a session directory
    /// under `home`, so transcripts never reach the real crusty home.
    fn offline_cli(home: &Path) -> Cli {
        let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config/offline.toml");
        let base = fs::read_to_string(base).expect("offline config");
        let sessions = home.join("sessions");
        let config = home.join("offline.toml");
        let text = format!("{base}\n[session]\ndir = '{}'\n", sessions.display());
        fs::write(&config, text).expect("write test config");
        let config = config.to_str().expect("UTF-8 path");
        Cli::try_parse_from(["crusty", "--config", config]).expect("test command line")
    }

    /// A JSON-RPC client over the duplex stream.
    struct Client {
        lines: Lines<BufReader<ReadHalf<tokio::io::DuplexStream>>>,
        writer: WriteHalf<tokio::io::DuplexStream>,
        next_id: u64,
    }

    impl Client {
        /// Sends a request and returns its answer plus every notification that came before it.
        async fn call(&mut self, method: &str, params: Value) -> (Value, Vec<Value>) {
            self.next_id += 1;
            let id = self.next_id;
            let request = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
            self.writer.write_all(format!("{request}\n").as_bytes()).await.expect("write request");
            let mut notifications = Vec::new();
            loop {
                let line = self.lines.next_line().await.expect("read").expect("server closed the stream");
                let message: Value = serde_json::from_str(&line).expect("server sent JSON");
                if message.get("id") == Some(&json!(id)) && message.get("method").is_none() {
                    return (message, notifications);
                }
                notifications.push(message);
            }
        }
    }

    #[tokio::test]
    async fn prompt_runs_a_turn_over_acp() {
        let home = tempfile::tempdir().expect("temp home");
        let backend = Arc::new(CrustyAcp::new(&offline_cli(home.path())));
        let (client_io, server_io) = tokio::io::duplex(1 << 16);
        let (server_read, server_write) = tokio::io::split(server_io);
        let info = AgentInfo { name: "crusty".into(), version: env!("CARGO_PKG_VERSION").into() };
        let server = tokio::spawn(serve(server_read, server_write, backend.clone(), info));
        let (read, writer) = tokio::io::split(client_io);
        let mut client = Client { lines: BufReader::new(read).lines(), writer, next_id: 0 };
        let dir = tempfile::tempdir().expect("temp dir");

        let outcome = tokio::time::timeout(Duration::from_secs(120), async {
            let (init, _) = client.call("initialize", json!({"protocolVersion": 1})).await;
            assert_eq!(init["result"]["protocolVersion"], json!(1));

            let cwd = dir.path().to_string_lossy().into_owned();
            let (created, _) = client.call("session/new", json!({"cwd": cwd, "mcpServers": []})).await;
            let session_id = created["result"]["sessionId"].as_str().expect("session id").to_owned();

            let prompt = json!({"sessionId": session_id, "prompt": [{"type": "text", "text": "hello"}]});
            let (done, updates) = client.call("session/prompt", prompt).await;
            assert_eq!(done["result"]["stopReason"], json!("end_turn"));
            assert!(updates.iter().any(|m| m["method"] == "session/update"), "no session/update: {updates:?}");

            // The turn is persisted where `session/load` and `--resume` read it.
            let workspace = dir.path().canonicalize().expect("canonical temp dir");
            let sessions = crusty_core::paths::sessions_dir(&workspace, Some(&home.path().join("sessions")));
            let path = transcript::find(&sessions, &session_id).expect("transcript written");
            let loaded = transcript::load(&path).expect("transcript readable");
            assert!(loaded.messages.iter().any(|m| m.text().contains("hello")), "prompt not persisted");

            // A fresh backend reopens the session from that transcript, as `session/load` does.
            let reopened = CrustyAcp::new(&offline_cli(home.path()));
            let handle = reopened.load_session(&session_id, dir.path().to_path_buf()).await.expect("session/load");
            assert_eq!(handle.id, session_id);
            let session = reopened.get(&handle.id).await.expect("loaded session is open");
            let conversation = session.conversation.lock().await;
            assert!(conversation.iter().any(|m| m.text().contains("hello")), "loaded conversation lacks the prompt");
            drop(conversation);
            reopened.shutdown().await;
        })
        .await;
        drop(client);
        let _ = server.await;
        backend.shutdown().await;
        outcome.expect("the ACP turn timed out");
    }

    #[test]
    fn mcp_entries_map_to_configs() {
        let stdio = json!({"name": "fs", "command": "node", "args": ["a.js"], "env": [{"name": "K", "value": "V"}]});
        let config = mcp_server_config(&stdio).expect("stdio");
        assert_eq!(config.command.as_deref(), Some("node"));
        assert_eq!(config.env.get("K").map(String::as_str), Some("V"));

        let http = json!({"type": "http", "name": "api", "url": "https://x", "headers": [{"name": "A", "value": "B"}]});
        let config = mcp_server_config(&http).expect("http");
        assert_eq!(config.transport.as_deref(), Some("http"));
        assert_eq!(config.headers.get("A").map(String::as_str), Some("B"));

        assert!(mcp_server_config(&json!({"name": "odd", "type": "ws", "url": "x"})).is_none());
    }
}
