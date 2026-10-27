use std::fs;
use std::path::Path;
use std::sync::Arc;

use crusty_core::config::GraphifyConfig;
use crusty_core::event::{AppEvent, EventSink};
use crusty_core::tokens::estimate_text;
use crusty_core::tool::Tool;
use crusty_core::{Stage, StageStatus, ToolContext};
use crusty_graph::analyze;
use crusty_graph::build::{build_or_update, load, save};
use crusty_graph::model::{EdgeKind, Graph, NodeKind, Provenance};
use crusty_graph::{CodeGraph, GraphTool, IndexStats, query, report};
use serde_json::json;
use tokio::sync::mpsc;

fn write(root: &Path, rel: &str, text: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

fn cfg() -> GraphifyConfig {
    GraphifyConfig::default()
}

fn build(root: &Path, previous: Option<Graph>) -> (Graph, IndexStats) {
    build_or_update(root, &cfg(), previous).unwrap()
}

fn has_edge(graph: &Graph, from: &str, to: &str, kind: EdgeKind) -> bool {
    graph.edges.iter().any(|e| e.from == from && e.to == to && e.kind == kind)
}

fn rust_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "Cargo.toml", "[package]\nname = \"demo\"\n");
    write(
        dir.path(),
        "src/lib.rs",
        "mod util;\nuse crate::util::helper;\nuse serde::Deserialize;\n\npub fn run() -> u32 {\n    helper()\n}\n",
    );
    write(dir.path(), "src/util.rs", "pub fn helper() -> u32 {\n    1\n}\n\npub struct Widget;\n");
    dir
}

#[test]
fn rust_symbols_lines_and_module_resolution() {
    let dir = rust_repo();
    let (graph, stats) = build(dir.path(), None);
    let helper = &graph.nodes["sym:src/util.rs#helper"];
    assert_eq!(helper.line, Some(1));
    assert!(matches!(&helper.kind, NodeKind::Symbol { kind } if kind == "fn"));
    assert!(graph.nodes.contains_key("sym:src/util.rs#Widget"));
    assert!(has_edge(&graph, "file:src/lib.rs", "file:src/util.rs", EdgeKind::Imports));
    assert!(graph.nodes.contains_key("ext:serde::Deserialize"));
    assert_eq!(stats.files, 2);
}

#[test]
fn python_package_imports_resolve_to_files() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "pkg/__init__.py", "");
    write(dir.path(), "pkg/a.py", "from pkg.b import thing\nimport os\n\ndef use():\n    return thing()\n");
    write(dir.path(), "pkg/b.py", "def thing():\n    return 1\n");
    let (graph, _) = build(dir.path(), None);
    assert!(has_edge(&graph, "file:pkg/a.py", "file:pkg/b.py", EdgeKind::Imports));
    assert!(graph.nodes.contains_key("ext:os"));
    assert!(graph.nodes.contains_key("sym:pkg/b.py#thing"));
}

#[test]
fn typescript_relative_imports_and_arrow_functions() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "index.ts",
        "import { add } from './math';\nimport React from 'react';\n\nexport function main() { return add(1, 2); }\n",
    );
    write(
        dir.path(),
        "math.ts",
        "export function add(a: number, b: number): number {\n  return a + b;\n}\n\nexport const multiply = (a: number, b: number) => a * b;\n",
    );
    let (graph, _) = build(dir.path(), None);
    assert!(has_edge(&graph, "file:index.ts", "file:math.ts", EdgeKind::Imports));
    assert!(graph.nodes.contains_key("ext:react"));
    assert!(matches!(&graph.nodes["sym:math.ts#multiply"].kind, NodeKind::Symbol { kind } if kind == "function"));
}

#[test]
fn references_are_inferred_across_files() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "shop/pricing.py", "def calculate_discount(total):\n    return total * 0.9\n");
    write(
        dir.path(),
        "shop/cart.py",
        "from shop.pricing import calculate_discount\n\ndef checkout(total):\n    return calculate_discount(total)\n",
    );
    let (graph, _) = build(dir.path(), None);
    let reference = graph.edges.iter().find(|e| e.kind == EdgeKind::References && e.from == "file:shop/cart.py").expect("reference edge");
    assert_eq!(reference.to, "sym:shop/pricing.py#calculate_discount");
    assert_eq!(reference.provenance, Provenance::Inferred);
}

#[test]
fn communities_are_deterministic_and_group_linked_files() {
    let dir = rust_repo();
    write(dir.path(), "scripts/tool.py", "def go():\n    pass\n");
    let (first, _) = build(dir.path(), None);
    let (second, _) = build(dir.path(), None);
    assert_eq!(first.communities, second.communities);
    assert_eq!(first.communities["file:src/lib.rs"], first.communities["file:src/util.rs"]);
    assert_ne!(first.communities["file:src/lib.rs"], first.communities["file:scripts/tool.py"]);
    assert_eq!(first.communities["sym:src/util.rs#helper"], first.communities["file:src/util.rs"]);
}

#[test]
fn incremental_build_reuses_unchanged_and_drops_deleted() {
    let dir = rust_repo();
    write(dir.path(), "src/extra.rs", "pub fn extra_fn() {}\n");
    let (first, _) = build(dir.path(), None);
    write(dir.path(), "src/util.rs", "pub fn helper() -> u32 {\n    2\n}\n");
    fs::remove_file(dir.path().join("src/extra.rs")).unwrap();
    let (second, stats) = build(dir.path(), Some(first));
    assert_eq!(stats.reused_files, 1);
    assert!(!second.nodes.contains_key("file:src/extra.rs"));
    assert!(!second.nodes.contains_key("sym:src/extra.rs#extra_fn"));
    let (fresh, _) = build(dir.path(), None);
    assert_eq!(second.nodes, fresh.nodes);
    assert_eq!(second.edges, fresh.edges);
    assert_eq!(second.communities, fresh.communities);
}

#[test]
fn find_explain_neighbors_and_path_answer_from_the_graph() {
    let dir = rust_repo();
    let (graph, _) = build(dir.path(), None);
    let hits = query::find(&graph, "HELP", 10);
    assert!(hits.iter().any(|n| n.id == "sym:src/util.rs#helper"));
    let explained = query::explain(&graph, "src/lib.rs").unwrap();
    assert!(explained.contains("File src/lib.rs"));
    assert!(explained.contains("Imports"));
    assert!(explained.contains("src/util.rs"));
    let symbol = query::explain(&graph, "helper").unwrap();
    assert!(symbol.contains("src/util.rs:1"));
    let near = query::neighbors(&graph, "file:src/lib.rs", 1);
    assert!(near.contains(&"file:src/util.rs".to_owned()));
    let route = query::path(&graph, "src/lib.rs", "helper").unwrap();
    assert_eq!(route.first().map(String::as_str), Some("file:src/lib.rs"));
    assert_eq!(route.last().map(String::as_str), Some("sym:src/util.rs#helper"));
    assert!(query::path(&graph, "src/lib.rs", "no_such_thing").is_none());
}

#[test]
fn repo_map_respects_budget_and_lists_entry_points() {
    let dir = rust_repo();
    let (graph, _) = build(dir.path(), None);
    let map = query::repo_map(&graph, 400);
    assert!(estimate_text(&map) <= 400);
    assert!(map.contains("Entry points"));
    assert!(map.contains("src/lib.rs"));
    assert!(map.contains("Cargo.toml"));
    let tiny = query::repo_map(&graph, 20);
    assert!(estimate_text(&tiny) <= 20);
}

#[test]
fn report_lists_god_nodes_and_provenance_legend() {
    let dir = rust_repo();
    let (graph, stats) = build(dir.path(), None);
    assert!(!analyze::god_nodes(&graph, 3).is_empty());
    let text = report::render(&graph, &stats);
    assert!(text.contains("God nodes"));
    assert!(text.contains("Extracted"));
    assert!(text.contains("Inferred"));
}

#[test]
fn save_then_load_round_trips() {
    let dir = rust_repo();
    let (graph, _) = build(dir.path(), None);
    let path = dir.path().join(".crusty/graph/graph.json");
    save(&graph, &path).unwrap();
    assert_eq!(load(&path).unwrap(), graph);
}

#[test]
fn code_graph_persists_and_serves_repo_map() {
    let dir = rust_repo();
    let graph = CodeGraph::new(dir.path(), cfg());
    let stats = graph.rebuild().unwrap();
    assert_eq!(stats.files, 2);
    assert!(dir.path().join(".crusty/graph/graph.json").exists());
    assert!(dir.path().join(".crusty/graph/GRAPH_REPORT.md").exists());
    assert!(graph.repo_map(400).unwrap().contains("Repository map"));
}

#[tokio::test]
async fn graph_tool_answers_and_emits_metrics() {
    let dir = rust_repo();
    let tool = GraphTool::new(Arc::new(CodeGraph::new(dir.path(), cfg())));
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut ctx = ToolContext::for_tests(dir.path());
    ctx.events = EventSink::new(tx);

    let out = tool.call(json!({"action": "find", "query": "helper"}), &ctx).await.unwrap();
    assert!(!out.is_error);
    assert!(out.text_content().contains("helper"));

    let mut metrics = None;
    while let Ok(event) = rx.try_recv() {
        if let AppEvent::Optimizer(m) = event {
            metrics = Some(m);
        }
    }
    let m = metrics.expect("optimizer metrics event");
    assert_eq!(m.stage, Stage::Graphify);
    assert_eq!(m.transformer, "graphify:find");
    assert_eq!(m.status, StageStatus::Applied);
    assert!(m.detail.contains("vs referenced files (est.)"));

    let rebuilt = tool.call(json!({"action": "rebuild"}), &ctx).await.unwrap();
    assert!(rebuilt.text_content().starts_with("Graph rebuilt"));
    let stats = tool.call(json!({"action": "stats"}), &ctx).await.unwrap();
    assert!(stats.text_content().contains("2 files"));
    assert!(tool.call(json!({"action": "nope"}), &ctx).await.is_err());
}
