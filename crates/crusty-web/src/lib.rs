//! WebFetch and WebSearch tools with SSRF protection and provenance.
//!
//! Fetches check every hop with `ssrf` and connect to the address that was checked. Page and
//! search content is returned as untrusted data; nothing in it is treated as an instruction.

use std::sync::Arc;

use crusty_core::Tool;

pub mod fetch;
pub mod search;
pub mod ssrf;

pub use fetch::WebFetchTool;
pub use search::WebSearchTool;
pub use ssrf::{check_url, is_forbidden_ip, resolve_checked};

/// The web tools with default endpoints. API keys are read from the environment on each call.
pub fn web_tools() -> Vec<Arc<dyn Tool>> {
    vec![Arc::new(WebFetchTool), Arc::new(WebSearchTool::new())]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn web_tools_are_fetch_and_search() {
        let names: Vec<String> = web_tools().iter().map(|t| t.name().to_owned()).collect();
        assert_eq!(names, ["WebFetch", "WebSearch"]);
    }
}
