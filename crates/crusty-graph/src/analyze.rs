//! Graph analytics: degrees, god nodes, deterministic communities and surprising edges.

use std::collections::BTreeMap;

use crate::model::{Edge, EdgeKind, Graph, Node, NodeKind, Provenance};

/// Upper bound of label-propagation rounds; the labels rarely settle later than this.
const MAX_ROUNDS: usize = 20;
/// Weight of an extracted import or containment edge in label propagation.
const EXTRACTED_WEIGHT: u32 = 4;
/// Weight of an inferred reference: 0.25 of an extracted edge.
const INFERRED_WEIGHT: u32 = 1;

/// Number of edges touching each node, in either direction.
pub fn degrees(graph: &Graph) -> BTreeMap<&str, usize> {
    let mut degree: BTreeMap<&str, usize> = graph.nodes.keys().map(|id| (id.as_str(), 0)).collect();
    for edge in &graph.edges {
        if let Some(count) = degree.get_mut(edge.from.as_str()) {
            *count += 1;
        }
        if let Some(count) = degree.get_mut(edge.to.as_str()) {
            *count += 1;
        }
    }
    degree
}

/// The `n` best-connected files and symbols, highest degree first (ties by id).
pub fn god_nodes(graph: &Graph, n: usize) -> Vec<(&Node, usize)> {
    let degree = degrees(graph);
    let mut ranked: Vec<(&Node, usize)> = graph
        .nodes
        .values()
        .filter(|node| matches!(node.kind, NodeKind::File | NodeKind::Symbol { .. }))
        .map(|node| (node, degree.get(node.id.as_str()).copied().unwrap_or(0)))
        .collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.id.cmp(&b.0.id)));
    ranked.truncate(n);
    ranked
}

/// Communities of every file and symbol node.
///
/// Label propagation over the undirected file graph. Extracted edges (imports and symbol
/// containment) weigh `EXTRACTED_WEIGHT`; inferred references weigh `INFERRED_WEIGHT`, a
/// quarter of that, so they only break ties. Files without any extracted edge are not
/// propagated: they are grouped by top-level directory instead (see `directory_key`).
/// Nodes are visited in sorted order and ties go to the smallest label, so the result is
/// the same on every build. Communities are renumbered by size, largest first.
/// Symbols take the community of their file; external nodes get none.
pub fn communities(graph: &Graph) -> BTreeMap<String, u32> {
    let keys: Vec<&str> =
        graph.nodes.values().filter(|node| matches!(node.kind, NodeKind::File)).filter_map(|node| node.file.as_deref()).collect();
    let index: BTreeMap<&str, usize> = keys.iter().enumerate().map(|(i, key)| (*key, i)).collect();

    let mut linked = vec![false; keys.len()];
    let mut pairs: Vec<(usize, usize, u32)> = Vec::new();
    for edge in &graph.edges {
        let weight = match (edge.provenance, edge.kind) {
            (Provenance::Extracted, EdgeKind::Imports | EdgeKind::Contains) => EXTRACTED_WEIGHT,
            (Provenance::Inferred, EdgeKind::References) => INFERRED_WEIGHT,
            _ => continue,
        };
        let from = file_key(graph, &edge.from).and_then(|key| index.get(key)).copied();
        let to = file_key(graph, &edge.to).and_then(|key| index.get(key)).copied();
        if let (Some(a), Some(b)) = (from, to)
            && a != b
        {
            if weight == EXTRACTED_WEIGHT {
                linked[a] = true;
                linked[b] = true;
            }
            pairs.push((a, b, weight));
        }
    }
    // Inferred edges only count between files that also have an extracted edge.
    let mut adjacent: Vec<BTreeMap<usize, u32>> = vec![BTreeMap::new(); keys.len()];
    for (a, b, weight) in pairs {
        if linked[a] && linked[b] {
            *adjacent[a].entry(b).or_insert(0) += weight;
            *adjacent[b].entry(a).or_insert(0) += weight;
        }
    }

    let mut label: Vec<usize> = (0..keys.len()).collect();
    for _ in 0..MAX_ROUNDS {
        let mut changed = false;
        for (i, neighbours) in adjacent.iter().enumerate() {
            if neighbours.is_empty() {
                continue;
            }
            let mut counts: BTreeMap<usize, u32> = BTreeMap::new();
            for (&j, &weight) in neighbours {
                *counts.entry(label[j]).or_insert(0) += weight;
            }
            // Ascending labels with a strict comparison: ties keep the smallest label.
            let mut best = (label[i], 0);
            for (&candidate, &count) in &counts {
                if count > best.1 {
                    best = (candidate, count);
                }
            }
            if best.0 != label[i] {
                label[i] = best.0;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }

    // Linked files group by label; unlinked files group by directory, so they never stay singletons.
    let mut groups: BTreeMap<(usize, String), Vec<usize>> = BTreeMap::new();
    for (i, &group) in label.iter().enumerate() {
        let key = if linked[i] { (group, String::new()) } else { (usize::MAX, directory_key(keys[i])) };
        groups.entry(key).or_default().push(i);
    }
    let mut ordered: Vec<Vec<usize>> = groups.into_values().collect();
    ordered.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.first().cmp(&b.first())));

    let mut file_community: BTreeMap<&str, u32> = BTreeMap::new();
    for (community, members) in ordered.iter().enumerate() {
        let community = u32::try_from(community).unwrap_or(u32::MAX);
        for &member in members {
            file_community.insert(keys[member], community);
        }
    }

    graph
        .nodes
        .values()
        .filter_map(|node| match node.kind {
            NodeKind::External => None,
            _ => node.file.as_deref().and_then(|file| file_community.get(file)).map(|c| (node.id.clone(), *c)),
        })
        .collect()
}

/// Edges whose endpoints lie in different communities, in edge order, at most `n`.
pub fn surprising_edges(graph: &Graph, n: usize) -> Vec<&Edge> {
    graph
        .edges
        .iter()
        .filter(|edge| match (graph.communities.get(&edge.from), graph.communities.get(&edge.to)) {
            (Some(a), Some(b)) => a != b,
            _ => false,
        })
        .take(n)
        .collect()
}

/// Grouping key for unlinked files: `crates/<name>` under `crates/`, otherwise the first
/// path component. Root-level files share the empty key.
fn directory_key(path: &str) -> String {
    let mut parts = path.split('/');
    match parts.next() {
        Some("crates") if path.matches('/').count() >= 2 => format!("crates/{}", parts.next().unwrap_or_default()),
        Some(first) if path.contains('/') => first.to_owned(),
        _ => String::new(),
    }
}

/// Repository-relative file path a node belongs to; `None` for external nodes.
fn file_key<'a>(graph: &'a Graph, id: &str) -> Option<&'a str> {
    let node = graph.nodes.get(id)?;
    match node.kind {
        NodeKind::External => None,
        _ => node.file.as_deref(),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::model::{GRAPH_VERSION, Provenance};

    fn file(path: &str) -> Node {
        Node { id: format!("file:{path}"), kind: NodeKind::File, name: path.into(), file: Some(path.into()), line: None, lang: None }
    }

    fn import(graph: &mut Graph, from: &str, to: &str) {
        graph.edges.push(Edge {
            from: format!("file:{from}"),
            to: format!("file:{to}"),
            kind: EdgeKind::Imports,
            provenance: Provenance::Extracted,
        });
    }

    #[test]
    fn linked_files_share_a_community_and_isolated_ones_do_not() {
        let mut graph = Graph {
            version: GRAPH_VERSION,
            root: String::new(),
            nodes: BTreeMap::new(),
            edges: Vec::new(),
            file_hashes: BTreeMap::new(),
            communities: BTreeMap::new(),
        };
        for path in ["a.rs", "b.rs", "c.rs", "lonely.rs"] {
            graph.nodes.insert(format!("file:{path}"), file(path));
        }
        import(&mut graph, "a.rs", "b.rs");
        import(&mut graph, "b.rs", "c.rs");
        let first = communities(&graph);
        let second = communities(&graph);
        assert_eq!(first, second);
        assert_eq!(first["file:a.rs"], first["file:c.rs"]);
        assert_ne!(first["file:lonely.rs"], first["file:a.rs"]);
        assert_eq!(first["file:lonely.rs"], 1, "the singleton is smaller, so it is numbered last");
    }

    fn reference(graph: &mut Graph, from: &str, to: &str) {
        graph.edges.push(Edge {
            from: format!("file:{from}"),
            to: format!("file:{to}"),
            kind: EdgeKind::References,
            provenance: Provenance::Inferred,
        });
    }

    #[test]
    fn inferred_cross_references_do_not_merge_import_clusters() {
        let mut graph = Graph {
            version: GRAPH_VERSION,
            root: String::new(),
            nodes: BTreeMap::new(),
            edges: Vec::new(),
            file_hashes: BTreeMap::new(),
            communities: BTreeMap::new(),
        };
        let left = ["crates/a/src/x.rs", "crates/a/src/y.rs", "crates/a/src/z.rs"];
        let right = ["crates/b/src/x.rs", "crates/b/src/y.rs", "crates/b/src/z.rs"];
        for path in left.iter().chain(&right).chain(&["crates/c/src/p.rs", "crates/c/src/q.rs"]) {
            graph.nodes.insert(format!("file:{path}"), file(path));
        }
        for cluster in [left, right] {
            import(&mut graph, cluster[0], cluster[1]);
            import(&mut graph, cluster[1], cluster[2]);
            import(&mut graph, cluster[2], cluster[0]);
        }
        for (a, b) in left.iter().zip(&right) {
            reference(&mut graph, a, b);
            reference(&mut graph, b, a);
        }
        let found = communities(&graph);
        let distinct: BTreeSet<u32> = found.values().copied().collect();
        assert!(distinct.len() >= 2, "two import clusters must stay apart, got {distinct:?}");
        assert_eq!(found["file:crates/a/src/x.rs"], found["file:crates/a/src/z.rs"]);
        assert_eq!(found["file:crates/b/src/x.rs"], found["file:crates/b/src/z.rs"]);
        assert_ne!(found["file:crates/a/src/x.rs"], found["file:crates/b/src/x.rs"]);
        assert_eq!(found["file:crates/c/src/p.rs"], found["file:crates/c/src/q.rs"], "unlinked files in one crate are grouped");
    }
}
