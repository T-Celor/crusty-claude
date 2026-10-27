//! Protocol tests: a scripted fake backend behind `serve`, over an in-memory duplex stream.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use crusty_core::AppEvent;
use crusty_core::event::{TodoItem, TodoStatus};
use crusty_core::permission::{ApprovalDecision, ApprovalRequest, RiskLevel};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines, ReadHalf, WriteHalf};
use tokio::sync::Barrier;
use tokio::sync::mpsc::UnboundedSender;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

use crate::{AcpBackend, AgentInfo, ApprovalBridge, McpServerSpec, PromptPart, SessionHandle, StopReason, serve};

/// Plays a script chosen by the text of the prompt.
struct FakeBackend {
    counter: AtomicU64,
    decisions: Mutex<Vec<ApprovalDecision>>,
    /// Two prompts wait here, so they only finish if they run concurrently.
    barrier: Barrier,
}

fn fake() -> (Arc<FakeBackend>, Arc<dyn AcpBackend>) {
    let fake = Arc::new(FakeBackend { counter: AtomicU64::new(0), decisions: Mutex::new(Vec::new()), barrier: Barrier::new(2) });
    let backend: Arc<dyn AcpBackend> = fake.clone();
    (fake, backend)
}

#[async_trait]
impl AcpBackend for FakeBackend {
    fn can_load_session(&self) -> bool {
        false
    }

    async fn new_session(&self, _cwd: PathBuf, _mcp_servers: Vec<McpServerSpec>) -> Result<SessionHandle, String> {
        let n = self.counter.fetch_add(1, Ordering::Relaxed);
        Ok(SessionHandle { id: format!("s{n}") })
    }

    async fn prompt(
        &self,
        _session: &str,
        prompt: Vec<PromptPart>,
        events: UnboundedSender<AppEvent>,
        approvals: Arc<dyn ApprovalBridge>,
        cancel: CancellationToken,
    ) -> Result<StopReason, String> {
        let script = match prompt.first() {
            Some(PromptPart::Text(text)) => text.clone(),
            _ => return Err("no text part".to_string()),
        };
        let agent = "main".to_string();
        match script.as_str() {
            "stream" => {
                let plan = vec![
                    TodoItem { content: "Read code".into(), status: TodoStatus::Completed },
                    TodoItem { content: "Fix".into(), status: TodoStatus::InProgress },
                ];
                let script_events = [
                    AppEvent::TextDelta { agent_id: agent.clone(), text: "Hel".into() },
                    AppEvent::TextDelta { agent_id: agent.clone(), text: "lo".into() },
                    AppEvent::ThinkingDelta { agent_id: agent.clone(), text: "hmm".into() },
                    AppEvent::ToolStarted {
                        agent_id: agent.clone(),
                        call_id: "c1".into(),
                        tool: "Read".into(),
                        summary: "Read(a.rs)".into(),
                    },
                    AppEvent::ToolFinished {
                        agent_id: agent.clone(),
                        call_id: "c1".into(),
                        tool: "Read".into(),
                        ok: true,
                        preview: "fn main() {}".into(),
                        duration_ms: 3,
                        exit_code: None,
                    },
                    AppEvent::PlanUpdated { agent_id: agent.clone(), items: plan },
                    AppEvent::TextDelta { agent_id: agent.clone(), text: "done".into() },
                ];
                for event in script_events {
                    let _ = events.send(event);
                }
                Ok(StopReason::EndTurn)
            }
            "permission" => {
                let request = ApprovalRequest {
                    id: "ap1".into(),
                    agent_id: agent,
                    tool: "Bash".into(),
                    summary: "Bash(ls)".into(),
                    risk: RiskLevel::Moderate,
                    reason: "test".into(),
                    suggested_rule: None,
                    preview: "ls".into(),
                };
                let decision = approvals.request(request).await;
                self.decisions.lock().unwrap_or_else(|p| p.into_inner()).push(decision);
                Ok(StopReason::EndTurn)
            }
            "wait" => {
                let _ = events.send(AppEvent::TextDelta { agent_id: agent, text: "working".into() });
                cancel.cancelled().await;
                Ok(StopReason::EndTurn)
            }
            "barrier" => {
                self.barrier.wait().await;
                Ok(StopReason::EndTurn)
            }
            other => Err(format!("unknown script {other}")),
        }
    }

    async fn set_mode(&self, _session: &str, _mode_id: &str) -> Result<(), String> {
        Ok(())
    }

    fn modes(&self) -> Vec<(String, String)> {
        vec![("default".into(), "Default".into()), ("plan".into(), "Plan".into())]
    }
}

/// The client end of a served connection.
struct Client {
    writer: WriteHalf<tokio::io::DuplexStream>,
    reader: Lines<BufReader<ReadHalf<tokio::io::DuplexStream>>>,
}

impl Client {
    fn start(backend: Arc<dyn AcpBackend>) -> Self {
        let (client_end, server_end) = tokio::io::duplex(1 << 16);
        let (server_read, server_write) = tokio::io::split(server_end);
        let info = AgentInfo { name: "crusty".into(), version: "0.0.0".into() };
        tokio::spawn(async move {
            let _ = serve(server_read, server_write, backend, info).await;
        });
        let (client_read, client_write) = tokio::io::split(client_end);
        Client { writer: client_write, reader: BufReader::new(client_read).lines() }
    }

    async fn send(&mut self, message: Value) {
        let line = format!("{message}\n");
        self.writer.write_all(line.as_bytes()).await.unwrap();
    }

    async fn next(&mut self) -> Value {
        let line = timeout(Duration::from_secs(5), self.reader.next_line())
            .await
            .expect("timed out waiting for the server")
            .unwrap()
            .expect("server closed the stream");
        serde_json::from_str(&line).unwrap()
    }

    /// Reads until the answer to `id`. Permission prompts get `answer` as the chosen option;
    /// every `session/update` seen on the way is returned.
    async fn await_response(&mut self, id: u64, answer: &str) -> (Value, Vec<Value>) {
        let mut updates = Vec::new();
        loop {
            let message = self.next().await;
            match message.get("method").and_then(Value::as_str) {
                Some("session/update") => updates.push(message["params"]["update"].clone()),
                Some("session/request_permission") => {
                    let reply = json!({
                        "jsonrpc": "2.0",
                        "id": message["id"],
                        "result": {"outcome": {"outcome": "selected", "optionId": answer}},
                    });
                    self.send(reply).await;
                }
                None if message["id"] == id => return (message, updates),
                _ => {}
            }
        }
    }

    async fn new_session(&mut self, id: u64) -> String {
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": "session/new",
            "params": {"cwd": "/work", "mcpServers": []}}))
            .await;
        let (created, _) = self.await_response(id, "").await;
        created["result"]["sessionId"].as_str().unwrap().to_string()
    }

    async fn prompt(&mut self, id: u64, session: &str, text: &str) {
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": "session/prompt",
            "params": {"sessionId": session, "prompt": [{"type": "text", "text": text}]}}))
            .await;
    }
}

#[tokio::test]
async fn initialize_negotiates_the_lower_protocol_version() {
    let (_, backend) = fake();
    let mut client = Client::start(backend);
    client.send(json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": 99}})).await;
    let (reply, _) = client.await_response(1, "").await;
    assert_eq!(reply["result"]["protocolVersion"], 1);
    assert_eq!(reply["result"]["agentInfo"]["name"], "crusty");
    assert_eq!(reply["result"]["agentCapabilities"]["loadSession"], false);
    assert_eq!(reply["result"]["agentCapabilities"]["promptCapabilities"]["image"], true);
    assert_eq!(reply["result"]["agentCapabilities"]["promptCapabilities"]["embeddedContext"], true);
}

#[tokio::test]
async fn prompt_streams_updates_in_order_then_stops() {
    let (_, backend) = fake();
    let mut client = Client::start(backend);
    let session = client.new_session(1).await;

    client.prompt(2, &session, "stream").await;
    let (done, updates) = client.await_response(2, "").await;
    assert_eq!(done["result"]["stopReason"], "end_turn");

    let kinds: Vec<&str> = updates.iter().map(|u| u["sessionUpdate"].as_str().unwrap()).collect();
    assert_eq!(
        kinds,
        [
            "agent_message_chunk",
            "agent_message_chunk",
            "agent_thought_chunk",
            "tool_call",
            "tool_call_update",
            "plan",
            "agent_message_chunk",
        ]
    );
    assert_eq!(updates[0]["content"]["text"], "Hel");
    assert_eq!(updates[3]["kind"], "read");
    assert_eq!(updates[3]["status"], "in_progress");
    assert_eq!(updates[4]["status"], "completed");
    assert_eq!(updates[5]["entries"][1]["status"], "in_progress");
    assert_eq!(updates[6]["content"]["text"], "done");
}

#[tokio::test]
async fn permission_answers_reach_the_backend() {
    let (fake, backend) = fake();
    let mut client = Client::start(backend);
    let session = client.new_session(1).await;

    client.prompt(2, &session, "permission").await;
    let (done, _) = client.await_response(2, "allow_once").await;
    assert_eq!(done["result"]["stopReason"], "end_turn");

    client.prompt(3, &session, "permission").await;
    let (done, _) = client.await_response(3, "reject_once").await;
    assert_eq!(done["result"]["stopReason"], "end_turn");

    let decisions = fake.decisions.lock().unwrap().clone();
    assert_eq!(decisions, vec![ApprovalDecision::AllowOnce, ApprovalDecision::Deny]);
}

#[tokio::test]
async fn cancel_ends_the_running_prompt_as_cancelled() {
    let (_, backend) = fake();
    let mut client = Client::start(backend);
    let session = client.new_session(1).await;

    client.prompt(2, &session, "wait").await;
    loop {
        if client.next().await["method"] == "session/update" {
            break;
        }
    }
    client.send(json!({"jsonrpc": "2.0", "method": "session/cancel", "params": {"sessionId": session}})).await;
    let (done, _) = client.await_response(2, "").await;
    assert_eq!(done["result"]["stopReason"], "cancelled");
}

#[tokio::test]
async fn unknown_method_gets_method_not_found() {
    let (_, backend) = fake();
    let mut client = Client::start(backend);
    client.send(json!({"jsonrpc": "2.0", "id": 5, "method": "fs/unknown", "params": {}})).await;
    let (reply, _) = client.await_response(5, "").await;
    assert_eq!(reply["error"]["code"], -32601);
}

#[tokio::test]
async fn malformed_line_gets_parse_error_and_session_survives() {
    let (_, backend) = fake();
    let mut client = Client::start(backend);
    client.writer.write_all(b"{not json\n").await.unwrap();
    let reply = client.next().await;
    assert_eq!(reply["error"]["code"], -32700);
    assert!(reply["id"].is_null());

    client.send(json!({"jsonrpc": "2.0", "id": 6, "method": "authenticate", "params": {}})).await;
    let (ok, _) = client.await_response(6, "").await;
    assert!(ok.get("error").is_none());
}

#[tokio::test]
async fn prompt_on_unknown_session_is_invalid_params() {
    let (_, backend) = fake();
    let mut client = Client::start(backend);
    client.prompt(7, "nope", "stream").await;
    let (reply, _) = client.await_response(7, "").await;
    assert_eq!(reply["error"]["code"], -32602);
}

#[tokio::test]
async fn two_sessions_prompt_concurrently() {
    let (_, backend) = fake();
    let mut client = Client::start(backend);
    let first = client.new_session(1).await;
    let second = client.new_session(2).await;

    client.prompt(3, &first, "barrier").await;
    client.prompt(4, &second, "barrier").await;

    let mut answered = Vec::new();
    while answered.len() < 2 {
        let message = client.next().await;
        if message.get("result").is_some() {
            answered.push(message["id"].as_u64().unwrap());
        }
    }
    answered.sort_unstable();
    assert_eq!(answered, vec![3, 4]);
}
