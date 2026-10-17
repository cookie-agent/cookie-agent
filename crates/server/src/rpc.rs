use cookie_agent_engine::{
    ApprovalRespondFailure, EngineError, delegation_events::DelegationEventError,
    session::SessionError,
};
use cookie_agent_protocol::{
    ApprovalRespondError, ApprovalRespondErrorCode, ApprovalRespondParams, ClientResponseId,
    ClientRunId, ProviderConnectError, ProviderConnectErrorCode, ProviderConnectParams,
    ProviderDisconnectError, ProviderDisconnectErrorCode, ProviderDisconnectParams,
    RunStartConflict, RunStartConflictCode, SESSION_TREE_USAGE_CORRUPT_DELEGATION_CODE,
    SESSION_TREE_USAGE_MISSING_SESSION_CODE, SessionId, SessionRenameError, SessionRenameErrorCode,
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
    pub(crate) fn run_start_conflict(
        session_id: SessionId,
        client_run_id: &ClientRunId,
        code: RunStartConflictCode,
    ) -> Self {
        Self {
            code: -32602,
            message: match code {
                RunStartConflictCode::IdempotencyConflict => "idempotency conflict",
                RunStartConflictCode::SessionRunning => "session running",
            },
            data: typed_error_data(RunStartConflict {
                code,
                session_id,
                client_run_id: client_run_id.clone(),
            }),
        }
    }

    pub(crate) const fn engine() -> Self {
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
            EngineError::ModelManager(
                cookie_agent_models::ModelManagerError::CustomProviderNotStoreBacked,
            ) => ProviderConnectErrorCode::CustomProviderNotStoreBacked,
            EngineError::ModelManager(cookie_agent_models::ModelManagerError::InvalidSetup) => {
                ProviderConnectErrorCode::InvalidSetupField
            }
            EngineError::ModelManager(cookie_agent_models::ModelManagerError::MissingSetup) => {
                ProviderConnectErrorCode::MissingSetupField
            }
            EngineError::ModelManager(
                cookie_agent_models::ModelManagerError::MissingCredentials,
            ) => ProviderConnectErrorCode::MissingCredential,
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
            EngineError::ModelManager(cookie_agent_models::ModelManagerError::ProviderStore(
                cookie_agent_models::provider_store::ProviderStoreError::Contention,
            )) => ProviderConnectErrorCode::LockContention,
            EngineError::ModelManager(cookie_agent_models::ModelManagerError::Contention) => {
                ProviderConnectErrorCode::LockContention
            }
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

    /// A refused `config.apply`: a conflict, an invalid result, or a change
    /// that cannot be made; anything else is an ordinary engine fault.
    pub(crate) fn config_apply(error: EngineError) -> Self {
        use cookie_agent_protocol::{ConfigApplyError, ConfigApplyErrorCode};
        let code = match &error {
            EngineError::ConfigConflict(_) => ConfigApplyErrorCode::Conflict,
            EngineError::ConfigChangeRefused(_) => ConfigApplyErrorCode::InvalidChange,
            EngineError::Config(_) => ConfigApplyErrorCode::InvalidConfiguration,
            _ => return engine_fault(error),
        };
        let message = match error {
            EngineError::ConfigConflict(message) | EngineError::ConfigChangeRefused(message) => {
                message
            }
            EngineError::Config(error) => error.to_string(),
            _ => unreachable!("matched above"),
        };
        Self {
            code: cookie_agent_protocol::CONFIG_APPLY_ERROR_CODE,
            message: "configuration not changed",
            data: typed_error_data(ConfigApplyError { code, message }),
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
            EngineError::ModelManager(cookie_agent_models::ModelManagerError::ProviderStore(cookie_agent_models::provider_store::ProviderStoreError::Contention)) => ProviderDisconnectErrorCode::LockContention,
            EngineError::ModelManager(cookie_agent_models::ModelManagerError::Contention) => ProviderDisconnectErrorCode::LockContention,
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
        EngineError::SessionOwnedByAnotherProcess(_) => RpcFault {
            code: -32022,
            message: "session is owned by another cookie process",
            data: None,
        },
        EngineError::StaleRevert {
            session_id,
            current_last_seq,
        } => RpcFault {
            code: -32602,
            message: "stale revert",
            data: typed_error_data(cookie_agent_protocol::SessionRevertConflict {
                session_id,
                current_last_seq,
            }),
        },
        EngineError::ForkIdempotencyConflict => RpcFault {
            code: -32602,
            message: "idempotency conflict",
            data: Some(json!({ "code": "idempotency_conflict" })),
        },
        EngineError::Goal(reason) => RpcFault {
            code: -32602,
            message: "goal operation rejected",
            data: Some(json!({"reason": cookie_agent_protocol::diagnostics::detail(&reason)})),
        },
        EngineError::Producer(reason) => RpcFault {
            code: -32602,
            message: "producer operation rejected",
            data: Some(json!({"reason": cookie_agent_protocol::diagnostics::detail(&reason)})),
        },
        EngineError::Plugin(reason) => RpcFault {
            code: -32602,
            message: "plugin request rejected",
            data: Some(json!({"reason": cookie_agent_protocol::diagnostics::detail(&reason)})),
        },
        error => RpcFault {
            data: Some(json!({"detail": error.user_message()})),
            ..RpcFault::engine()
        },
    }
}

pub(crate) fn run_start_fault(error: EngineError) -> RpcFault {
    #[cfg(debug_assertions)]
    {
        let diagnostic = run_start_debug_code(&error);
        let mut fault = engine_fault(error);
        fault.data.get_or_insert_with(|| json!({}))["debug_code"] = json!(diagnostic);
        fault
    }
    #[cfg(not(debug_assertions))]
    {
        engine_fault(error)
    }
}

#[cfg(debug_assertions)]
fn run_start_debug_code(error: &EngineError) -> &'static str {
    match error {
        EngineError::Session(error) => session_debug_code(error),
        EngineError::Event(error) => event_log_debug_code(error),
        EngineError::Config(error) => config_debug_code(error),
        EngineError::ConfigConflict(_) => "config_conflict",
        EngineError::ConfigChangeRefused(_) => "config_change_refused",
        EngineError::DelegationEvents(_) => "delegation_events",
        EngineError::GrantJournal(_) => "grant_journal",
        EngineError::ToolOutput(_) => "tool_output_io",
        EngineError::AgentMdIo { .. } => "agent_md_io",
        EngineError::Workspace { .. } => "workspace_unavailable",
        EngineError::IneligibleAgent(_) => "ineligible_agent",
        EngineError::UnknownAgentModel { .. } => "unknown_agent_model",
        EngineError::UnavailableAgentModel { .. } => "unavailable_agent_model",
        EngineError::DisabledAgent(_) => "disabled_agent",
        EngineError::MissingRun(_) => "missing_run",
        EngineError::SessionRunning(_) => "session_running",
        EngineError::StaleRevert { .. } => "stale_revert",
        EngineError::ForkIdempotencyConflict => "fork_idempotency_conflict",
        EngineError::RunIdempotencyConflict => "run_idempotency_conflict",
        EngineError::InputHandled(_) => "input_handled",
        EngineError::ModelSelectionBlocked(_) => "model_selection_blocked",
        EngineError::SessionOperationBlocked(_) => "session_operation_blocked",
        EngineError::CompactionCancelled(_) => "compaction_cancelled",
        EngineError::ApprovalNotPending { .. } => "approval_not_pending",
        EngineError::ApprovalConflict => "approval_conflict",
        EngineError::ApprovalResponse(_) => "approval_response",
        EngineError::RenameConflict => "rename_conflict",
        EngineError::Model(_) => "model",
        EngineError::ModelHistory(_) => "model_history",
        EngineError::ToolFailed(_) => "tool_failed",
        EngineError::ToolPrompt(_) => "tool_prompt",
        EngineError::MissingActor(_) => "missing_actor",
        EngineError::SessionOwnedByAnotherProcess(_) => "session_owned_by_another_process",
        EngineError::ActorStopped => "actor_stopped",
        EngineError::NoRunnableModel => "no_runnable_model",
        EngineError::UnknownAgentPreset(_) => "unknown_agent_preset",
        EngineError::ProviderStoreReloadFailed => "provider_store_reload_failed",
        EngineError::RuntimeCompileFailed => "runtime_compile_failed",
        EngineError::InvalidRuntimeAgent(_) => "invalid_runtime_agent",
        EngineError::Mcp(_) => "mcp",
        EngineError::CacheStrategy(_) => "cache_strategy",
        EngineError::Permission(_) => "permission",
        EngineError::Goal(_) => "goal",
        EngineError::Producer(_) => "producer",
        EngineError::Messaging(_) => "messaging",
        EngineError::Runtime(_) => "runtime",
        EngineError::Plugin(_) => "plugin",
        EngineError::ModelManager(_) => "model_manager",
        EngineError::Manifest(_) => "manifest",
    }
}

#[cfg(debug_assertions)]
fn session_debug_code(error: &SessionError) -> &'static str {
    match error {
        SessionError::Event(error) => event_log_debug_code(error),
        SessionError::Io { .. } => "session_io",
        SessionError::Json { .. } => "session_json",
        SessionError::Missing(_) => "session_missing",
        SessionError::SessionLocked(_) => "session_locked",
        SessionError::StoreClosed => "session_store_closed",
        SessionError::InvalidSequence { .. } => "session_invalid_sequence",
        SessionError::InvalidForkTitle(_) => "session_invalid_fork_title",
        SessionError::TreeRejected(_) => "session_tree_rejected",
        SessionError::TreeContended(_) => "session_tree_contended",
    }
}

#[cfg(debug_assertions)]
fn event_log_debug_code(error: &cookie_agent_engine::events::EventLogError) -> &'static str {
    match error {
        cookie_agent_engine::events::EventLogError::Io { .. } => "event_log_io",
        cookie_agent_engine::events::EventLogError::Json { .. } => "event_log_json",
        cookie_agent_engine::events::EventLogError::MissingCreation(_) => {
            "event_log_missing_creation"
        }
        cookie_agent_engine::events::EventLogError::Corrupt { .. } => "event_log_corrupt",
        cookie_agent_engine::events::EventLogError::ReadOnly(_) => "event_log_read_only",
    }
}

#[cfg(debug_assertions)]
fn config_debug_code(error: &cookie_agent_config::ConfigError) -> &'static str {
    use cookie_agent_config::ConfigError;

    match error {
        ConfigError::Io(_) => "config_io",
        ConfigError::UnsafePath => "config_unsafe_path",
        ConfigError::ChangedOnDisk(_) => "config_changed_on_disk",
        ConfigError::NotFound => "config_not_found",
        ConfigError::TooLarge(_) => "config_too_large",
        ConfigError::Toml(_) => "config_toml",
        ConfigError::TomlLimit => "config_toml_limit",
        ConfigError::Provider { .. } => "config_provider",
        ConfigError::HeaderOwnership(_) => "config_header_ownership",
        ConfigError::InvalidRuntime(_) => "config_invalid_runtime",
        ConfigError::FileIo { .. } => "config_io",
        ConfigError::McpServer { .. } => "config_mcp_server",
        ConfigError::RuntimeTemplate { .. } => "config_runtime_template",
        ConfigError::Interpolation(_) => "config_interpolation",
        ConfigError::MissingEnvironment { .. } => "config_missing_environment",
        ConfigError::NonUtf8Environment { .. } => "config_non_utf8_environment",
        ConfigError::AgentFilename(_) => "config_agent_filename",
        ConfigError::DuplicateAgent(_) => "config_duplicate_agent",
        ConfigError::AgentPresetName { .. } => "config_agent_preset_name",
        ConfigError::ReservedAgentId(_) => "config_reserved_agent_id",
        ConfigError::AgentDocument { .. } => "config_agent_document",
        ConfigError::AgentSchemaRemoved { .. } => "config_agent_schema_removed",
        ConfigError::ConfigSchemaRemoved { .. } => "config_schema_removed",
        ConfigError::AgentToolsRemoved(_) => "config_agent_tools_removed",
        ConfigError::AgentModelsRenamed(_) => "config_agent_models_renamed",
        ConfigError::AgentTimeoutInternalOnly(_) => "config_agent_timeout_internal_only",
        ConfigError::AgentPermissionExpression(_) => "config_agent_permission_expression",
        ConfigError::AgentYamlLimit => "config_agent_yaml_limit",
        ConfigError::EmptyPrompt(_) => "config_empty_prompt",
        ConfigError::UnknownPromptPlaceholder { .. } => "config_unknown_prompt_placeholder",
        ConfigError::AgentField { .. } => "config_agent_field",
        ConfigError::AgentLimit(_) => "config_agent_limit",
        ConfigError::PrimaryFallback(_) => "config_primary_fallback",
        ConfigError::Delegation(_) => "config_delegation",
        ConfigError::UnknownDelegationTarget { .. } => "config_unknown_delegation_target",
        ConfigError::IneligibleDelegationTarget { .. } => "config_ineligible_delegation_target",
        ConfigError::UnmatchedDelegationPattern { .. } => "config_unmatched_delegation_pattern",
        ConfigError::DuplicateFallbackModel { .. } => "config_duplicate_fallback_model",
        ConfigError::SkillName { .. } => "config_skill_name",
        ConfigError::SkillNameMismatch { .. } => "config_skill_name_mismatch",
        ConfigError::SkillDocument { .. } => "config_skill_document",
        ConfigError::SkillListingBudget => "config_skill_listing_budget",
    }
}

#[cfg(test)]
mod tests {
    use cookie_agent_protocol::{InvocationId, SessionId};

    use super::*;

    #[test]
    fn a_start_on_a_running_session_is_a_typed_conflict() {
        let session_id = SessionId::new_v7();
        let client_run_id = ClientRunId::new("second").unwrap();
        let fault: cookie_agent_protocol::ServerFault = RpcFault::run_start_conflict(
            session_id,
            &client_run_id,
            RunStartConflictCode::SessionRunning,
        )
        .into();
        assert_eq!(fault.code, -32602);
        assert_eq!(fault.message, "session running");
        let conflict: RunStartConflict =
            serde_json::from_value(fault.data.expect("typed data")).expect("conflict data");
        assert_eq!(conflict.code, RunStartConflictCode::SessionRunning);
        assert_eq!(conflict.session_id, session_id);
        assert_eq!(conflict.client_run_id, client_run_id);
    }

    #[test]
    fn provider_connect_faults_name_the_failure() {
        use cookie_agent_models::ModelManagerError;
        let request = ProviderConnectParams {
            provider_id: cookie_agent_protocol::ProviderId::new("openai").unwrap(),
            expected_catalog_revision: cookie_agent_protocol::CatalogRevision::new(format!(
                "sha256:{}",
                "0".repeat(64)
            ))
            .unwrap(),
            setup_values: std::collections::BTreeMap::new(),
            auth_method: cookie_agent_protocol::AuthMethodId::new("bearer-api-key-v1").unwrap(),
            auth_values: serde_json::from_value(json!({"api_key": "placeholder"})).unwrap(),
            client_connect_id: cookie_agent_protocol::ClientConnectId::new("connect-1").unwrap(),
        };
        for (error, expected) in [
            (
                ModelManagerError::CustomProviderNotStoreBacked,
                ProviderConnectErrorCode::CustomProviderNotStoreBacked,
            ),
            (
                ModelManagerError::MissingSetup,
                ProviderConnectErrorCode::MissingSetupField,
            ),
            (
                ModelManagerError::InvalidSetup,
                ProviderConnectErrorCode::InvalidSetupField,
            ),
            (
                ModelManagerError::MissingCredentials,
                ProviderConnectErrorCode::MissingCredential,
            ),
            (
                ModelManagerError::InvalidCredentials,
                ProviderConnectErrorCode::InvalidCredential,
            ),
        ] {
            let fault: cookie_agent_protocol::ServerFault =
                RpcFault::provider_connect(&request, EngineError::ModelManager(error)).into();
            let data: ProviderConnectError =
                serde_json::from_value(fault.data.expect("typed data")).expect("connect error");
            assert_eq!(data.code, expected);
        }
    }

    #[test]
    fn config_apply_faults_say_why_nothing_changed() {
        use cookie_agent_protocol::{ConfigApplyError, ConfigApplyErrorCode};
        for (error, expected) in [
            (
                EngineError::ConfigConflict("changed".into()),
                ConfigApplyErrorCode::Conflict,
            ),
            (
                EngineError::ConfigChangeRefused("bad path".into()),
                ConfigApplyErrorCode::InvalidChange,
            ),
            (
                EngineError::Config(Box::new(cookie_agent_config::ConfigError::UnsafePath)),
                ConfigApplyErrorCode::InvalidConfiguration,
            ),
        ] {
            let fault: cookie_agent_protocol::ServerFault = RpcFault::config_apply(error).into();
            assert_eq!(fault.code, cookie_agent_protocol::CONFIG_APPLY_ERROR_CODE);
            let data: ConfigApplyError =
                serde_json::from_value(fault.data.expect("typed data")).expect("apply error");
            assert_eq!(data.code, expected);
            assert!(!data.message.is_empty());
        }
        let other: cookie_agent_protocol::ServerFault =
            RpcFault::config_apply(EngineError::NoRunnableModel).into();
        assert_ne!(other.code, cookie_agent_protocol::CONFIG_APPLY_ERROR_CODE);
    }

    #[test]
    fn generic_rpc_fault_preserves_config_path_and_cause_for_client_display() {
        let fault: cookie_agent_protocol::ServerFault = engine_fault(EngineError::Config(
            Box::new(cookie_agent_config::ConfigError::FileIo {
                path: "/workspace/config.toml".into(),
                source: std::io::Error::new(std::io::ErrorKind::PermissionDenied, "access denied"),
            }),
        ))
        .into();
        let client = cookie_agent_protocol::ClientError::Rpc(cookie_agent_protocol::JsonRpcError {
            code: fault.code,
            message: fault.message.into(),
            data: fault.data,
        });
        let text = client.to_string();
        assert!(text.contains("/workspace/config.toml"));
        assert!(text.contains("access denied"));
    }

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

    #[cfg(debug_assertions)]
    #[test]
    fn run_start_debug_codes_are_allowlisted_and_causes_preserve_text() {
        let secret = "plugin-secret-value";
        for (error, expected) in [
            (
                EngineError::InputHandled(format!("Bearer {secret}")),
                "input_handled",
            ),
            (
                EngineError::ModelSelectionBlocked(format!("Bearer {secret}")),
                "model_selection_blocked",
            ),
            (
                EngineError::Config(Box::new(
                    cookie_agent_config::ConfigError::SkillListingBudget,
                )),
                "config_skill_listing_budget",
            ),
        ] {
            let fault: cookie_agent_protocol::ServerFault = run_start_fault(error).into();
            let data = fault.data.expect("debug diagnostic data");
            assert_eq!(data["debug_code"], expected);
            assert_eq!(
                data.to_string().contains(secret),
                expected != "config_skill_listing_budget"
            );
        }
    }
}
