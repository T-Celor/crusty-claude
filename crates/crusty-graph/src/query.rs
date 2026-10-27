//! Questions answered from a built graph: search, explanation, neighbourhoods, paths and the
//! repository map.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::Path;

use crusty_core::tokens::estimate_text;

use crate::analyze;
use crate::model::{EdgeKind, Graph, Node, NodeKind};

/// Entries listed per section of an explanation before it is cut short.
const LIST_LIMIT: usize = 30;
/// Communities shown in the repository map.
const MAP_COMMUNITIES: usize = 15;
/// Files named as representatives of a community in the repository map.
const MAP_REPRESENTATIVES: usize = 3;
const MANIFESTS: &[&str] = &["Cargo.toml", "package.json", "pyproject.toml"];

/// Nodes whose name contains `query` (case-insensitive). Exact name matches come first.
pub fn find<'a>(graph: &'a Graph, query: &str, limit: usize) -> Vec<&'a Node> {
    let needle = query.to_lowercase();
    if needle.is_empty() {
        return Vec::new();
    }
    let mut hits: Vec<&Node> = graph.nodes.values().filter(|node| node.name.to_lowercase().contains(&needle)).collect();
    hits.sort_by(|a, b| {
        let exact_a = a.name.to_lowercase() != needle;
        let exact_b = b.name.to_lowercase() != needle;
        exact_a.cmp(&exact_b).then_with(|| a.id.cmp(&b.id))
    });
    hits.truncate(limit);
    hits
}

/// Node ids a target refers to: an exact id, a file path, or the exact name of symbols.
pub fn resolve(graph: &Graph, target: &str) -> Vec<String> {
    let target = target.replace('\\', "/");
    if graph.nodes.contains_key(&target) {
        return vec![target];
    }
    let file_id = format!("file:{target}");
    if graph.nodes.contains_key(&file_id) {
        return vec![file_id];
    }
    graph
        .nodes
        .values()
        .filter(|node| matches!(node.kind, NodeKind::Symbol { .. }) && node.name == target)
        .map(|node| node.id.clone())
        .collect()
}

/// Short human label of a node: a file path, `kind name (file:line)` for symbols, or the id.
pub fn label(graph: &Graph, id: &str) -> String {
    let Some(node) = graph.nodes.get(id) else {
        return id.to_owned();
    };
    match (&node.kind, &node.file) {
        (NodeKind::File, Some(file)) => file.clone(),
        (NodeKind::Symbol { kind }, Some(file)) => format!("{kind} {} ({file}:{})", node.name, node.line.unwrap_or(0)),
        _ => id.to_owned(),
    }
}

/// Files of a node and of its direct neighbours: the source an answer about it draws on.
pub fn related_files(graph: &Graph, id: &str) -> Vec<String> {
    let mut files: BTreeSet<String> = BTreeSet::new();
    let mut add = |node_id: &str| {
        if let Some(file) = graph.nodes.get(node_id).and_then(|node| node.file.clone()) {
            files.insert(file);
        }
    };
    add(id);
    for edge in &graph.edges {
        if edge.from == id {
            add(edge.to.as_str());
        } else if edge.to == id {
            add(edge.from.as_str());
        }
    }
    files.into_iter().collect()
}

/// Multi-line explanation of a file or symbol target, or `None` when nothing matches.
pub fn explain(graph: &Graph, target: &str) -> Option<String> {
    let ids = resolve(graph, target);
    if ids.is_empty() {
        return None;
    }
    let mut lines = Vec::new();
    for id in &ids {
        let Some(node) = graph.nodes.get(id) else {
            continue;
        };
        match &node.kind {
            NodeKind::File => explain_file(graph, id, node, &mut lines),
            NodeKind::Symbol { .. } => explain_symbol(graph, id, node, &mut lines),
            NodeKind::External => {}
        }
    }
    Some(lines.join("\n"))
}

fn explain_file(graph: &Graph, id: &str, node: &Node, lines: &mut Vec<String>) {
    let path = node.file.as_deref().unwrap_or(&node.name);
    lines.push(format!("File {path} (lang {}, community {})", node.lang.as_deref().unwrap_or("?"), community(graph, id)));
    let mut definitions: Vec<&Node> = graph
        .edges
        .iter()
        .filter(|edge| edge.kind == EdgeKind::Contains && edge.from == id)
        .filter_map(|edge| graph.nodes.get(&edge.to))
        .collect();
    definitions.sort_by_key(|node| node.line);
    let definitions = definitions.iter().map(|node| format!("{} {} (line {})", kind_of(node), node.name, node.line.unwrap_or(0))).collect();
    section(lines, "Definitions", definitions);
    section(lines, "Imports", targets(graph, id, EdgeKind::Imports));
    section(lines, "Imported by", sources(graph, id, EdgeKind::Imports));
    section(lines, "References out (inferred)", targets(graph, id, EdgeKind::References));
    section(lines, "References in (inferred)", sources(graph, id, EdgeKind::References));
}

fn explain_symbol(graph: &Graph, id: &str, node: &Node, lines: &mut Vec<String>) {
    let file = node.file.as_deref().unwrap_or("?");
    lines.push(format!("{} {} at {file}:{} (community {})", kind_of(node), node.name, node.line.unwrap_or(0), community(graph, id)));
    section(lines, "Referenced by (inferred)", sources(graph, id, EdgeKind::References));
}

fn section(lines: &mut Vec<String>, title: &str, mut items: Vec<String>) {
    if items.is_empty() {
        return;
    }
    let total = items.len();
    lines.push(format!("{title} ({total}):"));
    let hidden = total.saturating_sub(LIST_LIMIT);
    items.truncate(LIST_LIMIT);
    lines.extend(items.into_iter().map(|item| format!("  - {item}")));
    if hidden > 0 {
        lines.push(format!("  ... {hidden} more"));
    }
}

fn targets(graph: &Graph, id: &str, kind: EdgeKind) -> Vec<String> {
    graph.edges.iter().filter(|e| e.kind == kind && e.from == id).map(|e| label(graph, &e.to)).collect()
}

fn sources(graph: &Graph, id: &str, kind: EdgeKind) -> Vec<String> {
    graph.edges.iter().filter(|e| e.kind == kind && e.to == id).map(|e| label(graph, &e.from)).collect()
}

fn community(graph: &Graph, id: &str) -> String {
    graph.communities.get(id).map_or_else(|| "none".to_owned(), u32::to_string)
}

fn kind_of(node: &Node) -> &str {
    match &node.kind {
        NodeKind::Symbol { kind } => kind.as_str(),
        NodeKind::File => "file",
        NodeKind::External => "external",
    }
}

/// Undirected adjacency over all edges.
fn adjacency(graph: &Graph) -> BTreeMap<&str, Vec<&str>> {
    let mut adjacent: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for edge in &graph.edges {
        adjacent.entry(edge.from.as_str()).or_default().push(edge.to.as_str());
        adjacent.entry(edge.to.as_str()).or_default().push(edge.from.as_str());
    }
    adjacent
}

/// Ids within `depth` hops of `id` (breadth first, start excluded). Unknown ids give nothing.
pub fn neighbors(graph: &Graph, id: &str, depth: usize) -> Vec<String> {
    let Some(start) = graph.nodes.get_key_value(id).map(|(key, _)| key.as_str()) else {
        return Vec::new();
    };
    let adjacent = adjacency(graph);
    let mut seen: BTreeSet<&str> = BTreeSet::from([start]);
    let mut frontier = vec![start];
    let mut out = Vec::new();
    for _ in 0..depth {
        let mut next = Vec::new();
        for node in frontier {
            if let Some(others) = adjacent.get(node) {
                for &other in others {
                    if seen.insert(other) {
                        out.push(other.to_owned());
                        next.push(other);
                    }
                }
            }
        }
        if next.is_empty() {
            break;
        }
        frontier = next;
    }
    out
}

/// Shortest undirected path between two targets (ids, file paths or symbol names).
pub fn path(graph: &Graph, from: &str, to: &str) -> Option<Vec<String>> {
    let start = resolve(graph, from).into_iter().next()?;
    let goal = resolve(graph, to).into_iter().next()?;
    let adjacent = adjacency(graph);
    let mut parent: BTreeMap<&str, &str> = BTreeMap::new();
    let mut seen: BTreeSet<&str> = BTreeSet::from([start.as_str()]);
    let mut queue = VecDeque::from([start.as_str()]);
    while let Some(node) = queue.pop_front() {
        if node == goal {
            break;
        }
        for &other in adjacent.get(node).into_iter().flatten() {
            if seen.insert(other) {
                parent.insert(other, node);
                queue.push_back(other);
            }
        }
    }
    let mut current = goal.as_str();
    let mut out = vec![goal.clone()];
    while current != start {
        current = parent.get(current).copied()?;
        out.push(current.to_owned());
    }
    out.reverse();
    Some(out)
}

/// One-line summary of the graph's size.
pub fn stats_text(graph: &Graph) -> String {
    let count = |kind: fn(&NodeKind) -> bool| graph.nodes.values().filter(|n| kind(&n.kind)).count();
    let files = count(|k| matches!(k, NodeKind::File));
    let symbols = count(|k| matches!(k, NodeKind::Symbol { .. }));
    let externals = count(|k| matches!(k, NodeKind::External));
    let communities: BTreeSet<&u32> = graph.communities.values().collect();
    format!(
        "Graph of {}: {files} files, {symbols} symbols, {externals} external modules, {} edges, {} communities.",
        graph.root,
        graph.edges.len(),
        communities.len()
    )
}

/// Repository map trimmed to `budget_tokens` (estimated). Sections are added in priority order
/// and the first one that does not fit ends the map.
pub fn repo_map(graph: &Graph, budget_tokens: u64) -> String {
    let files: Vec<&Node> = graph.nodes.values().filter(|n| matches!(n.kind, NodeKind::File)).collect();
    let mut lines = vec!["# Repository map".to_owned()];

    let mut languages: BTreeMap<&str, usize> = BTreeMap::new();
    for file in &files {
        *languages.entry(file.lang.as_deref().unwrap_or("other")).or_insert(0) += 1;
    }
    let languages = ranked(languages.into_iter().collect());
    lines.push(format!("Languages: {}", languages.iter().map(|(lang, count)| format!("{lang} {count}")).collect::<Vec<_>>().join(", ")));

    let mut entries: Vec<String> =
        files.iter().filter_map(|file| file.file.as_deref()).filter(|path| is_entry_point(path)).map(str::to_owned).collect();
    entries.extend(MANIFESTS.iter().filter(|m| Path::new(&graph.root).join(m).is_file()).map(|m| (*m).to_owned()));
    lines.push(if entries.is_empty() { "Entry points: none found".to_owned() } else { format!("Entry points: {}", entries.join(", ")) });

    let mut directories: BTreeMap<&str, usize> = BTreeMap::new();
    for file in &files {
        if let Some(path) = file.file.as_deref() {
            let top = path.split_once('/').map_or("(root)", |(dir, _)| dir);
            *directories.entry(top).or_insert(0) += 1;
        }
    }
    let mut directories = ranked(directories.into_iter().collect());
    directories.truncate(10);
    lines.push(format!(
        "Top-level directories: {}",
        directories.iter().map(|(dir, count)| format!("{dir} {count}")).collect::<Vec<_>>().join(", ")
    ));

    lines.push("God nodes:".to_owned());
    for (node, degree) in analyze::god_nodes(graph, 10) {
        lines.push(format!("- {} ({degree} edges)", label(graph, &node.id)));
    }

    let degree = analyze::degrees(graph);
    let mut by_community: BTreeMap<u32, Vec<&Node>> = BTreeMap::new();
    for file in &files {
        if let Some(community) = graph.communities.get(&file.id) {
            by_community.entry(*community).or_default().push(file);
        }
    }
    let mut communities: Vec<(u32, Vec<&Node>)> = by_community.into_iter().collect();
    communities.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then(a.0.cmp(&b.0)));
    lines.push(format!("Communities: {}", communities.len()));
    for (community, mut members) in communities.into_iter().take(MAP_COMMUNITIES) {
        members.sort_by(|a, b| {
            let da = degree.get(a.id.as_str()).copied().unwrap_or(0);
            let db = degree.get(b.id.as_str()).copied().unwrap_or(0);
            db.cmp(&da).then_with(|| a.id.cmp(&b.id))
        });
        let representatives: Vec<&str> = members.iter().take(MAP_REPRESENTATIVES).filter_map(|file| file.file.as_deref()).collect();
        lines.push(format!("- c{community} ({} files): {}", members.len(), representatives.join(", ")));
    }

    let mut out = String::new();
    for line in lines {
        let candidate = format!("{out}{line}\n");
        if estimate_text(&candidate) > budget_tokens {
            break;
        }
        out = candidate;
    }
    out
}

/// Sorts `(name, count)` pairs by count, largest first, then by name.
fn ranked(mut pairs: Vec<(&str, usize)>) -> Vec<(&str, usize)> {
    pairs.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
    pairs
}

fn is_entry_point(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    matches!(name, "main.rs" | "lib.rs" | "main.py" | "__main__.py" | "index.ts" | "index.js") || name.starts_with("app.")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Edge, GRAPH_VERSION, Provenance};

    fn graph_with(edges: &[(&str, &str)]) -> Graph {
        let mut graph = Graph {
            version: GRAPH_VERSION,
            root: "/nowhere".into(),
            nodes: BTreeMap::new(),
            edges: Vec::new(),
            file_hashes: BTreeMap::new(),
            communities: BTreeMap::new(),
        };
        for (from, to) in edges {
            for id in [from, to] {
                let path = id.trim_start_matches("file:").to_owned();
                graph.nodes.entry((*id).to_owned()).or_insert_with(|| Node {
                    id: (*id).to_owned(),
                    kind: NodeKind::File,
                    name: path.clone(),
                    file: Some(path),
                    line: None,
                    lang: Some("rust".into()),
                });
            }
            graph.edges.push(Edge {
                from: (*from).to_owned(),
                to: (*to).to_owned(),
                kind: EdgeKind::Imports,
                provenance: Provenance::Extracted,
            });
        }
        graph
    }

    #[test]
    fn path_and_neighbors_walk_the_undirected_graph() {
        let graph = graph_with(&[("file:a", "file:b"), ("file:b", "file:c")]);
        assert_eq!(path(&graph, "c", "a"), Some(vec!["file:c".to_owned(), "file:b".to_owned(), "file:a".to_owned()]));
        assert_eq!(neighbors(&graph, "file:b", 1), vec!["file:a".to_owned(), "file:c".to_owned()]);
        assert_eq!(neighbors(&graph, "file:a", 1), vec!["file:b".to_owned()]);
        assert_eq!(neighbors(&graph, "file:a", 2), vec!["file:b".to_owned(), "file:c".to_owned()]);
        assert!(path(&graph, "a", "missing").is_none());
    }
}
