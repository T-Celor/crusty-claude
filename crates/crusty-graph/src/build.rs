//! Index construction: walk the repository, hash files, extract, resolve imports, infer
//! references, and persist the graph atomically.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::Instant;

use crusty_core::config::GraphifyConfig;
use ignore::WalkBuilder;
use regex::Regex;
use sha2::{Digest, Sha256};

use crate::GraphError;
use crate::analyze;
use crate::extract::{Definition, Import, ImportKind, extract};
use crate::lang::language_of;
use crate::model::{Edge, EdgeKind, GRAPH_VERSION, Graph, Node, NodeKind, Provenance};

/// Directories never indexed, besides hidden ones and anything git-ignored.
const SKIP_DIRS: &[&str] = &[".git", "target", "node_modules"];
/// A name defined in more files than this is too generic to link files by.
const MAX_DEFINING_FILES: usize = 3;
/// Cap on inferred reference edges per file.
const MAX_REFERENCES_PER_FILE: usize = 200;
/// Extensions tried after a relative module path, in order.
const RELATIVE_SUFFIXES: &[&str] = &["", ".rb", ".h", ".hpp", ".php", ".sh", ".md"];
const JS_EXTENSIONS: &[&str] = &[".ts", ".tsx", ".js", ".jsx", ".mjs", ".cjs"];

static IDENTIFIER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[A-Za-z_][A-Za-z0-9_]{3,}").expect("valid literal regex"));

/// Counts of one build.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IndexStats {
    pub files: u64,
    pub symbols: u64,
    pub edges: u64,
    pub communities: u64,
    /// Files whose symbols were kept from the previous graph because their hash did not change.
    pub reused_files: u64,
    /// Files beyond `max_files` that were not read.
    pub skipped_files: u64,
    pub duration_ms: u64,
}

/// A file read during a build.
struct Scanned {
    rel: String,
    lang: &'static str,
    hash: String,
    reused: bool,
    definitions: Vec<Definition>,
    imports: Vec<Import>,
    identifiers: BTreeSet<String>,
}

/// Builds the graph of `root`, reusing `previous` where files are unchanged.
///
/// Unchanged files keep their symbol nodes from `previous`. Imports and identifiers are
/// recomputed for every file, because they depend on the whole file set, so the result is
/// always identical to a build from scratch.
pub fn build_or_update(root: &Path, cfg: &GraphifyConfig, previous: Option<Graph>) -> Result<(Graph, IndexStats), GraphError> {
    let started = Instant::now();
    let root_str = root.to_string_lossy().replace('\\', "/");
    let (old_hashes, mut old_symbols) = reusable(previous, &root_str);

    let mut scanned = Vec::new();
    let (listed, skipped_files) = collect_files(root, cfg.max_files);
    for (rel, lang, abs) in listed {
        let Ok(meta) = fs::metadata(&abs) else {
            continue;
        };
        if meta.len() > cfg.max_file_bytes {
            continue;
        }
        let Ok(bytes) = fs::read(&abs) else {
            continue;
        };
        let Ok(text) = String::from_utf8(bytes) else {
            continue;
        };
        let hash = sha256_hex(text.as_bytes());
        let reused = old_hashes.get(&rel) == Some(&hash);
        let extracted = extract(lang, &text, !reused);
        let identifiers = IDENTIFIER.find_iter(&text).map(|m| m.as_str().to_owned()).collect();
        scanned.push(Scanned { rel, lang, hash, reused, definitions: extracted.definitions, imports: extracted.imports, identifiers });
    }

    let files: BTreeSet<String> = scanned.iter().map(|s| s.rel.clone()).collect();
    let mut nodes: BTreeMap<String, Node> = BTreeMap::new();
    let mut edges: Vec<Edge> = Vec::new();
    let mut file_hashes = BTreeMap::new();
    let mut reused_files = 0u64;

    for s in &scanned {
        let file_id = format!("file:{}", s.rel);
        nodes.insert(
            file_id.clone(),
            Node {
                id: file_id.clone(),
                kind: NodeKind::File,
                name: basename(&s.rel).to_owned(),
                file: Some(s.rel.clone()),
                line: None,
                lang: Some(s.lang.to_owned()),
            },
        );
        let symbols = if s.reused {
            reused_files += 1;
            old_symbols.remove(&s.rel).unwrap_or_default()
        } else {
            symbol_nodes(&s.rel, s.lang, &s.definitions)
        };
        for symbol in symbols {
            edges.push(Edge { from: file_id.clone(), to: symbol.id.clone(), kind: EdgeKind::Contains, provenance: Provenance::Extracted });
            nodes.insert(symbol.id.clone(), symbol);
        }
        file_hashes.insert(s.rel.clone(), s.hash.clone());
    }

    for s in &scanned {
        let from = format!("file:{}", s.rel);
        for import in &s.imports {
            if import.module.chars().all(|c| c == '.') {
                continue;
            }
            let to = match resolve_import(&files, &s.rel, s.lang, import) {
                Some(target) => format!("file:{target}"),
                None => {
                    let id = format!("ext:{}", import.module);
                    nodes.entry(id.clone()).or_insert_with(|| Node {
                        id: id.clone(),
                        kind: NodeKind::External,
                        name: import.module.clone(),
                        file: None,
                        line: None,
                        lang: None,
                    });
                    id
                }
            };
            if to != from {
                edges.push(Edge { from: from.clone(), to, kind: EdgeKind::Imports, provenance: Provenance::Extracted });
            }
        }
    }

    let mut by_name: BTreeMap<&str, Vec<(&str, String)>> = BTreeMap::new();
    for node in nodes.values() {
        if let (NodeKind::Symbol { kind }, Some(file)) = (&node.kind, &node.file)
            && kind != "impl"
            && kind != "heading"
        {
            by_name.entry(node.name.as_str()).or_default().push((file.as_str(), node.id.clone()));
        }
    }
    for s in &scanned {
        let from = format!("file:{}", s.rel);
        let mut count = 0;
        for identifier in &s.identifiers {
            if count >= MAX_REFERENCES_PER_FILE {
                break;
            }
            let Some(definitions) = by_name.get(identifier.as_str()) else {
                continue;
            };
            let defining: BTreeSet<&str> = definitions.iter().map(|(file, _)| *file).collect();
            if defining.len() > MAX_DEFINING_FILES {
                continue;
            }
            for (file, id) in definitions {
                if *file == s.rel.as_str() || count >= MAX_REFERENCES_PER_FILE {
                    continue;
                }
                edges.push(Edge { from: from.clone(), to: id.clone(), kind: EdgeKind::References, provenance: Provenance::Inferred });
                count += 1;
            }
        }
    }

    edges.sort();
    edges.dedup();

    let mut graph = Graph { version: GRAPH_VERSION, root: root_str, nodes, edges, file_hashes, communities: BTreeMap::new() };
    graph.communities = analyze::communities(&graph);
    let stats = IndexStats {
        files: count(scanned.len()),
        symbols: count(graph.nodes.values().filter(|n| matches!(n.kind, NodeKind::Symbol { .. })).count()),
        edges: count(graph.edges.len()),
        communities: count(graph.communities.values().collect::<BTreeSet<_>>().len()),
        reused_files,
        skipped_files,
        duration_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
    };
    Ok((graph, stats))
}

/// Reads a persisted graph.
pub fn load(path: &Path) -> Result<Graph, GraphError> {
    let text = fs::read_to_string(path)?;
    Ok(serde_json::from_str(&text)?)
}

/// Writes a graph atomically: a temporary file is renamed over the target.
pub fn save(graph: &Graph, path: &Path) -> Result<(), GraphError> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    let mut file = fs::File::create(&tmp)?;
    file.write_all(&serde_json::to_vec(graph)?)?;
    // Flush to disk before the rename, so a crash cannot leave an empty graph under the final name.
    file.sync_all()?;
    drop(file);
    fs::rename(&tmp, path)?;
    Ok(())
}

fn count(n: usize) -> u64 {
    u64::try_from(n).unwrap_or(u64::MAX)
}

/// Hashes and symbol nodes of a previous graph, or empty maps when it cannot be reused.
fn reusable(previous: Option<Graph>, root: &str) -> (BTreeMap<String, String>, BTreeMap<String, Vec<Node>>) {
    let Some(prev) = previous.filter(|g| g.version == GRAPH_VERSION && g.root == root) else {
        return (BTreeMap::new(), BTreeMap::new());
    };
    let mut symbols: BTreeMap<String, Vec<Node>> = BTreeMap::new();
    for node in prev.nodes.into_values() {
        if matches!(node.kind, NodeKind::Symbol { .. })
            && let Some(file) = node.file.clone()
        {
            symbols.entry(file).or_default().push(node);
        }
    }
    (prev.file_hashes, symbols)
}

/// Supported files under `root`, sorted by path and capped at `max_files`. The second value
/// counts the files beyond the cap.
fn collect_files(root: &Path, max_files: usize) -> (Vec<(String, &'static str, PathBuf)>, u64) {
    let walker = WalkBuilder::new(root)
        .require_git(false)
        .filter_entry(|entry| {
            let is_skipped_dir =
                entry.file_type().is_some_and(|t| t.is_dir()) && entry.file_name().to_str().is_some_and(|name| SKIP_DIRS.contains(&name));
            !is_skipped_dir
        })
        .build();
    let mut files: Vec<(String, &'static str, PathBuf)> = walker
        .flatten()
        .filter(|entry| entry.file_type().is_some_and(|t| t.is_file()))
        .filter_map(|entry| {
            let rel = entry.path().strip_prefix(root).ok()?.to_string_lossy().replace('\\', "/");
            let lang = language_of(&rel)?;
            Some((rel, lang, entry.into_path()))
        })
        .collect();
    files.sort_by(|a, b| a.0.cmp(&b.0));
    let skipped = count(files.len().saturating_sub(max_files));
    files.truncate(max_files);
    (files, skipped)
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|byte| format!("{byte:02x}")).collect()
}

fn symbol_nodes(rel: &str, lang: &str, definitions: &[Definition]) -> Vec<Node> {
    let mut seen: BTreeSet<String> = BTreeSet::new();
    definitions
        .iter()
        .map(|definition| {
            let mut id = format!("sym:{rel}#{}", definition.name);
            if !seen.insert(id.clone()) {
                id = format!("sym:{rel}#{}@{}", definition.name, definition.line);
                seen.insert(id.clone());
            }
            Node {
                id,
                kind: NodeKind::Symbol { kind: definition.kind.to_owned() },
                name: definition.name.clone(),
                file: Some(rel.to_owned()),
                line: Some(definition.line),
                lang: Some(lang.to_owned()),
            }
        })
        .collect()
}

fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// Directory part of a relative path; empty for files at the root.
fn parent_dir(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(dir, _)| dir)
}

/// Joins a relative path onto `base`, resolving `.` and `..`. `None` if it escapes the root.
fn join_path(base: &str, rel: &str) -> Option<String> {
    let mut parts: Vec<&str> = base.split('/').filter(|s| !s.is_empty() && *s != ".").collect();
    for segment in rel.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            other => parts.push(other),
        }
    }
    Some(parts.join("/"))
}

/// The repository file an import resolves to, or `None` when it is external.
fn resolve_import(files: &BTreeSet<String>, from: &str, lang: &str, import: &Import) -> Option<String> {
    match lang {
        "rust" => resolve_rust(files, from, import),
        "python" => resolve_python(files, from, &import.module),
        "javascript" | "typescript" => resolve_js(files, from, &import.module),
        "c" | "cpp" | "ruby" | "php" | "shell" | "markdown" => resolve_relative(files, from, import),
        _ => None,
    }
}

fn resolve_relative(files: &BTreeSet<String>, from: &str, import: &Import) -> Option<String> {
    if import.kind != ImportKind::Relative {
        return None;
    }
    let bases = [join_path(parent_dir(from), &import.module), join_path("", &import.module)];
    bases
        .into_iter()
        .flatten()
        .flat_map(|base| RELATIVE_SUFFIXES.iter().map(move |suffix| format!("{base}{suffix}")))
        .find(|candidate| files.contains(candidate))
}

fn resolve_js(files: &BTreeSet<String>, from: &str, module: &str) -> Option<String> {
    if !module.starts_with('.') {
        return None;
    }
    let base = join_path(parent_dir(from), module)?;
    let mut candidates = vec![base.clone()];
    candidates.extend(JS_EXTENSIONS.iter().map(|ext| format!("{base}{ext}")));
    candidates.extend(JS_EXTENSIONS.iter().map(|ext| format!("{base}/index{ext}")));
    // TypeScript sources are often imported with their emitted `.js` extension.
    for ext in [".js", ".jsx", ".mjs", ".cjs"] {
        if let Some(stem) = base.strip_suffix(ext) {
            candidates.push(format!("{stem}.ts"));
            candidates.push(format!("{stem}.tsx"));
        }
    }
    candidates.into_iter().find(|candidate| files.contains(candidate))
}

fn resolve_python(files: &BTreeSet<String>, from: &str, module: &str) -> Option<String> {
    let trimmed = module.trim_start_matches('.');
    let dots = module.len() - trimmed.len();
    let segments: Vec<&str> = trimmed.split('.').filter(|s| !s.is_empty()).collect();
    if dots > 0 {
        let mut dir = parent_dir(from);
        for _ in 1..dots {
            dir = parent_dir(dir);
        }
        return python_in(files, dir, &segments);
    }
    if segments.is_empty() {
        return None;
    }
    python_in(files, "", &segments).or_else(|| python_in(files, parent_dir(from), &segments))
}

fn python_in(files: &BTreeSet<String>, base: &str, segments: &[&str]) -> Option<String> {
    if segments.is_empty() {
        return join_path(base, "__init__.py").filter(|p| files.contains(p));
    }
    for k in (1..=segments.len()).rev() {
        let Some(stem) = join_path(base, &segments[..k].join("/")) else {
            continue;
        };
        for candidate in [format!("{stem}.py"), format!("{stem}/__init__.py")] {
            if files.contains(&candidate) {
                return Some(candidate);
            }
        }
    }
    None
}

fn resolve_rust(files: &BTreeSet<String>, from: &str, import: &Import) -> Option<String> {
    let module_dir = rust_module_dir(from);
    if import.kind == ImportKind::ModDecl {
        return rust_path(files, &module_dir, &[import.module.as_str()]);
    }
    let segments: Vec<&str> = import.module.split("::").filter(|s| !s.is_empty()).collect();
    match segments.first().copied() {
        Some("crate") => rust_path(files, &crate_src(files, from), &segments[1..]),
        Some("self") => rust_path(files, &module_dir, &segments[1..]),
        Some("super") => {
            let mut base = module_dir;
            let mut rest: &[&str] = &segments;
            while let Some((&"super", tail)) = rest.split_first() {
                base = parent_dir(&base).to_owned();
                rest = tail;
            }
            rust_path(files, &base, rest)
        }
        _ => rust_path(files, &module_dir, &segments).or_else(|| rust_path(files, &crate_src(files, from), &segments)),
    }
}

/// Directory holding the child modules of a Rust source file.
fn rust_module_dir(from: &str) -> String {
    let dir = parent_dir(from);
    match basename(from).strip_suffix(".rs") {
        Some("lib" | "main" | "mod") | None => dir.to_owned(),
        Some(stem) => join_path(dir, stem).unwrap_or_default(),
    }
}

/// Source root of the crate containing `from`: the nearest ancestor with `lib.rs` or `main.rs`.
fn crate_src(files: &BTreeSet<String>, from: &str) -> String {
    let mut dir = parent_dir(from).to_owned();
    loop {
        let is_root = ["lib.rs", "main.rs"].iter().any(|name| files.contains(&join_path(&dir, name).unwrap_or_default()));
        if is_root || dir.is_empty() {
            return dir;
        }
        dir = parent_dir(&dir).to_owned();
    }
}

/// Resolves `segments` as module files under `base`, longest prefix first.
fn rust_path(files: &BTreeSet<String>, base: &str, segments: &[&str]) -> Option<String> {
    if segments.is_empty() {
        return ["mod.rs", "lib.rs", "main.rs"].iter().filter_map(|name| join_path(base, name)).find(|candidate| files.contains(candidate));
    }
    for k in (1..=segments.len()).rev() {
        let stem = join_path(base, &segments[..k].join("/"))?;
        for candidate in [format!("{stem}.rs"), format!("{stem}/mod.rs")] {
            if files.contains(&candidate) {
                return Some(candidate);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(paths: &[&str]) -> BTreeSet<String> {
        paths.iter().map(|p| (*p).to_owned()).collect()
    }

    #[test]
    fn join_resolves_dot_segments() {
        assert_eq!(join_path("src/a", "../b/./c.rs").as_deref(), Some("src/b/c.rs"));
        assert_eq!(join_path("", "./math").as_deref(), Some("math"));
        assert_eq!(join_path("", "../escape"), None);
    }

    #[test]
    fn python_lookup_continues_past_a_path_that_escapes_the_root() {
        let files = set(&["x.py"]);
        assert_eq!(python_in(&files, "", &["x", "..", "..", "y"]).as_deref(), Some("x.py"));
    }

    #[test]
    fn save_writes_a_loadable_graph_and_no_temp_file() {
        let graph = Graph {
            version: GRAPH_VERSION,
            root: "r".into(),
            nodes: BTreeMap::new(),
            edges: Vec::new(),
            file_hashes: BTreeMap::new(),
            communities: BTreeMap::new(),
        };
        let path = std::env::temp_dir().join(format!("crusty-graph-save-{}.json", std::process::id()));
        save(&graph, &path).unwrap();
        assert_eq!(load(&path).unwrap().root, "r");
        assert!(!path.with_extension("json.tmp").exists());
        fs::remove_file(&path).unwrap();
    }

    #[test]
    fn rust_paths_prefer_the_longest_module_file() {
        let files = set(&["src/lib.rs", "src/util.rs", "src/util/inner.rs"]);
        let from = "src/lib.rs";
        let import = Import { module: "crate::util::inner::deep".into(), kind: ImportKind::Path, line: 1 };
        assert_eq!(resolve_rust(&files, from, &import).as_deref(), Some("src/util/inner.rs"));
        let child = Import { module: "util".into(), kind: ImportKind::ModDecl, line: 1 };
        assert_eq!(resolve_rust(&files, "src/lib.rs", &child).as_deref(), Some("src/util.rs"));
    }
}
