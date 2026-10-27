//! Approvals answered by the UI: a request is published as an event and the agent waits
//! for the matching `UiCommand::Approval`.

use std::collections::HashMap;
use std::sync::Mutex;

use crusty_core::{AppEvent, ApprovalDecision, ApprovalRequest, Approver, EventSink, async_trait};
use tokio::sync::oneshot;

pub struct ChannelApprover {
    events: EventSink,
    pending: Mutex<HashMap<String, oneshot::Sender<ApprovalDecision>>>,
}

impl ChannelApprover {
    pub fn new(events: EventSink) -> Self {
        ChannelApprover { events, pending: Mutex::new(HashMap::new()) }
    }

    /// Delivers the user's answer. Returns false for an unknown or already answered id.
    pub fn resolve(&self, id: &str, decision: ApprovalDecision) -> bool {
        let tx = self.pending.lock().unwrap_or_else(|e| e.into_inner()).remove(id);
        match tx {
            Some(tx) => {
                self.events.emit(AppEvent::ApprovalResolved { id: id.to_owned(), allowed: decision.is_allowed() });
                tx.send(decision).is_ok()
            }
            None => false,
        }
    }

    /// Denies everything still waiting (interrupt, quit).
    pub fn deny_all(&self) {
        let pending: Vec<_> = self.pending.lock().unwrap_or_else(|e| e.into_inner()).drain().collect();
        for (id, tx) in pending {
            self.events.emit(AppEvent::ApprovalResolved { id, allowed: false });
            let _ = tx.send(ApprovalDecision::Deny);
        }
    }
}

#[async_trait]
impl Approver for ChannelApprover {
    async fn approve(&self, req: ApprovalRequest) -> ApprovalDecision {
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap_or_else(|e| e.into_inner()).insert(req.id.clone(), tx);
        self.events.emit(AppEvent::ApprovalRequested(req));
        // A dropped sender (session ending) counts as a refusal.
        rx.await.unwrap_or(ApprovalDecision::Deny)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crusty_core::RiskLevel;
    use std::sync::Arc;

    fn req(id: &str) -> ApprovalRequest {
        ApprovalRequest {
            id: id.into(),
            agent_id: "a".into(),
            tool: "Bash".into(),
            summary: "Bash(rm x)".into(),
            risk: RiskLevel::High,
            reason: "deletes".into(),
            suggested_rule: None,
            preview: String::new(),
        }
    }

    #[tokio::test]
    async fn resolve_answers_the_waiting_request() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let a = Arc::new(ChannelApprover::new(EventSink::new(tx)));
        let a2 = a.clone();
        let wait = tokio::spawn(async move { a2.approve(req("r1")).await });
        assert!(matches!(rx.recv().await, Some(AppEvent::ApprovalRequested(_))));
        assert!(a.resolve("r1", ApprovalDecision::AllowOnce));
        assert_eq!(wait.await.unwrap(), ApprovalDecision::AllowOnce);
        assert!(!a.resolve("r1", ApprovalDecision::AllowOnce));
    }

    #[tokio::test]
    async fn deny_all_refuses_pending() {
        let a = Arc::new(ChannelApprover::new(EventSink::noop()));
        let a2 = a.clone();
        let wait = tokio::spawn(async move { a2.approve(req("r2")).await });
        tokio::task::yield_now().await;
        while a.pending.lock().unwrap().is_empty() {
            tokio::task::yield_now().await;
        }
        a.deny_all();
        assert_eq!(wait.await.unwrap(), ApprovalDecision::Deny);
    }
}
