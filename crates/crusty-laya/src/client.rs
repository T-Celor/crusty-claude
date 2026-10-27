//! HTTP client for `laya-serve`: `GET /health` and `POST /v1/systemone`.
//!
//! The wire format follows the Laya HTTP API: a request carries one `state` and keyed
//! `questions`; the response carries `answers` keyed by the same ids. A missing or
//! malformed answer fails only that question.

use std::collections::BTreeMap;
use std::fmt;
use std::time::{Duration, Instant};

use crusty_core::config::LayaConfig;
use serde_json::{Map, Value, json};

/// Longest state sent in one request. Longer states keep their head and tail.
pub const MAX_STATE_CHARS: usize = 6000;

/// Characters kept from the start of a truncated state.
const HEAD_CHARS: usize = 3500;
/// Characters kept from the end of a truncated state.
const TAIL_CHARS: usize = 2300;

/// What a question asks.
#[derive(Debug, Clone, PartialEq)]
pub enum QuestionKind {
    /// Pick one of the options.
    Choice(Vec<String>),
    /// Place the state on an ordered scale; the levels are listed from low to high.
    Score(Vec<String>),
    /// Yes or no; the answer is the probability of yes.
    Noul,
}

/// One question about a state.
#[derive(Debug, Clone, PartialEq)]
pub struct Question {
    /// Key of the answer in the response.
    pub id: String,
    pub kind: QuestionKind,
    pub instructions: String,
}

/// A decoded answer.
#[derive(Debug, Clone, PartialEq)]
pub enum Answer {
    /// The chosen option and the probability Laya gave it.
    Choice { choice: String, confidence: f64 },
    /// Expected level index on the question's scale, possibly between two levels.
    Score(f64),
    /// Probability of yes, 0.0 to 1.0.
    Noul(f64),
}

/// One result per question id, so one bad answer does not hide the others.
pub type Answers = BTreeMap<String, Result<Answer, LayaError>>;

/// The `/health` answer, when the server says it is ok.
#[derive(Debug, Clone, PartialEq)]
pub struct Health {
    /// Round trip of the probe.
    pub latency_ms: u64,
    /// Model revisions or device, when the server reports them (authenticated probes).
    pub detail: Option<String>,
}

/// Why a call to `laya-serve` produced no usable answer.
#[derive(Debug, Clone, PartialEq)]
pub enum LayaError {
    /// No answer within the configured timeout.
    Timeout,
    /// Connection or transport failure.
    Transport(String),
    /// A non-success HTTP status.
    Status(u16),
    /// The body, or one answer, could not be understood.
    Parse(String),
}

impl fmt::Display for LayaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LayaError::Timeout => f.write_str("laya timed out"),
            LayaError::Transport(e) => write!(f, "laya unreachable: {e}"),
            LayaError::Status(code) => write!(f, "laya answered HTTP {code}"),
            LayaError::Parse(e) => write!(f, "laya answer unreadable: {e}"),
        }
    }
}

impl std::error::Error for LayaError {}

/// Client for one `laya-serve` instance.
#[derive(Clone)]
pub struct LayaClient {
    http: reqwest::Client,
    base: String,
    api_key: Option<String>,
    timeout: Duration,
}

impl LayaClient {
    /// Builds a client; the bearer token is read from the environment variable named by
    /// `cfg.api_key_env`, when that is set and non-empty.
    pub fn new(cfg: &LayaConfig, http: reqwest::Client) -> Self {
        let key = cfg.api_key_env.as_deref().and_then(|name| std::env::var(name).ok()).filter(|k| !k.trim().is_empty());
        Self::with_api_key(cfg, http, key)
    }

    /// Builds a client with an explicit bearer token (or none).
    pub fn with_api_key(cfg: &LayaConfig, http: reqwest::Client, api_key: Option<String>) -> Self {
        Self { http, base: cfg.url.trim_end_matches('/').to_string(), api_key, timeout: Duration::from_millis(cfg.timeout_ms) }
    }

    /// The configured base URL, without a trailing slash.
    pub fn url(&self) -> &str {
        &self.base
    }

    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        let mut req = self.http.request(method, format!("{}{path}", self.base)).timeout(self.timeout);
        if let Some(key) = &self.api_key {
            req = req.bearer_auth(key);
        }
        req
    }

    /// Probes `GET /health`. The server must answer 2xx with `"status": "ok"`.
    pub async fn health(&self) -> Result<Health, LayaError> {
        let started = Instant::now();
        let resp = self.request(reqwest::Method::GET, "/health").send().await.map_err(map_err)?;
        let status = resp.status();
        if !status.is_success() {
            return Err(LayaError::Status(status.as_u16()));
        }
        let body: Value = resp.json().await.map_err(map_err)?;
        let state = body.get("status").and_then(Value::as_str).unwrap_or_default();
        if state != "ok" {
            return Err(LayaError::Parse(format!("health status {state:?}")));
        }
        let latency_ms = elapsed_ms(started);
        Ok(Health { latency_ms, detail: health_detail(&body) })
    }

    /// Asks `questions` about `state` in one request. The state is truncated to
    /// [`MAX_STATE_CHARS`]. The outer error covers the whole call; each question has its own
    /// result in the returned map.
    pub async fn ask(&self, state: &str, questions: &[Question]) -> Result<Answers, LayaError> {
        if questions.is_empty() {
            return Ok(Answers::new());
        }
        let mut qs = Map::new();
        for q in questions {
            qs.insert(q.id.clone(), question_json(q));
        }
        let body = json!({ "state": truncate_state(state), "questions": qs });
        let resp = self.request(reqwest::Method::POST, "/v1/systemone").json(&body).send().await.map_err(map_err)?;
        let status = resp.status();
        if !status.is_success() {
            return Err(LayaError::Status(status.as_u16()));
        }
        let value: Value = resp.json().await.map_err(map_err)?;
        let answers = value.get("answers").and_then(Value::as_object).ok_or_else(|| LayaError::Parse("no answers object".into()))?;
        Ok(questions
            .iter()
            .map(|q| {
                let parsed = match answers.get(&q.id) {
                    Some(v) => parse_answer(q, v),
                    None => Err(LayaError::Parse(format!("no answer for {}", q.id))),
                };
                (q.id.clone(), parsed)
            })
            .collect())
    }
}

/// Maps a transport error to a [`LayaError`], keeping timeouts apart.
fn map_err(e: reqwest::Error) -> LayaError {
    if e.is_timeout() { LayaError::Timeout } else { LayaError::Transport(e.to_string()) }
}

/// Milliseconds since `started`, saturating.
pub(crate) fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// Revisions the server loaded, or the device it computes on.
fn health_detail(body: &Value) -> Option<String> {
    let revisions = body
        .get("revisions")
        .and_then(Value::as_object)
        .map(|m| m.iter().map(|(name, rev)| format!("{name}={}", rev.as_str().unwrap_or("?"))).collect::<Vec<_>>().join(", "))
        .filter(|s| !s.is_empty());
    revisions.or_else(|| body.get("device").and_then(Value::as_str).map(|d| format!("device={d}")))
}

/// The wire form of one question.
fn question_json(q: &Question) -> Value {
    match &q.kind {
        QuestionKind::Choice(options) => {
            let criteria: Map<String, Value> = options.iter().map(|o| (o.clone(), Value::String(o.clone()))).collect();
            json!({ "type": "choice", "instructions": q.instructions, "criteria": criteria })
        }
        QuestionKind::Score(levels) => {
            json!({ "type": "score", "instructions": q.instructions, "criteria": levels })
        }
        QuestionKind::Noul => json!({ "type": "noul", "instructions": q.instructions }),
    }
}

/// A finite number, or a numeric string, from an answer field.
fn number(v: Option<&Value>) -> Option<f64> {
    let n = match v? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse::<f64>().ok(),
        _ => None,
    };
    n.filter(|x| x.is_finite())
}

/// Decodes one answer object for the question it answers.
fn parse_answer(q: &Question, v: &Value) -> Result<Answer, LayaError> {
    let bad = |what: &str| LayaError::Parse(format!("{}: {what}", q.id));
    match &q.kind {
        QuestionKind::Choice(options) => {
            let raw = v.get("choice").and_then(Value::as_str).ok_or_else(|| bad("missing choice"))?.trim();
            let choice =
                options.iter().find(|o| o.eq_ignore_ascii_case(raw)).ok_or_else(|| bad("choice is not one of the options"))?.clone();
            let confidence = number(v.get("probabilities").and_then(|p| p.get(&choice)))
                .or_else(|| number(v.get("answer_confidence")))
                .unwrap_or(0.0)
                .clamp(0.0, 1.0);
            Ok(Answer::Choice { choice, confidence })
        }
        QuestionKind::Score(_) => number(v.get("score")).map(Answer::Score).ok_or_else(|| bad("missing score")),
        QuestionKind::Noul => number(v.get("noul")).map(|p| Answer::Noul(p.clamp(0.0, 1.0))).ok_or_else(|| bad("missing noul")),
    }
}

/// Keeps the head and the tail of a state longer than [`MAX_STATE_CHARS`], so the start
/// (instructions, subject) and the end (the most recent part) both survive.
pub fn truncate_state(state: &str) -> String {
    if state.chars().count() <= MAX_STATE_CHARS {
        return state.to_string();
    }
    let head: String = state.chars().take(HEAD_CHARS).collect();
    let mut tail: Vec<char> = state.chars().rev().take(TAIL_CHARS).collect();
    tail.reverse();
    let tail: String = tail.into_iter().collect();
    format!("{head}\n[... truncated ...]\n{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn cfg(url: &str) -> LayaConfig {
        LayaConfig { url: url.into(), api_key_env: None, timeout_ms: 2000, ..LayaConfig::default() }
    }

    fn choice(id: &str, options: &[&str]) -> Question {
        Question { id: id.into(), kind: QuestionKind::Choice(options.iter().map(|s| s.to_string()).collect()), instructions: "pick".into() }
    }

    #[tokio::test]
    async fn health_reports_latency_and_revisions() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/health"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "status": "ok", "revisions": {"english": "abc"}, "device": "cpu"
            })))
            .mount(&server)
            .await;
        let client = LayaClient::with_api_key(&cfg(&server.uri()), reqwest::Client::new(), None);
        let h = client.health().await.expect("healthy server");
        assert_eq!(h.detail.as_deref(), Some("english=abc"));
    }

    #[tokio::test]
    async fn health_fails_on_error_status_and_bad_status() {
        let server = MockServer::start().await;
        Mock::given(method("GET")).and(path("/health")).respond_with(ResponseTemplate::new(503)).mount(&server).await;
        let client = LayaClient::with_api_key(&cfg(&server.uri()), reqwest::Client::new(), None);
        assert_eq!(client.health().await, Err(LayaError::Status(503)));
    }

    #[tokio::test]
    async fn health_sends_bearer_only_when_key_given() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/health"))
            .and(header("authorization", "Bearer tok"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"status": "ok"})))
            .mount(&server)
            .await;
        let with = LayaClient::with_api_key(&cfg(&server.uri()), reqwest::Client::new(), Some("tok".into()));
        assert!(with.health().await.is_ok());
        let without = LayaClient::with_api_key(&cfg(&server.uri()), reqwest::Client::new(), None);
        assert!(without.health().await.is_err());
    }

    #[tokio::test]
    async fn ask_parses_all_question_kinds_and_fails_only_the_bad_one() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/systemone"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "answers": {
                    "v": {"type": "choice", "choice": "Block", "probabilities": {"allow": 0.1, "block": "0.8"}},
                    "s": {"type": "score", "score": "2.5"},
                    "n": {"type": "noul", "noul": 0.25}
                }
            })))
            .mount(&server)
            .await;
        let questions = vec![
            choice("v", &["allow", "block"]),
            Question { id: "s".into(), kind: QuestionKind::Score(vec!["a".into(), "b".into()]), instructions: "x".into() },
            Question { id: "n".into(), kind: QuestionKind::Noul, instructions: "y".into() },
            Question { id: "missing".into(), kind: QuestionKind::Noul, instructions: "z".into() },
        ];
        let client = LayaClient::with_api_key(&cfg(&server.uri()), reqwest::Client::new(), None);
        let answers = client.ask("state", &questions).await.expect("request succeeds");
        assert_eq!(answers["v"], Ok(Answer::Choice { choice: "block".into(), confidence: 0.8 }));
        assert_eq!(answers["s"], Ok(Answer::Score(2.5)));
        assert_eq!(answers["n"], Ok(Answer::Noul(0.25)));
        assert!(matches!(answers["missing"], Err(LayaError::Parse(_))));
    }

    #[tokio::test]
    async fn ask_rejects_unknown_choice_and_maps_http_errors() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/systemone"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "answers": {"v": {"type": "choice", "choice": "maybe"}}
            })))
            .mount(&server)
            .await;
        let client = LayaClient::with_api_key(&cfg(&server.uri()), reqwest::Client::new(), None);
        let answers = client.ask("s", &[choice("v", &["allow", "block"])]).await.expect("request succeeds");
        assert!(matches!(answers["v"], Err(LayaError::Parse(_))));

        let down = MockServer::start().await;
        Mock::given(method("POST")).and(path("/v1/systemone")).respond_with(ResponseTemplate::new(400)).mount(&down).await;
        let client = LayaClient::with_api_key(&cfg(&down.uri()), reqwest::Client::new(), None);
        assert_eq!(client.ask("s", &[choice("v", &["a"])]).await.err(), Some(LayaError::Status(400)));
    }

    #[test]
    fn long_states_keep_head_and_tail_within_limit() {
        let state = format!("{}{}", "h".repeat(10_000), "t".repeat(10_000));
        let out = truncate_state(&state);
        assert!(out.chars().count() <= MAX_STATE_CHARS);
        assert!(out.starts_with('h') && out.ends_with('t'));
        assert_eq!(truncate_state("short"), "short");
        // Multi-byte text must not split a character.
        let wide = "é".repeat(MAX_STATE_CHARS + 10);
        assert!(truncate_state(&wide).chars().count() <= MAX_STATE_CHARS);
    }
}
