//! Custom sub-agents loaded from Markdown files, Claude Code compatible, and merged into the
//! domain table of the config.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use crusty_core::config::Config;

use crate::frontmatter;
use crate::markdown_files;

/// A sub-agent defined by a Markdown file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentDef {
    pub name: String,
    pub description: String,
    /// System prompt: the file body without frontmatter.
    pub prompt: String,
    /// Allowed tools; `None` means the default set.
    pub tools: Option<Vec<String>>,
    /// Resolved model, possibly `provider:model`; `None` inherits the executor.
    pub model: Option<String>,
    /// Loaded from the user's home directory, the only source allowed to replace a built-in domain.
    pub from_user: bool,
}

/// Domains shipped with crusty. A project agent may not replace one of them.
const BUILTIN_DOMAINS: [&str; 10] =
    ["coder", "tester", "reviewer", "architect", "security", "designer", "graphiste", "web_researcher", "documentation", "scout"];

/// Tools a new agent gets when its file has no `tools:` line.
const READ_ONLY_TOOLS: [&str; 7] = ["Read", "Glob", "Grep", "LS", "Graph", "Retrieve", "TodoWrite"];

/// Loads agents from `project/.crusty/agents`, `project/.claude/agents`, `home/.crusty/agents`
/// and `home/.claude/agents`. Project agents load only when `trusted`; user agents always load.
/// On a name clash the first source wins, so project agents override user agents.
pub fn load_agent_files(project: &Path, home: &Path, trusted: bool) -> Vec<AgentDef> {
    let dirs = [
        (project.join(".crusty").join("agents"), false),
        (project.join(".claude").join("agents"), false),
        (home.join(".crusty").join("agents"), true),
        (home.join(".claude").join("agents"), true),
    ];
    let mut agents: BTreeMap<String, AgentDef> = BTreeMap::new();
    for (dir, user) in dirs.into_iter().filter(|(_, user)| trusted || *user) {
        for file in markdown_files(&dir, false) {
            if let Some(agent) = load_one(&file, user) {
                agents.entry(agent.name.clone()).or_insert(agent);
            }
        }
    }
    agents.into_values().collect()
}

fn load_one(file: &Path, from_user: bool) -> Option<AgentDef> {
    let text = fs::read_to_string(file).ok()?;
    let (fields, body) = frontmatter::parse(&text);
    let name =
        fields.get("name").filter(|n| !n.is_empty()).cloned().or_else(|| file.file_stem().map(|s| s.to_string_lossy().into_owned()))?;
    Some(AgentDef {
        name,
        description: fields.get("description").cloned().unwrap_or_default(),
        prompt: body.trim().to_owned(),
        tools: fields.get("tools").map(|v| frontmatter::list(v)).filter(|t| !t.is_empty()),
        model: fields.get("model").and_then(|m| resolve_model(m)),
        from_user,
    })
}

/// Turns a model field into a model id. `inherit` and empty values give `None`. Aliases
/// (`sonnet`, `opus`, `haiku`, `fable`) map to model ids and anything else is kept verbatim.
/// A `provider:model` value keeps its provider, with the model part resolved the same way.
pub fn resolve_model(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() || raw.eq_ignore_ascii_case("inherit") {
        return None;
    }
    Some(match raw.split_once(':') {
        Some((provider, model)) if !provider.is_empty() && !model.is_empty() => {
            format!("{provider}:{}", alias(model))
        }
        _ => alias(raw).to_owned(),
    })
}

fn alias(name: &str) -> &str {
    match name.to_ascii_lowercase().as_str() {
        "sonnet" => "claude-sonnet-5-5",
        "opus" => "claude-opus-5-5",
        "haiku" => "claude-haiku-5-5",
        "fable" => "claude-fable-5-1",
        _ => name,
    }
}

/// Adds each agent to `config.domains`, replacing a domain of the same name. Every agent is
/// enabled; its model override is set from the agent's model (provider split off), or cleared
/// when the agent inherits. A definition named after a built-in domain is skipped unless it
/// comes from the user's home; the returned warnings name each skipped definition. A new agent
/// without a `tools:` line gets the read-only set; a replaced domain keeps its tools.
pub fn merge_into_config(config: &mut Config, defs: &[AgentDef]) -> Vec<String> {
    let mut warnings = Vec::new();
    for def in defs {
        if BUILTIN_DOMAINS.contains(&def.name.as_str()) && !def.from_user {
            warnings.push(format!("agent `{}` skipped: it cannot replace a built-in domain from a project", def.name));
            continue;
        }
        let is_new = !config.domains.contains_key(&def.name);
        let domain = config.domains.entry(def.name.clone()).or_default();
        domain.enabled = Some(true);
        domain.description = def.description.clone();
        domain.prompt = def.prompt.clone();
        domain.tools = match (&def.tools, is_new) {
            (Some(tools), _) => Some(tools.clone()),
            (None, true) => Some(READ_ONLY_TOOLS.iter().map(|t| (*t).to_owned()).collect()),
            (None, false) => domain.tools.clone(),
        };
        let (provider, model) = match def.model.as_deref() {
            Some(m) => match m.split_once(':') {
                Some((p, rest)) if !p.is_empty() && !rest.is_empty() => (Some(p.to_owned()), Some(rest.to_owned())),
                _ => (None, Some(m.to_owned())),
            },
            None => (None, None),
        };
        domain.model.provider = provider;
        domain.model.model = model;
    }
    warnings
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn write(path: &Path, text: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    #[test]
    fn loads_agents_with_precedence_and_defaults() {
        let root = tempdir().unwrap();
        let home = root.path().join("home");
        let project = root.path().join("project");
        write(
            &project.join(".claude/agents/reviewer.md"),
            "---\ndescription: Reviews code\ntools: [Read, Grep]\nmodel: opus\n---\nYou review.\n",
        );
        write(&home.join(".claude/agents/reviewer.md"), "shadowed");
        write(&home.join(".crusty/agents/planner.md"), "---\nmodel: inherit\n---\nPlan things.");
        write(&home.join(".crusty/agents/notes.txt"), "not an agent");

        let agents = load_agent_files(&project, &home, true);
        let names: Vec<&str> = agents.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, ["planner", "reviewer"]);

        let reviewer = &agents[1];
        assert_eq!(reviewer.description, "Reviews code");
        assert_eq!(reviewer.prompt, "You review.");
        assert_eq!(reviewer.tools, Some(vec!["Read".to_owned(), "Grep".to_owned()]));
        assert_eq!(reviewer.model.as_deref(), Some("claude-opus-5-5"));

        let planner = &agents[0];
        assert_eq!(planner.model, None);
        assert_eq!(planner.tools, None);
    }

    #[test]
    fn model_aliases_and_provider_prefix() {
        assert_eq!(resolve_model("sonnet").as_deref(), Some("claude-sonnet-5-5"));
        assert_eq!(resolve_model("haiku").as_deref(), Some("claude-haiku-5-5"));
        assert_eq!(resolve_model("fable").as_deref(), Some("claude-fable-5-1"));
        assert_eq!(resolve_model("gpt-x-1"), Some("gpt-x-1".into()));
        assert_eq!(resolve_model("openai:opus").as_deref(), Some("openai:claude-opus-5-5"));
        assert_eq!(resolve_model("inherit"), None);
        assert_eq!(resolve_model("  "), None);
    }

    fn def(name: &str, tools: Option<Vec<String>>, model: Option<&str>, from_user: bool) -> AgentDef {
        AgentDef { name: name.into(), description: "d".into(), prompt: "p".into(), tools, model: model.map(str::to_owned), from_user }
    }

    #[test]
    fn merge_sets_domains_and_splits_provider() {
        let mut config = Config::default();
        let defs = vec![def("helper", Some(vec!["Read".into()]), Some("openai:gpt-x"), false), def("planner", None, None, false)];
        assert!(merge_into_config(&mut config, &defs).is_empty());

        let helper = &config.domains["helper"];
        assert_eq!(helper.enabled, Some(true));
        assert_eq!(helper.prompt, "p");
        assert_eq!(helper.tools, Some(vec!["Read".to_owned()]));
        assert_eq!(helper.model.provider.as_deref(), Some("openai"));
        assert_eq!(helper.model.model.as_deref(), Some("gpt-x"));
        assert_eq!(config.domains["planner"].model.model, None);
    }

    #[test]
    fn new_agent_without_tools_is_read_only() {
        let mut config = Config::default();
        merge_into_config(&mut config, &[def("planner", None, None, false)]);
        let tools = config.domains["planner"].tools.clone().unwrap();
        assert_eq!(tools, ["Read", "Glob", "Grep", "LS", "Graph", "Retrieve", "TodoWrite"]);
    }

    #[test]
    fn project_agent_cannot_replace_builtin_domain() {
        let mut config = Config::default();
        let before = config.domains["reviewer"].clone();
        let warnings = merge_into_config(&mut config, &[def("reviewer", Some(vec!["Bash".into()]), None, false)]);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("reviewer"));
        assert_eq!(config.domains["reviewer"], before);

        // The same definition from the user's home replaces it.
        let warnings = merge_into_config(&mut config, &[def("reviewer", Some(vec!["Read".into()]), None, true)]);
        assert!(warnings.is_empty());
        assert_eq!(config.domains["reviewer"].tools, Some(vec!["Read".to_owned()]));
    }

    #[test]
    fn untrusted_project_agents_are_not_loaded() {
        let root = tempdir().unwrap();
        let home = root.path().join("home");
        let project = root.path().join("project");
        write(&project.join(".crusty/agents/evil.md"), "---\nname: evil\n---\nDo bad things.");
        write(&home.join(".crusty/agents/mine.md"), "---\nname: mine\n---\nMine.");
        let names: Vec<String> = load_agent_files(&project, &home, false).into_iter().map(|a| a.name).collect();
        assert_eq!(names, ["mine"]);
    }
}
