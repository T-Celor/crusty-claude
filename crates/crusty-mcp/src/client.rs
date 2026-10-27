//! One rmcp client session per configured MCP server, all connected concurrently.

use std::collections::{BTreeMap, HashMap};
use std::hash::Hash;
use std::process::Stdio;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use crusty_core::EventSink;
use crusty_core::config::McpServerConfig;
use crusty_core::tool::Tool;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use rmcp::ServiceExt;
use rmcp::service::{RoleClient, RunningService};
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::{StreamableHttpClientTransport, TokioChildProcess};

use crate::config::expand_server;
use crate::convert;
use crate::sse::SseClientTransport;
use crate::tool::McpTool;

/// Tool-call timeout when the server config sets none.
const DEFAULT_CALL_TIMEOUT_SECS: u64 = 60;
/// Upper bound on closing one server at shutdown.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// State of one configured server, for the UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServerStatus {
    pub name: String,
    /// `stdio`, `http`, `sse` or `invalid` (neither `command` nor `url`).
    pub transport: String,
    pub connected: bool,
    pub tools: usize,
    pub error: Option<String>,
}

struct Connection {
    name: String,
    transport: &'static str,
    tools: Vec<Arc<dyn Tool>>,
    service: Option<RunningService<RoleClient, ()>>,
    error: Option<String>,
}

impl Connection {
    fn failed(name: String, transport: &'static str, error: String) -> Self {
        Connection { name, transport, tools: Vec::new(), service: None, error: Some(error) }
    }
}

/// The live connections to every configured MCP server.
pub struct McpManager {
    connections: Vec<Connection>,
}

impl McpManager {
    /// Connects to all servers concurrently. Never fails as a whole: a server that fails or
    /// does not initialise within `timeout` is kept with its error, and a warning is emitted.
    pub async fn connect_all(servers: BTreeMap<String, McpServerConfig>, events: &EventSink, timeout: Duration) -> McpManager {
        let mut set = tokio::task::JoinSet::new();
        let mut labels = HashMap::new();
        for (name, server) in servers {
            let transport = transport_label(&server);
            let call_timeout = Duration::from_secs(server.timeout_secs.unwrap_or(DEFAULT_CALL_TIMEOUT_SECS));
            let label = name.clone();
            let handle = set.spawn(async move {
                match connect_one(&name, &server, call_timeout, timeout).await {
                    Ok((service, tools)) => Connection { name, transport, tools, service: Some(service), error: None },
                    Err(error) => Connection::failed(name, transport, error),
                }
            });
            labels.insert(handle.id(), (label, transport));
        }

        let mut connections = BTreeMap::new();
        while let Some(joined) = set.join_next().await {
            let conn = match joined {
                Ok(conn) => conn,
                Err(err) => {
                    let (name, transport) = labels.remove(&err.id()).unwrap_or_else(|| ("<unknown>".to_owned(), "invalid"));
                    Connection::failed(name, transport, format!("connection task failed: {err}"))
                }
            };
            connections.insert(conn.name.clone(), conn);
        }
        let mut connections: Vec<Connection> = connections.into_values().collect();
        for warning in drop_duplicate_tools(&mut connections) {
            events.warn(warning);
        }
        for conn in &connections {
            if let Some(error) = &conn.error {
                events.warn(format!("MCP server `{}` unavailable: {error}", conn.name));
            }
        }
        McpManager { connections }
    }

    /// Every tool of every connected server.
    pub fn tools(&self) -> Vec<Arc<dyn Tool>> {
        self.connections.iter().flat_map(|conn| conn.tools.iter().cloned()).collect()
    }

    /// One entry per configured server, sorted by name.
    pub fn status(&self) -> Vec<McpServerStatus> {
        self.connections
            .iter()
            .map(|conn| McpServerStatus {
                name: conn.name.clone(),
                transport: conn.transport.to_owned(),
                connected: conn.service.is_some(),
                tools: conn.tools.len(),
                error: conn.error.clone(),
            })
            .collect()
    }

    /// Cancels every session. Each close is bounded so a wedged server cannot block exit.
    pub async fn shutdown(self) {
        for conn in self.connections {
            if let Some(service) = conn.service {
                let _ = tokio::time::timeout(SHUTDOWN_TIMEOUT, service.cancel()).await;
            }
        }
    }
}

/// Keeps the first tool with each model-facing name, in server-name order, and drops later
/// ones. Returns one warning per dropped tool, naming both servers.
fn drop_duplicate_tools(connections: &mut [Connection]) -> Vec<String> {
    let mut owner: HashMap<String, String> = HashMap::new();
    let mut warnings = Vec::new();
    for conn in connections.iter_mut() {
        let server = conn.name.clone();
        conn.tools.retain(|tool| match owner.get(tool.name()) {
            Some(first) => {
                warnings.push(format!(
                    "MCP tool `{}` from server `{server}` clashes with a tool from `{first}` of the same name; skipped",
                    tool.name()
                ));
                false
            }
            None => {
                owner.insert(tool.name().to_owned(), server.clone());
                true
            }
        });
    }
    warnings
}

fn transport_label(server: &McpServerConfig) -> &'static str {
    if server.transport.as_deref() == Some("sse") {
        "sse"
    } else if server.command.is_some() {
        "stdio"
    } else if server.url.is_some() {
        "http"
    } else {
        "invalid"
    }
}

/// Starts one server, initialises the session and lists its tools.
/// On Windows a bare name such as `npx` is a `.cmd` shim that process creation does not find
/// by itself: look it up on PATH with the usual extensions. Elsewhere the name is used as is.
fn resolve_command(command: &str) -> std::path::PathBuf {
    let as_is = std::path::PathBuf::from(command);
    if !cfg!(windows) || as_is.extension().is_some() || as_is.components().count() > 1 {
        return as_is;
    }
    let Some(path) = std::env::var_os("PATH") else { return as_is };
    std::env::split_paths(&path)
        .flat_map(|dir| ["exe", "cmd", "bat"].map(|ext| dir.join(format!("{command}.{ext}"))))
        .find(|p| p.is_file())
        .unwrap_or(as_is)
}

async fn connect_one(
    name: &str,
    server: &McpServerConfig,
    call_timeout: Duration,
    timeout: Duration,
) -> Result<(RunningService<RoleClient, ()>, Vec<Arc<dyn Tool>>), String> {
    let server = expand_server(server, |var| std::env::var(var).ok());
    let running = match tokio::time::timeout(timeout, start_transport(&server)).await {
        Ok(running) => running?,
        Err(_) => return Err(format!("timed out after {} s", timeout.as_secs())),
    };

    // Once the session exists, every failure below must cancel it, or the child process outlives us.
    let listed = match tokio::time::timeout(timeout, running.list_all_tools()).await {
        Ok(Ok(listed)) => listed,
        Ok(Err(e)) => {
            let _ = running.cancel().await;
            return Err(format!("tools/list failed: {e}"));
        }
        Err(_) => {
            let _ = running.cancel().await;
            return Err(format!("tools/list timed out after {} s", timeout.as_secs()));
        }
    };
    let peer = running.peer().clone();
    let tools: Vec<Arc<dyn Tool>> = listed
        .iter()
        .map(|tool| {
            let spec = convert::tool_spec(name, tool);
            let mcp_tool = McpTool::new(name, tool.name.to_string(), spec, peer.clone(), call_timeout);
            Arc::new(mcp_tool) as Arc<dyn Tool>
        })
        .collect();
    tracing::debug!(server = name, tools = tools.len(), "MCP server connected");
    Ok((running, tools))
}

/// Spawns the child process or opens the HTTP session and completes the initialise handshake.
async fn start_transport(server: &McpServerConfig) -> Result<RunningService<RoleClient, ()>, String> {
    if server.transport.as_deref() == Some("sse") {
        let raw = server.url.as_deref().ok_or("`sse` transport needs a `url`")?;
        let url = reqwest::Url::parse(raw).map_err(|e| format!("invalid url `{raw}`: {e}"))?;
        let headers: HeaderMap = collect_headers::<HeaderName, HeaderValue>(&server.headers)?.into_iter().collect();
        let transport = SseClientTransport::connect(url, headers).await.map_err(|e| format!("cannot open SSE stream: {e}"))?;
        return ().serve(transport).await.map_err(|e| format!("initialise failed: {e}"));
    }
    match (server.command.as_deref(), server.url.as_deref()) {
        (Some(command), _) => {
            let mut cmd = tokio::process::Command::new(resolve_command(command));
            cmd.args(&server.args).envs(&server.env);
            // The child's stderr would corrupt the TUI, so it is discarded.
            let (process, _stderr) =
                TokioChildProcess::builder(cmd).stderr(Stdio::null()).spawn().map_err(|e| format!("cannot start `{command}`: {e}"))?;
            ().serve(process).await.map_err(|e| format!("initialise failed: {e}"))
        }
        (None, Some(url)) => {
            let headers = collect_headers(&server.headers)?;
            let config = StreamableHttpClientTransportConfig::with_uri(url).custom_headers(headers);
            ().serve(StreamableHttpClientTransport::from_config(config)).await.map_err(|e| format!("initialise failed: {e}"))
        }
        (None, None) => Err("no `command` or `url` configured".to_owned()),
    }
}

/// Parses configured headers into the transport's header types. rmcp does not re-export
/// those types, so they are inferred from the transport's parameter type.
fn collect_headers<K, V>(headers: &BTreeMap<String, String>) -> Result<HashMap<K, V>, String>
where
    K: FromStr + Eq + Hash,
    K::Err: std::fmt::Display,
    V: FromStr,
    V::Err: std::fmt::Display,
{
    headers
        .iter()
        .map(|(key, value)| {
            let k = key.parse::<K>().map_err(|e| format!("invalid header name `{key}`: {e}"))?;
            let v = value.parse::<V>().map_err(|e| format!("invalid value for header `{key}`: {e}"))?;
            Ok::<(K, V), String>((k, v))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colliding_model_names_are_detected_from_server_and_tool_names() {
        // Both pairs sanitise to the same `mcp__<server>__<tool>`, so the second must be dropped.
        assert_eq!(convert::tool_name("a__b", "c"), convert::tool_name("a", "b__c"));
        assert_eq!(convert::tool_name("git hub", "x"), convert::tool_name("git_hub", "x"));
    }

    #[tokio::test]
    async fn failed_server_is_recorded_not_fatal() {
        let mut servers = BTreeMap::new();
        servers
            .insert("broken".to_owned(), McpServerConfig { command: Some("crusty-mcp-no-such-binary-xyz".into()), ..Default::default() });
        servers.insert("empty".to_owned(), McpServerConfig::default());
        let manager = McpManager::connect_all(servers, &EventSink::noop(), Duration::from_secs(10)).await;
        let status = manager.status();
        assert_eq!(status.len(), 2);
        assert_eq!(status[0].name, "broken");
        assert_eq!(status[0].transport, "stdio");
        assert!(!status[0].connected);
        assert!(status[0].error.is_some());
        assert_eq!(status[1].transport, "invalid");
        assert!(manager.tools().is_empty());
        manager.shutdown().await;
    }
}
