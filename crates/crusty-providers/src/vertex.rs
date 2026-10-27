//! Claude on Google Cloud Vertex AI (`streamRawPredict`). The model is part of the URL and the
//! body carries `anthropic_version` instead.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use crusty_core::config::ProviderConfig;
use crusty_core::message::{ChatRequest, ChatResponse};
use crusty_core::provider::{CallOptions, DeltaSink, Provider, ProviderError, ProviderKind};
use serde_json::Value;
use tokio::process::Command;

use crate::anthropic::{AnthropicProvider, Endpoint};
use crate::common::{EnvFn, non_blank, process_env, resolve_base};

/// `anthropic_version` the Vertex body carries.
pub(crate) const VERTEX_VERSION: &str = "vertex-2023-10-16";
const DEFAULT_REGION: &str = "global";
const TOKEN_TTL: Duration = Duration::from_secs(50 * 60);
const METADATA_URL: &str = "http://metadata.google.internal/computeMetadata/v1/instance/service-accounts/default/token";
const METADATA_TIMEOUT: Duration = Duration::from_secs(2);
const GCLOUD_TIMEOUT: Duration = Duration::from_secs(20);
/// `gcloud` invocations in order; the first that prints a token wins.
const GCLOUD_TOKEN_COMMANDS: [&[&str]; 2] = [&["auth", "application-default", "print-access-token"], &["auth", "print-access-token"]];

/// Source of OAuth access tokens for Vertex calls.
#[async_trait]
pub trait TokenSource: Send + Sync {
    /// A bearer token for one call.
    async fn access_token(&self) -> Result<String, ProviderError>;
}

/// Tokens from `GOOGLE_OAUTH_ACCESS_TOKEN`, then `gcloud`, then the GCE metadata server.
/// `gcloud` and metadata tokens are cached for 50 minutes.
pub struct DefaultTokenSource {
    env: EnvFn,
    http: reqwest::Client,
    cached: Mutex<Option<(String, Instant)>>,
}

impl DefaultTokenSource {
    /// Token source that reads `env` and uses `http` for the metadata server.
    pub fn new(env: EnvFn, http: reqwest::Client) -> Self {
        DefaultTokenSource { env, http, cached: Mutex::new(None) }
    }

    fn cached(&self) -> Option<String> {
        let guard = self.cached.lock().unwrap_or_else(PoisonError::into_inner);
        guard.as_ref().filter(|(_, until)| Instant::now() < *until).map(|(token, _)| token.clone())
    }
}

#[async_trait]
impl TokenSource for DefaultTokenSource {
    async fn access_token(&self) -> Result<String, ProviderError> {
        if let Some(token) = non_blank((self.env)("GOOGLE_OAUTH_ACCESS_TOKEN")) {
            return Ok(token);
        }
        if let Some(token) = self.cached() {
            return Ok(token);
        }
        let token = match gcloud_token().await {
            Some(token) => token,
            None => metadata_token(&self.http).await.ok_or_else(|| {
                ProviderError::Auth(
                    "no Google access token: set GOOGLE_OAUTH_ACCESS_TOKEN, run `gcloud auth application-default login`, or run on a GCE instance".to_owned(),
                )
            })?,
        };
        *self.cached.lock().unwrap_or_else(PoisonError::into_inner) = Some((token.clone(), Instant::now() + TOKEN_TTL));
        Ok(token)
    }
}

/// Token printed by the first `gcloud` command that succeeds.
async fn gcloud_token() -> Option<String> {
    for args in GCLOUD_TOKEN_COMMANDS {
        // On Windows gcloud is a `.cmd` shim, which only `cmd` can start.
        let mut cmd = if cfg!(windows) {
            let mut cmd = Command::new("cmd");
            cmd.args(["/C", "gcloud"]);
            cmd
        } else {
            Command::new("gcloud")
        };
        cmd.args(args).stdin(std::process::Stdio::null());
        let Ok(Ok(out)) = tokio::time::timeout(GCLOUD_TIMEOUT, cmd.output()).await else {
            continue;
        };
        if out.status.success() {
            let token = String::from_utf8_lossy(&out.stdout).trim().to_owned();
            if !token.is_empty() {
                return Some(token);
            }
        }
    }
    None
}

/// Token from the GCE metadata server, if this process runs on Google Cloud.
async fn metadata_token(http: &reqwest::Client) -> Option<String> {
    let resp =
        http.get(METADATA_URL).header("Metadata-Flavor", "Google").timeout(METADATA_TIMEOUT).send().await.ok()?.error_for_status().ok()?;
    let value: Value = resp.json().await.ok()?;
    value["access_token"].as_str().map(str::to_owned)
}

/// Project of Vertex calls: `cfg.project_id`, then `ANTHROPIC_VERTEX_PROJECT_ID`, then `GOOGLE_CLOUD_PROJECT`.
pub(crate) fn project(cfg: &ProviderConfig, env: &dyn Fn(&str) -> Option<String>) -> Option<String> {
    non_blank(cfg.project_id.clone())
        .or_else(|| non_blank(env("ANTHROPIC_VERTEX_PROJECT_ID")))
        .or_else(|| non_blank(env("GOOGLE_CLOUD_PROJECT")))
}

/// Region of Vertex calls: `cfg.region`, then `CLOUD_ML_REGION`, then `global`.
pub(crate) fn region(cfg: &ProviderConfig, env: &dyn Fn(&str) -> Option<String>) -> String {
    non_blank(cfg.region.clone()).or_else(|| non_blank(env("CLOUD_ML_REGION"))).unwrap_or_else(|| DEFAULT_REGION.to_owned())
}

/// Base URL: an explicit one wins, then `ANTHROPIC_VERTEX_BASE_URL`, then the regional host
/// (`global`, `us` and `eu` have their own hosts).
pub(crate) fn base_url(opts: &CallOptions, cfg: &ProviderConfig, env: &dyn Fn(&str) -> Option<String>) -> Result<String, ProviderError> {
    if opts.base_url_override.is_some() || !cfg.base_url.trim().is_empty() {
        return resolve_base(opts, cfg);
    }
    if let Some(base) = non_blank(env("ANTHROPIC_VERTEX_BASE_URL")) {
        return Ok(base.trim_end_matches('/').to_owned());
    }
    let host = match region(cfg, env).as_str() {
        "global" => "aiplatform.googleapis.com".to_owned(),
        "us" => "aiplatform.us.rep.googleapis.com".to_owned(),
        "eu" => "aiplatform.eu.rep.googleapis.com".to_owned(),
        other => format!("{other}-aiplatform.googleapis.com"),
    };
    Ok(format!("https://{host}/v1"))
}

fn project_required(cfg: &ProviderConfig, env: &dyn Fn(&str) -> Option<String>) -> Result<String, ProviderError> {
    project(cfg, env).ok_or_else(|| {
        ProviderError::Config("no Vertex project: set project_id, ANTHROPIC_VERTEX_PROJECT_ID or GOOGLE_CLOUD_PROJECT".to_owned())
    })
}

/// URL of a streamed Messages call for `model`.
pub(crate) fn messages_url(
    base: &str,
    cfg: &ProviderConfig,
    env: &dyn Fn(&str) -> Option<String>,
    model: &str,
) -> Result<String, ProviderError> {
    let project = project_required(cfg, env)?;
    let location = region(cfg, env);
    Ok(format!("{base}/projects/{project}/locations/{location}/publishers/anthropic/models/{model}:streamRawPredict"))
}

/// URL of the shared token-counting method.
pub(crate) fn count_url(base: &str, cfg: &ProviderConfig, env: &dyn Fn(&str) -> Option<String>) -> Result<String, ProviderError> {
    let project = project_required(cfg, env)?;
    let location = region(cfg, env);
    Ok(format!("{base}/projects/{project}/locations/{location}/publishers/anthropic/models/count-tokens:rawPredict"))
}

/// Claude on Google Cloud Vertex AI.
pub struct VertexProvider {
    inner: AnthropicProvider,
}

impl VertexProvider {
    /// Provider for a `kind = "vertex"` entry, reading the process environment and the default token chain.
    pub fn new(id: impl Into<String>, cfg: ProviderConfig, http: reqwest::Client) -> Self {
        let env = process_env();
        let tokens = Arc::new(DefaultTokenSource::new(env.clone(), http.clone()));
        Self::with_parts(id, cfg, http, env, tokens)
    }

    /// Provider that reads configuration through `env` and takes its tokens from `tokens`.
    pub fn with_parts(id: impl Into<String>, cfg: ProviderConfig, http: reqwest::Client, env: EnvFn, tokens: Arc<dyn TokenSource>) -> Self {
        VertexProvider { inner: AnthropicProvider::with_endpoint(id, cfg, http, Endpoint::Vertex(tokens), env) }
    }
}

#[async_trait]
impl Provider for VertexProvider {
    fn id(&self) -> &str {
        self.inner.id()
    }

    fn kind(&self) -> ProviderKind {
        ProviderKind::Vertex
    }

    async fn stream(&self, req: &ChatRequest, opts: &CallOptions, on_delta: DeltaSink<'_>) -> Result<ChatResponse, ProviderError> {
        self.inner.stream(req, opts, on_delta).await
    }

    async fn count_tokens(&self, req: &ChatRequest) -> Option<u64> {
        self.inner.count_tokens(req).await
    }

    async fn list_models(&self) -> Result<Vec<String>, ProviderError> {
        self.inner.list_models().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::{test_env, test_sse};
    use crusty_core::message::{Message, StreamDelta};
    use serde_json::json;
    use wiremock::matchers::{body_partial_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    struct StaticTokens(&'static str);

    #[async_trait]
    impl TokenSource for StaticTokens {
        async fn access_token(&self) -> Result<String, ProviderError> {
            Ok(self.0.to_owned())
        }
    }

    fn cfg() -> ProviderConfig {
        ProviderConfig { kind: ProviderKind::Vertex, max_retries: Some(0), ..Default::default() }
    }

    fn request() -> ChatRequest {
        let mut req = ChatRequest::new("claude-opus-5-5", 64);
        req.messages.push(Message::user_text("hello"));
        req
    }

    #[test]
    fn global_and_regional_urls() {
        let env = test_env(&[("GOOGLE_CLOUD_PROJECT", "proj")]);
        let base = base_url(&CallOptions::default(), &cfg(), &*env).expect("base");
        assert_eq!(base, "https://aiplatform.googleapis.com/v1");
        assert_eq!(
            messages_url(&base, &cfg(), &*env, "claude-opus-5-5").expect("url"),
            "https://aiplatform.googleapis.com/v1/projects/proj/locations/global/publishers/anthropic/models/claude-opus-5-5:streamRawPredict"
        );
        let env = test_env(&[("ANTHROPIC_VERTEX_PROJECT_ID", "proj"), ("CLOUD_ML_REGION", "us-east5")]);
        let base = base_url(&CallOptions::default(), &cfg(), &*env).expect("base");
        assert_eq!(base, "https://us-east5-aiplatform.googleapis.com/v1");
        assert_eq!(
            messages_url(&base, &cfg(), &*env, "claude-opus-5-5").expect("url"),
            "https://us-east5-aiplatform.googleapis.com/v1/projects/proj/locations/us-east5/publishers/anthropic/models/claude-opus-5-5:streamRawPredict"
        );
        assert!(count_url(&base, &cfg(), &*test_env(&[])).is_err());
    }

    #[test]
    fn body_drops_model_and_carries_version() {
        let provider = VertexProvider::with_parts("v", cfg(), reqwest::Client::new(), test_env(&[]), Arc::new(StaticTokens("t")));
        let mut body = json!({"model": "claude-opus-5-5", "max_tokens": 5});
        provider.inner.shape_body(&mut body, "claude-opus-5-5", true);
        assert!(body.get("model").is_none());
        assert_eq!(body["anthropic_version"], json!(VERTEX_VERSION));
    }

    #[tokio::test]
    async fn env_token_wins_without_network() {
        let source = DefaultTokenSource::new(test_env(&[("GOOGLE_OAUTH_ACCESS_TOKEN", "env-token")]), reqwest::Client::new());
        assert_eq!(source.access_token().await.expect("token"), "env-token");
    }

    #[tokio::test]
    async fn streams_from_raw_predict_with_injected_token() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/projects/proj/locations/us-east5/publishers/anthropic/models/claude-opus-5-5:streamRawPredict"))
            .and(header("authorization", "Bearer static-token"))
            .and(body_partial_json(json!({"anthropic_version": VERTEX_VERSION})))
            .respond_with(ResponseTemplate::new(200).set_body_raw(test_sse("hi"), "text/event-stream"))
            .expect(1)
            .mount(&server)
            .await;
        let cfg = ProviderConfig {
            base_url: format!("{}/v1", server.uri()),
            region: Some("us-east5".into()),
            project_id: Some("proj".into()),
            ..cfg()
        };
        let provider = VertexProvider::with_parts("v", cfg, reqwest::Client::new(), test_env(&[]), Arc::new(StaticTokens("static-token")));
        provider.stream(&request(), &CallOptions::default(), &|_: StreamDelta| {}).await.expect("stream");
    }

    #[tokio::test]
    async fn count_tokens_404_is_none() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/projects/proj/locations/global/publishers/anthropic/models/count-tokens:rawPredict"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        let cfg = ProviderConfig { base_url: format!("{}/v1", server.uri()), project_id: Some("proj".into()), ..cfg() };
        let provider = VertexProvider::with_parts("v", cfg, reqwest::Client::new(), test_env(&[]), Arc::new(StaticTokens("t")));
        assert_eq!(provider.count_tokens(&request()).await, None);
    }
}
