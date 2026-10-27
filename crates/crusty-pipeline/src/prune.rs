//! Prune stage: superseded and irrelevant tool results become retrievable stubs.
//!
//! Decisions are taken at a per-session frontier and advance only after `batch_messages`
//! new messages. Between advances the same blocks are replaced again, and each stub carries
//! the content-addressed handle, so the prompt prefix stays byte-identical for the cache.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::time::Instant;

use crusty_advisor::prune::stale_tool_results;
use crusty_core::config::OptimizerMode;
use crusty_core::event::{Stage, StageStatus};
use crusty_core::message::blocks_text;
use crusty_core::tokens::{estimate_request, estimate_text};
use crusty_core::{ChatRequest, ContentBlock, Message, OptimizeContext, OptimizeOutcome, OriginalStore, Role};

use crate::Pipeline;

/// Sessions whose prune frontier is remembered; the oldest is forgotten first, as in Headroom.
const MAX_SESSIONS: usize = 2000;
/// Results the advisor scores per frontier advance.
const MAX_RELEVANCE_ITEMS: usize = 64;
/// Characters of each result the advisor reads.
const RELEVANCE_CHARS: usize = 2000;
/// Start of every stub written by this stage; stubs are never pruned twice.
const STUB_PREFIX: &str = "[stale: superseded by a later read";

/// Pruning decisions of one session: the frontier they were made at and the blocks pruned.
#[derive(Debug, Clone, Default)]
pub(crate) struct PruneState {
    frontier: usize,
    pruned: BTreeSet<(usize, usize)>,
}

/// Prune state of every session seen, bounded.
#[derive(Debug, Default)]
pub(crate) struct PruneSessions {
    states: BTreeMap<String, PruneState>,
    order: VecDeque<String>,
}

impl PruneSessions {
    fn get(&self, session: &str) -> PruneState {
        self.states.get(session).cloned().unwrap_or_default()
    }

    /// Drops the state of `session`: its frontier was keyed by message positions of a
    /// conversation that no longer exists.
    pub(crate) fn forget(&mut self, session: &str) {
        if self.states.remove(session).is_some() {
            self.order.retain(|s| s != session);
        }
    }

    #[cfg(test)]
    pub(crate) fn frontier_of(&self, session: &str) -> usize {
        self.get(session).frontier
    }

    fn remember(&mut self, session: &str, state: PruneState) {
        if self.states.insert(session.to_owned(), state).is_none() {
            self.order.push_back(session.to_owned());
            while self.order.len() > MAX_SESSIONS {
                if let Some(old) = self.order.pop_front() {
                    self.states.remove(&old);
                }
            }
        }
    }
}

/// Text of a tool result that may be replaced: text only, not empty, not a stub already.
fn prunable_text(block: &ContentBlock) -> Option<String> {
    let ContentBlock::ToolResult { content, .. } = block else {
        return None;
    };
    if content.iter().any(|b| !matches!(b, ContentBlock::Text { .. })) {
        return None;
    }
    let text = blocks_text(content);
    (!text.is_empty() && !text.starts_with(STUB_PREFIX)).then_some(text)
}

/// Replaces one tool result by its stub when that saves tokens; returns whether it did.
fn replace(block: &mut ContentBlock, store: &OriginalStore) -> bool {
    let Some(text) = prunable_text(block) else {
        return false;
    };
    let handle = store.put(text.clone());
    let stub = format!("{STUB_PREFIX} — Retrieve(\"{handle}\") restores it]");
    if estimate_text(&text) <= estimate_text(&stub) {
        return false;
    }
    if let ContentBlock::ToolResult { content, .. } = block {
        *content = vec![ContentBlock::text(stub)];
    }
    true
}

/// The text of the latest user message that is not only tool results: what the agent works on.
fn goal_of(messages: &[Message]) -> String {
    messages
        .iter()
        .rev()
        .filter(|m| m.role == Role::User)
        .find_map(|m| {
            let text = m
                .content
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::Text { text, .. } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            (!text.trim().is_empty()).then_some(text)
        })
        .unwrap_or_default()
}

impl Pipeline {
    /// Decides which tool results to prune at the frontier. The frontier moves only once
    /// `batch_messages` messages were added since the last advance; the blocks pruned earlier
    /// stay pruned until then.
    async fn advance_prune(&self, req: &ChatRequest, state: &mut PruneState) {
        let cfg = &self.laya.pruner;
        let target = req.messages.len().saturating_sub(cfg.keep_recent_messages);
        if target < state.frontier {
            // The history shrank: another conversation under the same session id.
            *state = PruneState::default();
        }
        if target < state.frontier + cfg.batch_messages.max(1) {
            return;
        }
        let prev = state.frontier;
        let mut next: BTreeSet<(usize, usize)> =
            stale_tool_results(&req.messages, cfg.keep_recent_messages).into_iter().filter(|(mi, _)| *mi < target).collect();

        // Laya relevance for the results that just left the protected tail.
        let mut slots = Vec::new();
        let mut items = Vec::new();
        for (mi, m) in req.messages.iter().enumerate().take(target).skip(prev) {
            for (bi, block) in m.content.iter().enumerate() {
                if items.len() >= MAX_RELEVANCE_ITEMS || next.contains(&(mi, bi)) {
                    continue;
                }
                if let Some(text) = prunable_text(block) {
                    slots.push((mi, bi));
                    items.push(text.chars().take(RELEVANCE_CHARS).collect::<String>());
                }
            }
        }
        let goal = goal_of(&req.messages);
        if !items.is_empty() && !goal.is_empty() {
            let scores = self.advisor.relevance(&goal, &items).await;
            if let Some(values) = scores.for_items(items.len()) {
                for (&slot, &v) in slots.iter().zip(values) {
                    if v < cfg.min_relevance {
                        next.insert(slot);
                    }
                }
            }
        }
        state.frontier = target;
        state.pruned = next;
    }

    /// Prune stage: active mode supersedes the results decided at the frontier; observe mode
    /// measures the same replacements on a copy and leaves the request alone. Runs before
    /// Headroom.
    pub(crate) async fn prune_stage(&self, req: &mut ChatRequest, ctx: &OptimizeContext, mode: OptimizerMode, out: &mut OptimizeOutcome) {
        let started = Instant::now();
        let observe = mode == OptimizerMode::Observe;
        let mut state = self.prune.lock().unwrap_or_else(|e| e.into_inner()).get(&ctx.session_id);
        self.advance_prune(req, &mut state).await;

        // Observe mode writes into a throwaway store, so no original is kept for nothing.
        let scratch = OriginalStore::new(0);
        let store: &OriginalStore = if observe { &scratch } else { self.store.as_ref() };
        let mut candidate = req.clone();
        for &(mi, bi) in &state.pruned {
            if let Some(block) = candidate.messages.get_mut(mi).and_then(|m| m.content.get_mut(bi)) {
                replace(block, store);
            }
        }

        let before = estimate_request(req, true);
        let after = estimate_request(&candidate, true);
        let mut m = self.metric(Stage::Prune, "request", "prune:stale-results", ctx);
        m.tokens_before_est = before;
        m.tokens_after_est = after;
        m.status = match (state.pruned.is_empty(), observe) {
            (true, _) => StageStatus::Skipped,
            (false, true) => StageStatus::Observed,
            (false, false) => StageStatus::Applied,
        };
        m.detail = if state.pruned.is_empty() {
            format!("nothing to prune · frontier {}", state.frontier)
        } else {
            let verb = if observe { "observe: would supersede" } else { "superseded" };
            format!("{verb} {} tool results · frontier {}", state.pruned.len(), state.frontier)
        };
        if !observe {
            if candidate != *req {
                *req = candidate;
                req.tolerate_history_edits();
            }
            self.prune.lock().unwrap_or_else(|e| e.into_inner()).remember(&ctx.session_id, state);
        }
        m.duration_us = started.elapsed().as_micros() as u64;
        self.emit(out, m);
    }
}
