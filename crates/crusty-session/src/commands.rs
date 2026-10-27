//! Custom slash commands loaded from Markdown files, Claude Code compatible.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::frontmatter;
use crate::{markdown_files, truncate_chars};

const DESCRIPTION_CHARS: usize = 80;

/// A slash command defined by a Markdown file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CustomCommand {
    /// Command name; sub-directories join with `:` (`frontend/build.md` is `frontend:build`).
    pub name: String,
    pub description: String,
    pub argument_hint: Option<String>,
    pub model: Option<String>,
    pub allowed_tools: Option<Vec<String>>,
    /// Prompt template, with the frontmatter removed.
    pub body: String,
    pub source: PathBuf,
    /// `"project"` or `"user"`.
    pub scope: &'static str,
}

impl CustomCommand {
    /// Fills the template: `$ARGUMENTS` becomes the raw argument text and `$1`..`$9` the
    /// positional arguments (whitespace-separated, with `"..."` or `'...'` grouping).
    /// Missing positions become empty. Substituted text is not rescanned.
    pub fn render(&self, args: &str) -> String {
        let args = args.trim();
        let positional = split_args(args);
        let mut out = String::with_capacity(self.body.len());
        let mut rest = self.body.as_str();
        while let Some(pos) = rest.find('$') {
            out.push_str(&rest[..pos]);
            let after = &rest[pos + 1..];
            if let Some(tail) = after.strip_prefix("ARGUMENTS") {
                out.push_str(args);
                rest = tail;
            } else if let Some(n) = after.chars().next().and_then(|c| c.to_digit(10)).filter(|n| (1..=9).contains(n)) {
                out.push_str(positional.get(n as usize - 1).map_or("", String::as_str));
                rest = &after[1..];
            } else {
                out.push('$');
                rest = after;
            }
        }
        out.push_str(rest);
        out
    }
}

/// Loads commands from `project/.crusty/commands`, `project/.claude/commands`,
/// `home/.crusty/commands` and `home/.claude/commands`. On a name clash the first source wins,
/// so project commands override user commands.
pub fn load_commands(project: &Path, home: &Path) -> Vec<CustomCommand> {
    let sources = [
        (project.join(".crusty").join("commands"), "project"),
        (project.join(".claude").join("commands"), "project"),
        (home.join(".crusty").join("commands"), "user"),
        (home.join(".claude").join("commands"), "user"),
    ];
    let mut commands: BTreeMap<String, CustomCommand> = BTreeMap::new();
    for (dir, scope) in sources {
        for file in markdown_files(&dir, true) {
            if let Some(command) = load_one(&dir, &file, scope) {
                commands.entry(command.name.clone()).or_insert(command);
            }
        }
    }
    commands.into_values().collect()
}

fn load_one(root: &Path, file: &Path, scope: &'static str) -> Option<CustomCommand> {
    let text = fs::read_to_string(file).ok()?;
    let (fields, body) = frontmatter::parse(&text);
    let name = file
        .strip_prefix(root)
        .ok()?
        .with_extension("")
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(":");
    let description = fields
        .get("description")
        .filter(|d| !d.is_empty())
        .cloned()
        .or_else(|| body.lines().map(str::trim).find(|line| !line.is_empty()).map(str::to_owned))
        .unwrap_or_default();
    Some(CustomCommand {
        name,
        description: truncate_chars(&description, DESCRIPTION_CHARS),
        argument_hint: fields.get("argument-hint").cloned().filter(|s| !s.is_empty()),
        model: fields.get("model").cloned().filter(|s| !s.is_empty()),
        allowed_tools: fields.get("allowed-tools").map(|v| frontmatter::list(v)).filter(|v| !v.is_empty()),
        body: body.trim().to_owned(),
        source: file.to_path_buf(),
        scope,
    })
}

/// Splits arguments on whitespace. A `"` or `'` group keeps its spaces and the quotes are dropped.
fn split_args(args: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut has_token = false;
    let mut quote: Option<char> = None;
    for c in args.chars() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => current.push(c),
            None if c == '"' || c == '\'' => {
                quote = Some(c);
                has_token = true;
            }
            None if c.is_whitespace() => {
                if has_token {
                    out.push(std::mem::take(&mut current));
                    has_token = false;
                }
            }
            None => {
                current.push(c);
                has_token = true;
            }
        }
    }
    if has_token {
        out.push(current);
    }
    out
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
    fn loads_namespaces_and_project_takes_precedence() {
        let root = tempdir().unwrap();
        let home = root.path().join("home");
        let project = root.path().join("project");
        write(
            &project.join(".claude/commands/frontend/build.md"),
            "---\ndescription: Build UI\nargument-hint: \"[target]\"\nmodel: sonnet\nallowed-tools: [Bash, Read]\n---\nBuild $1 with $ARGUMENTS\n",
        );
        write(&project.join(".crusty/commands/review.md"), "\n\nReview the code\nmore detail\n");
        write(&home.join(".claude/commands/review.md"), "user review");
        write(&home.join(".crusty/commands/home-only.md"), "# Home only");

        let commands = load_commands(&project, &home);
        let names: Vec<&str> = commands.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["frontend:build", "home-only", "review"]);

        let build = &commands[0];
        assert_eq!(build.description, "Build UI");
        assert_eq!(build.argument_hint.as_deref(), Some("[target]"));
        assert_eq!(build.model.as_deref(), Some("sonnet"));
        assert_eq!(build.allowed_tools, Some(vec!["Bash".to_owned(), "Read".to_owned()]));
        assert_eq!(build.scope, "project");
        assert_eq!(build.body, "Build $1 with $ARGUMENTS");

        let review = &commands[2];
        assert_eq!(review.description, "Review the code");
        assert_eq!(review.scope, "project");
        assert_eq!(review.source, project.join(".crusty/commands/review.md"));
        assert_eq!(commands[1].scope, "user");
        assert_eq!(commands[1].description, "# Home only");
    }

    #[test]
    fn render_substitutes_arguments_and_positions() {
        let cmd = CustomCommand {
            name: "t".into(),
            description: String::new(),
            argument_hint: None,
            model: None,
            allowed_tools: None,
            body: "Build $1 with $ARGUMENTS and $2! ($9)".into(),
            source: PathBuf::new(),
            scope: "project",
        };
        assert_eq!(cmd.render("\"web ui\" fast"), "Build web ui with \"web ui\" fast and fast! ()");
        assert_eq!(cmd.render("  solo  "), "Build solo with solo and ! ()");
        assert_eq!(split_args("a 'b c' \"\" d"), ["a", "b c", "", "d"]);
    }
}
