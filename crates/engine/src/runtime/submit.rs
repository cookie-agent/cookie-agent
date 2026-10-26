//! Input from any client: start a run, steer the running one, or queue
//! behind it, decided atomically; take back one's own pending or queued
//! input; and start the queue's head once a run completes.

use std::time::Duration;

use cookie_agent_protocol::{
    EventOrigin, InputDelivery, QueuedInputs, RunId, RunSelection, RunStartParams, SessionId,
    SessionRecallInputResult, SessionStatus, SessionSubmitInputParams, SessionSubmitInputResult,
};

use super::{Engine, EngineError, Event, mailbox::pending_inputs};

/// Times `submit_input` re-reads a session whose run started or ended while
/// the input was on its way. Each wait is longer than the last.
const SUBMIT_ATTEMPTS: u32 = 6;
const SUBMIT_RETRY_BASE: Duration = Duration::from_millis(20);

impl Engine {
    /// Starts a run when the session is idle; otherwise steers or queues
    /// per `params.delivery`. A run that starts or ends while the input is
    /// on its way only changes which of those happens.
    pub async fn submit_input(
        &self,
        params: SessionSubmitInputParams,
        origin: EventOrigin,
    ) -> Result<SessionSubmitInputResult, EngineError> {
        let session = params.session_id;
        self.ensure_appendable(session)?;
        self.ensure_session_owned(session)?;
        for attempt in 0..SUBMIT_ATTEMPTS {
            if attempt > 0 {
                tokio::time::sleep(SUBMIT_RETRY_BASE * attempt).await;
            }
            // A manual compaction holds the session; the input waits for it
            // rather than failing.
            self.wait_for_compaction(session).await;
            if let Some(run_id) = self.running_run(session)? {
                match params.delivery {
                    InputDelivery::Steer => {
                        let steered = self
                            .steer(run_id, params.content.clone(), origin.clone())
                            .await?;
                        if let Some(reason) = steered.handled_reason {
                            return Ok(SessionSubmitInputResult::Handled { reason });
                        }
                        if let Some(user_input_seq) = steered.user_input_seq {
                            return Ok(SessionSubmitInputResult::Steered {
                                run_id,
                                user_input_seq,
                            });
                        }
                    }
                    InputDelivery::Queue => {
                        if let Some(queued_seq) = self.queue_input(&params, origin.clone()).await? {
                            return Ok(SessionSubmitInputResult::Queued { queued_seq });
                        }
                    }
                }
                // The run ended before the input reached it: start one.
            }
            let selection = match &params.selection {
                Some(selection) => selection.clone(),
                None => self.current_selection(session)?,
            };
            match self
                .start_run(
                    RunStartParams {
                        reset_fallback: params.reset_fallback,
                        session_id: session,
                        client_run_id: params.client_run_id.clone(),
                        selection,
                        content: params.content.clone(),
                    },
                    origin.clone(),
                )
                .await
            {
                Ok(started) => {
                    return Ok(SessionSubmitInputResult::Started {
                        run_id: started.run_id,
                    });
                }
                Err(EngineError::InputHandled(reason)) => {
                    return Ok(SessionSubmitInputResult::Handled { reason });
                }
                // Another client's run started first: steer or queue it.
                Err(EngineError::SessionRunning(_)) => {}
                Err(error) => return Err(error),
            }
        }
        Err(EngineError::SessionRunning(session))
    }

    /// Makes `selection` the session's model selection, which later runs use
    /// unless a request names its own and every client's draft follows.
    /// Validated as a run start would validate it. Returns false when the
    /// session already had it.
    pub async fn select_model(
        &self,
        session: SessionId,
        selection: RunSelection,
        origin: EventOrigin,
    ) -> Result<bool, EngineError> {
        self.ensure_session_owned(session)?;
        let projection = self.inner.store.get(session)?;
        self.reconcile_provider_store_off_worker().await?;
        match &projection.meta.origin {
            cookie_agent_protocol::SessionOrigin::Root => {
                self.freeze_root_selection(
                    &selection,
                    false,
                    &crate::permissions::MemoryVariables::default(),
                )?;
            }
            cookie_agent_protocol::SessionOrigin::Delegated { .. } => {
                let (_, frozen) = self.freeze_delegated_selection(
                    &projection.creation_agent,
                    projection.meta.creation_selection.preset.as_deref(),
                    &selection,
                    crate::policy::ResultLimits::from_config(&self.inner.config.load().runtime),
                    &crate::permissions::MemoryVariables::default(),
                )?;
                if frozen != selection {
                    return Err(EngineError::UnavailableAgentModel {
                        agent: selection.agent.clone(),
                        model: selection.model.model.clone(),
                    });
                }
            }
        }
        self.on_actor(session, move |engine| {
            if engine.session_model_selection(session)? == selection {
                return Ok(false);
            }
            engine.append_direct(
                session,
                None,
                origin,
                Event::SessionModelSelected { selection },
            )?;
            Ok(true)
        })
        .await
    }

    /// Withdraws `seq`, a pending steered input of the running run or a
    /// queued input, for any client: the recall is recorded under the
    /// recalling `origin`, and its content goes back to that client.
    pub async fn recall_input(
        &self,
        session: SessionId,
        seq: u64,
        origin: EventOrigin,
    ) -> Result<SessionRecallInputResult, EngineError> {
        self.ensure_session_owned(session)?;
        self.on_actor(session, move |engine| {
            let projection = engine.inner.store.get(session)?;
            let events = projection.log.event_snapshot();
            if let Some(run) = running_run_of(&projection)
                && let Some(pending) = pending_inputs(&events, run)
                    .into_iter()
                    .find(|pending| pending.admission_seq == seq)
            {
                engine.append_direct(
                    session,
                    Some(run),
                    origin,
                    Event::UserInputRecalled {
                        user_input_seq: seq,
                        content: pending.content.clone(),
                    },
                )?;
                return Ok(SessionRecallInputResult::Recalled {
                    content: pending.content,
                });
            }
            let queue = QueuedInputs::from_events(&events);
            let Some(entry) = queue.get(seq) else {
                return Ok(SessionRecallInputResult::Gone);
            };
            let content = entry.content.clone();
            engine.append_direct(
                session,
                None,
                origin,
                Event::UserInputQueueRecalled { queued_seq: seq },
            )?;
            Ok(SessionRecallInputResult::Recalled { content })
        })
        .await
    }

    /// Starts a run with the oldest queued input once the session's run
    /// completed. Called after a run's task ends and when a session is
    /// resumed, which covers a daemon that stopped between the two.
    pub(super) async fn drain_queued_input(&self, session: SessionId) {
        loop {
            let Ok(projection) = self.inner.store.get(session) else {
                return;
            };
            if projection.status == SessionStatus::Running {
                return;
            }
            let queue = QueuedInputs::from_events(&projection.log.event_snapshot());
            let Some(head) = queue.head().cloned() else {
                return;
            };
            let origin = head
                .origin
                .clone()
                .unwrap_or_else(|| super::event_origin("user"));
            let started = self
                .start_run(
                    RunStartParams {
                        reset_fallback: false,
                        session_id: session,
                        client_run_id: head.client_run_id,
                        selection: head.selection,
                        content: head.content,
                    },
                    origin,
                )
                .await;
            let outcome = match started {
                // The started run consumed the entry; a client's run that
                // won the race keeps the queue waiting for its completion.
                Ok(_) | Err(EngineError::SessionRunning(_)) => return,
                // A scheduled restart: the queue waits for the restarted
                // daemon, which drains it when the session resumes.
                Err(EngineError::Restarting) => return,
                // A plugin took the input: drop it and try the next one.
                Err(EngineError::InputHandled(_)) => {
                    self.append(
                        session,
                        None,
                        super::event_origin("engine:input-queue"),
                        Event::UserInputQueueRecalled {
                            queued_seq: head.queued_seq,
                        },
                    )
                    .await
                }
                // Nothing can run it: hand every queued input back to its
                // author instead of leaving the queue stuck.
                Err(error) => {
                    eprintln!(
                        "session {session} queued input could not start: {}",
                        error.user_message()
                    );
                    self.append(
                        session,
                        None,
                        super::event_origin("engine:input-queue"),
                        Event::UserInputQueueVoided {},
                    )
                    .await
                }
            };
            if let Err(error) = outcome {
                eprintln!(
                    "session {session} input queue update failed: {}",
                    error.user_message()
                );
                return;
            }
        }
    }

    /// Appends `params` to the session's queue on its actor, or returns
    /// `None` when no run is active to queue behind.
    async fn queue_input(
        &self,
        params: &SessionSubmitInputParams,
        origin: EventOrigin,
    ) -> Result<Option<u64>, EngineError> {
        let session = params.session_id;
        let selection = match &params.selection {
            Some(selection) => selection.clone(),
            None => self.current_selection(session)?,
        };
        let content = params.content.clone();
        // The run the entry starts uses its selection's model.
        let _input_artifacts = self
            .admit_input_media(
                session,
                &content,
                &selection.model.model,
                &super::uploads::catalog_input_media(
                    &self.current_runtime(),
                    &selection.model.model,
                ),
            )
            .await?;
        let client_run_id = params.client_run_id.clone();
        self.on_actor(session, move |engine| {
            let projection = engine.inner.store.get(session)?;
            if running_run_of(&projection).is_none() {
                return Ok(None);
            }
            let events = projection.log.event_snapshot();
            if let Some(entry) = QueuedInputs::from_events(&events)
                .entries()
                .iter()
                .find(|entry| entry.client_run_id == client_run_id)
            {
                // A retried submission resolves to the entry it queued.
                return if entry.content == content && entry.selection == selection {
                    Ok(Some(entry.queued_seq))
                } else {
                    Err(EngineError::RunIdempotencyConflict)
                };
            }
            if projection
                .runs
                .values()
                .any(|run| run.client_run_id == client_run_id)
            {
                return Err(EngineError::RunIdempotencyConflict);
            }
            engine.append_direct(
                session,
                None,
                origin,
                Event::UserInputQueued {
                    content,
                    selection,
                    client_run_id,
                },
            )?;
            Ok(Some(engine.inner.store.get(session)?.meta.last_event_seq))
        })
        .await
    }

    fn running_run(&self, session: SessionId) -> Result<Option<RunId>, EngineError> {
        Ok(running_run_of(&*self.inner.store.get(session)?))
    }

    /// The selection a run started without one uses: the session's picked
    /// or latest, or its creation selection before any run.
    fn current_selection(&self, session: SessionId) -> Result<RunSelection, EngineError> {
        self.session_model_selection(session)
    }
}

fn running_run_of(projection: &crate::session::SessionProjection) -> Option<RunId> {
    projection
        .runs
        .values()
        .find(|run| run.status == SessionStatus::Running)
        .map(|run| run.id)
}
