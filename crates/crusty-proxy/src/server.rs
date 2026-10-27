//! The built-in optimizing proxy: an Anthropic-compatible HTTP endpoint. `POST /v1/messages`
//! runs through the optimizer; every other request is forwarded unchanged.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::time::Instant;

use axum::Json;
use axum::Router;
use axum::body::{Body, Bytes, to_bytes};
use axum::extract::{Request, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use crusty_core::{
    AppEvent, CancellationToken, ChatRequest, Config, EventSink, OptimizeContext, OptimizeOutcome, ProviderKind, RequestOptimizer, Route,
    Usage,
};
use futures::StreamExt;
use serde_json::{Value, json};
use tokio::net::TcpListener;

use crate::usage::UsageWatcher;

/// Largest request body read into memory. The optimizer needs the whole request.
const MAX_BODY: usize = 32 * 1024 * 1024;

/// Request headers never forwarded: they describe the client's connection, or the
/// proxy recomputes them (`accept-encoding` because reqwest decompresses the answer).
const REQUEST_SKIP: [&str; 10] = [
    "host",
    "connection",
    "keep-alive",
    "proxy-connection",
    "transfer-encoding",
    "content-length",
    "te",
    "trailer",
    "upgrade",
    "accept-encoding",
];

/// Response headers not copied back: the body is re-framed and already decompressed.
const RESPONSE_SKIP: [&str; 5] = ["connection", "keep-alive", "transfer-encoding", "content-length", "content-encoding"];

/// Everything the proxy routes need. Shared by all connections.
pub struct ProxyState {
    /// Rewrites each Messages request and picks its route.
    pub optimizer: Arc<dyn RequestOptimizer>,
    /// Model windows and prices.
    pub config: Arc<Config>,
    /// Where usage and metrics are reported.
    pub events: EventSink,
    /// Client used for every upstream call.
    pub http: reqwest::Client,
    /// Base URL of direct requests, e.g. `https://api.anthropic.com`.
    pub forward_url: String,
    /// Start time, for the uptime in `/stats`.
    pub started: Instant,
}

/// Builds the proxy's HTTP router.
pub fn router(state: ProxyState) -> Router {
    Router::new().fallback(dispatch).with_state(Arc::new(state))
}

/// Serves `router(state)` on `listener` until `shutdown` is cancelled.
pub async fn serve(listener: TcpListener, state: ProxyState, shutdown: CancellationToken) -> std::io::Result<()> {
    axum::serve(listener, router(state)).with_graceful_shutdown(async move { shutdown.cancelled().await }).await
}

/// Binds the listener. Warns when the address is not loopback, because the proxy forwards
/// the client's own API key and has no authentication of its own.
pub async fn bind(addr: &str) -> std::io::Result<TcpListener> {
    let listener = TcpListener::bind(addr).await?;
    let local = listener.local_addr()?;
    if !local.ip().is_loopback() {
        tracing::warn!(address = %local, "crusty proxy listens beyond loopback and has no authentication");
    }
    Ok(listener)
}

async fn dispatch(State(state): State<Arc<ProxyState>>, req: Request) -> Response {
    let (parts, body) = req.into_parts();
    let path = parts.uri.path();
    let get = parts.method == Method::GET;
    if get && (path == "/" || path == "/livez") {
        return Json(json!({"ok": true})).into_response();
    }
    if get && path == "/stats" {
        let uptime_secs = state.started.elapsed().as_secs();
        return Json(json!({"uptime_secs": uptime_secs, "metrics": state.events.metrics().snapshot()})).into_response();
    }
    if parts.method == Method::POST && path == "/v1/messages" {
        return messages(&state, parts, body).await;
    }
    match read_body(body).await {
        Ok(raw) => direct(&state, &parts, raw, None).await,
        Err(res) => *res,
    }
}

/// Optimized `POST /v1/messages`. An unparseable body is forwarded as it arrived.
async fn messages(state: &ProxyState, mut parts: Parts, body: Body) -> Response {
    let raw = match read_body(body).await {
        Ok(raw) => raw,
        Err(res) => return *res,
    };
    let Ok(mut req) = serde_json::from_slice::<ChatRequest>(&raw) else {
        tracing::debug!("body is not a Messages request; forwarded unchanged");
        return direct(state, &parts, raw, None).await;
    };
    let ctx = context(state, &req);
    let mut outcome = state.optimizer.optimize(&mut req, &ctx).await;
    if req.has_block_binding() {
        add_beta(&mut parts.headers, crusty_core::message::THINKING_BINDING_BETA);
    }
    let body = match serde_json::to_vec(&req) {
        Ok(v) => Bytes::from(v),
        Err(e) => {
            tracing::warn!(error = %e, "cannot serialize the optimized request; forwarding the original");
            raw
        }
    };

    let via = match &outcome.route {
        Route::Via { name, base_url } => Some((name.clone(), base_url.clone())),
        Route::Direct => None,
    };
    if let Some((name, base_url)) = via {
        match forward(state, &base_url, &parts, body.clone()).await {
            Ok(resp) => return relay(resp, Some(recorder(state, ctx, outcome))),
            Err(e) if e.is_connect() => {
                tracing::warn!(route = %name, error = %e, "via route unreachable; sending the request direct");
                state.optimizer.record_route_failure(&ctx, &outcome);
                outcome.route = Route::Direct;
            }
            Err(e) => return bad_gateway(&e),
        }
    }
    direct(state, &parts, body, Some(recorder(state, ctx, outcome))).await
}

/// Adds `beta` to the `anthropic-beta` header, keeping the client's values.
fn add_beta(headers: &mut HeaderMap, beta: &str) {
    let existing: Vec<String> = headers
        .get_all("anthropic-beta")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty())
        .collect();
    if existing.iter().any(|v| v == beta) {
        return;
    }
    let mut all = existing;
    all.push(beta.to_owned());
    if let Ok(value) = all.join(",").parse() {
        headers.insert("anthropic-beta", value);
    }
}

/// Sends `body` to the direct upstream, reporting usage when `recorder` is given.
async fn direct(state: &ProxyState, parts: &Parts, body: Bytes, recorder: Option<Recorder>) -> Response {
    match forward(state, &state.forward_url, parts, body).await {
        Ok(resp) => relay(resp, recorder),
        Err(e) => bad_gateway(&e),
    }
}

/// Sends the request to `base` with the client's method, path, query and headers.
async fn forward(state: &ProxyState, base: &str, parts: &Parts, body: Bytes) -> reqwest::Result<reqwest::Response> {
    let url = join_url(base, parts.uri.path(), parts.uri.query());
    state.http.request(parts.method.clone(), url).headers(filter_headers(&parts.headers, &REQUEST_SKIP)).body(body).send().await
}

/// `base` + `path` + `?query`. A base ending in `/v1` does not repeat the `/v1` of the path.
fn join_url(base: &str, path: &str, query: Option<&str>) -> String {
    let base = base.trim_end_matches('/');
    let path = if base.ends_with("/v1") {
        path.strip_prefix("/v1").filter(|rest| rest.is_empty() || rest.starts_with('/')).unwrap_or(path)
    } else {
        path
    };
    match query {
        Some(q) => format!("{base}{path}?{q}"),
        None => format!("{base}{path}"),
    }
}

/// Streams the upstream answer back chunk by chunk, tapping it for usage when asked.
fn relay(resp: reqwest::Response, recorder: Option<Recorder>) -> Response {
    let status = resp.status();
    let headers = filter_headers(resp.headers(), &RESPONSE_SKIP);
    let tap = recorder.map(|r| {
        let content_type = resp.headers().get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok());
        (UsageWatcher::new(content_type), r)
    });
    let upstream = resp.bytes_stream().boxed();
    let stream = futures::stream::unfold(Some((upstream, tap)), |st| async move {
        let (mut upstream, mut tap) = st?;
        match upstream.next().await {
            Some(Ok(chunk)) => {
                if let Some((watcher, _)) = tap.as_mut() {
                    watcher.feed(&chunk);
                }
                Some((Ok::<Bytes, reqwest::Error>(chunk), Some((upstream, tap))))
            }
            Some(Err(e)) => {
                tracing::warn!(error = %e, "upstream answer broke off");
                Some((Err(e), None))
            }
            None => {
                if let Some((watcher, recorder)) = tap {
                    recorder.record(watcher.finish());
                }
                None
            }
        }
    });
    let mut out = Response::new(Body::from_stream(stream));
    *out.status_mut() = status;
    *out.headers_mut() = headers;
    out
}

/// Everything needed to report one answer's usage once its stream ends.
struct Recorder {
    optimizer: Arc<dyn RequestOptimizer>,
    config: Arc<Config>,
    events: EventSink,
    ctx: OptimizeContext,
    outcome: OptimizeOutcome,
}

fn recorder(state: &ProxyState, ctx: OptimizeContext, outcome: OptimizeOutcome) -> Recorder {
    Recorder { optimizer: state.optimizer.clone(), config: state.config.clone(), events: state.events.clone(), ctx, outcome }
}

impl Recorder {
    /// Reports `usage` to the optimizer and the event stream. Nothing is reported when the
    /// answer carried no usage.
    fn record(self, usage: Option<Usage>) {
        let Some(usage) = usage else {
            return;
        };
        self.optimizer.record_usage(&self.ctx, &self.outcome, &usage);
        self.events.emit(AppEvent::Usage {
            agent_id: "proxy".into(),
            provider: "proxy".into(),
            model: self.ctx.model.clone(),
            cost_usd: self.config.cost(&self.ctx.model, &usage),
            usage,
        });
    }
}

/// The optimizer context of one request. Sessions are keyed by a hash of the client's
/// `metadata.user_id`, or of the first message when there is none.
fn context(state: &ProxyState, req: &ChatRequest) -> OptimizeContext {
    let user_id = req.extra.get("metadata").and_then(|m| m.get("user_id")).and_then(Value::as_str);
    let seed = match user_id {
        Some(id) => id.to_owned(),
        None => serde_json::to_string(&req.messages.first()).unwrap_or_default(),
    };
    let mut hasher = DefaultHasher::new();
    seed.hash(&mut hasher);
    OptimizeContext {
        session_id: format!("{:016x}", hasher.finish()),
        agent_id: "proxy".into(),
        provider_id: "proxy".into(),
        provider_kind: ProviderKind::Anthropic,
        model: req.model.clone(),
        context_window: state.config.model_info(&req.model).context_window,
        supports_images: true,
    }
}

/// Copies `src` without the headers named in `skip`.
fn filter_headers(src: &HeaderMap, skip: &[&str]) -> HeaderMap {
    let mut out = HeaderMap::new();
    for (name, value) in src {
        if !skip.contains(&name.as_str()) {
            out.append(name.clone(), value.clone());
        }
    }
    out
}

/// Reads the whole request body, or the error response to send instead (boxed to keep
/// the `Result` small).
async fn read_body(body: Body) -> Result<Bytes, Box<Response>> {
    to_bytes(body, MAX_BODY).await.map_err(|e| {
        Box::new(json_error(StatusCode::PAYLOAD_TOO_LARGE, format!("crusty-proxy: request body unreadable or over 32 MiB: {e}")))
    })
}

fn bad_gateway(e: &reqwest::Error) -> Response {
    json_error(StatusCode::BAD_GATEWAY, format!("crusty-proxy: upstream unreachable: {e}"))
}

/// An Anthropic-style error body with the given status.
fn json_error(status: StatusCode, message: String) -> Response {
    let body = json!({"type": "error", "error": {"type": "api_error", "message": message}});
    let mut res = Json(body).into_response();
    *res.status_mut() = status;
    res
}
