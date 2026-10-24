use async_trait::async_trait;
use cookie_agent_engine::{EngineError, TailOwner};
use cookie_agent_protocol::CapabilityName;
use cookie_agent_protocol::{
    ApprovalListParams, ApprovalListResult, ApprovalRespondParams, ApprovalRespondResult,
    EventsSubscribeParams, EventsSubscribeResult, McpAuthBeginParams, McpAuthBeginResult,
    McpAuthCancelParams, McpAuthCancelResult, McpAuthCompleteParams, McpAuthCompleteResult,
    McpServerAddParams, McpServerEditParams, McpServerListParams, McpServerListResult,
    McpServerMutationResult, McpServerNameParams, McpServerPersistParams, McpServerReconnectParams,
    McpServerSetEnabledParams, ProviderConnectParams, ProviderConnectResult,
    ProviderDisconnectParams, ProviderDisconnectResult, RunCancelParams, RunCancelResult,
    RunStartParams, RunStartResult, RunSteerParams, RunSteerResult, RuntimeSnapshotGetParams,
    RuntimeSnapshotResult, ServerContext, ServerFault, ServerProtocol, SessionChildrenParams,
    SessionChildrenResult, SessionCompactParams, SessionCompactResult, SessionCreateParams,
    SessionCreateResult, SessionForkParams, SessionForkResult, SessionGetParams, SessionGetResult,
    SessionGoalGetParams, SessionGoalGetResult, SessionGoalLifecycleParams,
    SessionGoalLifecycleResult, SessionGoalSetParams, SessionGoalSetResult, SessionListParams,
    SessionListResult, SessionPermissionClearParams, SessionPermissionGetParams,
    SessionPermissionGetResult, SessionPermissionMutationResult, SessionPermissionPreviewParams,
    SessionPermissionPreviewResult, SessionPermissionSetParams, SessionProducersParams,
    SessionProducersResult, SessionRecallInputParams, SessionRecallInputResult,
    SessionRenameParams, SessionRenameResult, SessionResumeParams, SessionResumeResult,
    SessionRevertParams, SessionRevertResult, SessionSetPermissionModeParams,
    SessionSetPermissionModeResult, SessionSubmitInputParams, SessionSubmitInputResult,
    SessionTreeParams, SessionTreeResult, SessionTreeUsageResult, SessionUsageParams,
    SessionUsageResult, SkillsGetParams, SkillsGetResult, SkillsListParams, SkillsListResult,
};

use super::Server;
use crate::rpc::{RpcFault, engine_fault};

type Result<T> = std::result::Result<T, ServerFault>;

#[async_trait]
impl ServerProtocol for Server {
    async fn connected(&self, context: ServerContext) {
        self.start_runtime_notifications(context.clone());
        self.start_onboarding_notifications(context.clone());
        self.start_session_change_notifications(context.clone());
        self.start_mcp_change_notifications(context.clone());
        self.start_runtime_instance_notifications(context.clone());
        self.start_plugin_notifications(context.clone());
        self.start_machine_notifications(context.clone());
        self.start_memory_notifications(context.clone());
        self.start_project_notifications(context.clone());
        self.start_cron_notifications(context.clone());
        self.start_restart_notifications(context.clone());
        self.start_background_notifications(context.clone());
        self.start_upload_cleanup(&context);
        self.start_engine_event_notifications(context);
    }

    fn engine_version(&self) -> String {
        env!("CARGO_PKG_VERSION").to_owned()
    }

    async fn create_session(&self, params: SessionCreateParams) -> Result<SessionCreateResult> {
        let runtimes = cookie_agent_engine::SessionRuntimes {
            runtimes: params.runtimes.map(|requests| {
                requests
                    .into_iter()
                    .map(|request| cookie_agent_engine::SessionRuntimeRequest {
                        runtime_id: request.runtime_id.into_string(),
                        capability: request.capability.map(CapabilityName::into_string),
                        workspace: request.workspace.map(std::path::PathBuf::from),
                    })
                    .collect()
            }),
            global: params.global,
            cwd: params.cwd.map(std::path::PathBuf::from),
        };
        let session = self
            .engine
            .create_session_in_project(
                params.selection,
                params.permission_mode,
                params.permission_rules,
                runtimes,
                params.project,
            )
            .await
            .map_err(protocol_fault)?;
        let runtimes = self
            .engine
            .session_runtimes(session.session_id)
            .map_err(protocol_fault)?;
        Ok(SessionCreateResult { session, runtimes })
    }

    async fn list_sessions(&self, params: SessionListParams) -> Result<SessionListResult> {
        let sessions = self
            .engine
            .list_sessions()
            .into_iter()
            .filter(|session| {
                params
                    .cwd_identity
                    .as_ref()
                    .is_none_or(|cwd| &session.cwd_identity == cwd)
            })
            .filter(|session| {
                params
                    .project
                    .as_ref()
                    .is_none_or(|filter| filter.admits(session.project.as_ref()))
            })
            .collect();
        Ok(SessionListResult { sessions })
    }

    async fn get_session(&self, params: SessionGetParams) -> Result<SessionGetResult> {
        let session = self
            .engine
            .get_session(params.session_id)
            .map_err(protocol_fault)?;
        let runtimes = self
            .engine
            .session_runtimes(params.session_id)
            .map_err(protocol_fault)?;
        Ok(SessionGetResult { session, runtimes })
    }

    async fn get_session_goal(&self, params: SessionGoalGetParams) -> Result<SessionGoalGetResult> {
        self.engine
            .get_session_goal(params)
            .await
            .map_err(protocol_fault)
    }

    async fn set_session_goal(
        &self,
        params: SessionGoalSetParams,
        context: &ServerContext,
    ) -> Result<SessionGoalSetResult> {
        self.engine
            .set_session_goal(params, context.client_origin())
            .await
            .map_err(protocol_fault)
    }

    async fn change_session_goal_lifecycle(
        &self,
        params: SessionGoalLifecycleParams,
        context: &ServerContext,
    ) -> Result<SessionGoalLifecycleResult> {
        self.engine
            .change_session_goal_lifecycle(params, context.client_origin())
            .await
            .map_err(protocol_fault)
    }

    async fn session_producers(
        &self,
        params: SessionProducersParams,
    ) -> Result<SessionProducersResult> {
        self.engine
            .session_producers(params)
            .await
            .map_err(protocol_fault)
    }

    async fn session_usage(&self, params: SessionUsageParams) -> Result<SessionUsageResult> {
        self.engine
            .session_usage(params.session_id)
            .map_err(protocol_fault)
    }

    async fn session_tree_usage(
        &self,
        params: SessionUsageParams,
    ) -> Result<SessionTreeUsageResult> {
        self.engine
            .session_tree_usage(params.session_id)
            .map_err(|error| RpcFault::session_tree_usage(error).into())
    }

    async fn session_children(
        &self,
        params: SessionChildrenParams,
    ) -> Result<SessionChildrenResult> {
        Ok(SessionChildrenResult {
            children: self
                .engine
                .children(params.session_id)
                .map_err(protocol_fault)?,
        })
    }

    async fn session_tree(&self, params: SessionTreeParams) -> Result<SessionTreeResult> {
        self.engine
            .tree(params.session_id)
            .map(|tree| SessionTreeResult { tree })
            .map_err(protocol_fault)
    }

    async fn resume_session(&self, params: SessionResumeParams) -> Result<SessionResumeResult> {
        let session = self
            .engine
            .resume(params.session_id)
            .await
            .map_err(protocol_fault)?;
        let runtimes = self
            .engine
            .session_runtimes(params.session_id)
            .map_err(protocol_fault)?;
        Ok(SessionResumeResult { session, runtimes })
    }

    async fn rename_session(
        &self,
        params: SessionRenameParams,
        context: &ServerContext,
    ) -> Result<SessionRenameResult> {
        self.engine
            .rename_session(params.clone(), context.client_origin())
            .await
            .map_err(|error| rename_fault(&params, error).into())
    }

    async fn set_permission_mode(
        &self,
        params: SessionSetPermissionModeParams,
        context: &ServerContext,
    ) -> Result<SessionSetPermissionModeResult> {
        self.engine
            .set_permission_mode(params.session_id, params.mode, context.client_origin())
            .map_err(protocol_fault)?;
        Ok(SessionSetPermissionModeResult {})
    }

    async fn get_session_permissions(
        &self,
        params: SessionPermissionGetParams,
    ) -> Result<SessionPermissionGetResult> {
        self.engine
            .get_session_permissions(params.session_id)
            .map_err(protocol_fault)
    }

    async fn set_session_permission(
        &self,
        params: SessionPermissionSetParams,
        context: &ServerContext,
    ) -> Result<SessionPermissionMutationResult> {
        self.engine
            .set_session_permission(
                params.session_id,
                &params.action,
                params.runtime,
                params.resource,
                params.effect,
                context.client_origin(),
            )
            .await
            .map_err(protocol_fault)
    }

    async fn clear_session_permission(
        &self,
        params: SessionPermissionClearParams,
        context: &ServerContext,
    ) -> Result<SessionPermissionMutationResult> {
        self.engine
            .clear_session_permission(
                params.session_id,
                &params.action,
                params.runtime,
                &params.resource,
                context.client_origin(),
            )
            .await
            .map_err(protocol_fault)
    }

    async fn preview_session_permissions(
        &self,
        params: SessionPermissionPreviewParams,
    ) -> Result<SessionPermissionPreviewResult> {
        self.engine
            .preview_session_permissions(&params.selection, params.rules)
            .await
            .map(|permissions| SessionPermissionPreviewResult { permissions })
            .map_err(protocol_fault)
    }

    async fn list_skills(&self, params: SkillsListParams) -> Result<SkillsListResult> {
        match params.session_id {
            Some(session_id) => self.engine.list_skills(session_id).map_err(protocol_fault),
            None => Ok(self.engine.list_daemon_skills()),
        }
    }

    async fn update_skills(
        &self,
        _params: cookie_agent_protocol::SkillsUpdateParams,
    ) -> Result<cookie_agent_protocol::SkillsUpdateResult> {
        let report = self.engine.refresh_official_skills().await;
        Ok(cookie_agent_protocol::SkillsUpdateResult {
            enabled: report.enabled,
            skills: report
                .skills
                .into_iter()
                .map(|(name, version)| cookie_agent_protocol::OfficialSkill { name, version })
                .collect(),
            updated: report.updated,
            removed: report.removed,
            error: report.error,
        })
    }

    async fn get_skill(&self, params: SkillsGetParams) -> Result<SkillsGetResult> {
        self.engine
            .get_skill(params.session_id, &params.name, &params.args)
            .map_err(protocol_fault)
    }

    async fn compact_session(
        &self,
        params: SessionCompactParams,
        context: &ServerContext,
    ) -> Result<SessionCompactResult> {
        self.engine
            .compact_session_result(
                params.session_id,
                params.focus.as_ref().map(|focus| focus.as_str()),
                context.client_origin(),
            )
            .await
            .map_err(protocol_fault)
    }

    async fn revert_session(
        &self,
        params: SessionRevertParams,
        context: &ServerContext,
    ) -> Result<SessionRevertResult> {
        self.engine
            .revert_session(
                params.session_id,
                params.through_seq,
                context.client_origin(),
                Some(params.expected_last_seq),
            )
            .await
            .map_err(protocol_fault)
    }

    async fn delete_session(
        &self,
        params: cookie_agent_protocol::SessionDeleteParams,
        _context: &ServerContext,
    ) -> Result<cookie_agent_protocol::SessionDeleteResult> {
        self.engine
            .delete_session(params.session_id, params.expected_last_seq)
            .await
            .map(|deleted| cookie_agent_protocol::SessionDeleteResult { deleted })
            .map_err(protocol_fault)
    }

    async fn cancel_compaction(
        &self,
        params: cookie_agent_protocol::SessionCompactCancelParams,
        _context: &ServerContext,
    ) -> Result<cookie_agent_protocol::SessionCompactCancelResult> {
        Ok(cookie_agent_protocol::SessionCompactCancelResult {
            cancelled: self.engine.cancel_compaction(params.session_id),
        })
    }

    async fn fork_session(
        &self,
        params: SessionForkParams,
        context: &ServerContext,
    ) -> Result<SessionForkResult> {
        self.engine
            .fork_session(
                params.session_id,
                params.through_seq,
                context.client_origin(),
                Some(params.client_fork_id),
            )
            .await
            .map_err(protocol_fault)
    }

    async fn start_run(
        &self,
        params: RunStartParams,
        context: &ServerContext,
    ) -> Result<RunStartResult> {
        match self
            .engine
            .start_run(params.clone(), context.client_origin())
            .await
        {
            Ok(result) => Ok(result),
            Err(EngineError::RunIdempotencyConflict) => Err(RpcFault::run_idempotency_conflict(
                params.session_id,
                &params.client_run_id,
            )
            .into()),
            Err(EngineError::SessionRunning(_)) => {
                Err(RpcFault::session_running(params.session_id, &params.client_run_id).into())
            }
            Err(error) => Err(engine_fault(error).into()),
        }
    }

    async fn steer_run(
        &self,
        params: RunSteerParams,
        context: &ServerContext,
    ) -> Result<RunSteerResult> {
        self.engine
            .steer(params.run_id, params.content, context.client_origin())
            .await
            .map_err(protocol_fault)
    }

    async fn submit_input(
        &self,
        params: SessionSubmitInputParams,
        context: &ServerContext,
    ) -> Result<SessionSubmitInputResult> {
        let session_id = params.session_id;
        let client_run_id = params.client_run_id.clone();
        match self
            .engine
            .submit_input(params, context.client_origin())
            .await
        {
            Ok(result) => Ok(result),
            Err(EngineError::RunIdempotencyConflict) => {
                Err(RpcFault::run_idempotency_conflict(session_id, &client_run_id).into())
            }
            Err(EngineError::SessionRunning(_)) => {
                Err(RpcFault::session_running(session_id, &client_run_id).into())
            }
            Err(error) => Err(engine_fault(error).into()),
        }
    }

    async fn recall_input(
        &self,
        params: SessionRecallInputParams,
        context: &ServerContext,
    ) -> Result<SessionRecallInputResult> {
        self.engine
            .recall_input(params.session_id, params.seq, context.client_origin())
            .await
            .map_err(protocol_fault)
    }

    async fn cancel_run(
        &self,
        params: RunCancelParams,
        context: &ServerContext,
    ) -> Result<RunCancelResult> {
        self.engine
            .cancel_run(params.run_id, context.client_origin())
            .await
            .map_err(protocol_fault)
    }

    async fn subscribe_events(
        &self,
        params: EventsSubscribeParams,
        context: &ServerContext,
    ) -> Result<EventsSubscribeResult> {
        let (result, receiver) = match self
            .engine
            .subscribe_page(
                params.session_id,
                params.cursor,
                params.cursor_timestamp,
                params.limit,
                // One tail per session per connection: a re-subscribe (a
                // reselected session, a recovery replay) replaces the tail
                // rather than adding a second copy of every live message.
                Some(TailOwner(context.connection_id())),
            )
            .await
        {
            Ok(subscription) => subscription,
            Err(EngineError::SessionOwnedByAnotherProcess(_)) => {
                // The protocol has no separate snapshot RPC. Return only the replay
                // half of this response and deliberately register no live tail.
                return self
                    .engine
                    .snapshot_events(
                        params.session_id,
                        params.cursor,
                        params.cursor_timestamp,
                        params.limit,
                    )
                    .map_err(protocol_fault);
            }
            Err(error) => return Err(protocol_fault(error)),
        };
        // A non-final page is only history; the final page goes live.
        let Some(receiver) = receiver else {
            return Ok(result);
        };
        context.register_session_subscription(params.session_id);
        context.forward_session_tail(params.session_id, receiver);
        self.replay_connection_failures(context, params.session_id);
        Ok(result)
    }

    async fn select_model(
        &self,
        params: cookie_agent_protocol::SessionSelectModelParams,
        context: &ServerContext,
    ) -> Result<cookie_agent_protocol::SessionSelectModelResult> {
        self.engine
            .select_model(params.session_id, params.selection, context.client_origin())
            .await
            .map(|changed| cookie_agent_protocol::SessionSelectModelResult { changed })
            .map_err(protocol_fault)
    }

    async fn unsubscribe_events(
        &self,
        params: cookie_agent_protocol::EventsUnsubscribeParams,
        context: &ServerContext,
    ) -> Result<cookie_agent_protocol::EventsUnsubscribeResult> {
        // The forwarder goes first so nothing more for the session reaches
        // the connection, then the engine forgets the tail.
        let forwarded = context.unregister_session_subscription(params.session_id);
        let tailed = self
            .engine
            .unsubscribe(params.session_id, TailOwner(context.connection_id()));
        Ok(cookie_agent_protocol::EventsUnsubscribeResult {
            unsubscribed: forwarded || tailed,
        })
    }

    async fn respond_approval(
        &self,
        params: ApprovalRespondParams,
        context: &ServerContext,
    ) -> Result<ApprovalRespondResult> {
        self.engine
            .approval_respond(params.clone(), context.client_origin())
            .await
            .map_err(|error| approval_fault(&params, error).into())
    }

    async fn list_approvals(&self, params: ApprovalListParams) -> Result<ApprovalListResult> {
        Ok(self
            .engine
            .list_approvals(params.root_session_id, params.status))
    }

    async fn respond_elicitation(
        &self,
        params: cookie_agent_protocol::ElicitationRespondParams,
    ) -> Result<cookie_agent_protocol::ElicitationRespondResult> {
        self.engine
            .respond_elicitation(params)
            .await
            .map_err(protocol_fault)
    }

    async fn begin_mcp_auth(&self, params: McpAuthBeginParams) -> Result<McpAuthBeginResult> {
        let authorization_url = self
            .engine
            .begin_mcp_auth(params.server.clone(), params.redirect_uri)
            .await
            .map_err(protocol_fault)?;
        Ok(McpAuthBeginResult {
            server: params.server,
            authorization_url,
        })
    }

    async fn complete_mcp_auth(
        &self,
        params: McpAuthCompleteParams,
    ) -> Result<McpAuthCompleteResult> {
        self.engine
            .complete_mcp_auth(params.server.clone(), params.callback_url)
            .await
            .map_err(protocol_fault)?;
        Ok(McpAuthCompleteResult {
            server: params.server,
        })
    }

    async fn cancel_mcp_auth(&self, params: McpAuthCancelParams) -> Result<McpAuthCancelResult> {
        self.engine
            .cancel_mcp_auth(params.server.clone())
            .await
            .map_err(protocol_fault)?;
        Ok(McpAuthCancelResult {
            server: params.server,
        })
    }

    async fn attach_runtime(
        &self,
        params: cookie_agent_protocol::SessionRuntimeAttachParams,
        context: &ServerContext,
    ) -> Result<cookie_agent_protocol::SessionRuntimeAttachResult> {
        let attachment = self
            .engine
            .attach_runtime(
                params.session_id,
                params.runtime_id.as_str(),
                params.capability.as_ref().map(CapabilityName::as_str),
                params.workspace.map(std::path::PathBuf::from),
                context.client_origin(),
            )
            .await
            .map_err(protocol_fault)?;
        let runtimes = self
            .engine
            .session_runtimes(params.session_id)
            .map_err(protocol_fault)?;
        Ok(cookie_agent_protocol::SessionRuntimeAttachResult {
            attachment,
            runtimes,
        })
    }

    async fn detach_runtime(
        &self,
        params: cookie_agent_protocol::SessionRuntimeDetachParams,
        context: &ServerContext,
    ) -> Result<cookie_agent_protocol::SessionRuntimeDetachResult> {
        self.engine
            .detach_runtime(params.session_id, params.name, context.client_origin())
            .await
            .map_err(protocol_fault)?;
        let runtimes = self
            .engine
            .session_runtimes(params.session_id)
            .map_err(protocol_fault)?;
        Ok(cookie_agent_protocol::SessionRuntimeDetachResult { runtimes })
    }

    async fn list_runtimes(
        &self,
        _params: cookie_agent_protocol::RuntimesListParams,
    ) -> Result<cookie_agent_protocol::RuntimesListResult> {
        Ok(crate::runtimes::runtimes_list(&self.engine))
    }

    async fn list_machines(
        &self,
        _params: cookie_agent_protocol::MachinesListParams,
    ) -> Result<cookie_agent_protocol::MachinesListResult> {
        Ok(self.machines_list())
    }

    async fn rename_machine(
        &self,
        params: cookie_agent_protocol::MachinesRenameParams,
    ) -> Result<cookie_agent_protocol::MachinesListResult> {
        self.engine
            .rename_machine(params.machine, params.name)
            .await
            .map_err(protocol_fault)?;
        Ok(self.machines_list())
    }

    async fn assign_machine(
        &self,
        params: cookie_agent_protocol::MachinesAssignParams,
    ) -> Result<cookie_agent_protocol::MachinesListResult> {
        self.engine
            .assign_machine(params.runtime, params.machine)
            .await
            .map_err(protocol_fault)?;
        Ok(self.machines_list())
    }

    async fn unassign_machine(
        &self,
        params: cookie_agent_protocol::MachinesUnassignParams,
    ) -> Result<cookie_agent_protocol::MachinesListResult> {
        self.engine
            .unassign_machine(params.runtime)
            .await
            .map_err(protocol_fault)?;
        Ok(self.machines_list())
    }

    async fn merge_machines(
        &self,
        params: cookie_agent_protocol::MachinesMergeParams,
    ) -> Result<cookie_agent_protocol::MachinesListResult> {
        self.engine
            .merge_machines(params.from, params.into)
            .await
            .map_err(protocol_fault)?;
        Ok(self.machines_list())
    }

    async fn forget_machine(
        &self,
        params: cookie_agent_protocol::MachinesForgetParams,
    ) -> Result<cookie_agent_protocol::MachinesListResult> {
        self.engine
            .forget_machine(params.machine)
            .await
            .map_err(protocol_fault)?;
        Ok(self.machines_list())
    }

    async fn forget_machine_runtime(
        &self,
        params: cookie_agent_protocol::MachinesForgetRuntimeParams,
    ) -> Result<cookie_agent_protocol::MachinesListResult> {
        self.engine
            .forget_runtime_record(params.runtime)
            .await
            .map_err(protocol_fault)?;
        Ok(self.machines_list())
    }

    async fn list_projects(
        &self,
        _params: cookie_agent_protocol::ProjectsListParams,
    ) -> Result<cookie_agent_protocol::ProjectsListResult> {
        self.engine
            .list_projects()
            .await
            .map(|projects| cookie_agent_protocol::ProjectsListResult { projects })
            .map_err(protocol_fault)
    }

    async fn get_project(
        &self,
        params: cookie_agent_protocol::ProjectGetParams,
    ) -> Result<cookie_agent_protocol::ProjectGetResult> {
        self.engine
            .get_project(params.project)
            .await
            .map_err(protocol_fault)
    }

    async fn create_project(
        &self,
        params: cookie_agent_protocol::ProjectCreateParams,
    ) -> Result<cookie_agent_protocol::ProjectResult> {
        self.engine
            .create_project(params)
            .await
            .map(|project| cookie_agent_protocol::ProjectResult { project })
            .map_err(protocol_fault)
    }

    async fn update_project(
        &self,
        params: cookie_agent_protocol::ProjectUpdateParams,
    ) -> Result<cookie_agent_protocol::ProjectResult> {
        self.engine
            .update_project(params)
            .await
            .map(|project| cookie_agent_protocol::ProjectResult { project })
            .map_err(protocol_fault)
    }

    async fn delete_project(
        &self,
        params: cookie_agent_protocol::ProjectDeleteParams,
    ) -> Result<cookie_agent_protocol::ProjectDeleteResult> {
        self.engine
            .delete_project(params)
            .await
            .map_err(protocol_fault)
    }

    async fn set_session_project(
        &self,
        params: cookie_agent_protocol::SessionProjectSetParams,
    ) -> Result<cookie_agent_protocol::SessionProjectSetResult> {
        self.engine
            .set_session_project(params.session_id, params.project)
            .await
            .map(|session| cookie_agent_protocol::SessionProjectSetResult { session })
            .map_err(protocol_fault)
    }

    async fn set_session_agent_option(
        &self,
        params: cookie_agent_protocol::SessionAgentOptionSetParams,
    ) -> Result<cookie_agent_protocol::SessionAgentOptionSetResult> {
        self.engine
            .set_session_agent_option(params)
            .await
            .map_err(protocol_fault)
    }

    async fn list_crons(
        &self,
        _params: cookie_agent_protocol::CronsListParams,
    ) -> Result<cookie_agent_protocol::CronsListResult> {
        Ok(self.engine.list_crons())
    }

    async fn create_cron(
        &self,
        params: cookie_agent_protocol::CronCreateParams,
    ) -> Result<cookie_agent_protocol::CronJobResult> {
        self.engine
            .create_cron(params)
            .await
            .map(|job| cookie_agent_protocol::CronJobResult { job })
            .map_err(protocol_fault)
    }

    async fn update_cron(
        &self,
        params: cookie_agent_protocol::CronUpdateParams,
    ) -> Result<cookie_agent_protocol::CronJobResult> {
        self.engine
            .update_cron(params)
            .await
            .map(|job| cookie_agent_protocol::CronJobResult { job })
            .map_err(protocol_fault)
    }

    async fn delete_cron(
        &self,
        params: cookie_agent_protocol::CronDeleteParams,
    ) -> Result<cookie_agent_protocol::CronDeleteResult> {
        self.engine
            .delete_cron(params.id)
            .await
            .map(|()| cookie_agent_protocol::CronDeleteResult {})
            .map_err(protocol_fault)
    }

    async fn run_cron(
        &self,
        params: cookie_agent_protocol::CronRunParams,
    ) -> Result<cookie_agent_protocol::CronRunResult> {
        self.engine
            .run_cron(params.id)
            .await
            .map_err(protocol_fault)
    }

    async fn list_memory(
        &self,
        params: cookie_agent_protocol::MemoryListParams,
    ) -> Result<cookie_agent_protocol::MemoryListResult> {
        self.engine
            .list_memory(params)
            .await
            .map_err(protocol_fault)
    }

    async fn read_memory(
        &self,
        params: cookie_agent_protocol::MemoryReadParams,
    ) -> Result<cookie_agent_protocol::MemoryReadResult> {
        self.engine
            .read_memory(params)
            .await
            .map_err(protocol_fault)
    }

    async fn write_memory(
        &self,
        params: cookie_agent_protocol::MemoryWriteParams,
    ) -> Result<cookie_agent_protocol::MemoryWriteResult> {
        self.engine
            .write_memory(params)
            .await
            .map_err(protocol_fault)
    }

    async fn delete_memory(
        &self,
        params: cookie_agent_protocol::MemoryDeleteParams,
    ) -> Result<cookie_agent_protocol::MemoryDeleteResult> {
        self.engine
            .delete_memory(params)
            .await
            .map_err(protocol_fault)?;
        Ok(cookie_agent_protocol::MemoryDeleteResult {})
    }

    async fn list_plugins(
        &self,
        _params: cookie_agent_protocol::PluginsListParams,
    ) -> Result<cookie_agent_protocol::PluginsListResult> {
        Ok(cookie_agent_protocol::PluginsListResult {
            plugins: self.engine.plugin_statuses(),
        })
    }

    async fn restart_plugin(
        &self,
        params: cookie_agent_protocol::PluginsRestartParams,
    ) -> Result<cookie_agent_protocol::PluginsRestartResult> {
        self.engine
            .restart_plugin(&params.name)
            .map_err(protocol_fault)?;
        Ok(cookie_agent_protocol::PluginsRestartResult {})
    }

    async fn get_config(
        &self,
        _params: cookie_agent_protocol::ConfigGetParams,
    ) -> Result<cookie_agent_protocol::ConfigGetResult> {
        let documents = self
            .engine
            .configuration_documents()
            .await
            .map_err(protocol_fault)?;
        Ok(cookie_agent_protocol::ConfigGetResult {
            revision: documents.revision,
            documents: documents
                .documents
                .into_iter()
                .map(|document| cookie_agent_protocol::ConfigDocument {
                    path: document.path.as_str().to_owned(),
                    text: document.text,
                    fingerprint: document.fingerprint,
                })
                .collect(),
            empty_presets: documents.empty_presets,
            status: crate::config::status_to_wire(self.engine.configuration_status()),
        })
    }

    async fn apply_config(
        &self,
        params: cookie_agent_protocol::ConfigApplyParams,
    ) -> Result<cookie_agent_protocol::ConfigApplyResult> {
        let changes = params
            .changes
            .into_iter()
            .map(crate::config::change_from_wire)
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|error| ServerFault::from(RpcFault::config_apply(error)))?;
        let revision = self
            .engine
            .apply_config_changes(params.expected_revision, changes)
            .await
            .map_err(|error| ServerFault::from(RpcFault::config_apply(error)))?;
        Ok(cookie_agent_protocol::ConfigApplyResult {
            revision,
            status: crate::config::status_to_wire(self.engine.configuration_status()),
        })
    }

    async fn reload_config(
        &self,
        _params: cookie_agent_protocol::ConfigReloadParams,
    ) -> Result<cookie_agent_protocol::ConfigReloadResult> {
        // A configuration that fails validation is reported in the status;
        // the daemon keeps running on the applied one.
        match self.engine.reload_configuration().await {
            Ok(_) | Err(cookie_agent_engine::EngineError::Config(_)) => {}
            Err(error) => return Err(protocol_fault(error)),
        }
        Ok(cookie_agent_protocol::ConfigReloadResult {
            status: crate::config::status_to_wire(self.engine.configuration_status()),
        })
    }

    async fn restart_daemon(
        &self,
        params: cookie_agent_protocol::DaemonRestartParams,
    ) -> Result<cookie_agent_protocol::DaemonRestartResult> {
        let scheduled = self
            .engine
            .schedule_restart(params.when)
            .map_err(protocol_fault)?;
        Ok(cookie_agent_protocol::DaemonRestartResult {
            scheduled: scheduled.scheduled,
            active_runs: scheduled.active_runs,
        })
    }

    async fn get_onboarding(
        &self,
        _params: cookie_agent_protocol::OnboardingGetParams,
    ) -> Result<cookie_agent_protocol::OnboardingStatus> {
        Ok(crate::onboarding::status_to_wire(
            self.engine.onboarding_state(),
        ))
    }

    async fn preview_onboarding(
        &self,
        params: cookie_agent_protocol::OnboardingPreviewParams,
    ) -> Result<cookie_agent_protocol::OnboardingPreviewResult> {
        let source = crate::onboarding::source_from_wire(params.source).map_err(protocol_fault)?;
        let preview = self
            .engine
            .preview_kit(source)
            .await
            .map_err(protocol_fault)?;
        Ok(crate::onboarding::preview_to_wire(preview))
    }

    async fn run_onboarding(
        &self,
        params: cookie_agent_protocol::OnboardingRunParams,
    ) -> Result<cookie_agent_protocol::OnboardingRunResult> {
        let request = crate::onboarding::request_from_wire(params).map_err(protocol_fault)?;
        let outcome = self
            .engine
            .run_onboarding(request)
            .await
            .map_err(|error| match error {
                // A model value the configuration refuses, as `config.apply`
                // refuses it.
                error @ (cookie_agent_engine::EngineError::ConfigConflict(_)
                | cookie_agent_engine::EngineError::ConfigChangeRefused(_)
                | cookie_agent_engine::EngineError::Config(_)) => {
                    ServerFault::from(RpcFault::config_apply(error))
                }
                error => protocol_fault(error),
            })?;
        Ok(crate::onboarding::outcome_to_wire(outcome))
    }

    async fn reconnect_runtime(
        &self,
        params: cookie_agent_protocol::RuntimesReconnectParams,
    ) -> Result<cookie_agent_protocol::RuntimesReconnectResult> {
        let instance = self
            .engine
            .reconnect_runtime(
                params.runtime_id.as_str(),
                params.capability.as_ref().map(CapabilityName::as_str),
                params.workspace.map(std::path::PathBuf::from),
            )
            .await
            .map_err(protocol_fault)?;
        let instance = crate::runtimes::instance_info(instance).ok_or_else(|| {
            protocol_fault(cookie_agent_engine::EngineError::Runtime(
                "the runtime instance reported an invalid identity".into(),
            ))
        })?;
        Ok(cookie_agent_protocol::RuntimesReconnectResult { instance })
    }

    async fn background_tool(
        &self,
        params: cookie_agent_protocol::RunToolBackgroundParams,
        _context: &ServerContext,
    ) -> Result<cookie_agent_protocol::RunToolBackgroundResult> {
        Ok(cookie_agent_protocol::RunToolBackgroundResult {
            accepted: self.engine.request_detach(params.run_id, params.call_id),
        })
    }

    async fn list_background(
        &self,
        params: cookie_agent_protocol::SessionBackgroundListParams,
    ) -> Result<cookie_agent_protocol::SessionBackgroundListResult> {
        Ok(cookie_agent_protocol::SessionBackgroundListResult {
            calls: self.engine.background_calls(params.session_id),
        })
    }

    async fn stop_background(
        &self,
        params: cookie_agent_protocol::SessionBackgroundStopParams,
        _context: &ServerContext,
    ) -> Result<cookie_agent_protocol::SessionBackgroundStopResult> {
        self.engine
            .stop_background_as_user(params.session_id, &params.job)
            .await
            .map_err(protocol_fault)?;
        Ok(cookie_agent_protocol::SessionBackgroundStopResult {})
    }

    async fn list_mcp_servers(&self, _: McpServerListParams) -> Result<McpServerListResult> {
        Ok(McpServerListResult {
            servers: self.engine.list_mcp_servers(),
        })
    }

    async fn add_mcp_server(&self, params: McpServerAddParams) -> Result<McpServerMutationResult> {
        self.engine
            .add_mcp_server(params.name, params.definition)
            .await
            .map(|server| McpServerMutationResult { server })
            .map_err(protocol_fault)
    }

    async fn edit_mcp_server(
        &self,
        params: McpServerEditParams,
    ) -> Result<McpServerMutationResult> {
        self.engine
            .edit_mcp_server(params.name, params.definition)
            .await
            .map(|server| McpServerMutationResult { server })
            .map_err(protocol_fault)
    }

    async fn remove_mcp_server(
        &self,
        params: McpServerNameParams,
    ) -> Result<McpServerMutationResult> {
        self.engine
            .remove_mcp_server(params.name)
            .await
            .map(|server| McpServerMutationResult { server })
            .map_err(protocol_fault)
    }

    async fn set_mcp_server_enabled(
        &self,
        params: McpServerSetEnabledParams,
    ) -> Result<McpServerMutationResult> {
        self.engine
            .set_mcp_server_enabled(params.name, params.enabled)
            .await
            .map(|server| McpServerMutationResult { server })
            .map_err(protocol_fault)
    }

    async fn reconnect_mcp_server(
        &self,
        params: McpServerReconnectParams,
    ) -> Result<McpServerMutationResult> {
        let runtime_id = params
            .runtime_id
            .as_ref()
            .map_or(cookie_agent_protocol::RuntimeId::GLOBAL, |id| id.as_str());
        self.engine
            .reconnect_runtime_mcp_server(runtime_id, params.workspace.as_deref(), params.name)
            .await
            .map(|server| McpServerMutationResult { server })
            .map_err(protocol_fault)
    }

    async fn persist_mcp_server(
        &self,
        params: McpServerPersistParams,
    ) -> Result<McpServerMutationResult> {
        self.engine
            .persist_mcp_server(params.name, params.target)
            .await
            .map(|server| McpServerMutationResult { server })
            .map_err(protocol_fault)
    }

    async fn begin_upload(
        &self,
        params: cookie_agent_protocol::ArtifactUploadBeginParams,
        context: &ServerContext,
    ) -> Result<cookie_agent_protocol::ArtifactUploadBeginResult> {
        let upload_id = self
            .engine
            .begin_upload(
                context.connection_id(),
                params.session_id,
                params.byte_length,
                params.filename,
            )
            .await
            .map_err(protocol_fault)?;
        Ok(cookie_agent_protocol::ArtifactUploadBeginResult {
            upload_id,
            chunk_bytes: cookie_agent_protocol::ARTIFACT_CHUNK_BYTES,
        })
    }

    async fn upload_chunk(
        &self,
        params: cookie_agent_protocol::ArtifactUploadChunkParams,
        context: &ServerContext,
    ) -> Result<cookie_agent_protocol::ArtifactUploadChunkResult> {
        use base64::Engine as _;
        let data = base64::engine::general_purpose::STANDARD
            .decode(params.data.as_bytes())
            .map_err(|error| ServerFault::invalid_params(format!("data is not base64: {error}")))?;
        self.engine
            .upload_chunk(
                context.connection_id(),
                &params.upload_id,
                params.offset,
                data,
            )
            .await
            .map_err(protocol_fault)?;
        Ok(cookie_agent_protocol::ArtifactUploadChunkResult {})
    }

    async fn finish_upload(
        &self,
        params: cookie_agent_protocol::ArtifactUploadFinishParams,
        context: &ServerContext,
    ) -> Result<cookie_agent_protocol::ArtifactUploadFinishResult> {
        self.engine
            .finish_upload(context.connection_id(), &params.upload_id, &params.sha256)
            .await
            .map_err(protocol_fault)
    }

    async fn cancel_upload(
        &self,
        params: cookie_agent_protocol::ArtifactUploadCancelParams,
        context: &ServerContext,
    ) -> Result<cookie_agent_protocol::ArtifactUploadCancelResult> {
        self.engine
            .cancel_upload(context.connection_id(), &params.upload_id)
            .map_err(protocol_fault)?;
        Ok(cookie_agent_protocol::ArtifactUploadCancelResult {})
    }

    async fn read_artifact(
        &self,
        params: cookie_agent_protocol::ArtifactReadParams,
    ) -> Result<cookie_agent_protocol::ArtifactReadResult> {
        use base64::Engine as _;
        let (data, total_bytes, media_type) = self
            .engine
            .read_artifact_range(
                params.session_id,
                &params.sha256,
                params.offset,
                params.length,
            )
            .await
            .map_err(protocol_fault)?;
        Ok(cookie_agent_protocol::ArtifactReadResult {
            data: base64::engine::general_purpose::STANDARD.encode(data),
            total_bytes,
            media_type,
        })
    }

    async fn runtime_snapshot(&self, _: RuntimeSnapshotGetParams) -> Result<RuntimeSnapshotResult> {
        self.engine.runtime_snapshot().map_err(protocol_fault)
    }

    async fn connect_provider(
        &self,
        params: ProviderConnectParams,
    ) -> Result<ProviderConnectResult> {
        // Provider connect takes the cross-process store lock; keep the
        // synchronous disk work off the async worker.
        let server = self.clone();
        tokio::task::spawn_blocking(move || Server::connect_provider(&server, params))
            .await
            .map_err(|_| ServerFault::from(RpcFault::internal()))?
            .map_err(Into::into)
    }

    async fn disconnect_provider(
        &self,
        params: ProviderDisconnectParams,
    ) -> Result<ProviderDisconnectResult> {
        // Provider disconnect takes the cross-process store lock; keep the
        // synchronous disk work off the async worker.
        let server = self.clone();
        tokio::task::spawn_blocking(move || Server::disconnect_provider(&server, params))
            .await
            .map_err(|_| ServerFault::from(RpcFault::internal()))?
            .map_err(Into::into)
    }
}

impl Server {
    /// Drops the connection's uploads in progress once it closes.
    fn start_upload_cleanup(&self, context: &ServerContext) {
        let engine = self.engine.clone();
        let connection = context.connection_id();
        let shutdown = context.shutdown();
        tokio::spawn(async move {
            shutdown.cancelled().await;
            engine.drop_connection_uploads(connection);
        });
    }
}

fn protocol_fault(error: EngineError) -> ServerFault {
    engine_fault(error).into()
}

fn rename_fault(request: &SessionRenameParams, error: EngineError) -> RpcFault {
    RpcFault::rename(request, error)
}

fn approval_fault(request: &ApprovalRespondParams, error: EngineError) -> RpcFault {
    RpcFault::approval(request, error)
}
