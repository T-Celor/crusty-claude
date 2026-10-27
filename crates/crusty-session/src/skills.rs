//! Agent Skills: `SKILL.md` folders (Claude Code compatible), offered to the model through
//! the `Skill` tool.
//!
//! Precedence when two skills share a name: project (`.crusty/skills`, then
//! `.claude/skills`, only in trusted projects), then user (home, same two folders), then the
//! extra directories in the order given (plugins). The first definition wins; later ones
//! produce a warning.

use std::fs;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use crusty_core::message::ToolSpec;
use crusty_core::permission::{RiskAssessment, RiskLevel};
use crusty_core::tool::{Tool, ToolCategory, ToolContext, ToolError, ToolOutput, input_opt_str, input_str};
use serde_json::{Value, json};

use crate::frontmatter;

/// Longest description accepted, in characters.
const MAX_DESCRIPTION: usize = 1024;
/// Longest skill name accepted, in characters.
const MAX_NAME: usize = 64;
/// Most bundled file entries listed by the `Skill` tool.
const MAX_LISTED: usize = 50;

/// Where a skill came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkillSource {
    Project,
    User,
    /// A plugin, by plugin name.
    Plugin(String),
}

/// One loaded skill.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    /// Lowercase letters, digits and hyphens; from `name:` or the folder name.
    pub name: String,
    /// When the model should use it; from `description:`.
    pub description: String,
    /// The `SKILL.md` file.
    pub path: PathBuf,
    /// The folder holding `SKILL.md` and its bundled resources.
    pub dir: PathBuf,
    /// From `allowed-tools:`, when present. Informational for the model.
    pub allowed_tools: Option<Vec<String>>,
    pub source: SkillSource,
}

/// Loads every valid skill. Returns the skills and one warning per skipped or shadowed one.
///
/// `project_trusted` false skips the project folders. `disabled` names are never offered.
pub fn load_skills(
    project: &Path,
    home: &Path,
    extra_dirs: &[(PathBuf, SkillSource)],
    disabled: &[String],
    project_trusted: bool,
) -> (Vec<Skill>, Vec<String>) {
    let mut roots: Vec<(PathBuf, SkillSource)> = Vec::new();
    if project_trusted {
        roots.push((project.join(".crusty/skills"), SkillSource::Project));
        roots.push((project.join(".claude/skills"), SkillSource::Project));
    }
    roots.push((home.join(".crusty/skills"), SkillSource::User));
    roots.push((home.join(".claude/skills"), SkillSource::User));
    roots.extend(extra_dirs.iter().cloned());

    let mut skills: Vec<Skill> = Vec::new();
    let mut warnings = Vec::new();
    for (root, source) in roots {
        for dir in sorted_entries(&root).into_iter().filter(|p| p.is_dir()) {
            let path = dir.join("SKILL.md");
            if !path.is_file() {
                continue;
            }
            match parse_skill(&dir, &path, source.clone()) {
                Ok(skill) if disabled.contains(&skill.name) => {}
                Ok(skill) => match skills.iter().find(|s| s.name == skill.name) {
                    Some(first) => {
                        warnings.push(format!("skill `{}` at {} is shadowed by {}", skill.name, path.display(), first.path.display()))
                    }
                    None => skills.push(skill),
                },
                Err(e) => warnings.push(format!("skill at {}: {e}", path.display())),
            }
        }
    }
    (skills, warnings)
}

/// One line per skill for the system prompt: `- name: description`.
pub fn skills_prompt(skills: &[Skill]) -> String {
    skills.iter().map(|s| format!("- {}: {}", s.name, s.description)).collect::<Vec<_>>().join("\n")
}

/// The `Skill` tool: returns a skill's instructions and lists its bundled files.
pub struct SkillTool {
    skills: Vec<Skill>,
}

impl SkillTool {
    /// A tool offering `skills`.
    pub fn new(skills: Vec<Skill>) -> Self {
        SkillTool { skills }
    }
}

#[async_trait]
impl Tool for SkillTool {
    fn name(&self) -> &str {
        "Skill"
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "Skill",
            "Load a skill from the list in the system prompt and get its instructions. `args` replaces $ARGUMENTS in the skill. The result lists the skill's bundled files, which you can read with the Read tool.",
            json!({
                "type": "object",
                "properties": {
                    "skill": {"type": "string", "description": "Skill name"},
                    "args": {"type": "string", "description": "Arguments for the skill"}
                },
                "required": ["skill"]
            }),
        )
    }

    fn category(&self) -> ToolCategory {
        ToolCategory::Read
    }

    fn assess(&self, _input: &Value, _ctx: &ToolContext) -> RiskAssessment {
        RiskAssessment::new(RiskLevel::Low, "reads a skill's instructions")
    }

    async fn call(&self, input: Value, _ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let name = input_str(&input, "skill")?;
        let skill =
            self.skills.iter().find(|s| s.name == name).ok_or_else(|| ToolError::InvalidInput(format!("unknown skill `{name}`")))?;
        let args = input_opt_str(&input, "args").unwrap_or("");
        let text = fs::read_to_string(&skill.path)?;
        let mut out = frontmatter::parse(&text).1.trim().replace("$ARGUMENTS", args);
        out.push_str(&format!("\n\nSkill directory: {}\n", skill.dir.display()));
        if let Some(tools) = &skill.allowed_tools {
            out.push_str(&format!("Allowed tools: {}\n", tools.join(", ")));
        }
        let files = bundled_files(&skill.dir);
        if files.is_empty() {
            out.push_str("Bundled files: none\n");
        } else {
            out.push_str("Bundled files:\n");
            for file in files {
                out.push_str(&format!("- {file}\n"));
            }
        }
        Ok(ToolOutput::text(out))
    }
}

fn parse_skill(dir: &Path, path: &Path, source: SkillSource) -> Result<Skill, String> {
    let text = fs::read_to_string(path).map_err(|e| e.to_string())?;
    let (fields, _) = frontmatter::parse(&text);
    let folder = file_name(dir);
    let name = fields.get("name").cloned().unwrap_or(folder);
    if !valid_name(&name) {
        return Err(format!("invalid name `{name}` (lowercase letters, digits and hyphens, up to {MAX_NAME})"));
    }
    let description = fields.get("description").map(|d| d.trim().to_owned()).unwrap_or_default();
    if description.is_empty() {
        return Err("missing description".into());
    }
    if description.chars().count() > MAX_DESCRIPTION {
        return Err(format!("description longer than {MAX_DESCRIPTION} characters"));
    }
    let allowed_tools = fields.get("allowed-tools").map(|v| frontmatter::list(v));
    Ok(Skill { name, description, path: path.to_path_buf(), dir: dir.to_path_buf(), allowed_tools, source })
}

fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.chars().count() <= MAX_NAME && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// Entries of the skill folder, two levels deep, without `SKILL.md`, capped at 50.
fn bundled_files(dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    for top in sorted_entries(dir) {
        let name = file_name(&top);
        if top.is_dir() {
            out.push(format!("{name}/"));
            for child in sorted_entries(&top) {
                let suffix = if child.is_dir() { "/" } else { "" };
                out.push(format!("{name}/{}{suffix}", file_name(&child)));
            }
        } else if name != "SKILL.md" {
            out.push(name);
        }
    }
    if out.len() > MAX_LISTED {
        let extra = out.len() - MAX_LISTED;
        out.truncate(MAX_LISTED);
        out.push(format!("... {extra} more"));
    }
    out
}

fn sorted_entries(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = entries.filter_map(|e| e.ok().map(|e| e.path())).collect();
    paths.sort();
    paths
}

fn file_name(path: &Path) -> String {
    path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_skill(root: &Path, folder: &str, body: &str) {
        let dir = root.join(folder);
        fs::create_dir_all(&dir).expect("mkdir");
        fs::write(dir.join("SKILL.md"), body).expect("write");
    }

    fn names(skills: &[Skill]) -> Vec<(String, SkillSource)> {
        skills.iter().map(|s| (s.name.clone(), s.source.clone())).collect()
    }

    #[test]
    fn project_overrides_user_and_invalid_skills_are_skipped() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (home, project) = (tmp.path().join("home"), tmp.path().join("project"));
        write_skill(&home.join(".crusty/skills"), "alpha", "---\ndescription: user alpha\n---\nbody");
        write_skill(&home.join(".claude/skills"), "beta", "---\ndescription: user beta\n---\nbody");
        write_skill(&project.join(".crusty/skills"), "alpha", "---\ndescription: project alpha\n---\nbody");
        write_skill(&project.join(".claude/skills"), "gamma", "---\nname: gamma\ndescription: project gamma\n---\n");
        write_skill(&home.join(".crusty/skills"), "Bad_Name", "---\ndescription: bad\n---\n");
        write_skill(&home.join(".claude/skills"), "nodesc", "---\nname: nodesc\n---\n");

        let (skills, warnings) = load_skills(&project, &home, &[], &[], true);
        assert_eq!(
            names(&skills),
            vec![
                ("alpha".to_owned(), SkillSource::Project),
                ("gamma".to_owned(), SkillSource::Project),
                ("beta".to_owned(), SkillSource::User),
            ]
        );
        assert_eq!(skills[0].description, "project alpha");
        assert_eq!(warnings.len(), 3, "{warnings:?}");
        assert_eq!(skills_prompt(&skills).lines().count(), 3);
    }

    #[test]
    fn untrusted_project_and_disabled_skills_are_not_offered() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (home, project) = (tmp.path().join("home"), tmp.path().join("project"));
        write_skill(&home.join(".crusty/skills"), "beta", "---\ndescription: user beta\n---\n");
        write_skill(&project.join(".crusty/skills"), "alpha", "---\ndescription: project alpha\n---\n");

        let (skills, _) = load_skills(&project, &home, &[], &[], false);
        assert_eq!(names(&skills), vec![("beta".to_owned(), SkillSource::User)]);

        let disabled = vec!["beta".to_owned()];
        let (skills, _) = load_skills(&project, &home, &[], &disabled, true);
        assert_eq!(names(&skills), vec![("alpha".to_owned(), SkillSource::Project)]);
    }

    #[tokio::test]
    async fn skill_tool_substitutes_arguments_and_lists_files() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path().join("home");
        let dir = home.join(".crusty/skills/review");
        write_skill(
            &home.join(".crusty/skills"),
            "review",
            "---\ndescription: reviews\nallowed-tools: Read, Grep\n---\nDo $ARGUMENTS now.\n",
        );
        fs::create_dir_all(dir.join("scripts")).expect("mkdir");
        fs::write(dir.join("scripts/run.sh"), "x").expect("write");
        fs::write(dir.join("reference.md"), "x").expect("write");

        let (skills, _) = load_skills(tmp.path(), &home, &[], &[], false);
        let tool = SkillTool::new(skills);
        let ctx = ToolContext::for_tests(tmp.path());
        let out = tool.call(json!({"skill": "review", "args": "the diff"}), &ctx).await.expect("call").text_content();
        assert!(out.starts_with("Do the diff now."), "{out}");
        assert!(!out.contains("description:"));
        assert!(out.contains("Allowed tools: Read, Grep"));
        assert!(out.contains("- reference.md"));
        assert!(out.contains("- scripts/"));
        assert!(out.contains("- scripts/run.sh"));
        assert!(!out.contains("- SKILL.md"));

        let err = tool.call(json!({"skill": "missing"}), &ctx).await.expect_err("unknown");
        assert!(matches!(err, ToolError::InvalidInput(_)));
    }
}
