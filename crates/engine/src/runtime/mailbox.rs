use std::{
    collections::{HashSet, VecDeque},
    sync::atomic::Ordering,
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use cookie_agent_protocol::{
    ApprovalStatus, EventSubscriptionMessage, EventsSubscribeResult,
    PersistedToolResult as ToolResult, RunCancelResult, RunId, RunRecallSteerResult,
    RunSteerResult, RunToolStdinResult, SafeToolError, SessionForkResult, SessionId,
    SessionRenameChange, SessionRenameResult, SessionRevertResult, SessionStatus,
    SessionTitleChange, StoredEvent, ToolCallId, ToolCallTermination, ToolTerminationOutcome,
};
use tokio::sync::{mpsc, oneshot};

#[cfg(test)]
use super::PagingRaceHook;
use super::ToolCallFailureCode;
use super::{
    ApprovalTerminal, Engine, EngineError, Event, MAX_COMPACTION_DEFERRED_COMMANDS,
    PERSISTED_SUBSCRIBER_QUEUE_CAPACITY, PendingInput, PendingPromotionState, PersistedSubscriber,
    PredictiveCompactionInput, SESSION_MAILBOX_CAPACITY, SessionCommand, ToolFailure,
    approval_projection::{approval_records, approval_run_id},
    helpers::safe_error,
    model_loop,
};
use crate::{actor::SessionActor, events, session::SessionError, tool_api::StdinWrite};

impl Engine {
    pub async fn subscribe(
        &self,
        session: SessionId,
        cursor: Option<u64>,
    ) -> Result<
        (
            EventsSubscribeResult,
            mpsc::Receiver<EventSubscriptionMessage>,
        ),
        EngineError,
    > {
        self.request(session, |reply| SessionCommand::Subscribe { cursor, reply })
            .await
    }

    /// Subscribes to a currently running call's retained output and live tail.
    /// Output is ephemeral and intentionally separate from event cursors.
    pub fn subscribe_tool_output(
        &self,
        call: ToolCallId,
        stream: cookie_agent_protocol::OutputStream,
    ) -> Option<(
        cookie_agent_protocol::OutputSnapshot,
        mpsc::Receiver<events::OutputMessage>,
    )> {
        self.inner
            .output_hubs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&call)
            .cloned()
            .map(|hub| hub.subscribe(stream, 256))
    }

    pub(super) fn retain_finalized_output_hub(&self, call: ToolCallId) {
        const FINALIZED_HUB_RETENTION: usize = 128;
        let mut finalized = self
            .inner
            .finalized_output_hubs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        finalized.push_back(call);
        if finalized.len() > FINALIZED_HUB_RETENTION
            && let Some(expired) = finalized.pop_front()
        {
            self.inner
                .output_hubs
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .remove(&expired);
        }
    }

    pub async fn append(
        &self,
        session: SessionId,
        run: Option<RunId>,
        event: Event,
    ) -> Result<(), EngineError> {
        self.request(session, |reply| SessionCommand::Append {
            run,
            event,
            reply,
        })
        .await
    }

    /// Synchronous setup/CLI wrapper. Do not call from a Tokio runtime.
    pub fn append_blocking(
        &self,
        session: SessionId,
        run: Option<RunId>,
        event: Event,
    ) -> Result<(), EngineError> {
        self.request_blocking(session, |reply| SessionCommand::Append {
            run,
            event,
            reply,
        })
    }

    /// Commits a completed tool invocation through its session actor.
    pub async fn submit_tool_result(
        &self,
        session: SessionId,
        run: RunId,
        tool_call_id: ToolCallId,
        result: Result<ToolResult, String>,
    ) -> Result<(), EngineError> {
        let result = result.map_err(|message| ToolFailure {
            code: ToolCallFailureCode::ExecutionFailed,
            message,
        });
        self.submit_tool_result_status(session, run, tool_call_id, result)
            .await
            .and_then(|committed| {
                committed.then_some(()).ok_or_else(|| {
                    EngineError::MissingTool("tool call is no longer pending".into())
                })
            })
    }

    pub(super) async fn submit_tool_result_status(
        &self,
        session: SessionId,
        run: RunId,
        tool_call_id: ToolCallId,
        result: Result<ToolResult, ToolFailure>,
    ) -> Result<bool, EngineError> {
        self.request(session, |reply| SessionCommand::ToolResult {
            run,
            tool_call_id,
            result,
            reply,
        })
        .await
    }

    pub(crate) fn append_direct(
        &self,
        session: SessionId,
        run: Option<RunId>,
        event: Event,
    ) -> Result<(), EngineError> {
        let envelope = self.inner.store.append(session, run, event)?;
        self.inner
            .subscribers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .entry(session)
            .or_default()
            .retain_mut(|subscriber| {
                // Reserve one queue slot for a control message. Once the
                // event capacity is reached, queue a gap and close this live
                // subscription; the gap is delivered even if this event is
                // terminal, and the client resumes from `last_delivered_seq`.
                let is_gap = subscriber.sender.capacity() <= 1;
                let message = if is_gap {
                    EventSubscriptionMessage::Gap {
                        session_id: session,
                        last_delivered_seq: envelope.seq.saturating_sub(1),
                    }
                } else {
                    EventSubscriptionMessage::Event {
                        event: Box::new(envelope.clone()),
                    }
                };
                match subscriber.sender.try_send(message) {
                    Ok(()) => {
                        #[cfg(test)]
                        if is_gap
                            && let Some(hook) = self
                                .inner
                                .gap_send_hook
                                .lock()
                                .expect("gap send hook lock poisoned")
                                .take()
                        {
                            let _ = hook.reached.send(());
                            let _ = hook.release.recv();
                        }
                        !is_gap
                    }
                    Err(mpsc::error::TrySendError::Full(_)) => false,
                    Err(mpsc::error::TrySendError::Closed(_)) => false,
                }
            });
        Ok(())
    }

    pub(super) async fn request<T>(
        &self,
        session: SessionId,
        command: impl FnOnce(oneshot::Sender<Result<T, EngineError>>) -> SessionCommand,
    ) -> Result<T, EngineError> {
        let (reply, receiver) = oneshot::channel();
        {
            let _residency = self.inner.residency_mutation.lock().await;
            self.inner.store.get(session)?;
            self.spawn_actor(session);
            let actor = self
                .inner
                .actors
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .get(&session)
                .cloned()
                .ok_or(EngineError::MissingActor(session))?;
            actor
                .send(command(reply))
                .await
                .map_err(|_| EngineError::ActorStopped)?;
        }
        receiver.await.map_err(|_| EngineError::ActorStopped)?
    }

    pub(super) async fn prompt_events(
        &self,
        session: SessionId,
        run: RunId,
    ) -> Result<Vec<StoredEvent>, EngineError> {
        let events = self
            .request(session, |reply| SessionCommand::PromptSnapshot {
                run,
                reply,
            })
            .await?;
        #[cfg(test)]
        if let Some(hook) = {
            self.inner
                .prompt_snapshot_hook
                .lock()
                .expect("prompt snapshot hook lock poisoned")
                .take()
        } {
            if let Some(reached) = hook
                .reached
                .lock()
                .expect("prompt snapshot reached lock poisoned")
                .take()
            {
                let _ = reached.send(());
            }
            hook.release.notified().await;
        }
        Ok(events)
    }

    #[cfg(test)]
    pub(crate) fn compaction_reserved_for_test(&self, session: SessionId) -> bool {
        self.inner
            .compaction_in_progress
            .lock()
            .expect("compaction reservation lock poisoned")
            .contains(&session)
    }

    #[cfg(test)]
    pub(crate) fn run_active_for_test(&self, run: RunId) -> bool {
        self.inner
            .active
            .lock()
            .expect("active runs lock poisoned")
            .contains_key(&run)
    }

    #[cfg(test)]
    pub(crate) fn actor_resident_for_test(&self, session: SessionId) -> bool {
        self.inner
            .actors
            .lock()
            .expect("actor registry lock poisoned")
            .contains_key(&session)
    }

    #[cfg(test)]
    pub(crate) fn install_compaction_execution_hook_for_test(
        &self,
    ) -> (oneshot::Receiver<()>, std::sync::Arc<tokio::sync::Notify>) {
        let (reached, receiver) = oneshot::channel();
        let release = std::sync::Arc::new(tokio::sync::Notify::new());
        *self
            .inner
            .compaction_execution_hook
            .lock()
            .expect("compaction execution hook lock poisoned") =
            Some(std::sync::Arc::new(PagingRaceHook {
                reached: std::sync::Mutex::new(Some(reached)),
                release: std::sync::Arc::clone(&release),
            }));
        (receiver, release)
    }

    #[cfg(test)]
    pub(crate) async fn enqueue_compact_without_residency_for_test(
        &self,
        session: SessionId,
    ) -> Result<oneshot::Receiver<Result<bool, EngineError>>, EngineError> {
        let actor = self
            .inner
            .actors
            .lock()
            .expect("actor registry lock poisoned")
            .get(&session)
            .cloned()
            .ok_or(EngineError::MissingActor(session))?;
        let (reply, receiver) = oneshot::channel();
        actor
            .send(SessionCommand::Compact { focus: None, reply })
            .await
            .map_err(|_| EngineError::ActorStopped)?;
        Ok(receiver)
    }

    pub(super) fn request_blocking<T>(
        &self,
        session: SessionId,
        command: impl FnOnce(oneshot::Sender<Result<T, EngineError>>) -> SessionCommand,
    ) -> Result<T, EngineError> {
        let (reply, receiver) = oneshot::channel();
        {
            let _residency = self.inner.residency_mutation.blocking_lock();
            self.inner.store.get(session)?;
            self.spawn_actor(session);
            let actor = self
                .inner
                .actors
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .get(&session)
                .cloned()
                .ok_or(EngineError::MissingActor(session))?;
            actor
                .blocking_send(command(reply))
                .map_err(|_| EngineError::ActorStopped)?;
        }
        receiver
            .blocking_recv()
            .map_err(|_| EngineError::ActorStopped)?
    }

    pub(crate) fn spawn_actor(&self, session: SessionId) {
        if self
            .inner
            .actors
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .contains_key(&session)
        {
            return;
        }
        self.inner
            .context_token_estimators
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .entry(session)
            .or_default();
        let engine = self.clone();
        let actor = SessionActor::spawn(SESSION_MAILBOX_CAPACITY, move |command| {
            let engine = engine.clone();
            async move { engine.handle_actor_command(session, command).await }
        });
        self.inner
            .actors
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(session, actor);
    }

    fn reserve_compaction(&self, session: SessionId) -> bool {
        self.inner
            .compaction_in_progress
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(session)
    }

    async fn finish_compaction(&self, session: SessionId) -> Result<(), EngineError> {
        self.request(session, |reply| SessionCommand::CompactionFinished {
            reply,
        })
        .await
    }

    async fn release_compaction_direct(&self, session: SessionId) {
        self.inner
            .compaction_in_progress
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&session);
        let deferred = self
            .inner
            .compaction_deferred
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&session)
            .unwrap_or_default();
        for command in deferred {
            Box::pin(self.handle_actor_command(session, command)).await;
        }
    }

    pub(super) async fn handle_actor_command(&self, session: SessionId, command: SessionCommand) {
        if let Some(kind) = command.compaction_deferred_kind()
            && self
                .inner
                .compaction_in_progress
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .contains(&session)
        {
            let rejected = {
                let mut deferred = self
                    .inner
                    .compaction_deferred
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                let queue = deferred.entry(session).or_default();
                let duplicate = queue
                    .iter()
                    .any(|pending| pending.compaction_deferred_kind() == Some(kind));
                let rejected = if duplicate {
                    Some(command)
                } else {
                    queue.push_back(command);
                    None
                };
                debug_assert!(queue.len() <= MAX_COMPACTION_DEFERRED_COMMANDS);
                rejected
            };
            if let Some(rejected) = rejected {
                rejected.reject_duplicate_during_compaction(session);
            }
            return;
        }
        match command {
            SessionCommand::EvictionBarrier { reply } => {
                let _ = reply.send(Ok(()));
            }
            SessionCommand::Append { run, event, reply } => {
                let _ = reply.send(self.append_direct(session, run, event));
            }
            SessionCommand::EnsureToolCallLinked {
                run,
                tool_call_id,
                child_session_id,
                reply,
            } => {
                let result = (|| {
                    let linked = self
                        .inner
                        .store
                        .get(session)?
                        .log
                        .events()
                        .iter()
                        .any(|event| {
                            matches!(event.payload, Event::ToolCallLinked { tool_call_id: linked_call, child_session_id: linked_child }
                                if linked_call == tool_call_id && linked_child == child_session_id)
                        });
                    if !linked {
                        self.append_direct(
                            session,
                            Some(run),
                            Event::ToolCallLinked {
                                tool_call_id,
                                child_session_id,
                            },
                        )?;
                    }
                    Ok(())
                })();
                let _ = reply.send(result);
            }
            SessionCommand::Start {
                params,
                admission,
                reply,
            } => {
                if !self.reserve_compaction(session) {
                    let _ = reply.send(Err(EngineError::SessionRunning(session)));
                } else {
                    let engine = self.clone();
                    tokio::spawn(async move {
                        let child_session_id = params.session_id;
                        let mut result = engine.start_run_direct(params, admission).await;
                        if let (Some((invocation_id, generation)), Ok(started)) =
                            (admission, result.as_ref())
                            && let Err(error) = engine.publish_admission_run(
                                invocation_id,
                                generation,
                                child_session_id,
                                started.run_id,
                            )
                        {
                            let _ = engine.cancel_run_durably(
                                started.run_id,
                                Some("delegate admission could not be published".into()),
                            );
                            result = Err(error);
                        }
                        if let Err(error) = engine.finish_compaction(session).await
                            && result.is_ok()
                        {
                            result = Err(error);
                        }
                        let started = result.as_ref().ok().map(|result| result.run_id);
                        if reply.send(result).is_err()
                            && admission.is_some()
                            && let Some(run_id) = started
                        {
                            let _ = engine.cancel_run_durably(
                                run_id,
                                Some("delegate admission reply abandoned".into()),
                            );
                        }
                    });
                }
            }
            SessionCommand::Steer { run, input, reply } => {
                let active = self
                    .inner
                    .active
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .get(&run)
                    .cloned()
                    .filter(|active| active.session == session)
                    .ok_or(EngineError::MissingRun(run));
                let result = match active {
                    Err(error) => Err(error),
                    Ok(_) => {
                        let projection = self.inner.store.get(session);
                        match projection {
                            Err(error) => Err(error.into()),
                            Ok(projection)
                                if !projection
                                    .runs
                                    .get(&run)
                                    .is_some_and(|run| run.status == SessionStatus::Running) =>
                            {
                                Ok(RunSteerResult { accepted: false })
                            }
                            Ok(_) => self
                                .append_direct(
                                    session,
                                    Some(run),
                                    Event::UserInputAdmitted { input },
                                )
                                .map(|()| RunSteerResult { accepted: true }),
                        }
                    }
                };
                let _ = reply.send(result);
            }
            SessionCommand::AdmitDelegatedResume { run, input, reply } => {
                let active = self
                    .inner
                    .active
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .get(&run)
                    .cloned()
                    .filter(|active| active.session == session)
                    .ok_or(EngineError::MissingRun(run));
                let result = match active {
                    Err(error) => Err(error),
                    Ok(_) => {
                        let projection = self.inner.store.get(session);
                        match projection {
                            Err(error) => Err(error.into()),
                            Ok(projection)
                                if !projection
                                    .runs
                                    .get(&run)
                                    .is_some_and(|run| run.status == SessionStatus::Running) =>
                            {
                                Ok(super::DelegatedResumeAdmission {
                                    accepted: false,
                                    admission_seq: None,
                                })
                            }
                            Ok(_) => self
                                .append_direct(
                                    session,
                                    Some(run),
                                    Event::UserInputAdmitted { input },
                                )
                                .and_then(|()| {
                                    let admission_seq =
                                        self.inner.store.get(session)?.meta.last_event_seq;
                                    Ok(super::DelegatedResumeAdmission {
                                        accepted: true,
                                        admission_seq: Some(admission_seq),
                                    })
                                }),
                        }
                    }
                };
                let _ = reply.send(result);
            }
            SessionCommand::RecallDelegatedResume {
                run,
                admission_seq,
                reply,
            } => {
                let result = (|| {
                    let projection = self.inner.store.get(session)?;
                    if !projection.runs.contains_key(&run) {
                        return Err(EngineError::MissingRun(run));
                    }
                    let recalled = pending_inputs(&projection.log.events(), run)
                        .into_iter()
                        .find(|pending| pending.admission_seq == admission_seq);
                    if let Some(pending) = recalled {
                        self.append_direct(
                            session,
                            Some(run),
                            Event::UserInputRecalledV2 {
                                user_input_seq: admission_seq,
                                input: pending.input,
                            },
                        )?;
                        Ok(true)
                    } else {
                        Ok(false)
                    }
                })();
                let _ = reply.send(result);
            }
            SessionCommand::RecallSteer { run, reply } => {
                let result = (|| {
                    self.inner
                        .active
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .get(&run)
                        .cloned()
                        .filter(|active| active.session == session)
                        .ok_or(EngineError::MissingRun(run))?;
                    let projection = self.inner.store.get(session)?;
                    if !projection
                        .runs
                        .get(&run)
                        .is_some_and(|run| run.status == SessionStatus::Running)
                    {
                        return Ok(RunRecallSteerResult { recalled: None });
                    }
                    let recalled = pending_inputs(&projection.log.events(), run)
                        .pop()
                        .map(|pending| pending.input);
                    if let Some(input) = &recalled {
                        self.append_direct(
                            session,
                            Some(run),
                            Event::UserInputRecalled {
                                input: input.clone(),
                            },
                        )?;
                    }
                    Ok(RunRecallSteerResult { recalled })
                })();
                let _ = reply.send(result);
            }
            SessionCommand::CommitPendingPromotion {
                run,
                through_admission_seq,
                final_text,
                complete_if_empty,
                already_promoted,
                reply,
            } => {
                let result = (|| {
                    let projection = self.inner.store.get(session)?;
                    if !projection
                        .runs
                        .get(&run)
                        .is_some_and(|run| run.status == SessionStatus::Running)
                    {
                        return Ok(PendingPromotionState {
                            promoted: already_promoted,
                            pending: Vec::new(),
                            continue_run: false,
                        });
                    }
                    let eligible = pending_inputs(&projection.log.events(), run)
                        .into_iter()
                        .take_while(|pending| pending.admission_seq <= through_admission_seq)
                        .collect::<Vec<_>>();
                    for pending in &eligible {
                        self.append_direct(
                            session,
                            Some(run),
                            Event::UserInputSubmitted {
                                input: pending.input.clone(),
                            },
                        )?;
                    }
                    let promoted = already_promoted || !eligible.is_empty();
                    let pending = pending_inputs(&self.inner.store.get(session)?.log.events(), run);
                    if pending.is_empty() && !promoted && complete_if_empty {
                        self.append_direct(session, Some(run), Event::RunCompleted { final_text })?;
                    }
                    Ok(PendingPromotionState {
                        promoted,
                        continue_run: promoted,
                        pending,
                    })
                })();
                if result.as_ref().is_ok_and(|state| state.pending.is_empty()) {
                    self.release_compaction_direct(session).await;
                }
                let _ = reply.send(result);
            }
            SessionCommand::Compact { focus, reply } => {
                if !self.reserve_compaction(session) {
                    let _ = reply.send(Err(EngineError::SessionRunning(session)));
                } else {
                    let engine = self.clone();
                    tokio::spawn(async move {
                        #[cfg(test)]
                        let hook = engine
                            .inner
                            .compaction_execution_hook
                            .lock()
                            .expect("compaction execution hook lock poisoned")
                            .take();
                        #[cfg(test)]
                        if let Some(hook) = hook {
                            if let Some(reached) = hook
                                .reached
                                .lock()
                                .expect("compaction execution reached lock poisoned")
                                .take()
                            {
                                let _ = reached.send(());
                            }
                            hook.release.notified().await;
                        }
                        let mut result = engine
                            .compact_session_direct(session, focus.as_deref())
                            .await;
                        if let Err(error) = engine.finish_compaction(session).await
                            && result.is_ok()
                        {
                            result = Err(error);
                        }
                        let _ = reply.send(result);
                    });
                }
            }
            SessionCommand::Revert { through_seq, reply } => {
                if !self.reserve_compaction(session) {
                    let _ = reply.send(Err(EngineError::SessionRunning(session)));
                } else {
                    let result = (|| {
                        let projection = self.inner.store.get(session)?;
                        if projection.status == SessionStatus::Running {
                            return Err(EngineError::SessionRunning(session));
                        }
                        let tip = projection
                            .log
                            .all_events()
                            .last()
                            .map_or(0, |event| event.seq);
                        if through_seq == 0 || through_seq > tip {
                            return Err(SessionError::InvalidSequence {
                                session_id: session,
                                through_seq,
                            }
                            .into());
                        }
                        self.append_direct(session, None, Event::SessionReverted { through_seq })?;
                        self.inner
                            .context_token_estimators
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .remove(&session);
                        self.rebuild_visible_tree_grants();
                        Ok(SessionRevertResult {
                            session: self.inner.store.get(session)?.metadata(),
                        })
                    })();
                    self.release_compaction_direct(session).await;
                    let _ = reply.send(result);
                }
            }
            SessionCommand::Fork { through_seq, reply } => {
                let result = self
                    .inner
                    .store
                    .fork(session, through_seq)
                    .map(|session_id| SessionForkResult { session_id });
                let _ = reply.send(result.map_err(EngineError::from));
            }
            SessionCommand::CompactionFinished { reply } => {
                self.release_compaction_direct(session).await;
                let _ = reply.send(Ok(()));
            }
            SessionCommand::Cancel { run, reply } => {
                let result = (|| {
                    let active = self
                        .inner
                        .active
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .get(&run)
                        .cloned()
                        .filter(|active| active.session == session)
                        .ok_or(EngineError::MissingRun(run))?;
                    active.cancellation.cancel();
                    active
                        .stdin
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .clear();
                    let events = self.inner.store.get(session)?.log.events();
                    let pending = approval_records(session, &events)
                        .into_values()
                        .filter(|record| {
                            matches!(
                                record.status,
                                ApprovalStatus::Pending | ApprovalStatus::Escalated
                            ) && approval_run_id(&events, record.request.approval_id()) == Some(run)
                        })
                        .map(|record| record.request.approval_id())
                        .collect::<Vec<_>>();
                    for approval_id in pending {
                        self.approval_terminal_direct(
                            session,
                            run,
                            approval_id,
                            ApprovalTerminal::Cancelled,
                        )?;
                    }
                    Ok(RunCancelResult { cancelled: true })
                })();
                let _ = reply.send(result);
            }
            SessionCommand::Stdin { params, reply } => {
                let result = (|| {
                    let active = self
                        .inner
                        .active
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .get(&params.run_id)
                        .cloned()
                        .filter(|active| active.session == session)
                        .ok_or(EngineError::MissingRun(params.run_id))?;
                    let data = params
                        .data
                        .map(|encoded| STANDARD.decode(encoded))
                        .transpose()?
                        .unwrap_or_default();
                    let sender = active
                        .stdin
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .get(&params.call_id)
                        .cloned()
                        .ok_or(EngineError::StdinUnavailable)?;
                    sender
                        .try_send(StdinWrite {
                            data: data.clone(),
                            eof: params.eof,
                        })
                        .map_err(|_| EngineError::StdinUnavailable)?;
                    if params.eof {
                        active
                            .stdin
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .remove(&params.call_id);
                    }
                    self.append_direct(
                        session,
                        Some(params.run_id),
                        Event::ToolStdinSubmitted {
                            tool_call_id: params.call_id,
                            byte_count: data.len() as u64,
                        },
                    )?;
                    Ok(RunToolStdinResult { accepted: true })
                })();
                let _ = reply.send(result);
            }
            SessionCommand::Subscribe { cursor, reply } => {
                // Snapshot and registration share the actor turn, so appends
                // cannot land in the cursor-to-live handoff gap.
                let result = self.inner.store.get(session).map(|projection| {
                    let events = projection
                        .log
                        .all_events()
                        .into_iter()
                        .filter(|event| cursor.is_none_or(|cursor| event.seq > cursor))
                        .collect();
                    let (sender, receiver) = mpsc::channel(PERSISTED_SUBSCRIBER_QUEUE_CAPACITY);
                    self.inner
                        .subscribers
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .entry(session)
                        .or_default()
                        .push(PersistedSubscriber { sender });
                    (EventsSubscribeResult { events }, receiver)
                });
                let _ = reply.send(result.map_err(EngineError::from));
            }
            SessionCommand::Resume { reply } => {
                let result = self
                    .resolve_interrupted_direct(session)
                    .await
                    .and_then(|()| Ok(self.inner.store.get(session)?.metadata()));
                let _ = reply.send(result);
            }
            SessionCommand::Rename { params, reply } => {
                let result = (|| {
                    let projection = self.inner.store.get(session)?;
                    if let Some(record) = projection.rename_records.get(&params.client_rename_id) {
                        if record.conflicts_with(&params) {
                            return Err(EngineError::RenameConflict);
                        }
                        return Ok(SessionRenameResult {
                            client_rename_id: params.client_rename_id,
                            session: projection.metadata(),
                        });
                    }
                    let commit = match params.change {
                        SessionRenameChange::Set { title } => SessionTitleChange::UserSet {
                            title,
                            client_rename_id: params.client_rename_id.clone(),
                        },
                        SessionRenameChange::Clear => SessionTitleChange::UserClear {
                            client_rename_id: params.client_rename_id.clone(),
                        },
                        SessionRenameChange::Reset => SessionTitleChange::UserReset {
                            client_rename_id: params.client_rename_id.clone(),
                        },
                    };
                    let input_through_seq = projection
                        .log
                        .all_events()
                        .last()
                        .map_or(0, |event| event.seq);
                    self.append_direct(
                        session,
                        None,
                        Event::SessionTitleCommitted {
                            input_through_seq,
                            change: commit,
                        },
                    )?;
                    Ok(SessionRenameResult {
                        client_rename_id: params.client_rename_id,
                        session: self.inner.store.get(session)?.metadata(),
                    })
                })();
                let _ = reply.send(result);
            }
            SessionCommand::ApprovalRespond { params, reply } => {
                let _ = reply.send(self.approval_respond_direct(params));
            }
            SessionCommand::ApprovalCapabilityInvalid {
                params,
                invalidation,
                reply,
            } => {
                let _ = reply.send(self.approval_capability_invalid_direct(params, invalidation));
            }
            SessionCommand::ApprovalEvaluationComplete {
                run,
                request,
                executor,
                decision,
                cancelled,
                reply,
            } => {
                let _ = reply.send(self.approval_evaluation_complete_direct(
                    session, run, request, executor, decision, cancelled,
                ));
            }
            SessionCommand::ApprovalTerminal {
                run,
                approval_id,
                terminal,
                reply,
            } => {
                let _ =
                    reply.send(self.approval_terminal_direct(session, run, approval_id, terminal));
            }
            SessionCommand::ToolResult {
                run,
                tool_call_id,
                result,
                reply,
            } => {
                let pending = self
                    .inner
                    .store
                    .get(session)
                    .ok()
                    .and_then(|projection| projection.runs.get(&run).cloned())
                    .is_some_and(|run| run.pending_calls.contains_key(&tool_call_id));
                let response = if !pending {
                    Ok(false)
                } else {
                    (|| {
                        let owner = self.tool_call_owner(session, run, tool_call_id)?;
                        let event = match result {
                            Ok(result) => Event::ToolCallTerminated {
                                termination: ToolCallTermination {
                                    tool_call_id,
                                    owner,
                                    outcome: ToolTerminationOutcome::Completed,
                                    result: Some(result),
                                    error: None,
                                },
                            },
                            Err(failure) => Event::ToolCallTerminated {
                                termination: ToolCallTermination {
                                    tool_call_id,
                                    owner,
                                    outcome: ToolTerminationOutcome::Failed,
                                    result: None,
                                    error: Some(SafeToolError {
                                        code: failure.code.safe_code(),
                                        message: safe_error(&failure.message),
                                    }),
                                },
                            },
                        };
                        self.append_direct(session, Some(run), event)?;
                        Ok(true)
                    })()
                };
                let _ = reply.send(response);
            }
            SessionCommand::ResolveDelegateFailureIfPending {
                run,
                tool_call_id,
                result,
                reply,
            } => {
                let result = self.resolve_delegate_failure_if_pending_direct(
                    session,
                    run,
                    tool_call_id,
                    result,
                );
                let _ = reply.send(result);
            }
            SessionCommand::ResolveAbandonedDelegateFailureIfPending {
                invocation_id,
                generation,
                run,
                tool_call_id,
                result,
                reply,
            } => {
                let result = self.resolve_abandoned_delegate_failure_if_pending_direct(
                    invocation_id,
                    generation,
                    session,
                    run,
                    tool_call_id,
                    result,
                );
                let _ = reply.send(result);
            }
            SessionCommand::PromotePendingOrComplete {
                run,
                final_text,
                complete_if_empty,
                reply,
            } => {
                let active = self
                    .inner
                    .active
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .get(&run)
                    .cloned()
                    .filter(|active| active.session == session)
                    .ok_or(EngineError::MissingRun(run));
                let active = match active {
                    Ok(active) => active,
                    Err(error) => {
                        let _ = reply.send(Err(error));
                        return;
                    }
                };
                let pending = match self.inner.store.get(session) {
                    Ok(projection) => pending_inputs(&projection.log.events(), run),
                    Err(error) => {
                        let _ = reply.send(Err(error.into()));
                        return;
                    }
                };
                if pending.is_empty() {
                    let result = if complete_if_empty {
                        self.append_direct(session, Some(run), Event::RunCompleted { final_text })
                    } else {
                        Ok(())
                    }
                    .map(|()| false);
                    let _ = reply.send(result);
                } else if !self.reserve_compaction(session) {
                    let _ = reply.send(Err(EngineError::SessionRunning(session)));
                } else {
                    let engine = self.clone();
                    tokio::spawn(async move {
                        let mut pending = pending;
                        let mut promoted = false;
                        let result = loop {
                            let fallback_index =
                                active.fallback_index.load(Ordering::Acquire) as usize;
                            let through_admission_seq = pending
                                .last()
                                .expect("pending batch is nonempty")
                                .admission_seq;
                            let mut failed = None;
                            let mut serialized_message_bytes = 0_usize;
                            for pending_input in &pending {
                                let input_bytes = match model_loop::serialized_input_bytes(
                                    &pending_input.input,
                                ) {
                                    Ok(input_bytes) => input_bytes,
                                    Err(error) => {
                                        failed = Some(error);
                                        break;
                                    }
                                };
                                serialized_message_bytes =
                                    serialized_message_bytes.saturating_add(input_bytes);
                                match engine
                                    .maybe_predictive_compact_before_input(
                                        PredictiveCompactionInput {
                                            session,
                                            run,
                                            serialized_message_bytes,
                                            policy: &active.policy,
                                            fallback_index,
                                            cancellation: &active.cancellation,
                                            actor_direct: false,
                                        },
                                    )
                                    .await
                                {
                                    Ok(true) => break,
                                    Ok(false) => {}
                                    Err(error) => {
                                        failed = Some(error);
                                        break;
                                    }
                                }
                            }
                            if let Some(error) = failed {
                                break Err(error);
                            }
                            match engine
                                .request(session, |reply| SessionCommand::CommitPendingPromotion {
                                    run,
                                    through_admission_seq,
                                    final_text: final_text.clone(),
                                    complete_if_empty,
                                    already_promoted: promoted,
                                    reply,
                                })
                                .await
                            {
                                Ok(state) if state.pending.is_empty() => {
                                    break Ok(state.continue_run);
                                }
                                Ok(state) => {
                                    promoted = state.promoted;
                                    pending = state.pending;
                                }
                                Err(error) => break Err(error),
                            }
                        };
                        let mut result = result;
                        if result.is_err()
                            && let Err(error) = engine.finish_compaction(session).await
                        {
                            result = Err(error);
                        }
                        let _ = reply.send(result);
                    });
                }
            }
            SessionCommand::PromptSnapshot { run, reply } => {
                let result = self
                    .inner
                    .active
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .get(&run)
                    .cloned()
                    .filter(|active| active.session == session)
                    .ok_or(EngineError::MissingRun(run))
                    .and_then(|_| {
                        let events = self.inner.store.get(session)?.log.events();
                        let applied: HashSet<u64> = events
                            .iter()
                            .filter_map(|event| match &event.payload {
                                Event::UserInputApplied { user_input_seq }
                                    if event.run_id == Some(run) =>
                                {
                                    Some(*user_input_seq)
                                }
                                _ => None,
                            })
                            .collect();
                        for user_input_seq in
                            events.iter().filter_map(|event| match &event.payload {
                                Event::UserInputSubmitted { .. }
                                    if event.run_id == Some(run)
                                        && !applied.contains(&event.seq) =>
                                {
                                    Some(event.seq)
                                }
                                _ => None,
                            })
                        {
                            self.append_direct(
                                session,
                                Some(run),
                                Event::UserInputApplied { user_input_seq },
                            )?;
                        }
                        let events = self.inner.store.get(session)?.log.events();
                        Ok(events)
                    });
                let _ = reply.send(result);
            }
        }
    }
}

fn pending_inputs(events: &[StoredEvent], run: RunId) -> Vec<PendingInput> {
    let mut pending = VecDeque::new();
    let mut initial_input_submitted = false;
    let run_start_seq = events.iter().find_map(|event| {
        (event.run_id == Some(run) && matches!(event.payload, Event::RunStarted { .. }))
            .then_some(event.seq)
    });
    let previous_terminal_seq = run_start_seq
        .and_then(|run_start_seq| {
            events
                .iter()
                .rev()
                .find(|event| {
                    event.seq < run_start_seq
                        && matches!(
                            event.payload,
                            Event::RunCompleted { .. }
                                | Event::RunFailed { .. }
                                | Event::RunCancelled { .. }
                                | Event::RunInterrupted { .. }
                        )
                })
                .map(|event| event.seq)
        })
        .unwrap_or(0);
    for event in events.iter().filter(|event| {
        event.run_id == Some(run)
            || (run_start_seq.is_some_and(|run_start_seq| event.seq < run_start_seq)
                && event.seq > previous_terminal_seq
                && event.run_id.is_none()
                && matches!(
                    event.payload,
                    Event::UserInputAdmitted { .. } | Event::UserInputRecalled { .. }
                ))
    }) {
        match &event.payload {
            Event::UserInputAdmitted { input } => pending.push_back(PendingInput {
                admission_seq: event.seq,
                input: input.clone(),
            }),
            Event::UserInputRecalled { .. } => {
                pending.pop_back();
            }
            Event::UserInputRecalledV2 { user_input_seq, .. } => {
                if let Some(position) = pending
                    .iter()
                    .position(|pending| pending.admission_seq == *user_input_seq)
                {
                    pending.remove(position);
                }
            }
            Event::UserInputSubmitted { .. } => {
                if initial_input_submitted {
                    pending.pop_front();
                } else {
                    initial_input_submitted = true;
                }
            }
            Event::RunCompleted { .. }
            | Event::RunFailed { .. }
            | Event::RunCancelled { .. }
            | Event::RunInterrupted { .. } => pending.clear(),
            _ => {}
        }
    }
    pending.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use cookie_agent_protocol::{RunId, SessionId, StoredEvent};

    use super::{Event, PendingInput, pending_inputs};

    fn event(run: RunId, seq: u64, payload: Event) -> StoredEvent {
        StoredEvent {
            event_schema_version: cookie_agent_protocol::EventSchemaVersion::current(),
            session_id: SessionId::new_v7(),
            run_id: Some(run),
            seq,
            timestamp: jiff::Timestamp::now(),
            payload,
        }
    }

    #[test]
    fn pending_lane_replays_fifo_promotion_lifo_recall_and_terminal_voiding() {
        let run = RunId::new_v7();
        let events = vec![
            event(
                run,
                1,
                Event::UserInputSubmitted {
                    input: "initial".into(),
                },
            ),
            event(
                run,
                2,
                Event::UserInputAdmitted {
                    input: "one".into(),
                },
            ),
            event(
                run,
                3,
                Event::UserInputAdmitted {
                    input: "two".into(),
                },
            ),
            event(
                run,
                4,
                Event::UserInputRecalled {
                    input: "two".into(),
                },
            ),
            event(
                run,
                5,
                Event::UserInputAdmitted {
                    input: "three".into(),
                },
            ),
            event(
                run,
                6,
                Event::UserInputSubmitted {
                    input: "one".into(),
                },
            ),
        ];
        assert_eq!(
            pending_inputs(&events, run),
            vec![PendingInput {
                admission_seq: 5,
                input: "three".into(),
            }]
        );
        let replayed = serde_json::from_slice::<Vec<StoredEvent>>(
            &serde_json::to_vec(&events).expect("serialize replay events"),
        )
        .expect("deserialize replay events");
        assert_eq!(pending_inputs(&replayed, run), pending_inputs(&events, run));

        let mut terminal = replayed;
        terminal.push(event(run, 7, Event::RunCancelled { reason: None }));
        assert!(pending_inputs(&terminal, run).is_empty());
    }

    #[test]
    fn targeted_recall_removes_only_its_admission_sequence() {
        let run = RunId::new_v7();
        let events = vec![
            event(
                run,
                1,
                Event::UserInputSubmitted {
                    input: "initial".into(),
                },
            ),
            event(
                run,
                2,
                Event::UserInputAdmitted {
                    input: "resume".into(),
                },
            ),
            event(
                run,
                3,
                Event::UserInputAdmitted {
                    input: "direct steer".into(),
                },
            ),
            event(
                run,
                4,
                Event::UserInputRecalledV2 {
                    user_input_seq: 2,
                    input: "resume".into(),
                },
            ),
        ];
        assert_eq!(
            pending_inputs(&events, run),
            vec![PendingInput {
                admission_seq: 3,
                input: "direct steer".into(),
            }]
        );
    }
}
