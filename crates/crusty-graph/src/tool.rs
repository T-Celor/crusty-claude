//! The `Graph` tool: structure questions answered from the persisted graph, with an
//! optimizer metric comparing the answer size to the files it refers to.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use crusty_core::tokens::estimate_text;
use crusty_core::tool::input_str;
use crusty_core::{
    AppEvent, RiskAssessment, RiskLevel, Stage, StageMetrics, StageStatus, Tool, ToolCategory, ToolContext, ToolError, ToolOutput,
    ToolSpec, async_trait,
};
use serde_json::{Value, json};

use crate::model::{Graph, NodeKind};
use crate::{CodeGraph, GraphError, emit_indexed, query};

const DESCRIPTION: &str = "Answers repository structure questions from a persistent code graph (files, symbols, imports, \
inferred references, communities). Cheaper than reading files. Actions: find (query), explain (target: file path or \
symbol name), neighbors (target, depth 1-3), path (query = start, target = end), map (repository overview), stats, \
rebuild (re-index the repository).";

/// Tool exposing the code graph to agents.
pub struct GraphTool {
    graph: Arc<CodeGraph>,
}

impl GraphTool {
    pub fn new(graph: Arc<CodeGraph>) -> Self {
        Self { graph }
    }
}

fn schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "action": {"type": "string", "enum": ["find", "explain", "neighbors", "path", "map", "stats", "rebuild"]},
            "query": {"type": "string", "description": "find: text to match; path: start node"},
            "target": {"type": "string", "description": "explain/neighbors: file path or symbol; path: end node"},
            "depth": {"type": "integer", "minimum": 1, "maximum": 3},
            "limit": {"type": "integer", "minimum": 1, "maximum": 100}
        },
        "required": ["action"]
    })
}

#[async_trait]
impl Tool for GraphTool {
    fn name(&self) -> &str {
        "Graph"
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec::new("Graph", DESCRIPTION, schema())
    }

    fn category(&self) -> ToolCategory {
        ToolCategory::Read
    }

    fn assess(&self, _input: &Value, _ctx: &ToolContext) -> RiskAssessment {
        RiskAssessment::new(RiskLevel::Low, "reads the code graph")
    }

    async fn call(&self, input: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let action = input_str(&input, "action")?.to_owned();
        if !self.graph.config().enabled {
            return Ok(ToolOutput::error("Graphify is disabled in the configuration."));
        }
        if action == "rebuild" {
            let graph = Arc::clone(&self.graph);
            let stats = run_blocking(move || graph.rebuild()).await?;
            emit_indexed(&ctx.events, &stats);
            return Ok(ToolOutput::text(format!(
                "Graph rebuilt: {} files, {} symbols, {} edges, {} communities, {} files reused, {} ms.",
                stats.files, stats.symbols, stats.edges, stats.communities, stats.reused_files, stats.duration_ms
            )));
        }

        let graph = Arc::clone(&self.graph);
        let built = run_blocking(move || graph.ensure_built()).await?;
        // Known only when this call built the index; a graph built earlier reports nothing here.
        let skipped = built.as_ref().map_or(0, |stats| stats.skipped_files);
        if let Some(stats) = &built {
            emit_indexed(&ctx.events, stats);
        }

        let started = Instant::now();
        let budget = self.graph.config().repo_map_tokens;
        let answered = self.graph.with_graph(|g| answer(g, budget, &action, &input, skipped));
        let (text, files) = match answered {
            Some(result) => result?,
            None => return Err(ToolError::Failed("the code graph is not available".into())),
        };

        let source_bytes = source_bytes(self.graph.root(), &files);
        let mut metrics = StageMetrics::new(Stage::Graphify, "graph_query", format!("graphify:{action}"));
        metrics.status = StageStatus::Applied;
        metrics.detail = format!("{} referenced file(s); tokens vs referenced files (est.)", files.len());
        metrics.input_bytes = source_bytes;
        metrics.output_bytes = u64::try_from(text.len()).unwrap_or(u64::MAX);
        metrics.tokens_before_est = source_bytes / 4;
        metrics.tokens_after_est = estimate_text(&text);
        metrics.duration_us = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX);
        metrics.session_id = ctx.session_id.clone();
        metrics.agent_id = ctx.agent_id.clone();
        ctx.events.emit(AppEvent::Optimizer(metrics));

        Ok(ToolOutput::text(text))
    }
}

/// Runs a blocking graph operation off the async runtime.
async fn run_blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T, GraphError> + Send + 'static) -> Result<T, ToolError> {
    tokio::task::spawn_blocking(f).await.map_err(|e| ToolError::Failed(e.to_string()))?.map_err(|e| ToolError::Failed(e.to_string()))
}

/// Reads an optional integer field, clamped to `[min, max]`.
fn bounded(input: &Value, key: &str, default: u64, min: u64, max: u64) -> usize {
    let value = input.get(key).and_then(Value::as_u64).unwrap_or(default).clamp(min, max);
    usize::try_from(value).unwrap_or(usize::MAX)
}

/// Answer text and the files it draws on.
fn answer(graph: &Graph, budget: u64, action: &str, input: &Value, skipped: u64) -> Result<(String, Vec<String>), ToolError> {
    match action {
        "find" => {
            let needle = input_str(input, "query")?;
            let hits = query::find(graph, needle, bounded(input, "limit", 20, 1, 100));
            if hits.is_empty() {
                return Ok((format!("No nodes match `{needle}`."), Vec::new()));
            }
            let files = hits.iter().filter_map(|n| n.file.clone()).collect();
            let text = hits.iter().map(|n| format!("- {}", query::label(graph, &n.id))).collect::<Vec<_>>().join("\n");
            Ok((text, files))
        }
        "explain" => {
            let target = input_str(input, "target")?;
            let text = query::explain(graph, target).unwrap_or_else(|| format!("No node matches `{target}`."));
            let files = query::resolve(graph, target).iter().flat_map(|id| query::related_files(graph, id)).collect();
            Ok((text, files))
        }
        "neighbors" => {
            let target = input_str(input, "target")?;
            let Some(start) = query::resolve(graph, target).into_iter().next() else {
                return Ok((format!("No node matches `{target}`."), Vec::new()));
            };
            let ids = query::neighbors(graph, &start, bounded(input, "depth", 1, 1, 3));
            let mut files = query::related_files(graph, &start);
            files.extend(ids.iter().flat_map(|id| query::related_files(graph, id)));
            let text = ids.iter().map(|id| format!("- {}", query::label(graph, id))).collect::<Vec<_>>().join("\n");
            Ok((if text.is_empty() { "No neighbours.".into() } else { text }, files))
        }
        "path" => {
            let from = input_str(input, "query")?;
            let to = input_str(input, "target")?;
            match query::path(graph, from, to) {
                Some(route) => {
                    let files = route.iter().flat_map(|id| query::related_files(graph, id)).collect();
                    let text = route.iter().map(|id| query::label(graph, id)).collect::<Vec<_>>().join(" -> ");
                    Ok((text, files))
                }
                None => Ok((format!("No path between `{from}` and `{to}`."), Vec::new())),
            }
        }
        "map" => {
            let files = graph.nodes.values().filter(|n| matches!(n.kind, NodeKind::File)).filter_map(|n| n.file.clone()).collect();
            Ok((query::repo_map(graph, budget), files))
        }
        "stats" => {
            let mut text = query::stats_text(graph);
            if skipped > 0 {
                text.push_str(&format!("\n{skipped} file(s) skipped (over the max_files limit), so the graph is partial."));
            }
            Ok((text, Vec::new()))
        }
        other => Err(ToolError::InvalidInput(format!("unknown action `{other}`"))),
    }
}

/// Total size in bytes of the distinct files that exist under `root`.
fn source_bytes(root: &Path, files: &[String]) -> u64 {
    let unique: BTreeSet<&str> = files.iter().map(String::as_str).collect();
    unique.into_iter().filter_map(|file| fs::metadata(root.join(file)).ok()).map(|meta| meta.len()).sum()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde_json::json;

    use super::*;
    use crate::model::GRAPH_VERSION;

    #[test]
    fn stats_mentions_skipped_files_only_when_there_are_some() {
        let graph = Graph {
            version: GRAPH_VERSION,
            root: String::new(),
            nodes: BTreeMap::new(),
            edges: Vec::new(),
            file_hashes: BTreeMap::new(),
            communities: BTreeMap::new(),
        };
        let (text, _) = answer(&graph, 1000, "stats", &json!({}), 3).unwrap();
        assert!(text.contains("3 file(s) skipped"), "{text}");
        let (plain, _) = answer(&graph, 1000, "stats", &json!({}), 0).unwrap();
        assert!(!plain.contains("file(s) skipped"), "{plain}");
    }
}
