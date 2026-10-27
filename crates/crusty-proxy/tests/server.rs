//! End-to-end tests of the proxy: a real listener, wiremock as the upstream.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crusty_core::{
    AppEvent, CancellationToken, ChatRequest, Config, ContentBlock, EventSink, OptimizeContext, OptimizeOutcome, RequestOptimizer, Route,
    Usage, async_trait,
};
use crusty_proxy::{ProxyState, bind, serve};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Optimizer that counts calls, optionally appends a text block and routes via `via`.
#[derive(Default)]
struct TestOptimizer {
    append: bool,
    via: Option<String>,
    optimize_calls: AtomicUsize,
    route_failures: AtomicUsize,
    usages: Mutex<Vec<Usage>>,
}

#[async_trait]
impl RequestOptimizer for TestOptimizer {
    async fn optimize(&self, req: &mut ChatRequest, _ctx: &OptimizeContext) -> OptimizeOutcome {
        self.optimize_calls.fetch_add(1, Ordering::SeqCst);
        if let Some(last) = req.messages.last_mut().filter(|_| self.append) {
            last.content.push(ContentBlock::text("[optimized]"));
        }
        let route = match &self.via {
            Some(base) => Route::Via { name: "test".into(), base_url: base.clone() },
            None => Route::Direct,
        };
        OptimizeOutcome { route, ..OptimizeOutcome::default() }
    }

    fn record_usage(&self, _ctx: &OptimizeContext, _outcome: &OptimizeOutcome, usage: &Usage) {
        self.usages.lock().expect("usages lock").push(*usage);
    }

    fn record_route_failure(&self, _ctx: &OptimizeContext, _outcome: &OptimizeOutcome) {
        self.route_failures.fetch_add(1, Ordering::SeqCst);
    }
}

/// Starts a proxy on a random loopback port. Returns its base URL and its event receiver.
async fn start(opt: Arc<TestOptimizer>, forward_url: &str) -> (String, mpsc::UnboundedReceiver<AppEvent>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let listener = bind("127.0.0.1:0").await.expect("bind proxy");
    let addr = listener.local_addr().expect("proxy addr");
    let state = ProxyState {
        optimizer: opt,
        config: Arc::new(Config::default()),
        events: EventSink::new(tx),
        http: reqwest::Client::new(),
        forward_url: forward_url.to_owned(),
        started: Instant::now(),
    };
    tokio::spawn(serve(listener, state, CancellationToken::new()));
    (format!("http://{addr}"), rx)
}

/// A base URL nothing listens on: a port that was just free.
async fn closed_url() -> String {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("probe bind");
    let port = l.local_addr().expect("probe addr").port();
    drop(l);
    format!("http://127.0.0.1:{port}")
}

fn chat_body() -> Value {
    json!({
        "model": "claude-test",
        "max_tokens": 64,
        "messages": [{"role": "user", "content": "hi"}],
        "stream": true
    })
}

const SSE: &str = "event: message_start\n\
data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":10,\"cache_read_input_tokens\":5,\"cache_creation_input_tokens\":2,\"output_tokens\":1}}}\n\
\n\
event: content_block_delta\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hello\"}}\n\
\n\
event: message_delta\n\
data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":42}}\n\
\n\
event: message_stop\n\
data: {\"type\":\"message_stop\"}\n\
\n";

async fn next_usage(rx: &mut mpsc::UnboundedReceiver<AppEvent>) -> Option<Usage> {
    let deadline = Duration::from_secs(5);
    loop {
        match tokio::time::timeout(deadline, rx.recv()).await.ok()? {
            Some(AppEvent::Usage { usage, .. }) => return Some(usage),
            Some(_) => continue,
            None => return None,
        }
    }
}

#[tokio::test]
async fn health_endpoints_answer_ok() {
    let (base, _rx) = start(Arc::new(TestOptimizer::default()), "http://127.0.0.1:1").await;
    let client = reqwest::Client::new();
    for p in ["/", "/livez"] {
        let resp = client.get(format!("{base}{p}")).send().await.expect("health");
        assert_eq!(resp.status(), 200);
        let body: Value = resp.json().await.expect("health json");
        assert_eq!(body, json!({"ok": true}));
    }
}

#[tokio::test]
async fn stats_reports_uptime_and_metrics() {
    let (base, _rx) = start(Arc::new(TestOptimizer::default()), "http://127.0.0.1:1").await;
    let body: Value = reqwest::get(format!("{base}/stats")).await.expect("stats").json().await.expect("stats json");
    assert!(body["uptime_secs"].is_u64());
    assert!(body["metrics"].is_object());
}

#[tokio::test]
async fn sse_answer_is_passed_unchanged_and_usage_recorded() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(SSE, "text/event-stream"))
        .mount(&upstream)
        .await;
    let opt = Arc::new(TestOptimizer::default());
    let (base, mut rx) = start(opt.clone(), &upstream.uri()).await;

    let resp = reqwest::Client::new().post(format!("{base}/v1/messages")).json(&chat_body()).send().await.expect("messages");
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["content-type"], "text/event-stream");
    assert_eq!(resp.text().await.expect("sse body"), SSE);

    let expected = Usage { input_tokens: 10, output_tokens: 42, cache_read_input_tokens: 5, cache_creation_input_tokens: 2 };
    assert_eq!(next_usage(&mut rx).await, Some(expected));
    assert_eq!(opt.usages.lock().expect("usages").as_slice(), &[expected]);
    assert_eq!(opt.optimize_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn json_answer_usage_recorded() {
    let upstream = MockServer::start().await;
    let body = json!({"id": "m", "content": [], "usage": {"input_tokens": 3, "output_tokens": 7}});
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body.to_string(), "application/json"))
        .mount(&upstream)
        .await;
    let opt = Arc::new(TestOptimizer::default());
    let (base, mut rx) = start(opt.clone(), &upstream.uri()).await;

    let resp = reqwest::Client::new().post(format!("{base}/v1/messages")).json(&chat_body()).send().await.expect("messages");
    assert_eq!(resp.json::<Value>().await.expect("json"), body);
    assert_eq!(next_usage(&mut rx).await, Some(Usage { input_tokens: 3, output_tokens: 7, ..Usage::default() }));
}

#[tokio::test]
async fn optimizer_rewrite_reaches_upstream() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_raw("{}", "application/json"))
        .mount(&upstream)
        .await;
    let opt = Arc::new(TestOptimizer { append: true, ..TestOptimizer::default() });
    let (base, _rx) = start(opt, &upstream.uri()).await;

    reqwest::Client::new().post(format!("{base}/v1/messages")).json(&chat_body()).send().await.expect("messages");
    let received = upstream.received_requests().await.expect("recording on");
    let sent: Value = serde_json::from_slice(&received[0].body).expect("upstream json");
    let blocks = sent["messages"][0]["content"].as_array().expect("content blocks");
    assert_eq!(blocks.last().expect("last block")["text"], "[optimized]");
}

#[tokio::test]
async fn unparseable_body_is_forwarded_raw() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST")).and(path("/v1/messages")).respond_with(ResponseTemplate::new(200)).mount(&upstream).await;
    let opt = Arc::new(TestOptimizer::default());
    let (base, _rx) = start(opt.clone(), &upstream.uri()).await;

    let raw = b"not json at all".to_vec();
    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/messages"))
        .header("content-type", "text/plain")
        .body(raw.clone())
        .send()
        .await
        .expect("messages");
    assert_eq!(resp.status(), 200);
    let received = upstream.received_requests().await.expect("recording on");
    assert_eq!(received[0].body, raw);
    assert_eq!(opt.optimize_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn other_paths_and_query_strings_pass_through() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages/count_tokens"))
        .and(query_param("beta", "true"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(r#"{"input_tokens":9}"#, "application/json"))
        .mount(&upstream)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .and(query_param("limit", "2"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(r#"{"data":[]}"#, "application/json"))
        .mount(&upstream)
        .await;
    let opt = Arc::new(TestOptimizer::default());
    let (base, _rx) = start(opt.clone(), &upstream.uri()).await;
    let client = reqwest::Client::new();

    let count = client.post(format!("{base}/v1/messages/count_tokens?beta=true")).json(&chat_body()).send().await.expect("count_tokens");
    assert_eq!(count.json::<Value>().await.expect("count json"), json!({"input_tokens": 9}));

    let models = client.get(format!("{base}/v1/models?limit=2")).send().await.expect("models");
    assert_eq!(models.status(), 200);
    assert_eq!(opt.optimize_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn hop_by_hop_and_accept_encoding_are_not_forwarded() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_raw("{}", "application/json"))
        .mount(&upstream)
        .await;
    let (base, _rx) = start(Arc::new(TestOptimizer::default()), &upstream.uri()).await;

    reqwest::Client::new()
        .post(format!("{base}/v1/messages"))
        .header("x-api-key", "sk-test-key")
        .header("accept-encoding", "marker-encoding")
        .header("keep-alive", "timeout=5")
        .json(&chat_body())
        .send()
        .await
        .expect("messages");
    let received = upstream.received_requests().await.expect("recording on");
    let headers = &received[0].headers;
    assert_eq!(headers["x-api-key"], "sk-test-key");
    assert_ne!(headers.get("accept-encoding").and_then(|v| v.to_str().ok()), Some("marker-encoding"));
    assert!(!headers.contains_key("keep-alive"));
}

#[tokio::test]
async fn via_route_goes_to_the_via_url() {
    let direct = MockServer::start().await;
    let via = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_raw("{}", "application/json"))
        .mount(&via)
        .await;
    let opt = Arc::new(TestOptimizer { via: Some(via.uri()), ..TestOptimizer::default() });
    let (base, _rx) = start(opt.clone(), &direct.uri()).await;

    let resp = reqwest::Client::new().post(format!("{base}/v1/messages")).json(&chat_body()).send().await.expect("messages");
    assert_eq!(resp.status(), 200);
    assert_eq!(via.received_requests().await.expect("recording on").len(), 1);
    assert_eq!(direct.received_requests().await.expect("recording on").len(), 0);
    assert_eq!(opt.route_failures.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn unreachable_via_is_reported_and_served_direct() {
    let direct = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_raw("{\"direct\":true}", "application/json"))
        .mount(&direct)
        .await;
    let opt = Arc::new(TestOptimizer { via: Some(closed_url().await), ..TestOptimizer::default() });
    let (base, _rx) = start(opt.clone(), &direct.uri()).await;

    let resp = reqwest::Client::new().post(format!("{base}/v1/messages")).json(&chat_body()).send().await.expect("messages");
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.json::<Value>().await.expect("json"), json!({"direct": true}));
    assert_eq!(opt.route_failures.load(Ordering::SeqCst), 1);
    assert_eq!(direct.received_requests().await.expect("recording on").len(), 1);
}

#[tokio::test]
async fn unreachable_direct_upstream_gives_502_json() {
    let opt = Arc::new(TestOptimizer::default());
    let (base, _rx) = start(opt, &closed_url().await).await;

    let resp = reqwest::Client::new().post(format!("{base}/v1/messages")).json(&chat_body()).send().await.expect("messages");
    assert_eq!(resp.status(), 502);
    let body: Value = resp.json().await.expect("error json");
    assert_eq!(body["type"], "error");
    assert_eq!(body["error"]["type"], "api_error");
    assert!(body["error"]["message"].as_str().is_some_and(|m| m.starts_with("crusty-proxy: upstream unreachable")));
}

#[tokio::test]
async fn shutdown_stops_the_server() {
    let listener = bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let (tx, _rx) = mpsc::unbounded_channel();
    let state = ProxyState {
        optimizer: Arc::new(TestOptimizer::default()),
        config: Arc::new(Config::default()),
        events: EventSink::new(tx),
        http: reqwest::Client::new(),
        forward_url: "http://127.0.0.1:1".into(),
        started: Instant::now(),
    };
    let cancel = CancellationToken::new();
    let task = tokio::spawn(serve(listener, state, cancel.clone()));
    cancel.cancel();
    let result = tokio::time::timeout(Duration::from_secs(5), task).await.expect("server stops").expect("join");
    assert!(result.is_ok());
    assert!(tokio::net::TcpStream::connect(addr).await.is_err());
}
