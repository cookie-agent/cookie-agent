//! Every application error the daemon answers with, built from the error
//! registry (`cookie_agent_protocol::errors`): a stable `data.code`, a
//! message and hint from the registry, and the engine's own description as
//! the detail.

use cookie_agent_engine::{
    ApprovalRespondFailure, EngineError, MediaError, delegation_events::DelegationEventError,
    events::EventLogError, session::SessionError,
};
use cookie_agent_models::{ModelManagerError, provider_store::ProviderStoreError};
use cookie_agent_protocol::{
    ApprovalRespondErrorCode, ApprovalRespondParams, ClientResponseId, ClientRunId,
    ConfigApplyErrorCode, JsonRpcError, ProviderConnectParams, ProviderDisconnectParams, SessionId,
    SessionRenameParams,
    errors::{self, ErrorBuilder},
};

#[derive(Debug)]
pub(crate) struct RpcFault(JsonRpcError);

impl RpcFault {
    fn of(builder: ErrorBuilder) -> Self {
        Self(builder.build())
    }

    /// A failure the daemon cannot describe further, such as a worker that
    /// stopped before answering.
    pub(crate) fn internal() -> Self {
        Self::of(errors::INTERNAL.error())
    }

    /// `run.start` or `session.submit_input` on a session that is running.
    pub(crate) fn session_running(session_id: SessionId, client_run_id: &ClientRunId) -> Self {
        Self::of(
            errors::SESSION_RUNNING
                .error()
                .field("session_id", session_id)
                .field("client_run_id", client_run_id),
        )
    }

    /// A run key reused with other params.
    pub(crate) fn run_idempotency_conflict(
        session_id: SessionId,
        client_run_id: &ClientRunId,
    ) -> Self {
        Self::of(
            errors::IDEMPOTENCY_CONFLICT
                .error()
                .field("session_id", session_id)
                .field("client_run_id", client_run_id),
        )
    }

    pub(crate) fn provider_connect(request: &ProviderConnectParams, error: EngineError) -> Self {
        let code = match &error {
            EngineError::ModelManager(error) => match error {
                ModelManagerError::UnknownProvider => &errors::UNKNOWN_PROVIDER,
                ModelManagerError::UnsupportedProvider => &errors::UNSUPPORTED_PROVIDER,
                ModelManagerError::RemovedWithoutRetainedRecipeMatch => {
                    &errors::REMOVED_WITHOUT_RETAINED_RECIPE_MATCH
                }
                ModelManagerError::CustomProviderNotStoreBacked => {
                    &errors::CUSTOM_PROVIDER_NOT_STORE_BACKED
                }
                ModelManagerError::InvalidSetup => &errors::INVALID_SETUP_FIELD,
                ModelManagerError::MissingSetup => &errors::MISSING_SETUP_FIELD,
                ModelManagerError::MissingCredentials => &errors::MISSING_CREDENTIAL,
                ModelManagerError::UnsupportedAuthMethod => &errors::UNSUPPORTED_AUTH_METHOD,
                ModelManagerError::InvalidCredentials => &errors::INVALID_CREDENTIAL,
                ModelManagerError::ProviderStore(ProviderStoreError::CatalogRevisionConflict) => {
                    &errors::CATALOG_REVISION_CONFLICT
                }
                ModelManagerError::ProviderStore(ProviderStoreError::IdempotencyConflict) => {
                    &errors::IDEMPOTENCY_CONFLICT
                }
                ModelManagerError::ProviderStore(ProviderStoreError::Contention)
                | ModelManagerError::Contention => &errors::LOCK_CONTENTION,
                ModelManagerError::ProviderStore(_) => &errors::PROVIDER_STORE_WRITE_FAILED,
                _ => &errors::RUNTIME_COMPILE_FAILED,
            },
            _ => &errors::RUNTIME_COMPILE_FAILED,
        };
        let mut builder = code
            .error()
            .field("provider_id", &request.provider_id)
            .detail(error.user_message());
        if std::ptr::eq(code, &errors::IDEMPOTENCY_CONFLICT) {
            builder = builder.field("client_connect_id", &request.client_connect_id);
        }
        Self::of(builder)
    }

    pub(crate) fn provider_disconnect(
        request: &ProviderDisconnectParams,
        error: EngineError,
    ) -> Self {
        let code = match &error {
            EngineError::ModelManager(error) => match error {
                ModelManagerError::CustomProviderNotStoreBacked => {
                    &errors::CUSTOM_PROVIDER_NOT_STORE_BACKED
                }
                ModelManagerError::ProviderStore(ProviderStoreError::RuntimeRevisionConflict) => {
                    &errors::RUNTIME_REVISION_CONFLICT
                }
                ModelManagerError::ProviderStore(
                    ProviderStoreError::ProviderStateRevisionConflict,
                ) => &errors::PROVIDER_STATE_REVISION_CONFLICT,
                ModelManagerError::ProviderStore(ProviderStoreError::StaleConnectionGeneration) => {
                    &errors::STALE_PROVIDER_CONNECTION_GENERATION
                }
                ModelManagerError::ProviderStore(ProviderStoreError::IdempotencyConflict) => {
                    &errors::IDEMPOTENCY_CONFLICT
                }
                ModelManagerError::ProviderStore(ProviderStoreError::Contention)
                | ModelManagerError::Contention => &errors::LOCK_CONTENTION,
                ModelManagerError::ProviderStore(_) => &errors::PROVIDER_STORE_WRITE_FAILED,
                _ => &errors::RUNTIME_COMPILE_FAILED,
            },
            _ => &errors::RUNTIME_COMPILE_FAILED,
        };
        let mut builder = code
            .error()
            .field("provider_id", &request.provider_id)
            .detail(error.user_message());
        if std::ptr::eq(code, &errors::IDEMPOTENCY_CONFLICT) {
            builder = builder.field("client_request_id", &request.client_request_id);
        }
        Self::of(builder)
    }

    /// A refused `config.apply`: a conflict, an invalid result, or a change
    /// that cannot be made, with why as the detail; anything else is an
    /// ordinary engine fault.
    pub(crate) fn config_apply(error: EngineError) -> Self {
        let (reason, detail) = match error {
            EngineError::ConfigConflict(message) => (ConfigApplyErrorCode::Conflict, message),
            EngineError::ConfigChangeRefused(message) => {
                (ConfigApplyErrorCode::InvalidChange, message)
            }
            EngineError::Config(error) => (
                ConfigApplyErrorCode::InvalidConfiguration,
                error.to_string(),
            ),
            error => return engine_fault(error),
        };
        Self::of(
            errors::CONFIG_APPLY_REFUSED
                .error()
                .field("reason", reason)
                .detail(detail),
        )
    }

    pub(crate) fn rename(request: &SessionRenameParams, error: EngineError) -> Self {
        let builder = match &error {
            EngineError::RenameConflict => errors::IDEMPOTENCY_CONFLICT
                .error()
                .field("client_rename_id", &request.client_rename_id),
            EngineError::Session(SessionError::Missing(_)) | EngineError::MissingActor(_) => {
                errors::SESSION_NOT_FOUND.error()
            }
            _ => return engine_fault(error),
        };
        Self::of(builder.field("session_id", request.session_id))
    }

    pub(crate) fn approval(request: &ApprovalRespondParams, error: EngineError) -> Self {
        match error {
            EngineError::ApprovalResponse(failure) => Self::approval_failure(request, &failure),
            EngineError::ApprovalNotPending { .. } => Self::approval_code(
                request,
                ApprovalRespondErrorCode::ApprovalNotPending,
                None,
                None,
            ),
            EngineError::Session(SessionError::Missing(_)) | EngineError::MissingActor(_) => {
                Self::approval_code(
                    request,
                    ApprovalRespondErrorCode::ApprovalNotFound,
                    None,
                    None,
                )
            }
            error => engine_fault(error),
        }
    }

    fn approval_code(
        request: &ApprovalRespondParams,
        code: ApprovalRespondErrorCode,
        expected_revision: Option<u64>,
        found_revision: Option<u64>,
    ) -> Self {
        let mut builder = code
            .error_code()
            .error()
            .field("approval_id", request.approval_id);
        match code {
            ApprovalRespondErrorCode::ApprovalRevisionConflict => {
                builder = builder
                    .field("expected_revision", expected_revision)
                    .field("found_revision", found_revision);
            }
            ApprovalRespondErrorCode::IdempotencyConflict => {
                builder = builder
                    .field("session_id", request.session_id)
                    .field("client_response_id", &request.client_response_id);
            }
            _ => {}
        }
        Self::of(builder)
    }

    fn approval_failure(request: &ApprovalRespondParams, failure: &ApprovalRespondFailure) -> Self {
        let (expected_revision, found_revision) =
            if failure.code == ApprovalRespondErrorCode::ApprovalRevisionConflict {
                (failure.current_revision, Some(request.request_revision))
            } else {
                (None, None)
            };
        let client_response_id = ClientResponseId::new(failure.client_response_id.clone())
            .unwrap_or_else(|_| request.client_response_id.clone());
        let request = ApprovalRespondParams {
            session_id: failure.session_id,
            approval_id: failure.approval_id,
            client_response_id,
            ..request.clone()
        };
        Self::approval_code(&request, failure.code, expected_revision, found_revision)
    }

    pub(crate) fn session_tree_usage(error: EngineError) -> Self {
        match error {
            EngineError::Session(SessionError::Missing(session_id)) => Self::of(
                errors::TREE_USAGE_SESSION_MISSING
                    .error()
                    .field("session_id", session_id),
            ),
            EngineError::DelegationEvents(DelegationEventError::Corrupt(invocation_id)) => {
                Self::of(
                    errors::TREE_USAGE_DELEGATION_CORRUPT
                        .error()
                        .field("invocation_id", invocation_id),
                )
            }
            error => engine_fault(error),
        }
    }
}

impl From<RpcFault> for cookie_agent_protocol::ServerFault {
    fn from(fault: RpcFault) -> Self {
        Self::from_error(fault.0)
    }
}

/// An engine reason as a message clause: bounded, and without the closing
/// period the message template adds.
fn reason_text(reason: &str) -> String {
    cookie_agent_protocol::diagnostics::detail(reason.trim().trim_end_matches('.')).to_string()
}

/// The session-log part of `error`, if it is one.
fn event_log_error(error: &EngineError) -> Option<&EventLogError> {
    match error {
        EngineError::Event(error) | EngineError::Session(SessionError::Event(error)) => Some(error),
        _ => None,
    }
}

/// The registry error that answers `error`: a specific code where the
/// engine's error names one, else `engine_error`. The engine's own
/// description is the detail.
pub(crate) fn engine_fault(error: EngineError) -> RpcFault {
    let detail = error.user_message();
    match event_log_error(&error) {
        Some(EventLogError::RequiresNewerEngine {
            event,
            history_format,
            ..
        }) => {
            return RpcFault::of(
                errors::SESSION_REQUIRES_NEWER_ENGINE
                    .error()
                    .field("event", event)
                    .field("history_format", history_format)
                    .detail(detail),
            );
        }
        Some(EventLogError::SequenceBroken { seq, .. }) => {
            return RpcFault::of(
                errors::SEQUENCE_BROKEN
                    .error()
                    .field("seq", seq)
                    .detail(detail),
            );
        }
        _ => {}
    }
    let builder = match &error {
        EngineError::Media(error) => media_error(error),
        EngineError::PluginHookFailed(failure) => errors::PLUGIN_HOOK_FAILED
            .error()
            .field("plugin", &failure.plugin)
            .field("hook", failure.hook.name()),
        EngineError::NoRunnableModel => errors::NO_RUNNABLE_MODEL.error(),
        EngineError::OnboardingIncomplete(missing) => errors::ONBOARDING_INCOMPLETE.error().field(
            "missing",
            missing
                .iter()
                .copied()
                .map(crate::onboarding::requirement_to_wire)
                .collect::<Vec<_>>(),
        ),
        EngineError::OnboardingKit(_) => errors::ONBOARDING_KIT_UNAVAILABLE.error(),
        EngineError::ProviderStoreReloadFailed => errors::PROVIDER_STORE_RELOAD_FAILED.error(),
        EngineError::RuntimeCompileFailed => errors::RUNTIME_COMPILE_FAILED.error(),
        EngineError::SessionOwnedByAnotherProcess(session_id) => errors::SESSION_OWNED_ELSEWHERE
            .error()
            .field("session_id", session_id),
        EngineError::Session(SessionError::Missing(session_id))
        | EngineError::MissingActor(session_id) => errors::SESSION_NOT_FOUND
            .error()
            .field("session_id", session_id),
        EngineError::SessionRunning(session_id) => errors::SESSION_RUNNING
            .error()
            .field("session_id", session_id),
        EngineError::MissingRun(run_id) => errors::RUN_NOT_FOUND.error().field("run_id", run_id),
        EngineError::Restarting => errors::DAEMON_RESTARTING.error(),
        EngineError::RestartUnsupported => errors::RESTART_UNSUPPORTED.error(),
        EngineError::StaleRevert {
            session_id,
            current_last_seq,
        } => errors::STALE_REVERT
            .error()
            .field("session_id", session_id)
            .field("current_last_seq", current_last_seq),
        EngineError::NotARootSession {
            session_id,
            root_session_id,
        } => errors::NOT_A_ROOT_SESSION
            .error()
            .field("session_id", session_id)
            .field("root_session_id", root_session_id),
        EngineError::SessionTreeRunning { root, busy } => errors::SESSION_TREE_RUNNING
            .error()
            .field("session_id", root)
            .field("running_session_id", busy),
        EngineError::StaleDelete {
            session_id,
            current_last_seq,
        } => errors::SESSION_CHANGED
            .error()
            .field("session_id", session_id)
            .field("current_last_seq", current_last_seq),
        EngineError::ForkIdempotencyConflict
        | EngineError::RunIdempotencyConflict
        | EngineError::RenameConflict => errors::IDEMPOTENCY_CONFLICT.error(),
        EngineError::Goal(reason) => errors::GOAL_REJECTED
            .error()
            .field("reason", reason_text(reason)),
        EngineError::Producer(_) => errors::PRODUCER_REJECTED.error(),
        EngineError::Plugin(reason)
        | EngineError::InputHandled(reason)
        | EngineError::ModelSelectionBlocked(reason)
        | EngineError::SessionOperationBlocked(reason)
        | EngineError::CompactionCancelled(reason) => errors::PLUGIN_REJECTED
            .error()
            .field("reason", reason_text(reason)),
        EngineError::Runtime(reason) => errors::RUNTIME_REJECTED
            .error()
            .field("reason", reason_text(reason)),
        EngineError::Machine(error) => machine_error(error),
        EngineError::MemoryRequest(error) => memory_error(error),
        EngineError::Project(error) => project_error(error),
        EngineError::Cron(error) => cron_error(error),
        EngineError::Mcp(reason) => errors::MCP_REJECTED
            .error()
            .field("reason", reason_text(reason)),
        EngineError::UnreadableModelBinding(model) => errors::UNREADABLE_MODEL_BINDING
            .error()
            .field("model", model),
        EngineError::IneligibleAgent(agent) | EngineError::DisabledAgent(agent) => {
            errors::AGENT_UNAVAILABLE.error().field("agent", agent)
        }
        EngineError::UnknownAgentModel { agent, .. }
        | EngineError::UnavailableAgentModel { agent, .. } => {
            errors::AGENT_UNAVAILABLE.error().field("agent", agent)
        }
        EngineError::UnknownAgentPreset(_) => errors::AGENT_UNAVAILABLE.error(),
        EngineError::ApprovalNotPending { approval_id, .. } => errors::APPROVAL_NOT_PENDING
            .error()
            .field("approval_id", approval_id),
        _ => errors::ENGINE_FAILED.error(),
    };
    RpcFault::of(builder.detail(detail))
}

/// The registry error of a refused upload, read, or input media.
fn media_error(error: &MediaError) -> ErrorBuilder {
    match error {
        MediaError::UploadNotFound => errors::UPLOAD_NOT_FOUND.error(),
        MediaError::OffsetMismatch {
            expected_offset, ..
        } => errors::UPLOAD_OFFSET_MISMATCH
            .error()
            .field("expected_offset", expected_offset),
        MediaError::SizeMismatch {
            byte_length,
            received,
        } => errors::UPLOAD_SIZE_MISMATCH
            .error()
            .field("byte_length", byte_length)
            .field("received", received),
        MediaError::TooManyUploads { max_uploads } => errors::TOO_MANY_UPLOADS
            .error()
            .field("max_uploads", max_uploads),
        MediaError::DigestMismatch { .. } => errors::DIGEST_MISMATCH.error(),
        MediaError::TypeUnsupported { media_type, .. } => errors::MEDIA_TYPE_UNSUPPORTED
            .error()
            .field("media_type", media_type),
        MediaError::TooLarge {
            kind,
            byte_length,
            max_bytes,
            width,
            height,
            max_dimension,
            max_pixels,
        } => errors::MEDIA_TOO_LARGE
            .error()
            .field("kind", kind)
            .field("byte_length", byte_length)
            .field("max_bytes", max_bytes)
            .field("width", width)
            .field("height", height)
            .field("max_dimension", max_dimension)
            .field("max_pixels", max_pixels),
        MediaError::Unsupported { kind, model } => errors::MEDIA_UNSUPPORTED
            .error()
            .field("kind", kind)
            .field("model", model),
        MediaError::LimitExceeded {
            kind,
            count,
            max_count,
        } => errors::MEDIA_LIMIT_EXCEEDED
            .error()
            .field("kind", kind)
            .field("count", count)
            .field("max_count", max_count),
        MediaError::ArtifactNotFound { .. } => errors::ARTIFACT_NOT_FOUND.error(),
        MediaError::InvalidRequest(_) => errors::BAD_PARAMS.error(),
    }
}

/// The registry entry `error` is answered with.
#[cfg(test)]
fn code_of(fault: RpcFault) -> &'static errors::ErrorCode {
    let code = fault
        .0
        .data
        .as_ref()
        .and_then(|data| data["code"].as_str())
        .expect("registry data");
    errors::lookup(code).expect("a registered code")
}

fn memory_error(error: &cookie_agent_engine::MemoryRequestError) -> errors::ErrorBuilder {
    use cookie_agent_engine::MemoryRequestError;
    match error {
        MemoryRequestError::InvalidUrl { url, reason } => errors::MEMORY_URL_INVALID
            .error()
            .field("url", url)
            .field("reason", reason_text(reason)),
        MemoryRequestError::InvalidParams { field, .. } => {
            errors::BAD_PARAMS.error().field("field", field)
        }
        MemoryRequestError::NotFound { url } => errors::MEMORY_NOT_FOUND.error().field("url", url),
        MemoryRequestError::Exists { url, revision } => errors::MEMORY_EXISTS
            .error()
            .field("url", url)
            .field("revision", revision),
        MemoryRequestError::RevisionMismatch { url, revision } => errors::MEMORY_REVISION_MISMATCH
            .error()
            .field("url", url)
            .field("revision", revision),
        MemoryRequestError::Invalid { url, reason } => errors::MEMORY_INVALID
            .error()
            .field("url", url)
            .field("reason", reason_text(reason)),
        MemoryRequestError::Refused { url, reason } => errors::MEMORY_REFUSED
            .error()
            .field("url", url)
            .field("reason", reason_text(reason)),
        MemoryRequestError::Busy(_) => errors::MEMORY_BUSY.error(),
        MemoryRequestError::Store(_) => errors::MEMORY_STORE_FAILED.error(),
    }
}

fn project_error(error: &cookie_agent_engine::ProjectError) -> errors::ErrorBuilder {
    use cookie_agent_engine::ProjectError;
    match error {
        ProjectError::NotFound(project) => {
            errors::PROJECT_NOT_FOUND.error().field("project", project)
        }
        ProjectError::NameTaken(name) => errors::PROJECT_NAME_TAKEN.error().field("name", name),
        ProjectError::Changed { project, revision } => errors::PROJECT_CHANGED
            .error()
            .field("project", project)
            .field("revision", revision),
        ProjectError::Invalid { field, reason } => errors::PROJECT_INVALID
            .error()
            .field("field", field)
            .field("reason", reason_text(reason)),
        ProjectError::SessionsRunning { project, sessions } => errors::PROJECT_SESSIONS_RUNNING
            .error()
            .field("project", project)
            .field("session_ids", sessions),
        ProjectError::RootSessionOnly {
            session_id,
            root_session_id,
        } => errors::PROJECT_ROOT_SESSION_ONLY
            .error()
            .field("session_id", session_id)
            .field("root_session_id", root_session_id),
        ProjectError::Store(_) => errors::PROJECT_STORE_FAILED.error(),
    }
}

fn cron_error(error: &cookie_agent_engine::CronError) -> errors::ErrorBuilder {
    use cookie_agent_engine::CronError;
    match error {
        CronError::NotFound(id) => errors::CRON_NOT_FOUND.error().field("id", id),
        CronError::Invalid { field, reason } => errors::CRON_INVALID
            .error()
            .field("field", field)
            .field("reason", reason_text(reason)),
        CronError::Store(_) => errors::CRON_STORE_FAILED.error(),
    }
}

fn machine_error(error: &cookie_agent_engine::MachineError) -> errors::ErrorBuilder {
    use cookie_agent_engine::MachineError;
    match error {
        MachineError::NotFound(machine) => errors::MACHINE_NOT_FOUND
            .error()
            .field("machine", machine.as_str()),
        MachineError::NameTaken(name) => errors::MACHINE_NAME_TAKEN
            .error()
            .field("name", name.as_str()),
        MachineError::InUse(machine) => errors::MACHINE_IN_USE
            .error()
            .field("machine", machine.as_str()),
        MachineError::MemoryConflict { from, into, name } => errors::MACHINE_MEMORY_CONFLICT
            .error()
            .field("from", from.as_str())
            .field("into", into.as_str())
            .field("name", name.as_str()),
        MachineError::MergeSelf(machine) => errors::MACHINE_MERGE_SELF
            .error()
            .field("machine", machine.as_str()),
        MachineError::NotPinned(runtime) => errors::RUNTIME_NOT_PINNED
            .error()
            .field("runtime", runtime.as_str()),
        MachineError::NotRemembered(runtime) => errors::RUNTIME_NOT_REMEMBERED
            .error()
            .field("runtime", runtime.as_str()),
        MachineError::StillConnected(runtime) => errors::RUNTIME_STILL_CONNECTED
            .error()
            .field("runtime", runtime.as_str()),
        MachineError::Store(_) => errors::MACHINE_STORE_FAILED.error(),
    }
}

#[cfg(test)]
mod tests {
    use cookie_agent_protocol::{InvocationId, ServerFault, SessionId};
    use serde_json::{Value, json};

    use super::*;

    fn data(fault: RpcFault) -> Value {
        ServerFault::from(fault).data.expect("registry data")
    }

    #[test]
    fn a_session_a_newer_build_wrote_refuses_with_its_registry_code() {
        for (error, event, history_format) in [
            (
                EngineError::Event(EventLogError::RequiresNewerEngine {
                    path: "events.jsonl".into(),
                    event: Some("future_state".into()),
                    history_format: None,
                }),
                json!("future_state"),
                Value::Null,
            ),
            (
                EngineError::Session(SessionError::Event(EventLogError::RequiresNewerEngine {
                    path: "events.jsonl".into(),
                    event: None,
                    history_format: Some(2),
                })),
                Value::Null,
                json!(2),
            ),
        ] {
            let fault: ServerFault = engine_fault(error).into();
            assert_eq!(fault.code, errors::APPLICATION_ERROR);
            assert_eq!(fault.message, errors::SESSION_REQUIRES_NEWER_ENGINE.message);
            let data = fault.data.expect("registry data");
            assert_eq!(data["code"], "session_requires_newer_engine");
            assert_eq!(data["hint"], "Update cookie to continue this session.");
            assert_eq!(data.get("event").cloned().unwrap_or(Value::Null), event);
            assert_eq!(
                data.get("history_format").cloned().unwrap_or(Value::Null),
                history_format
            );
        }
    }

    #[test]
    fn a_broken_sequence_has_its_own_code() {
        let fault: ServerFault = engine_fault(EngineError::Event(EventLogError::SequenceBroken {
            path: "events.jsonl".into(),
            seq: 41,
        }))
        .into();
        assert_eq!(
            fault.message,
            "This session's log is damaged after event 41, so it can only be viewed."
        );
        let data = fault.data.expect("registry data");
        assert_eq!(data["code"], "sequence_broken");
        assert_eq!(data["seq"], 41);
    }

    #[test]
    fn a_failed_blocking_hook_is_the_registry_plugin_hook_failure_and_names_both() {
        let fault: ServerFault = engine_fault(EngineError::PluginHookFailed(
            cookie_agent_engine::PluginHookFailure {
                plugin: "policy".into(),
                hook: cookie_agent_protocol::ExtensionInterceptionHook::SessionBeforeFork,
                reason: "timed out".into(),
            },
        ))
        .into();
        assert_eq!(fault.code, errors::APPLICATION_ERROR);
        assert!(
            fault.message.contains("policy") && fault.message.contains("session_before_fork"),
            "{}",
            fault.message
        );
        let data = fault.data.expect("registry data");
        assert_eq!(data["code"], "plugin_hook_failed");
        assert_eq!(data["plugin"], "policy");
        assert_eq!(data["hook"], "session_before_fork");
        assert!(data["hint"].as_str().is_some_and(|hint| !hint.is_empty()));
        assert!(
            data["detail"]
                .as_str()
                .is_some_and(|detail| detail.contains("`session_before_fork` hook timed out")),
            "{data}"
        );
    }

    #[test]
    fn run_conflicts_name_the_session_and_the_run_key() {
        let session_id = SessionId::new_v7();
        let client_run_id = ClientRunId::new("second").unwrap();
        for (fault, code) in [
            (
                RpcFault::session_running(session_id, &client_run_id),
                "session_running",
            ),
            (
                RpcFault::run_idempotency_conflict(session_id, &client_run_id),
                "idempotency_conflict",
            ),
        ] {
            let fault: ServerFault = fault.into();
            assert_eq!(fault.code, errors::APPLICATION_ERROR);
            let data = fault.data.expect("registry data");
            assert_eq!(data["code"], code);
            assert_eq!(data["session_id"], json!(session_id));
            assert_eq!(data["client_run_id"], "second");
        }
    }

    #[test]
    fn provider_connect_faults_name_the_failure() {
        let request = ProviderConnectParams {
            provider_id: cookie_agent_protocol::ProviderId::new("openai").unwrap(),
            expected_revision: cookie_agent_protocol::ViewRevision::new("view-1").unwrap(),
            setup_values: std::collections::BTreeMap::new(),
            auth_method: cookie_agent_protocol::AuthMethodId::new("bearer-api-key-v1").unwrap(),
            auth_values: serde_json::from_value(json!({"api_key": "placeholder"})).unwrap(),
            client_connect_id: cookie_agent_protocol::ClientConnectId::new("connect-1").unwrap(),
        };
        for (error, expected) in [
            (
                ModelManagerError::CustomProviderNotStoreBacked,
                &errors::CUSTOM_PROVIDER_NOT_STORE_BACKED,
            ),
            (
                ModelManagerError::MissingSetup,
                &errors::MISSING_SETUP_FIELD,
            ),
            (
                ModelManagerError::InvalidSetup,
                &errors::INVALID_SETUP_FIELD,
            ),
            (
                ModelManagerError::MissingCredentials,
                &errors::MISSING_CREDENTIAL,
            ),
            (
                ModelManagerError::InvalidCredentials,
                &errors::INVALID_CREDENTIAL,
            ),
            (ModelManagerError::Contention, &errors::LOCK_CONTENTION),
        ] {
            let fault = RpcFault::provider_connect(&request, EngineError::ModelManager(error));
            assert_eq!(code_of(fault), expected);
        }
        let data = data(RpcFault::provider_connect(
            &request,
            EngineError::ModelManager(ModelManagerError::ProviderStore(
                ProviderStoreError::IdempotencyConflict,
            )),
        ));
        assert_eq!(data["code"], "idempotency_conflict");
        assert_eq!(data["provider_id"], "openai");
        assert_eq!(data["client_connect_id"], "connect-1");
        assert!(!data.to_string().contains("placeholder"), "{data}");
    }

    #[test]
    fn config_apply_faults_say_why_nothing_changed() {
        for (error, expected) in [
            (EngineError::ConfigConflict("changed".into()), "conflict"),
            (
                EngineError::ConfigChangeRefused("bad path".into()),
                "invalid_change",
            ),
            (
                EngineError::Config(Box::new(cookie_agent_config::ConfigError::UnsafePath)),
                "invalid_configuration",
            ),
        ] {
            let data = data(RpcFault::config_apply(error));
            assert_eq!(data["code"], "config_apply_refused");
            assert_eq!(data["reason"], expected);
            assert!(
                data["detail"]
                    .as_str()
                    .is_some_and(|detail| !detail.is_empty())
            );
        }
        assert_eq!(
            code_of(RpcFault::config_apply(EngineError::NoRunnableModel)),
            &errors::NO_RUNNABLE_MODEL
        );
    }

    #[test]
    fn an_engine_fault_keeps_the_cause_as_detail_and_shows_message_and_hint() {
        let fault: ServerFault = engine_fault(EngineError::Config(Box::new(
            cookie_agent_config::ConfigError::FileIo {
                path: "/workspace/config.toml".into(),
                source: std::io::Error::new(std::io::ErrorKind::PermissionDenied, "access denied"),
            },
        )))
        .into();
        let client = cookie_agent_protocol::ClientError::Rpc(JsonRpcError {
            code: fault.code,
            message: fault.message.into_owned(),
            data: fault.data,
        });
        assert_eq!(client.error_code(), Some("engine_error"));
        assert_eq!(
            client.to_string(),
            format!(
                "{} {}",
                errors::ENGINE_FAILED.message,
                errors::ENGINE_FAILED.hint.unwrap()
            )
        );
        let detail = client.detail().expect("detail");
        assert!(detail.contains("/workspace/config.toml") && detail.contains("access denied"));
    }

    #[test]
    fn session_tree_usage_faults_distinguish_missing_and_corruption() {
        let session_id = SessionId::new_v7();
        let missing = data(RpcFault::session_tree_usage(EngineError::Session(
            SessionError::Missing(session_id),
        )));
        let invocation_id = InvocationId::new_v7();
        let corrupt = data(RpcFault::session_tree_usage(EngineError::DelegationEvents(
            DelegationEventError::Corrupt(invocation_id),
        )));
        assert_eq!(missing["code"], "tree_usage_session_missing");
        assert_eq!(missing["session_id"], json!(session_id));
        assert_eq!(corrupt["code"], "tree_usage_delegation_corrupt");
        assert_eq!(corrupt["invocation_id"], json!(invocation_id));
    }

    #[test]
    fn engine_errors_map_to_their_codes_and_debug_codes_are_gone() {
        let session_id = SessionId::new_v7();
        for (error, expected) in [
            (
                EngineError::SessionOwnedByAnotherProcess(session_id),
                &errors::SESSION_OWNED_ELSEWHERE,
            ),
            (
                EngineError::StaleRevert {
                    session_id,
                    current_last_seq: 9,
                },
                &errors::STALE_REVERT,
            ),
            (
                EngineError::ForkIdempotencyConflict,
                &errors::IDEMPOTENCY_CONFLICT,
            ),
            (EngineError::Goal("no".into()), &errors::GOAL_REJECTED),
            (
                EngineError::Producer("no".into()),
                &errors::PRODUCER_REJECTED,
            ),
            (
                EngineError::InputHandled("Bearer plugin-secret".into()),
                &errors::PLUGIN_REJECTED,
            ),
            (EngineError::NoRunnableModel, &errors::NO_RUNNABLE_MODEL),
            (EngineError::ActorStopped, &errors::ENGINE_FAILED),
        ] {
            let data = data(engine_fault(error));
            assert_eq!(data["code"], expected.code);
            assert!(data.get("debug_code").is_none(), "{data}");
        }
    }
}
