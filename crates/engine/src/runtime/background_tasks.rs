//! Background calls: tool calls the host detached, as
//! `docs/site/specs/background-tools.md` specifies.
//!
//! A call of a background-capable tool is detached when the model starts it
//! with `background` or the user moves it there, once it reaches its detach
//! point. The engine commits an immediate result for the call and keeps its
//! execution running here: the same future, output capture, and
//! cancellation, no longer the run's. When the execution ends, the engine
//! renders the result the foreground call would have returned, wraps it in
//! `<background_result>`, and sends it as a `Tool`-owned producer message. It
//! reports only once the immediate result is durably committed; a call that
//! never commits, a revert past it, or deleting its session kills the
//! execution without a report, and so does engine shutdown.

use std::{
    collections::{HashMap, HashSet},
    future::Future,
    pin::Pin,
    sync::Mutex,
    time::Duration,
};

use cookie_agent_protocol::{
    PersistedToolResult, ProducerDeliveryMode, ProducerId, ProducerIdempotencyKey, ProducerOwner,
    ProducerSendParams, RetainedToolOutput, SessionId, ToolBackground, ToolBackgroundState,
    ToolCallId, ToolCallTermination, ToolTerminationOutcome,
};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use super::{Engine, EngineError, output_capture::OutputCapture, producers::ProducerAuthority};

use crate::{ToolCompletion, ToolError, ToolProgress, ToolResultTruncationPolicy};

/// Idempotency key of the one completion message a background call sends.
const COMPLETION_KEY: &str = "background-completion";

/// How long a stopped call gets to end before its execution is dropped.
const STOP_CLEANUP_TIMEOUT: Duration = Duration::from_secs(2);

/// Live background calls, keyed by the tool call they came from.
pub(crate) struct BackgroundTaskState {
    jobs: Mutex<HashMap<ToolCallId, Job>>,
    /// The sessions whose background calls changed.
    changes: tokio::sync::broadcast::Sender<SessionId>,
}

impl Default for BackgroundTaskState {
    fn default() -> Self {
        Self {
            jobs: Mutex::default(),
            changes: tokio::sync::broadcast::channel(64).0,
        }
    }
}

impl BackgroundTaskState {
    #[cfg(test)]
    pub(crate) fn job_count(&self) -> usize {
        self.jobs.lock().unwrap_or_else(|p| p.into_inner()).len()
    }
}

pub(crate) use cookie_agent_protocol::BackgroundActor;

struct Job {
    session: SessionId,
    job: String,
    tool: String,
    display: Option<String>,
    started_at: jiff::Timestamp,
    detached_at: jiff::Timestamp,
    by: BackgroundActor,
    /// Drops the execution without a report.
    kill: CancellationToken,
    /// Asks the execution to stop; it still reports.
    stop: CancellationToken,
    stopped_by: Option<BackgroundActor>,
    /// Cancelled once the execution has ended.
    ended: CancellationToken,
    /// Released once the immediate result commits as completed.
    arm: Option<oneshot::Sender<()>>,
}

/// The job ID of the background call that came from `tool_call_id`.
pub(crate) fn job_id(tool_call_id: ToolCallId) -> String {
    let hex = tool_call_id.to_string().replace('-', "");
    format!("job_{}", &hex[hex.len() - 8..])
}

/// A running call the engine is detaching.
pub(super) struct DetachedCall {
    pub(super) session: SessionId,
    pub(super) tool_call_id: ToolCallId,
    /// The tool's published name.
    pub(super) tool: String,
    /// The call's display argument, for the notification row.
    pub(super) display: Option<String>,
    pub(super) by: BackgroundActor,
    pub(super) started_at: jiff::Timestamp,
    /// The tool's line for the immediate result.
    pub(super) note: Option<String>,
    pub(super) execution: Pin<Box<dyn Future<Output = Result<ToolCompletion, ToolError>> + Send>>,
    pub(super) progress: mpsc::Receiver<ToolProgress>,
    pub(super) capture: Option<OutputCapture>,
    pub(super) truncation: ToolResultTruncationPolicy,
    pub(super) cancellation: CancellationToken,
}

impl Engine {
    /// Detaches `call`: registers its background job, keeps its execution
    /// running, and returns the immediate result to commit for the call.
    pub(super) async fn detach_call(
        &self,
        call: DetachedCall,
    ) -> Result<PersistedToolResult, ToolError> {
        let job = job_id(call.tool_call_id);
        let producer_id = self
            .register_producer(call.session, tool_authority(call.tool_call_id))
            .await
            .map_err(|error| ToolError::execution(error.to_string()))?;
        let (arm_sender, arm) = oneshot::channel();
        let kill = CancellationToken::new();
        let stop = CancellationToken::new();
        let ended = CancellationToken::new();
        self.inner
            .background
            .jobs
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(
                call.tool_call_id,
                Job {
                    session: call.session,
                    job: job.clone(),
                    tool: call.tool.clone(),
                    display: call.display.clone(),
                    started_at: call.started_at,
                    detached_at: jiff::Timestamp::now(),
                    by: call.by,
                    kill: kill.clone(),
                    stop: stop.clone(),
                    stopped_by: None,
                    ended: ended.clone(),
                    arm: Some(arm_sender),
                },
            );
        self.notify_background_changed(call.session);
        let immediate = immediate_result(&job, call.by, call.note.as_deref());
        let runtime = self
            .inner
            .runtime
            .clone()
            .or_else(|| tokio::runtime::Handle::try_current().ok());
        let (session, tool_call_id) = (call.session, call.tool_call_id);
        let started = runtime.is_some_and(|runtime| {
            self.spawn_admission_task(
                &runtime,
                self.clone().run_detached(
                    call,
                    job,
                    producer_id,
                    Signals {
                        kill,
                        stop,
                        ended,
                        arm,
                    },
                ),
            )
        });
        if !started {
            self.release_background_job(session, tool_call_id, producer_id);
            return Err(ToolError::execution("engine is shutting down"));
        }
        Ok(immediate)
    }

    async fn run_detached(
        self,
        call: DetachedCall,
        job: String,
        producer_id: ProducerId,
        mut signals: Signals,
    ) {
        let DetachedCall {
            session,
            tool_call_id,
            tool,
            display,
            mut execution,
            mut progress,
            capture,
            truncation,
            cancellation,
            ..
        } = call;
        let cleanup = tokio::time::sleep(Duration::MAX);
        tokio::pin!(cleanup);
        let mut stopping = false;
        let outcome = loop {
            tokio::select! {
                outcome = &mut execution => break Some(outcome),
                // Display previews need a reader; the capture already has the bytes.
                Some(_) = progress.recv() => {}
                () = signals.stop.cancelled(), if !stopping => {
                    stopping = true;
                    cancellation.cancel();
                    cleanup.as_mut().reset(tokio::time::Instant::now() + STOP_CLEANUP_TIMEOUT);
                }
                () = &mut cleanup, if stopping => break Some(Err(ToolError::execution(
                    "stopped; the call did not end within its cleanup deadline",
                ))),
                () = signals.kill.cancelled() => break None,
            }
        };
        drop(execution);
        signals.ended.cancel();
        self.notify_background_changed(session);
        let Some(outcome) = outcome else {
            self.release_background_job(session, tool_call_id, producer_id);
            return;
        };
        let stopped_by = self
            .inner
            .background
            .jobs
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&tool_call_id)
            .and_then(|job| job.stopped_by);
        let (status, result, error) = match outcome {
            Ok(completion) => {
                let failed = completion.failed;
                match super::tool_execution::finalize_output(
                    capture.as_ref(),
                    completion,
                    failed,
                    truncation,
                )
                .await
                {
                    Ok(result) => (
                        if failed { "failed" } else { "completed" },
                        Some(result),
                        None,
                    ),
                    Err(error) => ("failed", None, Some(error.to_string())),
                }
            }
            Err(error) => {
                let message = error.to_string();
                let partial = super::tool_execution::finalize_output(
                    capture.as_ref(),
                    super::tool_execution::incomplete_completion(&message),
                    true,
                    truncation,
                )
                .await
                .ok();
                ("failed", partial, Some(message))
            }
        };
        let status = if stopped_by.is_some() {
            "stopped"
        } else {
            status
        };
        let body = render_background_result(
            &job,
            &tool,
            status,
            stopped_by,
            result.as_ref(),
            error.as_deref(),
        );
        let retained = result.and_then(|result| result.retained_output);
        let summary = match display.as_deref().filter(|display| !display.is_empty()) {
            Some(display) => format!("{tool} {status}: {display}"),
            None => format!("{tool} {status}"),
        };
        tokio::select! {
            armed = &mut signals.arm => if armed.is_err() {
                self.release_background_job(session, tool_call_id, producer_id);
                return;
            },
            () = signals.kill.cancelled() => {
                self.release_background_job(session, tool_call_id, producer_id);
                return;
            }
        }
        let mut backoff = super::helpers::RetryBackoff::new();
        loop {
            let (summary, body, retained) = (summary.clone(), body.clone(), retained.clone());
            let committed = self
                .on_actor(session, move |engine| {
                    engine.commit_background_completion_direct(
                        session,
                        tool_call_id,
                        producer_id,
                        &summary,
                        body,
                        retained,
                    )
                })
                .await;
            let futile = committed
                .as_ref()
                .is_err_and(super::helpers::retry_is_futile);
            if committed.is_ok()
                || futile
                || self.admission_tasks_closing()
                || signals.kill.is_cancelled()
            {
                break;
            }
            tokio::select! {
                () = backoff.wait() => {}
                () = signals.kill.cancelled() => break,
            }
        }
        self.release_background_job(session, tool_call_id, producer_id);
    }

    /// The background calls of `session`, oldest first.
    pub fn background_calls(
        &self,
        session: SessionId,
    ) -> Vec<cookie_agent_protocol::BackgroundCall> {
        let mut calls = self
            .inner
            .background
            .jobs
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .filter(|(_, job)| job.session == session && !job.ended.is_cancelled())
            .map(
                |(tool_call_id, job)| cookie_agent_protocol::BackgroundCall {
                    job: job.job.clone(),
                    tool: job.tool.clone(),
                    tool_call_id: *tool_call_id,
                    display: job.display.clone(),
                    started_at: job.started_at,
                    detached_at: job.detached_at,
                    by: job.by,
                },
            )
            .collect::<Vec<_>>();
        calls.extend(self.running_subagent_turns(session));
        calls.sort_by_key(|call| call.detached_at);
        calls
    }

    /// The turns `session`'s direct children are running on their own: every
    /// running turn but the first turn of a foreground delegation, which its
    /// call still waits for. Each lists under the child's handle, which
    /// `stop_background` accepts.
    fn running_subagent_turns(
        &self,
        session: SessionId,
    ) -> Vec<cookie_agent_protocol::BackgroundCall> {
        let running = self
            .inner
            .sessions
            .active
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .map(|(run, active)| (*run, active.session, active.started_at))
            .collect::<Vec<_>>();
        running
            .into_iter()
            .filter_map(|(run, child, started_at)| {
                let projection = self.inner.store.get(child).ok()?;
                let cookie_agent_protocol::SessionOrigin::Delegated {
                    parent_session_id,
                    parent_tool_call_id,
                    ..
                } = projection.meta.origin
                else {
                    return None;
                };
                // A run whose end is appended no longer counts, though its
                // task has yet to drop it from the active runs: the parent
                // refreshes on that append, not on the drop.
                let ended = projection.runs.get(&run).is_some_and(|run| {
                    !matches!(
                        run.status,
                        cookie_agent_protocol::SessionStatus::Running
                            | cookie_agent_protocol::SessionStatus::Idle
                    )
                });
                if parent_session_id != session || ended || self.foreground_waits_for(child, run) {
                    return None;
                }
                Some(cookie_agent_protocol::BackgroundCall {
                    job: projection
                        .meta
                        .short_id
                        .clone()
                        .unwrap_or_else(|| child.to_string()),
                    tool: "delegate_subagent".into(),
                    tool_call_id: parent_tool_call_id,
                    display: projection
                        .meta
                        .title
                        .as_ref()
                        .map(|title| title.as_str().to_owned())
                        .or_else(|| Some(projection.creation_agent.agent.to_string())),
                    started_at,
                    detached_at: started_at,
                    by: BackgroundActor::Model,
                })
            })
            .collect()
    }

    /// Tells subscribers `session`'s background calls changed, and the
    /// store what the session now waits on.
    pub(super) fn notify_background_changed(&self, session: SessionId) {
        let _ = self.inner.background.changes.send(session);
        self.refresh_live_waiting(session);
    }

    /// Whether a direct child of `session` is running a turn on its own,
    /// one [`Self::background_calls`] lists.
    pub(super) fn has_running_subagent_turn(&self, session: SessionId) -> bool {
        !self.running_subagent_turns(session).is_empty()
    }

    /// The sessions whose background calls change, as they change.
    pub fn subscribe_background_changes(&self) -> tokio::sync::broadcast::Receiver<SessionId> {
        self.inner.background.changes.subscribe()
    }

    /// Stops the background call `job` of `session` as the user; see
    /// `stop_background`.
    pub async fn stop_background_as_user(
        &self,
        session: SessionId,
        job: &str,
    ) -> Result<(), EngineError> {
        self.stop_background(session, job, BackgroundActor::User)
            .await
            .map(|_| ())
    }

    /// Stops the background call `job` of `session`'s tree on behalf of
    /// `by`, and returns once its execution has ended. It still reports,
    /// with `status="stopped"`. `job` may also be the handle of a direct
    /// child, whose running turn is cancelled and reports as a subagent.
    pub(crate) async fn stop_background(
        &self,
        session: SessionId,
        job: &str,
        by: BackgroundActor,
    ) -> Result<Stopped, EngineError> {
        let root = self.session_root(session)?;
        let candidates = self
            .inner
            .background
            .jobs
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .filter(|(_, candidate)| candidate.job == job)
            .map(|(tool_call_id, candidate)| (*tool_call_id, candidate.session))
            .collect::<Vec<_>>();
        let mut found = None;
        for (tool_call_id, owner) in candidates {
            if self
                .session_root(owner)
                .is_ok_and(|owner_root| owner_root == root)
            {
                found = Some(tool_call_id);
                break;
            }
        }
        let ended = found.and_then(|tool_call_id| {
            let mut jobs = self
                .inner
                .background
                .jobs
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            let entry = jobs.get_mut(&tool_call_id)?;
            if entry.ended.is_cancelled() {
                return None;
            }
            entry.stopped_by.get_or_insert(by);
            entry.stop.cancel();
            Some(entry.ended.clone())
        });
        let Some(ended) = ended else {
            // A direct child's handle stops the turn it is running.
            if self
                .resolve_subagent_target(
                    session,
                    job,
                    super::handles::SubagentScope::DirectChildren,
                )
                .is_ok()
            {
                return self
                    .stop_subagent(session, job, stop_reason(by))
                    .await
                    .map(|()| Stopped::Subagent);
            }
            return Err(EngineError::Runtime(format!(
                "no running background call `{job}` in this session tree"
            )));
        };
        ended.cancelled().await;
        Ok(Stopped::Call)
    }
}

/// What [`Engine::stop_background`] stopped.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Stopped {
    /// A background call; its `<background_result>` follows.
    Call,
    /// A subagent's turn; its `<subagent_report>` follows.
    Subagent,
}

/// The reason a stopped subagent turn records.
fn stop_reason(by: BackgroundActor) -> Option<String> {
    Some(
        match by {
            BackgroundActor::Model => "stopped by its parent with stop_background",
            BackgroundActor::User => "stopped by the user",
        }
        .into(),
    )
}

struct Signals {
    kill: CancellationToken,
    stop: CancellationToken,
    ended: CancellationToken,
    arm: oneshot::Receiver<()>,
}

fn tool_authority(tool_call_id: ToolCallId) -> ProducerAuthority {
    ProducerAuthority {
        owner: ProducerOwner::Tool { tool_call_id },
        connection_epoch: None,
    }
}

/// The result committed for a call when it is detached.
fn immediate_result(job: &str, by: BackgroundActor, note: Option<&str>) -> PersistedToolResult {
    let lead = match by {
        BackgroundActor::Model => format!("Started in the background as job {job}."),
        BackgroundActor::User => format!(
            "User manually moved this call to the background; it is still running as job {job}."
        ),
    };
    let mut output = format!(
        "{lead} You will get a <background_result job=\"{job}\"> with its result when it \
         finishes; do not wait for it or run it again. Stop it with stop_background({job})."
    );
    if let Some(note) = note.filter(|note| !note.trim().is_empty()) {
        output.push('\n');
        output.push_str(note.trim());
    }
    PersistedToolResult {
        title: cookie_agent_protocol::SafeDisplayText::new("Moved to the background")
            .expect("static title is valid"),
        display: cookie_agent_runtime_api::bounded_tool_display(&output),
        output,
        background: Some(ToolBackground {
            state: ToolBackgroundState::Job,
            by,
            job: Some(job.to_owned()),
        }),
        retained_output: None,
        metadata: serde_json::Value::Null,
        attachments: Vec::new(),
        additional_messages: Vec::new(),
    }
}

/// The result committed for a call whose work reports itself (a subagent)
/// when it moves to the background: nothing keeps running under a job, so
/// the result points at the work's own reports instead.
pub(super) fn self_reporting_result(
    by: BackgroundActor,
    note: Option<&str>,
) -> PersistedToolResult {
    let mut output = match by {
        BackgroundActor::Model => "Started in the background.".to_owned(),
        BackgroundActor::User => {
            "User manually moved this call to the background; the work is still running.".to_owned()
        }
    };
    if let Some(note) = note.filter(|note| !note.trim().is_empty()) {
        output.push('\n');
        output.push_str(note.trim());
    }
    output.push_str("\nIts results arrive as reports when they are ready; do not wait for them.");
    PersistedToolResult {
        title: cookie_agent_protocol::SafeDisplayText::new("Moved to the background")
            .expect("static title is valid"),
        display: cookie_agent_runtime_api::bounded_tool_display(&output),
        output,
        background: Some(ToolBackground {
            state: ToolBackgroundState::SelfReporting,
            by,
            job: None,
        }),
        retained_output: None,
        metadata: serde_json::Value::Null,
        attachments: Vec::new(),
        additional_messages: Vec::new(),
    }
}

/// The `<background_result>` element a background call reports.
fn render_background_result(
    job: &str,
    tool: &str,
    status: &str,
    stopped_by: Option<BackgroundActor>,
    result: Option<&PersistedToolResult>,
    error: Option<&str>,
) -> String {
    let escape = |value: &str| {
        value
            .replace('&', "&amp;")
            .replace('"', "&quot;")
            .replace('<', "&lt;")
    };
    let mut body = format!(
        "<background_result job=\"{}\" tool=\"{}\" status=\"{status}\">\n",
        escape(job),
        escape(tool)
    );
    match stopped_by {
        Some(BackgroundActor::Model) => body.push_str("Stopped with stop_background.\n"),
        Some(BackgroundActor::User) => body.push_str("User stopped this background call.\n"),
        None => {}
    }
    if let Some(result) = result {
        body.push_str(&crate::model_history::tool_result_text(result));
        body.push('\n');
    }
    if let Some(error) = error {
        body.push_str(error);
        body.push('\n');
    }
    body.push_str("</background_result>");
    body
}

/// Whether a termination is the starting call's own committed completion,
/// the only state in which its job may report back.
pub(super) fn completed_start(termination: &ToolCallTermination) -> bool {
    termination.result.is_some()
        && match termination.outcome {
            ToolTerminationOutcome::Completed => true,
            ToolTerminationOutcome::Cancelled => termination
                .error
                .as_ref()
                .is_some_and(|error| error.code.as_str() == super::CANCELLED_AFTER_COMPLETION),
            _ => false,
        }
}

impl Engine {
    pub(super) fn admission_tasks_closing(&self) -> bool {
        self.inner
            .delegation
            .admission_tasks_closing
            .load(std::sync::atomic::Ordering::Acquire)
    }

    /// Settles the job of a just-appended termination: its committed
    /// completion arms it, anything else (including a failed append) kills it.
    pub(super) fn settle_background_job(&self, tool_call_id: ToolCallId, committed: bool) {
        let mut jobs = self
            .inner
            .background
            .jobs
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let Some(job) = jobs.get_mut(&tool_call_id) else {
            return;
        };
        if committed {
            if let Some(arm) = job.arm.take() {
                let _ = arm.send(());
            }
        } else {
            job.kill.cancel();
        }
    }

    /// Kills the jobs whose starting call no longer survives in `session`'s log.
    pub(super) fn kill_reverted_background_jobs(
        &self,
        session: SessionId,
    ) -> Result<(), EngineError> {
        let events = self.inner.store.log(session)?.event_snapshot();
        let surviving: HashSet<_> = events
            .iter()
            .filter_map(|event| match &event.payload {
                super::Event::ToolCallTerminated { termination }
                    if completed_start(termination) =>
                {
                    Some(termination.tool_call_id)
                }
                _ => None,
            })
            .collect();
        for (tool_call_id, job) in self
            .inner
            .background
            .jobs
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
        {
            if job.session == session && !surviving.contains(tool_call_id) {
                job.kill.cancel();
            }
        }
        Ok(())
    }

    /// Kills every job started in a deleted session.
    pub(super) fn kill_deleted_background_jobs(&self, deleted: &HashSet<SessionId>) {
        for job in self
            .inner
            .background
            .jobs
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .values()
        {
            if deleted.contains(&job.session) {
                job.kill.cancel();
            }
        }
    }

    fn release_background_job(
        &self,
        session: SessionId,
        tool_call_id: ToolCallId,
        producer_id: ProducerId,
    ) {
        if let Some(job) = self
            .inner
            .background
            .jobs
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&tool_call_id)
        {
            job.kill.cancel();
        }
        self.drop_producer_registration(session, producer_id);
    }

    /// Accepts a finished job's report once, if its starting call still
    /// stands in the session's log.
    pub(super) fn commit_background_completion_direct(
        &self,
        session: SessionId,
        tool_call_id: ToolCallId,
        producer_id: ProducerId,
        summary: &str,
        body: String,
        retained_output: Option<RetainedToolOutput>,
    ) -> Result<(), EngineError> {
        let authority = tool_authority(tool_call_id);
        let events = self.inner.store.log(session)?.event_snapshot();
        let started = events.iter().any(|event| {
            matches!(
                &event.payload,
                super::Event::ToolCallTerminated { termination }
                    if termination.tool_call_id == tool_call_id && completed_start(termination)
            )
        });
        let accepted = events.iter().any(|event| {
            matches!(
                &event.payload,
                super::Event::ProducerMessageAccepted { producer_owner, .. }
                    if *producer_owner == authority.owner
            )
        });
        if !started || accepted {
            return Ok(());
        }
        self.accept_producer_with_output_direct(
            &authority,
            ProducerSendParams {
                session_id: session,
                producer_id,
                mode: ProducerDeliveryMode::Steer,
                idempotency_key: ProducerIdempotencyKey::new(COMPLETION_KEY)
                    .expect("static background idempotency key is valid"),
                description: super::producers::producer_description("Background: ", summary),
                body,
            },
            None,
            retained_output,
        )
        .map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn job_ids_take_the_random_end_of_the_call_id() {
        let call = ToolCallId::new_v7();
        let job = job_id(call);
        assert!(job.starts_with("job_") && job.len() == 12, "{job}");
        assert!(call.to_string().replace('-', "").ends_with(&job[4..]));
    }

    #[test]
    fn immediate_results_say_who_moved_the_call_and_keep_the_note() {
        let model = immediate_result("job_1a2b3c4d", BackgroundActor::Model, Some("Process 42"));
        assert!(
            model
                .output
                .starts_with("Started in the background as job job_1a2b3c4d.")
        );
        assert!(
            model
                .output
                .contains("Stop it with stop_background(job_1a2b3c4d).\nProcess 42")
        );
        assert_eq!(
            model.background,
            Some(ToolBackground {
                state: ToolBackgroundState::Job,
                by: BackgroundActor::Model,
                job: Some("job_1a2b3c4d".into()),
            })
        );
        let user = immediate_result("job_1a2b3c4d", BackgroundActor::User, None);
        assert!(
            user.output
                .starts_with("User manually moved this call to the background")
        );
        assert_eq!(
            user.background
                .as_ref()
                .and_then(|background| background.job.as_deref()),
            Some("job_1a2b3c4d")
        );
        assert_eq!(user.metadata, serde_json::Value::Null);
    }

    #[test]
    fn background_results_wrap_the_foreground_text_and_say_who_stopped_them() {
        let result = PersistedToolResult {
            title: cookie_agent_protocol::SafeDisplayText::new("Bash").unwrap(),
            output: "[stdout]\nok\n".into(),
            display: Vec::new(),
            background: None,
            retained_output: None,
            metadata: serde_json::json!({"status": 0}),
            attachments: Vec::new(),
            additional_messages: Vec::new(),
        };
        let completed =
            render_background_result("job_1", "bash", "completed", None, Some(&result), None);
        assert_eq!(
            completed,
            format!(
                "<background_result job=\"job_1\" tool=\"bash\" status=\"completed\">\n{}\n</background_result>",
                crate::model_history::tool_result_text(&result)
            )
        );
        let stopped = render_background_result(
            "job_1",
            "a\"<b",
            "stopped",
            Some(BackgroundActor::User),
            None,
            Some("cancelled"),
        );
        assert_eq!(
            stopped,
            "<background_result job=\"job_1\" tool=\"a&quot;&lt;b\" status=\"stopped\">\nUser stopped \
             this background call.\ncancelled\n</background_result>"
        );
    }
}
