//! Native code knowledge graph (Graphify-style): index, communities, queries.
//!
//! The graph is built by regex extraction over the repository, persisted under
//! `<root>/.crusty/graph/`, and queried through [`GraphTool`] instead of reading whole files.
//! Everything is heuristic: edges marked `Inferred` are hints, not facts.

pub mod analyze;
pub mod build;
pub mod extract;
pub mod lang;
pub mod model;
pub mod query;
pub mod report;
pub mod tool;

use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};

use crusty_core::config::GraphifyConfig;
use crusty_core::event::{AppEvent, EventSink};

pub use build::{IndexStats, build_or_update, load, save};
pub use model::{Edge, EdgeKind, GRAPH_VERSION, Graph, Node, NodeKind, Provenance};
pub use tool::GraphTool;

/// Failure to read, write or parse the persisted graph.
#[derive(Debug)]
pub enum GraphError {
    Io(io::Error),
    Json(serde_json::Error),
}

impl fmt::Display for GraphError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GraphError::Io(e) => write!(f, "graph i/o error: {e}"),
            GraphError::Json(e) => write!(f, "graph serialization error: {e}"),
        }
    }
}

impl std::error::Error for GraphError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            GraphError::Io(e) => Some(e),
            GraphError::Json(e) => Some(e),
        }
    }
}

impl From<io::Error> for GraphError {
    fn from(e: io::Error) -> Self {
        GraphError::Io(e)
    }
}

impl From<serde_json::Error> for GraphError {
    fn from(e: serde_json::Error) -> Self {
        GraphError::Json(e)
    }
}

/// The graph of one repository, built lazily and kept in memory.
pub struct CodeGraph {
    root: PathBuf,
    cfg: GraphifyConfig,
    graph: RwLock<Option<Graph>>,
}

impl CodeGraph {
    /// A graph for the repository at `root`. Nothing is read or built until it is needed.
    pub fn new(root: impl Into<PathBuf>, cfg: GraphifyConfig) -> Self {
        CodeGraph { root: root.into(), cfg, graph: RwLock::new(None) }
    }

    pub fn config(&self) -> &GraphifyConfig {
        &self.cfg
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// `<root>/.crusty/graph/graph.json`.
    pub fn graph_path(&self) -> PathBuf {
        self.root.join(".crusty").join("graph").join("graph.json")
    }

    /// `<root>/.crusty/graph/GRAPH_REPORT.md`.
    pub fn report_path(&self) -> PathBuf {
        self.root.join(".crusty").join("graph").join("GRAPH_REPORT.md")
    }

    fn read(&self) -> RwLockReadGuard<'_, Option<Graph>> {
        self.graph.read().unwrap_or_else(PoisonError::into_inner)
    }

    fn write(&self) -> RwLockWriteGuard<'_, Option<Graph>> {
        self.graph.write().unwrap_or_else(PoisonError::into_inner)
    }

    /// Runs `f` on the in-memory graph; `None` when no graph is loaded yet.
    pub fn with_graph<R>(&self, f: impl FnOnce(&Graph) -> R) -> Option<R> {
        self.read().as_ref().map(f)
    }

    /// Rebuilds the graph (incrementally when possible), stores it and writes the report.
    pub fn rebuild(&self) -> Result<IndexStats, GraphError> {
        let previous = self.write().take().or_else(|| load(&self.graph_path()).ok());
        let (graph, stats) = build_or_update(&self.root, &self.cfg, previous)?;
        let report_text = report::render(&graph, &stats);
        *self.write() = Some(graph);
        let path = self.graph_path();
        self.with_graph(|g| save(g, &path)).unwrap_or(Ok(()))?;
        fs::write(self.report_path(), report_text)?;
        Ok(stats)
    }

    /// Rebuilds and reports the result as a `GraphIndexed` event.
    pub fn refresh(&self, events: &EventSink) -> Result<IndexStats, GraphError> {
        let stats = self.rebuild()?;
        emit_indexed(events, &stats);
        Ok(stats)
    }

    /// Makes sure a graph is available: loads the persisted one, or builds it. Returns the
    /// build stats when a build ran.
    pub fn ensure_built(&self) -> Result<Option<IndexStats>, GraphError> {
        if self.with_graph(|_| ()).is_some() {
            return Ok(None);
        }
        if let Ok(graph) = load(&self.graph_path())
            && graph.version == GRAPH_VERSION
        {
            *self.write() = Some(graph);
            return Ok(None);
        }
        self.rebuild().map(Some)
    }

    /// The repository map, followed by the external Graphify report excerpt when enabled.
    pub fn repo_map(&self, budget_tokens: u64) -> Option<String> {
        let mut text = self.with_graph(|g| query::repo_map(g, budget_tokens))?;
        if self.cfg.use_external_report
            && let Some(external) = report::external_report(&self.root)
        {
            text.push_str("\n## External Graphify report\n");
            text.push_str(&external);
        }
        Some(text)
    }
}

/// Emits the `GraphIndexed` event for a finished build.
pub fn emit_indexed(events: &EventSink, stats: &IndexStats) {
    events.emit(AppEvent::GraphIndexed {
        files: stats.files,
        symbols: stats.symbols,
        edges: stats.edges,
        communities: stats.communities,
        duration_ms: stats.duration_ms,
    });
}
