use std::{collections::HashSet, sync::Arc, time::Instant};

use cookie_agent_protocol::*;
use tokio::sync::oneshot;

use super::{Engine, EngineError, Event, SessionCommand, event_origin};
use crate::goal_projection::{GoalProducerProjection, ProducerMessageRecord};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ProducerAuthority {
    pub owner: ProducerOwner,
    pub connection_epoch: Option<u64>,
}

struct Registration {
    id: ProducerId,
    authority: ProducerAuthority,
    registered: Instant,
}

#[derive(Default)]
pub(super) struct SessionProducers {
    registrations: Vec<Registration>,
    wake_scheduled: bool,
    pub(super) starting: bool,
    pub(super) preempted: bool,
    diagnostics: HashSet<String>,
}

pub(super) enum ProducerCommand {
    GetGoal {
        reply: oneshot::Sender<Result<SessionGoalGetResult, EngineError>>,
    },
    SetGoal {
        objective: String,
        selection: Option<RunSelection>,
        origin: EventOrigin,
        reply: oneshot::Sender<Result<SessionGoalSetResult, EngineError>>,
    },
    Lifecycle {
        params: SessionGoalLifecycleParams,
        origin: EventOrigin,
        reply: oneshot::Sender<Result<SessionGoalLifecycleResult, EngineError>>,
    },
    UpdateGoal {
        params: GoalUpdateParams,
        reply: oneshot::Sender<Result<GoalUpdateResult, EngineError>>,
    },
    Inspect {
        reply: oneshot::Sender<Result<SessionProducersResult, EngineError>>,
    },
    Register {
        authority: ProducerAuthority,
        reply: oneshot::Sender<Result<ProducerId, EngineError>>,
    },
    Send {
        authority: ProducerAuthority,
        producer_id: ProducerId,
        mode: ProducerDeliveryMode,
        key: ProducerIdempotencyKey,
        body: String,
        reply: oneshot::Sender<Result<ProducerMessageId, EngineError>>,
    },
    CommitDelegationCompletion {
        reservation: DelegationReservation,
        producer_id: Option<ProducerId>,
        teaser: super::delegation::DelegateTeaser,
        reply: oneshot::Sender<Result<bool, EngineError>>,
    },
    Unregister {
        authority: ProducerAuthority,
        producer_id: ProducerId,
        reply: oneshot::Sender<Result<(), EngineError>>,
    },
    Discard {
        authority: ProducerAuthority,
        message_id: ProducerMessageId,
        reply: oneshot::Sender<Result<(), EngineError>>,
    },
    ClaimInputs {
        run: RunId,
        reply: oneshot::Sender<Result<super::producer_claims::ClaimedPrompt, EngineError>>,
    },
    ReleaseClaim {
        run: RunId,
        claim_seq: u64,
        reply: oneshot::Sender<Result<(), EngineError>>,
    },
    Reconcile {
        reply: oneshot::Sender<Result<(), EngineError>>,
    },
    Wake {
        reply: oneshot::Sender<Result<(), EngineError>>,
    },
    CommitStart {
        run: RunId,
        event: Box<Event>,
        reply: oneshot::Sender<Result<(), EngineError>>,
    },
    WakeFinished {
        successful: bool,
        reply: oneshot::Sender<Result<(), EngineError>>,
    },
}

impl Engine {
    pub(super) fn install_producer_runtime(&self) {
        let weak = Arc::downgrade(&self.inner);
        self.inner
            .plugins
            .set_producer_handler(Arc::new(move |authority, request| {
                let weak = weak.clone();
                Box::pin(async move {
                    let inner = weak
                        .upgrade()
                        .ok_or_else(|| producer_fault(EngineError::ActorStopped))?;
                    Engine { inner }
                        .dispatch_plugin_producer(authority, request)
                        .await
                })
            }));
        let Some(runtime) = self.inner.runtime.clone() else {
            return;
        };
        let weak = Arc::downgrade(&self.inner);
        let mut changes = self.inner.plugins.subscribe_producer_changes();
        self.spawn_admission_task(&runtime, async move {
            loop {
                if let Some(inner) = weak.upgrade() {
                    Engine { inner }.reconcile_plugin_producer_sessions().await;
                } else {
                    return;
                }
                if changes.changed().await.is_err() {
                    return;
                }
            }
        });
    }

    async fn dispatch_plugin_producer(
        &self,
        authenticated: crate::plugin::PluginConnectionAuthority,
        request: crate::plugin::PluginProducerRequest,
    ) -> Result<crate::plugin::PluginProducerResponse, JsonRpcError> {
        use crate::plugin::{PluginProducerRequest as Request, PluginProducerResponse as Response};
        let authority = ProducerAuthority {
            owner: ProducerOwner::Plugin {
                plugin: authenticated.plugin.clone(),
            },
            connection_epoch: Some(authenticated.connection_epoch),
        };
        self.validate_producer_authority(&authority)
            .map_err(producer_fault)?;
        match request {
            Request::Register(params) => self
                .register_producer(params.session_id, authority)
                .await
                .map(|producer_id| {
                    Response::Register(ExtensionProducerRegisterResult { producer_id })
                })
                .map_err(producer_fault),
            Request::Send(params) => self
                .send_producer_message(
                    params.session_id,
                    authority,
                    params.producer_id,
                    params.mode,
                    params.idempotency_key,
                    params.body,
                )
                .await
                .map(|message_id| Response::Send(ExtensionProducerSendResult { message_id }))
                .map_err(producer_fault),
            Request::Unregister(params) => self
                .unregister_producer(params.session_id, authority, params.producer_id)
                .await
                .map(|()| Response::Unregister(ExtensionProducerUnregisterResult {}))
                .map_err(producer_fault),
            Request::Discard(params) => self
                .discard_producer_message(params.session_id, authority, params.message_id)
                .await
                .map(|()| Response::Discard(ExtensionProducerDiscardResult {}))
                .map_err(producer_fault),
            Request::RecoveryComplete(params) => {
                self.inner
                    .plugins
                    .complete_producer_recovery(&authenticated, &params.outcome)?;
                self.reconcile_plugin_producer_sessions().await;
                Ok(Response::RecoveryComplete(
                    ExtensionRecoveryCompleteResult {},
                ))
            }
        }
    }

    async fn reconcile_plugin_producer_sessions(&self) {
        let mut sessions: HashSet<_> = self
            .inner
            .producers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .copied()
            .collect();
        for summary in self.inner.store.all_summaries() {
            let session = summary.meta.session_id;
            if self
                .inner
                .store
                .recovery_event_snapshot(session)
                .map(|events| GoalProducerProjection::from_events(&events))
                .is_ok_and(|projection| {
                    projection.goal.is_some()
                        || projection.messages.iter().any(|message| {
                            pending(message) || (message.consumed && !message.consumption_recorded)
                        })
                })
            {
                sessions.insert(session);
            }
        }
        for session in sessions {
            let _ = self.reconcile_producers(session).await;
        }
    }
    pub(super) async fn handle_producer_command(
        &self,
        session: SessionId,
        command: ProducerCommand,
    ) {
        match command {
            ProducerCommand::GetGoal { reply } => {
                let _ = reply.send(self.require_root_goal(session).and_then(|()| {
                    self.goal_producer_projection(session)
                        .map(|projection| SessionGoalGetResult {
                            goal: projection.goal,
                        })
                }));
            }
            ProducerCommand::SetGoal {
                objective,
                selection,
                origin,
                reply,
            } => {
                let _ = reply.send(self.set_goal_direct(session, objective, selection, origin));
            }
            ProducerCommand::Lifecycle {
                params,
                origin,
                reply,
            } => {
                let _ = reply.send(self.lifecycle_direct(session, params, origin));
            }
            ProducerCommand::UpdateGoal { params, reply } => {
                let _ = reply.send(self.update_goal_direct(session, params));
            }
            ProducerCommand::Inspect { reply } => {
                let producers = self
                    .inner
                    .producers
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .entry(session)
                    .or_default()
                    .registrations
                    .iter()
                    .map(|record| ProducerRegistration {
                        producer_id: record.id,
                        producer_owner: record.authority.owner.clone(),
                        session_id: session,
                        age_ms: record
                            .registered
                            .elapsed()
                            .as_millis()
                            .min(u128::from(u64::MAX)) as u64,
                    })
                    .collect();
                let _ = reply.send(Ok(SessionProducersResult {
                    producers,
                    plugin_recovery: self.inner.plugins.producer_recovery_states(),
                }));
            }
            ProducerCommand::Register { authority, reply } => {
                let _ = reply.send(self.register_producer_direct(session, authority));
            }
            ProducerCommand::Send {
                authority,
                producer_id,
                mode,
                key,
                body,
                reply,
            } => {
                let _ = reply.send(self.accept_producer_direct(
                    &authority,
                    ExtensionProducerSendParams {
                        session_id: session,
                        producer_id,
                        mode,
                        idempotency_key: key,
                        body,
                    },
                    None,
                ));
            }
            ProducerCommand::CommitDelegationCompletion {
                reservation,
                producer_id,
                teaser,
                reply,
            } => {
                let _ = reply.send(self.commit_delegation_completion_direct(
                    session,
                    &reservation,
                    producer_id,
                    teaser,
                ));
            }
            ProducerCommand::Unregister {
                authority,
                producer_id,
                reply,
            } => {
                let result = self
                    .require_registration(session, producer_id, &authority)
                    .map(|()| {
                        self.inner
                            .producers
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .entry(session)
                            .or_default()
                            .registrations
                            .retain(|record| record.id != producer_id);
                    });
                let _ = reply.send(result);
            }
            ProducerCommand::Discard {
                authority,
                message_id,
                reply,
            } => {
                let _ = reply
                    .send(self.discard_producer_message_direct(session, &authority, message_id));
            }
            ProducerCommand::ClaimInputs { run, reply } => {
                let _ = reply.send(self.claim_producer_snapshot_direct(session, run));
            }
            ProducerCommand::ReleaseClaim {
                run,
                claim_seq,
                reply,
            } => {
                let _ =
                    reply.send(self.release_producer_claim_direct(session, run, claim_seq, false));
            }
            ProducerCommand::Reconcile { reply } => {
                let _ = reply.send(self.reconcile_producers_direct(session));
            }
            ProducerCommand::CommitStart { run, event, reply } => {
                let _ = reply.send(self.commit_producer_start(session, run, *event));
            }
            ProducerCommand::Wake { reply } => {
                let result = self.begin_producer_wake(session);
                let _ = reply.send(result);
            }
            ProducerCommand::WakeFinished { successful, reply } => {
                let preempted = {
                    let mut registry = self
                        .inner
                        .producers
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    let state = registry.entry(session).or_default();
                    let preempted = state.preempted;
                    state.starting = false;
                    state.wake_scheduled = false;
                    state.preempted = false;
                    preempted
                };
                let _ = reply.send(if successful || preempted {
                    self.reconcile_producers_direct(session)
                } else {
                    Ok(())
                });
            }
        }
    }

    pub(super) fn reconcile_producers_direct(&self, session: SessionId) -> Result<(), EngineError> {
        if self.ensure_not_shutting_down().is_err() || !self.inner.store.is_owned(session) {
            return Ok(());
        }
        self.reconcile_consumed_producers(session, false)?;
        self.repair_goal_completion(session, false)?;
        {
            let mut registry = self
                .inner
                .producers
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            registry
                .entry(session)
                .or_default()
                .registrations
                .retain(|record| match &record.authority.owner {
                    ProducerOwner::Plugin { plugin } => {
                        record.authority.connection_epoch.is_some_and(|epoch| {
                            self.inner
                                .plugins
                                .producer_connection_is_current(plugin, &epoch)
                        })
                    }
                    _ => true,
                });
        }
        self.reconcile_goal_registration(session)?;
        self.record_recovery_diagnostics(session)?;
        if self
            .inner
            .active
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .any(|run| run.session == session)
            || self.inner.store.get(session)?.status == SessionStatus::Running
            || self
                .inner
                .compaction_in_progress
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains(&session)
        {
            return Ok(());
        }
        let projection = self.goal_producer_projection(session)?;
        let real = projection
            .messages
            .iter()
            .any(|message| pending(message) && message.reminder.is_none());
        let user_pending = super::residency::has_runless_pending_inputs(
            &self.inner.store.get(session)?.log.event_snapshot(),
        );
        if !real
            && !user_pending
            && let Some(goal) = projection.goal.as_ref().filter(|goal| {
                goal.status == GoalStatus::Active
                    && (goal.items.is_empty() || goal.items.iter().any(|item| !item.finished))
            })
            && let Some(producer_id) = self.ready_goal_producer(session, goal.goal_id)
            && !projection
                .messages
                .iter()
                .any(|message| pending(message) && message.reminder.is_some())
        {
            let body = format!(
                "Continue the root goal. Verify evidence directly or through subagents before marking items finished.\n{}\n{}",
                if goal.items.is_empty() {
                    "Establish the checklist with goal_update before continuing."
                } else {
                    "Continue unfinished checklist work; unchanged revision is not completion."
                },
                serde_json::to_string(goal).expect("goal state serializes")
            );
            let authority = ProducerAuthority {
                owner: ProducerOwner::Goal {
                    goal_id: goal.goal_id,
                },
                connection_epoch: None,
            };
            self.accept_producer_direct(
                &authority,
                ExtensionProducerSendParams {
                    session_id: session,
                    producer_id,
                    mode: ProducerDeliveryMode::Queue,
                    idempotency_key: ProducerIdempotencyKey::new(
                        ProducerMessageId::new_v7().to_string(),
                    )
                    .expect("UUID idempotency key"),
                    body,
                },
                Some(GoalReminderIdentity {
                    goal_id: goal.goal_id,
                    revision: goal.revision,
                }),
            )?;
        }
        let projection = self.goal_producer_projection(session)?;
        if !projection.messages.iter().any(pending) {
            return Ok(());
        }
        if !real && user_pending {
            return Ok(());
        }
        if !real
            && projection
                .goal
                .as_ref()
                .is_none_or(|goal| self.ready_goal_producer(session, goal.goal_id).is_none())
        {
            return Ok(());
        }
        let runtime = tokio::runtime::Handle::try_current()
            .ok()
            .or_else(|| self.inner.runtime.clone());
        let Some(runtime) = runtime else {
            return Ok(());
        };
        {
            let mut registry = self
                .inner
                .producers
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let state = registry.entry(session).or_default();
            if state.wake_scheduled {
                return Ok(());
            }
            state.wake_scheduled = true;
        }
        let engine = self.clone();
        if !self.spawn_admission_task(&runtime, async move {
            let _ = engine
                .request(session, |reply| {
                    SessionCommand::Producer(ProducerCommand::Wake { reply })
                })
                .await;
        }) {
            self.inner
                .producers
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .entry(session)
                .or_default()
                .wake_scheduled = false;
        }
        Ok(())
    }

    fn ready_goal_producer(&self, session: SessionId, goal_id: GoalId) -> Option<ProducerId> {
        if !self.plugin_goals_ready() {
            return None;
        }
        if self
            .inner
            .delegations_by_session
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .any(|record| {
                record.parent_session_id == session
                    && record.background
                    && !record.notification_sent
            })
        {
            return None;
        }
        let registry = self
            .inner
            .producers
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let state = registry.get(&session)?;
        let own = state
            .registrations
            .iter()
            .find(|record| record.authority.owner == ProducerOwner::Goal { goal_id })?;
        state
            .registrations
            .iter()
            .all(|record| record.id == own.id)
            .then_some(own.id)
    }

    fn begin_producer_wake(&self, session: SessionId) -> Result<(), EngineError> {
        let projection = self.inner.store.get(session)?;
        if projection.status == SessionStatus::Running
            || self
                .inner
                .active
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .values()
                .any(|active| active.session == session)
            || !self.reserve_compaction(session)
        {
            self.inner
                .producers
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .entry(session)
                .or_default()
                .wake_scheduled = false;
            return Ok(());
        }
        let events = projection.log.event_snapshot();
        let goal = GoalProducerProjection::from_events(&events);
        let goal_selection = goal
            .goal
            .as_ref()
            .filter(|goal| matches!(goal.status, GoalStatus::Active | GoalStatus::Paused))
            .and(goal.selection);
        let selection = goal_selection.unwrap_or_else(|| {
            events
                .iter()
                .rev()
                .find_map(|event| match &event.payload {
                    Event::RunStarted { selection, .. } => Some(selection.clone()),
                    _ => None,
                })
                .unwrap_or_else(|| projection.meta.creation_selection.clone())
        });
        {
            let mut registry = self
                .inner
                .producers
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let state = registry.entry(session).or_default();
            state.starting = true;
            state.preempted = false;
        }
        let engine = self.clone();
        tokio::spawn(async move {
            let params = RunStartParams {
                session_id: session,
                client_run_id: ClientRunId::new(format!(
                    "producer:{}",
                    ProducerMessageId::new_v7()
                ))
                .expect("producer run ID"),
                selection,
                input: String::new(),
            };
            let result = engine
                .start_run_direct(params, event_origin("engine:producer"), None, true)
                .await;
            let _ = engine.finish_compaction(session).await;
            let successful = result.is_ok();
            if let Err(error) = result
                && !matches!(error, EngineError::Producer(_))
            {
                eprintln!("session {session} producer wake failed: {error}");
            }
            let _ = engine
                .request(session, |reply| {
                    SessionCommand::Producer(ProducerCommand::WakeFinished { successful, reply })
                })
                .await;
        });
        Ok(())
    }

    fn commit_producer_start(
        &self,
        session: SessionId,
        run: RunId,
        event: Event,
    ) -> Result<(), EngineError> {
        self.reconcile_goal_registration(session)?;
        if self
            .inner
            .producers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&session)
            .is_some_and(|state| state.preempted)
        {
            return Err(EngineError::Producer(
                "session input or goal changed before automatic admission".into(),
            ));
        }
        let projection = self.goal_producer_projection(session)?;
        let real = projection
            .messages
            .iter()
            .any(|message| pending(message) && message.reminder.is_none());
        let user_pending = super::residency::has_runless_pending_inputs(
            &self.inner.store.get(session)?.log.event_snapshot(),
        );
        if !real {
            let valid = projection.goal.as_ref().is_some_and(|goal| {
                goal.status == GoalStatus::Active
                    && self.ready_goal_producer(session, goal.goal_id).is_some()
                    && !user_pending
            });
            if !valid {
                return Err(EngineError::Producer(
                    "goal reminder is no longer ready".into(),
                ));
            }
        }
        let messages: Vec<_> = projection
            .messages
            .iter()
            .filter(|message| pending(message) && (!real || message.reminder.is_none()))
            .collect();
        if messages.is_empty() {
            return Err(EngineError::Producer("no pending producer input".into()));
        }
        self.inner.store.persist_buffered_session(session)?;
        self.append_direct(session, Some(run), event_origin("engine:producer"), event)?;
        for message in messages {
            self.append_direct(
                session,
                Some(run),
                event_origin("engine:producer"),
                Event::ProducerMessageAdmitted {
                    message_id: message.message_id,
                },
            )?;
        }
        Ok(())
    }

    fn record_recovery_diagnostics(&self, session: SessionId) -> Result<(), EngineError> {
        for state in self.inner.plugins.producer_recovery_states() {
            let kind = match state.status {
                PluginRecoveryStatus::Failed => PluginDiagnosticKind::RecoveryFailed,
                PluginRecoveryStatus::Disabled => PluginDiagnosticKind::RecoveryDisabled,
                _ => continue,
            };
            let key = format!("{}:{:?}", state.plugin, state.status);
            if self
                .inner
                .producers
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .entry(session)
                .or_default()
                .diagnostics
                .contains(&key)
            {
                continue;
            }
            self.append_direct(session, None, event_origin("engine:plugin-host"), Event::PluginDiagnostic { plugin: state.plugin, kind, message: "Producer recovery is unavailable; external work is unknown and goal continuation is held.".into(), count: 1 })?;
            self.inner
                .producers
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .entry(session)
                .or_default()
                .diagnostics
                .insert(key);
        }
        Ok(())
    }
    pub async fn get_session_goal(
        &self,
        params: SessionGoalGetParams,
    ) -> Result<SessionGoalGetResult, EngineError> {
        self.request(params.session_id, |reply| {
            SessionCommand::Producer(ProducerCommand::GetGoal { reply })
        })
        .await
    }

    pub async fn set_session_goal(
        &self,
        params: SessionGoalSetParams,
        origin: EventOrigin,
    ) -> Result<SessionGoalSetResult, EngineError> {
        self.request(params.session_id, |reply| {
            SessionCommand::Producer(ProducerCommand::SetGoal {
                objective: params.objective,
                selection: params.selection,
                origin,
                reply,
            })
        })
        .await
    }

    pub async fn change_session_goal_lifecycle(
        &self,
        params: SessionGoalLifecycleParams,
        origin: EventOrigin,
    ) -> Result<SessionGoalLifecycleResult, EngineError> {
        self.request(params.session_id, |reply| {
            SessionCommand::Producer(ProducerCommand::Lifecycle {
                params,
                origin,
                reply,
            })
        })
        .await
    }

    pub async fn session_producers(
        &self,
        params: SessionProducersParams,
    ) -> Result<SessionProducersResult, EngineError> {
        self.request(params.session_id, |reply| {
            SessionCommand::Producer(ProducerCommand::Inspect { reply })
        })
        .await
    }

    pub async fn goal_get(&self, session: SessionId) -> Result<GoalGetResult, EngineError> {
        let result = self
            .get_session_goal(SessionGoalGetParams {
                session_id: session,
            })
            .await?;
        Ok(GoalGetResult { goal: result.goal })
    }

    pub async fn goal_update(
        &self,
        session: SessionId,
        params: GoalUpdateParams,
    ) -> Result<GoalUpdateResult, EngineError> {
        self.request(session, |reply| {
            SessionCommand::Producer(ProducerCommand::UpdateGoal { params, reply })
        })
        .await
    }

    pub(crate) async fn register_producer(
        &self,
        session: SessionId,
        authority: ProducerAuthority,
    ) -> Result<ProducerId, EngineError> {
        self.request(session, |reply| {
            SessionCommand::Producer(ProducerCommand::Register { authority, reply })
        })
        .await
    }

    pub(crate) async fn send_producer_message(
        &self,
        session: SessionId,
        authority: ProducerAuthority,
        producer_id: ProducerId,
        mode: ProducerDeliveryMode,
        key: ProducerIdempotencyKey,
        body: String,
    ) -> Result<ProducerMessageId, EngineError> {
        self.request(session, |reply| {
            SessionCommand::Producer(ProducerCommand::Send {
                authority,
                producer_id,
                mode,
                key,
                body,
                reply,
            })
        })
        .await
    }

    pub(crate) async fn unregister_producer(
        &self,
        session: SessionId,
        authority: ProducerAuthority,
        producer_id: ProducerId,
    ) -> Result<(), EngineError> {
        self.request(session, |reply| {
            SessionCommand::Producer(ProducerCommand::Unregister {
                authority,
                producer_id,
                reply,
            })
        })
        .await
    }

    pub(crate) fn goal_producer_projection(
        &self,
        session: SessionId,
    ) -> Result<GoalProducerProjection, EngineError> {
        Ok(GoalProducerProjection::from_events(
            &self.inner.store.get(session)?.log.event_snapshot(),
        ))
    }

    fn require_root_goal(&self, session: SessionId) -> Result<(), EngineError> {
        if !matches!(
            self.inner.store.get(session)?.meta.origin,
            SessionOrigin::Root
        ) {
            return Err(EngineError::Goal(
                "goals are available only in root sessions".into(),
            ));
        }
        Ok(())
    }

    fn set_goal_direct(
        &self,
        session: SessionId,
        objective: String,
        selection: Option<RunSelection>,
        origin: EventOrigin,
    ) -> Result<SessionGoalSetResult, EngineError> {
        self.require_root_goal(session)?;
        if objective.trim().is_empty() {
            return Err(EngineError::Goal("objective must not be blank".into()));
        }
        if self
            .goal_producer_projection(session)?
            .goal
            .is_some_and(|goal| matches!(goal.status, GoalStatus::Active | GoalStatus::Paused))
        {
            return Err(EngineError::Goal(
                "cancel or complete the current goal before setting another".into(),
            ));
        }
        if let Some(selection) = &selection {
            self.freeze_root_selection(selection)?;
        }
        let goal_id = GoalId::new_v7();
        self.append_direct(
            session,
            None,
            origin,
            Event::GoalActivated {
                goal_id,
                objective,
                revision: 0,
                selection,
            },
        )?;
        self.invalidate_pending_goal_admission(session);
        Ok(SessionGoalSetResult {
            goal: self
                .goal_producer_projection(session)?
                .goal
                .expect("validated activation"),
        })
    }

    fn checked_goal(
        &self,
        session: SessionId,
        goal_id: GoalId,
        expected_revision: u64,
    ) -> Result<GoalState, EngineError> {
        self.require_root_goal(session)?;
        let goal = self
            .goal_producer_projection(session)?
            .goal
            .ok_or_else(|| EngineError::Goal("no goal is set".into()))?;
        if goal.goal_id != goal_id || goal.revision != expected_revision {
            return Err(EngineError::Goal(format!(
                "stale goal: current goal_id={}, revision={}",
                goal.goal_id, goal.revision
            )));
        }
        if !matches!(goal.status, GoalStatus::Active | GoalStatus::Paused) {
            return Err(EngineError::Goal("terminal goals cannot be changed".into()));
        }
        Ok(goal)
    }

    fn lifecycle_direct(
        &self,
        session: SessionId,
        params: SessionGoalLifecycleParams,
        origin: EventOrigin,
    ) -> Result<SessionGoalLifecycleResult, EngineError> {
        let goal = self.checked_goal(session, params.goal_id, params.expected_revision)?;
        if params.selection.is_some() && params.action != GoalLifecycleAction::Resume {
            return Err(EngineError::Goal(
                "selection is only allowed when resuming a goal".into(),
            ));
        }
        if let Some(selection) = &params.selection {
            self.freeze_root_selection(selection)?;
        }
        let status = match (goal.status, params.action) {
            (GoalStatus::Active, GoalLifecycleAction::Pause) => GoalStatus::Paused,
            (GoalStatus::Paused, GoalLifecycleAction::Resume) => GoalStatus::Active,
            (_, GoalLifecycleAction::Cancel) => GoalStatus::Cancelled,
            _ => return Err(EngineError::Goal("invalid lifecycle transition".into())),
        };
        let revision = next_revision(goal.revision)?;
        let notify_root = matches!(status, GoalStatus::Paused | GoalStatus::Cancelled)
            && self.has_goal_control_audience(session, goal.goal_id)?;
        self.append_direct(
            session,
            None,
            origin,
            Event::GoalLifecycleChanged {
                goal_id: goal.goal_id,
                status,
                revision,
                selection: params.selection,
            },
        )?;
        self.invalidate_pending_goal_admission(session);
        if notify_root {
            self.accept_goal_control_direct(session, &goal, status, revision)?;
        }
        self.reconcile_goal_registration(session)?;
        Ok(SessionGoalLifecycleResult {
            goal: self
                .goal_producer_projection(session)?
                .goal
                .expect("validated lifecycle"),
        })
    }

    fn invalidate_pending_goal_admission(&self, session: SessionId) {
        if let Some(state) = self
            .inner
            .producers
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get_mut(&session)
            && state.starting
        {
            state.preempted = true;
        }
    }

    fn has_goal_control_audience(
        &self,
        session: SessionId,
        goal_id: GoalId,
    ) -> Result<bool, EngineError> {
        let projection = self.inner.store.get(session)?;
        if projection.status == SessionStatus::Running {
            return Ok(true);
        }
        {
            let registry = self
                .inner
                .producers
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if let Some(state) = registry.get(&session) {
                let own = state
                    .registrations
                    .iter()
                    .find(|record| record.authority.owner == ProducerOwner::Goal { goal_id })
                    .map(|record| record.id);
                if state.registrations.iter().any(|record| {
                    Some(record.id) != own
                        && self.validate_producer_authority(&record.authority).is_ok()
                }) {
                    return Ok(true);
                }
            }
        }
        let events = projection.log.event_snapshot();
        Ok(super::residency::has_runless_pending_inputs(&events)
            || GoalProducerProjection::from_events(&events)
                .messages
                .iter()
                .any(|message| pending(message) && message.reminder.is_none()))
    }

    fn accept_goal_control_direct(
        &self,
        session: SessionId,
        goal: &GoalState,
        status: GoalStatus,
        revision: u64,
    ) -> Result<(), EngineError> {
        let objective = serde_json::to_string(&goal.objective).expect("goal objective serializes");
        let body = match status {
            GoalStatus::Paused => format!(
                "[engine goal control]\nThe goal {objective} was paused by the user. Stop pursuing it autonomously. Wrap up current work and report status, or follow new user directions."
            ),
            GoalStatus::Cancelled => format!(
                "[engine goal control]\nThe goal {objective} was cancelled by the user. Stop pursuing this cancelled goal and do not call goal_update to maintain it. Follow new user directions."
            ),
            _ => return Ok(()),
        };
        let authority = ProducerAuthority {
            owner: ProducerOwner::GoalControl {
                goal_id: goal.goal_id,
            },
            connection_epoch: None,
        };
        let producer_id = self.register_producer_direct(session, authority.clone())?;
        let accepted = self.accept_producer_direct(
            &authority,
            ExtensionProducerSendParams {
                session_id: session,
                producer_id,
                mode: ProducerDeliveryMode::Steer,
                idempotency_key: ProducerIdempotencyKey::new(format!("goal-control:{revision}"))
                    .expect("goal control idempotency key"),
                body,
            },
            None,
        );
        // This registration exists only during the serialized control operation.
        // Its durable message remains pending even if the run ends before promotion.
        self.inner
            .producers
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .entry(session)
            .or_default()
            .registrations
            .retain(|record| record.id != producer_id);
        accepted.map(|_| ())
    }

    fn update_goal_direct(
        &self,
        session: SessionId,
        params: GoalUpdateParams,
    ) -> Result<GoalUpdateResult, EngineError> {
        self.require_root_goal(session)?;
        let goal = self
            .goal_producer_projection(session)?
            .goal
            .ok_or_else(|| EngineError::Goal("no goal is set".into()))?;
        if !matches!(goal.status, GoalStatus::Active | GoalStatus::Paused) {
            return Err(EngineError::Goal("terminal goals cannot be changed".into()));
        }
        if params
            .items
            .iter()
            .any(|item| item.description.trim().is_empty())
        {
            return Err(EngineError::Goal(
                "items require nonblank descriptions".into(),
            ));
        }
        let revision = next_revision(goal.revision)?;
        let completed = !params.items.is_empty() && params.items.iter().all(|item| item.finished);
        let completion_revision = completed.then(|| next_revision(revision)).transpose()?;
        self.append_direct(
            session,
            None,
            event_origin("engine:goal"),
            Event::GoalChecklistRevised {
                goal_id: goal.goal_id,
                items: params.items,
                revision,
            },
        )?;
        if let Some(revision) = completion_revision {
            self.append_direct(
                session,
                None,
                event_origin("engine:goal"),
                Event::GoalLifecycleChanged {
                    goal_id: goal.goal_id,
                    status: GoalStatus::Completed,
                    revision,
                    selection: None,
                },
            )?;
        }
        self.invalidate_pending_goal_admission(session);
        self.reconcile_goal_registration(session)?;
        Ok(GoalUpdateResult {
            goal: self
                .goal_producer_projection(session)?
                .goal
                .expect("validated checklist"),
        })
    }

    fn register_producer_direct(
        &self,
        session: SessionId,
        authority: ProducerAuthority,
    ) -> Result<ProducerId, EngineError> {
        self.ensure_not_shutting_down()?;
        self.validate_producer_authority(&authority)?;
        let id = ProducerId::new_v7();
        self.inner
            .producers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(session)
            .or_default()
            .registrations
            .push(Registration {
                id,
                authority,
                registered: Instant::now(),
            });
        Ok(id)
    }

    fn validate_producer_authority(
        &self,
        authority: &ProducerAuthority,
    ) -> Result<(), EngineError> {
        if let ProducerOwner::Plugin { plugin } = &authority.owner {
            let Some(connection_epoch) = authority.connection_epoch else {
                return Err(EngineError::Producer(
                    "missing plugin connection epoch".into(),
                ));
            };
            if !self
                .inner
                .plugins
                .producer_connection_is_current(plugin, &connection_epoch)
            {
                return Err(EngineError::Producer(
                    "plugin connection is no longer live".into(),
                ));
            }
        }
        Ok(())
    }

    fn require_registration(
        &self,
        session: SessionId,
        id: ProducerId,
        authority: &ProducerAuthority,
    ) -> Result<(), EngineError> {
        self.ensure_not_shutting_down()?;
        self.validate_producer_authority(authority)?;
        if !self
            .inner
            .producers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&session)
            .is_some_and(|state| {
                state.registrations.iter().any(|registration| {
                    registration.id == id && &registration.authority == authority
                })
            })
        {
            return Err(EngineError::Producer(
                "closed, foreign, or wrong-session registration".into(),
            ));
        }
        Ok(())
    }

    fn accept_producer_direct(
        &self,
        authority: &ProducerAuthority,
        params: ExtensionProducerSendParams,
        reminder: Option<GoalReminderIdentity>,
    ) -> Result<ProducerMessageId, EngineError> {
        let ExtensionProducerSendParams {
            session_id: session,
            producer_id,
            mode,
            idempotency_key: key,
            body,
        } = params;
        self.require_registration(session, producer_id, authority)?;
        if let Some(existing) = self
            .goal_producer_projection(session)?
            .messages
            .iter()
            .find(|message| {
                message.producer_owner == authority.owner && message.idempotency_key == key
            })
        {
            if existing.mode != mode || existing.body != body || existing.reminder != reminder {
                return Err(EngineError::Producer(
                    "idempotency key already accepted with different payload".into(),
                ));
            }
            self.inner.store.persist_buffered_session(session)?;
            return Ok(existing.message_id);
        }
        let message_id = ProducerMessageId::new_v7();
        self.append_direct(
            session,
            None,
            event_origin("engine:producer"),
            Event::ProducerMessageAccepted {
                message_id,
                producer_owner: authority.owner.clone(),
                mode,
                idempotency_key: key,
                body,
                reminder,
            },
        )?;
        self.inner.store.persist_buffered_session(session)?;
        Ok(message_id)
    }

    fn commit_delegation_completion_direct(
        &self,
        session: SessionId,
        reservation: &DelegationReservation,
        producer_id: Option<ProducerId>,
        teaser: super::delegation::DelegateTeaser,
    ) -> Result<bool, EngineError> {
        if teaser.session_id != reservation.child_session_id {
            return Err(EngineError::Producer(
                "completion child does not match reservation".into(),
            ));
        }
        let body = super::delegation::render_background_completion(&teaser);
        let super::delegation::DelegateTeaser {
            status,
            preview,
            total_lines,
            ..
        } = teaser;
        let parent = self.inner.store.get(session)?;
        let visible = parent.log.event_snapshot().iter().any(|event| {
            !parent.log.delegation_event_tainted(event)
                && matches!(
                    &event.payload,
                    Event::DelegationReserved { reservation: candidate, .. }
                        if event.run_id == Some(reservation.parent_run_id)
                            && candidate == reservation
                            && reservation.parent_session_id == session
                )
        });
        if !visible {
            let authority = ProducerOwner::Delegation {
                invocation_id: reservation.invocation_id,
            };
            if let (Some(producer_id), Some(state)) = (
                producer_id,
                self.inner
                    .producers
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .get_mut(&session),
            ) {
                state.registrations.retain(|record| {
                    record.id != producer_id || record.authority.owner != authority
                });
            }
            return Ok(false);
        }

        let authority = ProducerAuthority {
            owner: ProducerOwner::Delegation {
                invocation_id: reservation.invocation_id,
            },
            connection_epoch: None,
        };
        let events = self.inner.store.get(session)?.log.event_snapshot();
        let already_logged = events.iter().any(|event| {
            matches!(
                event.payload,
                Event::DelegateFinishedV2 { invocation_id, session_id, .. }
                    if invocation_id == reservation.invocation_id
                        && session_id == reservation.child_session_id
            )
        });
        let already_accepted = events.iter().any(|event| {
            matches!(
                &event.payload,
                Event::ProducerMessageAccepted {
                    producer_owner: ProducerOwner::Delegation { invocation_id },
                    ..
                } if *invocation_id == reservation.invocation_id
            )
        });
        if !already_accepted && !already_logged {
            let producer_id = producer_id.ok_or_else(|| {
                EngineError::Producer("delegation completion registration is missing".into())
            })?;
            self.accept_producer_direct(
                &authority,
                ExtensionProducerSendParams {
                    session_id: session,
                    producer_id,
                    mode: ProducerDeliveryMode::Steer,
                    idempotency_key: ProducerIdempotencyKey::new("delegation-completion")
                        .expect("static delegation idempotency key is valid"),
                    body,
                },
                None,
            )?;
        }
        if !already_logged {
            self.append_direct(
                session,
                Some(reservation.parent_run_id),
                event_origin("engine:delegation"),
                Event::DelegateFinishedV2 {
                    invocation_id: reservation.invocation_id,
                    session_id: reservation.child_session_id,
                    status,
                    preview,
                    total_lines,
                },
            )?;
        }
        Ok(true)
    }

    pub(super) fn reconcile_consumed_producers(
        &self,
        session: SessionId,
        recovery: bool,
    ) -> Result<(), EngineError> {
        for message in self.goal_producer_projection(session)?.messages {
            if let Some(run_id) = message
                .consumed_run
                .filter(|_| !message.consumption_recorded)
            {
                let event = Event::ProducerMessageConsumed {
                    message_id: message.message_id,
                    run_id,
                };
                if recovery {
                    self.append_recovery_direct(
                        session,
                        Some(run_id),
                        event_origin("engine:producer"),
                        event,
                    )?;
                } else {
                    self.append_direct(
                        session,
                        Some(run_id),
                        event_origin("engine:producer"),
                        event,
                    )?;
                }
            }
        }
        Ok(())
    }

    pub(super) fn repair_goal_completion(
        &self,
        session: SessionId,
        recovery: bool,
    ) -> Result<(), EngineError> {
        if !matches!(
            self.inner.store.get(session)?.meta.origin,
            SessionOrigin::Root
        ) {
            return Ok(());
        }
        if let Some(goal) = self.goal_producer_projection(session)?.goal.filter(|goal| {
            matches!(goal.status, GoalStatus::Active | GoalStatus::Paused)
                && !goal.items.is_empty()
                && goal.items.iter().all(|item| item.finished)
        }) {
            let event = Event::GoalLifecycleChanged {
                goal_id: goal.goal_id,
                status: GoalStatus::Completed,
                revision: next_revision(goal.revision)?,
                selection: None,
            };
            if recovery {
                self.append_recovery_direct(session, None, event_origin("engine:goal"), event)?;
            } else {
                self.append_direct(session, None, event_origin("engine:goal"), event)?;
            }
        }
        Ok(())
    }

    fn plugin_goals_ready(&self) -> bool {
        self.inner
            .plugins
            .producer_recovery_states()
            .iter()
            .all(|state| state.status == PluginRecoveryStatus::Ready)
    }

    pub(super) fn reconcile_goal_registration(
        &self,
        session: SessionId,
    ) -> Result<(), EngineError> {
        let is_root = matches!(
            self.inner.store.get(session)?.meta.origin,
            SessionOrigin::Root
        );
        let projection = self.goal_producer_projection(session)?;
        let active = projection.goal.as_ref().filter(|goal| {
            is_root && goal.status == GoalStatus::Active && self.plugin_goals_ready()
        });
        let mut registry = self
            .inner
            .producers
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let state = registry.entry(session).or_default();
        state.registrations.retain(|record| !matches!(&record.authority.owner, ProducerOwner::Goal { goal_id } if active.is_none_or(|goal| goal.goal_id != *goal_id)));
        if let Some(goal) = active
            && !state.registrations.iter().any(|record| {
                record.authority.owner
                    == (ProducerOwner::Goal {
                        goal_id: goal.goal_id,
                    })
            })
        {
            state.registrations.push(Registration {
                id: ProducerId::new_v7(),
                authority: ProducerAuthority {
                    owner: ProducerOwner::Goal {
                        goal_id: goal.goal_id,
                    },
                    connection_epoch: None,
                },
                registered: Instant::now(),
            });
        }
        drop(registry);
        for message in projection
            .messages
            .iter()
            .filter(|message| pending(message) && message.reminder.is_some())
        {
            if !message.claims.is_empty() {
                continue;
            }
            let identity = message.reminder.expect("filtered reminder");
            if !active.is_some_and(|goal| {
                goal.goal_id == identity.goal_id && goal.revision == identity.revision
            }) {
                self.append_direct(
                    session,
                    None,
                    event_origin("engine:goal"),
                    Event::ProducerMessageDiscarded {
                        message_id: message.message_id,
                        reminder: Some(identity),
                        producer_owner: Some(message.producer_owner.clone()),
                    },
                )?;
            }
        }
        Ok(())
    }

    pub(super) fn promote_producer_inputs_direct(
        &self,
        session: SessionId,
        run: RunId,
        include_queue: bool,
    ) -> Result<bool, EngineError> {
        if !self
            .inner
            .store
            .get(session)?
            .runs
            .get(&run)
            .is_some_and(|run| run.status == SessionStatus::Running)
        {
            return Ok(false);
        }
        let projection = self.goal_producer_projection(session)?;
        let mut promoted = false;
        for message in projection.messages.iter().filter(|message| {
            pending(message)
                && (include_queue
                    || (message.mode == ProducerDeliveryMode::Steer && message.reminder.is_none()))
        }) {
            if message
                .admission
                .is_some_and(|(admitted_run, _)| admitted_run == run)
            {
                continue;
            }
            self.append_direct(
                session,
                Some(run),
                event_origin("engine:producer"),
                Event::ProducerMessageAdmitted {
                    message_id: message.message_id,
                },
            )?;
            promoted = true;
        }
        Ok(promoted)
    }

    pub(super) async fn reconcile_producers(&self, session: SessionId) -> Result<(), EngineError> {
        self.request(session, |reply| {
            SessionCommand::Producer(ProducerCommand::Reconcile { reply })
        })
        .await
    }

    pub(crate) async fn discard_producer_message(
        &self,
        session: SessionId,
        authority: ProducerAuthority,
        message_id: ProducerMessageId,
    ) -> Result<(), EngineError> {
        self.request(session, |reply| {
            SessionCommand::Producer(ProducerCommand::Discard {
                authority,
                message_id,
                reply,
            })
        })
        .await
    }

    fn discard_producer_message_direct(
        &self,
        session: SessionId,
        authority: &ProducerAuthority,
        message_id: ProducerMessageId,
    ) -> Result<(), EngineError> {
        self.ensure_not_shutting_down()?;
        self.validate_producer_authority(authority)?;
        let projection = self.goal_producer_projection(session)?;
        let message = projection
            .messages
            .iter()
            .find(|message| message.message_id == message_id)
            .ok_or_else(|| EngineError::Producer("message not found".into()))?;
        if message.producer_owner != authority.owner {
            return Err(EngineError::Producer(
                "message belongs to another producer owner".into(),
            ));
        }
        if message.consumed || !message.claims.is_empty() {
            return Err(EngineError::Producer(
                "too late: message is consumed or reserved by a model request".into(),
            ));
        }
        if !message.discarded {
            self.append_direct(
                session,
                None,
                event_origin("engine:producer"),
                Event::ProducerMessageDiscarded {
                    message_id,
                    reminder: message.reminder,
                    producer_owner: Some(authority.owner.clone()),
                },
            )?;
        }
        self.inner.store.persist_buffered_session(session)?;
        Ok(())
    }

    pub(super) fn producers_pin_session(&self, session: SessionId) -> bool {
        self.inner
            .producers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&session)
            .is_some_and(|state| !state.registrations.is_empty() || state.wake_scheduled)
            || self
                .goal_producer_projection(session)
                .is_ok_and(|projection| projection.messages.iter().any(pending))
    }

    pub(super) fn reconcile_reverted_producers_direct(
        &self,
        session: SessionId,
    ) -> Result<(), EngineError> {
        let events = self.inner.store.get(session)?.log.event_snapshot();
        let surviving: HashSet<_> = events
            .iter()
            .filter_map(|event| match &event.payload {
                Event::DelegationReserved { reservation, .. } => Some(reservation.invocation_id),
                _ => None,
            })
            .collect();
        let removed_children = {
            let mut records = self
                .inner
                .delegations_by_session
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let removed: HashSet<_> = records
                .iter()
                .filter(|(_, record)| {
                    record.parent_session_id == session
                        && !surviving.contains(&record.invocation_id)
                })
                .map(|(child, _)| *child)
                .collect();
            records.retain(|child, _| !removed.contains(child));
            removed
        };
        self.inner
            .delegation_queue
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|child| !removed_children.contains(child));
        if let Some(state) = self
            .inner
            .producers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_mut(&session)
        {
            state
                .registrations
                .retain(|record| match &record.authority.owner {
                    ProducerOwner::Delegation { invocation_id } => {
                        surviving.contains(invocation_id)
                    }
                    _ => true,
                });
        }
        self.reconcile_goal_registration(session)
    }
}

fn next_revision(revision: u64) -> Result<u64, EngineError> {
    revision
        .checked_add(1)
        .ok_or_else(|| EngineError::Goal("revision exhausted".into()))
}

fn pending(message: &ProducerMessageRecord) -> bool {
    !message.consumed && !message.discarded
}

fn producer_fault(error: EngineError) -> JsonRpcError {
    JsonRpcError {
        code: -32000,
        message: error.to_string(),
        data: None,
    }
}
