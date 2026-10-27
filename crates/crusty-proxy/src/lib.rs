//! Built-in optimizing HTTP proxy (Anthropic-compatible) and proxy health probes.

pub mod health;
pub mod server;
pub mod usage;

pub use health::{ProxyProbe, probe, spawn_health_monitor};
pub use server::{ProxyState, bind, router, serve};
pub use usage::UsageWatcher;
