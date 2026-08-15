use async_trait::async_trait;
use cookie_agent_engine::{EngineError, session::SessionError};
use cookie_agent_protocol::{
    ApprovalListParams, ApprovalListResult, ApprovalRespondErrorCode, ApprovalRespondParams,
    ApprovalRespondResult, EventsSubscribeParams, EventsSubscribeResult, McpApprovalDecision,
    McpApprovalListParams, McpApprovalListResult, McpApprovalRespondParams,
    McpApprovalRespondResult, McpPendingApproval, ProviderConnectParams, ProviderConnectResult,
    ProviderDisconnectParams, ProviderDisconnectResult, RunCancelParams, RunCancelResult,
    RunRecallSteerParams, RunRecallSteerResult, RunStartParams, RunStartResult, RunSteerParams,
    RunSteerResult, RunToolStdinParams, RunToolStdinResult, RuntimeSnapshotGetParams,
    RuntimeSnapshotResult, ServerContext, ServerFault, ServerProtocol, SessionChildrenParams,
    SessionChildrenResult, SessionCompactParams, SessionCompactResult, SessionCreateParams,
    SessionCreateResult, SessionForkParams, SessionForkResult, SessionGetParams, SessionGetResult,
    SessionListParams, SessionListResult, SessionRenameErrorCode, SessionRenameParams,
    SessionRenameResult, SessionResumeParams, SessionResumeResult, SessionRevertParams,
    SessionRevertResult, SessionSetPermissionModeParams, SessionSetPermissionModeResult,
    SessionTreeParams, SessionTreeResult,
};

use super::Server;
use crate::rpc::{RpcFault, engine_fault};

type Result<T> = std::result::Result<T, ServerFault>;

#[async_trait]
impl ServerProtocol for Server {
    async fn connected(&self, context: ServerContext) {
        self.start_runtime_notifications(context);
    }

    async fn create_session(&self, params: SessionCreateParams) -> Result<SessionCreateResult> {
        self.engine
            .create_session(params.selection)
            .map(|session| SessionCreateResult { session })
            .map_err(protocol_fault)
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
            .collect();
        Ok(SessionListResult { sessions })
    }

    async fn get_session(&self, params: SessionGetParams) -> Result<SessionGetResult> {
        self.engine
            .get_session(params.session_id)
            .map(|session| SessionGetResult { session })
            .map_err(protocol_fault)
    }

    async fn session_children(
        &self,
        params: SessionChildrenParams,
    ) -> Result<SessionChildrenResult> {
        Ok(SessionChildrenResult {
            children: self.engine.children(params.session_id),
        })
    }

    async fn session_tree(&self, params: SessionTreeParams) -> Result<SessionTreeResult> {
        self.engine
            .tree(params.session_id)
            .map(|tree| SessionTreeResult { tree })
            .map_err(protocol_fault)
    }

    async fn resume_session(&self, params: SessionResumeParams) -> Result<SessionResumeResult> {
        self.engine
            .resume(params.session_id)
            .await
            .map(|session| SessionResumeResult { session })
            .map_err(protocol_fault)
    }

    async fn rename_session(&self, params: SessionRenameParams) -> Result<SessionRenameResult> {
        self.engine
            .rename_session(params.clone())
            .await
            .map_err(|error| rename_fault(&params, error).into())
    }

    async fn set_permission_mode(
        &self,
        params: SessionSetPermissionModeParams,
    ) -> Result<SessionSetPermissionModeResult> {
        self.engine
            .set_permission_mode(params.session_id, params.mode)
            .map_err(protocol_fault)?;
        Ok(SessionSetPermissionModeResult {})
    }

    async fn compact_session(&self, params: SessionCompactParams) -> Result<SessionCompactResult> {
        self.engine
            .compact_session(
                params.session_id,
                params.focus.as_ref().map(|focus| focus.as_str()),
            )
            .await
            .map(|compacted| SessionCompactResult { compacted })
            .map_err(protocol_fault)
    }

    async fn revert_session(&self, params: SessionRevertParams) -> Result<SessionRevertResult> {
        self.engine
            .revert_session(params.session_id, params.through_seq)
            .await
            .map_err(protocol_fault)
    }

    async fn fork_session(&self, params: SessionForkParams) -> Result<SessionForkResult> {
        self.engine
            .fork_session(params.session_id, params.through_seq)
            .await
            .map_err(protocol_fault)
    }

    async fn start_run(&self, params: RunStartParams) -> Result<RunStartResult> {
        match self.engine.start_run(params.clone()).await {
            Ok(result) => Ok(result),
            Err(EngineError::RunIdempotencyConflict) => {
                Err(RpcFault::run_start_conflict(&params).into())
            }
            Err(error) => Err(protocol_fault(error)),
        }
    }

    async fn steer_run(&self, params: RunSteerParams) -> Result<RunSteerResult> {
        self.engine
            .steer(params.run_id, params.input)
            .await
            .map_err(protocol_fault)
    }

    async fn recall_steer(&self, params: RunRecallSteerParams) -> Result<RunRecallSteerResult> {
        self.engine
            .recall_steer(params.run_id)
            .await
            .map_err(protocol_fault)
    }

    async fn cancel_run(&self, params: RunCancelParams) -> Result<RunCancelResult> {
        self.engine
            .cancel_run(params.run_id)
            .await
            .map_err(protocol_fault)
    }

    async fn tool_stdin(&self, params: RunToolStdinParams) -> Result<RunToolStdinResult> {
        self.engine.tool_stdin(params).await.map_err(protocol_fault)
    }

    async fn subscribe_events(
        &self,
        params: EventsSubscribeParams,
        context: &ServerContext,
    ) -> Result<EventsSubscribeResult> {
        let (result, receiver) = self
            .engine
            .subscribe(params.session_id, params.cursor)
            .await
            .map_err(protocol_fault)?;
        self.start_event_tail(receiver, context.clone());
        for event in &result.events {
            if let cookie_agent_protocol::EventPayload::ToolCallStarted { start } = &event.payload {
                self.start_output_tail(start.tool_call_id, context.clone());
            }
        }
        Ok(result)
    }

    async fn respond_approval(
        &self,
        params: ApprovalRespondParams,
    ) -> Result<ApprovalRespondResult> {
        self.engine
            .approval_respond(params.clone())
            .await
            .map_err(|error| approval_fault(&params, error).into())
    }

    async fn list_approvals(&self, params: ApprovalListParams) -> Result<ApprovalListResult> {
        Ok(self
            .engine
            .list_approvals(params.root_session_id, params.status))
    }

    async fn list_mcp_approvals(&self, _: McpApprovalListParams) -> Result<McpApprovalListResult> {
        Ok(McpApprovalListResult {
            approvals: self
                .engine
                .pending_mcp_approvals()
                .into_iter()
                .map(|approval| McpPendingApproval {
                    server: approval.server,
                    digest: approval.digest,
                    connection: approval.connection,
                })
                .collect(),
        })
    }

    async fn respond_mcp_approval(
        &self,
        params: McpApprovalRespondParams,
    ) -> Result<McpApprovalRespondResult> {
        match params.decision {
            McpApprovalDecision::Approve => self
                .engine
                .approve_project_mcp_server(&params.server, &params.digest),
            McpApprovalDecision::Reject => self
                .engine
                .reject_project_mcp_server(&params.server, &params.digest),
        }
        .map_err(protocol_fault)?;
        Ok(McpApprovalRespondResult {
            server: params.server,
            digest: params.digest,
            decision: params.decision,
        })
    }

    async fn runtime_snapshot(&self, _: RuntimeSnapshotGetParams) -> Result<RuntimeSnapshotResult> {
        self.engine.runtime_snapshot().map_err(protocol_fault)
    }

    async fn connect_provider(
        &self,
        params: ProviderConnectParams,
    ) -> Result<ProviderConnectResult> {
        Server::connect_provider(self, params).map_err(Into::into)
    }

    async fn disconnect_provider(
        &self,
        params: ProviderDisconnectParams,
    ) -> Result<ProviderDisconnectResult> {
        Server::disconnect_provider(self, params).map_err(Into::into)
    }
}

fn protocol_fault(error: EngineError) -> ServerFault {
    engine_fault(error).into()
}

fn rename_fault(request: &SessionRenameParams, error: EngineError) -> RpcFault {
    let code = match error {
        EngineError::RenameConflict => SessionRenameErrorCode::IdempotencyConflict,
        EngineError::Session(SessionError::Missing(_)) | EngineError::MissingActor(_) => {
            SessionRenameErrorCode::SessionNotFound
        }
        _ => return engine_fault(error),
    };
    RpcFault::rename(request, code)
}

fn approval_fault(request: &ApprovalRespondParams, error: EngineError) -> RpcFault {
    match error {
        EngineError::ApprovalResponse(failure) => RpcFault::approval_failure(request, &failure),
        EngineError::ApprovalNotPending { .. } => RpcFault::approval(
            request,
            ApprovalRespondErrorCode::ApprovalNotPending,
            None,
            None,
        ),
        EngineError::Session(SessionError::Missing(_)) | EngineError::MissingActor(_) => {
            RpcFault::approval(
                request,
                ApprovalRespondErrorCode::ApprovalNotFound,
                None,
                None,
            )
        }
        _ => engine_fault(error),
    }
}
