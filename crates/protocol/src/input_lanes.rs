//! The queued-input lane, as every reader of a session log reduces it.
//!
//! A queued input waits for the running run to complete. Once that happens,
//! the engine starts a run with it, and that run's `run_started` carries the
//! entry's `client_run_id`. The queue empties on any failed, cancelled, or
//! interrupted run, and on `user_input_queue_voided`. Voided entries go back
//! to their authors, who restore the text for editing.

use std::borrow::Borrow;

use crate::{ClientRunId, EventOrigin, EventPayload, RunSelection, StoredEvent, UserContent};

/// One input waiting in a session's queue.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QueuedInput {
    /// The seq of its `user_input_queued` event.
    pub queued_seq: u64,
    /// Who queued it; absent only on records written without an origin.
    pub origin: Option<EventOrigin>,
    pub content: UserContent,
    pub selection: RunSelection,
    pub client_run_id: ClientRunId,
    pub queued_at: jiff::Timestamp,
}

/// A session's queued inputs, oldest first.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct QueuedInputs {
    entries: Vec<QueuedInput>,
}

impl QueuedInputs {
    /// The queue after a session's physical `events`, in order.
    #[must_use]
    pub fn from_events<E: Borrow<StoredEvent>>(events: &[E]) -> Self {
        let mut queue = Self::default();
        for event in events {
            queue.apply(event.borrow());
        }
        queue
    }

    /// Folds one event and returns the entries it voided, oldest first.
    pub fn apply(&mut self, event: &StoredEvent) -> Vec<QueuedInput> {
        self.apply_payload(
            event.seq,
            event.timestamp,
            event.origin.as_ref(),
            &event.payload,
        )
    }

    /// [`Self::apply`] for a reader that holds an event's parts.
    pub fn apply_payload(
        &mut self,
        seq: u64,
        timestamp: jiff::Timestamp,
        origin: Option<&EventOrigin>,
        payload: &EventPayload,
    ) -> Vec<QueuedInput> {
        match payload {
            EventPayload::UserInputQueued {
                content,
                selection,
                client_run_id,
            } => self.entries.push(QueuedInput {
                queued_seq: seq,
                origin: origin.cloned(),
                content: content.clone(),
                selection: selection.clone(),
                client_run_id: client_run_id.clone(),
                queued_at: timestamp,
            }),
            EventPayload::RunStarted { client_run_id, .. } => {
                self.entries
                    .retain(|entry| &entry.client_run_id != client_run_id);
            }
            EventPayload::UserInputQueueRecalled { queued_seq } => {
                self.entries.retain(|entry| entry.queued_seq != *queued_seq);
            }
            EventPayload::RunFinished { outcome }
                if outcome.status() != crate::SessionStatus::Completed =>
            {
                return std::mem::take(&mut self.entries);
            }
            EventPayload::UserInputQueueVoided {} => {
                return std::mem::take(&mut self.entries);
            }
            _ => {}
        }
        Vec::new()
    }

    #[must_use]
    pub fn entries(&self) -> &[QueuedInput] {
        &self.entries
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The oldest entry: the next run the queue starts.
    #[must_use]
    pub fn head(&self) -> Option<&QueuedInput> {
        self.entries.first()
    }

    #[must_use]
    pub fn get(&self, queued_seq: u64) -> Option<&QueuedInput> {
        self.entries
            .iter()
            .find(|entry| entry.queued_seq == queued_seq)
    }
}

#[cfg(test)]
mod tests {
    use super::QueuedInputs;
    use crate::{
        AgentId, ClientRunId, EventOrigin, EventPayload, ModelSelection, RunId, RunSelection,
        SessionId, StoredEvent,
    };

    fn selection() -> RunSelection {
        RunSelection {
            agent: AgentId::new("primary").unwrap(),
            model: ModelSelection {
                model: "custom.test/model".parse().unwrap(),
                variant: None,
            },
            preset: None,
        }
    }

    fn event(seq: u64, origin: &str, payload: EventPayload) -> StoredEvent {
        StoredEvent {
            engine_version: None,
            origin: Some(EventOrigin::new(origin).unwrap()),
            session_id: SessionId::new_v7(),
            run_id: None,
            seq,
            timestamp: jiff::Timestamp::new(seq as i64, 0).unwrap(),
            payload,
        }
    }

    fn queued(seq: u64, origin: &str, input: &str) -> StoredEvent {
        event(
            seq,
            origin,
            EventPayload::UserInputQueued {
                content: crate::UserContent::text(input),
                selection: selection(),
                client_run_id: ClientRunId::new(format!("queued-{seq}")).unwrap(),
            },
        )
    }

    #[test]
    fn recall_removes_only_its_entry_and_the_head_keeps_its_author() {
        let mut queue = QueuedInputs::default();
        for entry in [
            queued(1, "client:tui-a", "first"),
            queued(2, "client:tui-b", "second"),
            queued(3, "client:tui-a", "third"),
        ] {
            assert!(queue.apply(&entry).is_empty());
        }
        let recalled = event(
            4,
            "client:tui-a",
            EventPayload::UserInputQueueRecalled { queued_seq: 2 },
        );
        assert!(queue.apply(&recalled).is_empty());
        assert_eq!(
            queue
                .entries()
                .iter()
                .map(|entry| entry.content.as_plain_text().into_owned())
                .collect::<Vec<_>>(),
            ["first", "third"]
        );
        assert_eq!(
            queue.head().and_then(|entry| entry.origin.clone()),
            Some(EventOrigin::new("client:tui-a").unwrap())
        );
        assert_eq!(queue.get(3).map(|entry| entry.queued_seq), Some(3));
    }

    #[test]
    fn a_failed_cancelled_or_interrupted_run_voids_the_whole_queue() {
        for terminal in [
            EventPayload::run_finished(crate::RunOutcome::Cancelled { reason: None }),
            EventPayload::run_finished(crate::RunOutcome::Interrupted { reason: None }),
            EventPayload::UserInputQueueVoided {},
        ] {
            let mut queue = QueuedInputs::default();
            queue.apply(&queued(1, "client:tui-a", "first"));
            queue.apply(&queued(2, "client:tui-b", "second"));
            let mut terminal = event(3, "engine:model-loop", terminal);
            terminal.run_id = Some(RunId::new_v7());
            let voided = queue.apply(&terminal);
            assert_eq!(
                voided
                    .iter()
                    .map(|entry| entry.content.as_plain_text().into_owned())
                    .collect::<Vec<_>>(),
                ["first", "second"]
            );
            assert!(queue.is_empty());
        }
    }

    #[test]
    fn completion_leaves_the_queue_for_the_engine_to_drain() {
        let mut queue = QueuedInputs::default();
        queue.apply(&queued(1, "client:tui-a", "first"));
        let mut completed = event(
            2,
            "engine:model-loop",
            EventPayload::run_finished(crate::RunOutcome::Completed { final_text: None }),
        );
        completed.run_id = Some(RunId::new_v7());
        assert!(queue.apply(&completed).is_empty());
        assert_eq!(queue.entries().len(), 1);
    }
}
