//! Background jobs a tool call leaves running after it returns.
//!
//! A tool reserves a job while its call executes, returns its own result, and
//! hands the engine a future that resolves when the job ends. The engine owns
//! the rest: the job's output capture, a `Tool` producer registration, and the
//! completion message that wakes the session. A job only reports back once the
//! call that started it is durably committed as completed; a call that never
//! commits, a revert past it, or deleting its session kills the job instead.
//! Engine shutdown drops the job future, which kills whatever it owns.

use std::{
    collections::{HashMap, HashSet},
    future::Future,
    sync::{Mutex, Weak},
    time::Duration,
};

use cookie_agent_protocol::{
    ExtensionProducerSendParams, ProducerDeliveryMode, ProducerId, ProducerIdempotencyKey,
    ProducerOwner, RetainedToolOutput, SessionId, ToolCallId, ToolCallTermination,
    ToolOutputDeclaration, ToolTerminationOutcome,
};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use super::{
    Engine, EngineError, Inner, output_capture::OutputCapture, producers::ProducerAuthority,
};
use crate::{ProgressSink, ToolCompletion, ToolError, ToolProgress, events::OutputHub};

/// Idempotency key of the one completion message a job sends.
const COMPLETION_KEY: &str = "background-completion";

/// Live background jobs, keyed by the tool call that started them.
#[derive(Default)]
pub(crate) struct BackgroundTaskState {
    jobs: Mutex<HashMap<ToolCallId, Job>>,
}

impl BackgroundTaskState {
    #[cfg(test)]
    pub(crate) fn job_count(&self) -> usize {
        self.jobs.lock().unwrap_or_else(|p| p.into_inner()).len()
    }
}

struct Job {
    session: SessionId,
    kill: CancellationToken,
    /// Released once the starting call commits as completed.
    arm: Option<oneshot::Sender<()>>,
}

/// Lets one executing tool call reserve a background job.
#[derive(Clone)]
pub(crate) struct BackgroundCapability {
    pub(crate) inner: Weak<Inner>,
    pub(crate) tool_call_id: ToolCallId,
    /// The run's tool-output preview limits, applied to the job's report.
    pub(crate) limits: crate::preview::PreviewLimits,
}

impl std::fmt::Debug for BackgroundCapability {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BackgroundCapability")
            .field("tool_call_id", &self.tool_call_id)
            .finish_non_exhaustive()
    }
}

/// What a finished background job reports to the agent. The engine renders
/// `<element attributes…>`, the body, and the job's output preview (truncated
/// like any tool result), then the closing tag.
pub struct BackgroundReport {
    /// One-line summary shown on the notification row.
    pub summary: String,
    /// Element name wrapping the agent-visible report, such as `background_bash`.
    pub element: String,
    /// Attributes on the opening tag, in order.
    pub attributes: Vec<(String, String)>,
    /// Report lines placed before the output preview.
    pub body: String,
}

/// A reserved background job. [`BackgroundJob::progress`] captures the job's
/// output like a foreground call's; [`BackgroundJob::launch`] hands over the
/// future that runs it. Dropping the job unlaunched releases the reservation.
pub struct BackgroundJob {
    progress: ProgressSink,
    launch: Launch,
}

struct Launch {
    engine: Engine,
    session: SessionId,
    tool_call_id: ToolCallId,
    producer_id: ProducerId,
    capture: OutputCapture,
    progress: mpsc::Receiver<ToolProgress>,
    kill: CancellationToken,
    arm: oneshot::Receiver<()>,
}

impl Drop for Launch {
    fn drop(&mut self) {
        self.engine
            .release_background_job(self.session, self.tool_call_id, self.producer_id);
        self.capture.release_publication();
    }
}

impl BackgroundJob {
    pub(crate) async fn reserve(
        capability: &BackgroundCapability,
        session: SessionId,
    ) -> Result<Self, ToolError> {
        let inner = capability
            .inner
            .upgrade()
            .ok_or_else(|| ToolError::execution("engine is shutting down"))?;
        let engine = Engine { inner };
        let tool_call_id = capability.tool_call_id;
        let producer_id = engine
            .register_producer(session, tool_authority(tool_call_id))
            .await
            .map_err(|error| ToolError::execution(error.to_string()))?;
        let (sender, progress) = mpsc::channel(64);
        let (arm_sender, arm) = oneshot::channel();
        let kill = CancellationToken::new();
        engine
            .inner
            .background
            .jobs
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(
                tool_call_id,
                Job {
                    session,
                    kill: kill.clone(),
                    arm: Some(arm_sender),
                },
            );
        let capture = match OutputCapture::with_limits(
            engine.inner.artifacts.clone(),
            session,
            ToolOutputDeclaration::Named {
                streams: vec!["stdout".into(), "stderr".into()],
            },
            capability.limits,
        )
        .await
        {
            Ok(capture) => capture,
            Err(error) => {
                engine.release_background_job(session, tool_call_id, producer_id);
                return Err(error);
            }
        };
        Ok(Self {
            progress: ProgressSink::with_capture(
                sender,
                OutputHub::new(tool_call_id, 0),
                capture.clone(),
            ),
            launch: Launch {
                engine,
                session,
                tool_call_id,
                producer_id,
                capture,
                progress,
                kill,
                arm,
            },
        })
    }

    /// The sink the job writes its output to.
    #[must_use]
    pub fn progress(&self) -> ProgressSink {
        self.progress.clone()
    }

    /// Runs `job` detached from the call. Its report is delivered once the
    /// call commits; the job is dropped (killing what it owns) if the call
    /// never commits, is reverted, or its session is deleted.
    pub fn launch(
        self,
        job: impl Future<Output = BackgroundReport> + Send + 'static,
    ) -> Result<(), ToolError> {
        let Self { progress, launch } = self;
        drop(progress);
        let engine = launch.engine.clone();
        let runtime = engine
            .inner
            .runtime
            .clone()
            .or_else(|| tokio::runtime::Handle::try_current().ok())
            .ok_or_else(|| ToolError::execution("engine is shutting down"))?;
        if engine.spawn_admission_task(&runtime, launch.run(job)) {
            Ok(())
        } else {
            Err(ToolError::execution("engine is shutting down"))
        }
    }
}

impl Launch {
    async fn run(mut self, job: impl Future<Output = BackgroundReport> + Send) {
        let report = {
            let mut job = std::pin::pin!(job);
            loop {
                tokio::select! {
                    report = &mut job => break report,
                    () = self.kill.cancelled() => return,
                    // Display previews need a reader; the capture already has the bytes.
                    Some(_) = self.progress.recv() => {}
                }
            }
        };
        tokio::select! {
            armed = &mut self.arm => if armed.is_err() { return },
            () = self.kill.cancelled() => return,
        }
        let (output, retained) = match self
            .capture
            .finish(ToolCompletion::streamed(job_result()), false)
            .await
        {
            Ok(result) => (Some(result.output), result.retained_output),
            Err(_) => (None, None),
        };
        let body = render_notification(&report, output.as_deref());
        loop {
            let (summary, body, retained) =
                (report.summary.clone(), body.clone(), retained.clone());
            let (session, tool_call_id, producer_id) =
                (self.session, self.tool_call_id, self.producer_id);
            let committed = self
                .engine
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
            if committed.is_ok()
                || self.engine.admission_tasks_closing()
                || self.kill.is_cancelled()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}

fn tool_authority(tool_call_id: ToolCallId) -> ProducerAuthority {
    ProducerAuthority {
        owner: ProducerOwner::Tool { tool_call_id },
        connection_epoch: None,
    }
}

fn job_result() -> cookie_agent_protocol::PersistedToolResult {
    cookie_agent_protocol::PersistedToolResult {
        title: cookie_agent_protocol::SafeDisplayText::new("Background job output")
            .expect("static title is valid"),
        output: String::new(),
        display: None,
        retained_output: None,
        metadata: serde_json::Value::Null,
        truncation: None,
        attachments: Vec::new(),
        additional_messages: Vec::new(),
    }
}

/// Wraps a job's report and output preview in its element.
fn render_notification(report: &BackgroundReport, output: Option<&str>) -> String {
    let mut body = format!("<{}", report.element);
    for (name, value) in &report.attributes {
        let value = value
            .replace('&', "&amp;")
            .replace('"', "&quot;")
            .replace('<', "&lt;");
        body.push_str(&format!(" {name}=\"{value}\""));
    }
    body.push_str(">\n");
    body.push_str(report.body.trim_end());
    if let Some(output) = output
        .map(str::trim_end)
        .filter(|output| !output.is_empty())
    {
        body.push('\n');
        body.push_str(output);
    }
    body.push_str(&format!("\n</{}>", report.element));
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
    fn admission_tasks_closing(&self) -> bool {
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
            ExtensionProducerSendParams {
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
    fn notification_wraps_the_report_and_output_in_its_element() {
        let report = BackgroundReport {
            summary: "bash exited with code 0: make".into(),
            element: "background_bash".into(),
            attributes: vec![("from".into(), "42\"<&".into())],
            body: "Command: make\nStatus: exited with code 0 after 1.0s\n".into(),
        };
        assert_eq!(
            render_notification(&report, Some("[stdout]\nok\n\n[stderr]\n")),
            "<background_bash from=\"42&quot;&lt;&amp;\">\nCommand: make\nStatus: exited with \
             code 0 after 1.0s\n[stdout]\nok\n\n[stderr]\n</background_bash>"
        );
        assert_eq!(
            render_notification(&report, None),
            "<background_bash from=\"42&quot;&lt;&amp;\">\nCommand: make\nStatus: exited with \
             code 0 after 1.0s\n</background_bash>"
        );
    }
}
