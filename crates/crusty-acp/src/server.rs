//! The ACP server: newline-delimited JSON-RPC 2.0 over any async byte stream.

use std::collections::HashMap;
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use async_trait::async_trait;
use crusty_core::AppEvent;
use crusty_core::permission::{ApprovalDecision, ApprovalRequest};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::backend::{AcpBackend, ApprovalBridge, McpServerSpec, PromptPart, StopReason};
use crate::protocol;

/// The ACP protocol version this agent speaks. `initialize` answers with the lower of this
/// and the client's version.
pub const PROTOCOL_VERSION: u64 = 1;

/// Name and version the agent reports in `initialize`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentInfo {
    /// Program name, e.g. `crusty-claude`.
    pub name: String,
    /// Program version.
    pub version: String,
}

/// State shared by the reader, the writer and the per-request tasks.
struct Shared {
    backend: Arc<dyn AcpBackend>,
    info: AgentInfo,
    /// Finished lines (with the trailing newline) for the single writer task.
    out: mpsc::UnboundedSender<String>,
    /// Answers we are waiting for, keyed by the id of our request to the client.
    pending: Mutex<HashMap<u64, oneshot::Sender<Value>>>,
    next_id: AtomicU64,
    /// Known sessions, each with the cancellation token of its running prompt.
    sessions: Mutex<HashMap<String, Option<CancellationToken>>>,
}

impl Shared {
    fn send(&self, message: Value) {
        // A closed writer means the client is gone; there is nobody left to tell.
        let _ = self.out.send(format!("{message}\n"));
    }
}

/// Locks a std mutex; a poisoned lock still holds consistent maps, so it is reused.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// A JSON-RPC error answer.
struct RpcError {
    code: i64,
    message: String,
}

impl RpcError {
    fn invalid(message: impl Into<String>) -> Self {
        Self { code: protocol::INVALID_PARAMS, message: message.into() }
    }

    fn internal(message: String) -> Self {
        Self { code: protocol::INTERNAL_ERROR, message }
    }
}

type RpcResult = Result<Value, RpcError>;

/// Serves ACP on `reader`/`writer` until the reader reaches end of input.
///
/// Requests run concurrently: each `session/prompt` is a task of its own, so a cancel or a
/// permission answer can arrive while a turn is running. Output lines are written by one
/// task, so messages never interleave.
pub async fn serve<R, W>(reader: R, mut writer: W, backend: Arc<dyn AcpBackend>, info: AgentInfo) -> io::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin + Send,
{
    let (out, mut lines_out) = mpsc::unbounded_channel::<String>();
    let shared = Arc::new(Shared {
        backend,
        info,
        out,
        pending: Mutex::new(HashMap::new()),
        next_id: AtomicU64::new(0),
        sessions: Mutex::new(HashMap::new()),
    });

    let write_loop = async {
        while let Some(line) = lines_out.recv().await {
            writer.write_all(line.as_bytes()).await?;
            writer.flush().await?;
        }
        Ok::<(), io::Error>(())
    };

    let read_loop = async {
        let mut lines = BufReader::new(reader).lines();
        while let Some(line) = lines.next_line().await? {
            if !line.trim().is_empty() {
                dispatch_line(&shared, &line).await;
            }
        }
        Ok::<(), io::Error>(())
    };

    // End of input ends the session; a failed write ends it with that error.
    tokio::select! {
        result = read_loop => result,
        result = write_loop => result,
    }
}

async fn dispatch_line(shared: &Arc<Shared>, line: &str) {
    let message: Value = match serde_json::from_str(line) {
        Ok(value) => value,
        Err(err) => {
            shared.send(protocol::error(&Value::Null, protocol::PARSE_ERROR, &format!("parse error: {err}")));
            return;
        }
    };
    let Some(method) = message.get("method").and_then(Value::as_str) else {
        // An answer to a request the agent sent (a permission prompt) goes to its waiter.
        if message.get("result").is_some() || message.get("error").is_some() {
            if let Some(id) = message.get("id").and_then(Value::as_u64) {
                let waiting = lock(&shared.pending).remove(&id);
                if let Some(tx) = waiting {
                    let _ = tx.send(message.get("result").cloned().unwrap_or(Value::Null));
                }
            }
        } else {
            let id = message.get("id").unwrap_or(&Value::Null);
            shared.send(protocol::error(id, protocol::INVALID_REQUEST, "missing method"));
        }
        return;
    };

    let params = message.get("params").cloned().unwrap_or(Value::Null);
    if method == "session/cancel" {
        cancel_session(shared, &params);
        return;
    }
    let Some(id) = message.get("id").cloned() else {
        // Notifications get no answer.
        return;
    };

    let result = match method {
        "initialize" => initialize(shared, &params),
        "authenticate" => Ok(json!({})),
        "session/new" => new_session(shared, &params).await,
        "session/load" => load_session(shared, &params).await,
        "session/set_mode" => set_mode(shared, &params).await,
        "session/prompt" => {
            tokio::spawn(run_prompt(shared.clone(), id, params));
            return;
        }
        _ => Err(RpcError { code: protocol::METHOD_NOT_FOUND, message: format!("method not found: {method}") }),
    };
    shared.send(match result {
        Ok(value) => protocol::response(&id, value),
        Err(err) => protocol::error(&id, err.code, &err.message),
    });
}

fn str_param<'a>(params: &'a Value, key: &str) -> Result<&'a str, RpcError> {
    params.get(key).and_then(Value::as_str).ok_or_else(|| RpcError::invalid(format!("missing string param `{key}`")))
}

fn initialize(shared: &Shared, params: &Value) -> RpcResult {
    let client = params
        .get("protocolVersion")
        .and_then(Value::as_u64)
        .ok_or_else(|| RpcError::invalid("missing integer param `protocolVersion`"))?;
    Ok(json!({
        "protocolVersion": client.min(PROTOCOL_VERSION),
        "agentCapabilities": {
            "loadSession": shared.backend.can_load_session(),
            "promptCapabilities": {"image": true, "embeddedContext": true},
        },
        "authMethods": [],
        "agentInfo": {"name": shared.info.name, "version": shared.info.version},
    }))
}

/// The `sessionId` answer, with `modes` when the backend has any.
fn session_result(backend: &dyn AcpBackend, session: &str) -> Value {
    let mut result = json!({"sessionId": session});
    let modes = backend.modes();
    if let Some((current, _)) = modes.first() {
        result["modes"] = json!({
            "currentModeId": current,
            "availableModes": modes
                .iter()
                .map(|(id, name)| json!({"id": id, "name": name}))
                .collect::<Vec<_>>(),
        });
    }
    result
}

fn mcp_servers(params: &Value) -> Result<Vec<McpServerSpec>, RpcError> {
    let Some(list) = params.get("mcpServers") else {
        return Ok(Vec::new());
    };
    let list = list.as_array().ok_or_else(|| RpcError::invalid("`mcpServers` must be an array"))?;
    list.iter()
        .map(|server| -> Result<McpServerSpec, RpcError> {
            let name = server.get("name").and_then(Value::as_str).ok_or_else(|| RpcError::invalid("MCP server without `name`"))?;
            Ok(McpServerSpec { name: name.to_string(), config: server.clone() })
        })
        .collect()
}

fn register(shared: &Shared, session: &str) {
    lock(&shared.sessions).insert(session.to_string(), None);
}

fn ensure_session(shared: &Shared, session: &str) -> Result<(), RpcError> {
    if lock(&shared.sessions).contains_key(session) { Ok(()) } else { Err(RpcError::invalid(format!("unknown session `{session}`"))) }
}

async fn new_session(shared: &Arc<Shared>, params: &Value) -> RpcResult {
    let cwd = PathBuf::from(str_param(params, "cwd")?);
    let servers = mcp_servers(params)?;
    let handle = shared.backend.new_session(cwd, servers).await.map_err(RpcError::internal)?;
    register(shared, &handle.id);
    Ok(session_result(shared.backend.as_ref(), &handle.id))
}

async fn load_session(shared: &Arc<Shared>, params: &Value) -> RpcResult {
    let id = str_param(params, "sessionId")?;
    let cwd = PathBuf::from(str_param(params, "cwd")?);
    let handle = shared.backend.load_session(id, cwd).await.map_err(RpcError::internal)?;
    register(shared, &handle.id);
    Ok(session_result(shared.backend.as_ref(), &handle.id))
}

async fn set_mode(shared: &Arc<Shared>, params: &Value) -> RpcResult {
    let session = str_param(params, "sessionId")?;
    let mode = str_param(params, "modeId")?;
    ensure_session(shared, session)?;
    shared.backend.set_mode(session, mode).await.map_err(RpcError::internal)?;
    Ok(json!({}))
}

fn cancel_session(shared: &Shared, params: &Value) {
    let Some(session) = params.get("sessionId").and_then(Value::as_str) else {
        return;
    };
    let token = lock(&shared.sessions).get(session).and_then(|slot| slot.clone());
    if let Some(token) = token {
        token.cancel();
    }
}

fn prompt_part(block: &Value) -> Result<PromptPart, RpcError> {
    match block.get("type").and_then(Value::as_str) {
        Some("text") => Ok(PromptPart::Text(str_param(block, "text")?.to_string())),
        Some("resource_link") => Ok(PromptPart::Resource { uri: str_param(block, "uri")?.to_string(), text: None }),
        Some("resource") => {
            let resource = block.get("resource").ok_or_else(|| RpcError::invalid("`resource` block without `resource`"))?;
            Ok(PromptPart::Resource {
                uri: str_param(resource, "uri")?.to_string(),
                text: resource.get("text").and_then(Value::as_str).map(str::to_string),
            })
        }
        Some("image") => {
            Ok(PromptPart::Image { mime_type: str_param(block, "mimeType")?.to_string(), data: str_param(block, "data")?.to_string() })
        }
        other => Err(RpcError::invalid(format!("unsupported prompt block {other:?}"))),
    }
}

fn prompt_parts(params: &Value) -> Result<Vec<PromptPart>, RpcError> {
    let blocks = params.get("prompt").and_then(Value::as_array).ok_or_else(|| RpcError::invalid("missing array param `prompt`"))?;
    blocks.iter().map(prompt_part).collect()
}

async fn run_prompt(shared: Arc<Shared>, id: Value, params: Value) {
    let reply = match prompt(&shared, &params).await {
        Ok(result) => protocol::response(&id, result),
        Err(err) => protocol::error(&id, err.code, &err.message),
    };
    shared.send(reply);
}

/// Runs one turn, forwarding its events as they come, and answers with the stop reason.
async fn prompt(shared: &Arc<Shared>, params: &Value) -> RpcResult {
    let session = str_param(params, "sessionId")?.to_string();
    let parts = prompt_parts(params)?;

    let cancel = CancellationToken::new();
    {
        let mut sessions = lock(&shared.sessions);
        let Some(slot) = sessions.get_mut(&session) else {
            return Err(RpcError::invalid(format!("unknown session `{session}`")));
        };
        *slot = Some(cancel.clone());
    }

    let (events_tx, mut events_rx) = mpsc::unbounded_channel::<AppEvent>();
    let approvals: Arc<dyn ApprovalBridge> =
        Arc::new(ServerApprovals { shared: shared.clone(), session: session.clone(), cancel: cancel.clone() });

    let outcome = {
        let run = shared.backend.prompt(&session, parts, events_tx, approvals, cancel.clone());
        tokio::pin!(run);
        loop {
            tokio::select! {
                result = &mut run => break result,
                Some(event) = events_rx.recv() => forward(shared, &session, &event),
            }
        }
    };
    // The turn is over: forward whatever it queued before the answer.
    events_rx.close();
    while let Ok(event) = events_rx.try_recv() {
        forward(shared, &session, &event);
    }
    if let Some(slot) = lock(&shared.sessions).get_mut(&session) {
        *slot = None;
    }

    let stop = if cancel.is_cancelled() { StopReason::Cancelled } else { outcome.map_err(RpcError::internal)? };
    Ok(json!({"stopReason": stop.as_str()}))
}

fn forward(shared: &Shared, session: &str, event: &AppEvent) {
    if let Some(update) = protocol::update_for(event) {
        shared.send(protocol::session_update(session, update));
    }
}

/// Maps the client's answer to `session/request_permission` to a decision.
fn decision_from(result: &Value) -> ApprovalDecision {
    if result.pointer("/outcome/outcome").and_then(Value::as_str) != Some("selected") {
        return ApprovalDecision::Deny;
    }
    match result.pointer("/outcome/optionId").and_then(Value::as_str) {
        Some("allow_once") => ApprovalDecision::AllowOnce,
        Some("allow_always") => ApprovalDecision::AllowAlways,
        _ => ApprovalDecision::Deny,
    }
}

/// Approval bridge of one prompt: asks the client through `session/request_permission`.
struct ServerApprovals {
    shared: Arc<Shared>,
    session: String,
    cancel: CancellationToken,
}

#[async_trait]
impl ApprovalBridge for ServerApprovals {
    async fn request(&self, request: ApprovalRequest) -> ApprovalDecision {
        let (tx, rx) = oneshot::channel();
        let id = self.shared.next_id.fetch_add(1, Ordering::Relaxed);
        lock(&self.shared.pending).insert(id, tx);

        self.shared.send(protocol::session_update(&self.session, protocol::tool_started(&request.id, &request.tool, &request.summary)));
        self.shared.send(protocol::request(
            id,
            "session/request_permission",
            protocol::permission_params(&self.session, &request.id, &request.summary, &request.tool),
        ));

        let answer = tokio::select! {
            reply = rx => reply.ok(),
            _ = self.cancel.cancelled() => None,
        };
        lock(&self.shared.pending).remove(&id);
        answer.map_or(ApprovalDecision::Deny, |result| decision_from(&result))
    }
}
