//! Tests of the proxy health probes and the health monitor, with wiremock as the proxy.

use std::sync::{Arc, RwLock};
use std::time::Duration;

use crusty_core::config::ProxyUpstream;
use crusty_core::{AppEvent, CancellationToken, EventSink};
use crusty_proxy::{probe, spawn_health_monitor};
use serde_json::json;
use tokio::sync::mpsc;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn upstream(name: &str, url: &str, stats_path: Option<&str>) -> ProxyUpstream {
    ProxyUpstream {
        name: name.into(),
        url: url.into(),
        health_path: "/health".into(),
        stats_path: stats_path.map(str::to_owned),
        ..ProxyUpstream::default()
    }
}

async fn closed_url() -> String {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("probe bind");
    let port = l.local_addr().expect("probe addr").port();
    drop(l);
    format!("http://127.0.0.1:{port}")
}

#[tokio::test]
async fn probe_up_reads_stats() {
    let server = MockServer::start().await;
    Mock::given(method("GET")).and(path("/health")).respond_with(ResponseTemplate::new(200)).mount(&server).await;
    Mock::given(method("GET"))
        .and(path("/stats"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"saved_tokens": 1234})))
        .mount(&server)
        .await;
    let up = upstream("headroom", &server.uri(), Some("/stats"));
    let p = probe(&up, &reqwest::Client::new()).await;
    assert!(p.up);
    assert!(p.latency_ms.is_some());
    assert_eq!(p.stats, Some(json!({"saved_tokens": 1234})));
}

#[tokio::test]
async fn probe_treats_client_errors_as_up_and_server_errors_as_down() {
    let server = MockServer::start().await;
    Mock::given(method("GET")).and(path("/health")).respond_with(ResponseTemplate::new(404)).mount(&server).await;
    let p = probe(&upstream("a", &server.uri(), None), &reqwest::Client::new()).await;
    assert!(p.up);
    assert_eq!(p.stats, None);

    let broken = MockServer::start().await;
    Mock::given(method("GET")).and(path("/health")).respond_with(ResponseTemplate::new(503)).mount(&broken).await;
    let p = probe(&upstream("b", &broken.uri(), Some("/stats")), &reqwest::Client::new()).await;
    assert!(!p.up);
    assert_eq!(p.latency_ms, None);
    assert_eq!(p.stats, None);
}

#[tokio::test]
async fn probe_down_when_nothing_listens() {
    let p = probe(&upstream("gone", &closed_url().await, Some("/stats")), &reqwest::Client::new()).await;
    assert!(!p.up);
    assert_eq!(p.latency_ms, None);
    assert_eq!(p.stats, None);
}

#[tokio::test]
async fn probe_ignores_unparseable_stats() {
    let server = MockServer::start().await;
    Mock::given(method("GET")).and(path("/health")).respond_with(ResponseTemplate::new(200)).mount(&server).await;
    Mock::given(method("GET"))
        .and(path("/stats"))
        .respond_with(ResponseTemplate::new(200).set_body_raw("<html>", "text/html"))
        .mount(&server)
        .await;
    let p = probe(&upstream("s", &server.uri(), Some("/stats")), &reqwest::Client::new()).await;
    assert!(p.up);
    assert_eq!(p.stats, None);
}

#[tokio::test]
async fn monitor_emits_status_for_enabled_upstreams() {
    let server = MockServer::start().await;
    Mock::given(method("GET")).and(path("/health")).respond_with(ResponseTemplate::new(200)).mount(&server).await;
    let mut disabled = upstream("off", "http://127.0.0.1:1", None);
    disabled.enabled = false;
    let list = Arc::new(RwLock::new(vec![upstream("live", &server.uri(), None), disabled]));

    let (tx, mut rx) = mpsc::unbounded_channel();
    let cancel = CancellationToken::new();
    let handle = spawn_health_monitor(list, reqwest::Client::new(), EventSink::new(tx), Duration::from_millis(50), cancel.clone());

    let event = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match rx.recv().await {
                Some(e @ AppEvent::ProxyStatus { .. }) => return e,
                Some(_) => continue,
                None => panic!("event channel closed"),
            }
        }
    })
    .await
    .expect("a ProxyStatus event");

    match event {
        AppEvent::ProxyStatus { name, up, .. } => {
            assert_eq!(name, "live");
            assert!(up);
        }
        other => panic!("unexpected event {other:?}"),
    }
    cancel.cancel();
    tokio::time::timeout(Duration::from_secs(5), handle).await.expect("monitor stops").expect("join");
}
