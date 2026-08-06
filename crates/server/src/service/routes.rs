use cookie_agent_engine::EngineError;
use cookie_agent_protocol::{
    ApprovalListParams, ApprovalRespondErrorCode, ApprovalRespondParams, ClientHello, EventPayload,
    EventsSubscribeParams, PROVIDER_CONNECT_METHOD, PROVIDER_DISCONNECT_METHOD,
    RUNTIME_SNAPSHOT_GET_METHOD, RunCancelParams, RunStartParams, RunSteerParams,
    RunToolStdinParams, RuntimeSnapshotGetParams, SessionChildrenParams, SessionChildrenResult,
    SessionCreateParams, SessionCreateResult, SessionGetParams, SessionGetResult,
    SessionListParams, SessionListResult, SessionRenameErrorCode, SessionRenameParams,
    SessionResumeParams, SessionResumeResult, SessionSetPermissionModeParams,
    SessionSetPermissionModeResult, SessionTreeParams, SessionTreeResult,
};
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::Server;
use crate::rpc::{
    RouteResult, RpcFault, decode_params, decode_rename_params, engine_fault, params_or_default,
    value,
};

impl Server {
    pub(super) async fn route_after_handshake(
        &self,
        handshaken: &mut bool,
        method: &str,
        params: Option<Value>,
        has_request_id: bool,
        notifications: mpsc::Sender<Value>,
        shutdown: &CancellationToken,
    ) -> Result<RouteResult, RpcFault> {
        let result = if !*handshaken && method != "handshake" {
            Err(RpcFault::handshake_required())
        } else {
            self.route(method, params, has_request_id, notifications, shutdown)
                .await
        };
        if matches!(result, Ok(RouteResult::Handshake)) {
            *handshaken = true;
        }
        result
    }

    async fn route(
        &self,
        method: &str,
        params: Option<Value>,
        has_request_id: bool,
        notifications: mpsc::Sender<Value>,
        shutdown: &CancellationToken,
    ) -> Result<RouteResult, RpcFault> {
        match method {
            "handshake" => {
                if !has_request_id {
                    return Err(RpcFault::request_id_required());
                }
                let _: ClientHello = decode_params(params)?;
                Ok(RouteResult::Handshake)
            }
            "session.create" => {
                let request: SessionCreateParams = decode_params(params)?;
                let session = self
                    .engine
                    .create_session(request.selection)
                    .map_err(engine_fault)?;
                value(SessionCreateResult { session })
            }
            "session.list" => {
                let request: SessionListParams = params_or_default(params)?;
                let sessions = self
                    .engine
                    .list_sessions()
                    .into_iter()
                    .filter(|session| {
                        request
                            .cwd_identity
                            .as_ref()
                            .is_none_or(|cwd| &session.cwd_identity == cwd)
                    })
                    .collect();
                value(SessionListResult { sessions })
            }
            "session.get" => {
                let request: SessionGetParams = decode_params(params)?;
                value(SessionGetResult {
                    session: self
                        .engine
                        .get_session(request.session_id)
                        .map_err(engine_fault)?,
                })
            }
            "session.children" => {
                let request: SessionChildrenParams = decode_params(params)?;
                value(SessionChildrenResult {
                    children: self.engine.children(request.session_id),
                })
            }
            "session.tree" => {
                let request: SessionTreeParams = decode_params(params)?;
                value(SessionTreeResult {
                    tree: self.engine.tree(request.session_id).map_err(engine_fault)?,
                })
            }
            "session.resume" => {
                let request: SessionResumeParams = decode_params(params)?;
                let session = self
                    .engine
                    .resume(request.session_id)
                    .await
                    .map_err(engine_fault)?;
                value(SessionResumeResult { session })
            }
            "session.rename" => {
                let request = decode_rename_params(params)?;
                let result = self
                    .engine
                    .rename_session(request.clone())
                    .await
                    .map_err(|error| rename_fault(&request, error))?;
                value(result)
            }
            "session.set_permission_mode" => {
                let request: SessionSetPermissionModeParams = decode_params(params)?;
                self.engine
                    .set_permission_mode(request.session_id, request.mode)
                    .map_err(engine_fault)?;
                value(SessionSetPermissionModeResult {})
            }
            "run.start" => {
                let request: RunStartParams = decode_params(params)?;
                match self.engine.start_run(request.clone()).await {
                    Ok(result) => value(result),
                    Err(EngineError::RunIdempotencyConflict) => {
                        Err(RpcFault::run_start_conflict(&request))
                    }
                    Err(error) => Err(engine_fault(error)),
                }
            }
            "run.steer" => {
                let request: RunSteerParams = decode_params(params)?;
                value(
                    self.engine
                        .steer(request.run_id, request.input)
                        .await
                        .map_err(engine_fault)?,
                )
            }
            "run.cancel" => {
                let request: RunCancelParams = decode_params(params)?;
                value(
                    self.engine
                        .cancel_run(request.run_id)
                        .await
                        .map_err(engine_fault)?,
                )
            }
            "run.tool_stdin" => value(
                self.engine
                    .tool_stdin(decode_params::<RunToolStdinParams>(params)?)
                    .await
                    .map_err(engine_fault)?,
            ),
            "events.subscribe" => {
                let request: EventsSubscribeParams = decode_params(params)?;
                let (result, receiver) = self
                    .engine
                    .subscribe(request.session_id, request.cursor)
                    .await
                    .map_err(engine_fault)?;
                self.start_event_tail(receiver, notifications.clone(), shutdown.child_token());
                for event in &result.events {
                    if let EventPayload::ToolCallStarted { start } = &event.payload {
                        self.start_output_tail(
                            start.tool_call_id,
                            notifications.clone(),
                            shutdown.child_token(),
                        );
                    }
                }
                value(result)
            }
            "approval.respond" => {
                let request: ApprovalRespondParams = decode_params(params)?;
                match self.engine.approval_respond(request.clone()).await {
                    Ok(result) => value(result),
                    Err(error) => Err(approval_fault(&request, error)),
                }
            }
            "approval.list" => {
                let request: ApprovalListParams = decode_params(params)?;
                value(
                    self.engine
                        .list_approvals(request.root_session_id, request.status),
                )
            }
            RUNTIME_SNAPSHOT_GET_METHOD => {
                if !has_request_id {
                    return Err(RpcFault::request_id_required());
                }
                let _: RuntimeSnapshotGetParams = params_or_default(params)?;
                value(self.engine.runtime_snapshot().map_err(engine_fault)?)
            }
            PROVIDER_CONNECT_METHOD => {
                if !has_request_id {
                    return Err(RpcFault::request_id_required());
                }
                value(self.connect_provider(decode_params(params)?)?)
            }
            PROVIDER_DISCONNECT_METHOD => {
                if !has_request_id {
                    return Err(RpcFault::request_id_required());
                }
                value(self.disconnect_provider(decode_params(params)?)?)
            }
            _ => Err(RpcFault::method_not_found()),
        }
    }
}

fn rename_fault(request: &SessionRenameParams, error: EngineError) -> RpcFault {
    let code = match error {
        EngineError::RenameConflict => SessionRenameErrorCode::IdempotencyConflict,
        EngineError::Session(cookie_agent_engine::session::SessionError::Missing(_))
        | EngineError::MissingActor(_) => SessionRenameErrorCode::SessionNotFound,
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
        EngineError::Session(cookie_agent_engine::session::SessionError::Missing(_))
        | EngineError::MissingActor(_) => RpcFault::approval(
            request,
            ApprovalRespondErrorCode::ApprovalNotFound,
            None,
            None,
        ),
        _ => engine_fault(error),
    }
}
