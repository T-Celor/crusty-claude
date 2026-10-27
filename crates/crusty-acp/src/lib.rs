//! Crusty-Claude as an Agent Client Protocol (ACP) agent: editors (Zed, JetBrains, Neovim,
//! Emacs) drive the agent over newline-delimited JSON-RPC 2.0 on stdio.
//!
//! The transport and protocol live here; the engine plugs in through [`AcpBackend`], and
//! [`serve`] runs one connection on any `AsyncRead`/`AsyncWrite` pair.

mod backend;
mod protocol;
mod server;

#[cfg(test)]
mod tests;

pub use backend::{AcpBackend, ApprovalBridge, McpServerSpec, PromptPart, SessionHandle, StopReason};
pub use server::{AgentInfo, PROTOCOL_VERSION, serve};
