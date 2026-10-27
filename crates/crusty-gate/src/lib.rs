//! Context gate (gain-driven routing) and pxpipe (context rendered as images).
//!
//! [`ContextGate`] decides per request whether the prompt is sent as text or with its bulky
//! old context imaged by [`PxpipeRenderer`]. The gate itself does no I/O: callers measure
//! the pxpipe saving with a dry run and feed the result back.

pub mod gate;
pub mod pxpipe;

pub use gate::{ContextGate, DryRun, Econ, GateDecision, GateInput, GateRoute, GateStats, PreDecision, Zone, economics};
pub use pxpipe::{PxpipeRenderer, PxpipeResult, RenderedPage, factsheet, render_pages, verbatim_risk};
