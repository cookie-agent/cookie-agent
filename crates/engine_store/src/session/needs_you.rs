//! What waits for the user's answer in each session: the approvals
//! escalated to them and the agents' questions, which `SessionMeta::needs_you`
//! reports for the session and every session above it.
//!
//! Folded from the records as they are appended and never persisted: a
//! daemon restart ends every run, and with them the approvals and questions
//! they waited on.

use std::{
    collections::{BTreeSet, HashMap, HashSet},
    sync::Mutex,
};

use cookie_agent_protocol::{
    ApprovalId, ApprovalStep, ElicitationId, EventPayload, NeedsYou, Open, SessionId,
};

/// The approvals and questions waiting in one session.
#[derive(Debug, Default)]
struct Waiting {
    approvals: HashSet<ApprovalId>,
    questions: HashSet<ElicitationId>,
}

impl Waiting {
    fn kinds(&self) -> BTreeSet<NeedsYou> {
        let mut kinds = BTreeSet::new();
        if !self.approvals.is_empty() {
            kinds.insert(NeedsYou::Approval);
        }
        if !self.questions.is_empty() {
            kinds.insert(NeedsYou::Question);
        }
        kinds
    }

    fn is_empty(&self) -> bool {
        self.approvals.is_empty() && self.questions.is_empty()
    }
}

/// Each session's waiting approvals and questions, by session.
#[derive(Debug, Default)]
pub(super) struct NeedsYouSessions {
    sessions: Mutex<HashMap<SessionId, Waiting>>,
}

impl NeedsYouSessions {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<SessionId, Waiting>> {
        self.sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Folds `payload`, appended to `id`, and returns whether what waits
    /// on the user in `id` changed kind. An approval waits from its
    /// escalation until the user decides or it ends; a question from its
    /// asking until it is resolved.
    pub(super) fn observe(&self, id: SessionId, payload: &EventPayload) -> bool {
        let mut sessions = self.lock();
        let before = sessions.get(&id).map(Waiting::kinds).unwrap_or_default();
        match payload {
            EventPayload::ApprovalStep {
                approval_id,
                step: Open::Known(ApprovalStep::Escalated { .. }),
            } => {
                sessions
                    .entry(id)
                    .or_default()
                    .approvals
                    .insert(*approval_id);
            }
            EventPayload::ApprovalStep {
                approval_id,
                step: Open::Known(ApprovalStep::UserDecision { .. }),
            }
            | EventPayload::ApprovalFinalized { approval_id, .. } => {
                if let Some(waiting) = sessions.get_mut(&id) {
                    waiting.approvals.remove(approval_id);
                }
            }
            EventPayload::ElicitationRequested { request } => {
                sessions
                    .entry(id)
                    .or_default()
                    .questions
                    .insert(request.elicitation_id);
            }
            EventPayload::ElicitationResolved { resolution } => {
                if let Some(waiting) = sessions.get_mut(&id) {
                    waiting.questions.remove(&resolution.elicitation_id);
                }
            }
            _ => return false,
        }
        let after = sessions.get(&id).map(Waiting::kinds).unwrap_or_default();
        if sessions.get(&id).is_some_and(Waiting::is_empty) {
            sessions.remove(&id);
        }
        before != after
    }

    /// Forgets what waited in `id`, returning whether anything did: a
    /// revert closed what was open.
    pub(super) fn clear(&self, id: SessionId) -> bool {
        self.lock().remove(&id).is_some()
    }

    /// Forgets what waited in deleted sessions.
    pub(super) fn forget(&self, deleted: &HashSet<SessionId>) {
        self.lock().retain(|id, _| !deleted.contains(id));
    }

    /// The sessions something waits in, with what.
    pub(super) fn waiting(&self) -> Vec<(SessionId, BTreeSet<NeedsYou>)> {
        self.lock()
            .iter()
            .map(|(id, waiting)| (*id, waiting.kinds()))
            .collect()
    }
}
