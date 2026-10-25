use std::{
    collections::{HashMap, HashSet, VecDeque},
    num::NonZeroU32,
    sync::{Arc, Mutex},
};

use cookie_agent_protocol::{
    EventOrigin, EventSubscriptionMessage, EventsSubscribeResult, ExtensionBusEventParams,
    ExtensionEmitStatus, PersistedToolResult as ToolResult, PluginDiagnosticKind, RunId,
    SafeToolError, SessionId, StoredEvent, ToolCallId, ToolCallTermination, ToolTerminationOutcome,
};
use tokio::sync::{mpsc, oneshot};

#[cfg(test)]
use super::PagingRaceHook;
use super::ToolCallFailureCode;
use super::{
    Engine, EngineError, Event, MAX_COMPACTION_DEFERRED_COMMANDS, PendingInput,
    SESSION_MAILBOX_CAPACITY, SessionCommand, ToolFailure, helpers::safe_error,
};
use crate::{actor::SessionActor, blocking::off_worker, events, session::SessionError};

use super::producers;

/// A session's producer/goal fold and how far into its log it folded.
pub(super) struct CachedProducerProjection {
    /// The events folded, a prefix of the log.
    pub(super) folded: usize,
    /// The last event folded. Holding it keeps its allocation alive, so an
    /// event at the same place in a later view of the log that is the same
    /// allocation means the log still begins with what was folded.
    pub(super) last: Option<Arc<cookie_agent_protocol::StoredEvent>>,
    pub(super) fold: crate::goal_projection::GoalProducerFold,
}

impl CachedProducerProjection {
    /// Whether `events` begins with the events this fold went through.
    pub(super) fn is_prefix_of(&self, events: &[Arc<cookie_agent_protocol::StoredEvent>]) -> bool {
        match (self.folded.checked_sub(1), &self.last) {
            (None, None) => true,
            (Some(index), Some(last)) => events
                .get(index)
                .is_some_and(|event| Arc::ptr_eq(event, last)),
            _ => false,
        }
    }
}

/// Per-session mailbox and run state owned by [`super::Inner`].
#[derive(Default)]
pub(crate) struct SessionRuntimeState {
    pub(super) actors: Mutex<HashMap<SessionId, SessionActor<SessionCommand>>>,
    pub(super) active: super::runs::ActiveRuns,
    pub(super) producers: Mutex<HashMap<SessionId, producers::SessionProducers>>,
    /// Each session's latest producer/goal fold, carried forward over what
    /// its log gains.
    pub(super) producer_projections: Mutex<HashMap<SessionId, CachedProducerProjection>>,
    /// Each delegated session that waits on something live, keyed to its
    /// direct parent, which waits on it as a `subagent` (see
    /// `Engine::refresh_live_waiting`). Its lock serializes those refreshes
    /// and is taken before the producer, run, and store locks, never under
    /// them.
    pub(super) waking_children: Mutex<HashMap<SessionId, SessionId>>,
    /// The task driving each in-flight run loop.
    ///
    /// Shutdown joins these after cancelling their runs, so a run task gets to
    /// write its `RunCancelled` while the session actors and the store are
    /// still up. A task drops its own entry when it finishes, and a new
    /// registration sweeps whatever finished without being noticed.
    pub(super) run_tasks: Mutex<HashMap<RunId, tokio::task::JoinHandle<()>>>,
    pub(crate) residency_mutation: tokio::sync::Mutex<()>,
    /// Forks made with a client fork id, so a retry returns the same fork:
    /// (source, key) -> (through seq, fork).
    pub(super) forks:
        Mutex<HashMap<(SessionId, cookie_agent_protocol::ClientForkId), (u64, SessionId)>>,
    /// Per-session order for steers, held across plugin input interception
    /// so two steers are admitted in the order they arrived.
    pub(super) steer_order: Mutex<HashMap<SessionId, Arc<tokio::sync::Mutex<()>>>>,
}

/// Removes a run task's join handle when its task ends, however it ends.
pub(super) struct RunTaskSlot {
    pub(super) engine: Engine,
    pub(super) run_id: RunId,
}

impl Drop for RunTaskSlot {
    fn drop(&mut self) {
        if let Ok(mut tasks) = self.engine.inner.sessions.run_tasks.lock() {
            tasks.remove(&self.run_id);
        }
    }
}

impl Engine {
    /// Records the task driving `run_id` so shutdown can wait for it.
    ///
    /// A task that finished before its handle arrived here has already dropped
    /// its slot, so the sweep is what retires the handle it left behind.
    pub(super) fn register_run_task(&self, run_id: RunId, task: tokio::task::JoinHandle<()>) {
        let mut tasks = self
            .inner
            .sessions
            .run_tasks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        tasks.retain(|_, task| !task.is_finished());
        tasks.insert(run_id, task);
    }

    /// Waits for the in-flight run tasks, then aborts whatever is left.
    ///
    /// Called from shutdown while the session actors and the store are still
    /// up, so a cancelled run's terminal event lands in its log instead of
    /// being repaired as a restart interruption on the next startup.
    pub(super) async fn join_run_tasks(&self, bound: std::time::Duration) {
        let tasks = self
            .inner
            .sessions
            .run_tasks
            .lock()
            .map(|mut tasks| tasks.drain().map(|(_, task)| task).collect::<Vec<_>>())
            .unwrap_or_default();
        if tasks.is_empty() {
            return;
        }
        let deadline = tokio::time::Instant::now() + bound;
        for mut task in tasks {
            if tokio::time::timeout_at(deadline, &mut task).await.is_err() {
                task.abort();
                let _ = task.await;
            }
        }
    }

    /// Replays every event after `cursor` and registers a live tail.
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
        let (result, receiver) = self
            .subscribe_page(session, cursor, None, None, None)
            .await?;
        Ok((
            result,
            receiver.expect("an unlimited subscription is always its final page"),
        ))
    }

    /// One page of at most `limit` events after `cursor`. Only the final page
    /// (no `has_more`) registers the live tail and returns its receiver. A
    /// tail registered for an `owner` replaces the one it already holds for
    /// the session: the earlier receiver ends once drained, and nothing
    /// published afterwards reaches it.
    /// Ends `owner`'s live tail on `session`. Returns whether it had one.
    pub fn unsubscribe(&self, session: SessionId, owner: crate::session::TailOwner) -> bool {
        self.inner.store.unsubscribe_events(session, owner)
    }

    pub async fn subscribe_page(
        &self,
        session: SessionId,
        cursor: Option<u64>,
        cursor_timestamp: Option<jiff::Timestamp>,
        limit: Option<NonZeroU32>,
        owner: Option<crate::session::TailOwner>,
    ) -> Result<
        (
            EventsSubscribeResult,
            Option<mpsc::Receiver<EventSubscriptionMessage>>,
        ),
        EngineError,
    > {
        // Complete the tree only: a full `get` here would parse and fold a
        // log this process does not own just to learn that, and the snapshot
        // fallback reads it again.
        self.inner.store.ensure_tree_for(session)?;
        if !self.inner.store.is_owned(session) {
            return Err(EngineError::SessionOwnedByAnotherProcess(session));
        }
        // A history page or live-tail registration appends nothing.
        self.on_actor_unreconciled(session, move |engine| {
            Ok(engine.inner.store.subscribe_events(
                session,
                cursor,
                cursor_timestamp,
                limit,
                owner,
            )?)
        })
        .await
    }

    /// One page of events for a session this process does not own. It never
    /// registers a live tail.
    pub fn snapshot_events(
        &self,
        session: SessionId,
        cursor: Option<u64>,
        cursor_timestamp: Option<jiff::Timestamp>,
        limit: Option<NonZeroU32>,
    ) -> Result<EventsSubscribeResult, EngineError> {
        Ok(self
            .inner
            .store
            .snapshot_events(session, cursor, cursor_timestamp, limit)?)
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
            .output
            .hubs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&call)
            .cloned()
            .map(|hub| hub.subscribe(stream, 256))
    }

    pub fn tool_output_streams(
        &self,
        call_id: ToolCallId,
    ) -> Option<Vec<cookie_agent_protocol::OutputStream>> {
        self.inner
            .output
            .hubs
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&call_id)
            .map(|hub| hub.streams())
    }

    pub(super) fn retain_finalized_output_hub(&self, call: ToolCallId) {
        const FINALIZED_HUB_RETENTION: usize = 128;
        /// What the kept hubs hold at most together: a hub holds up to 64 KiB
        /// a stream, but most calls print far less.
        const FINALIZED_HUB_RETENTION_BYTES: u64 = 2 * 1024 * 1024;
        let mut hubs = self
            .inner
            .output
            .hubs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let bytes = hubs.get(&call).map_or(0, events::OutputHub::retained_bytes);
        let mut finalized = self
            .inner
            .output
            .finalized_hubs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        finalized.hubs.push_back((call, bytes));
        finalized.bytes += bytes;
        while finalized.hubs.len() > FINALIZED_HUB_RETENTION
            || (finalized.bytes > FINALIZED_HUB_RETENTION_BYTES && finalized.hubs.len() > 1)
        {
            let Some((expired, bytes)) = finalized.hubs.pop_front() else {
                break;
            };
            finalized.bytes -= bytes;
            hubs.remove(&expired);
        }
    }

    pub async fn append(
        &self,
        session: SessionId,
        run: Option<RunId>,
        origin: EventOrigin,
        event: Event,
    ) -> Result<(), EngineError> {
        self.request(session, |reply| SessionCommand::Append {
            run,
            origin,
            event: Box::new(event),
            reply,
        })
        .await
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
            partial_output: None,
            code: ToolCallFailureCode::ExecutionFailed,
            message,
        });
        self.submit_tool_result_status(session, run, tool_call_id, result, false)
            .await
            .and_then(|committed| {
                committed
                    .then_some(())
                    .ok_or_else(|| EngineError::ToolFailed("tool call is no longer pending".into()))
            })
    }

    pub(super) async fn submit_tool_result_status(
        &self,
        session: SessionId,
        run: RunId,
        tool_call_id: ToolCallId,
        result: Result<ToolResult, ToolFailure>,
        cancelled: bool,
    ) -> Result<bool, EngineError> {
        // A tool termination touches nothing producer reconciliation reads.
        self.on_actor_unreconciled(session, move |engine| {
            let pending = engine.inner.store.get(session).is_ok_and(|projection| {
                projection
                    .runs
                    .get(&run)
                    .is_some_and(|run| run.pending_calls.contains_key(&tool_call_id))
            });
            let response = if !pending {
                Ok(false)
            } else {
                (|| {
                    let owner = engine.tool_call_owner(session, run, tool_call_id)?;
                    let event = match result {
                        Ok(result) => Event::ToolCallTerminated {
                            termination: ToolCallTermination {
                                tool_call_id,
                                owner,
                                outcome: if cancelled {
                                    ToolTerminationOutcome::Cancelled
                                } else {
                                    ToolTerminationOutcome::Completed
                                },
                                result: Some(result),
                                error: cancelled.then(|| SafeToolError {
                                    code: super::safe_code(super::CANCELLED_AFTER_COMPLETION),
                                    message: safe_error("tool call cancelled after it started"),
                                }),
                            },
                        },
                        Err(failure) => Event::ToolCallTerminated {
                            termination: ToolCallTermination {
                                tool_call_id,
                                owner,
                                outcome: if cancelled {
                                    ToolTerminationOutcome::Cancelled
                                } else {
                                    ToolTerminationOutcome::Failed
                                },
                                result: failure.partial_output.map(|result| *result),
                                error: Some(SafeToolError {
                                    code: failure.code.safe_code(),
                                    message: safe_error(&failure.message),
                                }),
                            },
                        },
                    };
                    engine.append_direct(
                        session,
                        Some(run),
                        super::event_origin("engine:tool-result"),
                        event,
                    )?;
                    Ok(true)
                })()
            };
            if let Some(capture) = engine
                .inner
                .output
                .captures
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .remove(&tool_call_id)
            {
                capture.release_publication();
            }
            response
        })
        .await
    }

    pub(crate) fn append_direct(
        &self,
        session: SessionId,
        run: Option<RunId>,
        origin: EventOrigin,
        event: Event,
    ) -> Result<(), EngineError> {
        self.append_direct_with_mode(session, run, origin, event, false)
            .map(|_| ())
    }

    pub(super) fn append_direct_record(
        &self,
        session: SessionId,
        run: Option<RunId>,
        origin: EventOrigin,
        event: Event,
    ) -> Result<Arc<StoredEvent>, EngineError> {
        self.append_direct_with_mode(session, run, origin, event, false)
    }

    pub(crate) fn append_recovery_direct(
        &self,
        session: SessionId,
        run: Option<RunId>,
        origin: EventOrigin,
        event: Event,
    ) -> Result<(), EngineError> {
        self.append_direct_with_mode(session, run, origin, event, true)
            .map(|_| ())
    }

    fn append_direct_with_mode(
        &self,
        session: SessionId,
        run: Option<RunId>,
        origin: EventOrigin,
        mut event: Event,
        recovery: bool,
    ) -> Result<Arc<StoredEvent>, EngineError> {
        if let Event::ToolCallTerminated { termination } = &mut event {
            // Finalization and after-result hooks can await after the executor succeeds.
            // Freeze status and retained-output state together at this append boundary.
            let cancelled = run.is_some_and(|run| {
                self.inner
                    .sessions
                    .active
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .get(&run)
                    .is_some_and(|active| {
                        active.session == session && active.cancellation.is_cancelled()
                    })
            });
            if cancelled
                && matches!(
                    termination.outcome,
                    ToolTerminationOutcome::Completed | ToolTerminationOutcome::Failed
                )
            {
                let finalized = termination.outcome == ToolTerminationOutcome::Completed;
                termination.outcome = ToolTerminationOutcome::Cancelled;
                termination.error.get_or_insert_with(|| SafeToolError {
                    code: if finalized {
                        super::safe_code(super::CANCELLED_AFTER_COMPLETION)
                    } else {
                        ToolCallFailureCode::ExecutionFailed.safe_code()
                    },
                    message: safe_error("tool call cancelled after it started"),
                });
            }
            if termination.outcome != ToolTerminationOutcome::Completed
                && let Some(result) = &mut termination.result
            {
                let was_incomplete = result.retained_output.as_mut().map(|retained| {
                    let was_incomplete = retained.incomplete;
                    retained.incomplete = true;
                    was_incomplete
                });
                if termination.outcome == ToolTerminationOutcome::Cancelled
                    || was_incomplete.is_some_and(|incomplete| {
                        !incomplete
                            || cookie_agent_protocol::display_text(&result.display)
                                .trim()
                                .is_empty()
                    })
                {
                    result.display =
                        cookie_agent_protocol::text_display(match termination.outcome {
                            ToolTerminationOutcome::Cancelled => {
                                if result.retained_output.is_some() {
                                    "Tool cancelled; retained output is incomplete."
                                } else {
                                    "Tool cancelled; output is incomplete."
                                }
                            }
                            ToolTerminationOutcome::Interrupted => {
                                "Tool interrupted; retained output is incomplete."
                            }
                            _ => "Tool failed; retained output is incomplete.",
                        });
                }
            }
        }
        let run_edge = run.and_then(|run| match &event {
            Event::RunStarted { .. } => Some((run, false)),
            Event::RunFinished { .. } => Some((run, true)),
            _ => None,
        });
        let started = match &event {
            Event::ToolCallTerminated { termination } => Some((
                termination.tool_call_id,
                super::background_tasks::completed_start(termination),
            )),
            _ => None,
        };
        // Input to steer reaches a running ACP turn through its agent's
        // steering extension (`docs/site/specs/acp-agents.md` section 15).
        let steers = !recovery
            && match &event {
                Event::UserInputAdmitted { .. } => run.is_some(),
                Event::ProducerMessageAccepted { mode, .. } => {
                    *mode == cookie_agent_protocol::ProducerDeliveryMode::Steer
                }
                _ => false,
            };
        let was_persisted = self.inner.store.is_persisted(session)?;
        let envelope = if recovery {
            self.inner
                .store
                .append_recovery(session, run, origin, event)
        } else {
            self.inner.store.append(session, run, origin, event)
        };
        if let Some((tool_call_id, completed)) = started {
            self.settle_background_job(tool_call_id, completed && envelope.is_ok());
        }
        let envelope = envelope?;
        if super::delegation::changes_subagent_waits(&envelope.payload) {
            self.subagent_changed(session);
        }
        if steers {
            self.inner.acp.processes.steering_input(session);
        }
        if let Some((run, ended)) = run_edge {
            self.observe_subagent_run(session, run, ended);
            if ended {
                self.observe_cron_run(session, run);
            }
        }
        if !recovery {
            self.observe_push_event(session, &envelope.payload);
        }
        if !was_persisted && self.inner.store.is_persisted(session)? {
            for durable in self.inner.store.log(session)?.event_snapshot().iter() {
                let drops = self.inner.plugins.stream_session_event(durable);
                self.record_plugin_drops(session, drops);
            }
        } else {
            let drops = self.inner.plugins.stream_session_event(&envelope);
            self.record_plugin_drops(session, drops);
        }
        Ok(envelope)
    }

    /// Delivers live-only stream output (text and reasoning deltas, tool
    /// progress) to event tails and plugins. It is never stored: the
    /// committed turn and the tool termination are its durable record.
    pub(super) fn publish_transient(&self, session: SessionId, run: RunId, payload: Event) {
        let Some(event) = self
            .inner
            .store
            .publish_transient(session, Some(run), payload)
        else {
            return;
        };
        let drops = self.inner.plugins.stream_transient_event(&event);
        self.record_plugin_drops(session, drops);
    }

    /// Durably marks the start of a streamed part when the attempt's output
    /// switches to `kind` (or first begins), ahead of the part's first
    /// live-only delta.
    pub(super) async fn mark_output_part(
        &self,
        session: SessionId,
        run: RunId,
        attempt_id: cookie_agent_protocol::AttemptId,
        streaming: &mut Option<cookie_agent_protocol::StreamedOutputKind>,
        kind: cookie_agent_protocol::StreamedOutputKind,
    ) -> Result<(), EngineError> {
        if *streaming == Some(kind) {
            return Ok(());
        }
        *streaming = Some(kind);
        self.append(
            session,
            Some(run),
            super::event_origin("engine:model-loop"),
            Event::ModelOutputStarted { attempt_id, kind },
        )
        .await
    }

    fn record_plugin_drops(
        &self,
        session: SessionId,
        drops: Vec<crate::plugin::PluginDeliveryDrop>,
    ) {
        for drop in drops {
            let class = match drop.class {
                crate::plugin::PluginDeliveryClass::Chunk => "chunk",
                crate::plugin::PluginDeliveryClass::Ordinary => "ordinary",
                crate::plugin::PluginDeliveryClass::Terminal => "terminal",
            };
            self.queue_plugin_diagnostic(
                session,
                drop.plugin,
                PluginDiagnosticKind::EventDrop,
                format!("plugin {class} event buffer overflow; event delivery was dropped"),
                1,
            );
        }
    }

    pub(crate) fn record_plugin_diagnostic(
        &self,
        session: SessionId,
        plugin: String,
        kind: PluginDiagnosticKind,
        message: String,
    ) {
        self.queue_plugin_diagnostic(session, plugin, kind, message, 1);
    }

    fn queue_plugin_diagnostic(
        &self,
        session_id: SessionId,
        plugin: String,
        kind: PluginDiagnosticKind,
        message: String,
        count: u64,
    ) {
        let message = cookie_agent_protocol::diagnostics::detail(&message).to_string();
        self.inner
            .plugin_diagnostics
            .accumulator
            .record((session_id, plugin, kind, message), count);
    }

    pub(crate) async fn publish_plugin_emit(
        &self,
        request: crate::plugin::PluginEmitRequest,
    ) -> crate::plugin::PluginEmitOutcome {
        let session_id = request.session_id;
        if let crate::plugin::PluginEmitContext::Rejected {
            diagnostic_session_id,
            reason,
        } = &request.context
        {
            let message = format!("plugin emit context rejected: {reason}");
            if let Some(diagnostic_session_id) = diagnostic_session_id {
                self.queue_plugin_diagnostic(
                    *diagnostic_session_id,
                    request.plugin.clone(),
                    PluginDiagnosticKind::ContextMismatch,
                    message,
                    1,
                );
            } else {
                self.inner
                    .plugins
                    .note_offender_diagnostic(&request.plugin, message);
            }
            return crate::plugin::PluginEmitOutcome {
                bus: ExtensionEmitStatus::Dropped,
                reason: Some(reason.clone()),
            };
        }
        let payload_bytes =
            serde_json::to_vec(&request.payload).map_or(usize::MAX, |bytes| bytes.len());
        let event_bytes = serde_json::to_vec(&serde_json::json!({
            "plugin": request.plugin,
            "name": request.name,
            "payload": request.payload,
        }))
        .map_or(usize::MAX, |bytes| bytes.len());
        let invalid_name = request.name.is_empty()
            || request.name.chars().count() > crate::plugin::MAX_PLUGIN_EVENT_NAME_CHARS
            || request.name.chars().any(char::is_control);
        if invalid_name
            || payload_bytes > crate::plugin::MAX_PLUGIN_EVENT_PAYLOAD_BYTES
            || event_bytes > crate::plugin::MAX_PLUGIN_EVENT_TOTAL_BYTES
        {
            self.queue_plugin_diagnostic(
                session_id,
                request.plugin.clone(),
                PluginDiagnosticKind::OversizedEvent,
                "plugin event name or serialized event exceeds its protocol limit".into(),
                1,
            );
            return crate::plugin::PluginEmitOutcome {
                bus: ExtensionEmitStatus::Dropped,
                reason: Some(format!(
                    "event name must be 1-{} control-free characters, payload at most {} bytes, and total event at most {} bytes",
                    crate::plugin::MAX_PLUGIN_EVENT_NAME_CHARS,
                    crate::plugin::MAX_PLUGIN_EVENT_PAYLOAD_BYTES,
                    crate::plugin::MAX_PLUGIN_EVENT_TOTAL_BYTES,
                )),
            };
        }
        if let Err(reason) =
            self.inner
                .plugins
                .check_publish_quota(&request.plugin, session_id, event_bytes)
        {
            self.queue_plugin_diagnostic(
                session_id,
                request.plugin.clone(),
                PluginDiagnosticKind::RateLimited,
                reason.into(),
                1,
            );
            return crate::plugin::PluginEmitOutcome {
                bus: ExtensionEmitStatus::Dropped,
                reason: Some(reason.into()),
            };
        }

        if !request.publish_bus {
            return crate::plugin::PluginEmitOutcome {
                bus: ExtensionEmitStatus::Rejected,
                reason: Some("plugin did not declare `publish_bus`".into()),
            };
        }
        {
            let event = crate::EngineEvent::PluginEvent {
                session_id,
                plugin: request.plugin.clone(),
                name: request.name.clone(),
                payload: request.payload.clone(),
            };
            let _ = self.inner.engine_events.send(event);
            let drops = self.inner.plugins.stream_bus_event(
                &ExtensionBusEventParams {
                    session_id,
                    context_id: None,
                    plugin: request.plugin.clone(),
                    name: request.name.clone(),
                    payload: request.payload.clone(),
                },
                Some(&request.plugin),
            );
            self.record_plugin_drops(session_id, drops);
        }
        crate::plugin::PluginEmitOutcome {
            bus: ExtensionEmitStatus::Published,
            reason: None,
        }
    }

    /// Queues `command` on `session`'s actor, adopting the session and
    /// starting its actor first. Holds the residency lock only while queueing,
    /// never while the command runs.
    async fn send_command(
        &self,
        session: SessionId,
        command: SessionCommand,
    ) -> Result<(), EngineError> {
        let _residency = self.inner.sessions.residency_mutation.lock().await;
        self.ensure_session_owned(session)?;
        self.spawn_actor(session)?;
        let actor = self
            .inner
            .sessions
            .actors
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&session)
            .cloned()
            .ok_or(EngineError::MissingActor(session))?;
        actor
            .send(command)
            .await
            .map_err(|_| EngineError::ActorStopped)
    }

    /// Sends one of the typed commands the actor loop special-cases and
    /// waits for its reply.
    pub(super) async fn request<T>(
        &self,
        session: SessionId,
        command: impl FnOnce(oneshot::Sender<Result<T, EngineError>>) -> SessionCommand,
    ) -> Result<T, EngineError> {
        let (reply, receiver) = oneshot::channel();
        self.send_command(session, command(reply)).await?;
        receiver.await.map_err(|_| EngineError::ActorStopped)?
    }

    /// Runs `call` on `session`'s actor, serialized with every other command
    /// for that session, then reconciles the session's producers.
    pub(super) async fn on_actor<T: Send + 'static>(
        &self,
        session: SessionId,
        call: impl FnOnce(&Engine) -> Result<T, EngineError> + Send + 'static,
    ) -> Result<T, EngineError> {
        self.call_on_actor(session, true, move |engine| async move {
            off_worker(|| call(&engine))
        })
        .await
    }

    /// [`Self::on_actor`] for work that touches nothing producer
    /// reconciliation reads, so no reconciliation follows it.
    pub(super) async fn on_actor_unreconciled<T: Send + 'static>(
        &self,
        session: SessionId,
        call: impl FnOnce(&Engine) -> Result<T, EngineError> + Send + 'static,
    ) -> Result<T, EngineError> {
        self.call_on_actor(session, false, move |engine| async move {
            off_worker(|| call(&engine))
        })
        .await
    }

    /// [`Self::on_actor`] for work that awaits while it holds the actor.
    pub(super) async fn on_actor_async<T, Fut>(
        &self,
        session: SessionId,
        call: impl FnOnce(Engine) -> Fut + Send + 'static,
    ) -> Result<T, EngineError>
    where
        T: Send + 'static,
        Fut: std::future::Future<Output = Result<T, EngineError>> + Send + 'static,
    {
        self.call_on_actor(session, true, call).await
    }

    async fn call_on_actor<T, F, Fut>(
        &self,
        session: SessionId,
        reconcile_producers: bool,
        call: F,
    ) -> Result<T, EngineError>
    where
        T: Send + 'static,
        F: FnOnce(Engine) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<T, EngineError>> + Send + 'static,
    {
        let (command, receiver) = SessionCommand::call(reconcile_producers, call);
        self.send_command(session, command).await?;
        receiver.await.map_err(|_| EngineError::ActorStopped)?
    }

    pub(super) async fn prompt_events(
        &self,
        session: SessionId,
        run: RunId,
    ) -> Result<super::producer_claims::ClaimedPrompt, EngineError> {
        #[cfg(test)]
        if let Some(hook) = {
            self.inner
                .test_hooks
                .prompt_before_claim_hook
                .lock()
                .expect("prompt before-claim hook lock poisoned")
                .take()
        } {
            if let Some(reached) = hook
                .reached
                .lock()
                .expect("prompt before-claim reached lock poisoned")
                .take()
            {
                let _ = reached.send(());
            }
            hook.release.notified().await;
        }
        let events = self
            .request(session, |reply| SessionCommand::PromotePendingInputs {
                run,
                reply,
            })
            .await?;
        #[cfg(test)]
        if let Some(hook) = {
            self.inner
                .test_hooks
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
    pub(crate) fn install_prompt_before_claim_hook_for_test(
        &self,
    ) -> (oneshot::Receiver<()>, std::sync::Arc<tokio::sync::Notify>) {
        let (reached, receiver) = oneshot::channel();
        let release = std::sync::Arc::new(tokio::sync::Notify::new());
        *self
            .inner
            .test_hooks
            .prompt_before_claim_hook
            .lock()
            .expect("before-claim hook lock") =
            Some(std::sync::Arc::new(super::PromptSnapshotHook {
                reached: std::sync::Mutex::new(Some(reached)),
                release: release.clone(),
            }));
        (receiver, release)
    }

    #[cfg(test)]
    pub(crate) fn install_prompt_snapshot_hook_for_test(
        &self,
    ) -> (oneshot::Receiver<()>, std::sync::Arc<tokio::sync::Notify>) {
        let (reached, receiver) = oneshot::channel();
        let release = std::sync::Arc::new(tokio::sync::Notify::new());
        *self
            .inner
            .test_hooks
            .prompt_snapshot_hook
            .lock()
            .expect("snapshot hook lock") = Some(std::sync::Arc::new(super::PromptSnapshotHook {
            reached: std::sync::Mutex::new(Some(reached)),
            release: release.clone(),
        }));
        (receiver, release)
    }

    #[cfg(test)]
    pub(crate) async fn reconcile_producers_for_test(
        &self,
        session: SessionId,
    ) -> Result<(), EngineError> {
        self.reconcile_producers(session).await
    }

    #[cfg(test)]
    pub(crate) fn compaction_reserved_for_test(&self, session: SessionId) -> bool {
        self.inner
            .compaction
            .in_progress
            .lock()
            .expect("compaction reservation lock poisoned")
            .contains(&session)
    }

    #[cfg(test)]
    pub(crate) fn run_active_for_test(&self, run: RunId) -> bool {
        self.inner
            .sessions
            .active
            .lock()
            .expect("active runs lock poisoned")
            .contains_key(&run)
    }

    #[cfg(test)]
    pub(crate) fn actor_resident_for_test(&self, session: SessionId) -> bool {
        self.inner
            .sessions
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
            .test_hooks
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
    ) -> Result<
        oneshot::Receiver<Result<cookie_agent_protocol::SessionCompactResult, EngineError>>,
        EngineError,
    > {
        let actor = self
            .inner
            .sessions
            .actors
            .lock()
            .expect("actor registry lock poisoned")
            .get(&session)
            .cloned()
            .ok_or(EngineError::MissingActor(session))?;
        let (reply, receiver) = oneshot::channel();
        let (command, _queued) = SessionCommand::call(true, move |engine| async move {
            engine.spawn_compaction(
                session,
                None,
                super::event_origin("engine:auto-compact"),
                reply,
            );
            Ok(())
        });
        actor
            .send(command)
            .await
            .map_err(|_| EngineError::ActorStopped)?;
        Ok(receiver)
    }

    pub(crate) fn spawn_actor(&self, session: SessionId) -> Result<(), EngineError> {
        let mut actors = self
            .inner
            .sessions
            .actors
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.ensure_not_shutting_down()?;
        if !self.inner.store.is_owned(session) {
            return Err(EngineError::SessionOwnedByAnotherProcess(session));
        }
        if actors.contains_key(&session) {
            return Ok(());
        }
        self.inner
            .compaction
            .context_token_estimators
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .entry(session)
            .or_default();
        let engine = self.clone();
        let runtime = tokio::runtime::Handle::try_current()
            .ok()
            .or_else(|| self.inner.runtime.clone());
        let actor =
            SessionActor::spawn(SESSION_MAILBOX_CAPACITY, runtime.as_ref(), move |command| {
                let engine = engine.clone();
                async move { engine.handle_actor_command(session, command).await }
            });
        actors.insert(session, actor);
        Ok(())
    }

    /// Refuses to change `session` when a newer build wrote a state event
    /// this one does not know: such a session can only be viewed. Every
    /// append checks the same in the event log; this answers before a
    /// request starts any work.
    pub(crate) fn ensure_appendable(&self, session: SessionId) -> Result<(), EngineError> {
        self.inner
            .store
            .log(session)?
            .ensure_appendable()
            .map_err(EngineError::Event)
    }

    pub(crate) fn ensure_session_owned(&self, session: SessionId) -> Result<(), EngineError> {
        self.ensure_not_shutting_down()?;
        let adoption_lock = self.inner.store.adoption_lock(session);
        let _adoption = adoption_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.ensure_not_shutting_down()?;
        let write_open = self
            .inner
            .store
            .begin_write(session)
            .map_err(|error| match error {
                SessionError::SessionLocked(_) => {
                    EngineError::SessionOwnedByAnotherProcess(session)
                }
                SessionError::StoreClosed => EngineError::ActorStopped,
                error => error.into(),
            })?;
        if let Err(error) = self.ensure_not_shutting_down() {
            if write_open == crate::session::WriteOpen::Adopting {
                self.inner.store.rollback_adoption(session);
            }
            return Err(error);
        }
        // Nothing is repaired in a render-only session: it takes no appends.
        let render_only = write_open == crate::session::WriteOpen::Adopting
            && self
                .inner
                .store
                .log(session)
                .is_ok_and(|log| log.is_render_only());
        if write_open == crate::session::WriteOpen::Adopting {
            if !render_only && let Err(error) = self.reconcile_session(session) {
                self.inner.store.rollback_adoption(session);
                return Err(error);
            }
            if let Err(error) = self.ensure_not_shutting_down() {
                self.inner.store.rollback_adoption(session);
                return Err(error);
            }
            if let Err(error) = self.inner.store.commit_adoption(session) {
                self.inner.store.rollback_adoption(session);
                return Err(if matches!(error, SessionError::StoreClosed) {
                    EngineError::ActorStopped
                } else {
                    error.into()
                });
            }
            self.ensure_not_shutting_down()?;
            if let Err(error) = self.recover_missing_delegation_children(session) {
                eprintln!("session {session} missing-child recovery deferred: {error}");
            }
            let root = self.inner.store.root_of(session).unwrap_or(session);
            if let Err(error) = self.rebuild_delegation_registry(root, true) {
                eprintln!("session {session} delegation recovery deferred: {error}");
            }
        }
        Ok(())
    }

    pub(super) fn ensure_not_shutting_down(&self) -> Result<(), EngineError> {
        if self
            .inner
            .delegation
            .admission_tasks_closing
            .load(std::sync::atomic::Ordering::Acquire)
        {
            Err(EngineError::ActorStopped)
        } else {
            Ok(())
        }
    }

    pub(super) fn reserve_compaction(&self, session: SessionId) -> bool {
        self.inner
            .compaction
            .in_progress
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(session)
    }

    pub(super) async fn finish_compaction(&self, session: SessionId) -> Result<(), EngineError> {
        self.on_actor_async(session, move |engine| async move {
            engine.release_compaction_direct(session).await;
            Ok(())
        })
        .await
    }

    /// Releases `session`'s compaction hold and replays the commands it
    /// deferred, in arrival order. Called on the session actor.
    ///
    /// The replay future is boxed as a named type: callers of this function
    /// are themselves reachable from [`Self::handle_actor_command`], so an
    /// opaque return type would make that recursion unresolvable.
    pub(super) fn release_compaction_direct(
        &self,
        session: SessionId,
    ) -> futures_util::future::BoxFuture<'_, ()> {
        if let Some(state) = self
            .inner
            .sessions
            .producers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_mut(&session)
        {
            state.starting = false;
        }
        self.subagent_changed(session);
        self.inner
            .compaction
            .in_progress
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&session);
        let deferred = self
            .inner
            .compaction
            .deferred
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&session)
            .unwrap_or_default();
        Box::pin(async move {
            for command in deferred {
                self.handle_actor_command(session, command).await;
            }
            self.inner.compaction.finished.notify_waiters();
        })
    }

    /// Returns once `session` holds no compaction or run-start reservation,
    /// so a submission neither fails nor waits in the one-slot deferral.
    pub(super) async fn wait_for_compaction(&self, session: SessionId) {
        loop {
            let finished = self.inner.compaction.finished.notified();
            tokio::pin!(finished);
            finished.as_mut().enable();
            if !self
                .inner
                .compaction
                .in_progress
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .contains(&session)
            {
                return;
            }
            finished.await;
        }
    }

    pub(super) async fn handle_actor_command(&self, session: SessionId, command: SessionCommand) {
        if matches!(&command, SessionCommand::Start { .. }) {
            {
                let mut registry = self
                    .inner
                    .sessions
                    .producers
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                let state = registry.entry(session).or_default();
                if state.starting {
                    state.preempted = true;
                }
            }
        }
        if let Some(kind) = command.compaction_deferred_kind()
            && self
                .inner
                .compaction
                .in_progress
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .contains(&session)
        {
            let rejected = {
                let mut deferred = self
                    .inner
                    .compaction
                    .deferred
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
        let reconcile = match &command {
            SessionCommand::Call {
                reconcile_producers,
                ..
            } => *reconcile_producers,
            SessionCommand::Append { event, .. } => matches!(
                &**event,
                Event::ModelTurnCommitted { .. }
                    | Event::RunFinished { .. }
                    | Event::GoalActivated { .. }
                    | Event::GoalChecklistRevised { .. }
                    | Event::GoalLifecycleChanged { .. }
                    | Event::ProducerMessageAccepted { .. }
                    | Event::ProducerMessageConsumed { .. }
                    | Event::ProducerMessageDiscarded { .. }
                    | Event::UserInputAdmitted { .. }
                    | Event::UserInputSubmitted { .. }
                    | Event::UserInputRecalled { .. }
            ),
            SessionCommand::Start { .. }
            | SessionCommand::Resume { .. }
            | SessionCommand::PromotePendingOrComplete { .. }
            | SessionCommand::PromotePendingInputs { .. } => true,
        };
        match command {
            SessionCommand::Call { call, .. } => call(self.clone()).await,
            SessionCommand::Append {
                run,
                origin,
                event,
                reply,
            } => {
                #[cfg(test)]
                if matches!(
                    &*event,
                    Event::MessageInjected { .. }
                        | Event::UserInputTransformed { .. }
                        | Event::UserInputSubmitted { .. }
                        | Event::RunFinished {
                            outcome: cookie_agent_protocol::Open::Known(
                                cookie_agent_protocol::RunOutcome::Failed { .. }
                            )
                        }
                ) && crate::runtime::take_injected_failure(
                    &self.inner.test_hooks.run_setup_append_failures,
                ) {
                    let event_name = match &*event {
                        Event::MessageInjected { .. } => "message injected",
                        Event::UserInputTransformed { .. } => "user input transformed",
                        Event::UserInputSubmitted { .. } => "user input submitted",
                        Event::RunFinished {
                            outcome:
                                cookie_agent_protocol::Open::Known(
                                    cookie_agent_protocol::RunOutcome::Failed { .. },
                                ),
                        } => "run failed",
                        _ => unreachable!("run setup failure filter checked"),
                    };
                    let _ = reply.send(Err(EngineError::Mcp(format!(
                        "injected {event_name} append failure"
                    ))));
                    return;
                }
                let _ = reply.send(off_worker(|| {
                    self.append_direct(session, run, origin, *event)
                }));
            }
            SessionCommand::Start {
                params,
                origin,
                admission,
                reply,
            } => {
                if !self.reserve_compaction(session) {
                    let _ = reply.send(Err(EngineError::SessionRunning(session)));
                } else {
                    let engine = self.clone();
                    tokio::spawn(async move {
                        let child_session_id = params.session_id;
                        let mut result = engine
                            .start_run_direct(params, origin, admission, false)
                            .await;
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
            SessionCommand::Resume { reply } => {
                let result = self
                    .resolve_interrupted_direct(session)
                    .await
                    .and_then(|()| Ok(self.inner.store.get(session)?.metadata()));
                let _ = reply.send(result);
            }
            SessionCommand::PromotePendingOrComplete {
                run,
                final_text,
                complete_if_empty,
                reply,
            } => {
                let result = self.active_run_in(session, run).and_then(|_| {
                    self.promote_pending_or_complete_direct(
                        session,
                        run,
                        final_text,
                        complete_if_empty,
                    )
                });
                let _ = reply.send(result);
            }
            SessionCommand::PromotePendingInputs { run, reply } => {
                let result = self.active_run_in(session, run).and_then(|_| {
                    self.promote_producer_inputs_direct(session, run, false)?;
                    let events = self.inner.store.log(session)?.event_snapshot();
                    let run_events = from_run_start(&events, run);
                    let applied: HashSet<u64> = run_events
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
                        run_events.iter().filter_map(|event| match &event.payload {
                            Event::UserInputSubmitted { .. }
                                if event.run_id == Some(run) && !applied.contains(&event.seq) =>
                            {
                                Some(event.seq)
                            }
                            _ => None,
                        })
                    {
                        self.append_direct(
                            session,
                            Some(run),
                            super::event_origin("engine:model-loop"),
                            Event::UserInputApplied { user_input_seq },
                        )?;
                    }
                    self.claim_producer_snapshot_direct(session, run)
                });
                let _ = reply.send(result);
            }
        }
        if reconcile && let Err(error) = self.reconcile_producers_direct(session, None) {
            eprintln!("session {session} producer reconciliation failed: {error}");
        }
    }

    /// Submits the run's pending inputs, completing the run when nothing was
    /// left to promote. Returns whether the run continues. Called on the
    /// session actor.
    fn promote_pending_or_complete_direct(
        &self,
        session: SessionId,
        run: RunId,
        final_text: Option<String>,
        complete_if_empty: bool,
    ) -> Result<bool, EngineError> {
        let mut promoted = self.promote_producer_inputs_direct(session, run, false)?;
        for pending in pending_inputs(&self.inner.store.log(session)?.event_snapshot(), run) {
            self.append_direct(
                session,
                Some(run),
                pending.origin,
                Event::UserInputSubmitted {
                    content: pending.content,
                },
            )?;
            promoted = true;
        }
        promoted |= self.promote_producer_inputs_direct(session, run, false)?;
        if !promoted && complete_if_empty {
            // The run's task stops once it completes: its entry goes as the
            // completion is published.
            let released = self.release_active_run(session, run);
            if let Err(error) = self.append_direct(
                session,
                Some(run),
                super::event_origin("engine:model-loop"),
                Event::run_finished(cookie_agent_protocol::RunOutcome::Completed { final_text }),
            ) {
                self.restore_active_run(run, released);
                return Err(error);
            }
        }
        Ok(promoted)
    }
}

/// Where `run` starts in `events`: the index of its `RunStarted`, looked
/// for from the end, since the run asked about is nearly always the latest.
fn run_start_index<E: std::borrow::Borrow<StoredEvent>>(events: &[E], run: RunId) -> Option<usize> {
    events.iter().rposition(|event| {
        let event = event.borrow();
        event.run_id == Some(run) && matches!(event.payload, Event::RunStarted { .. })
    })
}

/// The events from `run`'s start on, which hold every event of the run;
/// the whole log when it has no start for the run.
fn from_run_start<E: std::borrow::Borrow<StoredEvent>>(events: &[E], run: RunId) -> &[E] {
    &events[run_start_index(events, run).unwrap_or(0)..]
}

/// The inputs admitted for `run` that it has yet to submit. Reads only the
/// run's own events and the runless admissions queued since the run before
/// it ended, not the whole log.
pub(super) fn pending_inputs<E: std::borrow::Borrow<StoredEvent>>(
    events: &[E],
    run: RunId,
) -> Vec<PendingInput> {
    let mut pending = VecDeque::new();
    let mut initial_input_submitted = false;
    let start = run_start_index(events, run);
    let run_start_seq = start.map(|start| events[start].borrow().seq);
    // The run before this one ended at `previous_terminal`; nothing before
    // it passes the filter below.
    let previous_terminal = start.and_then(|start| {
        events[..start]
            .iter()
            .rposition(|event| matches!(event.borrow().payload, Event::RunFinished { .. }))
    });
    let previous_terminal_seq =
        previous_terminal.map_or(0, |previous| events[previous].borrow().seq);
    let events = events[previous_terminal.unwrap_or(0)..]
        .iter()
        .map(std::borrow::Borrow::borrow);
    for event in events.filter(|event| {
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
            Event::ProducerMessageAdmitted { .. } => initial_input_submitted = true,
            Event::UserInputAdmitted { content } => pending.push_back(PendingInput {
                admission_seq: event.seq,
                origin: event
                    .origin
                    .clone()
                    .unwrap_or_else(|| super::event_origin("user")),
                content: content.clone(),
            }),
            Event::UserInputRecalled { user_input_seq, .. } => {
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
            Event::RunFinished { .. } => pending.clear(),
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
            engine_version: None,
            origin: None,
            session_id: SessionId::new_v7(),
            run_id: Some(run),
            seq,
            timestamp: jiff::Timestamp::now(),
            payload,
        }
    }

    #[test]
    fn pending_lane_replays_fifo_promotion_targeted_recall_and_terminal_voiding() {
        let run = RunId::new_v7();
        let events = vec![
            event(
                run,
                1,
                Event::UserInputSubmitted {
                    content: "initial".into(),
                },
            ),
            event(
                run,
                2,
                Event::UserInputAdmitted {
                    content: "one".into(),
                },
            ),
            event(
                run,
                3,
                Event::UserInputAdmitted {
                    content: "two".into(),
                },
            ),
            event(
                run,
                4,
                Event::UserInputRecalled {
                    user_input_seq: 3,
                    content: "two".into(),
                },
            ),
            event(
                run,
                5,
                Event::UserInputAdmitted {
                    content: "three".into(),
                },
            ),
            event(
                run,
                6,
                Event::UserInputSubmitted {
                    content: "one".into(),
                },
            ),
        ];
        assert_eq!(
            pending_inputs(&events, run),
            vec![PendingInput {
                admission_seq: 5,
                origin: cookie_agent_protocol::EventOrigin::new("user").unwrap(),
                content: "three".into(),
            }]
        );
        let replayed = serde_json::from_slice::<Vec<StoredEvent>>(
            &serde_json::to_vec(&events).expect("serialize replay events"),
        )
        .expect("deserialize replay events");
        assert_eq!(pending_inputs(&replayed, run), pending_inputs(&events, run));

        let mut terminal = replayed;
        terminal.push(event(
            run,
            7,
            Event::run_finished(cookie_agent_protocol::RunOutcome::Cancelled { reason: None }),
        ));
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
                    content: "initial".into(),
                },
            ),
            event(
                run,
                2,
                Event::UserInputAdmitted {
                    content: "resume".into(),
                },
            ),
            event(
                run,
                3,
                Event::UserInputAdmitted {
                    content: "direct steer".into(),
                },
            ),
            event(
                run,
                4,
                Event::UserInputRecalled {
                    user_input_seq: 2,
                    content: "resume".into(),
                },
            ),
        ];
        assert_eq!(
            pending_inputs(&events, run),
            vec![PendingInput {
                admission_seq: 3,
                origin: cookie_agent_protocol::EventOrigin::new("user").unwrap(),
                content: "direct steer".into(),
            }]
        );
    }
}
