//! Builds providers from configuration and keeps them by id.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use crusty_core::Config;
use crusty_core::config::ProviderConfig;
use crusty_core::provider::{Provider, ProviderKind};

use crate::anthropic::AnthropicProvider;
use crate::mock::MockProvider;
use crate::openai::OpenAiProvider;

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
