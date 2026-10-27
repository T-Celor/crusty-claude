//! Reads MCP server definitions from `.mcp.json` (Claude Code format) and from the
//! `--mcp-config` file, and merges them with the config file's `[mcp]` section.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crusty_core::config::{McpConfig, McpServerConfig};
use serde_json::Value;

use crate::McpError;

/// Project-level MCP file, in Claude Code's format.
pub const MCP_JSON: &str = ".mcp.json";

/// Reads `<project>/.mcp.json`. A missing file means no servers.
pub fn load_mcp_json(project: &Path) -> Result<BTreeMap<String, McpServerConfig>, McpError> {
    let path = project.join(MCP_JSON);
    match std::fs::read_to_string(&path) {
        Ok(text) => parse_servers(&text, &path),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
        Err(source) => Err(McpError::Io { path, source }),
    }
}

/// Reads a server file given explicitly (`--mcp-config`). Unlike `.mcp.json`, a missing file is an error.
pub fn load_server_file(path: &Path) -> Result<BTreeMap<String, McpServerConfig>, McpError> {
    let text = std::fs::read_to_string(path).map_err(|source| McpError::Io { path: path.to_path_buf(), source })?;
    parse_servers(&text, path)
}

/// Servers from every source, by name. Precedence: `.mcp.json` < `--mcp-config` file < config
/// file. Servers with `enabled = false` are removed after merging.
pub fn merged_servers(cfg: &McpConfig, project: &Path, extra_file: Option<&Path>) -> Result<BTreeMap<String, McpServerConfig>, McpError> {
    let mut servers = BTreeMap::new();
    if cfg.read_mcp_json != Some(false) {
        servers.extend(load_mcp_json(project)?);
    }
    if let Some(path) = extra_file {
        servers.extend(load_server_file(path)?);
    }
    servers.extend(cfg.servers.clone());
    servers.retain(|_, server| server.enabled != Some(false));
    Ok(servers)
}

/// Expands `${VAR}` in args, env values, url and header values with `lookup`. Unknown
/// variables are kept as written so the mistake stays visible.
pub fn expand_server(server: &McpServerConfig, lookup: impl Fn(&str) -> Option<String>) -> McpServerConfig {
    let expand = |text: &str| expand_str(text, &lookup);
    McpServerConfig {
        args: server.args.iter().map(|arg| expand(arg)).collect(),
        env: server.env.iter().map(|(k, v)| (k.clone(), expand(v))).collect(),
        url: server.url.as_deref().map(expand),
        headers: server.headers.iter().map(|(k, v)| (k.clone(), expand(v))).collect(),
        ..server.clone()
    }
}

/// Replaces each `${NAME}` in `text` that `lookup` resolves; other text is copied as is.
pub fn expand_str(text: &str, lookup: &dyn Fn(&str) -> Option<String>) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("${") {
        let Some(len) = rest[start + 2..].find('}') else { break };
        let name = &rest[start + 2..start + 2 + len];
        out.push_str(&rest[..start]);
        match lookup(name) {
            Some(value) => out.push_str(&value),
            None => out.push_str(&rest[start..start + 3 + len]),
        }
        rest = &rest[start + 3 + len..];
    }
    out.push_str(rest);
    out
}

fn parse_servers(text: &str, path: &Path) -> Result<BTreeMap<String, McpServerConfig>, McpError> {
    let root: Value = serde_json::from_str(text).map_err(|source| McpError::Json { path: path.to_path_buf(), source })?;
    let mut servers = BTreeMap::new();
    let entries = match root.get("mcpServers") {
        None | Some(Value::Null) => return Ok(servers),
        Some(Value::Object(entries)) => entries,
        Some(_) => return Err(invalid(path, "`mcpServers` must be an object")),
    };
    for (name, entry) in entries {
        let mut server: McpServerConfig =
            serde_json::from_value(entry.clone()).map_err(|source| McpError::Json { path: path.to_path_buf(), source })?;
        if let Some(kind) = entry.get("type").filter(|kind| !kind.is_null()) {
            match kind.as_str() {
                Some(known @ ("stdio" | "http" | "sse")) => server.transport = Some(known.to_owned()),
                _ => return Err(invalid(path, format!("server `{name}`: unsupported type {kind}"))),
            }
        }
        servers.insert(name.clone(), server);
    }
    Ok(servers)
}

fn invalid(path: &Path, message: impl Into<String>) -> McpError {
    McpError::Invalid { path: PathBuf::from(path), message: message.into() }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("crusty-mcp-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create scratch dir");
        dir
    }

    #[test]
    fn loads_stdio_and_http_entries() {
        let dir = scratch_dir("load");
        std::fs::write(
            dir.join(MCP_JSON),
            r#"{"mcpServers": {
                "fs": {"type": "stdio", "command": "npx", "args": ["-y", "server-fs"], "env": {"ROOT": "/x"}},
                "docs": {"type": "http", "url": "https://example.test/mcp", "headers": {"Authorization": "Bearer ${TOKEN}"}}
            }}"#,
        )
        .expect("write");
        let servers = load_mcp_json(&dir).expect("valid file");
        let fs = &servers["fs"];
        assert_eq!(fs.command.as_deref(), Some("npx"));
        assert_eq!(fs.args, ["-y", "server-fs"]);
        assert_eq!(fs.env.get("ROOT").map(String::as_str), Some("/x"));
        let docs = &servers["docs"];
        assert_eq!(docs.url.as_deref(), Some("https://example.test/mcp"));
        assert_eq!(docs.headers["Authorization"], "Bearer ${TOKEN}");
    }

    #[test]
    fn missing_file_means_no_servers() {
        let dir = scratch_dir("missing");
        assert!(load_mcp_json(&dir).expect("missing is fine").is_empty());
    }

    #[test]
    fn invalid_json_is_an_error() {
        let dir = scratch_dir("badjson");
        std::fs::write(dir.join(MCP_JSON), "{not json").expect("write");
        assert!(matches!(load_mcp_json(&dir), Err(McpError::Json { .. })));
    }

    #[test]
    fn unsupported_type_is_an_error() {
        let dir = scratch_dir("badtype");
        std::fs::write(dir.join(MCP_JSON), r#"{"mcpServers":{"x":{"type":"websocket","url":"ws://x"}}}"#).expect("write");
        assert!(matches!(load_mcp_json(&dir), Err(McpError::Invalid { .. })));
    }

    #[test]
    fn sse_type_sets_the_transport() {
        let dir = scratch_dir("sse");
        std::fs::write(dir.join(MCP_JSON), r#"{"mcpServers":{"old":{"type":"sse","url":"https://x.test/sse"}}}"#).expect("write");
        let servers = load_mcp_json(&dir).expect("sse is valid");
        assert_eq!(servers["old"].transport.as_deref(), Some("sse"));
        assert_eq!(servers["old"].url.as_deref(), Some("https://x.test/sse"));
    }

    #[test]
    fn merge_precedence_and_disabling() {
        let dir = scratch_dir("merge");
        std::fs::write(
            dir.join(MCP_JSON),
            r#"{"mcpServers":{"a":{"command":"from-file"},"b":{"command":"b"},"c":{"command":"from-file"}}}"#,
        )
        .expect("write");
        let extra = dir.join("extra.json");
        std::fs::write(&extra, r#"{"mcpServers":{"c":{"command":"from-extra"},"d":{"command":"d"}}}"#).expect("write");
        let mut cfg = McpConfig::default();
        cfg.servers.insert("a".into(), McpServerConfig { command: Some("from-config".into()), ..Default::default() });
        cfg.servers.insert("b".into(), McpServerConfig { command: Some("b".into()), enabled: Some(false), ..Default::default() });
        let merged = merged_servers(&cfg, &dir, Some(&extra)).expect("merge");
        let names: Vec<&str> = merged.keys().map(String::as_str).collect();
        assert_eq!(names, ["a", "c", "d"]);
        assert_eq!(merged["a"].command.as_deref(), Some("from-config"));
        assert_eq!(merged["c"].command.as_deref(), Some("from-extra"));
    }

    #[test]
    fn read_mcp_json_false_skips_project_file() {
        let dir = scratch_dir("skip");
        std::fs::write(dir.join(MCP_JSON), r#"{"mcpServers":{"a":{"command":"a"}}}"#).expect("write");
        let cfg = McpConfig { read_mcp_json: Some(false), ..Default::default() };
        assert!(merged_servers(&cfg, &dir, None).expect("merge").is_empty());
    }

    #[test]
    fn expands_variables_and_keeps_unknown() {
        let server = McpServerConfig {
            args: vec!["--token=${TOKEN}".into(), "${MISSING}".into(), "unterminated ${TOKEN".into()],
            env: BTreeMap::from([("KEY".to_owned(), "${TOKEN}-suffix".to_owned())]),
            url: Some("https://${HOST}/mcp".into()),
            headers: BTreeMap::from([("X-Key".to_owned(), "${TOKEN}".to_owned())]),
            ..Default::default()
        };
        let lookup = |name: &str| match name {
            "TOKEN" => Some("s3cret".to_owned()),
            "HOST" => Some("h.test".to_owned()),
            _ => None,
        };
        let out = expand_server(&server, lookup);
        assert_eq!(out.args, ["--token=s3cret", "${MISSING}", "unterminated ${TOKEN"]);
        assert_eq!(out.env["KEY"], "s3cret-suffix");
        assert_eq!(out.url.as_deref(), Some("https://h.test/mcp"));
        assert_eq!(out.headers["X-Key"], "s3cret");
    }
}
