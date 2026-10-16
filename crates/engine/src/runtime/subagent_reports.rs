//! Subagent reports (`docs/site/specs/background-tools.md` section 7.2): every
//! turn a delegated session finishes reports to its direct parent, whatever
//! started the turn.
//!
//! A run of a delegated session that ends schedules a `<subagent_report>`
//! producer message to the parent, owned by `Subagent { session_id }` (the
//! child) with the run ID as idempotency key, so each run reports once across
//! retries and restarts. Two runs report elsewhere: the first run of a
//! background delegation, through its invocation's completion, and the first
//! run of a foreground delegation, whose tool result carries it while the
//! call waits.

use std::time::Duration;

use cookie_agent_protocol::{
    ExtensionProducerSendParams, ProducerDeliveryMode, ProducerIdempotencyKey, ProducerOwner,
    RunId, SessionId, SessionOrigin, SessionStatus,
};

use super::{Engine, EngineError, Event, producers::ProducerAuthority};

impl Engine {
    /// Called once `event` is appended to `session`: a run of a delegated
    /// session that started or ended changes its parent's background calls,
    /// and one that ended reports to the parent.
    pub(super) fn observe_subagent_run(&self, session: SessionId, run: RunId, ended: bool) {
        let Ok(projection) = self.inner.store.get(session) else {
            return;
        };
        let SessionOrigin::Delegated {
            parent_session_id, ..
        } = projection.meta.origin
        else {
            return;
        };
        self.notify_background_changed(parent_session_id);
        if !ended {
            return;
        }
        let engine = self.clone();
        let Some(runtime) = self
            .inner
            .runtime
            .clone()
            .or_else(|| tokio::runtime::Handle::try_current().ok())
        else {
            return;
        };
        let _ = self.spawn_admission_task(&runtime, async move {
            engine
                .deliver_subagent_report_until_done(session, run)
                .await;
        });
    }

    /// Delivers the report of `run` of `child`, retrying until it is
    /// delivered, not owed, or the engine shuts down.
    pub(super) async fn deliver_subagent_report_until_done(&self, child: SessionId, run: RunId) {
        loop {
            match self.deliver_subagent_report(child, run).await {
                Ok(()) => return,
                Err(_) if self.admission_tasks_closing() => return,
                Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
            }
        }
    }

    async fn deliver_subagent_report(
        &self,
        child: SessionId,
        run: RunId,
    ) -> Result<(), EngineError> {
        let projection = self.inner.store.get(child)?;
        let SessionOrigin::Delegated {
            parent_session_id, ..
        } = projection.meta.origin
        else {
            return Ok(());
        };
        let Some(status) = projection.runs.get(&run).map(|run| run.status) else {
            return Ok(());
        };
        if !matches!(
            status,
            SessionStatus::Completed
                | SessionStatus::Failed
                | SessionStatus::Cancelled
                | SessionStatus::Interrupted
        ) {
            return Ok(());
        }
        if !self.report_owed(parent_session_id, child, run).await? {
            return Ok(());
        }
        let teaser = super::delegation::delegate_teaser(
            &projection,
            child,
            status,
            Some(run),
            self.subagent_preview_limits(),
        );
        let body = render_subagent_report(&teaser);
        let authority = ProducerAuthority {
            owner: ProducerOwner::Subagent { session_id: child },
            connection_epoch: None,
        };
        let key = ProducerIdempotencyKey::new(run.to_string())
            .map_err(|error| EngineError::Producer(error.to_string()))?;
        let producer_id = self
            .register_producer(parent_session_id, authority.clone())
            .await?;
        let description = super::producers::producer_description(
            "Subagent: ",
            &format!(
                "{} {}",
                teaser.short_id,
                super::delegation::session_status_name(status)
            ),
        );
        let accepted = self
            .on_actor(parent_session_id, move |engine| {
                let already = engine
                    .goal_producer_projection(parent_session_id)?
                    .messages
                    .iter()
                    .any(|message| {
                        message.producer_owner == authority.owner && message.idempotency_key == key
                    });
                if already {
                    return Ok(());
                }
                engine
                    .accept_producer_with_output_direct(
                        &authority,
                        ExtensionProducerSendParams {
                            session_id: parent_session_id,
                            producer_id,
                            mode: ProducerDeliveryMode::Steer,
                            idempotency_key: key,
                            description,
                            body,
                        },
                        None,
                        None,
                    )
                    .map(|_| ())
            })
            .await;
        self.drop_producer_registration(parent_session_id, producer_id);
        accepted
    }

    /// Whether `run` of `child` reports through a subagent report, rather
    /// than through its invocation's completion or a foreground tool result.
    async fn report_owed(
        &self,
        parent: SessionId,
        child: SessionId,
        run: RunId,
    ) -> Result<bool, EngineError> {
        let delegation_events = self.inner.delegation_events.clone();
        let entries = self
            .spawn_admission_blocking(move || Ok::<_, EngineError>(delegation_events.entries()))
            .await?;
        let Some(entry) = entries.iter().find(|entry| {
            entry.reservation.child_session_id == child && entry.child_run_id == Some(run)
        }) else {
            return Ok(true);
        };
        if entry.request.background {
            return Ok(false);
        }
        if self.foreground_waits_for(child, run) {
            return Ok(false);
        }
        // A foreground call that got the child's result reported it already.
        let parent = self.inner.store.get(parent)?;
        let reported = parent.log.events().iter().any(|event| {
            matches!(
                &event.payload,
                Event::ToolCallTerminated { termination }
                    if termination.tool_call_id == entry.reservation.parent_tool_call_id
                        && termination.result.as_ref().is_some_and(|result| {
                            result.metadata.get("background").is_none()
                                && crate::delegation_api::delegate_result_matches_child(
                                    result, child,
                                )
                        })
            )
        });
        Ok(!reported)
    }
}

/// The `<subagent_report>` element a subagent's finished turn reports.
pub(super) fn render_subagent_report(teaser: &super::delegation::DelegateTeaser) -> String {
    format!(
        "<subagent_report handle=\"{}\" status=\"{}\">\n{}\n</subagent_report>",
        teaser.short_id.replace('"', "&quot;"),
        super::delegation::session_status_name(teaser.status),
        super::delegation::render_delegate_teaser_body(
            &teaser.preview,
            teaser.status,
            teaser.total_lines,
            &teaser.short_id,
        )
    )
}
