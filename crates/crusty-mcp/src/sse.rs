//! Legacy MCP HTTP+SSE transport (protocol revision 2024-11-05) as an rmcp client transport.
//!
//! The client opens `GET <url>` and keeps that event stream open. The server's `endpoint` event
//! names the URL that requests are POSTed to. Each `message` event carries one JSON-RPC message
//! from the server.

use std::fmt;

use reqwest::header::{ACCEPT, HeaderMap};
use reqwest::{StatusCode, Url};
use rmcp::service::{RoleClient, RxJsonRpcMessage, TxJsonRpcMessage};
use rmcp::transport::Transport;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

/// Server messages buffered before the reader waits for the session to consume them.
const MESSAGE_BUFFER: usize = 64;

/// Errors of the legacy SSE transport.
#[derive(Debug)]
pub enum SseError {
    /// An HTTP request could not be sent or its response could not be read.
    Request(reqwest::Error),
    /// The server answered with a status that does not fit the step.
    Status { url: Url, status: StatusCode },
    /// The `endpoint` event is unusable, or names a URL on another origin.
    Endpoint(String),
    /// The event stream ended before the session became usable.
    Closed(&'static str),
}

impl fmt::Display for SseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SseError::Request(error) => write!(f, "HTTP request failed: {error}"),
            SseError::Status { url, status } => write!(f, "HTTP {status} from {url}"),
            SseError::Endpoint(message) => f.write_str(message),
            SseError::Closed(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for SseError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            SseError::Request(error) => Some(error),
            _ => None,
        }
    }
}

/// One dispatched server-sent event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseEvent {
    /// The `event` field, or `message` when absent or empty.
    pub name: String,
    /// The `data` lines joined with `\n`.
    pub data: String,
}

/// Incremental server-sent-events parser. Input may be split at any byte, including inside a
/// UTF-8 sequence, because a line is decoded only once it is complete.
#[derive(Debug, Default)]
pub struct SseParser {
    line: Vec<u8>,
    after_cr: bool,
    name: Option<String>,
    data: Vec<String>,
}

impl SseParser {
    /// Feeds raw bytes and returns the events they complete. Lines end with CRLF, LF or a bare CR.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<SseEvent> {
        let mut events = Vec::new();
        for &byte in bytes {
            match byte {
                // The LF of a CRLF pair was already handled as the line end.
                b'\n' if self.after_cr => self.after_cr = false,
                b'\r' | b'\n' => {
                    self.after_cr = byte == b'\r';
                    let line = String::from_utf8_lossy(&self.line).into_owned();
                    self.line.clear();
                    events.extend(self.handle_line(&line));
                }
                _ => {
                    self.after_cr = false;
                    self.line.push(byte);
                }
            }
        }
        events
    }

    fn handle_line(&mut self, line: &str) -> Option<SseEvent> {
        if line.is_empty() {
            let data = std::mem::take(&mut self.data);
            let name = self.name.take().filter(|name| !name.is_empty()).unwrap_or_else(|| "message".to_owned());
            return (!data.is_empty()).then(|| SseEvent { name, data: data.join("\n") });
        }
        if line.starts_with(':') {
            return None;
        }
        let (field, value) = line.split_once(':').map_or((line, ""), |(field, value)| (field, value.strip_prefix(' ').unwrap_or(value)));
        match field {
            "event" => self.name = Some(value.to_owned()),
            "data" => self.data.push(value.to_owned()),
            // `id` and `retry` do not affect this client.
            _ => {}
        }
        None
    }
}

/// Resolves the `endpoint` event's URL against the SSE URL. It must stay on the same origin.
pub fn resolve_endpoint(base: &Url, endpoint: &str) -> Result<Url, SseError> {
    let url = base.join(endpoint).map_err(|error| SseError::Endpoint(format!("invalid endpoint `{endpoint}`: {error}")))?;
    if url.origin() != base.origin() {
        return Err(SseError::Endpoint(format!("endpoint `{endpoint}` is on another origin than {base}")));
    }
    Ok(url)
}

/// Aborts the reader task when dropped, so abandoning a connect or a session closes the stream.
struct Reader(JoinHandle<()>);

impl Drop for Reader {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// rmcp client transport for the legacy HTTP+SSE protocol.
pub struct SseClientTransport {
    client: reqwest::Client,
    headers: HeaderMap,
    endpoint: Url,
    messages: mpsc::Receiver<RxJsonRpcMessage<RoleClient>>,
    reader: Reader,
}

impl SseClientTransport {
    /// Opens the event stream at `url` and waits for the `endpoint` event. The wait is unbounded
    /// here: the caller bounds it with a timeout, and dropping the future stops the reader.
    pub async fn connect(url: Url, headers: HeaderMap) -> Result<Self, SseError> {
        let client = reqwest::Client::builder().build().map_err(SseError::Request)?;
        let response =
            client.get(url.clone()).headers(headers.clone()).header(ACCEPT, "text/event-stream").send().await.map_err(SseError::Request)?;
        let status = response.status();
        if !status.is_success() {
            return Err(SseError::Status { url, status });
        }
        let (endpoint_tx, endpoint_rx) = oneshot::channel();
        let (messages_tx, messages) = mpsc::channel(MESSAGE_BUFFER);
        let reader = Reader(tokio::spawn(read_stream(response, url, endpoint_tx, messages_tx)));
        let endpoint = endpoint_rx.await.map_err(|_| SseError::Closed("event stream ended before the endpoint event"))??;
        Ok(SseClientTransport { client, headers, endpoint, messages, reader })
    }
}

/// Reads the event stream until it ends: resolves the endpoint, then forwards each message.
async fn read_stream(
    mut response: reqwest::Response,
    base: Url,
    endpoint: oneshot::Sender<Result<Url, SseError>>,
    messages: mpsc::Sender<RxJsonRpcMessage<RoleClient>>,
) {
    let mut endpoint = Some(endpoint);
    let mut parser = SseParser::default();
    loop {
        let chunk = match response.chunk().await {
            Ok(Some(chunk)) => chunk,
            Ok(None) => return,
            Err(error) => {
                tracing::debug!(%error, "SSE stream failed");
                return;
            }
        };
        for event in parser.feed(&chunk) {
            match event.name.as_str() {
                "endpoint" => {
                    if let Some(tx) = endpoint.take() {
                        let _ = tx.send(resolve_endpoint(&base, event.data.trim()));
                    }
                }
                "message" => match serde_json::from_str::<RxJsonRpcMessage<RoleClient>>(&event.data) {
                    Ok(message) => {
                        if messages.send(message).await.is_err() {
                            return;
                        }
                    }
                    Err(error) => tracing::warn!(%error, "ignoring invalid JSON-RPC message from SSE stream"),
                },
                _ => {}
            }
        }
    }
}

impl Transport<RoleClient> for SseClientTransport {
    type Error = SseError;

    fn send(&mut self, item: TxJsonRpcMessage<RoleClient>) -> impl Future<Output = Result<(), SseError>> + Send + 'static {
        let request = self.client.post(self.endpoint.clone()).headers(self.headers.clone()).json(&item);
        async move {
            let response = request.send().await.map_err(SseError::Request)?;
            let status = response.status();
            if status == StatusCode::OK || status == StatusCode::ACCEPTED {
                Ok(())
            } else {
                Err(SseError::Status { url: response.url().clone(), status })
            }
        }
    }

    async fn receive(&mut self) -> Option<RxJsonRpcMessage<RoleClient>> {
        self.messages.recv().await
    }

    async fn close(&mut self) -> Result<(), SseError> {
        self.messages.close();
        self.reader.0.abort();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::convert::Infallible;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use axum::Router;
    use axum::body::Body;
    use axum::extract::{Json, State};
    use axum::http::header;
    use axum::response::IntoResponse;
    use axum::routing::{get, post};
    use crusty_core::EventSink;
    use crusty_core::config::McpServerConfig;
    use futures::{StreamExt, stream};
    use rmcp::ServiceExt;
    use rmcp::model::CallToolRequestParams;
    use serde_json::json;

    use super::*;
    use crate::client::McpManager;

    fn event(name: &str, data: &str) -> SseEvent {
        SseEvent { name: name.to_owned(), data: data.to_owned() }
    }

    fn parse_all(bytes: &[u8]) -> Vec<SseEvent> {
        SseParser::default().feed(bytes)
    }

    #[test]
    fn parser_accepts_any_chunk_boundary() {
        let input = b"event: message\r\ndata: {\"a\":1}\r\n\r\n";
        let whole = parse_all(input);
        assert_eq!(whole, [event("message", "{\"a\":1}")]);
        let mut parser = SseParser::default();
        let mut split = Vec::new();
        for byte in input {
            split.extend(parser.feed(std::slice::from_ref(byte)));
        }
        assert_eq!(split, whole);
    }

    #[test]
    fn parser_joins_data_lines_and_skips_comments() {
        assert_eq!(parse_all(b": keep-alive\ndata: a\ndata:b\n\n"), [event("message", "a\nb")]);
    }

    #[test]
    fn parser_decodes_utf8_split_across_chunks() {
        let input = "data: héllo\n\n".as_bytes();
        let split_at = input.iter().position(|byte| *byte == 0xC3).expect("é starts with 0xC3") + 1;
        let mut parser = SseParser::default();
        let mut events = parser.feed(&input[..split_at]);
        events.extend(parser.feed(&input[split_at..]));
        assert_eq!(events, [event("message", "héllo")]);
    }

    #[test]
    fn parser_accepts_bare_cr_and_crlf_split_across_chunks() {
        assert_eq!(parse_all(b"data: a\rdata: b\r\r"), [event("message", "a\nb")]);
        let mut parser = SseParser::default();
        let mut events = parser.feed(b"data: x\r");
        events.extend(parser.feed(b"\n\r\n"));
        assert_eq!(events, [event("message", "x")]);
    }

    #[test]
    fn parser_reports_event_names_and_drops_events_without_data() {
        let events = parse_all(b"event: ping\ndata: x\n\nid: 7\nretry: 100\n\nevent: ping\n\n");
        assert_eq!(events, [event("ping", "x")]);
    }

    #[test]
    fn endpoint_resolution_keeps_same_origin_only() {
        let base = Url::parse("http://127.0.0.1:8080/sse").expect("url");
        assert_eq!(resolve_endpoint(&base, "/messages?session=1").expect("relative").as_str(), "http://127.0.0.1:8080/messages?session=1");
        assert_eq!(resolve_endpoint(&base, "messages?x=1").expect("relative").as_str(), "http://127.0.0.1:8080/messages?x=1");
        assert_eq!(resolve_endpoint(&base, "http://127.0.0.1:8080/post").expect("absolute").as_str(), "http://127.0.0.1:8080/post");
        assert!(matches!(resolve_endpoint(&base, "http://example.test/post"), Err(SseError::Endpoint(_))));
        assert!(matches!(resolve_endpoint(&base, "http://127.0.0.1:9090/post"), Err(SseError::Endpoint(_))));
        assert!(matches!(resolve_endpoint(&base, "//example.test/post"), Err(SseError::Endpoint(_))));
    }

    type Outbox = Arc<Mutex<Option<tokio::sync::mpsc::UnboundedSender<String>>>>;

    fn event_stream<S>(events: S) -> impl IntoResponse
    where
        S: futures::Stream<Item = String> + Send + 'static,
    {
        ([(header::CONTENT_TYPE, "text/event-stream")], Body::from_stream(events.map(Ok::<String, Infallible>)))
    }

    /// `GET /sse`: announces `/messages`, then pushes every reply the POST handler produces.
    async fn open_stream(State(outbox): State<Outbox>) -> impl IntoResponse {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        *outbox.lock().expect("outbox lock") = Some(tx);
        let endpoint = stream::once(async { "event: endpoint\r\ndata: /messages?session=1\r\n\r\n".to_owned() });
        let replies = stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|event| (event, rx)) });
        event_stream(endpoint.chain(replies))
    }

    /// `POST /messages`: answers requests on the open stream, ignores notifications.
    async fn answer(State(outbox): State<Outbox>, Json(request): Json<serde_json::Value>) -> axum::http::StatusCode {
        let Some(id) = request.get("id").cloned() else { return axum::http::StatusCode::ACCEPTED };
        let result = match request["method"].as_str() {
            Some("initialize") => json!({
                "protocolVersion": request["params"]["protocolVersion"],
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "legacy", "version": "0.0.0"}
            }),
            Some("tools/list") => json!({"tools": [{
                "name": "echo",
                "description": "Echoes the text argument",
                "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}}
            }]}),
            Some("tools/call") => json!({
                "content": [{"type": "text", "text": request["params"]["arguments"]["text"]}],
                "isError": false
            }),
            _ => json!({}),
        };
        let response = json!({"jsonrpc": "2.0", "id": id, "result": result});
        if let Some(tx) = outbox.lock().expect("outbox lock").as_ref() {
            // The ping checks that unknown events are skipped; CRLF checks line endings.
            let _ = tx.send(format!("event: ping\r\ndata: keep-alive\r\n\r\nevent: message\r\ndata: {response}\r\n\r\n"));
        }
        axum::http::StatusCode::ACCEPTED
    }

    /// `GET /silent`: never sends an endpoint.
    async fn silent() -> impl IntoResponse {
        event_stream(stream::pending::<String>())
    }

    /// `GET /broken-sse`: announces `/broken`, whose POST handler refuses.
    async fn broken() -> impl IntoResponse {
        event_stream(stream::once(async { "event: endpoint\ndata: /broken\n\n".to_owned() }))
    }

    async fn refuse() -> axum::http::StatusCode {
        axum::http::StatusCode::INTERNAL_SERVER_ERROR
    }

    /// Starts the legacy test server on loopback and returns its base URL.
    async fn spawn_legacy_server() -> String {
        let app = Router::new()
            .route("/sse", get(open_stream))
            .route("/messages", post(answer))
            .route("/silent", get(silent))
            .route("/broken-sse", get(broken))
            .route("/broken", post(refuse))
            .with_state(Outbox::default());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind loopback");
        let addr = listener.local_addr().expect("local address");
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        format!("http://{addr}")
    }

    fn sse_server(name: &str, url: String) -> BTreeMap<String, McpServerConfig> {
        BTreeMap::from([(name.to_owned(), McpServerConfig { transport: Some("sse".into()), url: Some(url), ..Default::default() })])
    }

    #[tokio::test]
    async fn sse_session_lists_and_calls_tools() {
        let base = spawn_legacy_server().await;
        let url = Url::parse(&format!("{base}/sse")).expect("url");
        let transport = SseClientTransport::connect(url, HeaderMap::new()).await.expect("connect");
        let service = ().serve(transport).await.expect("initialise");
        let tools = service.list_all_tools().await.expect("tools/list");
        let names: Vec<String> = tools.iter().map(|tool| tool.name.to_string()).collect();
        assert_eq!(names, ["echo"]);
        let params: CallToolRequestParams = serde_json::from_value(json!({"name": "echo", "arguments": {"text": "hi"}})).expect("params");
        let result = service.peer().call_tool(params).await.expect("tools/call");
        let result = serde_json::to_value(&result).expect("json");
        assert_eq!(result["content"][0]["text"], "hi");
        let _ = service.cancel().await;
    }

    #[tokio::test]
    async fn manager_connects_to_sse_servers_from_config() {
        let base = spawn_legacy_server().await;
        let manager =
            McpManager::connect_all(sse_server("legacy", format!("{base}/sse")), &EventSink::noop(), Duration::from_secs(10)).await;
        let status = manager.status();
        assert!(status[0].connected, "{:?}", status[0].error);
        assert_eq!((status[0].transport.as_str(), status[0].tools), ("sse", 1));
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn server_that_never_sends_endpoint_times_out() {
        let base = spawn_legacy_server().await;
        let manager =
            McpManager::connect_all(sse_server("silent", format!("{base}/silent")), &EventSink::noop(), Duration::from_secs(1)).await;
        let status = manager.status();
        assert!(!status[0].connected);
        assert!(status[0].error.as_deref().is_some_and(|error| error.contains("timed out")), "{:?}", status[0].error);
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn refused_post_is_a_transport_error() {
        let base = spawn_legacy_server().await;
        let url = Url::parse(&format!("{base}/broken-sse")).expect("url");
        let mut transport = SseClientTransport::connect(url, HeaderMap::new()).await.expect("connect");
        let notice: TxJsonRpcMessage<RoleClient> =
            serde_json::from_value(json!({"jsonrpc": "2.0", "method": "notifications/initialized"})).expect("message");
        let error = Transport::send(&mut transport, notice).await.expect_err("a 500 must fail the send");
        assert!(matches!(error, SseError::Status { status, .. } if status == StatusCode::INTERNAL_SERVER_ERROR), "{error}");
    }
}
