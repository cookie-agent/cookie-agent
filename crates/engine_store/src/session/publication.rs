//! Publishing a session's records in append order, each commit point only
//! after its sync (`docs/site/specs/store-locks-and-durability.md`,
//! section 4.1).
//!
//! An append writes its record under the session's slot and queues its
//! publication there. A record that rides along publishes at once unless an
//! earlier commit point is still syncing; a commit point publishes when the
//! sync covering it ends, which its append waits for after releasing the
//! slot, and takes the records queued behind it along.

use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex},
};

use cookie_agent_protocol::{SessionId, SessionsChangedNotification, StoredEvent};

use super::{SessionMeta, SessionStore};
use crate::events::{Commit, EventLog};

/// Each session's publications not yet made, oldest first.
#[derive(Debug, Default)]
pub(super) struct Publications {
    queues: Mutex<HashMap<SessionId, Arc<Mutex<VecDeque<Pending>>>>>,
}

/// One publication waiting its turn.
#[derive(Debug)]
pub(super) enum Pending {
    Event {
        event: Arc<StoredEvent>,
        /// The sync a commit point waits for, on the log it was written to.
        commit: Option<(Arc<EventLog>, Commit)>,
        /// The session's listed metadata, when the record changed it.
        upserted: Option<SessionMeta>,
        /// The root whose pending approvals the record changed.
        approvals_of: Option<SessionId>,
    },
    /// A revert cut the log after `through_seq`.
    Rewound { through_seq: u64, meta: SessionMeta },
}

impl Pending {
    fn ready(&self) -> bool {
        match self {
            Self::Event {
                commit: Some((log, commit)),
                ..
            } => log.is_settled(*commit),
            Self::Event { commit: None, .. } | Self::Rewound { .. } => true,
        }
    }
}

impl Publications {
    fn queue(&self, id: SessionId) -> Arc<Mutex<VecDeque<Pending>>> {
        Arc::clone(
            self.queues
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .entry(id)
                .or_default(),
        )
    }

    /// Drops `id`'s queue once it is empty and nobody else holds it, so the
    /// map keeps only sessions with something pending. The caller holds
    /// `queue`'s lock; anyone else about to use the queue holds a clone, and
    /// clones are made only under the map's lock, taken here.
    fn forget_if_idle(&self, id: SessionId, queue: &Arc<Mutex<VecDeque<Pending>>>) {
        let mut queues = self
            .queues
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if queues
            .get(&id)
            .is_some_and(|held| Arc::ptr_eq(held, queue) && Arc::strong_count(queue) == 2)
        {
            queues.remove(&id);
        }
    }
}

impl SessionStore {
    /// Queues `pending` behind `id`'s earlier publications and publishes
    /// whatever is ready. Called under the session's slot, so a session's
    /// publications queue in append order.
    pub(super) fn queue_publication(&self, id: SessionId, pending: Pending) {
        let queue = self.publications.queue(id);
        let mut pending_queue = queue
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        pending_queue.push_back(pending);
        self.publish_ready(id, &queue, &mut pending_queue);
    }

    /// Publishes `id`'s queued records whose syncs ended, in order, up to the
    /// first commit point still waiting.
    pub(super) fn publish_settled(&self, id: SessionId) {
        let queue = self.publications.queue(id);
        let mut pending_queue = queue
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.publish_ready(id, &queue, &mut pending_queue);
    }

    /// Holding the queue's lock for the whole pass keeps two passes from
    /// interleaving a session's publications.
    fn publish_ready(
        &self,
        id: SessionId,
        queue: &Arc<Mutex<VecDeque<Pending>>>,
        pending_queue: &mut VecDeque<Pending>,
    ) {
        while pending_queue.front().is_some_and(Pending::ready) {
            match pending_queue.pop_front().expect("a ready publication") {
                Pending::Event {
                    event,
                    upserted,
                    approvals_of,
                    ..
                } => {
                    self.publish_stored_event(&event);
                    if let Some(meta) = upserted {
                        self.notify_upserted(&meta);
                    }
                    if let Some(root_session_id) = approvals_of {
                        self.notify_index_change(SessionsChangedNotification::ApprovalsChanged {
                            root_session_id,
                            session_id: id,
                        });
                    }
                }
                Pending::Rewound { through_seq, meta } => {
                    self.publish_rewound(id, through_seq);
                    self.notify_upserted(&meta);
                }
            }
        }
        if pending_queue.is_empty() {
            self.publications.forget_if_idle(id, queue);
        }
    }
}
