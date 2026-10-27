//! Builds providers from configuration and keeps them by id.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use crusty_core::Config;
use crusty_core::config::ProviderConfig;
use crusty_core::provider::{Provider, ProviderKind};

use crate::anthropic::AnthropicProvider;
use crate::bedrock::{self, BedrockProvider};
use crate::mock::MockProvider;
use crate::openai::OpenAiProvider;
use crate::vertex::{self, VertexProvider};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_TIMEOUT_SECS: u64 = 600;

/// HTTP client with a connect timeout and a per-read timeout.
pub fn http_client(timeout_secs: u64) -> reqwest::Client {
    let builder = reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .read_timeout(Duration::from_secs(timeout_secs))
        .user_agent(format!("crusty-claude/{}", env!("CARGO_PKG_VERSION")));
    build_or_default(builder)
}

/// Builds the client, or falls back to the default client and logs why the builder failed.
fn build_or_default(builder: reqwest::ClientBuilder) -> reqwest::Client {
    builder.build().unwrap_or_else(|err| {
        tracing::warn!(error = %err, "HTTP client builder failed; using default client");
        reqwest::Client::new()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_builder_falls_back_to_default_client() {
        let builder = reqwest::Client::builder().user_agent("bad\nagent");
        assert!(reqwest::Client::builder().user_agent("bad\nagent").build().is_err());
        let _client: reqwest::Client = build_or_default(builder);
    }
}

/// Provider for one configuration entry, chosen by its kind.
pub fn build_provider(id: &str, cfg: &ProviderConfig, http: reqwest::Client) -> Arc<dyn Provider> {
    match cfg.kind {
        ProviderKind::Anthropic => Arc::new(AnthropicProvider::new(id, cfg.clone(), http)),
        ProviderKind::Openai => Arc::new(OpenAiProvider::new(id, cfg.clone(), http)),
        ProviderKind::Mock => Arc::new(MockProvider::new(id)),
        ProviderKind::Bedrock => Arc::new(BedrockProvider::new(id, cfg.clone(), http)),
        ProviderKind::Vertex => Arc::new(VertexProvider::new(id, cfg.clone(), http)),
    }
}

/// Whether the credentials a provider needs are configured, with a one-line description of
/// what was found. Reads only through `env`; `gcloud` and the metadata server are not probed.
pub fn credential_status(cfg: &ProviderConfig, env: impl Fn(&str) -> Option<String>) -> (bool, String) {
    let found = |name: Option<&str>| name.and_then(|n| env(n).filter(|v| !v.trim().is_empty()));
    match cfg.kind {
        ProviderKind::Anthropic => {
            match found(cfg.api_key_env.as_deref())
                .map(|_| "API key")
                .or_else(|| found(cfg.auth_token_env.as_deref()).map(|_| "bearer token"))
            {
                Some(what) => (true, format!("{what} found")),
                None => (false, "no API key or auth token environment variable is set".to_owned()),
            }
        }
        ProviderKind::Openai => match found(cfg.api_key_env.as_deref()) {
            Some(_) => (true, "API key found".to_owned()),
            None => (false, "no API key set (keyless servers need none)".to_owned()),
        },
        ProviderKind::Bedrock => {
            if bedrock::bearer_token(cfg, &env).is_some() {
                (true, "bearer key found".to_owned())
            } else if bedrock::load_credentials(cfg, &env).is_some() {
                (true, "AWS credentials found (SigV4)".to_owned())
            } else {
                (false, "no AWS_BEARER_TOKEN_BEDROCK, AWS access keys or credentials profile".to_owned())
            }
        }
        ProviderKind::Vertex => {
            let adc = found(Some("GOOGLE_APPLICATION_CREDENTIALS"))
                .map(std::path::PathBuf::from)
                .or_else(|| dirs::config_dir().map(|dir| dir.join("gcloud").join("application_default_credentials.json")));
            if vertex::project(cfg, &env).is_none() {
                (false, "no project: set project_id, ANTHROPIC_VERTEX_PROJECT_ID or GOOGLE_CLOUD_PROJECT".to_owned())
            } else if found(Some("GOOGLE_OAUTH_ACCESS_TOKEN")).is_some() {
                (true, "OAuth token from GOOGLE_OAUTH_ACCESS_TOKEN".to_owned())
            } else if adc.is_some_and(|path| path.exists()) {
                (true, "application default credentials found; token fetched per process".to_owned())
            } else {
                (false, "no OAuth token or application default credentials".to_owned())
            }
        }
        ProviderKind::Mock => (true, "no credentials needed".to_owned()),
    }
}

/// Providers of a configuration, keyed by their `[providers.<name>]` id.

#[derive(Default)]
pub struct ProviderRegistry {
    providers: BTreeMap<String, Arc<dyn Provider>>,
}

impl ProviderRegistry {
    /// One provider per configured entry, each with its own HTTP client.
    pub fn from_config(config: &Config) -> Self {
        let mut registry = ProviderRegistry::default();
        for (id, cfg) in &config.providers {
            let timeout = cfg.timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS);
            registry.insert(id.clone(), build_provider(id, cfg, http_client(timeout)));
        }
        registry
    }

    pub fn get(&self, id: &str) -> Option<Arc<dyn Provider>> {
        self.providers.get(id).cloned()
    }

    pub fn ids(&self) -> Vec<&str> {
        self.providers.keys().map(String::as_str).collect()
    }

    /// Adds or replaces the provider with this id.
    pub fn insert(&mut self, id: impl Into<String>, provider: Arc<dyn Provider>) {
        self.providers.insert(id.into(), provider);
    }

    pub fn is_empty(&self) -> bool {
        self.providers.is_empty()
    }
}
