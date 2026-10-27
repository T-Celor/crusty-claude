//! MCP client over the official Rust SDK (rmcp).
//!
//! `config` reads server definitions, `client` connects to them, `tool` exposes each remote
//! tool to the agent as a `crusty_core::Tool`, and `convert` maps MCP values to crusty-core values.

use std::fmt;
use std::path::PathBuf;

pub mod client;
pub mod config;
pub mod convert;
pub mod sse;
pub mod tool;

pub use client::{McpManager, McpServerStatus};
pub use config::{load_mcp_json, merged_servers};
pub use tool::McpTool;

/// Errors while reading MCP configuration. Connection failures are not errors: they are
/// reported per server by [`McpManager::status`].
#[derive(Debug)]
pub enum McpError {
    /// A config file could not be read.
    Io { path: PathBuf, source: std::io::Error },
    /// A config file is not valid JSON, or a server entry does not match the schema.
    Json { path: PathBuf, source: serde_json::Error },
    /// A config file is valid JSON but not a valid MCP configuration.
    Invalid { path: PathBuf, message: String },
}

impl fmt::Display for McpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            McpError::Io { path, source } => write!(f, "cannot read {}: {source}", path.display()),
            McpError::Json { path, source } => write!(f, "invalid JSON in {}: {source}", path.display()),
            McpError::Invalid { path, message } => write!(f, "invalid MCP config {}: {message}", path.display()),
        }
    }
}

impl std::error::Error for McpError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            McpError::Io { source, .. } => Some(source),
            McpError::Json { source, .. } => Some(source),
            McpError::Invalid { .. } => None,
        }
    }
}
