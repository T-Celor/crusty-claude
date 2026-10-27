//! Non-interactive mode (`crusty -p`): one orchestrator turn, no UI.

use std::sync::Arc;
use std::time::Instant;

use crusty_core::{AppEvent, CancellationToken, Message, Usage};
use crusty_session::transcript::Entry;

use crate::controller::prepare_prompt;
use crate::engine::{ORCHESTRATOR_ID, StopKind, orchestrator_spec, run_agent};
use crate::runtime::Runtime;

#[derive(Debug, Clone)]
pub struct PrintOutcome {
    pub text: String,
    pub session_id: String,
    pub usage: Usage,
    pub cost_usd: f64,
    pub turns: u32,
    pub stop: StopKind,
    pub duration_ms: u64,
}

impl PrintOutcome {
    pub fn is_error(&self) -> bool {
        self.stop != StopKind::Done
    }
}

/// Runs `prompt` as one orchestrator turn on top of `messages`.
pub async fn run_print(rt: &Arc<Runtime>, messages: &mut Vec<Message>, prompt: &str, cancel: &CancellationToken) -> PrintOutcome {
    let started = Instant::now();
    let Some(message) = prepare_prompt(rt, prompt).await else {
        return PrintOutcome {
            text: "prompt blocked by a UserPromptSubmit hook".into(),
            session_id: rt.session_id(),
            usage: Usage::default(),
            cost_usd: 0.0,
            turns: 0,
            stop: StopKind::Error("blocked by hook".into()),
            duration_ms: started.elapsed().as_millis() as u64,
        };
    };
    rt.reset_turn_counters();
    rt.events.emit(AppEvent::UserMessage { text: prompt.to_owned() });
    rt.log(&Entry::Message { agent_id: ORCHESTRATOR_ID.into(), message: message.clone(), ts: chrono::Utc::now() });
    messages.push(message);
    let spec = orchestrator_spec(rt);
    let outcome = run_agent(rt, &spec, messages, cancel).await;
    rt.log(&Entry::Usage {
        input_tokens: outcome.usage.total_input(),
        output_tokens: outcome.usage.output_tokens,
        cost_usd: outcome.cost_usd,
        ts: chrono::Utc::now(),
    });
    rt.events.emit(AppEvent::TurnFinished { ok: outcome.stop == StopKind::Done });
    PrintOutcome {
        text: outcome.text,
        session_id: rt.session_id(),
        usage: outcome.usage,
        cost_usd: outcome.cost_usd,
        turns: outcome.turns,
        stop: outcome.stop,
        duration_ms: started.elapsed().as_millis() as u64,
    }
}
