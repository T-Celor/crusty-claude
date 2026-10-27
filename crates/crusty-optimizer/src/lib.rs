//! Optimization stages: RTK filters, HTML to Markdown, Headroom compression, Ponytail policy.
//!
//! Everything here is native Rust and implements or serves the contracts in `crusty-core`.
//! Pipeline wiring lives in the binary, not in this crate.

pub mod filters;
pub mod headroom;
pub mod html;
pub mod ponytail;
pub mod rtk;

pub use filters::NativeFilters;
pub use headroom::{Headroom, HeadroomReport};
pub use html::convert_html;
pub use ponytail::{PONYTAIL_VERSION, apply_ponytail, ponytail_prompt};
pub use rtk::filter_command;
