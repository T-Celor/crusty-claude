//! JSON-RPC envelopes and the mapping from engine events to ACP `session/update` payloads.
//! Payloads follow the names of `agent-client-protocol-schema` v1.

use crusty_core::AppEvent;
use crusty_core::event::{TodoItem, TodoStatus};
use serde_json::{Value, json};

/// JSON-RPC error code: the line was not valid JSON.
pub const PARSE_ERROR: i64 = -32700;
/// JSON-RPC error code: the message is not a valid request.
pub const INVALID_REQUEST: i64 = -32600;
/// JSON-RPC error code: no such method.
pub const METHOD_NOT_FOUND: i64 = -32601;
/// JSON-RPC error code: the params do not fit the method.
pub const INVALID_PARAMS: i64 = -32602;
/// JSON-RPC error code: the backend failed.
pub const INTERNAL_ERROR: i64 = -32603;

/// A successful JSON-RPC response.
pub fn response(id: &Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

/// A JSON-RPC error response.
pub fn error(id: &Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

/// A request from the agent to the client.
pub fn request(id: u64, method: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
}

/// A JSON-RPC notification (no id, no answer expected).
pub fn notification(method: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "method": method, "params": params})
}

/// A `session/update` notification carrying one update.
pub fn session_update(session: &str, update: Value) -> Value {
    notification("session/update", json!({"sessionId": session, "update": update}))
}

/// The ACP tool kind for an engine tool name.
pub fn tool_kind(tool: &str) -> &'static str {
    match tool {
        "Read" | "LS" => "read",
        "Glob" | "Grep" => "search",
        "Edit" | "Write" | "MultiEdit" => "edit",
        "Bash" => "execute",
        "WebFetch" | "WebSearch" => "fetch",
        "Task" => "think",
        _ => "other",
    }
}

fn text_block(text: &str) -> Value {
    json!({"type": "text", "text": text})
}

/// A `tool_call` update that opens a tool call as in progress.
pub fn tool_started(call_id: &str, tool: &str, title: &str) -> Value {
    json!({
        "sessionUpdate": "tool_call",
        "toolCallId": call_id,
        "title": title,
        "kind": tool_kind(tool),
        "status": "in_progress",
    })
}

fn plan_entry(item: &TodoItem) -> Value {
    let status = match item.status {
        TodoStatus::Pending => "pending",
        TodoStatus::InProgress => "in_progress",
        TodoStatus::Completed => "completed",
    };
    json!({"content": item.content, "priority": "medium", "status": status})
}

/// The `update` payload for an engine event, or `None` when ACP has no equivalent.
pub fn update_for(event: &AppEvent) -> Option<Value> {
    let update = match event {
        AppEvent::TextDelta { text, .. } => {
            json!({"sessionUpdate": "agent_message_chunk", "content": text_block(text)})
        }
        AppEvent::ThinkingDelta { text, .. } => {
            json!({"sessionUpdate": "agent_thought_chunk", "content": text_block(text)})
        }
        AppEvent::ToolStarted { call_id, tool, summary, .. } => tool_started(call_id, tool, summary),
        AppEvent::ToolFinished { call_id, ok, preview, .. } => {
            let status = if *ok { "completed" } else { "failed" };
            json!({
                "sessionUpdate": "tool_call_update",
                "toolCallId": call_id,
                "status": status,
                "content": [{"type": "content", "content": text_block(preview)}],
            })
        }
        AppEvent::PlanUpdated { items, .. } => {
            json!({"sessionUpdate": "plan", "entries": items.iter().map(plan_entry).collect::<Vec<_>>()})
        }
        _ => return None,
    };
    Some(update)
}

/// Params of `session/request_permission`. The tool call must already exist in the client,
/// so the server sends a `tool_started` update with the same `call_id` first.
pub fn permission_params(session: &str, call_id: &str, title: &str, tool: &str) -> Value {
    json!({
        "sessionId": session,
        "toolCall": {
            "toolCallId": call_id,
            "title": title,
            "kind": tool_kind(tool),
            "status": "pending",
        },
        "options": [
            {"optionId": "allow_once", "name": "Allow once", "kind": "allow_once"},
            {"optionId": "allow_always", "name": "Always allow", "kind": "allow_always"},
            {"optionId": "reject_once", "name": "Reject", "kind": "reject_once"},
        ],
    })
}
