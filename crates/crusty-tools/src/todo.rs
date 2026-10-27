//! The `TodoWrite` tool: the agent's plan, published as it changes and kept per agent.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

use async_trait::async_trait;
use crusty_core::event::{AppEvent, TodoItem, TodoStatus};
use crusty_core::message::ToolSpec;
use crusty_core::permission::{RiskAssessment, RiskLevel};
use crusty_core::tool::{Tool, ToolCategory, ToolContext, ToolError, ToolOutput, input_str};
use serde_json::{Value, json};

/// Latest plan of every agent, shared between the tool and whoever displays plans.
#[derive(Debug, Clone, Default)]
pub struct TodoStore {
    plans: Arc<Mutex<HashMap<String, Vec<TodoItem>>>>,
}

impl TodoStore {
    /// The agent's current plan; empty when it has none.
    pub fn get(&self, agent_id: &str) -> Vec<TodoItem> {
        self.lock().get(agent_id).cloned().unwrap_or_default()
    }

    fn set(&self, agent_id: &str, items: Vec<TodoItem>) {
        self.lock().insert(agent_id.to_owned(), items);
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<String, Vec<TodoItem>>> {
        self.plans.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Replaces the calling agent's plan.
pub struct TodoWriteTool {
    store: TodoStore,
}

impl TodoWriteTool {
    pub fn new(store: TodoStore) -> Self {
        TodoWriteTool { store }
    }
}

/// Parses and validates the `todos` array. At most one item may be in progress.
fn parse_todos(input: &Value) -> Result<Vec<TodoItem>, ToolError> {
    let items =
        input.get("todos").and_then(Value::as_array).ok_or_else(|| ToolError::InvalidInput("missing array field `todos`".into()))?;
    let todos = items
        .iter()
        .map(|item| -> Result<TodoItem, ToolError> {
            let content = input_str(item, "content")?.trim().to_owned();
            if content.is_empty() {
                return Err(ToolError::InvalidInput("todo content must not be empty".into()));
            }
            let status = match input_str(item, "status")? {
                "pending" => TodoStatus::Pending,
                "in_progress" => TodoStatus::InProgress,
                "completed" => TodoStatus::Completed,
                other => return Err(ToolError::InvalidInput(format!("unknown status `{other}`"))),
            };
            Ok(TodoItem { content, status })
        })
        .collect::<Result<Vec<_>, _>>()?;
    if todos.iter().filter(|t| t.status == TodoStatus::InProgress).count() > 1 {
        return Err(ToolError::InvalidInput("only one todo may be in_progress at a time".into()));
    }
    Ok(todos)
}

/// One line per item: `☐` pending, `◐` in progress, `☑` completed.
fn render(items: &[TodoItem]) -> String {
    if items.is_empty() {
        return "Todo list cleared.".to_owned();
    }
    items
        .iter()
        .map(|item| {
            let mark = match item.status {
                TodoStatus::Pending => "☐",
                TodoStatus::InProgress => "◐",
                TodoStatus::Completed => "☑",
            };
            format!("{mark} {}", item.content)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[async_trait]
impl Tool for TodoWriteTool {
    fn name(&self) -> &str {
        "TodoWrite"
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "TodoWrite",
            "Creates or replaces the task list for this work. Use it for multi-step tasks and keep exactly one item in_progress. \
             Mark items completed as soon as they are done.",
            json!({
                "type": "object",
                "properties": {
                    "todos": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "content": {"type": "string"},
                                "status": {"type": "string", "enum": ["pending", "in_progress", "completed"]},
                                "activeForm": {"type": "string", "description": "Present-tense form shown while in progress."}
                            },
                            "required": ["content", "status"]
                        }
                    }
                },
                "required": ["todos"]
            }),
        )
    }

    fn category(&self) -> ToolCategory {
        ToolCategory::Other
    }

    fn assess(&self, _input: &Value, _ctx: &ToolContext) -> RiskAssessment {
        RiskAssessment::new(RiskLevel::Low, "updates the task list")
    }

    async fn call(&self, input: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let items = parse_todos(&input)?;
        self.store.set(&ctx.agent_id, items.clone());
        ctx.events.emit(AppEvent::PlanUpdated { agent_id: ctx.agent_id.clone(), items: items.clone() });
        Ok(ToolOutput::text(render(&items)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::context;
    use crusty_core::event::EventSink;
    use tempfile::tempdir;
    use tokio::sync::mpsc;

    #[tokio::test]
    async fn emits_plan_and_stores_it() {
        let dir = tempdir().unwrap();
        let mut ctx = context(&dir);
        let (tx, mut rx) = mpsc::unbounded_channel();
        ctx.events = EventSink::new(tx);
        let store = TodoStore::default();
        let tool = TodoWriteTool::new(store.clone());
        let input = json!({"todos": [
            {"content": "Read code", "status": "completed", "activeForm": "Reading code"},
            {"content": "Write tests", "status": "in_progress"},
            {"content": "Ship", "status": "pending"}
        ]});
        let out = tool.call(input, &ctx).await.unwrap();
        assert_eq!(out.text_content(), "☑ Read code\n◐ Write tests\n☐ Ship");
        assert!(matches!(
            rx.try_recv(),
            Ok(AppEvent::PlanUpdated { ref agent_id, ref items }) if agent_id == "test" && items.len() == 3
        ));
        assert_eq!(store.get("test").len(), 3);
        assert!(store.get("other").is_empty());
    }

    #[tokio::test]
    async fn rejects_two_items_in_progress() {
        let dir = tempdir().unwrap();
        let ctx = context(&dir);
        let tool = TodoWriteTool::new(TodoStore::default());
        let input = json!({"todos": [
            {"content": "a", "status": "in_progress"},
            {"content": "b", "status": "in_progress"}
        ]});
        assert!(matches!(tool.call(input, &ctx).await, Err(ToolError::InvalidInput(m)) if m.contains("only one")));
    }

    #[tokio::test]
    async fn rejects_unknown_status() {
        let dir = tempdir().unwrap();
        let ctx = context(&dir);
        let tool = TodoWriteTool::new(TodoStore::default());
        let input = json!({"todos": [{"content": "a", "status": "doing"}]});
        assert!(tool.call(input, &ctx).await.is_err());
    }
}
