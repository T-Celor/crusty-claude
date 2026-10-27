//! The `Task` tool: the orchestrator hands a self-contained task to an executor agent of a
//! domain, which runs with its own model, tools and budget and returns a report.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use crusty_core::tool::{input_opt_str, input_str, preview_of};
use crusty_core::{AppEvent, CancellationToken, Message, ModelRef, ToolError, ToolOutput, ToolSpec};
use serde_json::{Value, json};

use crate::engine::{AgentSpec, StopKind, TASK_TOOL, TOOL_SEARCH, run_agent};
use crate::prompts;
use crate::runtime::Runtime;

pub fn task_spec(rt: &Runtime) -> ToolSpec {
    let domains = rt.domain_names();
    let description = format!(
        "Delegate one self-contained task to an executor agent. The executor has its own model and tools, \
         sees nothing of this conversation except your prompt, and returns a report. Call Task several times \
         in one message to run independent tasks in parallel.\n\nDomains:\n{}",
        prompts::domains_table(rt)
    );
    ToolSpec::new(
        TASK_TOOL,
        description,
        json!({
            "type": "object",
            "properties": {
                "domain": {"type": "string", "enum": domains, "description": "Executor domain that fits the task"},
                "description": {"type": "string", "description": "3-5 word label shown to the user"},
                "prompt": {"type": "string", "description": "Complete instructions: goal, files, constraints, what done looks like, what to report"},
                "model": {"type": "string", "description": "Optional provider:model for this task only"}
            },
            "required": ["domain", "description", "prompt"],
            "additionalProperties": false
        }),
    )
}

pub fn summarize(input: &Value) -> String {
    let domain = input_opt_str(input, "domain").unwrap_or("?");
    let label = input_opt_str(input, "description").or_else(|| input_opt_str(input, "prompt")).unwrap_or("");
    format!("Task({domain}: {})", preview_of(label))
}

/// Tool names an executor of this domain gets: the domain's list (an `mcp__server` entry
/// covers that server's tools) or every registered tool, never `Task` unless depth allows.
fn executor_tools(rt: &Runtime, listed: Option<&[String]>, depth: u32) -> Vec<String> {
    let registered: Vec<&str> = rt.tools.keys().map(String::as_str).collect();
    tool_names(&registered, listed, depth < rt.config.orchestrator.max_depth)
}

/// The name rule of `executor_tools`: `delegable` says whether `Task` is offered.
fn tool_names(registered: &[&str], listed: Option<&[String]>, delegable: bool) -> Vec<String> {
    let mut tools: Vec<String> = match listed {
        Some(names) => registered
            .iter()
            .copied()
            .filter(|k| names.iter().any(|n| n.as_str() == *k || (n.starts_with("mcp__") && k.starts_with(&format!("{n}__")))))
            .map(str::to_owned)
            .collect(),
        None => registered.iter().copied().map(str::to_owned).collect(),
    };
    // `ToolSearch` is handled by the engine, so it is not a registered tool. An MCP entry
    // needs it to load the server's tools.
    if listed.is_none_or(|names| names.iter().any(|n| n == TOOL_SEARCH || n.starts_with("mcp__"))) {
        tools.push(TOOL_SEARCH.into());
    }
    if delegable {
        tools.push(TASK_TOOL.into());
    }
    tools
}

pub async fn run_task(rt: &Arc<Runtime>, parent: &AgentSpec, input: &Value, cancel: &CancellationToken) -> Result<ToolOutput, ToolError> {
    let domain_name = input_str(input, "domain")?;
    let prompt = input_str(input, "prompt")?;
    let label = input_opt_str(input, "description").map(str::to_owned).unwrap_or_else(|| preview_of(prompt));
    let depth = parent.depth + 1;
    if depth > rt.config.orchestrator.max_depth {
        return Err(ToolError::Denied(format!("delegation depth limit ({}) reached", rt.config.orchestrator.max_depth)));
    }
    let Some(domain) = rt.domain(domain_name) else {
        return Err(ToolError::InvalidInput(format!("unknown domain `{domain_name}`; available: {}", rt.domain_names().join(", "))));
    };
    let mut profile = domain.model.clone();
    if let Some(m) = input_opt_str(input, "model") {
        let mref = ModelRef::parse(m, &profile.provider).ok_or_else(|| ToolError::InvalidInput(format!("bad model `{m}`")))?;
        if rt.provider(&mref.provider).is_none() {
            return Err(ToolError::InvalidInput(format!("unknown provider `{}`", mref.provider)));
        }
        profile.provider = mref.provider;
        profile.model = mref.model;
    }
    if domain_name == "reviewer" {
        let n = rt.review_count.fetch_add(1, Ordering::Relaxed) + 1;
        let max = rt.config.orchestrator.max_review_loops + 1;
        if n > max {
            return Err(ToolError::Denied(format!(
                "review loop limit reached ({max} reviews this turn); report the remaining findings to the user"
            )));
        }
    }
    let spec = AgentSpec {
        id: rt.next_agent_id(domain_name),
        parent: Some(parent.id.clone()),
        domain: domain_name.to_owned(),
        system: prompts::executor_prompt(rt, &domain),
        tools: executor_tools(rt, domain.tools.as_deref(), depth),
        profile,
        depth,
        max_steps: rt.config.orchestrator.max_steps,
    };
    let _permit = tokio::select! {
        p = rt.agent_slots(depth).acquire_owned() => p.map_err(|e| ToolError::Failed(e.to_string()))?,
        _ = cancel.cancelled() => return Err(ToolError::Cancelled),
    };
    rt.events.emit(AppEvent::AgentSpawned {
        agent_id: spec.id.clone(),
        parent: spec.parent.clone(),
        domain: spec.domain.clone(),
        provider: spec.profile.provider.clone(),
        model: spec.profile.model.clone(),
        task: label.clone(),
    });
    if let Some((_, level)) = rt.policy_text() {
        rt.events.emit(AppEvent::PolicyApplied {
            agent_id: spec.id.clone(),
            policy: rt.policy.name.clone(),
            version: rt.policy.version.clone(),
            level,
        });
    }
    let mut messages = vec![Message::user_text(prompt)];
    let child = cancel.child_token();
    let outcome = run_agent(rt, &spec, &mut messages, &child).await;
    let ok = outcome.stop == StopKind::Done;
    rt.events.emit(AppEvent::AgentFinished {
        agent_id: spec.id.clone(),
        ok,
        summary: preview_of(&outcome.text),
        usage: outcome.usage,
        cost_usd: outcome.cost_usd,
        turns: outcome.turns,
    });
    if outcome.stop == StopKind::Cancelled {
        return Err(ToolError::Cancelled);
    }
    let footer = format!(
        "[executor {} · {} · {} turns · {} tokens · ${:.4} · {}]",
        spec.id,
        outcome.model,
        outcome.turns,
        outcome.usage.total(),
        outcome.cost_usd,
        outcome.stop.label()
    );
    let body = if outcome.text.trim().is_empty() { "(the executor returned no text)".to_owned() } else { outcome.text };
    let text = format!("{body}\n\n{footer}");
    Ok(if matches!(outcome.stop, StopKind::Error(_)) { ToolOutput::error(text) } else { ToolOutput::text(text) })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_entry_brings_tool_search() {
        let listed = vec!["mcp__docs".to_owned()];
        let tools = tool_names(&["mcp__docs__search", "Read"], Some(&listed), false);
        assert!(tools.contains(&"mcp__docs__search".to_owned()));
        assert!(tools.contains(&TOOL_SEARCH.to_owned()));
        assert!(!tools.contains(&"Read".to_owned()));
        assert!(!tools.contains(&TASK_TOOL.to_owned()));
    }

    #[test]
    fn plain_domain_has_no_tool_search() {
        let listed = vec!["Read".to_owned()];
        assert!(!tool_names(&["mcp__docs__search", "Read"], Some(&listed), true).contains(&TOOL_SEARCH.to_owned()));
    }
}
