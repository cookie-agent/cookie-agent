//! When the daemon pushes a notification to the user's browsers
//! (`docs/site/specs/push-notifications.md` sections 3 and 4): a root
//! session that settled (its run ended and nothing but recurring cron jobs
//! can start another), and an approval or an ACP agent's question that
//! waits for the user. The engine decides when; the server sends. Whether a
//! run's end settles is decided once, as it ends: a turn that ended while
//! something else could still wake the session is never pushed, however
//! that ends.

use std::{
    collections::{HashMap, HashSet},
    sync::{Mutex, MutexGuard},
};

use cookie_agent_protocol::{
    ApprovalRequest, GoalStatus, ProducerOwner, RunId, RunOutcome, SessionId, SessionStatus,
};
use jiff::Timestamp;
use tokio::sync::broadcast;

use super::{Engine, Event};

/// What the engine asks the server to push.
#[derive(Clone, Debug)]
pub enum PushSignal {
    /// A root session settled.
    Settled(SettledSignal),
    /// An approval or a question waits for the user in a tree.
    NeedsYou(NeedsYouSignal),
}

/// A root session whose run ended and that nothing will wake but the user
/// or a recurring cron job.
#[derive(Clone, Debug)]
pub struct SettledSignal {
    pub root: SessionId,
    pub outcome: SettledOutcome,
    pub title: Option<String>,
    pub short_id: Option<String>,
    /// A new-session cron job created the session.
    pub cron: bool,
    pub at: Timestamp,
}

impl SettledSignal {
    #[must_use]
    pub const fn failed(&self) -> bool {
        matches!(self.outcome, SettledOutcome::Failed { .. })
    }
}

/// How the run that settled ended.
#[derive(Clone, Debug)]
pub enum SettledOutcome {
    Completed { final_text: Option<String> },
    Failed { error: String },
}

/// An approval or a question that waits for the user.
#[derive(Clone, Debug)]
pub struct NeedsYouSignal {
    pub root: SessionId,
    /// Where it waits: the root or a subagent below it.
    pub session: SessionId,
    /// The subagent's short id, when it is not the root.
    pub handle: Option<String>,
    pub root_title: Option<String>,
    pub root_short_id: Option<String>,
    pub item: NeedsYouItem,
    pub at: Timestamp,
}

impl NeedsYouSignal {
    /// When the approval expires, if it does.
    #[must_use]
    pub const fn expires_at(&self) -> Option<Timestamp> {
        match &self.item {
            NeedsYouItem::Approval { expires_at, .. } => *expires_at,
            NeedsYouItem::Question { .. } => None,
        }
    }
}

#[derive(Clone, Debug)]
pub enum NeedsYouItem {
    Approval {
        /// The tool's display title.
        tool: String,
        /// The call's primary argument, such as a command line.
        summary: Option<String>,
        expires_at: Option<Timestamp>,
    },
    Question {
        /// The agent that asks.
        agent: String,
        message: String,
        /// The question asks to open a page, such as a sign-in.
        url: bool,
    },
}

/// The engine's push bookkeeping, in memory only: a restart forgets it.
pub(crate) struct PushSignalState {
    sender: broadcast::Sender<PushSignal>,
    /// Root sessions whose last run ended completed or failed, until the
    /// check after that end, another run, or the user's input.
    armed: Mutex<HashMap<SessionId, SettledOutcome>>,
    /// Each delegated session that can still wake counting only one-time
    /// cron jobs, keyed to its parent (`waking_children` without the
    /// recurring jobs). Locked after `waking_children`, never under the
    /// producer, run, or store locks.
    pub(super) waking_once: Mutex<HashMap<SessionId, SessionId>>,
    /// The subagent reports being delivered, by child and run, with the
    /// parent each wakes.
    reports: Mutex<HashMap<(SessionId, RunId), SessionId>>,
}

impl Default for PushSignalState {
    fn default() -> Self {
        Self {
            sender: broadcast::channel(256).0,
            armed: Mutex::new(HashMap::new()),
            waking_once: Mutex::new(HashMap::new()),
            reports: Mutex::new(HashMap::new()),
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl Engine {
    /// The engine's push signals. A receiver that lags misses signals.
    #[must_use]
    pub fn subscribe_push_signals(&self) -> broadcast::Receiver<PushSignal> {
        self.inner.push.sender.subscribe()
    }

    /// The daemon's data root, where its own files live.
    #[must_use]
    pub fn data_root(&self) -> &std::path::Path {
        self.inner.store.data_root()
    }

    /// The root of `session`'s tree: `session` itself for a root.
    #[must_use]
    pub fn root_session_of(&self, session: SessionId) -> SessionId {
        let mut current = session;
        let mut seen = HashSet::new();
        while seen.insert(current) {
            match self.inner.store.parent_of(current) {
                Some(parent) => current = parent,
                None => break,
            }
        }
        current
    }

    /// Follows the run edges and inputs of `session` that arm or disarm its
    /// settle push (spec section 3.1).
    pub(super) fn observe_push_event(&self, session: SessionId, payload: &Event) {
        match payload {
            Event::RunStarted { .. } | Event::UserInputAdmitted { .. } => {
                lock(&self.inner.push.armed).remove(&session);
            }
            Event::RunFinished { outcome } => {
                if self.inner.store.parent_of(session).is_some() {
                    // An agent's subagent's run of its work ended.
                    self.set_acp_subagent_working(session, false);
                    return;
                }
                let armed = match outcome {
                    cookie_agent_protocol::Open::Known(RunOutcome::Completed { final_text }) => {
                        Some(SettledOutcome::Completed {
                            final_text: final_text.clone(),
                        })
                    }
                    cookie_agent_protocol::Open::Known(RunOutcome::Failed { error, .. }) => {
                        Some(SettledOutcome::Failed {
                            error: error.to_string(),
                        })
                    }
                    _ => None,
                };
                match armed {
                    Some(outcome) => {
                        lock(&self.inner.push.armed).insert(session, outcome);
                        self.schedule_settle_check(session);
                    }
                    None => {
                        lock(&self.inner.push.armed).remove(&session);
                    }
                }
            }
            // A held root waits for the user however much is pending: one
            // held for failures is pushed now, one the user stopped is not
            // (`docs/site/specs/held-sessions.md` section 5).
            Event::AutomaticRunsHeld { reason } => {
                lock(&self.inner.push.armed).remove(&session);
                if self.inner.store.parent_of(session).is_some() {
                    return;
                }
                let cookie_agent_protocol::Open::Known(
                    cookie_agent_protocol::HeldReason::Failing { failures, error },
                ) = reason
                else {
                    return;
                };
                let Ok(projection) = self.inner.store.get(session) else {
                    return;
                };
                let meta = &projection.meta;
                let error = match error {
                    Some(error) => format!("stopped after {failures} failed runs: {error}"),
                    None => format!("stopped after {failures} failed runs"),
                };
                let _ = self
                    .inner
                    .push
                    .sender
                    .send(PushSignal::Settled(SettledSignal {
                        root: session,
                        outcome: SettledOutcome::Failed { error },
                        title: meta.title.as_ref().map(ToString::to_string),
                        short_id: meta.short_id.clone(),
                        cron: meta.cron.is_some(),
                        at: Timestamp::now(),
                    }));
            }
            _ => {}
        }
    }

    /// Records that the report of `child`'s `run` is on its way to
    /// `parent` (`true`), or no longer is.
    pub(super) fn report_in_flight(
        &self,
        child: SessionId,
        run: RunId,
        parent: SessionId,
        in_flight: bool,
    ) {
        let mut reports = lock(&self.inner.push.reports);
        if in_flight {
            reports.insert((child, run), parent);
        } else {
            reports.remove(&(child, run));
        }
    }

    /// Forgets deleted sessions: their armed settles, and the reports on
    /// their way from or to them.
    pub(super) fn forget_push_state(&self, deleted: &HashSet<SessionId>) {
        lock(&self.inner.push.armed).retain(|session, _| !deleted.contains(session));
        lock(&self.inner.push.reports)
            .retain(|(child, _), parent| !deleted.contains(child) && !deleted.contains(parent));
    }

    /// Checks a root armed by a run's end on its actor once the actor is
    /// done with what it is doing: a run that just ended may still start
    /// the next one.
    fn schedule_settle_check(&self, root: SessionId) {
        if !lock(&self.inner.push.armed).contains_key(&root) {
            return;
        }
        let Some(runtime) = tokio::runtime::Handle::try_current()
            .ok()
            .or_else(|| self.inner.runtime.clone())
        else {
            return;
        };
        let engine = self.clone();
        runtime.spawn(async move {
            let _ = engine
                .on_actor_unreconciled(root, move |engine| {
                    engine.check_settled(root);
                    Ok(())
                })
                .await;
        });
    }

    /// Sends `Settled` for an armed root that settled (spec section 3.2),
    /// and disarms it either way: what still waits when its run ends keeps
    /// that run from being pushed, even once it is gone. Called on the
    /// root's actor.
    fn check_settled(&self, root: SessionId) {
        let Some(outcome) = lock(&self.inner.push.armed).remove(&root) else {
            return;
        };
        if self.ensure_not_shutting_down().is_err() {
            return;
        }
        let running = self
            .inner
            .sessions
            .active
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .any(|run| run.session == root)
            || self
                .inner
                .compaction
                .in_progress
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains(&root)
            || self
                .inner
                .sessions
                .producers
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&root)
                .is_some_and(|state| state.wake_scheduled);
        if running {
            return;
        }
        let Ok(projection) = self.inner.store.get(root) else {
            return;
        };
        if projection.status == SessionStatus::Running
            || super::residency::has_runless_pending_inputs(&projection.log.event_snapshot())
        {
            return;
        }
        let Ok(producers) = self.goal_producer_projection(root) else {
            return;
        };
        if producers.messages.iter().any(super::producers::pending)
            || producers
                .goal
                .as_ref()
                .is_some_and(|goal| goal.status == GoalStatus::Active)
        {
            return;
        }
        let waking_once = lock(&self.inner.push.waking_once).clone();
        if self.wakes_once(root, &waking_once) {
            return;
        }
        let meta = &projection.meta;
        let _ = self
            .inner
            .push
            .sender
            .send(PushSignal::Settled(SettledSignal {
                root,
                outcome,
                title: meta.title.as_ref().map(ToString::to_string),
                short_id: meta.short_id.clone(),
                cron: meta.cron.is_some(),
                at: Timestamp::now(),
            }));
    }

    /// Whether `session` can still wake on its own, counting one-time cron
    /// jobs and not recurring ones: a background call, a background
    /// delegation, an external producer, a one-time job, or a direct child
    /// that runs a turn or itself wakes so (`waking_once`), a child's
    /// report on its way, a wake already scheduled for a producer message
    /// that arrived, an ACP agent's command that runs, or a subagent of the
    /// ACP agent's own that works. The goal is checked apart, from the
    /// fold.
    pub(super) fn wakes_once(
        &self,
        session: SessionId,
        waking_once: &HashMap<SessionId, SessionId>,
    ) -> bool {
        let registered = self
            .inner
            .sessions
            .producers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&session)
            .is_some_and(|state| {
                // A producer message that arrived starts a run: the
                // session wakes until that run is running.
                state.wake_scheduled
                    || state.registrations.iter().any(|registration| {
                        match &registration.authority.owner {
                            ProducerOwner::Tool { .. } | ProducerOwner::Delegation { .. } => true,
                            owner if super::cron_fire::is_cron_owner(owner) => false,
                            ProducerOwner::External { .. } => true,
                            _ => false,
                        }
                    })
            });
        registered
            || lock(&self.inner.push.reports)
                .values()
                .any(|parent| *parent == session)
            || self.inner.crons.waits_on_once(session)
            || waking_once.values().any(|parent| *parent == session)
            || self.has_running_subagent_turn(session)
            || self.acp_background_running(session)
            || self.acp_subagent_working(session)
    }

    /// Sends `NeedsYou` for an approval that escalated to the user in
    /// `session` (spec section 4.1).
    pub(super) fn signal_approval_needs_you(
        &self,
        session: SessionId,
        request: &ApprovalRequest,
        expires_at: Option<Timestamp>,
    ) {
        let tool_call_id = request.tool_call_id();
        let presentation = self.inner.store.get(session).ok().and_then(|projection| {
            projection
                .log
                .event_snapshot()
                .iter()
                .rev()
                .find_map(|event| match &event.payload {
                    Event::ToolCallStarted { start } if start.tool_call_id == tool_call_id => {
                        Some(start.presentation.clone())
                    }
                    _ => None,
                })
        });
        let (tool, summary) = presentation.map_or_else(
            || ("A tool call".to_owned(), None),
            |presentation| {
                (
                    presentation.title.to_string(),
                    presentation
                        .primary_argument
                        .as_ref()
                        .map(ToString::to_string),
                )
            },
        );
        self.signal_needs_you(
            session,
            NeedsYouItem::Approval {
                tool,
                summary,
                expires_at,
            },
        );
    }

    /// Sends `NeedsYou` for a question an ACP agent asks in `session`
    /// (spec section 4.2).
    pub(crate) fn signal_question_needs_you(
        &self,
        session: SessionId,
        agent: String,
        message: String,
        url: bool,
    ) {
        self.signal_needs_you(
            session,
            NeedsYouItem::Question {
                agent,
                message,
                url,
            },
        );
    }

    fn signal_needs_you(&self, session: SessionId, item: NeedsYouItem) {
        let root = self.root_session_of(session);
        let meta = |id: SessionId| {
            self.inner
                .store
                .summary(id)
                .ok()
                .map(|summary| summary.meta)
        };
        let root_meta = meta(root);
        let handle = (session != root)
            .then(|| meta(session).and_then(|meta| meta.short_id))
            .flatten();
        let _ = self
            .inner
            .push
            .sender
            .send(PushSignal::NeedsYou(NeedsYouSignal {
                root,
                session,
                handle,
                root_title: root_meta
                    .as_ref()
                    .and_then(|meta| meta.title.as_ref().map(ToString::to_string)),
                root_short_id: root_meta.and_then(|meta| meta.short_id),
                item,
                at: Timestamp::now(),
            }));
    }
}
