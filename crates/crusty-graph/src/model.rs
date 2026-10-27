//! Serialized shape of the code graph: nodes, edges, provenance and the persisted document.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Format version of the persisted graph. A file with another version is rebuilt.
pub const GRAPH_VERSION: u32 = 1;

/// What a node stands for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum NodeKind {
    /// A source file of the repository.
    File,
    /// A definition inside a file; `kind` is its flavour (`fn`, `struct`, `heading`, ...).
    Symbol { kind: String },
    /// An import target outside the repository.
    External,
}

/// A vertex of the graph.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Node {
    /// `file:<path>`, `sym:<path>#<name>` or `ext:<module>`.
    pub id: String,
    pub kind: NodeKind,
    pub name: String,
    /// Repository-relative path with forward slashes.
    pub file: Option<String>,
    /// 1-based line of a symbol.
    pub line: Option<u32>,
    pub lang: Option<String>,
}

/// Relation between two nodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeKind {
    /// A file contains a symbol.
    Contains,
    /// A file imports another file or an external module.
    Imports,
    /// A file mentions a symbol defined in another file.
    References,
}

/// How an edge was obtained.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provenance {
    /// Read directly from the source text.
    Extracted,
    /// Derived by a heuristic; a hint, not a fact.
    Inferred,
}

/// A directed edge. Every edge starts at a file node.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Edge {
    pub from: String,
    pub to: String,
    pub kind: EdgeKind,
    pub provenance: Provenance,
}

/// The persisted graph document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Graph {
    pub version: u32,
    /// Repository root the graph was built from, with forward slashes.
    pub root: String,
    pub nodes: BTreeMap<String, Node>,
    pub edges: Vec<Edge>,
    /// SHA-256 of each indexed file, keyed by its relative path.
    pub file_hashes: BTreeMap<String, String>,
    /// Community of every file and symbol node.
    pub communities: BTreeMap<String, u32>,
}
