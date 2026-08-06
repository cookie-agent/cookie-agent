use cookie_agent_engine::{ApprovalRespondFailure, EngineError};
use cookie_agent_protocol::{
    ApprovalRespondError, ApprovalRespondErrorCode, ApprovalRespondParams, ClientRenameId,
    ClientResponseId, ErrorResponse, JsonRpcError, JsonRpcId, JsonRpcVersion, Notification,
    ProviderConnectError, ProviderConnectErrorCode, ProviderConnectParams, ProviderDisconnectError,
    ProviderDisconnectErrorCode, ProviderDisconnectParams, Request as RpcRequest,
    Response as RpcResponse, RunStartConflict, RunStartConflictCode, RunStartParams, SessionId,
    SessionRenameChange, SessionRenameError, SessionRenameErrorCode, SessionRenameParams,
    SessionTitle, SuccessResponse,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};

use crate::transport::{MessageFrame, TransportError};

const MAX_RAW_RENAME_PARAMS_BYTES: usize = 4 * 1024;

pub(crate) enum RouteResult {
    Handshake,
    Value(Value),
}

pub(crate) enum Incoming {
    Request {
        id: JsonRpcId,
        method: String,
        params: Option<Value>,
    },
    Notification {
        method: String,
        params: Option<Value>,
    },
}

#[derive(Debug)]
pub(crate) struct RpcFault {
    code: i32,
    message: &'static str,
    data: Option<Value>,
}

impl RpcFault {
    const fn parse_error() -> Self {
        Self {
            code: -32700,
            message: "parse error",
            data: None,
        }
    }

    fn invalid_request(detail: &'static str) -> Self {
        Self {
            code: -32600,
            message: "invalid request",
            data: Some(json!({ "detail": detail })),
        }
    }

    pub(crate) const fn method_not_found() -> Self {
        Self {
            code: -32601,
            message: "method not found",
            data: None,
        }
    }

    const fn invalid_params() -> Self {
        Self {
            code: -32602,
            message: "invalid params",
            data: None,
        }
    }

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

    pub(crate) const fn handshake_required() -> Self {
        Self {
            code: -32001,
            message: "handshake required",
            data: None,
        }
    }

    pub(crate) const fn request_id_required() -> Self {
        Self {
            code: -32600,
            message: "request id required",
            data: None,
        }
    }

    const fn engine() -> Self {
        Self {
            code: -32000,
            message: "engine error",
            data: None,
        }
    }

    pub(crate) const fn internal() -> Self {
        Self {
            code: -32603,
            message: "internal error",
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
        Self::rename_parts(request.session_id, request.client_rename_id.clone(), code)
    }

    fn rename_parts(
        session_id: SessionId,
        client_rename_id: ClientRenameId,
        code: SessionRenameErrorCode,
    ) -> Self {
        Self {
            code: -32602,
            message: "session rename rejected",
            data: typed_error_data(SessionRenameError {
                code,
                session_id,
                client_rename_id,
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

pub(crate) fn parse_incoming(frame: MessageFrame) -> Result<Value, RpcFault> {
    match frame {
        MessageFrame::Value(value) => Ok(value),
        MessageFrame::Text(text) => {
            serde_json::from_str(&text).map_err(|_| RpcFault::parse_error())
        }
    }
}

pub(crate) fn classify_incoming(value: Value) -> Result<Incoming, RpcFault> {
    let object = value
        .as_object()
        .ok_or_else(|| RpcFault::invalid_request("a batch or scalar is not a request"))?;
    if object.contains_key("id") {
        let request: RpcRequest = serde_json::from_value(value)
            .map_err(|_| RpcFault::invalid_request("invalid JSON-RPC request envelope"))?;
        if request.method.is_empty() {
            return Err(RpcFault::invalid_request("method must be non-empty"));
        }
        Ok(Incoming::Request {
            id: request.id,
            method: request.method,
            params: request.params,
        })
    } else {
        let notification: Notification = serde_json::from_value(value)
            .map_err(|_| RpcFault::invalid_request("invalid JSON-RPC notification envelope"))?;
        if notification.method.is_empty() {
            return Err(RpcFault::invalid_request("method must be non-empty"));
        }
        Ok(Incoming::Notification {
            method: notification.method,
            params: notification.params,
        })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSessionRenameParams {
    session_id: SessionId,
    client_rename_id: ClientRenameId,
    change: RawSessionRenameChange,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum RawSessionRenameChange {
    Set { title: String },
    Clear,
    Reset,
}

pub(crate) fn decode_rename_params(params: Option<Value>) -> Result<SessionRenameParams, RpcFault> {
    let params = params.unwrap_or(Value::Null);
    if let Ok(request) = serde_json::from_value::<SessionRenameParams>(params.clone()) {
        return Ok(request);
    }
    if serde_json::to_vec(&params)
        .map_err(|_| RpcFault::invalid_params())?
        .len()
        > MAX_RAW_RENAME_PARAMS_BYTES
    {
        return Err(RpcFault::invalid_params());
    }
    let raw: RawSessionRenameParams =
        serde_json::from_value(params).map_err(|_| RpcFault::invalid_params())?;
    let change = match raw.change {
        RawSessionRenameChange::Set { title } => SessionRenameChange::Set {
            title: SessionTitle::new(title).map_err(|_| {
                RpcFault::rename_parts(
                    raw.session_id,
                    raw.client_rename_id.clone(),
                    SessionRenameErrorCode::InvalidTitle,
                )
            })?,
        },
        RawSessionRenameChange::Clear => SessionRenameChange::Clear,
        RawSessionRenameChange::Reset => SessionRenameChange::Reset,
    };
    Ok(SessionRenameParams {
        session_id: raw.session_id,
        client_rename_id: raw.client_rename_id,
        change,
    })
}

pub(crate) fn decode_params<T: DeserializeOwned>(params: Option<Value>) -> Result<T, RpcFault> {
    serde_json::from_value(params.unwrap_or(Value::Null)).map_err(|_| RpcFault::invalid_params())
}

pub(crate) fn params_or_default<T>(params: Option<Value>) -> Result<T, RpcFault>
where
    T: DeserializeOwned + Default,
{
    match params {
        Some(params) => serde_json::from_value(params).map_err(|_| RpcFault::invalid_params()),
        None => Ok(T::default()),
    }
}

pub(crate) fn value<T: Serialize>(value: T) -> Result<RouteResult, RpcFault> {
    serde_json::to_value(value)
        .map(RouteResult::Value)
        .map_err(|_| RpcFault::internal())
}

pub(crate) fn success_response<T: Serialize>(
    id: JsonRpcId,
    result: &T,
) -> Result<Value, TransportError> {
    serde_json::to_value(RpcResponse::Success(SuccessResponse {
        jsonrpc: JsonRpcVersion::current(),
        id,
        result: serde_json::to_value(result).map_err(|_| TransportError::Closed)?,
    }))
    .map_err(|_| TransportError::Closed)
}

pub(crate) fn error_response(
    id: Option<JsonRpcId>,
    fault: RpcFault,
) -> Result<Value, TransportError> {
    match id {
        Some(id) => serde_json::to_value(RpcResponse::Error(ErrorResponse {
            jsonrpc: JsonRpcVersion::current(),
            id,
            error: JsonRpcError {
                code: fault.code,
                message: fault.message.into(),
                data: fault.data,
            },
        }))
        .map_err(|_| TransportError::Closed),
        None => Ok(json!({
            "jsonrpc": "2.0",
            "id": null,
            "error": { "code": fault.code, "message": fault.message, "data": fault.data },
        })),
    }
}
