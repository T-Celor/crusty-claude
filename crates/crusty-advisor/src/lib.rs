//! Native advisor: deterministic heuristics for the guard, reranking, triage, routing and auto mode.
//!
//! [`NativeAdvisor`] answers the [`crusty_core::advisor::Advisor`] questions without a model.
//! It is the fallback used when the Laya server is absent. Its judgements are conservative:
//! `Allow` means no objection, `Review` means a human must confirm, `Block` means refuse.
//! Every answer is tagged [`crusty_core::advisor::AdviceSource::Native`].
//!
//! The pure algorithms are public so the pipeline can reuse them: [`rank`] (BM25),
//! [`secret`] (secret probability), [`triage`] (build and test log condensing), [`prune`]
//! (superseded tool results) and [`route`] (tier, intent and auto-approval).

pub mod guard;
pub mod native;
pub mod prune;
pub mod rank;
pub mod route;
pub mod secret;
pub mod triage;

pub use native::{DEFAULT_SENSITIVE_PATHS, NativeAdvisor};
