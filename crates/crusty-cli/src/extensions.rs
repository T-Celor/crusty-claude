//! What installed plugins contribute to a session: custom commands, agents, skill folders,
//! hooks and MCP servers. Each function reads one kind of plugin content; `wiring` decides
//! where the results go.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use crusty_core::config::{Config, HookMatcher, McpServerConfig};
use crusty_core::hooks::HookEvent;
use crusty_session::agents::{AgentDef, resolve_model};
use crusty_session::commands::CustomCommand;
use crusty_session::frontmatter;
use crusty_session::plugins::{Plugin, PluginSource, expand_plugin_root};
use crusty_session::skills::{Skill, SkillSource, load_skills};
use serde_json::Value;

/// Markdown files under `dir`, recursively and sorted. A missing folder has none.
fn markdown_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(folder) = stack.pop() {
        let Ok(entries) = fs::read_dir(&folder) else { continue };
        for path in entries.flatten().map(|e| e.path()) {
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "md") {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

/// Slash commands from the plugins' `commands` folders. A name already taken by the first
/// plugin wins; the caller drops plugin commands that clash with project or user commands.
pub fn plugin_commands(plugins: &[Plugin]) -> Vec<CustomCommand> {
    let mut out: Vec<CustomCommand> = Vec::new();
    for plugin in plugins {
        for dir in &plugin.commands_dirs {
            for file in markdown_files(dir) {
                let Ok(text) = fs::read_to_string(&file) else { continue };
                let Ok(rel) = file.strip_prefix(dir) else { continue };
                let name =
                    rel.with_extension("").components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect::<Vec<_>>().join(":");
                if name.is_empty() || out.iter().any(|c| c.name == name) {
                    continue;
                }
                let (fields, body) = frontmatter::parse(&text);
                let description = fields
                    .get("description")
                    .filter(|d| !d.is_empty())
                    .cloned()
                    .or_else(|| body.lines().map(str::trim).find(|l| !l.is_empty()).map(str::to_owned))
                    .unwrap_or_default();
                out.push(CustomCommand {
                    name,
                    description,
                    argument_hint: fields.get("argument-hint").cloned().filter(|s| !s.is_empty()),
                    model: fields.get("model").cloned().filter(|s| !s.is_empty()),
                    allowed_tools: fields.get("allowed-tools").map(|v| frontmatter::list(v)).filter(|v| !v.is_empty()),
                    body: body.trim().to_owned(),
                    source: file.clone(),
                    scope: "plugin",
                });
            }
        }
    }
    out
}

/// Sub-agents from the plugins' `agents` folders. They never replace a built-in domain
/// (`from_user` is false) and never replace an agent that is already defined.
pub fn plugin_agents(plugins: &[Plugin]) -> Vec<AgentDef> {
    let mut out: Vec<AgentDef> = Vec::new();
    for plugin in plugins {
        for dir in &plugin.agents_dirs {
            for file in markdown_files(dir) {
                let Ok(text) = fs::read_to_string(&file) else { continue };
                let (fields, body) = frontmatter::parse(&text);
                let name = fields
                    .get("name")
                    .filter(|n| !n.is_empty())
                    .cloned()
                    .or_else(|| file.file_stem().map(|s| s.to_string_lossy().into_owned()));
                let Some(name) = name else { continue };
                if out.iter().any(|a| a.name == name) {
                    continue;
                }
                out.push(AgentDef {
                    name,
                    description: fields.get("description").cloned().unwrap_or_default(),
                    prompt: body.trim().to_owned(),
                    tools: fields.get("tools").map(|v| frontmatter::list(v)).filter(|t| !t.is_empty()),
                    model: fields.get("model").and_then(|m| resolve_model(m)),
                    from_user: false,
                });
            }
        }
    }
    out
}

/// Adds plugin agents, except names that `defs` already has.
pub fn merge_plugin_agents(defs: &mut Vec<AgentDef>, plugins: &[Plugin]) {
    let extra: Vec<_> = plugin_agents(plugins).into_iter().filter(|a| !defs.iter().any(|d| d.name == a.name)).collect();
    defs.extend(extra);
}

/// Adds plugin MCP servers, except names that `servers` (the user's configuration) already has.
pub fn merge_plugin_mcp(servers: &mut BTreeMap<String, McpServerConfig>, plugins: &[Plugin], warnings: &mut Vec<String>) {
    for (name, server) in plugin_mcp(plugins, warnings) {
        servers.entry(name).or_insert(server);
    }
}

/// Folders of skills that the plugins bring, each tagged with its plugin name.
pub fn plugin_skill_dirs(plugins: &[Plugin]) -> Vec<(PathBuf, SkillSource)> {
    plugins.iter().flat_map(|p| p.skills_dirs.iter().map(move |d| (d.clone(), SkillSource::Plugin(p.name.clone())))).collect()
}

/// Every skill offered to the model, with the warnings for skipped ones. Empty when skills
/// are disabled.
pub fn discover_skills(config: &Config, workspace: &Path, home: &Path, plugins: &[Plugin], trusted: bool) -> (Vec<Skill>, Vec<String>) {
    if !config.skills.enabled {
        return (Vec::new(), Vec::new());
    }
    load_skills(workspace, home, &plugin_skill_dirs(plugins), &config.skills.disabled, trusted)
}

/// Hooks from the plugins' hooks files (Claude Code format), with `${CLAUDE_PLUGIN_ROOT}`
/// expanded. Unknown events and unreadable files are skipped with a warning.
pub fn plugin_hooks(plugins: &[Plugin], warnings: &mut Vec<String>) -> BTreeMap<String, Vec<HookMatcher>> {
    let mut out: BTreeMap<String, Vec<HookMatcher>> = BTreeMap::new();
    for plugin in plugins {
        let Some(file) = &plugin.hooks_file else { continue };
        let value: Value =
            match fs::read_to_string(file).map_err(|e| e.to_string()).and_then(|t| serde_json::from_str(&t).map_err(|e| e.to_string())) {
                Ok(v) => v,
                Err(e) => {
                    warnings.push(format!("plugin `{}` hooks ignored: {e}", plugin.name));
                    continue;
                }
            };
        let Some(events) = value.get("hooks").and_then(Value::as_object) else { continue };
        for (event, matchers) in events {
            if HookEvent::parse(event).is_none() {
                warnings.push(format!("plugin `{}`: unknown hook event `{event}` skipped", plugin.name));
                continue;
            }
            for matcher in matchers.as_array().into_iter().flatten() {
                let pattern = matcher.get("matcher").and_then(Value::as_str).unwrap_or_default();
                for hook in matcher.get("hooks").and_then(Value::as_array).into_iter().flatten() {
                    let Some(command) = hook.get("command").and_then(Value::as_str) else { continue };
                    if hook.get("type").and_then(Value::as_str).unwrap_or("command") != "command" {
                        continue;
                    }
                    out.entry(event.clone()).or_default().push(HookMatcher {
                        matcher: pattern.to_owned(),
                        command: crusty_session::plugins::expand_plugin_root_in_command(command, &plugin.root),
                        timeout_secs: hook.get("timeout").and_then(Value::as_u64).unwrap_or(HookMatcher::default().timeout_secs),
                    });
                }
            }
        }
    }
    out
}

/// MCP servers from the plugins' server files, with `${CLAUDE_PLUGIN_ROOT}` expanded in the
/// command, arguments, environment, URL and headers.
pub fn plugin_mcp(plugins: &[Plugin], warnings: &mut Vec<String>) -> BTreeMap<String, McpServerConfig> {
    let mut out = BTreeMap::new();
    for plugin in plugins {
        let Some(file) = &plugin.mcp_file else { continue };
        match crusty_mcp::config::load_server_file(file) {
            Ok(servers) => {
                for (name, s) in servers {
                    out.entry(name).or_insert_with(|| expand_server(s, &plugin.root));
                }
            }
            Err(e) => warnings.push(format!("plugin `{}` MCP servers ignored: {e}", plugin.name)),
        }
    }
    out
}

fn expand_server(mut server: McpServerConfig, root: &Path) -> McpServerConfig {
    server.command = server.command.map(|c| expand_plugin_root(&c, root));
    server.args = server.args.iter().map(|a| expand_plugin_root(a, root)).collect();
    server.env = server.env.iter().map(|(k, v)| (k.clone(), expand_plugin_root(v, root))).collect();
    server.url = server.url.map(|u| expand_plugin_root(&u, root));
    server.headers = server.headers.iter().map(|(k, v)| (k.clone(), expand_plugin_root(v, root))).collect();
    server
}

/// `name version (user|project)`, one line of the session context.
pub fn plugin_line(plugin: &Plugin) -> String {
    let source = match plugin.source {
        PluginSource::User => "user",
        PluginSource::Project => "project",
    };
    format!("{} {} ({source})", plugin.name, plugin.version)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plugin(name: &str, root: &Path) -> Plugin {
        Plugin {
            name: name.into(),
            version: "1.0.0".into(),
            description: String::new(),
            root: root.to_path_buf(),
            commands_dirs: Vec::new(),
            agents_dirs: Vec::new(),
            skills_dirs: Vec::new(),
            hooks_file: None,
            mcp_file: None,
            source: PluginSource::User,
        }
    }

    fn write(path: &Path, text: &str) {
        fs::create_dir_all(path.parent().expect("parent folder")).expect("create folders");
        fs::write(path, text).expect("write file");
    }

    #[test]
    fn commands_are_named_by_folder_and_first_plugin_wins() {
        let dir = tempfile::tempdir().expect("temp dir");
        let (a, b) = (dir.path().join("a"), dir.path().join("b"));
        write(&a.join("commands/sub/deploy.md"), "---\ndescription: Ship it\nargument-hint: <env>\nmodel: fast\n---\nRun the deploy.\n");
        write(&a.join("commands/review.md"), "Review the diff.\n\nMore.");
        write(&b.join("commands/review.md"), "Other review.");
        write(&b.join("commands/extra.md"), "---\ndescription:\n---\nFirst body line\n");
        let mut pa = plugin("a", &a);
        pa.commands_dirs = vec![a.join("commands")];
        let mut pb = plugin("b", &b);
        pb.commands_dirs = vec![b.join("commands")];

        let commands = plugin_commands(&[pa, pb]);
        let names: Vec<&str> = commands.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["review", "sub:deploy", "extra"]);
        let deploy = &commands[1];
        assert_eq!(deploy.description, "Ship it");
        assert_eq!(deploy.argument_hint.as_deref(), Some("<env>"));
        assert_eq!(deploy.model.as_deref(), Some("fast"));
        assert_eq!(deploy.body, "Run the deploy.");
        assert_eq!(deploy.scope, "plugin");
        assert_eq!(commands[0].description, "Review the diff.");
        assert_eq!(commands[0].body, "Review the diff.\n\nMore.");
        assert_eq!(commands[2].description, "First body line");
    }

    #[test]
    fn agents_take_their_name_from_frontmatter_or_file_and_first_plugin_wins() {
        let dir = tempfile::tempdir().expect("temp dir");
        let (a, b) = (dir.path().join("a"), dir.path().join("b"));
        write(&a.join("agents/reviewer.md"), "---\nname: reviewer\ndescription: Reviews code\n---\nYou review.\n");
        write(&a.join("agents/helper.md"), "Help.");
        write(&b.join("agents/reviewer.md"), "---\nname: reviewer\n---\nDuplicate.\n");
        let mut pa = plugin("a", &a);
        pa.agents_dirs = vec![a.join("agents")];
        let mut pb = plugin("b", &b);
        pb.agents_dirs = vec![b.join("agents")];

        let agents = plugin_agents(&[pa, pb]);
        let names: Vec<&str> = agents.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, ["helper", "reviewer"]);
        let reviewer = &agents[1];
        assert_eq!(reviewer.description, "Reviews code");
        assert_eq!(reviewer.prompt, "You review.");
        assert!(!reviewer.from_user);
    }

    #[test]
    fn existing_agents_win_over_plugin_agents() {
        let dir = tempfile::tempdir().expect("temp dir");
        let a = dir.path().join("a");
        write(&a.join("agents/reviewer.md"), "---\ndescription: Plugin reviewer\n---\nplugin prompt\n");
        let mut pa = plugin("a", &a);
        pa.agents_dirs = vec![a.join("agents")];
        let mut defs = vec![AgentDef {
            name: "reviewer".into(),
            description: "Mine".into(),
            prompt: "mine".into(),
            tools: None,
            model: None,
            from_user: true,
        }];

        merge_plugin_agents(&mut defs, &[pa]);
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].description, "Mine");
    }

    #[test]
    fn skill_dirs_carry_the_plugin_name() {
        let mut p = plugin("tools", Path::new("/unused"));
        p.skills_dirs = vec![PathBuf::from("/x/skills")];

        let dirs = plugin_skill_dirs(&[p]);
        assert_eq!(dirs.len(), 1);
        assert_eq!(dirs[0].0, PathBuf::from("/x/skills"));
        assert!(matches!(&dirs[0].1, SkillSource::Plugin(name) if name == "tools"));
    }

    #[test]
    fn hooks_expand_plugin_root_and_skip_unusable_entries() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = dir.path().join("p");
        write(
            &root.join("hooks/hooks.json"),
            r#"{"hooks": {
                "PreToolUse": [{"matcher": "Bash", "hooks": [
                    {"type": "command", "command": "${CLAUDE_PLUGIN_ROOT}/check.sh", "timeout": 7},
                    {"type": "prompt", "prompt": "not a command"}
                ]}],
                "Bogus": [{"hooks": [{"command": "x"}]}]
            }}"#,
        );
        let mut p = plugin("p", &root);
        p.hooks_file = Some(root.join("hooks/hooks.json"));
        let mut bad = plugin("bad", &root);
        bad.hooks_file = Some(root.join("missing.json"));
        let mut warnings = Vec::new();

        let hooks = plugin_hooks(&[p, bad], &mut warnings);
        let pre = &hooks["PreToolUse"];
        assert_eq!(pre.len(), 1);
        assert_eq!(pre[0].matcher, "Bash");
        // Hook commands run under a shell: the plugin folder is quoted so it stays one word.
        assert_eq!(pre[0].command, format!("'{}'/check.sh", root.display()));
        assert_eq!(pre[0].timeout_secs, 7);
        assert!(!hooks.contains_key("Bogus"));
        assert!(warnings.iter().any(|w| w.contains("Bogus")), "{warnings:?}");
        assert!(warnings.iter().any(|w| w.contains("`bad`")), "{warnings:?}");
    }

    #[test]
    fn plugin_mcp_expands_root_and_first_plugin_wins() {
        let dir = tempfile::tempdir().expect("temp dir");
        let (a, b) = (dir.path().join("a"), dir.path().join("b"));
        write(
            &a.join(".mcp.json"),
            r#"{"mcpServers": {"fs": {"command": "${CLAUDE_PLUGIN_ROOT}/bin/fs", "args": ["${CLAUDE_PLUGIN_ROOT}/a.js"], "env": {"DATA": "${CLAUDE_PLUGIN_ROOT}/data"}}}}"#,
        );
        write(&b.join(".mcp.json"), r#"{"mcpServers": {"fs": {"command": "other"}, "extra": {"command": "e"}}}"#);
        let mut pa = plugin("a", &a);
        pa.mcp_file = Some(a.join(".mcp.json"));
        let mut pb = plugin("b", &b);
        pb.mcp_file = Some(b.join(".mcp.json"));
        let mut bad = plugin("bad", &b);
        bad.mcp_file = Some(b.join("missing.json"));
        let mut warnings = Vec::new();

        let servers = plugin_mcp(&[pa, pb, bad], &mut warnings);
        let fs_server = &servers["fs"];
        assert_eq!(fs_server.command, Some(expand_plugin_root("${CLAUDE_PLUGIN_ROOT}/bin/fs", &a)));
        assert_eq!(fs_server.args, vec![expand_plugin_root("${CLAUDE_PLUGIN_ROOT}/a.js", &a)]);
        assert_eq!(fs_server.env.get("DATA"), Some(&expand_plugin_root("${CLAUDE_PLUGIN_ROOT}/data", &a)));
        assert!(servers.contains_key("extra"));
        assert!(warnings.iter().any(|w| w.contains("`bad`")), "{warnings:?}");
    }

    #[test]
    fn user_mcp_servers_win_over_plugin_servers_of_the_same_name() {
        let dir = tempfile::tempdir().expect("temp dir");
        let a = dir.path().join("a");
        write(&a.join(".mcp.json"), r#"{"mcpServers": {"fs": {"command": "plugin-fs"}, "extra": {"command": "e"}}}"#);
        let mut pa = plugin("a", &a);
        pa.mcp_file = Some(a.join(".mcp.json"));
        let mut servers = BTreeMap::new();
        servers.insert("fs".to_owned(), McpServerConfig { command: Some("mine".into()), ..Default::default() });
        let mut warnings = Vec::new();

        merge_plugin_mcp(&mut servers, &[pa], &mut warnings);
        assert_eq!(servers["fs"].command.as_deref(), Some("mine"));
        assert!(servers.contains_key("extra"));
        assert!(warnings.is_empty(), "{warnings:?}");
    }
}
