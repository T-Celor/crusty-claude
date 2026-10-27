//! CLAUDE.md-style memory files: discovery, `@import` expansion and prompt rendering.

use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

const MAX_FILE_BYTES: usize = 64 * 1024;
const MAX_IMPORT_DEPTH: usize = 5;
const TRUNCATION_NOTE: &str = "\n\n[truncated: file exceeds 64 KiB]\n";
const PROJECT_MEMORY_HEADER: &str = "# Project memory\n\n";

/// Where a memory file came from; listed after its path in the prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryScope {
    /// Under the home directory (`~/.crusty`, `~/.claude`).
    User,
    /// In the project directory.
    Project,
    /// In a directory between the project and home.
    Parent,
    /// `CRUSTY.local.md` / `CLAUDE.local.md`, meant to stay out of version control.
    Local,
}

impl MemoryScope {
    /// Lower-case name used in the prompt heading.
    pub fn label(self) -> &'static str {
        match self {
            MemoryScope::User => "user",
            MemoryScope::Project => "project",
            MemoryScope::Parent => "parent",
            MemoryScope::Local => "local",
        }
    }
}

/// One memory file, with `@import`s already expanded and the size capped at 64 KiB.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryFile {
    pub path: PathBuf,
    pub scope: MemoryScope,
    pub content: String,
}

/// Finds memory files in order: user files, parent directories (top down, stopping before
/// `home`), the project directory with `names`, then the project's local files. Duplicate
/// paths are loaded once.
pub fn load_memory(project: &Path, home: &Path, names: &[String]) -> Vec<MemoryFile> {
    let mut candidates: Vec<(PathBuf, MemoryScope)> =
        vec![(home.join(".crusty").join("CRUSTY.md"), MemoryScope::User), (home.join(".claude").join("CLAUDE.md"), MemoryScope::User)];

    let mut parents: Vec<&Path> = if project == home {
        Vec::new()
    } else {
        project.ancestors().skip(1).take_while(|dir| *dir != home && dir.parent().is_some()).collect()
    };
    parents.reverse();
    for dir in parents {
        candidates.extend(names.iter().map(|name| (dir.join(name), MemoryScope::Parent)));
    }
    candidates.extend(names.iter().map(|name| (project.join(name), MemoryScope::Project)));
    candidates.push((project.join("CRUSTY.local.md"), MemoryScope::Local));
    candidates.push((project.join("CLAUDE.local.md"), MemoryScope::Local));

    let mut seen = HashSet::new();
    let mut files = Vec::new();
    for (path, scope) in candidates {
        if !path.is_file() || !seen.insert(canonical(&path)) {
            continue;
        }
        let Some(text) = read_text(&path) else {
            continue;
        };
        let base = path.parent().unwrap_or(Path::new("."));
        let mut stack = vec![canonical(&path)];
        let content = cap(expand_imports(&text, base, home, 0, &mut stack));
        files.push(MemoryFile { path, scope, content });
    }
    files
}

/// Renders memory files as a prompt section. Empty when there are no files.
pub fn memory_prompt(files: &[MemoryFile]) -> String {
    if files.is_empty() {
        return String::new();
    }
    let mut out = String::from("# Memory files\n");
    for file in files {
        out.push_str(&format!("\n## {} ({})\n{}\n", file.path.display(), file.scope.label(), file.content.trim_end()));
    }
    out
}

/// Appends `- <note>` to the project's memory file: `CRUSTY.md` if it exists, else `CLAUDE.md`,
/// else a new `CRUSTY.md`. Returns the file written. Line breaks in `note` become spaces.
pub fn append_memory(project: &Path, note: &str) -> io::Result<PathBuf> {
    let line = note.replace(['\r', '\n'], " ");
    let line = line.trim();
    if line.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "memory note is empty"));
    }
    let crusty = project.join("CRUSTY.md");
    let claude = project.join("CLAUDE.md");
    let target = if crusty.is_file() {
        crusty
    } else if claude.is_file() {
        claude
    } else {
        fs::write(&crusty, PROJECT_MEMORY_HEADER)?;
        crusty
    };
    let mut file = OpenOptions::new().append(true).open(&target)?;
    file.write_all(format!("- {line}\n").as_bytes())?;
    Ok(target)
}

fn canonical(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Reads a file as lossy UTF-8, capped at 64 KiB. `None` if it cannot be read.
fn read_text(path: &Path) -> Option<String> {
    let bytes = fs::read(path).ok()?;
    Some(cap(String::from_utf8_lossy(&bytes).into_owned()))
}

/// Cuts `text` to 64 KiB (including the truncation note) at a character boundary.
fn cap(mut text: String) -> String {
    if text.len() <= MAX_FILE_BYTES {
        return text;
    }
    let mut end = MAX_FILE_BYTES - TRUNCATION_NOTE.len();
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    text.push_str(TRUNCATION_NOTE);
    text
}

/// Appends the content of every `@path` token found in `text` after its line.
///
/// Paths resolve against `base`, `~/` against `home`. Missing files and cycles leave the
/// token alone; nesting stops after `MAX_IMPORT_DEPTH` levels.
fn expand_imports(text: &str, base: &Path, home: &Path, depth: usize, stack: &mut Vec<PathBuf>) -> String {
    if depth >= MAX_IMPORT_DEPTH {
        return text.to_owned();
    }
    let mut out = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        out.push_str(line);
        for token in line.split_whitespace() {
            let Some(rel) = token.strip_prefix('@').filter(|rel| !rel.is_empty() && !rel.contains('@')) else {
                continue;
            };
            let path = resolve(rel, base, home);
            let key = canonical(&path);
            if stack.contains(&key) || !path.is_file() {
                continue;
            }
            let Some(content) = read_text(&path) else {
                continue;
            };
            let dir = path.parent().unwrap_or(base).to_path_buf();
            stack.push(key);
            let expanded = expand_imports(&content, &dir, home, depth + 1, stack);
            stack.pop();
            out.push_str(&expanded);
            if !out.ends_with('\n') {
                out.push('\n');
            }
        }
    }
    out
}

fn resolve(rel: &str, base: &Path, home: &Path) -> PathBuf {
    if let Some(rest) = rel.strip_prefix("~/") {
        return home.join(rest);
    }
    let path = Path::new(rel);
    if path.is_absolute() { path.to_path_buf() } else { base.join(path) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn names() -> Vec<String> {
        vec!["CRUSTY.md".into(), "CLAUDE.md".into()]
    }

    #[test]
    fn discovery_order_is_user_parent_project_local() {
        let root = tempdir().unwrap();
        let home = root.path().join("home");
        let project = home.join("work").join("app");
        fs::create_dir_all(home.join(".claude")).unwrap();
        fs::create_dir_all(&project).unwrap();
        fs::write(home.join(".claude").join("CLAUDE.md"), "user rule").unwrap();
        fs::write(home.join("work").join("CLAUDE.md"), "parent rule").unwrap();
        fs::write(project.join("CRUSTY.md"), "project rule").unwrap();
        fs::write(project.join("CLAUDE.local.md"), "local rule").unwrap();
        fs::write(root.path().join("CLAUDE.md"), "above home, ignored").unwrap();

        let files = load_memory(&project, &home, &names());
        let scopes: Vec<MemoryScope> = files.iter().map(|f| f.scope).collect();
        assert_eq!(scopes, [MemoryScope::User, MemoryScope::Parent, MemoryScope::Project, MemoryScope::Local]);
        let contents: Vec<&str> = files.iter().map(|f| f.content.as_str()).collect();
        assert_eq!(contents, ["user rule", "parent rule", "project rule", "local rule"]);

        let prompt = memory_prompt(&files);
        assert!(prompt.starts_with("# Memory files\n"));
        assert!(prompt.contains("(project)\nproject rule"));
        assert!(memory_prompt(&[]).is_empty());
    }

    #[test]
    fn imports_expand_relative_to_importer() {
        let root = tempdir().unwrap();
        let home = root.path().join("home");
        let project = home.join("app");
        fs::create_dir_all(&project).unwrap();
        fs::write(home.join("shared.md"), "shared text").unwrap();
        fs::write(project.join("CRUSTY.md"), "see\n@../shared.md\n@~/shared.md\n@missing.md\n").unwrap();

        let files = load_memory(&project, &home, &names());
        let content = &files[0].content;
        assert!(content.contains("@../shared.md\nshared text"));
        assert!(content.contains("@missing.md"));
    }

    #[test]
    fn import_cycles_and_depth_are_bounded() {
        let root = tempdir().unwrap();
        let home = root.path().join("home");
        let project = home.join("app");
        fs::create_dir_all(&project).unwrap();
        fs::write(project.join("a.md"), "@b.md\n").unwrap();
        fs::write(project.join("b.md"), "B text\n@a.md\n").unwrap();
        for i in 1..=7 {
            let body = if i == 7 { "END".to_owned() } else { format!("@d{}.md\n", i + 1) };
            fs::write(project.join(format!("d{i}.md")), body).unwrap();
        }

        let cycle = load_memory(&project, &home, &["a.md".into()]);
        assert!(cycle[0].content.contains("B text"));
        assert!(cycle[0].content.contains("@a.md"));

        let deep = load_memory(&project, &home, &["d1.md".into()]);
        assert!(deep[0].content.contains("@d7.md"));
        assert!(!deep[0].content.contains("END"));
    }

    #[test]
    fn oversized_file_is_capped_with_note() {
        let root = tempdir().unwrap();
        let home = root.path().join("home");
        let project = home.join("app");
        fs::create_dir_all(&project).unwrap();
        fs::write(project.join("CRUSTY.md"), "a".repeat(70_000)).unwrap();

        let files = load_memory(&project, &home, &names());
        assert!(files[0].content.len() <= MAX_FILE_BYTES);
        assert!(files[0].content.ends_with(TRUNCATION_NOTE));
    }

    #[test]
    fn append_memory_creates_then_appends() {
        let root = tempdir().unwrap();
        let project = root.path().join("app");
        fs::create_dir_all(&project).unwrap();

        let created = append_memory(&project, "use tabs\nnot spaces").unwrap();
        assert_eq!(created, project.join("CRUSTY.md"));
        assert_eq!(fs::read_to_string(&created).unwrap(), "# Project memory\n\n- use tabs not spaces\n");

        append_memory(&project, "second").unwrap();
        assert!(fs::read_to_string(&created).unwrap().ends_with("- second\n"));

        fs::remove_file(&created).unwrap();
        fs::write(project.join("CLAUDE.md"), "# Claude\n").unwrap();
        let target = append_memory(&project, "into claude").unwrap();
        assert_eq!(target, project.join("CLAUDE.md"));
        assert_eq!(fs::read_to_string(&target).unwrap(), "# Claude\n- into claude\n");

        assert!(append_memory(&project, "  ").is_err());
    }
}
