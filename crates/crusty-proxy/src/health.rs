//! Health and stats probes of external token-saving proxies, shown in the TUI side panel.

use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use crusty_core::config::ProxyUpstream;
use crusty_core::{AppEvent, CancellationToken, EventSink};
use futures::future::join_all;
use serde_json::Value;
use tokio::task::JoinHandle;

/// Timeout of each probe request.
const PROBE_TIMEOUT: Duration = Duration::from_secs(3);

/// Result of probing one upstream proxy.
#[derive(Debug, Clone, PartialEq)]
pub struct ProxyProbe {
    /// The health endpoint answered with a status below 500.
    pub up: bool,
    /// Time the health request took, when the proxy is up.
    pub latency_ms: Option<u64>,
    /// JSON from the stats endpoint, when configured, up and parseable.
    pub stats: Option<Value>,
}

/// Probes `up`: its health endpoint, then its stats endpoint when it is up.
pub async fn probe(up: &ProxyUpstream, http: &reqwest::Client) -> ProxyProbe {
    let started = Instant::now();
    let health = http.get(join(&up.url, &up.health_path)).timeout(PROBE_TIMEOUT).send().await;
    let is_up = matches!(&health, Ok(resp) if !resp.status().is_server_error());
    if !is_up {
        return ProxyProbe { up: false, latency_ms: None, stats: None };
    }
    let latency_ms = Some(u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX));
    let stats = match &up.stats_path {
        Some(path) => fetch_json(http, join(&up.url, path)).await,
        None => None,
    };
    ProxyProbe { up: true, latency_ms, stats }
}

/// GETs `url` and parses the body as JSON. Any failure yields `None`.
async fn fetch_json(http: &reqwest::Client, url: String) -> Option<Value> {
    let resp = http.get(url).timeout(PROBE_TIMEOUT).send().await.ok()?;
    resp.json::<Value>().await.ok()
}

/// `base` and `path` joined with exactly one slash.
fn join(base: &str, path: &str) -> String {
    format!("{}/{}", base.trim_end_matches('/'), path.trim_start_matches('/'))
}

/// Probes every enabled upstream each `interval` and emits `AppEvent::ProxyStatus` per proxy.
/// The list is re-read every round, so changes at runtime apply. Stops when `cancel` fires.
pub fn spawn_health_monitor(
    upstreams: Arc<RwLock<Vec<ProxyUpstream>>>,
    http: reqwest::Client,
    events: EventSink,
    interval: Duration,
    cancel: CancellationToken,
) -> JoinHandle<()> {
    // tokio::time::interval panics on a zero period.
    let interval = if interval.is_zero() { Duration::from_secs(1) } else { interval };
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        loop {
            tokio::select! {
                () = cancel.cancelled() => break,
                _ = ticker.tick() => {}
            }
            let enabled: Vec<ProxyUpstream> = {
                let list = upstreams.read().unwrap_or_else(|e| e.into_inner());
                list.iter().filter(|u| u.enabled).cloned().collect()
            };
            let probes = join_all(enabled.iter().map(|u| probe(u, &http))).await;
            for (up, p) in enabled.into_iter().zip(probes) {
                events.emit(AppEvent::ProxyStatus { name: up.name, url: up.url, up: p.up, latency_ms: p.latency_ms, stats: p.stats });
            }
        }
    })
}
