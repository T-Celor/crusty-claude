//! `GRAPH_REPORT.md`: summary, languages, god nodes, communities, surprising connections, and
//! the provenance legend. Also reads the excerpt of an external Graphify report.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use crate::analyze;
use crate::build::IndexStats;
use crate::model::{Graph, NodeKind};
use crate::query::label;

const EXTERNAL_REPORT: &str = "graphify-out/GRAPH_REPORT.md";
const EXTERNAL_REPORT_CHARS: usize = 8_000;
const GOD_NODES: usize = 15;
const REPORT_COMMUNITIES: usize = 20;
const COMMUNITY_EXAMPLES: usize = 5;
const SURPRISES: usize = 10;

/// Markdown report of a built graph.
pub fn render(graph: &Graph, stats: &IndexStats) -> String {
    let mut lines: Vec<String> = vec![
        "# Code graph report".into(),
        String::new(),
        format!("Root: `{}`", graph.root),
        String::new(),
        "## Summary".into(),
        format!("- Files: {}", stats.files),
        format!("- Symbols: {}", stats.symbols),
        format!("- Edges: {}", stats.edges),
        format!("- Communities: {}", stats.communities),
        format!("- Files reused from the previous index: {}", stats.reused_files),
        format!("- Build time: {} ms", stats.duration_ms),
        String::new(),
        "## Languages".into(),
        "| Language | Files |".into(),
        "| --- | --- |".into(),
    ];

    let mut languages: BTreeMap<&str, usize> = BTreeMap::new();
    for node in graph.nodes.values().filter(|n| matches!(n.kind, NodeKind::File)) {
        *languages.entry(node.lang.as_deref().unwrap_or("other")).or_insert(0) += 1;
    }
    let mut languages: Vec<(&str, usize)> = languages.into_iter().collect();
    languages.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
    lines.extend(languages.iter().map(|(lang, count)| format!("| {lang} | {count} |")));

    lines.push(String::new());
    lines.push("## God nodes".into());
    for (rank, (node, degree)) in analyze::god_nodes(graph, GOD_NODES).into_iter().enumerate() {
        lines.push(format!("{}. `{}` ({degree} edges)", rank + 1, label(graph, &node.id)));
    }

    lines.push(String::new());
    lines.push("## Communities".into());
    let mut members: BTreeMap<u32, Vec<&str>> = BTreeMap::new();
    for node in graph.nodes.values().filter(|n| matches!(n.kind, NodeKind::File)) {
        if let (Some(community), Some(file)) = (graph.communities.get(&node.id), node.file.as_deref()) {
            members.entry(*community).or_default().push(file);
        }
    }
    let mut communities: Vec<(u32, Vec<&str>)> = members.into_iter().collect();
    communities.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then(a.0.cmp(&b.0)));
    for (community, mut files) in communities.into_iter().take(REPORT_COMMUNITIES) {
        files.sort_unstable();
        let total = files.len();
        let examples: Vec<&str> = files.into_iter().take(COMMUNITY_EXAMPLES).collect();
        lines.push(format!("- **c{community}** ({total} files): {}", examples.join(", ")));
    }

    lines.push(String::new());
    lines.push("## Surprising connections".into());
    let surprises = analyze::surprising_edges(graph, SURPRISES);
    if surprises.is_empty() {
        lines.push("- None.".into());
    }
    for edge in surprises {
        lines.push(format!("- `{}` -> `{}` ({:?}, {:?})", label(graph, &edge.from), label(graph, &edge.to), edge.kind, edge.provenance));
    }

    lines.push(String::new());
    lines.push("## Provenance".into());
    lines.push("- **Extracted**: read from the source text (definitions, imports, containment).".into());
    lines.push("- **Inferred**: heuristic links (identifiers that match a definition in another file). Treat them as hints.".into());
    lines.push(String::new());
    lines.join("\n")
}

/// Excerpt of the report written by the external Graphify tool, when present under `root`.
pub fn external_report(root: &Path) -> Option<String> {
    let text = fs::read_to_string(root.join(EXTERNAL_REPORT)).ok()?;
    Some(text.chars().take(EXTERNAL_REPORT_CHARS).collect())
}
