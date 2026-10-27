//! Outbound data-loss prevention: secrets and personal data masked before requests leave.
//!
//! [`scan`] finds the values, [`mask_text`] replaces them with vault placeholders such as
//! `⟦SECRET_1⟧`, and [`redact_request`] applies both to a whole chat request. The model
//! only ever sees placeholders; the engine restores real values locally.

mod detect;
mod mask;
mod request;
#[cfg(test)]
mod tests;

pub use detect::{Finding, scan};
pub use mask::{MaskCounts, mask_text};
pub use request::{RedactionReport, redact_request};
