//! The `NotebookEdit` tool: replaces, inserts or deletes one cell of a Jupyter notebook.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use crusty_core::message::ToolSpec;
use crusty_core::permission::RiskAssessment;
use crusty_core::tool::{Tool, ToolCategory, ToolContext, ToolError, ToolOutput, input_opt_str, input_str};
use serde::Serialize;
use serde_json::{Value, json};

use crate::sandbox::{display_path, resolve_path};
use crate::tracker::FileTracker;
use crate::write::{file_edit_risk, write_atomic};
use crate::{file_subject, opt_u64};

/// Edits one cell of an `.ipynb` file, keeping every other field as it is.
pub struct NotebookEditTool {
    tracker: Arc<FileTracker>,
}

impl NotebookEditTool {
    pub fn new(tracker: Arc<FileTracker>) -> Self {
        NotebookEditTool { tracker }
    }
}

/// Source text as nbformat stores it: a list of lines, each keeping its newline.
fn source_lines(source: &str) -> Value {
    Value::Array(source.split_inclusive('\n').map(|line| Value::String(line.to_owned())).collect())
}

/// Index of the cell named by `cell_id` or `cell_number` (0-based), if either is given.
fn locate(cells: &[Value], input: &Value) -> Result<Option<usize>, ToolError> {
    if let Some(id) = input_opt_str(input, "cell_id") {
        return cells
            .iter()
            .position(|cell| cell.get("id").and_then(Value::as_str) == Some(id))
            .map(Some)
            .ok_or_else(|| ToolError::Failed(format!("no cell with id `{id}`")));
    }
    match opt_u64(input, "cell_number")? {
        Some(n) => {
            let index = usize::try_from(n).unwrap_or(usize::MAX);
            if index >= cells.len() {
                return Err(ToolError::Failed(format!("cell_number {n} is out of range (the notebook has {} cells)", cells.len())));
            }
            Ok(Some(index))
        }
        None => Ok(None),
    }
}

/// A fresh cell id in nbformat's allowed character set.
fn new_cell_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    format!("cell-{:x}{:x}", nanos as u64, COUNTER.fetch_add(1, Ordering::Relaxed))
}

/// Serialises with nbformat's one-space indentation.
fn to_notebook_bytes(notebook: &Value) -> Result<Vec<u8>, ToolError> {
    let mut buf = Vec::new();
    let formatter = serde_json::ser::PrettyFormatter::with_indent(b" ");
    let mut ser = serde_json::Serializer::with_formatter(&mut buf, formatter);
    notebook.serialize(&mut ser).map_err(|e| ToolError::Failed(e.to_string()))?;
    buf.push(b'\n');
    Ok(buf)
}

/// Applies the requested edit to the cells in place. Returns a short description.
fn apply(cells: &mut Vec<Value>, input: &Value) -> Result<String, ToolError> {
    let mode = input_opt_str(input, "edit_mode").unwrap_or("replace");
    let target = locate(cells, input)?;
    let cell_type = input_opt_str(input, "cell_type");
    if let Some(kind) = cell_type
        && kind != "code"
        && kind != "markdown"
    {
        return Err(ToolError::InvalidInput(format!("cell_type must be `code` or `markdown`, not `{kind}`")));
    }

    match mode {
        "replace" => {
            let index = target.ok_or_else(|| ToolError::InvalidInput("replace needs cell_id or cell_number".into()))?;
            let source = input_str(input, "new_source")?;
            let cell = cells[index].as_object_mut().ok_or_else(|| ToolError::Failed(format!("cell {index} is not an object")))?;
            cell.insert("source".into(), source_lines(source));
            if let Some(kind) = cell_type
                && cell.get("cell_type").and_then(Value::as_str) != Some(kind)
            {
                cell.insert("cell_type".into(), json!(kind));
                if kind == "markdown" {
                    cell.remove("outputs");
                    cell.remove("execution_count");
                } else {
                    cell.entry("outputs").or_insert(json!([]));
                    cell.entry("execution_count").or_insert(Value::Null);
                }
            }
            Ok(format!("replaced cell {index}"))
        }
        "insert" => {
            let source = input_str(input, "new_source")?;
            let kind = cell_type.unwrap_or("code");
            let mut cell = json!({"cell_type": kind, "metadata": {}, "source": source_lines(source)});
            if kind == "code" {
                cell["outputs"] = json!([]);
                cell["execution_count"] = Value::Null;
            }
            if cells.iter().any(|c| c.get("id").is_some()) {
                cell["id"] = json!(new_cell_id());
            }
            // Insert after the referenced cell, or at the top when no cell is named.
            let at = target.map_or(0, |index| index + 1);
            cells.insert(at, cell);
            Ok(format!("inserted cell {at}"))
        }
        "delete" => {
            let index = target.ok_or_else(|| ToolError::InvalidInput("delete needs cell_id or cell_number".into()))?;
            cells.remove(index);
            Ok(format!("deleted cell {index}"))
        }
        other => Err(ToolError::InvalidInput(format!("unknown edit_mode `{other}`"))),
    }
}

#[async_trait]
impl Tool for NotebookEditTool {
    fn name(&self) -> &str {
        "NotebookEdit"
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "NotebookEdit",
            "Replaces, inserts or deletes one cell of a Jupyter notebook (.ipynb). Cells are addressed by cell_id, or by \
             cell_number (0-based). Insert places the new cell after the addressed cell, or first when none is given.",
            json!({
                "type": "object",
                "properties": {
                    "notebook_path": {"type": "string"},
                    "cell_id": {"type": "string"},
                    "cell_number": {"type": "integer", "minimum": 0},
                    "new_source": {"type": "string", "description": "New cell content (not needed for delete)."},
                    "cell_type": {"type": "string", "enum": ["code", "markdown"]},
                    "edit_mode": {"type": "string", "enum": ["replace", "insert", "delete"]}
                },
                "required": ["notebook_path"]
            }),
        )
    }

    fn category(&self) -> ToolCategory {
        ToolCategory::Edit
    }

    fn assess(&self, input: &Value, ctx: &ToolContext) -> RiskAssessment {
        file_edit_risk(ctx, input, "notebook_path")
    }

    fn permission_subject(&self, input: &Value) -> Option<String> {
        file_subject(input, "notebook_path")
    }

    async fn call(&self, input: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let path = resolve_path(ctx, input_str(&input, "notebook_path")?)?;
        if path.extension().and_then(|e| e.to_str()) != Some("ipynb") {
            return Err(ToolError::InvalidInput("notebook_path must end in .ipynb".into()));
        }
        self.tracker.check_can_write(&path)?;
        let text = std::fs::read_to_string(&path)?;
        let mut notebook: Value = serde_json::from_str(&text).map_err(|e| ToolError::Failed(format!("not a valid notebook: {e}")))?;
        let cells = notebook
            .get_mut("cells")
            .and_then(Value::as_array_mut)
            .ok_or_else(|| ToolError::Failed("notebook has no `cells` array".into()))?;
        let summary = apply(cells, &input)?;
        write_atomic(&path, &to_notebook_bytes(&notebook)?)?;
        self.tracker.record_write(&path);
        Ok(ToolOutput::text(format!("{summary} in {}", display_path(&path))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::context;
    use tempfile::tempdir;

    const NOTEBOOK: &str = r##"{
 "cells": [
  {"cell_type": "code", "execution_count": null, "id": "first", "metadata": {}, "outputs": [], "source": ["x = 1\n"]},
  {"cell_type": "markdown", "id": "second", "metadata": {"tags": ["keep"]}, "source": ["# Title"]}
 ],
 "metadata": {"kernelspec": {"name": "python3"}},
 "nbformat": 4,
 "nbformat_minor": 5
}
"##;

    /// Writes the sample notebook into the workspace, marks it read, and returns its path.
    fn setup(ctx: &ToolContext, tracker: &FileTracker) -> std::path::PathBuf {
        let path = ctx.workspace.join("nb.ipynb");
        std::fs::write(&path, NOTEBOOK).unwrap();
        tracker.record_read(&path);
        path
    }

    fn load(path: &std::path::Path) -> Value {
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    #[tokio::test]
    async fn replace_by_id_keeps_other_fields() {
        let dir = tempdir().unwrap();
        let ctx = context(&dir);
        let tracker = Arc::new(FileTracker::new());
        let path = setup(&ctx, &tracker);
        let tool = NotebookEditTool::new(tracker);
        tool.call(json!({"notebook_path": "nb.ipynb", "cell_id": "first", "new_source": "y = 2\nz = 3"}), &ctx).await.unwrap();
        let nb = load(&path);
        assert_eq!(nb["cells"][0]["source"], json!(["y = 2\n", "z = 3"]));
        assert_eq!(nb["cells"][0]["execution_count"], Value::Null);
        assert_eq!(nb["cells"][1]["metadata"]["tags"], json!(["keep"]));
        assert_eq!(nb["metadata"]["kernelspec"]["name"], "python3");
    }

    #[tokio::test]
    async fn insert_after_cell_number() {
        let dir = tempdir().unwrap();
        let ctx = context(&dir);
        let tracker = Arc::new(FileTracker::new());
        let path = setup(&ctx, &tracker);
        let tool = NotebookEditTool::new(tracker);
        tool.call(
            json!({"notebook_path": "nb.ipynb", "cell_number": 0, "new_source": "new", "edit_mode": "insert", "cell_type": "markdown"}),
            &ctx,
        )
        .await
        .unwrap();
        let nb = load(&path);
        let cells = nb["cells"].as_array().unwrap();
        assert_eq!(cells.len(), 3);
        assert_eq!(cells[1]["cell_type"], "markdown");
        assert_eq!(cells[1]["source"], json!(["new"]));
        assert!(cells[1]["id"].as_str().is_some());
    }

    #[tokio::test]
    async fn delete_removes_the_cell() {
        let dir = tempdir().unwrap();
        let ctx = context(&dir);
        let tracker = Arc::new(FileTracker::new());
        let path = setup(&ctx, &tracker);
        let tool = NotebookEditTool::new(tracker);
        tool.call(json!({"notebook_path": "nb.ipynb", "cell_id": "first", "edit_mode": "delete"}), &ctx).await.unwrap();
        let nb = load(&path);
        assert_eq!(nb["cells"].as_array().unwrap().len(), 1);
        assert_eq!(nb["cells"][0]["id"], "second");
    }

    #[tokio::test]
    async fn unknown_cell_and_out_of_range_are_errors() {
        let dir = tempdir().unwrap();
        let ctx = context(&dir);
        let tracker = Arc::new(FileTracker::new());
        setup(&ctx, &tracker);
        let tool = NotebookEditTool::new(tracker);
        let unknown = tool.call(json!({"notebook_path": "nb.ipynb", "cell_id": "nope", "new_source": "x"}), &ctx).await;
        assert!(matches!(unknown, Err(ToolError::Failed(m)) if m.contains("no cell with id")));
        let range = tool.call(json!({"notebook_path": "nb.ipynb", "cell_number": 9, "new_source": "x"}), &ctx).await;
        assert!(matches!(range, Err(ToolError::Failed(m)) if m.contains("out of range")));
    }

    #[tokio::test]
    async fn rejects_non_notebook_paths() {
        let dir = tempdir().unwrap();
        let ctx = context(&dir);
        let tool = NotebookEditTool::new(Arc::new(FileTracker::new()));
        let result = tool.call(json!({"notebook_path": "x.py", "cell_number": 0, "new_source": "x"}), &ctx).await;
        assert!(matches!(result, Err(ToolError::InvalidInput(_))));
    }
}
