//! Work an ACP agent starts on its own (`docs/site/specs/acp-v2.md`
//! sections 6 and 7), such as Claude Code's follow-up after a background
//! command it started ends.
//!
//! Between runs a listener reads the session's updates and keeps them for
//! the next round. When the agent's foreground work starts with no run of
//! cookie's open (`state_update` `running` or `requires_action`, or a
//! request to answer), the session accepts a producer message from the
//! agent itself, owner `acp_agent`, and the run it starts listens: it sends
//! no prompt, takes what the listener kept, and ends at the work's `idle`.

use std::sync::{Arc, atomic::Ordering};
use std::time::Duration;

use cookie_agent_protocol::{
    AcpSessionUpdate, AgentId, ProducerDeliveryMode, ProducerIdempotencyKey, ProducerMessageId,
    ProducerOwner, SafeDisplayText, SessionId, SessionStatus, UserContent,
};
use serde_json::Value;

use super::super::{Engine, EngineError, Event, event_origin};
use super::process::SessionEvent;
use super::runner::LiveSession;
use super::terminal::TerminalUpdate;

/// How long the listener waits for a wake's run to take the session's
/// updates before it listens again.
const WAKE_START_WAIT: Duration = Duration::from_secs(30);

/// How long a subagent's first work waits for its task: the adapter sends
/// the subagent's `running` first and its task right after.
const TASK_WAIT: Duration = Duration::from_secs(2);

/// The body of a wake's producer message: v2 names no cause.
const WAKE_BODY: &str = "The agent started work on its own.";

/// Whether `event` means the agent's foreground work started: its state
/// is `running` or `requires_action`, or it asks something to answer.
fn starts_work(event: &SessionEvent) -> bool {
    match event {
        SessionEvent::Update(update) => {
            update.get("sessionUpdate").and_then(Value::as_str) == Some("state_update")
                && matches!(
                    update.get("state").and_then(Value::as_str),
                    Some("running" | "requires_action")
                )
        }
        SessionEvent::Permission(..)
        | SessionEvent::BridgeCall { .. }
        | SessionEvent::Elicitation(..) => true,
    }
}

/// The task a message to `live`'s session gave it among what the listener
/// kept, such as a subagent's prompt from its parent: the wake's body. It
/// is taken out of what the listener kept, so the run does not show it a
/// second time.
fn take_task(live: &LiveSession) -> Option<String> {
    let mut carried = live
        .carried
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let (index, task) = carried
        .iter()
        .enumerate()
        .rev()
        .find_map(|(index, event)| task_text(live, event).map(|task| (index, task)))?;
    carried.remove(index);
    Some(task)
}

/// The task `event` gives `live`'s session: the text of a message to it.
fn task_text(live: &LiveSession, event: &SessionEvent) -> Option<String> {
    let SessionEvent::Update(update) = event else {
        return None;
    };
    (update.get("sessionUpdate").and_then(Value::as_str) == Some("session_message")
        && update.get("recipientSessionId").and_then(Value::as_str)
            == Some(live.acp_session_id.as_str()))
    .then(|| {
        update
            .get("content")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(super::wire::content_text)
            .collect::<Vec<_>>()
            .join("\n")
    })
    .filter(|text| !text.trim().is_empty())
}

impl Engine {
    /// Listens to `live`'s session between runs, until the session is no
    /// longer `live` or its process ends.
    pub(super) fn listen_between_runs(&self, live: &LiveSession) {
        let engine = self.clone();
        let live = live.clone();
        tokio::spawn(async move { engine.listen(live).await });
    }

    async fn listen(self, live: LiveSession) {
        let (rounds, round_taken) = &*Arc::clone(&live.rounds);
        loop {
            // A round is about to take the updates: stay off them.
            if live.waiting.load(Ordering::SeqCst) > 0 {
                tokio::select! {
                    () = tokio::time::sleep(Duration::from_millis(5)) => {}
                    () = live.process.exited.cancelled() => return,
                }
                continue;
            }
            let mut events = tokio::select! {
                events = live.events.lock() => events,
                () = live.process.exited.cancelled() => {
                    self.acp_commands_gone(live.session);
                    return;
                }
            };
            if live.waiting.load(Ordering::SeqCst) > 0 {
                continue;
            }
            let event = tokio::select! {
                biased;
                // A round wants the updates: it takes the lock next.
                () = live.wanted.notified() => {
                    drop(events);
                    tokio::task::yield_now().await;
                    continue;
                }
                event = events.recv() => event,
            };
            let Some(event) = event else {
                if live.process.exited.is_cancelled() {
                    self.acp_commands_gone(live.session);
                }
                return;
            };
            // Work starts: what already came with it too, such as the task a
            // subagent got, which follows its `running`.
            let mut batch = vec![event];
            if starts_work(&batch[0]) {
                while let Ok(more) = events.try_recv() {
                    batch.push(more);
                }
                // A subagent's first work starts before its task arrives;
                // waiting for it lets the run start with the task.
                let has_task = |batch: &[SessionEvent]| {
                    batch
                        .iter()
                        .chain(
                            live.carried
                                .lock()
                                .unwrap_or_else(|poisoned| poisoned.into_inner())
                                .iter(),
                        )
                        .any(|event| task_text(&live, event).is_some())
                };
                if !has_task(&batch) && self.awaits_first_task(live.session) {
                    let deadline = tokio::time::Instant::now() + TASK_WAIT;
                    while let Ok(Some(more)) =
                        tokio::time::timeout_at(deadline, events.recv()).await
                    {
                        let task = task_text(&live, &more).is_some();
                        batch.push(more);
                        if task {
                            break;
                        }
                    }
                }
            }
            drop(events);
            if !self.still_live(&live) {
                return;
            }
            let mut wakes = false;
            for event in batch {
                // A command running in the background ended: the session
                // shows it before the run it may wake. The run that takes
                // the update finds the command already gone.
                if let SessionEvent::Update(update) = &event
                    && update.get("sessionUpdate").and_then(Value::as_str)
                        == Some("terminal_update")
                    && let Some(read) = TerminalUpdate::read("terminal_update", update)
                    && let Some(exit) = read.exit
                    && let Some(ended) = self.acp_terminal_exited(live.session, &read.id, exit)
                {
                    let _ = self
                        .append(
                            live.session,
                            None,
                            event_origin("engine:acp"),
                            Event::AcpSessionUpdated {
                                update: AcpSessionUpdate {
                                    background_ended: Some(ended),
                                    ..AcpSessionUpdate::default()
                                },
                            },
                        )
                        .await;
                }
                // A subagent announced between runs gets its session at
                // once, so its own work shows while this session is idle.
                if let SessionEvent::Update(update) = &event
                    && update.get("sessionUpdate").and_then(Value::as_str)
                        == Some("subagent_update")
                {
                    self.acp_subagent_update(&live, None, update).await;
                    continue;
                }
                wakes |= starts_work(&event);
                live.carried
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .push(event);
            }
            if !wakes {
                continue;
            }
            // Listen again once the wake's run took the updates; its round
            // holds them until the work ends.
            let before = rounds.load(Ordering::SeqCst);
            let taken = round_taken.notified();
            tokio::pin!(taken);
            taken.as_mut().enable();
            let task = take_task(&live);
            if self
                .start_acp_wake(live.session, live.agent.id.clone(), task)
                .await
                .is_err()
            {
                continue;
            }
            if rounds.load(Ordering::SeqCst) == before {
                tokio::select! {
                    () = taken => {}
                    () = tokio::time::sleep(WAKE_START_WAIT) => {}
                    () = live.process.exited.cancelled() => return,
                }
            }
        }
    }

    /// Whether `session` is an ACP agent's subagent that has not run yet,
    /// whose first work comes with a task.
    fn awaits_first_task(&self, session: SessionId) -> bool {
        self.inner.store.get(session).is_ok_and(|projection| {
            projection.log.last_run_started().is_none()
                && super::subagents::is_acp_subagent(&projection.log.event_snapshot())
        })
    }

    /// Whether `live` is still its cookie session's ACP session.
    fn still_live(&self, live: &LiveSession) -> bool {
        self.live_session(live.session)
            .is_some_and(|known| known.acp_session_id == live.acp_session_id)
    }

    /// Records that the session's ACP agent `agent` started work on its
    /// own, which starts a run to show it; nothing when a run is already
    /// open, which takes the work in.
    async fn start_acp_wake(
        &self,
        session: SessionId,
        agent: AgentId,
        task: Option<String>,
    ) -> Result<(), EngineError> {
        self.on_actor(session, move |engine| {
            let running = engine
                .inner
                .sessions
                .active
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .values()
                .any(|run| run.session == session)
                || engine.inner.store.get(session)?.status == SessionStatus::Running;
            if running {
                return Ok(());
            }
            engine.append_direct(
                session,
                None,
                event_origin("engine:acp"),
                Event::ProducerMessageAccepted {
                    message_id: ProducerMessageId::new_v7(),
                    producer_owner: ProducerOwner::AcpAgent { agent },
                    mode: ProducerDeliveryMode::Steer,
                    idempotency_key: ProducerIdempotencyKey::new(
                        ProducerMessageId::new_v7().to_string(),
                    )
                    .expect("UUID idempotency key"),
                    description: SafeDisplayText::new(if task.is_some() {
                        "Started with a task"
                    } else {
                        "Woke on its own"
                    })
                    .expect("static description"),
                    body: UserContent::text(task.as_deref().unwrap_or(WAKE_BODY)),
                    reminder: None,
                    agent_hop: None,
                    retained_output: None,
                },
            )?;
            engine.inner.store.persist_buffered_session(session)?;
            Ok(())
        })
        .await
    }

    /// Drops the agent's wakes still waiting when a run of `session` is
    /// stopped: the stop ended that work, so no run starts again to show
    /// it. A wake a claim holds stays; the run releases it first.
    pub(super) async fn drop_stopped_acp_wakes(
        &self,
        session: SessionId,
    ) -> Result<(), EngineError> {
        self.on_actor(session, move |engine| {
            let projection = engine.goal_producer_projection(session)?;
            for message in projection.messages.iter().filter(|message| {
                !message.consumed
                    && !message.discarded
                    && message.claims.is_empty()
                    && matches!(message.producer_owner, ProducerOwner::AcpAgent { .. })
            }) {
                engine.append_direct(
                    session,
                    None,
                    event_origin("engine:acp"),
                    Event::ProducerMessageDiscarded {
                        message_id: message.message_id,
                        reminder: None,
                        producer_owner: message.producer_owner.clone(),
                    },
                )?;
            }
            engine.inner.store.persist_buffered_session(session)?;
            Ok(())
        })
        .await
    }
}
