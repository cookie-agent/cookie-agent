//! Delivering into threads, and thread prompts
//! (`docs/site/specs/webhooks.md` sections 7.4 and 7.5).
//!
//! **Delivery.** [`Engine::thread_target`] finds or makes the session a
//! key's delivery goes into: the session the owner's map names, while it
//! is a live root session, or a new one the caller creates and records. It
//! holds a per-key lock across that and the caller's send, so two
//! deliveries with one key never create two sessions.
//!
//! **Thread prompts.** An owner's `thread_prompt` reaches a session it
//! created as a plain user message (`user_input_submitted` and
//! `user_input_applied` under the owner's origin, `engine:cron-<id>`),
//! appended in the run that admits one of the owner's producer messages,
//! just before the admission, so the model reads it first and in the same
//! request. Nothing is stored about it: it is due when the session's model
//! context holds no such message from the owner after what the latest
//! checkpoint folded, which also covers a restart between the
//! checkpoint and the next delivery. An ACP agent's session gets it only
//! once: cookie does not see the agent's own compactions.

use std::{
    collections::HashMap,
    future::Future,
    sync::{Arc, Mutex, Weak},
};

use cookie_agent_protocol::{
    EventOrigin, EventPayload, ProducerOwner, RunId, SessionId, SessionOrigin, UserContent,
};

use crate::runtime::{Engine, EngineError, Event};
use crate::threads::ThreadOwner;

/// A thread: its owner and key.
type ThreadId = (ThreadOwner, String);

/// The per-key locks of deliveries in flight.
#[derive(Default)]
pub(crate) struct ThreadLocks(Mutex<HashMap<ThreadId, Weak<tokio::sync::Mutex<()>>>>);

/// Held while a delivery into a thread is made: its key takes no other.
pub(crate) struct ThreadGuard {
    _held: tokio::sync::OwnedMutexGuard<()>,
}

/// The session a delivery goes into.
pub(crate) struct ThreadTarget {
    pub(crate) session: SessionId,
    /// Whether the delivery created it.
    pub(crate) created: bool,
    pub(crate) guard: ThreadGuard,
}

impl Engine {
    /// Locks `owner`'s thread `key` until the guard drops.
    async fn lock_thread(&self, owner: &ThreadOwner, key: &str) -> ThreadGuard {
        let lock = {
            let mut locks = self
                .inner
                .thread_locks
                .0
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            locks.retain(|_, lock| lock.strong_count() > 0);
            let entry = locks.entry((owner.clone(), key.to_owned())).or_default();
            entry.upgrade().unwrap_or_else(|| {
                let lock = Arc::new(tokio::sync::Mutex::new(()));
                *entry = Arc::downgrade(&lock);
                lock
            })
        };
        ThreadGuard {
            _held: lock.lock_owned().await,
        }
    }

    /// Whether `session` can still be a thread: a root session this daemon
    /// holds.
    pub(crate) fn thread_session_live(&self, session: SessionId) -> bool {
        self.inner.store.is_owned(session)
            && self
                .inner
                .store
                .summary(session)
                .is_ok_and(|summary| matches!(summary.meta.origin, SessionOrigin::Root))
    }

    /// The session `owner`'s delivery with thread key `key` goes into:
    /// `lookup`'s session, read under the key's lock, while it is live, or
    /// else one `create` makes, which `record` then remembers. The guard
    /// keeps the key's other deliveries waiting until the caller has sent.
    pub(crate) async fn thread_target<Create, Record>(
        &self,
        owner: &ThreadOwner,
        key: &str,
        lookup: impl FnOnce(&Engine) -> Option<SessionId>,
        create: impl FnOnce() -> Create,
        record: impl FnOnce(SessionId) -> Record,
    ) -> Result<ThreadTarget, String>
    where
        Create: Future<Output = Result<SessionId, String>>,
        Record: Future<Output = Result<(), String>>,
    {
        let guard = self.lock_thread(owner, key).await;
        if let Some(session) = lookup(self).filter(|session| self.thread_session_live(*session)) {
            return Ok(ThreadTarget {
                session,
                created: false,
                guard,
            });
        }
        let session = create().await?;
        record(session).await?;
        Ok(ThreadTarget {
            session,
            created: true,
            guard,
        })
    }

    /// The thread prompt `owner` gives `session`, if the owner created the
    /// session and has one.
    fn thread_prompt_of(&self, owner: &ThreadOwner, session: SessionId) -> Option<String> {
        match owner {
            ThreadOwner::Cron(id) => {
                let summary = self.inner.store.summary(session).ok()?;
                if summary.meta.cron.as_ref()?.job_id != *id {
                    return None;
                }
                self.inner
                    .crons
                    .job(id)?
                    .target
                    .template()?
                    .thread_prompt
                    .clone()
            }
            ThreadOwner::Webhook(id) => {
                let summary = self.inner.store.summary(session).ok()?;
                if summary.meta.webhook.as_ref()?.webhook_id != *id {
                    return None;
                }
                self.inner
                    .webhooks
                    .webhook(id)?
                    .target
                    .new_target()?
                    .template
                    .thread_prompt
                    .clone()
            }
        }
    }

    /// Whether `session`'s model context holds a thread prompt under
    /// `origin` since its window's start: everything for an ACP agent's run,
    /// whose compactions cookie does not see.
    fn thread_prompt_in_context(
        &self,
        session: SessionId,
        origin: &EventOrigin,
        acp: bool,
    ) -> Result<bool, EngineError> {
        let log = self.inner.store.log(session)?;
        let sent = |event: &cookie_agent_protocol::StoredEvent| {
            matches!(event.payload, EventPayload::UserInputSubmitted { .. })
                && event.origin.as_ref() == Some(origin)
        };
        if acp {
            // The prompt goes with a session's first run, among its first
            // inputs.
            return Ok(log.state().opening_inputs().iter().any(|event| sent(event)));
        }
        let events = log.model_context_events()?;
        // What a checkpoint folded, into a summary or a provider's window,
        // the model no longer reads as it was sent.
        let start = crate::events::model_context::latest_checkpoint(&events)
            .map_or(0, |(_, commit)| {
                crate::events::model_context::commit_window_start(commit)
            });
        Ok(events
            .iter()
            .rev()
            .take_while(|event| event.seq >= start)
            .any(|event| sent(event)))
    }

    /// Before `owners`' producer messages are admitted into `run` of
    /// `session`: appends each owner's thread prompt that is due, once per
    /// owner, in the order of `owners`. Called on the session's actor.
    pub(in crate::runtime) fn submit_due_thread_prompts_direct<'a>(
        &self,
        session: SessionId,
        run: RunId,
        owners: impl IntoIterator<Item = &'a ProducerOwner>,
    ) -> Result<(), EngineError> {
        let mut seen = Vec::new();
        for owner in owners {
            let Some(owner) = ThreadOwner::of_producer(owner) else {
                continue;
            };
            if seen.contains(&owner) {
                continue;
            }
            seen.push(owner.clone());
            let Some(prompt) = self.thread_prompt_of(&owner, session) else {
                continue;
            };
            let acp = self
                .active_run_policy(run)
                .is_some_and(|policy| policy.acp_agent().is_some());
            let origin = owner.origin();
            if self.thread_prompt_in_context(session, &origin, acp)? {
                continue;
            }
            let submitted = self.append_direct_record(
                session,
                Some(run),
                origin.clone(),
                Event::UserInputSubmitted {
                    content: UserContent::text(prompt),
                },
            )?;
            self.append_direct(
                session,
                Some(run),
                origin,
                Event::UserInputApplied {
                    user_input_seq: submitted.seq,
                },
            )?;
        }
        Ok(())
    }
}
