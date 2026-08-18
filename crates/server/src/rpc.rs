use cookie_agent_engine::{
    ApprovalRespondFailure, EngineError, delegation_events::DelegationEventError,
    session::SessionError,
};
use cookie_agent_protocol::{
    ApprovalRespondError, ApprovalRespondErrorCode, ApprovalRespondParams, ClientResponseId,
    ProviderConnectError, ProviderConnectErrorCode, ProviderConnectParams, ProviderDisconnectError,
    ProviderDisconnectErrorCode, ProviderDisconnectParams, RunStartConflict, RunStartConflictCode,
    RunStartParams, SESSION_TREE_USAGE_CORRUPT_DELEGATION_CODE,
    SESSION_TREE_USAGE_MISSING_SESSION_CODE, SessionRenameError, SessionRenameErrorCode,
    SessionRenameParams,
};
use serde::Serialize;
use serde_json::{Value, json};

#[derive(Debug)]
pub(crate) struct RpcFault {
    code: i32,
    message: &'static str,
    data: Option<Value>,
}

impl RpcFault {
    pub(crate) fn run_start_conflict(params: &RunStartParams) -> Self {
        Self {
            code: -32602,
            message: "idempotency conflict",
            data: typed_error_data(RunStartConflict {
                code: RunStartConflictCode::IdempotencyConflict,
                session_id: params.session_id,
                client_run_id: params.client_run_id.clone(),
            }),
        }
    }

    const fn engine() -> Self {
        Self {
            code: -32000,
            message: "engine error",
            data: None,
        }
    }

    pub(crate) fn provider_connect(request: &ProviderConnectParams, error: EngineError) -> Self {
        let code = match error {
            EngineError::ModelManager(cookie_agent_models::ModelManagerError::UnknownProvider) => {
                ProviderConnectErrorCode::UnknownProvider
            }
            EngineError::ModelManager(
                cookie_agent_models::ModelManagerError::UnsupportedProvider,
            ) => ProviderConnectErrorCode::UnsupportedProvider,
            EngineError::ModelManager(
                cookie_agent_models::ModelManagerError::RemovedWithoutRetainedRecipeMatch,
            ) => ProviderConnectErrorCode::RemovedWithoutRetainedRecipeMatch,
            EngineError::ModelManager(cookie_agent_models::ModelManagerError::InvalidSetup) => {
                ProviderConnectErrorCode::InvalidSetupField
            }
            EngineError::ModelManager(
                cookie_agent_models::ModelManagerError::UnsupportedAuthMethod,
            ) => ProviderConnectErrorCode::UnsupportedAuthMethod,
            EngineError::ModelManager(
                cookie_agent_models::ModelManagerError::InvalidCredentials,
            ) => ProviderConnectErrorCode::InvalidCredential,
            EngineError::ModelManager(cookie_agent_models::ModelManagerError::ProviderStore(
                cookie_agent_models::provider_store::ProviderStoreError::CatalogRevisionConflict,
            )) => ProviderConnectErrorCode::CatalogRevisionConflict,
            EngineError::ModelManager(cookie_agent_models::ModelManagerError::ProviderStore(
                cookie_agent_models::provider_store::ProviderStoreError::IdempotencyConflict,
            )) => ProviderConnectErrorCode::IdempotencyConflict,
            EngineError::ModelManager(cookie_agent_models::ModelManagerError::ProviderStore(_)) => {
                ProviderConnectErrorCode::ProviderStoreWriteFailed
            }
            _ => ProviderConnectErrorCode::RuntimeCompileFailed,
        };
        Self {
            code: -32011,
            message: "provider connect error",
            data: typed_error_data(ProviderConnectError {
                code,
                provider_id: request.provider_id.clone(),
                client_connect_id: request.client_connect_id.clone(),
            }),
        }
    }

    pub(crate) fn provider_disconnect(
        request: &ProviderDisconnectParams,
        error: EngineError,
    ) -> Self {
        let code = match error {
            EngineError::ModelManager(cookie_agent_models::ModelManagerError::CustomProviderNotStoreBacked) => ProviderDisconnectErrorCode::CustomProviderNotStoreBacked,
            EngineError::ModelManager(cookie_agent_models::ModelManagerError::ProviderStore(cookie_agent_models::provider_store::ProviderStoreError::RuntimeRevisionConflict)) => ProviderDisconnectErrorCode::RuntimeRevisionConflict,
            EngineError::ModelManager(cookie_agent_models::ModelManagerError::ProviderStore(cookie_agent_models::provider_store::ProviderStoreError::ProviderStateRevisionConflict)) => ProviderDisconnectErrorCode::ProviderStateRevisionConflict,
            EngineError::ModelManager(cookie_agent_models::ModelManagerError::ProviderStore(cookie_agent_models::provider_store::ProviderStoreError::StaleConnectionGeneration)) => ProviderDisconnectErrorCode::StaleProviderConnectionGeneration,
            EngineError::ModelManager(cookie_agent_models::ModelManagerError::ProviderStore(cookie_agent_models::provider_store::ProviderStoreError::IdempotencyConflict)) => ProviderDisconnectErrorCode::IdempotencyConflict,
            EngineError::ModelManager(cookie_agent_models::ModelManagerError::ProviderStore(_)) => ProviderDisconnectErrorCode::ProviderStoreWriteFailed,
            _ => ProviderDisconnectErrorCode::RuntimeCompileFailed,
        };
        Self {
            code: -32012,
            message: "provider disconnect error",
            data: typed_error_data(ProviderDisconnectError {
                code,
                provider_id: request.provider_id.clone(),
                client_request_id: request.client_request_id.clone(),
            }),
        }
    }

    pub(crate) fn rename(request: &SessionRenameParams, code: SessionRenameErrorCode) -> Self {
        Self {
            code: -32602,
            message: "session rename rejected",
            data: typed_error_data(SessionRenameError {
                code,
                session_id: request.session_id,
                client_rename_id: request.client_rename_id.clone(),
            }),
        }
    }

    pub(crate) fn approval(
        request: &ApprovalRespondParams,
        code: ApprovalRespondErrorCode,
        expected_revision: Option<u64>,
        found_revision: Option<u64>,
    ) -> Self {
        Self {
            code: -32602,
            message: "approval response rejected",
            data: typed_error_data(ApprovalRespondError {
                code,
                session_id: request.session_id,
                approval_id: request.approval_id,
                client_response_id: request.client_response_id.clone(),
                expected_revision,
                found_revision,
            }),
        }
    }

    pub(crate) fn approval_failure(
        request: &ApprovalRespondParams,
        failure: &ApprovalRespondFailure,
    ) -> Self {
        let (expected_revision, found_revision) =
            if failure.code == ApprovalRespondErrorCode::ApprovalRevisionConflict {
                (failure.current_revision, Some(request.request_revision))
            } else {
                (None, None)
            };
        let client_response_id = ClientResponseId::new(failure.client_response_id.clone())
            .unwrap_or_else(|_| ClientResponseId::new("invalid-response-id").expect("static ID"));
        Self {
            code: -32602,
            message: "approval response rejected",
            data: typed_error_data(ApprovalRespondError {
                code: failure.code,
                session_id: failure.session_id,
                approval_id: failure.approval_id,
                client_response_id,
                expected_revision,
                found_revision,
            }),
        }
    }

    pub(crate) fn session_tree_usage(error: EngineError) -> Self {
        match error {
            EngineError::Session(SessionError::Missing(_)) => Self {
                code: SESSION_TREE_USAGE_MISSING_SESSION_CODE,
                message: "session tree usage session not found",
                data: None,
            },
            EngineError::DelegationEvents(DelegationEventError::Corrupt(_)) => Self {
                code: SESSION_TREE_USAGE_CORRUPT_DELEGATION_CODE,
                message: "session tree usage corrupted delegation record",
                data: None,
            },
            error => engine_fault(error),
        }
    }
}

impl From<RpcFault> for cookie_agent_protocol::ServerFault {
    fn from(fault: RpcFault) -> Self {
        Self::new(fault.code, fault.message, fault.data)
    }
}

fn typed_error_data(error: impl Serialize) -> Option<Value> {
    serde_json::to_value(error).ok()
}

pub(crate) fn engine_fault(error: EngineError) -> RpcFault {
    match error {
        EngineError::NoRunnableModel => RpcFault {
            code: -32020,
            message: "no runnable model",
            data: Some(json!({
                "code": "no_runnable_model",
                "message": "type /connect to continue"
            })),
        },
        EngineError::ProviderStoreReloadFailed => RpcFault {
            code: -32021,
            message: "provider store reload failed",
            data: Some(json!({ "code": "provider_store_reload_failed" })),
        },
        _ => RpcFault::engine(),
    }
}

#[cfg(test)]
mod tests {
    use cookie_agent_protocol::{InvocationId, SessionId};

    use super::*;

    #[test]
    fn session_tree_usage_faults_distinguish_missing_and_corruption() {
        let missing: cookie_agent_protocol::ServerFault = RpcFault::session_tree_usage(
            EngineError::Session(SessionError::Missing(SessionId::new_v7())),
        )
        .into();
        let corrupt: cookie_agent_protocol::ServerFault = RpcFault::session_tree_usage(
            EngineError::DelegationEvents(DelegationEventError::Corrupt(InvocationId::new_v7())),
        )
        .into();

        assert_eq!(missing.code, SESSION_TREE_USAGE_MISSING_SESSION_CODE);
        assert_eq!(missing.message, "session tree usage session not found");
        assert_eq!(corrupt.code, SESSION_TREE_USAGE_CORRUPT_DELEGATION_CODE);
        assert_eq!(
            corrupt.message,
            "session tree usage corrupted delegation record"
        );
        assert_ne!(missing.code, corrupt.code);
    }
}
