//! Named collection of tools, and the built-in file tool set.

use std::collections::BTreeMap;
use std::sync::Arc;

use crusty_core::message::ToolSpec;
use crusty_core::tool::Tool;

use crate::edit::{EditTool, MultiEditTool};
use crate::glob::GlobTool;
use crate::grep::GrepTool;
use crate::ls::LsTool;
use crate::notebook::NotebookEditTool;
use crate::read::ReadTool;
use crate::retrieve::RetrieveTool;
use crate::todo::{TodoStore, TodoWriteTool};
use crate::tracker::FileTracker;
use crate::write::WriteTool;

/// Tools by name. Iteration and `specs` are sorted by name, which keeps the provider's
/// prompt stable across requests.
#[derive(Default, Clone)]
pub struct ToolRegistry {
    tools: BTreeMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a tool, replacing any tool with the same name.
    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        self.tools.insert(tool.name().to_owned(), tool);
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.get(name).cloned()
    }

    pub fn names(&self) -> Vec<String> {
        self.tools.keys().cloned().collect()
    }

    pub fn contains(&self, name: &str) -> bool {
        self.tools.contains_key(name)
    }

    /// Specs of the tools in `allowed` (all tools when `None`), sorted by name.
    pub fn specs(&self, allowed: Option<&[String]>) -> Vec<ToolSpec> {
        self.tools
            .iter()
            .filter(|(name, _)| allowed.is_none_or(|list| list.iter().any(|allowed| allowed == *name)))
            .map(|(_, tool)| tool.spec())
            .collect()
    }

    /// A registry holding only the tools named in `names` that exist here.
    pub fn subset(&self, names: &[String]) -> ToolRegistry {
        let tools =
            self.tools.iter().filter(|(name, _)| names.contains(*name)).map(|(name, tool)| (name.clone(), Arc::clone(tool))).collect();
        ToolRegistry { tools }
    }
}

/// Session state shared by the built-in file tools.
#[derive(Debug, Default)]
pub struct FileToolsState {
    pub tracker: Arc<FileTracker>,
    pub todos: TodoStore,
}

/// The built-in file, search, notebook, plan and retrieval tools, sharing `state`.
pub fn file_tools(state: &FileToolsState) -> Vec<Arc<dyn Tool>> {
    let tracker = &state.tracker;
    let tools: Vec<Arc<dyn Tool>> = vec![
        Arc::new(ReadTool::new(Arc::clone(tracker))),
        Arc::new(WriteTool::new(Arc::clone(tracker))),
        Arc::new(EditTool::new(Arc::clone(tracker))),
        Arc::new(MultiEditTool::new(Arc::clone(tracker))),
        Arc::new(GlobTool),
        Arc::new(GrepTool),
        Arc::new(LsTool),
        Arc::new(NotebookEditTool::new(Arc::clone(tracker))),
        Arc::new(TodoWriteTool::new(state.todos.clone())),
        Arc::new(RetrieveTool),
    ];
    tools
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_tools_have_expected_names_and_sorted_specs() {
        let state = FileToolsState::default();
        let mut registry = ToolRegistry::new();
        for tool in file_tools(&state) {
            registry.register(tool);
        }
        assert_eq!(registry.names(), ["Edit", "Glob", "Grep", "LS", "MultiEdit", "NotebookEdit", "Read", "Retrieve", "TodoWrite", "Write"]);
        let specs: Vec<String> = registry.specs(None).into_iter().map(|s| s.name).collect();
        assert_eq!(specs, registry.names());
        let allowed = vec!["Read".to_owned(), "Nope".to_owned()];
        assert_eq!(registry.specs(Some(&allowed)).len(), 1);
        assert_eq!(registry.subset(&allowed).names(), ["Read"]);
        assert!(registry.contains("Glob") && registry.get("Missing").is_none());
    }
}
