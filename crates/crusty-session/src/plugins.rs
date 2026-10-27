//! Claude Code plugins: folders with a manifest bringing commands, agents, skills, hooks and
//! MCP servers.
//!
//! The manifest is `.claude-plugin/plugin.json` (or `.crusty-plugin/plugin.json`, or
//! `plugin.json` at the root). Plugins live in `~/.crusty/plugins/<name>` and, in trusted
//! projects only, `<project>/.crusty/plugins/<name>`. Project plugins come first, then user
//! plugins; the first with a given name wins. The loaders return data only: wiring into
//! commands, agents, skills, hooks and MCP happens elsewhere.

use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use serde::Deserialize;
use uuid::Uuid;

/// Longest plugin name accepted.
const MAX_NAME: usize = 64;

/// Where a plugin was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginSource {
    User,
    Project,
}

/// One loaded plugin. Directories and files are only listed when they exist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plugin {
    pub name: String,
    pub version: String,
    pub description: String,
    /// The plugin folder; `${CLAUDE_PLUGIN_ROOT}` expands to it.
    pub root: PathBuf,
    pub commands_dirs: Vec<PathBuf>,
    pub agents_dirs: Vec<PathBuf>,
    pub skills_dirs: Vec<PathBuf>,
    pub hooks_file: Option<PathBuf>,
    pub mcp_file: Option<PathBuf>,
    pub source: PluginSource,
}

/// A manifest field: one relative path, or several.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum PathSpec {
    One(String),
    Many(Vec<String>),
}

/// The fields of `plugin.json` that this loader reads. Unknown fields are ignored.
#[derive(Debug, Deserialize)]
struct Manifest {
    name: String,
    #[serde(default)]
    version: String,
    #[serde(default)]
    description: String,
    commands: Option<PathSpec>,
    agents: Option<PathSpec>,
    skills: Option<PathSpec>,
    hooks: Option<PathSpec>,
    #[serde(rename = "mcpServers")]
    mcp_servers: Option<PathSpec>,
}

/// Loads every valid plugin from the user folder and, when `project_trusted`, the project
/// folder. Returns the plugins and one warning per skipped or shadowed one.
///
/// `disabled` names are never loaded.
pub fn load_plugins(project: &Path, home: &Path, project_trusted: bool, disabled: &[String]) -> (Vec<Plugin>, Vec<String>) {
    let mut roots: Vec<(PathBuf, PluginSource)> = Vec::new();
    if project_trusted {
        roots.push((project.join(".crusty/plugins"), PluginSource::Project));
    }
    roots.push((plugins_dir(home), PluginSource::User));

    let mut plugins: Vec<Plugin> = Vec::new();
    let mut warnings = Vec::new();
    for (dir, source) in roots {
        for folder in sorted_dirs(&dir) {
            match load_one(&folder, source) {
                Ok(plugin) if disabled.contains(&plugin.name) => {}
                Ok(plugin) => {
                    if plugins.iter().any(|p| p.name == plugin.name) {
                        warnings.push(format!("plugin `{}` at {} is shadowed by an earlier one", plugin.name, folder.display()));
                    } else {
                        plugins.push(plugin);
                    }
                }
                Err(e) => warnings.push(format!("plugin at {}: {e}", folder.display())),
            }
        }
    }
    (plugins, warnings)
}

/// Replaces `${CLAUDE_PLUGIN_ROOT}` and `${CRUSTY_PLUGIN_ROOT}` in a hook command or MCP
/// argument with the plugin folder.
pub fn expand_plugin_root(text: &str, root: &Path) -> String {
    let root = root.display().to_string();
    text.replace("${CLAUDE_PLUGIN_ROOT}", &root).replace("${CRUSTY_PLUGIN_ROOT}", &root)
}

/// Like [`expand_plugin_root`], for a hook command: hooks run under a shell, so the folder is
/// single-quoted and a path with spaces or quotes stays one word. Do not use it for argument lists.
pub fn expand_plugin_root_in_command(text: &str, root: &Path) -> String {
    let quoted = format!("'{}'", root.display().to_string().replace('\'', r"'\''"));
    text.replace("${CLAUDE_PLUGIN_ROOT}", &quoted).replace("${CRUSTY_PLUGIN_ROOT}", &quoted)
}

/// Copies a plugin folder into `~/.crusty/plugins/<name>` after validating its manifest.
///
/// An existing plugin of that name is replaced only when `force` is set. `.git` folders and
/// symbolic links are not copied.
pub fn install_from_dir(src: &Path, home: &Path, force: bool) -> Result<Plugin, String> {
    let plugin = load_one(src, PluginSource::User)?;
    let dest = plugins_dir(home).join(&plugin.name);
    if dest.exists() {
        if !force {
            return Err(format!("plugin `{}` is already installed (use force to replace it)", plugin.name));
        }
        if same_dir(src, &dest) {
            return Err(format!("plugin `{}` is already installed from this folder", plugin.name));
        }
        fs::remove_dir_all(&dest).map_err(|e| e.to_string())?;
    }
    if let Err(e) = copy_tree(src, &dest) {
        let _ = fs::remove_dir_all(&dest);
        return Err(e.to_string());
    }
    load_one(&dest, PluginSource::User)
}

/// Clones `url` with `git clone --depth 1` into a temporary folder, then installs it as
/// [`install_from_dir`] does. `git` is the path of the git executable; no shell is used.
pub fn install_from_git(url: &str, home: &Path, git: &Path, force: bool) -> Result<Plugin, String> {
    validate_git_url(url)?;
    let tmp = std::env::temp_dir().join(format!("crusty-plugin-{}", Uuid::new_v4().simple()));
    fs::create_dir_all(&tmp).map_err(|e| e.to_string())?;
    let checkout = tmp.join("plugin");
    let output = Command::new(git)
        .args(["clone", "--depth", "1", "--", url])
        .arg(&checkout)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .map_err(|e| format!("could not run git: {e}"));
    let result = match output {
        Ok(out) if out.status.success() => install_from_dir(&checkout, home, force),
        Ok(out) => Err(format!("git clone failed: {}", String::from_utf8_lossy(&out.stderr).trim())),
        Err(e) => Err(e),
    };
    let _ = fs::remove_dir_all(&tmp);
    result
}

/// Accepts only `https://host/...` (without credentials) and `git@host:path` URLs.
pub fn validate_git_url(url: &str) -> Result<(), String> {
    if url.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err("the URL contains whitespace or control characters".into());
    }
    if let Some(rest) = url.strip_prefix("https://") {
        let host = rest.split('/').next().unwrap_or("");
        if host.is_empty() || host.contains('@') {
            return Err("https URLs need a host and must not contain credentials".into());
        }
        return Ok(());
    }
    if let Some(rest) = url.strip_prefix("git@")
        && rest.split_once(':').is_some_and(|(host, path)| !host.is_empty() && !path.is_empty())
    {
        return Ok(());
    }
    Err("only https:// and git@ URLs are accepted".into())
}

/// Deletes the installed plugin `name` from `~/.crusty/plugins`.
pub fn remove(name: &str, home: &Path) -> Result<(), String> {
    if !valid_name(name) {
        return Err(format!("invalid plugin name `{name}`"));
    }
    let dir = plugins_dir(home).join(name);
    if !dir.is_dir() {
        return Err(format!("plugin `{name}` is not installed"));
    }
    fs::remove_dir_all(&dir).map_err(|e| e.to_string())
}

/// The user's installed plugins (project plugins are never listed here).
pub fn list(home: &Path) -> Vec<Plugin> {
    load_plugins(home, home, false, &[]).0
}

fn plugins_dir(home: &Path) -> PathBuf {
    home.join(".crusty/plugins")
}

/// Reads and validates the plugin in `root`.
fn load_one(root: &Path, source: PluginSource) -> Result<Plugin, String> {
    let manifest_path = locate_manifest(root).ok_or("no .claude-plugin/plugin.json or plugin.json")?;
    let text = fs::read_to_string(&manifest_path).map_err(|e| e.to_string())?;
    let m: Manifest = serde_json::from_str(&text).map_err(|e| format!("invalid plugin.json: {e}"))?;
    if !valid_name(&m.name) {
        return Err(format!("invalid plugin name `{}`", m.name));
    }
    let hooks = declared(root, m.hooks, "hooks/hooks.json")?.into_iter().find(|p| p.is_file());
    let mcp = declared(root, m.mcp_servers, ".mcp.json")?.into_iter().find(|p| p.is_file());
    Ok(Plugin {
        name: m.name,
        version: m.version,
        description: m.description,
        root: root.to_path_buf(),
        commands_dirs: existing_dirs(root, m.commands, "commands")?,
        agents_dirs: existing_dirs(root, m.agents, "agents")?,
        skills_dirs: existing_dirs(root, m.skills, "skills")?,
        hooks_file: hooks,
        mcp_file: mcp,
        source,
    })
}

fn locate_manifest(root: &Path) -> Option<PathBuf> {
    [".claude-plugin/plugin.json", ".crusty-plugin/plugin.json", "plugin.json"].iter().map(|rel| root.join(rel)).find(|p| p.is_file())
}

/// The paths a manifest field names, or the default when the field is absent.
fn declared(root: &Path, spec: Option<PathSpec>, default: &str) -> Result<Vec<PathBuf>, String> {
    let rels = match spec {
        Some(PathSpec::One(rel)) => vec![rel],
        Some(PathSpec::Many(rels)) => rels,
        None => vec![default.to_owned()],
    };
    rels.iter().map(|rel| contained(root, rel)).collect()
}

/// The declared or default directories that exist.
fn existing_dirs(root: &Path, spec: Option<PathSpec>, default: &str) -> Result<Vec<PathBuf>, String> {
    Ok(declared(root, spec, default)?.into_iter().filter(|p| p.is_dir()).collect())
}

/// `root/rel`, refused when `rel` is absolute, contains `..`, or resolves outside `root`.
fn contained(root: &Path, rel: &str) -> Result<PathBuf, String> {
    let rel_path = Path::new(rel);
    let plain = rel_path.components().all(|c| matches!(c, Component::Normal(_) | Component::CurDir));
    if rel.is_empty() || !plain {
        return Err(format!("path `{rel}` must stay inside the plugin folder"));
    }
    let full = root.join(rel_path);
    if let (Ok(real), Ok(base)) = (full.canonicalize(), root.canonicalize())
        && !real.starts_with(&base)
    {
        return Err(format!("path `{rel}` leads outside the plugin folder"));
    }
    Ok(full)
}

/// Plugin names are used as folder names, so they are kept to a safe subset.
fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME
        && !name.starts_with('.')
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

fn sorted_dirs(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = entries.filter_map(|e| e.ok().map(|e| e.path())).filter(|p| p.is_dir()).collect();
    paths.sort();
    paths
}

fn same_dir(a: &Path, b: &Path) -> bool {
    matches!((a.canonicalize(), b.canonicalize()), (Ok(x), Ok(y)) if x == y)
}

/// Copies `src` to `dst`, skipping `.git` folders and symbolic links.
fn copy_tree(src: &Path, dst: &Path) -> io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if file_type.is_symlink() || entry.file_name().to_str() == Some(".git") {
            continue;
        }
        let target = dst.join(entry.file_name());
        if file_type.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, text: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("mkdir");
        }
        fs::write(path, text).expect("write");
    }

    #[test]
    fn manifests_in_both_locations_and_defaults() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let plugins = tmp.path().join("home/.crusty/plugins");
        let p1 = plugins.join("p1");
        write(&p1.join(".claude-plugin/plugin.json"), r#"{"name":"p1","version":"1.0.0","description":"first"}"#);
        fs::create_dir_all(p1.join("commands")).expect("mkdir");
        fs::create_dir_all(p1.join("skills")).expect("mkdir");
        write(&p1.join("hooks/hooks.json"), "{}");
        write(&p1.join(".mcp.json"), "{}");
        let p2 = plugins.join("p2");
        write(&p2.join("plugin.json"), r#"{"name":"p2","commands":["cmds","missing"]}"#);
        fs::create_dir_all(p2.join("cmds")).expect("mkdir");

        let (loaded, warnings) = load_plugins(tmp.path(), &tmp.path().join("home"), false, &[]);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(loaded.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(), ["p1", "p2"]);
        let first = &loaded[0];
        assert_eq!(first.version, "1.0.0");
        assert_eq!(first.commands_dirs, vec![p1.join("commands")]);
        assert!(first.agents_dirs.is_empty());
        assert_eq!(first.skills_dirs, vec![p1.join("skills")]);
        assert_eq!(first.hooks_file, Some(p1.join("hooks/hooks.json")));
        assert_eq!(first.mcp_file, Some(p1.join(".mcp.json")));
        assert_eq!(loaded[1].commands_dirs, vec![p2.join("cmds")]);
        assert_eq!(loaded[1].hooks_file, None);
        assert_eq!(loaded[1].source, PluginSource::User);
    }

    #[test]
    fn paths_escaping_the_plugin_are_refused() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path().join("home");
        write(&home.join(".crusty/plugins/evil/plugin.json"), r#"{"name":"evil","commands":"../outside"}"#);
        write(&home.join(".crusty/plugins/good/plugin.json"), r#"{"name":"good"}"#);
        let (loaded, warnings) = load_plugins(tmp.path(), &home, false, &[]);
        assert_eq!(loaded.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(), ["good"]);
        assert!(warnings.iter().any(|w| w.contains("must stay inside")), "{warnings:?}");
    }

    #[test]
    fn untrusted_project_plugins_are_skipped() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let project = tmp.path().join("project");
        write(&project.join(".crusty/plugins/pp/plugin.json"), r#"{"name":"pp"}"#);
        let home = tmp.path().join("home");
        assert!(load_plugins(&project, &home, false, &[]).0.is_empty());
        let (loaded, _) = load_plugins(&project, &home, true, &[]);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].source, PluginSource::Project);
        assert!(load_plugins(&project, &home, true, &["pp".to_owned()]).0.is_empty());
    }

    #[test]
    fn install_from_dir_and_remove() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let src = tmp.path().join("src/mytool");
        write(&src.join("plugin.json"), r#"{"name":"mytool","version":"0.1.0"}"#);
        write(&src.join("commands/hi.md"), "hi");
        write(&src.join(".git/config"), "x");
        let home = tmp.path().join("home");

        let plugin = install_from_dir(&src, &home, false).expect("install");
        assert_eq!(plugin.root, home.join(".crusty/plugins/mytool"));
        assert!(home.join(".crusty/plugins/mytool/commands/hi.md").is_file());
        assert!(!home.join(".crusty/plugins/mytool/.git").exists());
        assert!(install_from_dir(&src, &home, false).is_err());
        assert!(install_from_dir(&src, &home, true).is_ok());
        assert_eq!(list(&home).len(), 1);

        remove("mytool", &home).expect("remove");
        assert!(!home.join(".crusty/plugins/mytool").exists());
        assert!(remove("mytool", &home).is_err());
        assert!(remove("../x", &home).is_err());
    }

    #[test]
    fn git_urls_are_validated_without_cloning() {
        assert!(validate_git_url("https://github.com/a/b.git").is_ok());
        assert!(validate_git_url("git@github.com:a/b.git").is_ok());
        for bad in ["http://x/y", "--upload-pack=touch x", "https://user:pw@host/x", "ext::sh -c x", "git@host", "https://a b"] {
            assert!(validate_git_url(bad).is_err(), "{bad} should be refused");
        }
    }

    #[test]
    fn root_placeholders_expand() {
        assert_eq!(expand_plugin_root("${CLAUDE_PLUGIN_ROOT}/run.sh ${CRUSTY_PLUGIN_ROOT}", Path::new("p")), "p/run.sh p");
    }

    #[test]
    fn hook_commands_quote_the_root_and_argument_lists_do_not() {
        let root = Path::new("/home/u/my plug'in");
        assert_eq!(
            expand_plugin_root_in_command("${CLAUDE_PLUGIN_ROOT}/run.sh ${CRUSTY_PLUGIN_ROOT}", root),
            r"'/home/u/my plug'\''in'/run.sh '/home/u/my plug'\''in'"
        );
        assert_eq!(expand_plugin_root("${CLAUDE_PLUGIN_ROOT}/run.sh", root), "/home/u/my plug'in/run.sh");
    }
}
