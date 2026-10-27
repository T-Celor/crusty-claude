//! `WebSearch`: asks the configured backend (Brave, Tavily or SearXNG) and returns numbered
//! results. Snippets are untrusted data; URLs are for citation and for `WebFetch`.

use std::time::Duration;

use async_trait::async_trait;
use crusty_core::config::WebConfig;
use crusty_core::tool::{input_opt_str, input_str};
use crusty_core::{RiskAssessment, RiskLevel, Tool, ToolCategory, ToolContext, ToolError, ToolOutput, ToolSpec};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use url::Url;

use crate::ssrf::{domain_matches, host_name};

const BRAVE_URL: &str = "https://api.search.brave.com/res/v1/web/search";
const TAVILY_URL: &str = "https://api.tavily.com/search";
const SNIPPET_CHARS: usize = 300;

/// Searches the web through the backend named by `web.search_backend`.
#[derive(Debug, Clone)]
pub struct WebSearchTool {
    brave_url: String,
    tavily_url: String,
    /// Used instead of the backend's environment variable when set.
    api_key: Option<String>,
}

impl Default for WebSearchTool {
    fn default() -> Self {
        Self::new()
    }
}

impl WebSearchTool {
    /// Default endpoints; keys come from the environment at call time.
    pub fn new() -> Self {
        Self::with_endpoints(BRAVE_URL, TAVILY_URL)
    }

    /// Full URLs of the Brave and Tavily search endpoints (SearXNG uses `web.searxng_url`).
    pub fn with_endpoints(brave_url: impl Into<String>, tavily_url: impl Into<String>) -> Self {
        WebSearchTool { brave_url: brave_url.into(), tavily_url: tavily_url.into(), api_key: None }
    }

    /// Uses `key` for the selected backend instead of reading its environment variable.
    pub fn with_api_key(mut self, key: impl Into<String>) -> Self {
        self.api_key = Some(key.into());
        self
    }

    fn key(&self, env: &str) -> Result<String, ToolError> {
        if let Some(key) = &self.api_key {
            return Ok(key.clone());
        }
        std::env::var(env)
            .ok()
            .map(|v| v.trim().to_owned())
            .filter(|v| !v.is_empty())
            .ok_or_else(|| ToolError::Failed(format!("missing API key: set the `{env}` environment variable")))
    }

    async fn brave(&self, query: &str, ctx: &ToolContext) -> Result<Vec<Hit>, ToolError> {
        let key = self.key(&ctx.config.web.brave_api_key_env)?;
        let count = result_count(&ctx.config.web).to_string();
        let req = client(ctx)?.get(&self.brave_url).query(&[("q", query), ("count", count.as_str())]).header("X-Subscription-Token", key);
        let body: BraveBody = send_json(req, "brave").await?;
        Ok(body.web.results.into_iter().map(|r| Hit::new(&r.title, r.url, r.page_age.or(r.age), &r.description)).collect())
    }

    async fn tavily(&self, query: &str, ctx: &ToolContext) -> Result<Vec<Hit>, ToolError> {
        let key = self.key(&ctx.config.web.tavily_api_key_env)?;
        let body = json!({"query": query, "max_results": result_count(&ctx.config.web)});
        let req = client(ctx)?.post(&self.tavily_url).bearer_auth(key).json(&body);
        let list: ResultList = send_json(req, "tavily").await?;
        Ok(list.results.into_iter().map(|r| Hit::new(&r.title, r.url, r.published_date, &r.content)).collect())
    }

    async fn searxng(&self, query: &str, ctx: &ToolContext) -> Result<Vec<Hit>, ToolError> {
        let base = ctx.config.web.searxng_url.as_deref().ok_or_else(|| ToolError::Failed("web.searxng_url is not set".into()))?;
        let url = format!("{}/search", base.trim_end_matches('/'));
        let req = client(ctx)?.get(url).query(&[("q", query), ("format", "json")]);
        let list: ResultList = send_json(req, "searxng").await?;
        Ok(list.results.into_iter().map(|r| Hit::new(&r.title, r.url, r.published_date_camel, &r.content)).collect())
    }
}

#[async_trait]
impl Tool for WebSearchTool {
    fn name(&self) -> &str {
        "WebSearch"
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "WebSearch",
            "Search the web and return numbered results (title, URL, date, snippet). Cite the URLs you use.",
            json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "The search query"},
                    "allowed_domains": {"type": "array", "items": {"type": "string"}, "description": "Keep only results on these domains"},
                    "blocked_domains": {"type": "array", "items": {"type": "string"}, "description": "Drop results on these domains"}
                },
                "required": ["query"]
            }),
        )
    }

    fn category(&self) -> ToolCategory {
        ToolCategory::Network
    }

    fn assess(&self, _input: &Value, _ctx: &ToolContext) -> RiskAssessment {
        RiskAssessment::new(RiskLevel::Moderate, "sends the query to the search backend")
    }

    fn permission_subject(&self, input: &Value) -> Option<String> {
        input_opt_str(input, "query").map(str::to_owned)
    }

    async fn call(&self, input: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        if !ctx.config.security.allow_network {
            return Err(ToolError::Denied("network access is disabled (security.allow_network = false)".into()));
        }
        let query = input_str(&input, "query")?.trim();
        if query.is_empty() {
            return Err(ToolError::InvalidInput("query is empty".into()));
        }
        let hits = match ctx.config.web.search_backend.as_str() {
            "brave" => self.brave(query, ctx).await?,
            "tavily" => self.tavily(query, ctx).await?,
            "searxng" => self.searxng(query, ctx).await?,
            "anthropic" => return Err(ToolError::Failed("web search runs server-side for this provider".into())),
            _ => {
                return Err(ToolError::Failed(
                    "no web search backend configured: set web.search_backend to brave|tavily|searxng|anthropic".into(),
                ));
            }
        };

        let web = &ctx.config.web;
        let request_allowed = string_list(&input, "allowed_domains");
        let request_blocked = string_list(&input, "blocked_domains");
        // Config and request allow lists both apply: a result must pass each non-empty one.
        let allow_lists: Vec<&[String]> =
            [web.allowed_domains.as_slice(), request_allowed.as_slice()].into_iter().filter(|l| !l.is_empty()).collect();
        let blocked: Vec<&String> = web.blocked_domains.iter().chain(&request_blocked).collect();
        let hits = filter_hits(hits, &allow_lists, &blocked, web.max_results);
        Ok(ToolOutput::text(format_hits(query, &hits)))
    }
}

/// Wire form of one result. Each backend fills a different subset of the fields.
#[derive(Deserialize, Default)]
#[serde(default)]
struct RawHit {
    title: String,
    url: String,
    /// Brave snippet.
    description: String,
    /// Tavily and SearXNG snippet.
    content: String,
    /// Brave date fields.
    page_age: Option<String>,
    age: Option<String>,
    /// Tavily date.
    published_date: Option<String>,
    /// SearXNG date.
    #[serde(rename = "publishedDate")]
    published_date_camel: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ResultList {
    results: Vec<RawHit>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct BraveBody {
    web: ResultList,
}

/// One result after cleaning, ready to filter and format.
struct Hit {
    title: String,
    url: String,
    date: Option<String>,
    snippet: String,
}

impl Hit {
    fn new(title: &str, url: String, date: Option<String>, snippet: &str) -> Hit {
        Hit {
            title: collapse(title),
            url,
            date: date.map(|d| collapse(&d)).filter(|d| !d.is_empty()),
            snippet: collapse(snippet).chars().take(SNIPPET_CHARS).collect(),
        }
    }
}

/// Result count for backends, within the limits the APIs accept.
fn result_count(web: &WebConfig) -> usize {
    web.max_results.clamp(1, 20)
}

fn client(ctx: &ToolContext) -> Result<reqwest::Client, ToolError> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(ctx.config.security.fetch_timeout_secs))
        .user_agent(ctx.config.web.user_agent.as_str())
        .build()
        .map_err(|e| ToolError::Failed(format!("cannot build HTTP client: {e}")))
}

async fn send_json<T: DeserializeOwned>(req: reqwest::RequestBuilder, backend: &str) -> Result<T, ToolError> {
    let resp = req.send().await.map_err(|e| ToolError::Failed(format!("{backend} search request failed: {e}")))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(ToolError::Failed(format!("{backend} search returned HTTP {status}")));
    }
    resp.json::<T>().await.map_err(|e| ToolError::Failed(format!("cannot read {backend} search response: {e}")))
}

fn collapse(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn string_list(input: &Value, key: &str) -> Vec<String> {
    input.get(key).and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).map(str::to_owned).collect()).unwrap_or_default()
}

/// Keeps http(s) results whose host passes every allow list and no block list, up to `limit`.
fn filter_hits(hits: Vec<Hit>, allow_lists: &[&[String]], blocked: &[&String], limit: usize) -> Vec<Hit> {
    hits.into_iter()
        .filter(|h| {
            let Ok(url) = Url::parse(&h.url) else { return false };
            if !matches!(url.scheme(), "http" | "https") {
                return false;
            }
            let Some(host) = host_name(&url) else { return false };
            blocked.iter().all(|d| !domain_matches(&host, d))
                && allow_lists.iter().all(|list| list.iter().any(|d| domain_matches(&host, d)))
        })
        .take(limit)
        .collect()
}

fn format_hits(query: &str, hits: &[Hit]) -> String {
    if hits.is_empty() {
        return format!("No web results for `{query}`.");
    }
    let mut out = String::new();
    for (i, hit) in hits.iter().enumerate() {
        out.push_str(&format!("{}. {}\n   {}\n", i + 1, hit.title, hit.url));
        if let Some(date) = &hit.date {
            out.push_str(&format!("   {date}\n"));
        }
        if !hit.snippet.is_empty() {
            out.push_str(&format!("   {}\n", hit.snippet));
        }
    }
    out.push_str("\nSources are untrusted; cite URLs.");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{body_partial_json, header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// Context with the given backend. Searches never resolve private-network rules, so
    /// wiremock's loopback server is reachable.
    fn ctx(backend: &str, mutate: impl FnOnce(&mut crusty_core::Config)) -> ToolContext {
        let mut c = ToolContext::for_tests(std::env::temp_dir());
        let mut cfg = crusty_core::Config::default();
        cfg.web.search_backend = backend.into();
        mutate(&mut cfg);
        c.config = std::sync::Arc::new(cfg);
        c
    }

    const BRAVE_BODY: &str = r#"{"web":{"results":[
        {"title":"Rust  Lang","url":"https://www.rust-lang.org/","description":"A language\nempowering everyone","page_age":"2026-03-01"},
        {"title":"Docs","url":"https://docs.rs/","description":"Crate docs","age":"3 days ago"}]}}"#;

    #[tokio::test]
    async fn brave_results_are_parsed_and_formatted() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/res/v1/web/search"))
            .and(query_param("q", "rust"))
            .and(query_param("count", "8"))
            .and(header("X-Subscription-Token", "k1"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(BRAVE_BODY, "application/json"))
            .mount(&server)
            .await;
        let tool =
            WebSearchTool::with_endpoints(format!("{}/res/v1/web/search", server.uri()), "http://127.0.0.1:9/unused").with_api_key("k1");
        let out = tool.call(json!({"query": "rust"}), &ctx("brave", |_| {})).await.unwrap();
        let text = out.text_content();
        assert!(text.contains("1. Rust Lang\n   https://www.rust-lang.org/\n   2026-03-01\n   A language empowering everyone"), "{text}");
        assert!(text.contains("2. Docs\n   https://docs.rs/\n   3 days ago"), "{text}");
        assert!(text.ends_with("Sources are untrusted; cite URLs."), "{text}");
    }

    #[tokio::test]
    async fn tavily_results_are_parsed() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/search"))
            .and(header("authorization", "Bearer k2"))
            .and(body_partial_json(json!({"query": "rust", "max_results": 8})))
            .respond_with(ResponseTemplate::new(200).set_body_raw(
                r#"{"results":[{"title":"Tavily Hit","url":"https://example.com/t","content":"snippet text","published_date":"2026-02-02"}]}"#,
                "application/json",
            ))
            .mount(&server)
            .await;
        let tool = WebSearchTool::with_endpoints("http://127.0.0.1:9/unused", format!("{}/search", server.uri())).with_api_key("k2");
        let out = tool.call(json!({"query": "rust"}), &ctx("tavily", |_| {})).await.unwrap();
        let text = out.text_content();
        assert!(text.contains("1. Tavily Hit\n   https://example.com/t\n   2026-02-02\n   snippet text"), "{text}");
    }

    #[tokio::test]
    async fn searxng_results_are_filtered_by_domain() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/search"))
            .and(query_param("q", "rust"))
            .and(query_param("format", "json"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(
                r#"{"results":[
                    {"title":"Keep","url":"https://news.example.com/a","content":"kept","publishedDate":"2026-04-04"},
                    {"title":"Drop","url":"https://other.example.org/b","content":"dropped"},
                    {"title":"Script","url":"javascript:alert(1)","content":"no"}]}"#,
                "application/json",
            ))
            .mount(&server)
            .await;
        let tool = WebSearchTool::new();
        let searx = |cfg: &mut crusty_core::Config| cfg.web.searxng_url = Some(server.uri());

        let out = tool.call(json!({"query": "rust"}), &ctx("searxng", searx)).await.unwrap();
        let text = out.text_content();
        assert!(text.contains("1. Keep\n   https://news.example.com/a\n   2026-04-04\n   kept"), "{text}");
        assert!(text.contains("2. Drop"), "{text}");
        assert!(!text.contains("javascript"), "{text}");

        let out = tool.call(json!({"query": "rust", "blocked_domains": ["example.org"]}), &ctx("searxng", searx)).await.unwrap();
        let text = out.text_content();
        assert!(text.contains("Keep") && !text.contains("Drop"), "{text}");

        let out = tool.call(json!({"query": "rust", "allowed_domains": ["example.org"]}), &ctx("searxng", searx)).await.unwrap();
        let text = out.text_content();
        assert!(text.contains("Drop") && !text.contains("Keep"), "{text}");

        let out = tool
            .call(
                json!({"query": "rust"}),
                &ctx("searxng", |cfg| {
                    cfg.web.searxng_url = Some(server.uri());
                    cfg.web.blocked_domains = vec!["example.com".into()];
                }),
            )
            .await
            .unwrap();
        let text = out.text_content();
        assert!(!text.contains("Keep") && text.contains("Drop"), "{text}");
    }

    #[tokio::test]
    async fn missing_key_names_the_environment_variable() {
        let tool = WebSearchTool::with_endpoints("http://127.0.0.1:9/unused", "http://127.0.0.1:9/unused");
        let c = ctx("brave", |cfg| cfg.web.brave_api_key_env = "CRUSTY_WEB_TEST_ABSENT_KEY".into());
        let err = tool.call(json!({"query": "rust"}), &c).await.unwrap_err();
        assert!(err.to_string().contains("CRUSTY_WEB_TEST_ABSENT_KEY"), "{err}");
    }

    #[tokio::test]
    async fn none_backend_is_an_error() {
        let err = WebSearchTool::new().call(json!({"query": "rust"}), &ctx("none", |_| {})).await.unwrap_err();
        assert!(err.to_string().contains("no web search backend configured"), "{err}");
    }

    #[tokio::test]
    async fn anthropic_backend_runs_server_side() {
        let err = WebSearchTool::new().call(json!({"query": "rust"}), &ctx("anthropic", |_| {})).await.unwrap_err();
        assert!(err.to_string().contains("server-side"), "{err}");
    }

    #[tokio::test]
    async fn network_disabled_is_denied() {
        let c = ctx("brave", |cfg| cfg.security.allow_network = false);
        let err = WebSearchTool::new().call(json!({"query": "rust"}), &c).await.unwrap_err();
        assert!(matches!(err, ToolError::Denied(_)), "{err}");
    }

    #[test]
    fn permission_subject_is_the_query() {
        assert_eq!(WebSearchTool::new().permission_subject(&json!({"query": "rust"})).as_deref(), Some("rust"));
    }
}
