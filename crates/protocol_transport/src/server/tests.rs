use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::sync::{Notify, mpsc};
use tokio_util::sync::CancellationToken;

use super::*;

/// A transport whose far end is driven by the test.
struct ChannelTransport {
    incoming: mpsc::UnboundedReceiver<MessageFrame>,
    outgoing: mpsc::UnboundedSender<MessageFrame>,
}

#[async_trait]
impl Transport for ChannelTransport {
    async fn send(&mut self, frame: MessageFrame) -> Result<(), TransportError> {
        self.outgoing
            .send(frame)
            .map_err(|_| TransportError::Closed)
    }

    async fn recv(&mut self) -> Result<Option<MessageFrame>, TransportError> {
        Ok(self.incoming.recv().await)
    }
}

/// `session.get` blocks until released; `session.list` answers at once;
/// `events.subscribe` starts a tail that notifies immediately, then takes a
/// while to build its own response.
#[derive(Default)]
struct StubServer {
    release_get: Notify,
}

#[async_trait]
impl ServerProtocol for StubServer {
    async fn list_sessions(
        &self,
        _: cookie_agent_protocol::SessionListParams,
    ) -> Result<cookie_agent_protocol::SessionListResult, ServerFault> {
        Ok(cookie_agent_protocol::SessionListResult {
            sessions: Vec::new(),
        })
    }
    async fn get_session(
        &self,
        _: cookie_agent_protocol::SessionGetParams,
    ) -> Result<cookie_agent_protocol::SessionGetResult, ServerFault> {
        self.release_get.notified().await;
        Err(ServerFault::internal())
    }
    async fn subscribe_events(
        &self,
        _: cookie_agent_protocol::EventsSubscribeParams,
        context: &ServerContext,
    ) -> Result<cookie_agent_protocol::EventsSubscribeResult, ServerFault> {
        let context = context.clone();
        tokio::spawn(async move {
            let _ = context.notify("stub.tail", &json!({})).await;
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        Ok(cookie_agent_protocol::EventsSubscribeResult {
            events: Vec::new(),
            has_more: false,
            stale_cursor: false,
        })
    }
    async fn create_session(
        &self,
        _: cookie_agent_protocol::SessionCreateParams,
    ) -> Result<cookie_agent_protocol::SessionCreateResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn get_session_goal(
        &self,
        _: cookie_agent_protocol::SessionGoalGetParams,
    ) -> Result<cookie_agent_protocol::SessionGoalGetResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn set_session_goal(
        &self,
        _: cookie_agent_protocol::SessionGoalSetParams,
        _context: &ServerContext,
    ) -> Result<cookie_agent_protocol::SessionGoalSetResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn change_session_goal_lifecycle(
        &self,
        _: cookie_agent_protocol::SessionGoalLifecycleParams,
        _context: &ServerContext,
    ) -> Result<cookie_agent_protocol::SessionGoalLifecycleResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn session_producers(
        &self,
        _: cookie_agent_protocol::SessionProducersParams,
    ) -> Result<cookie_agent_protocol::SessionProducersResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn session_usage(
        &self,
        _: cookie_agent_protocol::SessionUsageParams,
    ) -> Result<cookie_agent_protocol::SessionUsageResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn session_tree_usage(
        &self,
        _: cookie_agent_protocol::SessionUsageParams,
    ) -> Result<cookie_agent_protocol::SessionTreeUsageResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn session_children(
        &self,
        _: cookie_agent_protocol::SessionChildrenParams,
    ) -> Result<cookie_agent_protocol::SessionChildrenResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn session_tree(
        &self,
        _: cookie_agent_protocol::SessionTreeParams,
    ) -> Result<cookie_agent_protocol::SessionTreeResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn resume_session(
        &self,
        _: cookie_agent_protocol::SessionResumeParams,
    ) -> Result<cookie_agent_protocol::SessionResumeResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn rename_session(
        &self,
        _: cookie_agent_protocol::SessionRenameParams,
        _context: &ServerContext,
    ) -> Result<cookie_agent_protocol::SessionRenameResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn set_permission_mode(
        &self,
        _: cookie_agent_protocol::SessionSetPermissionModeParams,
        _context: &ServerContext,
    ) -> Result<cookie_agent_protocol::SessionSetPermissionModeResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn get_session_permissions(
        &self,
        _: cookie_agent_protocol::SessionPermissionGetParams,
    ) -> Result<cookie_agent_protocol::SessionPermissionGetResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn set_session_permission(
        &self,
        _: cookie_agent_protocol::SessionPermissionSetParams,
        _context: &ServerContext,
    ) -> Result<cookie_agent_protocol::SessionPermissionMutationResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn clear_session_permission(
        &self,
        _: cookie_agent_protocol::SessionPermissionClearParams,
        _context: &ServerContext,
    ) -> Result<cookie_agent_protocol::SessionPermissionMutationResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn preview_session_permissions(
        &self,
        _: cookie_agent_protocol::SessionPermissionPreviewParams,
    ) -> Result<cookie_agent_protocol::SessionPermissionPreviewResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn list_skills(
        &self,
        _: cookie_agent_protocol::SkillsListParams,
    ) -> Result<cookie_agent_protocol::SkillsListResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn get_skill(
        &self,
        _: cookie_agent_protocol::SkillsGetParams,
    ) -> Result<cookie_agent_protocol::SkillsGetResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn update_skills(
        &self,
        _: cookie_agent_protocol::SkillsUpdateParams,
    ) -> Result<cookie_agent_protocol::SkillsUpdateResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn update_agents(
        &self,
        _: cookie_agent_protocol::AgentsUpdateParams,
    ) -> Result<cookie_agent_protocol::AgentsUpdateResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn compact_session(
        &self,
        _: cookie_agent_protocol::SessionCompactParams,
        _context: &ServerContext,
    ) -> Result<cookie_agent_protocol::SessionCompactResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn cancel_compaction(
        &self,
        _: cookie_agent_protocol::SessionCompactCancelParams,
        _context: &ServerContext,
    ) -> Result<cookie_agent_protocol::SessionCompactCancelResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn resume_held_session(
        &self,
        _: cookie_agent_protocol::SessionHeldResumeParams,
        _context: &ServerContext,
    ) -> Result<cookie_agent_protocol::SessionHeldResumeResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn revert_session(
        &self,
        _: cookie_agent_protocol::SessionRevertParams,
        _context: &ServerContext,
    ) -> Result<cookie_agent_protocol::SessionRevertResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn delete_session(
        &self,
        _: cookie_agent_protocol::SessionDeleteParams,
        _context: &ServerContext,
    ) -> Result<cookie_agent_protocol::SessionDeleteResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn fork_session(
        &self,
        _: cookie_agent_protocol::SessionForkParams,
        _context: &ServerContext,
    ) -> Result<cookie_agent_protocol::SessionForkResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn start_run(
        &self,
        _: cookie_agent_protocol::RunStartParams,
        _context: &ServerContext,
    ) -> Result<cookie_agent_protocol::RunStartResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn steer_run(
        &self,
        _: cookie_agent_protocol::RunSteerParams,
        _context: &ServerContext,
    ) -> Result<cookie_agent_protocol::RunSteerResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn select_model(
        &self,
        _: cookie_agent_protocol::SessionSelectModelParams,
        _context: &ServerContext,
    ) -> Result<cookie_agent_protocol::SessionSelectModelResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn unsubscribe_events(
        &self,
        _: cookie_agent_protocol::EventsUnsubscribeParams,
        _context: &ServerContext,
    ) -> Result<cookie_agent_protocol::EventsUnsubscribeResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn submit_input(
        &self,
        _: cookie_agent_protocol::SessionSubmitInputParams,
        _context: &ServerContext,
    ) -> Result<cookie_agent_protocol::SessionSubmitInputResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn recall_input(
        &self,
        _: cookie_agent_protocol::SessionRecallInputParams,
        _context: &ServerContext,
    ) -> Result<cookie_agent_protocol::SessionRecallInputResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn cancel_run(
        &self,
        _: cookie_agent_protocol::RunCancelParams,
        _context: &ServerContext,
    ) -> Result<cookie_agent_protocol::RunCancelResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn respond_approval(
        &self,
        _: cookie_agent_protocol::ApprovalRespondParams,
        _context: &ServerContext,
    ) -> Result<cookie_agent_protocol::ApprovalRespondResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn list_approvals(
        &self,
        _: cookie_agent_protocol::ApprovalListParams,
    ) -> Result<cookie_agent_protocol::ApprovalListResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn begin_mcp_auth(
        &self,
        _: cookie_agent_protocol::McpAuthBeginParams,
    ) -> Result<cookie_agent_protocol::McpAuthBeginResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn complete_mcp_auth(
        &self,
        _: cookie_agent_protocol::McpAuthCompleteParams,
    ) -> Result<cookie_agent_protocol::McpAuthCompleteResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn cancel_mcp_auth(
        &self,
        _: cookie_agent_protocol::McpAuthCancelParams,
    ) -> Result<cookie_agent_protocol::McpAuthCancelResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn attach_runtime(
        &self,
        _: cookie_agent_protocol::SessionRuntimeAttachParams,
        _: &ServerContext,
    ) -> Result<cookie_agent_protocol::SessionRuntimeAttachResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn detach_runtime(
        &self,
        _: cookie_agent_protocol::SessionRuntimeDetachParams,
        _: &ServerContext,
    ) -> Result<cookie_agent_protocol::SessionRuntimeDetachResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn list_runtimes(
        &self,
        _: cookie_agent_protocol::RuntimesListParams,
    ) -> Result<cookie_agent_protocol::RuntimesListResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn reconnect_runtime(
        &self,
        _: cookie_agent_protocol::RuntimesReconnectParams,
    ) -> Result<cookie_agent_protocol::RuntimesReconnectResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn connect_runtime(
        &self,
        _: cookie_agent_protocol::RuntimesConnectParams,
    ) -> Result<cookie_agent_protocol::RuntimesConnectResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn disconnect_runtime(
        &self,
        _: cookie_agent_protocol::RuntimesConnectParams,
    ) -> Result<cookie_agent_protocol::RuntimesConnectResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn list_machines(
        &self,
        _: cookie_agent_protocol::MachinesListParams,
    ) -> Result<cookie_agent_protocol::MachinesListResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn rename_machine(
        &self,
        _: cookie_agent_protocol::MachinesRenameParams,
    ) -> Result<cookie_agent_protocol::MachinesListResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn assign_machine(
        &self,
        _: cookie_agent_protocol::MachinesAssignParams,
    ) -> Result<cookie_agent_protocol::MachinesListResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn unassign_machine(
        &self,
        _: cookie_agent_protocol::MachinesUnassignParams,
    ) -> Result<cookie_agent_protocol::MachinesListResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn merge_machines(
        &self,
        _: cookie_agent_protocol::MachinesMergeParams,
    ) -> Result<cookie_agent_protocol::MachinesListResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn forget_machine(
        &self,
        _: cookie_agent_protocol::MachinesForgetParams,
    ) -> Result<cookie_agent_protocol::MachinesListResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn forget_machine_runtime(
        &self,
        _: cookie_agent_protocol::MachinesForgetRuntimeParams,
    ) -> Result<cookie_agent_protocol::MachinesListResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn list_memory(
        &self,
        _: cookie_agent_protocol::MemoryListParams,
    ) -> Result<cookie_agent_protocol::MemoryListResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn read_memory(
        &self,
        _: cookie_agent_protocol::MemoryReadParams,
    ) -> Result<cookie_agent_protocol::MemoryReadResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn write_memory(
        &self,
        _: cookie_agent_protocol::MemoryWriteParams,
    ) -> Result<cookie_agent_protocol::MemoryWriteResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn delete_memory(
        &self,
        _: cookie_agent_protocol::MemoryDeleteParams,
    ) -> Result<cookie_agent_protocol::MemoryDeleteResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn list_projects(
        &self,
        _: cookie_agent_protocol::ProjectsListParams,
    ) -> Result<cookie_agent_protocol::ProjectsListResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn get_project(
        &self,
        _: cookie_agent_protocol::ProjectGetParams,
    ) -> Result<cookie_agent_protocol::ProjectGetResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn create_project(
        &self,
        _: cookie_agent_protocol::ProjectCreateParams,
    ) -> Result<cookie_agent_protocol::ProjectResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn update_project(
        &self,
        _: cookie_agent_protocol::ProjectUpdateParams,
    ) -> Result<cookie_agent_protocol::ProjectResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn delete_project(
        &self,
        _: cookie_agent_protocol::ProjectDeleteParams,
    ) -> Result<cookie_agent_protocol::ProjectDeleteResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn set_session_project(
        &self,
        _: cookie_agent_protocol::SessionProjectSetParams,
    ) -> Result<cookie_agent_protocol::SessionProjectSetResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn set_session_agent_option(
        &self,
        _: cookie_agent_protocol::SessionAgentOptionSetParams,
    ) -> Result<cookie_agent_protocol::SessionAgentOptionSetResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn respond_elicitation(
        &self,
        _: cookie_agent_protocol::ElicitationRespondParams,
    ) -> Result<cookie_agent_protocol::ElicitationRespondResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn list_crons(
        &self,
        _: cookie_agent_protocol::CronsListParams,
    ) -> Result<cookie_agent_protocol::CronsListResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn create_cron(
        &self,
        _: cookie_agent_protocol::CronCreateParams,
    ) -> Result<cookie_agent_protocol::CronJobResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn update_cron(
        &self,
        _: cookie_agent_protocol::CronUpdateParams,
    ) -> Result<cookie_agent_protocol::CronJobResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn delete_cron(
        &self,
        _: cookie_agent_protocol::CronDeleteParams,
    ) -> Result<cookie_agent_protocol::CronDeleteResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn run_cron(
        &self,
        _: cookie_agent_protocol::CronRunParams,
    ) -> Result<cookie_agent_protocol::CronRunResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn get_notifications(
        &self,
        _: cookie_agent_protocol::NotificationsGetParams,
    ) -> Result<cookie_agent_protocol::NotificationsGetResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn subscribe_notifications(
        &self,
        _: cookie_agent_protocol::NotificationsSubscribeParams,
        _: &ServerContext,
    ) -> Result<cookie_agent_protocol::NotificationsSubscribeResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn unsubscribe_notifications(
        &self,
        _: cookie_agent_protocol::NotificationsUnsubscribeParams,
    ) -> Result<cookie_agent_protocol::NotificationsUnsubscribeResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn test_notifications(
        &self,
        _: cookie_agent_protocol::NotificationsTestParams,
    ) -> Result<cookie_agent_protocol::NotificationsTestResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn set_client_presence(
        &self,
        _: cookie_agent_protocol::ClientPresenceParams,
        _: &ServerContext,
    ) -> Result<cookie_agent_protocol::ClientPresenceResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn set_session_unread(
        &self,
        _: cookie_agent_protocol::SessionUnreadSetParams,
    ) -> Result<cookie_agent_protocol::SessionUnreadSetResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn list_plugins(
        &self,
        _: cookie_agent_protocol::PluginsListParams,
    ) -> Result<cookie_agent_protocol::PluginsListResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn restart_plugin(
        &self,
        _: cookie_agent_protocol::PluginsRestartParams,
    ) -> Result<cookie_agent_protocol::PluginsRestartResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn get_config(
        &self,
        _: cookie_agent_protocol::ConfigGetParams,
    ) -> Result<cookie_agent_protocol::ConfigGetResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn apply_config(
        &self,
        _: cookie_agent_protocol::ConfigApplyParams,
    ) -> Result<cookie_agent_protocol::ConfigApplyResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn reload_config(
        &self,
        _: cookie_agent_protocol::ConfigReloadParams,
    ) -> Result<cookie_agent_protocol::ConfigReloadResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn get_onboarding(
        &self,
        _: cookie_agent_protocol::OnboardingGetParams,
    ) -> Result<cookie_agent_protocol::OnboardingStatus, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn preview_onboarding(
        &self,
        _: cookie_agent_protocol::OnboardingPreviewParams,
    ) -> Result<cookie_agent_protocol::OnboardingPreviewResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn run_onboarding(
        &self,
        _: cookie_agent_protocol::OnboardingRunParams,
    ) -> Result<cookie_agent_protocol::OnboardingRunResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn background_tool(
        &self,
        _: cookie_agent_protocol::RunToolBackgroundParams,
        _context: &ServerContext,
    ) -> Result<cookie_agent_protocol::RunToolBackgroundResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn list_background(
        &self,
        _: cookie_agent_protocol::SessionBackgroundListParams,
    ) -> Result<cookie_agent_protocol::SessionBackgroundListResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn stop_background(
        &self,
        _: cookie_agent_protocol::SessionBackgroundStopParams,
        _context: &ServerContext,
    ) -> Result<cookie_agent_protocol::SessionBackgroundStopResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn list_mcp_servers(
        &self,
        _: cookie_agent_protocol::McpServerListParams,
    ) -> Result<cookie_agent_protocol::McpServerListResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn add_mcp_server(
        &self,
        _: cookie_agent_protocol::McpServerAddParams,
    ) -> Result<cookie_agent_protocol::McpServerMutationResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn edit_mcp_server(
        &self,
        _: cookie_agent_protocol::McpServerEditParams,
    ) -> Result<cookie_agent_protocol::McpServerMutationResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn remove_mcp_server(
        &self,
        _: cookie_agent_protocol::McpServerNameParams,
    ) -> Result<cookie_agent_protocol::McpServerMutationResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn set_mcp_server_enabled(
        &self,
        _: cookie_agent_protocol::McpServerSetEnabledParams,
    ) -> Result<cookie_agent_protocol::McpServerMutationResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn reconnect_mcp_server(
        &self,
        _: cookie_agent_protocol::McpServerReconnectParams,
    ) -> Result<cookie_agent_protocol::McpServerMutationResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn persist_mcp_server(
        &self,
        _: cookie_agent_protocol::McpServerPersistParams,
    ) -> Result<cookie_agent_protocol::McpServerMutationResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn begin_upload(
        &self,
        _: cookie_agent_protocol::ArtifactUploadBeginParams,
        _: &ServerContext,
    ) -> Result<cookie_agent_protocol::ArtifactUploadBeginResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn upload_chunk(
        &self,
        _: cookie_agent_protocol::ArtifactUploadChunkParams,
        _: &ServerContext,
    ) -> Result<cookie_agent_protocol::ArtifactUploadChunkResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn finish_upload(
        &self,
        _: cookie_agent_protocol::ArtifactUploadFinishParams,
        _: &ServerContext,
    ) -> Result<cookie_agent_protocol::ArtifactUploadFinishResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn cancel_upload(
        &self,
        _: cookie_agent_protocol::ArtifactUploadCancelParams,
        _: &ServerContext,
    ) -> Result<cookie_agent_protocol::ArtifactUploadCancelResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn read_artifact(
        &self,
        _: cookie_agent_protocol::ArtifactReadParams,
    ) -> Result<cookie_agent_protocol::ArtifactReadResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn runtime_snapshot(
        &self,
        _: cookie_agent_protocol::RuntimeSnapshotGetParams,
    ) -> Result<cookie_agent_protocol::RuntimeSnapshotResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn connect_provider(
        &self,
        _: cookie_agent_protocol::ProviderConnectParams,
    ) -> Result<cookie_agent_protocol::ProviderConnectResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
    async fn disconnect_provider(
        &self,
        _: cookie_agent_protocol::ProviderDisconnectParams,
    ) -> Result<cookie_agent_protocol::ProviderDisconnectResult, ServerFault> {
        Err(ServerFault::method_not_found("stub"))
    }
}

struct Connection {
    requests: mpsc::UnboundedSender<MessageFrame>,
    frames: mpsc::UnboundedReceiver<MessageFrame>,
}

impl Connection {
    async fn open(server: Arc<StubServer>) -> Self {
        let (requests, incoming) = mpsc::unbounded_channel();
        let (outgoing, frames) = mpsc::unbounded_channel();
        tokio::spawn(serve(
            server,
            ChannelTransport { incoming, outgoing },
            CancellationToken::new(),
            Authentication::Authenticated,
        ));
        let mut connection = Self { requests, frames };
        connection.send(0, "handshake", hello(None));
        assert_eq!(connection.next().await["id"], 0);
        connection
    }

    /// A connection that must authenticate in its handshake within
    /// `deadline`, not yet handshaken.
    fn unauthenticated(server: Arc<StubServer>, deadline: Duration) -> Self {
        let (requests, incoming) = mpsc::unbounded_channel();
        let (outgoing, frames) = mpsc::unbounded_channel();
        tokio::spawn(serve_within(
            server,
            ChannelTransport { incoming, outgoing },
            CancellationToken::new(),
            Authentication::Token(Arc::new(zeroize::Zeroizing::new(TOKEN.to_owned()))),
            deadline,
        ));
        Self { requests, frames }
    }

    /// Whether the server closed the connection.
    async fn closed(&mut self) -> bool {
        tokio::time::timeout(Duration::from_secs(2), self.frames.recv())
            .await
            .expect("the server acts in time")
            .is_none()
    }

    fn send(&self, id: i64, method: &str, params: Value) {
        self.requests
            .send(MessageFrame::Value(json!({
                "jsonrpc": "2.0", "id": id, "method": method, "params": params
            })))
            .expect("server reading");
    }

    async fn next(&mut self) -> Value {
        let frame = tokio::time::timeout(Duration::from_secs(1), self.frames.recv())
            .await
            .expect("frame in time")
            .expect("server open");
        let MessageFrame::Text(text) = frame else {
            panic!("expected a text frame");
        };
        serde_json::from_str(&text).expect("JSON frame")
    }
}

const TOKEN: &str = "the-daemon-token";

fn hello(token: Option<&str>) -> Value {
    let mut hello = json!({
        "protocol_version": cookie_agent_protocol::CLIENT_PROTOCOL_VERSION,
        "client": {"kind": "test", "instance_id": cookie_agent_protocol::ClientInstanceId::new_v7()},
    });
    if let Some(token) = token {
        hello["token"] = json!(token);
    }
    hello
}

#[tokio::test]
async fn an_unauthenticated_connection_may_only_handshake_with_the_token() {
    let mut connection =
        Connection::unauthenticated(Arc::new(StubServer::default()), Duration::from_secs(60));
    connection.send(1, "session.list", json!({}));
    let reply = connection.next().await;
    assert_eq!(
        reply["error"]["code"],
        cookie_agent_protocol::errors::UNAUTHENTICATED
    );
    assert_eq!(reply["error"]["data"]["code"], "unauthenticated");
    // A handshake of another version is refused before the token is read,
    // and the connection stays open.
    connection.send(
        2,
        "handshake",
        json!({"protocol_version": "0.0.1", "client": {"kind": "test", "instance_id": cookie_agent_protocol::ClientInstanceId::new_v7()}}),
    );
    assert_eq!(
        connection.next().await["error"]["code"],
        cookie_agent_protocol::errors::UNSUPPORTED_PROTOCOL
    );
    connection.send(3, "handshake", hello(Some(TOKEN)));
    let reply = connection.next().await;
    assert_eq!(
        reply["result"]["protocol_version"],
        cookie_agent_protocol::CLIENT_PROTOCOL_VERSION.to_string(),
        "{reply}"
    );
    connection.send(4, "session.list", json!({}));
    assert_eq!(connection.next().await["result"]["sessions"], json!([]));
}

#[tokio::test]
async fn a_wrong_or_missing_token_is_refused_and_closes_the_connection() {
    for token in [Some("wrong"), Some(""), None] {
        let mut connection =
            Connection::unauthenticated(Arc::new(StubServer::default()), Duration::from_secs(60));
        connection.send(1, "handshake", hello(token));
        let reply = connection.next().await;
        assert_eq!(
            reply["error"]["code"],
            cookie_agent_protocol::errors::UNAUTHENTICATED,
            "{token:?}"
        );
        assert!(connection.closed().await, "{token:?}");
    }
}

#[tokio::test]
async fn a_connection_that_does_not_authenticate_in_time_is_closed() {
    let mut connection =
        Connection::unauthenticated(Arc::new(StubServer::default()), Duration::from_millis(100));
    // Being answered does not extend the deadline.
    connection.send(1, "session.list", json!({}));
    assert_eq!(
        connection.next().await["error"]["code"],
        cookie_agent_protocol::errors::UNAUTHENTICATED
    );
    assert!(connection.closed().await);

    // Authenticating in time keeps it open past the deadline.
    let mut connection =
        Connection::unauthenticated(Arc::new(StubServer::default()), Duration::from_millis(100));
    connection.send(1, "handshake", hello(Some(TOKEN)));
    assert!(connection.next().await["result"].is_object());
    tokio::time::sleep(Duration::from_millis(200)).await;
    connection.send(2, "session.list", json!({}));
    assert_eq!(connection.next().await["id"], 2);
}

#[tokio::test]
async fn an_authenticated_connection_must_still_handshake_first() {
    let (requests, incoming) = mpsc::unbounded_channel();
    let (outgoing, frames) = mpsc::unbounded_channel();
    tokio::spawn(serve(
        Arc::new(StubServer::default()),
        ChannelTransport { incoming, outgoing },
        CancellationToken::new(),
        Authentication::Authenticated,
    ));
    let mut connection = Connection { requests, frames };
    connection.send(1, "session.list", json!({}));
    let reply = connection.next().await;
    assert_eq!(
        reply["error"]["code"],
        cookie_agent_protocol::errors::HANDSHAKE_REQUIRED
    );
    assert_eq!(reply["error"]["data"]["code"], "handshake_required");
    connection.send(2, "handshake", hello(None));
    assert!(connection.next().await["result"].is_object());
}

#[tokio::test]
async fn a_slow_request_does_not_hold_up_later_ones() {
    let server = Arc::new(StubServer::default());
    let mut connection = Connection::open(Arc::clone(&server)).await;
    let session_id = cookie_agent_protocol::SessionId::new_v7();
    connection.send(1, "session.get", json!({ "session_id": session_id }));
    connection.send(2, "session.list", json!({}));
    assert_eq!(
        connection.next().await["id"],
        2,
        "the fast request answers first"
    );
    server.release_get.notify_one();
    assert_eq!(connection.next().await["id"], 1);
}

#[tokio::test]
async fn a_request_s_notifications_follow_its_response() {
    let mut connection = Connection::open(Arc::new(StubServer::default())).await;
    let session_id = cookie_agent_protocol::SessionId::new_v7();
    connection.send(1, "events.subscribe", json!({ "session_id": session_id }));
    let first = connection.next().await;
    assert_eq!(first["id"], 1, "the response precedes its tail: {first}");
    assert_eq!(connection.next().await["method"], "stub.tail");
}

#[tokio::test]
async fn a_replacement_session_tail_forwards_after_the_tail_it_replaces() {
    async fn next_gap(outbound: &mut mpsc::Receiver<String>) -> u64 {
        let text = tokio::time::timeout(Duration::from_secs(2), outbound.recv())
            .await
            .expect("notification in time")
            .expect("connection open");
        let value: Value = serde_json::from_str(&text).expect("JSON notification");
        assert_eq!(value["method"], "events.subscription");
        value["params"]["last_delivered_seq"]
            .as_u64()
            .expect("gap sequence")
    }

    let (notifications, mut outbound) = mpsc::channel(16);
    let context = ServerContext::new(notifications, CancellationToken::new());
    let session_id = cookie_agent_protocol::SessionId::new_v7();
    let gap = |seq| EventSubscriptionMessage::Gap {
        session_id,
        last_delivered_seq: seq,
    };
    let (replaced_sender, replaced) = mpsc::channel(8);
    let (replacement_sender, replacement) = mpsc::channel(8);
    replaced_sender.send(gap(1)).await.expect("queue");
    replacement_sender.send(gap(3)).await.expect("queue");
    context.forward_session_tail(session_id, replaced);
    context.forward_session_tail(session_id, replacement);
    assert_eq!(next_gap(&mut outbound).await, 1);
    // Until its producer closes the replaced tail, what that tail still
    // carries comes first, even though the replacement is ready.
    replaced_sender.send(gap(2)).await.expect("queue");
    assert_eq!(next_gap(&mut outbound).await, 2);
    drop(replaced_sender);
    assert_eq!(next_gap(&mut outbound).await, 3);
}

#[tokio::test]
async fn params_refuse_unknown_members_inside_types_history_reads_tolerantly() {
    let mut connection = Connection::open(Arc::new(StubServer::default())).await;
    let session_id = cookie_agent_protocol::SessionId::new_v7();
    let selection = json!({
        "agent": "build",
        "model": {"model": "openai/gpt-5", "variant": null},
    });
    let mut nested = selection.clone();
    nested["model"]["extra"] = json!(true);
    let rule = json!({"action": "bash", "resource": "*", "effect": "allow", "note": "x"});
    for (id, method, params, field) in [
        (
            1,
            "run.start",
            json!({
                "session_id": session_id,
                "client_run_id": "run-1",
                "selection": {"agent": "build", "model": selection["model"], "extra": 1},
                "content": [{"type": "text", "text": "hi"}],
            }),
            "selection.extra",
        ),
        (
            2,
            "session.select_model",
            json!({"session_id": session_id, "selection": nested}),
            "selection.model.extra",
        ),
        (
            3,
            "session.create",
            json!({"selection": selection, "permission_rules": [rule]}),
            "permission_rules[0].note",
        ),
        (
            4,
            "approval.respond",
            json!({
                "session_id": session_id,
                "approval_id": cookie_agent_protocol::ApprovalId::new_v7(),
                "request_revision": 1,
                "operation_fingerprint": {"digest": "a".repeat(64), "extra": 1},
                "client_response_id": "response-1",
                "decision": "reject",
            }),
            "operation_fingerprint.extra",
        ),
        (
            5,
            "session.submit_input",
            json!({
                "session_id": session_id,
                "content": [
                    {"type": "text", "text": "look"},
                    {"type": "file", "artifact": {
                        "sha256": "a".repeat(64), "byte_length": 3,
                        "media_type": "image/png", "width": 2,
                    }},
                ],
                "delivery": "steer",
                "client_run_id": "run-2",
            }),
            "content[1].artifact.width",
        ),
    ] {
        connection.send(id, method, params);
        let reply = connection.next().await;
        assert_eq!(reply["id"], id, "{reply}");
        assert_eq!(reply["error"]["code"], -32602, "{reply}");
        assert_eq!(reply["error"]["data"]["code"], "unknown_field", "{reply}");
        assert_eq!(reply["error"]["data"]["field"], field, "{reply}");
    }
}

#[tokio::test]
async fn a_client_authenticates_with_the_token_in_its_handshake() {
    let connect = || {
        let (client, server) = cookie_agent_protocol::in_process_pair(8);
        tokio::spawn(serve(
            Arc::new(StubServer::default()),
            server,
            CancellationToken::new(),
            Authentication::Token(Arc::new(zeroize::Zeroizing::new(TOKEN.to_owned()))),
        ));
        crate::Client::connect_stream(client)
    };
    let identity = cookie_agent_protocol::ClientIdentity::generate(
        cookie_agent_protocol::ClientKind::new("web").expect("client kind"),
    );
    let client = connect();
    let hello = client
        .handshake_with_token(
            identity.clone(),
            cookie_agent_protocol::HandshakeToken::new(TOKEN),
        )
        .await
        .expect("handshake with the token");
    assert_eq!(hello.origin, identity.origin());
    let refused = connect()
        .handshake_with_token(
            identity,
            cookie_agent_protocol::HandshakeToken::new("wrong"),
        )
        .await
        .expect_err("a wrong token is refused");
    assert_eq!(refused.error_code(), Some("unauthenticated"), "{refused}");
}

/// A connection that reaches the server as it shuts down (as a restart
/// begins) gets no handshake: its client never takes it for one that works.
#[tokio::test]
async fn a_connection_the_server_is_shutting_down_completes_no_handshake() {
    let shutdown = CancellationToken::new();
    shutdown.cancel();
    let (requests, incoming) = mpsc::unbounded_channel();
    let (outgoing, mut frames) = mpsc::unbounded_channel();
    // The handshake is already waiting when the connection is served, so
    // reading it and seeing the shutdown are both ready at once.
    let connection_requests = requests;
    connection_requests
        .send(MessageFrame::Text(
            serde_json::json!({"jsonrpc": "2.0", "id": 0, "method": "handshake", "params": hello(None)})
                .to_string(),
        ))
        .unwrap();
    serve(
        Arc::new(StubServer::default()),
        ChannelTransport { incoming, outgoing },
        shutdown,
        Authentication::Authenticated,
    )
    .await
    .expect("the connection ends cleanly");
    while let Ok(frame) = frames.try_recv() {
        let MessageFrame::Text(text) = frame else {
            continue;
        };
        let reply: Value = serde_json::from_str(&text).unwrap();
        assert!(
            reply.get("result").is_none(),
            "a handshake answered: {reply}"
        );
    }
}

#[tokio::test]
async fn a_client_gone_before_its_answer_ends_the_connection_cleanly() {
    let server = Arc::new(StubServer::default());
    let (requests, incoming) = mpsc::unbounded_channel();
    let (outgoing, frames) = mpsc::unbounded_channel();
    let served = tokio::spawn(serve(
        Arc::clone(&server),
        ChannelTransport { incoming, outgoing },
        CancellationToken::new(),
        Authentication::Authenticated,
    ));
    let mut connection = Connection { requests, frames };
    connection.send(0, "handshake", hello(None));
    assert_eq!(connection.next().await["id"], 0);
    // The client asks, then goes away while the answer is still being built:
    // sending it meets a closed transport, which is the client's end, not a
    // failure of the connection.
    let session_id = cookie_agent_protocol::SessionId::new_v7();
    connection.send(1, "session.get", json!({ "session_id": session_id }));
    let Connection { requests, frames } = connection;
    drop(frames);
    server.release_get.notify_one();
    let result = tokio::time::timeout(Duration::from_secs(2), served)
        .await
        .expect("the connection ends in time")
        .expect("serve does not panic");
    assert!(result.is_ok(), "{result:?}");
    drop(requests);
}

/// A server that does not provide `daemon.restart` refuses it with
/// `restart_unsupported`; its params are strict and `when` defaults to
/// `idle`.
#[tokio::test]
async fn a_daemon_without_a_restart_refuses_it() {
    let mut connection = Connection::open(Arc::new(StubServer::default())).await;
    connection.send(1, cookie_agent_protocol::DAEMON_RESTART_METHOD, json!({}));
    let reply = connection.next().await;
    assert_eq!(
        reply["error"]["data"]["code"], "restart_unsupported",
        "{reply}"
    );
    connection.send(
        2,
        cookie_agent_protocol::DAEMON_RESTART_METHOD,
        json!({ "when": "later" }),
    );
    let reply = connection.next().await;
    assert_eq!(reply["error"]["data"]["code"], "unknown_variant", "{reply}");
    let params: cookie_agent_protocol::DaemonRestartParams =
        serde_json::from_value(json!({})).expect("empty params");
    assert_eq!(params.when, cookie_agent_protocol::RestartWhen::Idle);
}

/// A connection the server shuts down still gets the frames already queued
/// for it before the transport closes.
#[tokio::test]
async fn shutting_down_sends_what_is_queued_then_closes() {
    let (requests, incoming) = mpsc::unbounded_channel();
    let (outgoing, frames) = mpsc::unbounded_channel();
    let shutdown = CancellationToken::new();
    let served = tokio::spawn(serve(
        Arc::new(StubServer::default()),
        ChannelTransport { incoming, outgoing },
        shutdown.clone(),
        Authentication::Authenticated,
    ));
    let mut connection = Connection { requests, frames };
    connection.send(0, "handshake", hello(None));
    assert_eq!(connection.next().await["id"], 0);
    connection.send(1, "session.list", json!({}));
    // The answer is queued as soon as the request task runs; shut down
    // right behind it.
    tokio::task::yield_now().await;
    tokio::time::sleep(Duration::from_millis(20)).await;
    shutdown.cancel();
    let result = tokio::time::timeout(Duration::from_secs(2), served)
        .await
        .expect("the connection ends in time")
        .expect("serve does not panic");
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(connection.next().await["id"], 1);
    assert!(connection.closed().await);
}
