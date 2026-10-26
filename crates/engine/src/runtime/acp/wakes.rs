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
    AgentId, ProducerDeliveryMode, ProducerIdempotencyKey, ProducerMessageId, ProducerOwner,
    SafeDisplayText, SessionId, SessionStatus, UserContent,
};
use serde_json::Value;

use super::super::{Engine, EngineError, Event, event_origin};
use super::process::SessionEvent;
use super::runner::LiveSession;

/// How long the listener waits for a wake's run to take the session's
/// updates before it listens again.
const WAKE_START_WAIT: Duration = Duration::from_secs(30);

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
                () = live.process.exited.cancelled() => return,
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
            drop(events);
            let Some(event) = event else {
                return;
            };
            if !self.still_live(&live) {
                return;
            }
            let wakes = starts_work(&event);
            live.carried
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(event);
            if !wakes {
                continue;
            }
            // Listen again once the wake's run took the updates; its round
            // holds them until the work ends.
            let before = rounds.load(Ordering::SeqCst);
            let taken = round_taken.notified();
            tokio::pin!(taken);
            taken.as_mut().enable();
            if self
                .start_acp_wake(live.session, live.agent.id.clone())
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

    /// Whether `live` is still its cookie session's ACP session.
    fn still_live(&self, live: &LiveSession) -> bool {
        self.live_session(live.session)
            .is_some_and(|known| known.acp_session_id == live.acp_session_id)
    }

    /// Records that the session's ACP agent `agent` started work on its
    /// own, which starts a run to show it; nothing when a run is already
    /// open, which takes the work in.
    async fn start_acp_wake(&self, session: SessionId, agent: AgentId) -> Result<(), EngineError> {
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
                    description: SafeDisplayText::new("Woke on its own")
                        .expect("static description"),
                    body: UserContent::text(WAKE_BODY),
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
}
