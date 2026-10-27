//! Built-in file tools, planning tools and the permission engine.
//!
//! Every tool here is a `crusty_core::Tool`. The engine decides whether a call runs
//! (`PermissionEngine`), and the tools confine themselves to the workspace (`sandbox`).

pub mod edit;
pub mod glob;
pub mod grep;
pub mod ls;
pub mod notebook;
pub mod permissions;
pub mod read;
pub mod registry;
pub mod retrieve;
pub mod sandbox;
pub mod todo;
pub mod tracker;
pub mod write;

use std::path::Path;

use crusty_core::tool::{ToolError, input_opt_str};
use serde_json::Value;

pub use permissions::{PermissionEngine, Rule};
pub use registry::{FileToolsState, ToolRegistry, file_tools};
pub use sandbox::{display_path, is_protected, resolve_path};
pub use todo::TodoStore;
pub use tracker::FileTracker;

/// Reads an optional unsigned integer field. `null` and absent both mean `None`.
pub(crate) fn opt_u64(input: &Value, key: &str) -> Result<Option<u64>, ToolError> {
    match input.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value.as_u64().map(Some).ok_or_else(|| ToolError::InvalidInput(format!("`{key}` must be a non-negative integer"))),
    }
}

/// Reads an optional boolean field, defaulting to `false`.
pub(crate) fn opt_bool(input: &Value, key: &str) -> Result<bool, ToolError> {
    match input.get(key) {
        None | Some(Value::Null) => Ok(false),
        Some(Value::Bool(flag)) => Ok(*flag),
        Some(_) => Err(ToolError::InvalidInput(format!("`{key}` must be true or false"))),
    }
}

/// The path field used as the permission subject of search tools (`.` when absent).
pub(crate) fn search_subject(input: &Value) -> Option<String> {
    Some(input_opt_str(input, "path").unwrap_or(".").to_owned())
}

/// The file field used as the permission subject of file tools.
pub(crate) fn file_subject(input: &Value, key: &str) -> Option<String> {
    input.get(key)?.as_str().map(str::to_owned)
}

/// Forward-slash form of a path, as rules and the model see it.
pub(crate) fn to_slashes(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// Workspace-relative path for output, or the display path when outside the workspace.
pub(crate) fn display_rel(path: &Path, workspace: &Path) -> String {
    match path.strip_prefix(workspace) {
        Ok(rel) => to_slashes(rel),
        Err(_) => display_path(path),
    }
}

/// A file is binary when its first 8 KiB contain a NUL byte.
pub(crate) fn is_binary(bytes: &[u8]) -> bool {
    bytes[..bytes.len().min(8 * 1024)].contains(&0)
}

#[cfg(test)]
pub(crate) mod test_support {
    use crusty_core::tool::ToolContext;
    use tempfile::TempDir;

    /// A context whose workspace is `dir`, with default configuration.
    pub fn context(dir: &TempDir) -> ToolContext {
        ToolContext::for_tests(dir.path())
    }
}
