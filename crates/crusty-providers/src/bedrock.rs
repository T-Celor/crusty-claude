//! Claude on Amazon Bedrock through the Mantle endpoint, which serves the Messages API shape.
//! Requests carry a bearer key when one is configured, otherwise a SigV4 signature.

use std::path::PathBuf;

use async_trait::async_trait;
use crusty_core::config::ProviderConfig;
use crusty_core::message::{ChatRequest, ChatResponse};
use crusty_core::provider::{CallOptions, DeltaSink, Provider, ProviderError, ProviderKind};
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use url::Url;

use crate::anthropic::{AnthropicProvider, Endpoint};
use crate::common::{EnvFn, non_blank, process_env, resolve_base};

/// SigV4 service name of the Mantle endpoint (the SDK's `DEFAULT_SERVICE_NAME`).
const SERVICE: &str = "bedrock-mantle";
/// Bearer key variable, read when `cfg.auth_token_env` is unset.
const BEARER_ENV: &str = "AWS_BEARER_TOKEN_BEDROCK";
/// Base URL override the SDK also honours.
const MANTLE_BASE_ENV: &str = "ANTHROPIC_BEDROCK_MANTLE_BASE_URL";
const SIGNING_ALGORITHM: &str = "AWS4-HMAC-SHA256";

/// AWS credentials used to sign requests.
pub(crate) struct AwsCredentials {
    access_key: String,
    secret_key: String,
    session_token: Option<String>,
}

/// One request as SigV4 sees it.
struct Signed<'a> {
    method: &'a str,
    path: &'a str,
    query: &'a str,
    /// Lowercase names and values of every signed header.
    headers: &'a [(String, String)],
    payload_hash: &'a str,
    /// `YYYYMMDDTHHMMSSZ`.
    amz_date: &'a str,
}

/// Region of a Bedrock call: `cfg.region`, then `AWS_REGION`, then `AWS_DEFAULT_REGION`.
pub(crate) fn region(cfg: &ProviderConfig, env: &dyn Fn(&str) -> Option<String>) -> Option<String> {
    non_blank(cfg.region.clone()).or_else(|| non_blank(env("AWS_REGION"))).or_else(|| non_blank(env("AWS_DEFAULT_REGION")))
}

/// Bedrock model id: `anthropic.` is prefixed unless the id already carries it, including
/// regional ids such as `us.anthropic.…`.
pub(crate) fn model_id(model: &str) -> String {
    if model.contains("anthropic.") { model.to_owned() } else { format!("anthropic.{model}") }
}

/// Bearer key from `cfg.auth_token_env`, then `AWS_BEARER_TOKEN_BEDROCK`.
pub(crate) fn bearer_token(cfg: &ProviderConfig, env: &dyn Fn(&str) -> Option<String>) -> Option<String> {
    cfg.auth_token_env.as_deref().and_then(|name| non_blank(env(name))).or_else(|| non_blank(env(BEARER_ENV)))
}

/// Credentials from the environment (unless a profile is named), else from the credentials
/// file's profile: `cfg.aws_profile`, then `AWS_PROFILE`, then `default`.
pub(crate) fn load_credentials(cfg: &ProviderConfig, env: &dyn Fn(&str) -> Option<String>) -> Option<AwsCredentials> {
    if cfg.aws_profile.is_none()
        && let (Some(access_key), Some(secret_key)) = (non_blank(env("AWS_ACCESS_KEY_ID")), non_blank(env("AWS_SECRET_ACCESS_KEY")))
    {
        return Some(AwsCredentials { access_key, secret_key, session_token: non_blank(env("AWS_SESSION_TOKEN")) });
    }
    let profile = non_blank(cfg.aws_profile.clone()).or_else(|| non_blank(env("AWS_PROFILE"))).unwrap_or_else(|| "default".to_owned());
    let path = non_blank(env("AWS_SHARED_CREDENTIALS_FILE"))
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".aws").join("credentials")))?;
    parse_credentials(&std::fs::read_to_string(path).ok()?, &profile)
}

/// Credentials of `profile` in an INI credentials file. Comments and other profiles are skipped.
pub(crate) fn parse_credentials(text: &str, profile: &str) -> Option<AwsCredentials> {
    let mut in_profile = false;
    let (mut access_key, mut secret_key, mut session_token) = (None, None, None);
    for line in text.lines().map(str::trim) {
        if let Some(name) = line.strip_prefix('[').and_then(|rest| rest.strip_suffix(']')) {
            in_profile = name.trim() == profile;
            continue;
        }
        if !in_profile {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            let value = value.trim().to_owned();
            match key.trim() {
                "aws_access_key_id" => access_key = Some(value),
                "aws_secret_access_key" => secret_key = Some(value),
                "aws_session_token" => session_token = Some(value),
                _ => {}
            }
        }
    }
    Some(AwsCredentials { access_key: non_blank(access_key)?, secret_key: non_blank(secret_key)?, session_token: non_blank(session_token) })
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
    // HMAC accepts keys of any length, so the constructor cannot fail in practice.
    match Hmac::<Sha256>::new_from_slice(key) {
        Ok(mut mac) => {
            mac.update(data);
            mac.finalize().into_bytes().to_vec()
        }
        Err(_) => Vec::new(),
    }
}

/// SigV4 `Authorization` header value for `req`, signed for `region` and `service`.
fn authorization(creds: &AwsCredentials, region: &str, service: &str, req: &Signed<'_>) -> String {
    let mut headers = req.headers.to_vec();
    headers.sort_by(|a, b| a.0.cmp(&b.0));
    let canonical_headers: String = headers.iter().map(|(name, value)| format!("{name}:{}\n", value.trim())).collect();
    let signed_names = headers.iter().map(|(name, _)| name.as_str()).collect::<Vec<_>>().join(";");
    let canonical = format!("{}\n{}\n{}\n{canonical_headers}\n{signed_names}\n{}", req.method, req.path, req.query, req.payload_hash);
    let date = req.amz_date.get(..8).unwrap_or(req.amz_date);
    let scope = format!("{date}/{region}/{service}/aws4_request");
    let to_sign = format!("{SIGNING_ALGORITHM}\n{}\n{scope}\n{}", req.amz_date, hex(&Sha256::digest(canonical.as_bytes())));
    let k_date = hmac_sha256(format!("AWS4{}", creds.secret_key).as_bytes(), date.as_bytes());
    let k_region = hmac_sha256(&k_date, region.as_bytes());
    let k_service = hmac_sha256(&k_region, service.as_bytes());
    let k_signing = hmac_sha256(&k_service, b"aws4_request");
    let signature = hex(&hmac_sha256(&k_signing, to_sign.as_bytes()));
    format!("{SIGNING_ALGORITHM} Credential={}/{scope}, SignedHeaders={signed_names}, Signature={signature}", creds.access_key)
}

/// Headers that authorize one request with SigV4. The `host` header is implied by the URL and
/// set by the HTTP client, so it is signed but not returned.
fn sign_headers(creds: &AwsCredentials, region: &str, method: &str, url: &Url, body: &[u8], amz_date: &str) -> Vec<(String, String)> {
    let payload = hex(&Sha256::digest(body));
    let host_name = url.host_str().unwrap_or_default();
    let host = match url.port() {
        Some(port) => format!("{host_name}:{port}"),
        None => host_name.to_owned(),
    };
    let mut signed = vec![
        ("host".to_owned(), host),
        ("x-amz-content-sha256".to_owned(), payload.clone()),
        ("x-amz-date".to_owned(), amz_date.to_owned()),
    ];
    if let Some(token) = &creds.session_token {
        signed.push(("x-amz-security-token".to_owned(), token.clone()));
    }
    let auth = authorization(
        creds,
        region,
        SERVICE,
        &Signed { method, path: url.path(), query: url.query().unwrap_or_default(), headers: &signed, payload_hash: &payload, amz_date },
    );
    let mut out: Vec<(String, String)> = signed.into_iter().filter(|(name, _)| name != "host").collect();
    out.push(("authorization".to_owned(), auth));
    out
}

/// Authorization headers for one Bedrock call: a bearer key when configured, else SigV4.
pub(crate) fn auth_headers(
    cfg: &ProviderConfig,
    env: &dyn Fn(&str) -> Option<String>,
    method: &str,
    url: &str,
    body: &[u8],
) -> Result<Vec<(String, String)>, ProviderError> {
    if let Some(token) = bearer_token(cfg, env) {
        return Ok(vec![("authorization".to_owned(), format!("Bearer {token}"))]);
    }
    let sign_region =
        region(cfg, env).ok_or_else(|| ProviderError::Config("no AWS region: set region, AWS_REGION or AWS_DEFAULT_REGION".to_owned()))?;
    let creds = load_credentials(cfg, env).ok_or_else(|| {
        ProviderError::Auth("no AWS credentials: set AWS_BEARER_TOKEN_BEDROCK, AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY, or a profile in ~/.aws/credentials".to_owned())
    })?;
    let parsed = Url::parse(url).map_err(|e| ProviderError::Config(e.to_string()))?;
    let amz_date = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    Ok(sign_headers(&creds, &sign_region, method, &parsed, body, &amz_date))
}

/// Base URL of a call: an explicit one wins, then `ANTHROPIC_BEDROCK_MANTLE_BASE_URL`, then the
/// regional Mantle host.
pub(crate) fn base_url(opts: &CallOptions, cfg: &ProviderConfig, env: &dyn Fn(&str) -> Option<String>) -> Result<String, ProviderError> {
    if opts.base_url_override.is_some() || !cfg.base_url.trim().is_empty() {
        return resolve_base(opts, cfg);
    }
    if let Some(base) = non_blank(env(MANTLE_BASE_ENV)) {
        return Ok(base.trim_end_matches('/').to_owned());
    }
    let sign_region = region(cfg, env)
        .ok_or_else(|| ProviderError::Config("no AWS region for Bedrock: set region, AWS_REGION or AWS_DEFAULT_REGION".to_owned()))?;
    Ok(format!("https://bedrock-mantle.{sign_region}.api.aws/anthropic"))
}

/// Claude on Amazon Bedrock (Mantle endpoint).
pub struct BedrockProvider {
    inner: AnthropicProvider,
}

impl BedrockProvider {
    /// Provider for a `kind = "bedrock"` entry, reading the process environment.
    pub fn new(id: impl Into<String>, cfg: ProviderConfig, http: reqwest::Client) -> Self {
        Self::with_env(id, cfg, http, process_env())
    }

    /// Provider that reads its configuration values through `env`.
    pub fn with_env(id: impl Into<String>, cfg: ProviderConfig, http: reqwest::Client, env: EnvFn) -> Self {
        BedrockProvider { inner: AnthropicProvider::with_endpoint(id, cfg, http, Endpoint::Bedrock, env) }
    }
}

#[async_trait]
impl Provider for BedrockProvider {
    fn id(&self) -> &str {
        self.inner.id()
    }

    fn kind(&self) -> ProviderKind {
        ProviderKind::Bedrock
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
    use wiremock::matchers::{body_partial_json, header, header_exists, header_regex, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn cfg(base: String) -> ProviderConfig {
        ProviderConfig { kind: ProviderKind::Bedrock, base_url: base, max_retries: Some(0), ..Default::default() }
    }

    fn request() -> ChatRequest {
        let mut req = ChatRequest::new("claude-opus-5-5", 64);
        req.messages.push(Message::user_text("hello"));
        req
    }

    #[test]
    fn sigv4_matches_aws_get_vanilla_vector() {
        let creds = AwsCredentials {
            access_key: "AKIDEXAMPLE".to_owned(),
            secret_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".to_owned(),
            session_token: None,
        };
        let headers =
            vec![("host".to_owned(), "example.amazonaws.com".to_owned()), ("x-amz-date".to_owned(), "20150830T123600Z".to_owned())];
        let payload = hex(&Sha256::digest(b""));
        let auth = authorization(
            &creds,
            "us-east-1",
            "service",
            &Signed { method: "GET", path: "/", query: "", headers: &headers, payload_hash: &payload, amz_date: "20150830T123600Z" },
        );
        assert!(auth.ends_with("Signature=5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31"));
        assert!(auth.contains("Credential=AKIDEXAMPLE/20150830/us-east-1/service/aws4_request"));
    }

    #[test]
    fn credentials_file_profiles() {
        let text = "[default]\naws_access_key_id = AK1\naws_secret_access_key = S1\n\n[work]\n# comment\naws_access_key_id=AK2\naws_secret_access_key=S2\naws_session_token = T2\n";
        let default = parse_credentials(text, "default").expect("default profile");
        assert_eq!((default.access_key.as_str(), default.secret_key.as_str()), ("AK1", "S1"));
        assert!(default.session_token.is_none());
        let work = parse_credentials(text, "work").expect("work profile");
        assert_eq!(work.session_token.as_deref(), Some("T2"));
        assert!(parse_credentials(text, "missing").is_none());
    }

    #[test]
    fn region_precedence_and_model_prefix() {
        let env = test_env(&[("AWS_REGION", "eu-west-1"), ("AWS_DEFAULT_REGION", "us-west-2")]);
        assert_eq!(region(&ProviderConfig::default(), &*env).as_deref(), Some("eu-west-1"));
        let pinned = ProviderConfig { region: Some("ap-south-1".into()), ..Default::default() };
        assert_eq!(region(&pinned, &*env).as_deref(), Some("ap-south-1"));
        let fallback = test_env(&[("AWS_DEFAULT_REGION", "us-west-2")]);
        assert_eq!(region(&ProviderConfig::default(), &*fallback).as_deref(), Some("us-west-2"));
        assert_eq!(model_id("claude-opus-5-5"), "anthropic.claude-opus-5-5");
        assert_eq!(model_id("anthropic.claude-opus-5-5"), "anthropic.claude-opus-5-5");
        assert_eq!(model_id("us.anthropic.claude-opus-5-5-v1:0"), "us.anthropic.claude-opus-5-5-v1:0");
    }

    #[tokio::test]
    async fn bearer_call_uses_mantle_url_and_prefixed_model() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/anthropic/v1/messages"))
            .and(header("authorization", "Bearer key-1"))
            .and(header_exists("anthropic-version"))
            .and(body_partial_json(json!({"model": "anthropic.claude-opus-5-5"})))
            .respond_with(ResponseTemplate::new(200).set_body_raw(test_sse("hi"), "text/event-stream"))
            .expect(1)
            .mount(&server)
            .await;
        let provider = BedrockProvider::with_env(
            "bed",
            cfg(format!("{}/anthropic", server.uri())),
            reqwest::Client::new(),
            test_env(&[(BEARER_ENV, "key-1")]),
        );
        provider.stream(&request(), &CallOptions::default(), &|_: StreamDelta| {}).await.expect("stream");
    }

    #[tokio::test]
    async fn sigv4_call_sends_scoped_authorization() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/anthropic/v1/messages"))
            .and(header_regex(
                "authorization",
                r"^AWS4-HMAC-SHA256 Credential=AKIDTEST/\d{8}/us-east-1/bedrock-mantle/aws4_request, SignedHeaders=host;x-amz-content-sha256;x-amz-date, Signature=[0-9a-f]{64}$",
            ))
            .and(header_exists("x-amz-date"))
            .and(header_exists("x-amz-content-sha256"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(test_sse("hi"), "text/event-stream"))
            .expect(1)
            .mount(&server)
            .await;
        let env = test_env(&[("AWS_REGION", "us-east-1"), ("AWS_ACCESS_KEY_ID", "AKIDTEST"), ("AWS_SECRET_ACCESS_KEY", "secret")]);
        let provider = BedrockProvider::with_env("bed", cfg(format!("{}/anthropic", server.uri())), reqwest::Client::new(), env);
        provider.stream(&request(), &CallOptions::default(), &|_: StreamDelta| {}).await.expect("stream");
    }

    #[tokio::test]
    async fn count_tokens_404_is_none() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/anthropic/v1/messages/count_tokens"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        let provider = BedrockProvider::with_env(
            "bed",
            cfg(format!("{}/anthropic", server.uri())),
            reqwest::Client::new(),
            test_env(&[(BEARER_ENV, "key-1")]),
        );
        assert_eq!(provider.count_tokens(&request()).await, None);
    }
}
