use std::sync::{Arc, Weak};

use cookie_agent_protocol::{EventPayload, RunId, SessionId, StoredEvent};

use super::{Engine, EngineError, Inner, SessionCommand, event_origin, producers::ProducerCommand};

/// A request-input reservation, not evidence that a provider received the input.
/// The lease remains held through compaction, hooks, streaming, and model commit.
pub(super) struct ClaimedPrompt {
    pub events: Arc<[StoredEvent]>,
    engine: Weak<Inner>,
    session: SessionId,
    run: RunId,
    claim_seq: Option<u64>,
}

impl ClaimedPrompt {
    pub async fn release(&mut self) -> Result<(), EngineError> {
        let Some(claim_seq) = self.claim_seq else {
            return Ok(());
        };
        let inner = self.engine.upgrade().ok_or(EngineError::ActorStopped)?;
        Engine { inner }
            .request(self.session, |reply| {
                SessionCommand::Producer(ProducerCommand::ReleaseClaim {
                    run: self.run,
                    claim_seq,
                    reply,
                })
            })
            .await?;
        self.claim_seq = None;
        Ok(())
    }
}

impl Drop for ClaimedPrompt {
    fn drop(&mut self) {
        let Some(claim_seq) = self.claim_seq.take() else {
            return;
        };
        let Some(inner) = self.engine.upgrade() else {
            return;
        };
        let engine = Engine { inner };
        let Some(runtime) = engine
            .inner
            .runtime
            .clone()
            .or_else(|| tokio::runtime::Handle::try_current().ok())
        else {
            return;
        };
        let session = self.session;
        let run = self.run;
        let release = engine.clone();
        engine.spawn_admission_task(&runtime, async move {
            let _ = release
                .request(session, |reply| {
                    SessionCommand::Producer(ProducerCommand::ReleaseClaim {
                        run,
                        claim_seq,
                        reply,
                    })
                })
                .await;
        });
    }
}

impl Engine {
    pub(super) async fn claim_existing_producer_inputs(
        &self,
        session: SessionId,
        run: RunId,
    ) -> Result<ClaimedPrompt, EngineError> {
        self.request(session, |reply| {
            SessionCommand::Producer(ProducerCommand::ClaimInputs { run, reply })
        })
        .await
    }

    pub(super) fn claim_producer_snapshot_direct(
        &self,
        session: SessionId,
        run: RunId,
    ) -> Result<ClaimedPrompt, EngineError> {
        self.reconcile_goal_registration(session)?;
        let projection = self.goal_producer_projection(session)?;
        let message_ids: Vec<_> = projection
            .messages
            .iter()
            .filter(|message| {
                !message.consumed
                    && !message.discarded
                    && message.admission.is_some_and(|(owner, _)| owner == run)
            })
            .map(|message| message.message_id)
            .collect();
        let claim_seq = if message_ids.is_empty() {
            None
        } else {
            let event = self.append_direct_record(
                session,
                Some(run),
                event_origin("engine:producer"),
                EventPayload::ProducerMessagesClaimed { message_ids },
            )?;
            self.inner.store.persist_buffered_session(session)?;
            Some(event.seq)
        };
        Ok(ClaimedPrompt {
            events: self.inner.store.get(session)?.log.event_snapshot(),
            engine: Arc::downgrade(&self.inner),
            session,
            run,
            claim_seq,
        })
    }

    pub(super) fn release_producer_claim_direct(
        &self,
        session: SessionId,
        run: RunId,
        claim_seq: u64,
        recovery: bool,
    ) -> Result<(), EngineError> {
        let projection = self.goal_producer_projection(session)?;
        let Some(claim) = projection.claims.get(&claim_seq) else {
            return Ok(());
        };
        if claim.run_id != run {
            return Err(EngineError::Producer("claim belongs to another run".into()));
        }
        let event = EventPayload::ProducerMessagesReleased { claim_seq };
        if recovery {
            self.append_recovery_direct(session, Some(run), event_origin("engine:producer"), event)
        } else {
            self.append_direct(session, Some(run), event_origin("engine:producer"), event)
        }
    }

    pub(super) fn release_recovered_producer_claims(
        &self,
        session: SessionId,
    ) -> Result<(), EngineError> {
        for (claim_seq, claim) in self.goal_producer_projection(session)?.claims {
            self.release_producer_claim_direct(session, claim.run_id, claim_seq, true)?;
        }
        Ok(())
    }
}
