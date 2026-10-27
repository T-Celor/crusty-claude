//! Laya advisor: laya-serve client with native fallback and health monitoring.
//!
//! [`LayaAdvisor`] asks a Laya server for each advisor decision and falls back to the native
//! advisor when Laya is off, unreachable, unsure or unreadable. Verdicts only harden. The
//! health monitor ([`spawn_health_monitor`]) keeps the up flag and the UI status current.

pub mod advisor;
pub mod client;

pub use advisor::{LayaAdvisor, build_advisor, spawn_health_monitor};
pub use client::{Answer, Answers, Health, LayaClient, LayaError, MAX_STATE_CHARS, Question, QuestionKind, truncate_state};
