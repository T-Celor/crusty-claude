//! Contract of the request optimization pipeline (Headroom → Context Gate → pxpipe).
//! The pipeline lives in `crusty-optimizer`; the agent engine and the HTTP proxy only see
//! this trait.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::config::OptimizerMode;
use crate::event::{GateInfo, Stage, StageMetrics};
use crate::message::{ChatRequest, Usage};
use crate::provider::ProviderKind;
use crate::tokens::estimate_request;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OptimizeContext {
    /// Stable per conversation: routing stays sticky per session.
    pub session_id: String,
    pub agent_id: String,
    pub provider_id: String,
    pub provider_kind: ProviderKind,
    pub model: String,
    pub context_window: u64,
    pub supports_images: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(tag = "route", rename_all = "snake_case")]
pub enum Route {
    /// Send to the provider's own base URL.
    #[default]
    Direct,
    /// Send through an external proxy (e.g. a pxpipe server).
    Via { name: String, base_url: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct OptimizeOutcome {
    pub route: Route,
    /// Estimated prompt tokens before and after all stages.
    pub est_before: u64,
    pub est_after: u64,
    pub stages: Vec<StageMetrics>,
    pub gate: Option<GateInfo>,
    /// Real input tokens of the original request, when counted by the provider.
    pub real_before: Option<u64>,
}

#[async_trait]
pub trait RequestOptimizer: Send + Sync {
    /// Rewrites `req` in place and says where to send it. Must never fail: on any internal
    /// error the request is left as it was and the route is `Direct`.
    async fn optimize(&self, req: &mut ChatRequest, ctx: &OptimizeContext) -> OptimizeOutcome;

    /// Real usage of the answer, for calibration and measured savings.
    fn record_usage(&self, _ctx: &OptimizeContext, _outcome: &OptimizeOutcome, _usage: &Usage) {}

    /// The `Via` route of `outcome` could not be reached; the caller resent the request
    /// direct. Lets the optimizer unstick the session and count the failure.
    fn record_route_failure(&self, _ctx: &OptimizeContext, _outcome: &OptimizeOutcome) {}

    /// Turns one stage on or off at runtime (`/optimizer headroom off`).
    fn set_stage_enabled(&self, _stage: Stage, _enabled: bool) -> Result<(), String> {
        Err("this optimizer has no switchable stages".into())
    }

    /// Switches between active and observe mode at runtime.
    fn set_mode(&self, _mode: OptimizerMode) -> Result<(), String> {
        Err("this optimizer has no modes".into())
    }

    /// One line per stage: enabled, mode, backend.
    fn describe(&self) -> String {
        "optimizer: pass-through".into()
    }
}

/// Pass-through optimizer: the "no optimizer" mode that must always work.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopOptimizer;

#[async_trait]
impl RequestOptimizer for NoopOptimizer {
    async fn optimize(&self, req: &mut ChatRequest, _ctx: &OptimizeContext) -> OptimizeOutcome {
        let est = estimate_request(req, false);
        OptimizeOutcome { est_before: est, est_after: est, ..OptimizeOutcome::default() }
    }
}
