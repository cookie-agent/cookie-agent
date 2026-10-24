//! `cron_run` (`docs/site/specs/cron.md` section 8.2): an agent runs a job
//! now. An in-session job fires into its session as a scheduled fire
//! would. A new-session job runs as a delegated child of the caller, made
//! from the job's template, through the delegation path `delegate_subagent`
//! takes: the call's [`CronDelegation`], kept by its tool call ID while the
//! child is admitted, lets the job's agent through whatever the caller's
//! delegation targets are (any agent but an internal one), gives the child
//! the template's attachments and rules, and records the job on it.

use cookie_agent_protocol::{
    CronJobId, PermissionRule, RunId, RuntimeAttachment, SessionId, SessionStatus, ToolCallId,
};
use cookie_agent_runtime_api::{ToolCompletion, ToolError, ToolExecutionContext};
use jiff::Timestamp;

use super::{Engine, EngineError};
use crate::{DelegateInvocation, crons::StoredTarget};

/// What a `cron_run` call of a new-session job gives the child it creates.
#[derive(Clone, Debug)]
pub(crate) struct CronDelegation {
    pub(crate) job: CronJobId,
    pub(crate) fired_at: Timestamp,
    pub(crate) rules: Vec<PermissionRule>,
    pub(crate) attachments: Vec<RuntimeAttachment>,
}

/// Whether a pending call of `tool` creates a delegated child, which
/// delegation recovery resolves.
pub(crate) fn is_delegating_tool(tool: &str) -> bool {
    matches!(tool, "delegate_subagent" | "cron_run")
}

impl Engine {
    /// The cron job a pending `cron_run` call is admitting a child for.
    pub(super) fn cron_delegation(&self, call: ToolCallId) -> Option<CronDelegation> {
        self.inner
            .delegation
            .cron_runs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&call)
            .cloned()
    }

    /// How deep the children of a run of `policy` may go: its delegation
    /// policy's ceiling, or, for a cron job's run now by an agent that
    /// delegates to nothing, the configured depth.
    pub(super) fn delegation_ceiling(
        &self,
        policy: &crate::policy::FrozenRunPolicy,
        cron: bool,
    ) -> Result<u32, EngineError> {
        match (&policy.agent.delegation, cron) {
            (Some(delegation), _) => Ok(delegation.effective_depth_ceiling),
            (None, true) => Ok(self.inner.config.load().runtime.delegation.max_depth),
            (None, false) => Err(EngineError::ToolFailed("delegation is disabled".into())),
        }
    }

    /// Runs `cron_run` for job `id` from `context`'s session.
    pub(crate) async fn cron_run_tool(
        &self,
        context: ToolExecutionContext,
        call: ToolCallId,
        id: CronJobId,
    ) -> Result<ToolCompletion, ToolError> {
        let refused = |error: EngineError| ToolError::execution(error.to_string());
        let job = self
            .inner
            .crons
            .job(&id)
            .ok_or_else(|| ToolError::execution(format!("there is no cron job {id}")))?;
        let StoredTarget::New(template) = &job.target else {
            let result = self.run_cron_now(id.clone()).await.map_err(refused)?;
            let output = match result.session_id {
                Some(session) => {
                    let handle = self
                        .inner
                        .store
                        .summary(session)
                        .ok()
                        .and_then(|summary| summary.meta.short_id)
                        .unwrap_or_else(|| session.to_string());
                    format!("Fired cron job {id} into session {handle}.")
                }
                None => format!(
                    "Cron job {id} was skipped: its previous fire is still waiting in its session."
                ),
            };
            return Ok(ToolCompletion::single(
                cookie_agent_protocol::PersistedToolResult {
                    title: cookie_agent_runtime_api::authoring::safe_title(format!(
                        "Ran cron job {id}"
                    )),
                    display: crate::base_tools::markdown_display(&output),
                    output,
                    background: None,
                    retained_output: None,
                    metadata: serde_json::json!({}),
                    attachments: Vec::new(),
                    additional_messages: Vec::new(),
                },
            ));
        };
        self.check_cron_template(template)
            .map_err(|error| ToolError::execution(error.to_string()))?;
        let runtimes = super::attachments::SessionRuntimes {
            runtimes: template.runtimes.as_ref().map(|requests| {
                requests
                    .iter()
                    .map(|request| super::attachments::SessionRuntimeRequest {
                        runtime_id: request.runtime_id.to_string(),
                        capability: request.capability.as_ref().map(ToString::to_string),
                        workspace: request.workspace.as_ref().map(std::path::PathBuf::from),
                    })
                    .collect()
            }),
            global: template.global,
            cwd: template.cwd.as_ref().map(std::path::PathBuf::from),
        };
        let attachments = self
            .resolve_session_runtimes(&runtimes)
            .await
            .map_err(refused)?;
        let fired_at = Timestamp::now();
        self.inner
            .delegation
            .cron_runs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(
                call,
                CronDelegation {
                    job: id.clone(),
                    fired_at,
                    rules: template.permission_rules.clone(),
                    attachments,
                },
            );
        let invoked = self
            .delegate_invoke(DelegateInvocation {
                parent_session_id: context.session,
                parent_run_id: context.run,
                parent_tool_call_id: call,
                agent_type: template.agent.clone(),
                description: job.label(),
                prompt: super::cron_fire::render_scheduled_job(&job, fired_at),
                background: context.background,
                resume_session_id: None,
                inherit_context: false,
                capabilities: None,
            })
            .await;
        self.inner
            .delegation
            .cron_runs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&call);
        let handle = invoked.map_err(refused)?;
        self.record_cron_child(id, handle.child_session_id)
            .await
            .map_err(refused)?;
        // The child's run may have ended before its fire was recorded, when
        // nothing awaited it yet; its end is recorded now instead.
        if let Some(ended) = self.first_ended_run(handle.child_session_id) {
            self.settle_cron_runs(handle.child_session_id, ended)
                .await
                .map_err(refused)?;
        }
        let short_id = self.session_short_id(handle.child_session_id);
        context
            .detach
            .reached_with(crate::base_tools::delegate::subagent_detachable(
                handle.child_session_id,
                short_id,
            ));
        self.await_delegate(handle)
            .await
            .map(ToolCompletion::single)
            .map_err(refused)
    }
}

impl Engine {
    /// The earliest of `session`'s runs that has ended, if any. Run IDs are
    /// UUIDv7, so their text orders them by start.
    fn first_ended_run(&self, session: SessionId) -> Option<RunId> {
        let projection = self.inner.store.get(session).ok()?;
        projection
            .runs
            .values()
            .filter(|run| {
                matches!(
                    run.status,
                    SessionStatus::Completed
                        | SessionStatus::Failed
                        | SessionStatus::Cancelled
                        | SessionStatus::Interrupted
                )
            })
            .map(|run| run.id)
            .min_by_key(ToString::to_string)
    }
}
