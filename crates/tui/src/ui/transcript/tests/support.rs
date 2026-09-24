//! Shared fixtures and helpers for the transcript rendering tests.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::Duration,
};

#[cfg(unix)]
use std::fs;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt as _;

use crate::ui::transcript::*;

use cookie_agent_config::{
    ApprovalConfig, ContextCompactionConfig, EngineConfig, LoadedConfiguration, ServerConfig,
    SessionTitleConfig, ToolOutputConfig,
};

use cookie_agent_engine::{Engine, EngineOptions};

use cookie_agent_models::{
    ModelManager,
    catalog::{
        CatalogAgeState, CatalogAvailability, CatalogLimits, CatalogModalities, CatalogModelEntry,
        CatalogModelRecord, CatalogModelStatus, CatalogProviderEntry, CatalogProviderRecord,
        CatalogRuntimeState, CatalogSnapshot, CatalogSource,
    },
    provider_store::{
        ClientConnectId as StoreClientConnectId, ConnectMutation, ConnectProposal,
        ProviderAuthValues, ProviderStore, SafePolicyString, StoredProviderPolicyProjection,
    },
};

use cookie_agent_protocol::{
    AgentId, ApprovalBoundary, ApprovalCapability, ApprovalConstraints, ApprovalEvaluation,
    ApprovalId, ApprovalRecord, ApprovalRequest, ApprovalResourceSource, ApprovalStatus,
    ApprovalTrigger, AssistantToolCallRef, AttemptId, BoundedDisplayText, DecisionTrace,
    EventPayload, EventSubscriptionMessage, GoalId, GoalItem, ModelCallId, ModelKey,
    ModelSelection, OperationFingerprint, PermissionAction, PermissionEffect,
    PreparedApprovalResource, PreparedBindingLifetime, PreparedCapabilityOperation,
    PreparedOperationIdentity, PreparedResourceDigest, PreparedResourceIdentity, ProducerMessageId,
    ProviderId, RunId, RunSelection, SafeCode, SafeDisplayText, SafeErrorMessage, SessionId,
    SessionMeta, SessionOrigin, SessionStatus, SessionTitle, Sha256Digest, StoredEvent, ToolCallId,
    ToolCallStart, Usage,
};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

use jiff::Timestamp;

use ratatui::{Terminal, backend::TestBackend, text::Line};

use crate::Client;

use crate::client::ClientDelivery;

use crate::markdown::PlainHighlighter;

use crate::state::{
    ApprovalState, AssistantChild, FrozenAssistantAttribution, SessionState, StateStore,
    ToolCallState,
};

use crate::theme::{ColorLevel, ThemeKind};

use crate::ui::app::*;

use async_trait::async_trait;

use cookie_agent_server::{MessageFrame, MessageStream, Server, TransportError};

use serde_json::Value;

pub(crate) const AGENT: &str = "primary";

pub(crate) const MODEL: &str = "gateway/arbitrary-model";

pub(crate) struct ProductionProviderHarness {
    pub(crate) _directory: tempfile::TempDir,
    pub(crate) engine: Engine,
    pub(crate) server: Arc<Server>,
}

pub(crate) fn production_provider_harness(
    catalog: Arc<CatalogSnapshot>,
    prepare_store: impl FnOnce(&ProviderStore),
) -> ProductionProviderHarness {
    let directory = tempfile::tempdir().expect("production provider test directory");
    #[cfg(unix)]
    fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700))
        .expect("private test directory");
    let provider_store_path = directory.path().join("provider-store");
    #[cfg(unix)]
    {
        fs::create_dir(&provider_store_path).expect("provider store directory");
        fs::set_permissions(&provider_store_path, fs::Permissions::from_mode(0o700))
            .expect("private provider store");
    }
    #[cfg(windows)]
    cookie_agent_models::secure_store::SecureDirectory::open(&provider_store_path)
        .expect("private provider store");
    let store = ProviderStore::open(&provider_store_path).expect("provider store");
    prepare_store(&store);
    let manager = Arc::new(
        ModelManager::new(BTreeMap::new(), catalog, store).expect("production model manager"),
    );
    let config = LoadedConfiguration {
        runtime: EngineConfig {
            server: ServerConfig::default(),
            tool_output: ToolOutputConfig::default(),
            agent_md: cookie_agent_config::AgentMdConfig::default(),
            approval: ApprovalConfig::default(),
            model_retry: cookie_agent_config::ModelRetryConfig::default(),
            context_compaction: ContextCompactionConfig::default(),
            session_title: SessionTitleConfig::default(),
            delegation: cookie_agent_config::DelegationConfig::default(),
            messaging: cookie_agent_config::MessagingConfig::default(),
            pricing: cookie_agent_config::PricingConfig::default(),
            headers: BTreeMap::new(),
            providers: BTreeMap::new(),
        },
        agents: BTreeMap::new(),
        agent_presets: BTreeMap::new(),
        mcp_servers: BTreeMap::new(),
        user_mcp_servers: BTreeMap::new(),
        workspace_mcp_servers: BTreeMap::new(),
        plugins: Default::default(),
        config_paths: cookie_agent_config::ConfigLayerPaths::default(),
        skills: cookie_agent_config::SkillRegistry::default(),
    };
    let engine = Engine::open(EngineOptions {
        data_dir: directory.path().join("data"),
        cwd: directory.path().to_owned(),
        config,
        model_manager: manager,
        tools: Vec::new(),
    })
    .expect("production engine");
    let server = Arc::new(Server::new(engine.clone()));
    ProductionProviderHarness {
        _directory: directory,
        engine,
        server,
    }
}

pub(crate) fn production_openai_catalog(label: char, quarantined: bool) -> Arc<CatalogSnapshot> {
    let provider_id = ProviderId::new("openai").expect("provider ID");
    let model_id = cookie_agent_protocol::ProviderModelId::new("gpt-5-mini").expect("model ID");
    let environment = vec!["OPENAI_API_KEY".to_owned()];
    let model = CatalogModelRecord {
        id: model_id.clone(),
        name: "GPT-5 mini".to_owned(),
        description: "TUI production projection test".to_owned(),
        family: None,
        attachment: false,
        reasoning: false,
        tool_call: true,
        structured_output: Some(true),
        temperature: Some(true),
        open_weights: false,
        status: CatalogModelStatus::Stable,
        release_date: "2026-01-01".to_owned(),
        last_updated: "2026-01-01".to_owned(),
        modalities: CatalogModalities {
            input: vec!["text".to_owned()],
            output: vec!["text".to_owned()],
        },
        limits: CatalogLimits {
            context: 128_000,
            input: None,
            output: 16_384,
        },
        shape: None,
        provider: None,
        reasoning_options: Vec::new(),
        cost: None,
        interleaved: None,
        canonical_provenance: None,
    };
    let shape = quarantined.then(|| "unexpected".to_owned());
    let record = CatalogProviderRecord {
        id: provider_id.clone(),
        name: "OpenAI".to_owned(),
        environment: environment.clone(),
        npm: "@ai-sdk/openai".to_owned(),
        api: None,
        shape: shape.clone(),
        documentation_url: "https://example.test/openai".to_owned(),
        models: BTreeMap::from([(
            model_id.clone(),
            CatalogModelEntry {
                id: model_id,
                record: Some(model),
                quarantine: None,
            },
        )]),
    };
    let now = Timestamp::now();
    Arc::new(CatalogSnapshot {
        revision: cookie_agent_protocol::CatalogRevision::new(format!(
            "sha256:{}",
            label.to_string().repeat(64)
        ))
        .expect("catalog revision"),
        source: CatalogSource::Network,
        state: CatalogRuntimeState {
            availability: CatalogAvailability::Ready,
            age: CatalogAgeState::Current,
            last_error: None,
        },
        validated_at: now,
        last_checked_at: now,
        etag: None,
        providers: BTreeMap::from([(
            provider_id.clone(),
            CatalogProviderEntry {
                id: provider_id,
                record: Some(record),
                quarantine: None,
            },
        )]),
        canonical_models: BTreeMap::new(),
        quarantine: Vec::new(),
    })
}

pub(crate) fn production_empty_catalog(label: char) -> Arc<CatalogSnapshot> {
    let now = Timestamp::now();
    Arc::new(CatalogSnapshot {
        revision: cookie_agent_protocol::CatalogRevision::new(format!(
            "sha256:{}",
            label.to_string().repeat(64)
        ))
        .expect("catalog revision"),
        source: CatalogSource::Network,
        state: CatalogRuntimeState {
            availability: CatalogAvailability::Ready,
            age: CatalogAgeState::Current,
            last_error: None,
        },
        validated_at: now,
        last_checked_at: now,
        etag: None,
        providers: BTreeMap::new(),
        canonical_models: BTreeMap::new(),
        quarantine: Vec::new(),
    })
}

pub(crate) fn install_unmatched_openai_connection(
    store: &ProviderStore,
    catalog: &CatalogSnapshot,
) {
    let transaction = store.begin_transaction().expect("provider transaction");
    let snapshot = transaction.snapshot();
    let mutation = ConnectMutation {
        client_connect_id: StoreClientConnectId::new("tui-unmatched-retained").expect("connect ID"),
        provider_id: ProviderId::new("openai").expect("provider ID"),
        expected_catalog_revision: catalog.revision.clone(),
        expectation: snapshot.expectation(),
        setup_values: BTreeMap::new(),
        auth_method: cookie_agent_protocol::AuthMethodId::new("bearer-api-key-v1")
            .expect("auth method"),
        auth_values: ProviderAuthValues::new(BTreeMap::from([(
            cookie_agent_protocol::AuthFieldName::new("api_key").expect("auth field"),
            "stored-secret".to_owned(),
        )]))
        .expect("auth values"),
        policy: StoredProviderPolicyProjection {
            catalog_revision: catalog.revision.clone(),
            family_id: SafePolicyString::new("openai").expect("family ID"),
            setup_recipe: cookie_agent_protocol::ProviderSetupRecipeId::new("no-setup-v1")
                .expect("setup recipe"),
            adapter_id: SafePolicyString::new("openai").expect("adapter ID"),
            compiler_version: cookie_agent_protocol::RecipeCompilerVersion::new(
                "family-registry-compiler-v1",
            )
            .expect("compiler version"),
            default_endpoint_identity: SafePolicyString::new("https://api.openai.com/v1")
                .expect("endpoint"),
            package_claim: SafePolicyString::new("@ai-sdk/openai-forged")
                .expect("mismatched package"),
            source_record_digest: cookie_agent_models::Sha256Digest::new("d".repeat(64))
                .expect("source digest"),
            recipe_fingerprint: cookie_agent_models::Sha256Digest::new("e".repeat(64))
                .expect("recipe fingerprint"),
            model_overrides: BTreeMap::new(),
        },
    };
    let ConnectProposal::Proposed(proposal) = transaction
        .propose_connect(&mutation, &catalog.revision)
        .expect("connect proposal")
    else {
        panic!("unmatched retained policy unexpectedly replayed")
    };
    transaction.commit(*proposal).expect("connect commit");
}

pub(crate) fn agent_id() -> AgentId {
    AgentId::new(AGENT).expect("agent id")
}

pub(crate) fn model_key() -> ModelKey {
    MODEL.parse::<ModelKey>().expect("model key")
}

pub(crate) fn resolved_model(variant: Option<&str>) -> cookie_agent_protocol::ResolvedModelRef {
    let selection = ModelSelection {
        model: model_key(),
        variant: variant.map(|id| cookie_agent_protocol::VariantId::new(id).expect("variant id")),
    };
    cookie_agent_protocol::ResolvedModelRef {
        provider_id: ProviderId::new("gateway").expect("provider id"),
        model_id: cookie_agent_protocol::ProviderModelId::new("arbitrary-model").expect("model id"),
        adapter_id: cookie_agent_protocol::AdaptorId::OpenaiCompatible,
        selection_fingerprint: Sha256Digest::of_bytes(
            format!("selection:{selection:?}").as_bytes(),
        ),
        selection,
    }
}

pub(crate) fn protocol_revision<T>(digit: &str) -> T
where
    T: serde::de::DeserializeOwned,
{
    serde_json::from_value(serde_json::json!(format!(
        "sha256:{}",
        digit.repeat(64 / digit.len())
    )))
    .expect("protocol revision")
}

pub(crate) fn frozen_binding(
    resolved: cookie_agent_protocol::ResolvedModelRef,
) -> cookie_agent_protocol::FrozenModelBinding {
    cookie_agent_protocol::FrozenModelBinding {
        manifest_revision: protocol_revision("a"),
        blueprint_fingerprint: Sha256Digest::of_bytes(b"blueprint"),
        selection: resolved.selection.clone(),
        source: cookie_agent_protocol::FrozenProviderSource::Custom {
            safe_definition_fingerprint: Sha256Digest::of_bytes(b"definition"),
        },
        config_override_fingerprint: Sha256Digest::of_bytes(b"override"),
        credential_binding: cookie_agent_protocol::FrozenCredentialBinding {
            source: cookie_agent_protocol::FrozenCredentialSource::NoAuth,
            auth_method: cookie_agent_protocol::AuthMethodId::new("no-auth").expect("auth method"),
            fields: Vec::new(),
            parameters: BTreeMap::new(),
            owned_headers: Vec::new(),
        },
        setup_binding: cookie_agent_protocol::FrozenSetupBinding {
            setup_recipe: cookie_agent_protocol::ProviderSetupRecipeId::new("custom-setup")
                .expect("setup recipe"),
            values: BTreeMap::new(),
            setup_fingerprint: Sha256Digest::of_bytes(b"setup"),
        },
        endpoint_identity: cookie_agent_protocol::SafeEndpointIdentity::new(
            "https://example.test/v1",
        )
        .expect("endpoint"),
        provider_recipe: cookie_agent_protocol::ProviderRecipeId::new("custom-provider")
            .expect("provider recipe"),
        protocol_recipe: cookie_agent_protocol::ProtocolRecipeId::new("custom-protocol")
            .expect("protocol recipe"),
        setup_recipe: cookie_agent_protocol::ProviderSetupRecipeId::new("custom-setup")
            .expect("setup recipe"),
        compiler_version: cookie_agent_protocol::RecipeCompilerVersion::new("compiler-v1")
            .expect("compiler version"),
        descriptor: serde_json::from_value(serde_json::json!({
            "identity": {"provider_id": "gateway", "model_id": "arbitrary-model"},
            "adapter_id": "openai-compatible",
            "capabilities": {
                "features": [],
                "limits": {"context": 8192, "input": null, "output": 2048},
                "modalities": {"input": ["text"], "output": ["text"]},
                "media": {"input": {}},
                "cancellation": "local_only",
                "compaction": "unsupported",
                "replay": {"policy": "never", "capability": "unsupported", "reasoning": false}
            },
            "provider_metadata": {}
        }))
        .expect("test descriptor"),
        defaults: cookie_agent_protocol::FrozenResolvedRequestDefaults {
            request: cookie_agent_protocol::FrozenRequestDefaults::default(),
            reasoning: None,
        },
        options: cookie_agent_protocol::ProviderOptions::OpenAiCompatible { api_path: None },
        static_headers: BTreeMap::new(),
        behavior_fingerprint: resolved.selection_fingerprint.clone(),
        selection_fingerprint: resolved.selection_fingerprint,
    }
}

pub(crate) fn attribution(variant: Option<&str>) -> FrozenAssistantAttribution {
    FrozenAssistantAttribution {
        agent: agent_id(),
        resolved_model: resolved_model(variant),
    }
}

pub(crate) fn run_id() -> RunId {
    RunId::new_v7()
}

pub(crate) fn event(
    session_id: SessionId,
    seq: u64,
    run: RunId,
    payload: EventPayload,
) -> StoredEvent {
    StoredEvent {
        engine_version: None,
        origin: None,
        session_id,
        run_id: Some(run),
        seq,
        timestamp: Timestamp::now(),
        payload,
    }
}

pub(crate) fn runless_event(session_id: SessionId, seq: u64, payload: EventPayload) -> StoredEvent {
    StoredEvent {
        engine_version: None,
        origin: None,
        session_id,
        run_id: None,
        seq,
        timestamp: Timestamp::now(),
        payload,
    }
}

pub(crate) fn checkpoint_commit(summary: &str) -> cookie_agent_protocol::ContextCheckpointCommit {
    let max = cookie_agent_protocol::SummaryByteLimit::new(1024).expect("summary limit");
    cookie_agent_protocol::ContextCheckpointCommit {
        checkpoint: cookie_agent_protocol::ContextCheckpoint::InternalSummary {
            checkpoint: cookie_agent_protocol::InternalSummaryCheckpoint::new(
                summary.into(),
                cookie_agent_protocol::InternalAgentInvocationId::new_v7(),
                cookie_agent_protocol::InternalAgentRunId::new_v7(),
                max,
            )
            .expect("summary checkpoint"),
        },
        boundaries: cookie_agent_protocol::ContextCheckpointBoundaries {
            source_from_seq: 2,
            source_through_seq: 3,
            recent_from_seq: None,
            input_through_seq: 4,
            prior_checkpoint_seq: None,
        },
        budgets: cookie_agent_protocol::ContextCheckpointBudgets {
            context_limit_tokens: 10_000,
            trigger_tokens: 8_000,
            input_tokens_before: 9_000,
            input_tokens_after: 1_200,
            keep_recent_tokens: 0,
            max_summary_bytes: max,
        },
    }
}

pub(crate) fn attempt_started(
    session_id: SessionId,
    seq: u64,
    run: RunId,
    attempt: AttemptId,
    variant: Option<&str>,
) -> StoredEvent {
    event(
        session_id,
        seq,
        run,
        EventPayload::ModelAttemptStarted {
            attempt_id: attempt,
            attempt_ordinal: 1,
            fallback_index: 0,
            retry_ordinal: 0,
            resolved_model: resolved_model(variant),
            prompt_fingerprint: Sha256Digest::of_bytes(b"prompt"),
        },
    )
}

pub(crate) fn session_created(session_id: SessionId, seq: u64) -> StoredEvent {
    session_created_with(session_id, seq, AGENT, vec![resolved_model(None)], 0)
}

pub(crate) fn session_created_with(
    session_id: SessionId,
    seq: u64,
    agent: &str,
    chain: Vec<cookie_agent_protocol::ResolvedModelRef>,
    suffix_start: u32,
) -> StoredEvent {
    let selection = RunSelection {
        agent: AgentId::new(agent).expect("agent id"),
        model: chain[suffix_start as usize].selection.clone(),
        preset: None,
    };
    let chain = chain.into_iter().map(frozen_binding).collect::<Vec<_>>();
    session_created_from_bindings(session_id, seq, selection, chain, suffix_start)
}

pub(crate) fn session_created_from_bindings(
    session_id: SessionId,
    seq: u64,
    selection: RunSelection,
    chain: Vec<cookie_agent_protocol::FrozenModelBinding>,
    suffix_start: u32,
) -> StoredEvent {
    StoredEvent {
        engine_version: None,
        origin: None,
        session_id,
        run_id: None,
        seq,
        timestamp: Timestamp::now(),
        payload: EventPayload::SessionCreated {
            short_id: None,
            origin: SessionOrigin::Root,
            cwd_identity: cookie_agent_protocol::CwdIdentity::new("/workspace").expect("cwd"),
            creation_selection: selection.clone(),
            creation_agent: Box::new(cookie_agent_protocol::AgentSnapshot {
                agent: selection.agent.clone(),
                schema: cookie_agent_protocol::AgentSchemaVersion::current(),
                mode: cookie_agent_protocol::AgentMode::Primary,
                description: "Test primary agent".into(),
                document_source: cookie_agent_protocol::AgentDocumentSource::Workspace,
                document_fingerprint: Sha256Digest::of_bytes(b"document"),
                composed_prompt: "You are the primary test agent.\n".into(),
                prompt_fingerprint: Sha256Digest::of_bytes(b"prompt"),
                max_output_tokens: 0,
                permissions: Vec::new(),
                delegation: None,
                fallback_chain: chain,
                selected_suffix_start: suffix_start,
            }),
            runtime_revision: protocol_revision("1"),
            catalog_revision: protocol_revision("2"),
            provider_state_revision: protocol_revision("3"),
            model_revision: protocol_revision("4"),
            agent_revision: protocol_revision("5"),
            recipe_registry_revision: protocol_revision("6"),
            manifest_revision: protocol_revision("7"),
        },
    }
}

pub(crate) fn text_delta(
    session_id: SessionId,
    seq: u64,
    run: RunId,
    attempt: AttemptId,
    text: &str,
) -> StoredEvent {
    event(
        session_id,
        seq,
        run,
        EventPayload::TextDelta {
            attempt_id: attempt,
            text: text.into(),
        },
    )
}

pub(crate) fn reasoning_delta(
    session_id: SessionId,
    seq: u64,
    run: RunId,
    attempt: AttemptId,
    text: &str,
) -> StoredEvent {
    event(
        session_id,
        seq,
        run,
        EventPayload::ReasoningDelta {
            attempt_id: attempt,
            text: text.into(),
        },
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn turn_committed(
    session_id: SessionId,
    seq: u64,
    run: RunId,
    attempt: AttemptId,
    turn_seq: u64,
    content: Vec<cookie_agent_protocol::PersistedAssistantPart>,
    warnings: Vec<&str>,
    variant: Option<&str>,
) -> StoredEvent {
    event(
        session_id,
        seq,
        run,
        EventPayload::ModelTurnCommitted {
            attempt_id: attempt,
            model_turn_seq: turn_seq,
            resolved_model: resolved_model(variant),
            input_through_seq: seq,
            turn: cookie_agent_protocol::PersistedModelTurn {
                content,
                provider_options: BTreeMap::new(),
                finish_reason: cookie_agent_protocol::ModelFinishReason::Stop,
                usage: Usage {
                    input_tokens: Some(10),
                    input_tokens_no_cache: Some(8),
                    input_tokens_cache_read: Some(2),
                    input_tokens_cache_write: Some(0),
                    output_tokens: Some(4),
                    output_tokens_text: Some(3),
                    output_tokens_reasoning: Some(1),
                },
                response_metadata: BTreeMap::new(),
                provider_metadata: BTreeMap::new(),
                native_replay: None,
            },
            warnings: warnings
                .into_iter()
                .map(|warning| SafeErrorMessage::new(warning).expect("warning"))
                .collect(),
        },
    )
}

pub(crate) fn usage_recorded(
    session_id: SessionId,
    seq: u64,
    run: RunId,
    model_turn_seq: u64,
    estimated_cost_pico_usd: Option<u64>,
) -> StoredEvent {
    event(
        session_id,
        seq,
        run,
        EventPayload::ModelUsageRecorded {
            model_turn_seq,
            agent_id: agent_id(),
            resolved_model: resolved_model(None),
            usage: Usage::default(),
            estimated_cost_pico_usd,
        },
    )
}

pub(crate) fn test_goal(status: GoalStatus, items: Vec<GoalItem>) -> GoalState {
    GoalState {
        goal_id: GoalId::new_v7(),
        objective: "Ship transcript rendering without regressions".to_owned(),
        status,
        items,
        revision: 3,
    }
}

pub(crate) fn goal_item(description: &str, finished: bool) -> GoalItem {
    GoalItem {
        description: description.to_owned(),
        finished,
    }
}

pub(crate) fn text_part(text: &str) -> cookie_agent_protocol::PersistedAssistantPart {
    cookie_agent_protocol::PersistedAssistantPart::Text {
        text: text.into(),
        metadata: None,
    }
}

pub(crate) fn reasoning_part(text: &str) -> cookie_agent_protocol::PersistedAssistantPart {
    cookie_agent_protocol::PersistedAssistantPart::Reasoning {
        text: text.into(),
        metadata: None,
    }
}

pub(crate) fn tool_part(call: &str) -> cookie_agent_protocol::PersistedAssistantPart {
    cookie_agent_protocol::PersistedAssistantPart::ToolCall {
        id: ModelCallId::new(call).expect("call"),
        provider_item_id: None,
        name: SafeCode::new("bash").expect("tool"),
        input: serde_json::json!({"command": call}),
        raw_input: None,
        metadata: None,
    }
}

pub(crate) fn presentation(
    title: &str,
    primary: Option<&str>,
) -> cookie_agent_protocol::ToolCallPresentation {
    cookie_agent_protocol::ToolCallPresentation {
        title: SafeDisplayText::new(title).expect("presentation title"),
        primary_argument: primary
            .map(|argument| BoundedDisplayText::new(argument).expect("primary argument")),
    }
}

pub(crate) fn owner(turn_seq: u64, call: &str) -> AssistantToolCallRef {
    AssistantToolCallRef {
        model_turn_seq: turn_seq,
        content_index: 0,
        model_call_id: ModelCallId::new(call).expect("model call id"),
        provider_item_id: None,
    }
}

pub(crate) fn operation_fingerprint() -> OperationFingerprint {
    OperationFingerprint::from_prepared_operation(
        &PreparedOperationIdentity::new(
            Sha256Digest::of_bytes(b"arguments"),
            vec![ApprovalCapability {
                action: PermissionAction::Bash,
                operation: PreparedCapabilityOperation::new("execute")
                    .expect("capability operation"),
            }],
            vec![PreparedApprovalResource {
                capability: PermissionAction::Bash,
                canonical: PreparedResourceIdentity::new("command:test")
                    .expect("resource identity"),
                binding_digest: PreparedResourceDigest::from_canonical_binding_bytes(b"test"),
                binding_lifetime: PreparedBindingLifetime::ProcessLocal,
                boundary: ApprovalBoundary::Exact,
                source: ApprovalResourceSource::PrimaryOperation,
            }],
            Sha256Digest::of_bytes(b"context"),
        )
        .expect("prepared operation"),
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn tool_started_at(
    session_id: SessionId,
    seq: u64,
    run: RunId,
    call_id: ToolCallId,
    turn_seq: u64,
    call: &str,
    content_index: u32,
    title: &str,
    primary: Option<&str>,
) -> StoredEvent {
    let mut owner = owner(turn_seq, call);
    owner.content_index = content_index;
    event(
        session_id,
        seq,
        run,
        EventPayload::ToolCallStarted {
            start: ToolCallStart {
                output: Default::default(),
                tool_call_id: call_id,
                owner,
                presentation: presentation(title, primary),
                operation_fingerprint: operation_fingerprint(),
            },
        },
    )
}

pub(crate) fn tool_started(
    session_id: SessionId,
    seq: u64,
    run: RunId,
    call_id: ToolCallId,
    turn_seq: u64,
    call: &str,
) -> StoredEvent {
    tool_started_at(session_id, seq, run, call_id, turn_seq, call, 0, call, None)
}

pub(crate) fn tool_terminated(
    session_id: SessionId,
    seq: u64,
    run: RunId,
    call_id: ToolCallId,
    turn_seq: u64,
    call: &str,
    outcome: cookie_agent_protocol::ToolTerminationOutcome,
) -> StoredEvent {
    let completed = matches!(
        outcome,
        cookie_agent_protocol::ToolTerminationOutcome::Completed
    );
    event(
        session_id,
        seq,
        run,
        EventPayload::ToolCallTerminated {
            termination: cookie_agent_protocol::ToolCallTermination {
                tool_call_id: call_id,
                owner: owner(turn_seq, call),
                outcome,
                result: completed.then(|| cookie_agent_protocol::PersistedToolResult {
                    display: None,
                    retained_output: None,
                    title: SafeDisplayText::new("ran true").expect("result title"),
                    output: "done".into(),
                    metadata: serde_json::Value::Null,
                    truncation: None,
                    attachments: Vec::new(),
                    additional_messages: Vec::new(),
                }),
                error: (!completed).then(|| cookie_agent_protocol::SafeToolError {
                    code: SafeCode::new("exit_failure").expect("error code"),
                    message: SafeErrorMessage::new("command failed").expect("error message"),
                }),
            },
        },
    )
}

pub(crate) fn session_meta(id: SessionId) -> SessionMeta {
    SessionMeta {
        short_id: None,
        session_id: id,
        origin: SessionOrigin::Root,
        cwd_identity: cookie_agent_protocol::CwdIdentity::new("/workspace").expect("cwd"),
        creation_selection: RunSelection {
            agent: agent_id(),
            model: ModelSelection {
                model: model_key(),
                variant: None,
            },
            preset: None,
        },
        runtime_revision: protocol_revision("1"),
        catalog_revision: protocol_revision("2"),
        provider_state_revision: protocol_revision("3"),
        model_revision: protocol_revision("4"),
        agent_revision: protocol_revision("5"),
        recipe_registry_revision: protocol_revision("6"),
        manifest_revision: protocol_revision("7"),
        title: None,
        title_updated_seq: 0,
        // Creation plus one user message: a session with content that
        // renders in the Agents panel. Tests for the empty-session ghost
        // filter set this back to 1 explicitly.
        last_event_seq: 2,
        last_activity: "2026-08-06T12:00:00Z".parse().expect("timestamp"),
        status: SessionStatus::Idle,
        skipped_events: Vec::new(),
    }
}

pub(crate) fn titled_meta(session_id: SessionId, title: &str, title_seq: u64) -> SessionMeta {
    SessionMeta {
        title: Some(SessionTitle::new(title).expect("title")),
        title_updated_seq: title_seq,
        ..session_meta(session_id)
    }
}

pub(crate) fn approval_request(trigger: ApprovalTrigger) -> ApprovalRequest {
    let resource = PreparedApprovalResource {
        capability: PermissionAction::Bash,
        canonical: PreparedResourceIdentity::new("command:git-status")
            .expect("prepared resource identity"),
        binding_digest: PreparedResourceDigest::from_canonical_binding_bytes(b"git status"),
        binding_lifetime: PreparedBindingLifetime::ProcessLocal,
        boundary: ApprovalBoundary::CommandPrefix {
            prefix: "git status".into(),
        },
        source: ApprovalResourceSource::ModelRequest,
    };
    let resource_digest = resource.binding_digest.clone();
    let operation = PreparedOperationIdentity::new(
        Sha256Digest::of_bytes(b"normalized arguments"),
        vec![ApprovalCapability {
            action: PermissionAction::Bash,
            operation: PreparedCapabilityOperation::new("execute")
                .expect("prepared capability operation"),
        }],
        vec![resource],
        Sha256Digest::of_bytes(b"execution context"),
    )
    .expect("prepared operation");
    ApprovalRequest::new(
        ApprovalId::new_v7(),
        3,
        trigger,
        operation,
        vec![ApprovalEvaluation {
            resource_digest,
            effect: PermissionEffect::Ask,
            trace: DecisionTrace {
                action: PermissionAction::Bash,
                normalized_resource: "git status".into(),
                candidates: Vec::new(),
                effect: PermissionEffect::Ask,
                precedence_reason: "model requested approval".into(),
            },
        }],
        ApprovalConstraints {
            allow_once: true,
            allow_tree_grant: false,
            cancellable: true,
            expires_at: None,
        },
    )
    .expect("approval request")
}

pub(crate) fn approval(session_id: SessionId) -> ApprovalState {
    crate::state::approval_state_from_record(ApprovalRecord {
        session_id,
        request: approval_request(ApprovalTrigger::ModelToolApproval),
        status: ApprovalStatus::Escalated,
        internal_decision: None,
        user_decision: None,
        final_decision: None,
    })
    .expect("escalated approval state")
}

pub(crate) fn descriptor(agent: &str, runnable: bool) -> cookie_agent_protocol::AgentDescriptor {
    cookie_agent_protocol::AgentDescriptor {
        id: AgentId::new(agent).expect("agent id"),
        preset: None,
        description: format!("Test {agent} agent"),
        mode: cookie_agent_protocol::AgentMode::Primary,
        enabled: runnable,
        runnable_as_root: runnable,
        resolved_fallback: vec![ModelSelection {
            model: model_key(),
            variant: None,
        }],
        delegation_targets: Vec::new(),
    }
}

pub(crate) fn preset_descriptor(
    preset: &str,
    agent: &str,
) -> cookie_agent_protocol::AgentDescriptor {
    let mut descriptor = descriptor(agent, true);
    descriptor.preset = Some(preset.into());
    descriptor.description = format!("{preset} {agent} agent");
    descriptor
}

pub(crate) fn model_descriptor() -> cookie_agent_protocol::AvailableModelDescriptor {
    cookie_agent_protocol::AvailableModelDescriptor {
        key: model_key(),
        display_name: "Arbitrary Model".into(),
        capabilities: cookie_agent_protocol::ModelCapabilities {
            input: [cookie_agent_protocol::Modality::Text]
                .into_iter()
                .collect(),
            output: [cookie_agent_protocol::Modality::Text]
                .into_iter()
                .collect(),
            context_tokens: 8192,
            output_tokens: 2048,
            tool_calling: true,
            parallel_tool_calls: true,
            structured_output: false,
            reasoning: true,
            temperature: true,
            top_p: true,
            seed: true,
            native_replay: cookie_agent_protocol::ReplayCapability::Optional,
            cancellation: cookie_agent_protocol::CancellationCapability::LocalOnly,
            media: BTreeMap::new(),
        },
        variants: vec![
            cookie_agent_protocol::AvailableVariantDescriptor {
                id: cookie_agent_protocol::VariantId::new("fast").expect("variant"),
                display_name: "Fast".into(),
                origin: cookie_agent_protocol::VariantOrigin::Explicit,
                behavior_fingerprint: Sha256Digest::of_bytes(b"fast"),
            },
            cookie_agent_protocol::AvailableVariantDescriptor {
                id: cookie_agent_protocol::VariantId::new("high").expect("variant"),
                display_name: "High".into(),
                origin: cookie_agent_protocol::VariantOrigin::ModelsDevEffort,
                behavior_fingerprint: Sha256Digest::of_bytes(b"high"),
            },
        ],
        variant_order: vec![
            cookie_agent_protocol::VariantId::new("fast").expect("variant"),
            cookie_agent_protocol::VariantId::new("high").expect("variant"),
        ],
        default_variant: None,
        behavior_fingerprint: Sha256Digest::of_bytes(b"model"),
    }
}

pub(crate) fn provider_descriptor(
    id: &str,
    support: &str,
    presence: &str,
    connected: bool,
) -> cookie_agent_protocol::ProviderDescriptor {
    let reason = (support != "supported").then_some("unsupported_environment");
    let durable = connected.then(|| {
        serde_json::json!({
            "provider_id": id,
            "setup_values": {"region": "us-east-1"},
            "setup_fingerprint": Sha256Digest::of_bytes(b"setup"),
            "recipe_fingerprint": Sha256Digest::of_bytes(b"recipe"),
            "auth_method": "api-key",
            "credential_fields": ["api_key"],
            "connection_generation": 1,
            "connected_at": Timestamp::now()
        })
    });
    serde_json::from_value(serde_json::json!({
            "id": id,
            "display_name": format!("{id} provider"),
            "presence": presence,
            "support": {"state": support, "reason": reason},
            "setup_fields": [{
                "id": "region",
                "display_name": "Region",
                "help": "Public service region",
                "required": true,
                "default": "us-west-2",
                "validation": {"value_type": "string", "min_length": 2, "max_length": 32, "minimum": null, "maximum": null},
                "safe_to_project": true
            }],
            "auth_methods": [{
                "id": "api-key",
                "display_name": "API key",
                "credentials": [{
                    "id": "api_key",
                    "display_name": "API key",
                    "help": "Secret API credential",
                    "required": true,
                    "credential_type": "api_key"
                }]
            }],
            "configuration": if connected {"stored"} else {"unconfigured"},
            "effective_auth_state": if connected {"provider_store"} else {"unavailable"},
            "durable_connection": durable,
            "quarantine": null
        }))
        .expect("provider descriptor")
}

pub(crate) fn multi_auth_provider() -> cookie_agent_protocol::ProviderDescriptor {
    let mut value = serde_json::to_value(provider_descriptor(
        "multi-auth",
        "supported",
        "current",
        false,
    ))
    .expect("serialize provider");
    value["auth_methods"] = serde_json::json!([
        {
            "id": "api-key",
            "display_name": "API key",
            "credentials": [{
                "id": "api_key",
                "display_name": "API key",
                "help": "Secret API credential",
                "required": true,
                "credential_type": "api_key"
            }]
        },
        {
            "id": "bearer",
            "display_name": "Bearer token",
            "credentials": [{
                "id": "access_token",
                "display_name": "Access token",
                "help": "Secret bearer credential",
                "required": true,
                "credential_type": "access_token"
            }]
        }
    ]);
    value["setup_fields"] = serde_json::json!([
        {
            "id": "region",
            "display_name": "Region",
            "help": "Public service region",
            "required": true,
            "default": null,
            "validation": {"value_type": "string", "min_length": 1, "max_length": 32, "minimum": null, "maximum": null},
            "safe_to_project": true
        },
        {
            "id": "service_token",
            "display_name": "Service token",
            "help": "Derived secret setup placeholder",
            "required": true,
            "default": null,
            "validation": {"value_type": "string", "min_length": 1, "max_length": 64, "minimum": null, "maximum": null},
            "safe_to_project": false
        }
    ]);
    serde_json::from_value(value).expect("multi-auth provider")
}

pub(crate) fn runtime_snapshot(
    digit: &str,
    providers: Vec<cookie_agent_protocol::ProviderDescriptor>,
    models: Vec<cookie_agent_protocol::AvailableModelDescriptor>,
    agents: Vec<cookie_agent_protocol::AgentDescriptor>,
) -> cookie_agent_protocol::RuntimeSnapshotV1 {
    cookie_agent_protocol::RuntimeSnapshotV1 {
        snapshot_schema_version: cookie_agent_protocol::RuntimeSnapshotSchemaVersion::current(),
        recipe_registry_revision: protocol_revision(digit),
        catalog_revision: protocol_revision(digit),
        catalog_source: cookie_agent_protocol::CatalogSource::Network,
        catalog_state: cookie_agent_protocol::CatalogRuntimeState {
            stale: false,
            provider_quarantine_count: 0,
            model_quarantine_count: 0,
            quarantine_digest: Sha256Digest::of_bytes(b"quarantine"),
            last_error: None,
        },
        provider_state_revision: protocol_revision(digit),
        provider_store_generation: cookie_agent_protocol::ProviderStoreGeneration::new(1)
            .expect("store generation"),
        model_revision: protocol_revision(digit),
        agent_revision: protocol_revision(digit),
        runtime_revision: protocol_revision(digit),
        providers,
        models,
        agents,
    }
}

pub(crate) fn catalog_model(
    key: &str,
    variants: &[&str],
    default_variant: Option<&str>,
) -> cookie_agent_protocol::AvailableModelDescriptor {
    let mut descriptor = model_descriptor();
    descriptor.key = key.parse().expect("model key");
    descriptor.display_name = format!("Catalog {key}");
    descriptor.variants = variants
        .iter()
        .map(|id| cookie_agent_protocol::AvailableVariantDescriptor {
            id: cookie_agent_protocol::VariantId::new(*id).expect("variant"),
            display_name: format!("Variant {id}"),
            origin: cookie_agent_protocol::VariantOrigin::Explicit,
            behavior_fingerprint: Sha256Digest::of_bytes(id.as_bytes()),
        })
        .collect();
    descriptor
        .variants
        .sort_by(|left, right| left.id.cmp(&right.id));
    descriptor.variant_order = variants
        .iter()
        .map(|id| cookie_agent_protocol::VariantId::new(*id).expect("variant"))
        .collect();
    descriptor.default_variant = default_variant
        .map(|id| cookie_agent_protocol::VariantId::new(id).expect("default variant"));
    descriptor
}

pub(crate) fn frame_rows(app: &mut App, width: u16, height: u16) -> Vec<String> {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    terminal
        .draw(|frame| app.draw_for_test(frame))
        .expect("app render");
    let buffer = terminal.backend().buffer();
    (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect()
}

pub(crate) fn rect_text(rows: &[String], rect: Rect) -> String {
    rows[usize::from(rect.y)]
        .chars()
        .skip(usize::from(rect.x))
        .take(usize::from(rect.width))
        .collect()
}

pub(crate) async fn test_app() -> App {
    let (client, _requests) = recording_client();
    let mut app = App::new(client).await.expect("test app");
    app.install_initial_runtime(runtime_snapshot(
        "1",
        Vec::new(),
        vec![model_descriptor()],
        vec![descriptor("primary", true)],
    ));
    app
}

pub(crate) struct ScriptedStream {
    pub(crate) incoming: tokio::sync::mpsc::UnboundedReceiver<MessageFrame>,
    pub(crate) sent: tokio::sync::mpsc::UnboundedSender<MessageFrame>,
}

#[async_trait]
impl MessageStream for ScriptedStream {
    async fn send(&mut self, frame: MessageFrame) -> Result<(), TransportError> {
        self.sent.send(frame).map_err(|_| TransportError::Closed)
    }

    async fn recv(&mut self) -> Result<Option<MessageFrame>, TransportError> {
        Ok(self.incoming.recv().await)
    }
}

pub(crate) fn recording_client() -> (Client, Arc<Mutex<Vec<Value>>>) {
    let (_incoming, incoming_rx) = tokio::sync::mpsc::unbounded_channel();
    let (sent, mut sent_rx) = tokio::sync::mpsc::unbounded_channel();
    let recorded = Arc::new(Mutex::new(Vec::new()));
    let sink = recorded.clone();
    tokio::spawn(async move {
        while let Some(frame) = sent_rx.recv().await {
            let value = match frame {
                MessageFrame::Value(value) => value,
                MessageFrame::Text(text) => serde_json::from_str(&text).unwrap_or(Value::Null),
            };
            sink.lock().expect("recorded lock").push(value);
        }
    });
    (
        Client::connect_stream(ScriptedStream {
            incoming: incoming_rx,
            sent,
        }),
        recorded,
    )
}

pub(crate) fn live_recording_client() -> (
    Client,
    Arc<Mutex<Vec<Value>>>,
    tokio::sync::mpsc::UnboundedSender<MessageFrame>,
) {
    let (incoming, incoming_rx) = tokio::sync::mpsc::unbounded_channel();
    let (sent, mut sent_rx) = tokio::sync::mpsc::unbounded_channel();
    let recorded = Arc::new(Mutex::new(Vec::new()));
    let sink = recorded.clone();
    tokio::spawn(async move {
        while let Some(frame) = sent_rx.recv().await {
            let value = match frame {
                MessageFrame::Value(value) => value,
                MessageFrame::Text(text) => serde_json::from_str(&text).unwrap_or(Value::Null),
            };
            sink.lock().expect("recorded lock").push(value);
        }
    });
    (
        Client::connect_stream(ScriptedStream {
            incoming: incoming_rx,
            sent,
        }),
        recorded,
        incoming,
    )
}

pub(crate) fn last_request_params(recorded: &Arc<Mutex<Vec<Value>>>, method: &str) -> Value {
    recorded
        .lock()
        .expect("recorded")
        .iter()
        .rfind(|value| value["method"].as_str() == Some(method))
        .map(|value| value["params"].clone())
        .expect("recorded request")
}

pub(crate) fn recorded_method_count(recorded: &Arc<Mutex<Vec<Value>>>, method: &str) -> usize {
    recorded
        .lock()
        .expect("recorded")
        .iter()
        .filter(|value| value["method"].as_str() == Some(method))
        .count()
}

pub(crate) async fn handle_detached_replay(
    app: &mut App,
    deliveries: &mut tokio::sync::mpsc::UnboundedReceiver<ClientDelivery>,
    session_id: SessionId,
) {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let delivery = deliveries.recv().await.expect("event-loop delivery");
            let finished = matches!(
                &delivery,
                ClientDelivery::ReplayEnd {
                    session_id: replay_session,
                    ..
                } if *replay_session == session_id
            );
            app.handle_delivery(delivery).await;
            if finished {
                break;
            }
        }
    })
    .await
    .expect("detached replay timeout");
}

pub(crate) fn assistant_state(children: Vec<AssistantChild>) -> SessionState {
    SessionState {
        transcript: vec![TranscriptItem::Assistant {
            id: 1,
            version: 0,
            attribution: attribution(None),
            committed_turn_seq: Some(1),
            children,
        }],
        ..SessionState::default()
    }
}

pub(crate) fn assistant_projection(
    state: &SessionState,
) -> Vec<(String, Option<u64>, Vec<String>)> {
    state
        .transcript
        .iter()
        .filter_map(|item| match item {
            TranscriptItem::Assistant {
                attribution,
                committed_turn_seq,
                children,
                ..
            } => Some((
                attribution.header(),
                *committed_turn_seq,
                children
                    .iter()
                    .map(|child| match child {
                        AssistantChild::Text { markdown, .. } => {
                            format!("text:{}", markdown.as_str())
                        }
                        AssistantChild::Thinking { text, .. } => {
                            format!("thinking:{text}")
                        }
                        AssistantChild::Tool { call_id } => format!("tool:{call_id}"),
                        AssistantChild::Attribution { resolved_model } => format!(
                            "attribution:{}:{:?}",
                            resolved_model.selection.model, resolved_model.selection.variant
                        ),
                        AssistantChild::CommittedTool {
                            turn_seq,
                            content_index,
                            ..
                        } => format!("placeholder:{turn_seq}:{content_index}"),
                        AssistantChild::MediaFile {
                            turn_seq,
                            content_index,
                            ..
                        } => format!("media:{turn_seq}:{content_index}"),
                        AssistantChild::Notice { text } => format!("notice:{text}"),
                    })
                    .collect(),
            )),
            _ => None,
        })
        .collect()
}

pub(crate) fn rendered_frame(app: &mut App, width: u16, height: u16) -> String {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    terminal
        .draw(|frame| app.draw_for_test(frame))
        .expect("app render");
    let buffer = terminal.backend().buffer();
    (0..buffer.area.height)
        .flat_map(|y| (0..buffer.area.width).map(move |x| buffer[(x, y)].symbol().to_owned()))
        .collect::<String>()
}

pub(crate) fn rendered_row(app: &mut App, width: u16, height: u16, row: u16) -> String {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    terminal
        .draw(|frame| app.draw_for_test(frame))
        .expect("app render");
    let buffer = terminal.backend().buffer();
    (0..width)
        .map(|x| buffer[(x, row)].symbol())
        .collect::<String>()
}

pub(crate) fn rendered_cursor_visible(app: &mut App, width: u16, height: u16) -> bool {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    terminal
        .draw(|frame| app.draw_for_test(frame))
        .expect("app render");
    terminal.backend().cursor_visible()
}

pub(crate) fn rendered_agent_rows(app: &mut App, width: u16) -> Vec<String> {
    let backend = TestBackend::new(width, 30);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    terminal
        .draw(|frame| app.draw_for_test(frame))
        .expect("app render");
    let buffer = terminal.backend().buffer();
    (1..=3)
        .map(|y| {
            (1..width.saturating_sub(1))
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
                .trim_end()
                .to_owned()
        })
        .collect()
}

pub(crate) fn text_column(row: &str, text: &str) -> usize {
    let byte = row.find(text).expect("row text");
    row[..byte].chars().count()
}

pub(crate) fn chevron_counts(rendered: &str) -> (usize, usize) {
    (rendered.matches('▸').count(), rendered.matches('▾').count())
}

pub(crate) async fn open_usage_from_bottom_bar(app: &mut App) {
    rendered_frame(app, 80, 24);
    let hit = app.hit_map.session_cost.expect("session cost hit");
    app.handle_click(hit.x, hit.y).await;
    assert_eq!(app.modal, Modal::Usage);
}

pub(crate) fn usage_loaded_update(
    generation: u64,
    session_id: SessionId,
    request_count: u64,
) -> RpcUpdate {
    RpcUpdate::UsageLoaded {
        generation,
        session_id: Some(session_id),
        session: Ok(Some(cookie_agent_protocol::SessionUsageResult {
            session_id,
            usage: cookie_agent_protocol::UsageRollup {
                request_count,
                ..cookie_agent_protocol::UsageRollup::default()
            },
        })),
        tree: Ok(Some(cookie_agent_protocol::SessionTreeUsageResult {
            session_id,
            usage: cookie_agent_protocol::UsageRollup {
                request_count,
                ..cookie_agent_protocol::UsageRollup::default()
            },
            session_count: 1,
        })),
    }
}

pub(crate) fn failed_tree_usage_update(
    generation: u64,
    session_id: SessionId,
    code: i32,
    message: &str,
) -> RpcUpdate {
    RpcUpdate::UsageLoaded {
        generation,
        session_id: Some(session_id),
        session: Ok(None),
        tree: Err(crate::client::ClientError::Rpc(
            cookie_agent_protocol::JsonRpcError {
                code,
                message: message.into(),
                data: None,
            },
        )),
    }
}

pub(crate) fn assert_agent_panel_visible(app: &mut App, visible: bool) {
    let rows = frame_rows(app, 80, 24);
    assert_eq!(rows.iter().any(|row| row.contains("Agents")), visible);
    assert_eq!(app.hit_map.tree.is_some(), visible);
}

pub(crate) fn footer_event_log(
    session: SessionId,
    run: RunId,
    attempt: AttemptId,
    usage: Option<(u64, u64)>,
    commit_after_seconds: i64,
) -> Vec<StoredEvent> {
    let base: Timestamp = "2026-08-06T12:00:00Z".parse().expect("timestamp");
    let at = |seconds: i64| {
        base.checked_add(jiff::SignedDuration::from_secs(seconds))
            .expect("timestamp")
    };
    let stamp = |stored: StoredEvent, seconds: i64| StoredEvent {
        timestamp: at(seconds),
        ..stored
    };
    let mut commit = turn_committed(
        session,
        4,
        run,
        attempt,
        1,
        vec![text_part("the answer")],
        Vec::new(),
        None,
    );
    let EventPayload::ModelTurnCommitted {
        input_through_seq,
        turn,
        ..
    } = &mut commit.payload
    else {
        panic!("expected committed turn");
    };
    *input_through_seq = 2;
    match usage {
        Some((input_tokens, output_tokens)) => {
            turn.usage.input_tokens = Some(input_tokens);
            turn.usage.output_tokens = Some(output_tokens);
        }
        None => {
            turn.usage = Usage {
                input_tokens: None,
                input_tokens_no_cache: None,
                input_tokens_cache_read: None,
                input_tokens_cache_write: None,
                output_tokens: None,
                output_tokens_text: None,
                output_tokens_reasoning: None,
            };
        }
    }
    vec![
        stamp(session_created(session, 1), 0),
        stamp(
            event(
                session,
                2,
                run,
                EventPayload::UserInputSubmitted {
                    input: "question".into(),
                },
            ),
            0,
        ),
        stamp(attempt_started(session, 3, run, attempt, None), 0),
        stamp(commit, commit_after_seconds),
    ]
}

pub(crate) async fn app_with_footer_log(events: Vec<StoredEvent>, session: SessionId) -> App {
    let mut app = test_app().await;
    app.selected = Some(session);
    for event in events {
        assert!(app.store.apply_event(event));
    }
    app
}

pub(crate) fn children_has_tool(item: &TranscriptItem, call_id: ToolCallId) -> bool {
    match item {
        TranscriptItem::Assistant { children, .. } => children
            .iter()
            .any(|child| matches!(child, AssistantChild::Tool { call_id: id } if *id == call_id)),
        _ => false,
    }
}

pub(crate) fn mid_stream_failure(session: SessionId, seq: u64, run: RunId) -> StoredEvent {
    event(
        session,
        seq,
        run,
        EventPayload::InternalAgentFailed {
            invocation_id: cookie_agent_protocol::InternalAgentInvocationId::new_v7(),
            internal_run_id: cookie_agent_protocol::InternalAgentRunId::new_v7(),
            kind: cookie_agent_protocol::InternalAgentKind::ContextCompaction,
            failure: cookie_agent_protocol::InternalAgentFailure {
                code: cookie_agent_protocol::SafeCode::new("model_failure").expect("failure code"),
                message: cookie_agent_protocol::SafeErrorMessage::new("Compaction failed")
                    .expect("failure message"),
                retryable: false,
                model_error: None,
            },
        },
    )
}

pub(crate) fn assistant_items(state: &crate::state::SessionState) -> Vec<&TranscriptItem> {
    state
        .transcript
        .iter()
        .filter(|item| matches!(item, TranscriptItem::Assistant { .. }))
        .collect()
}

pub(crate) fn assistant_texts(item: &TranscriptItem) -> Vec<String> {
    let TranscriptItem::Assistant { children, .. } = item else {
        return Vec::new();
    };
    children
        .iter()
        .filter_map(|child| match child {
            AssistantChild::Text { markdown, .. } => Some(markdown.as_str().to_owned()),
            _ => None,
        })
        .collect()
}

pub(crate) fn mid_stream_info(session: SessionId, seq: u64, run: RunId) -> StoredEvent {
    event(
        session,
        seq,
        run,
        EventPayload::InternalAgentCompleted {
            invocation_id: cookie_agent_protocol::InternalAgentInvocationId::new_v7(),
            internal_run_id: cookie_agent_protocol::InternalAgentRunId::new_v7(),
            kind: cookie_agent_protocol::InternalAgentKind::ContextCompaction,
            result: cookie_agent_protocol::SafeInternalAgentResult {
                output_summary: cookie_agent_protocol::SafeDisplayText::new("compacted")
                    .expect("summary"),
                output_digest: Sha256Digest::of_bytes(b"summary"),
            },
        },
    )
}

pub(crate) fn transcript_shape(state: &crate::state::SessionState) -> Vec<String> {
    state
        .transcript
        .iter()
        .filter_map(|item| match item {
            TranscriptItem::Assistant { children, .. } => Some(format!(
                "assistant[{}]",
                children
                    .iter()
                    .map(|child| match child {
                        AssistantChild::Text { markdown, .. } =>
                            format!("text:{}", markdown.as_str()),
                        AssistantChild::Thinking { text, .. } => format!("thinking:{text}"),
                        AssistantChild::Tool { .. } => "tool".to_owned(),
                        AssistantChild::CommittedTool { .. } => "committed-tool".to_owned(),
                        AssistantChild::MediaFile { .. } => "media".to_owned(),
                        AssistantChild::Attribution { .. } => "attribution".to_owned(),
                        AssistantChild::Notice { text } => format!("notice:{text}"),
                    })
                    .collect::<Vec<_>>()
                    .join(",")
            )),
            TranscriptItem::Compaction { seq, .. } => Some(format!("compaction:{seq}")),
            _ => None,
        })
        .collect()
}

pub(crate) fn checkpoint_mid_run_events(
    session: SessionId,
    run: RunId,
    before: AttemptId,
    after: AttemptId,
    before_call: ToolCallId,
    after_call: ToolCallId,
) -> Vec<StoredEvent> {
    let completed = cookie_agent_protocol::ToolTerminationOutcome::Completed;
    vec![
        run_started_with_suffix(session, 1, run, vec![resolved_model(None)]),
        attempt_started(session, 2, run, before, None),
        reasoning_delta(session, 3, run, before, "pondering the pre-flight plan"),
        text_delta(session, 4, run, before, "answer before the checkpoint"),
        turn_committed(
            session,
            5,
            run,
            before,
            11,
            vec![
                reasoning_part("pondering the pre-flight plan"),
                text_part("answer before the checkpoint"),
                tool_part("pre-call"),
            ],
            Vec::new(),
            None,
        ),
        tool_started_at(
            session,
            6,
            run,
            before_call,
            11,
            "pre-call",
            2,
            "pre-call",
            None,
        ),
        tool_terminated(session, 7, run, before_call, 11, "pre-call", completed),
        event(
            session,
            8,
            run,
            EventPayload::ContextCheckpointCommitted {
                commit: checkpoint_commit("checkpoint summary"),
            },
        ),
        attempt_started(session, 9, run, after, None),
        text_delta(session, 10, run, after, "answer after the checkpoint"),
        reasoning_delta(session, 11, run, after, "pondering the post-flight plan"),
        turn_committed(
            session,
            12,
            run,
            after,
            12,
            vec![
                text_part("answer after the checkpoint"),
                reasoning_part("pondering the post-flight plan"),
                tool_part("post-call"),
            ],
            Vec::new(),
            None,
        ),
        tool_started_at(
            session,
            13,
            run,
            after_call,
            12,
            "post-call",
            2,
            "post-call",
            None,
        ),
        tool_terminated(session, 14, run, after_call, 12, "post-call", completed),
    ]
}

pub(crate) fn expand_thinking(state: &crate::state::SessionState, expanded: &mut HashSet<BlockId>) {
    for item in &state.transcript {
        if let TranscriptItem::Assistant { children, .. } = item {
            for child in children {
                if let AssistantChild::Thinking { id, .. } = child {
                    expanded.insert(BlockId::Thinking(*id));
                }
            }
        }
    }
}

pub(crate) fn checkpoint_recovery_events(
    session: SessionId,
    run: RunId,
    first: AttemptId,
    retry: AttemptId,
    warned: bool,
) -> Vec<StoredEvent> {
    let abandoned = |seq| {
        event(
            session,
            seq,
            run,
            EventPayload::AttemptAbandoned {
                attempt_id: first,
                model_error: None,
            },
        )
    };
    let checkpoint = |seq| {
        event(
            session,
            seq,
            run,
            EventPayload::ContextCheckpointCommitted {
                commit: checkpoint_commit("context length recovery"),
            },
        )
    };
    let answered = |seq| {
        turn_committed(
            session,
            seq,
            run,
            retry,
            1,
            vec![text_part("recovered answer")],
            Vec::new(),
            None,
        )
    };
    let head = run_started_with_suffix(session, 1, run, vec![resolved_model(None)]);
    let opened = attempt_started(session, 2, run, first, None);
    let rejected = text_delta(session, 3, run, first, "rejected partial");
    if warned {
        vec![
            head,
            opened,
            rejected,
            mid_stream_failure(session, 4, run),
            abandoned(5),
            checkpoint(6),
            attempt_started(session, 7, run, retry, None),
            text_delta(session, 8, run, retry, "recovered answer"),
            answered(9),
        ]
    } else {
        vec![
            head,
            opened,
            rejected,
            abandoned(4),
            checkpoint(5),
            attempt_started(session, 6, run, retry, None),
            text_delta(session, 7, run, retry, "recovered answer"),
            answered(8),
        ]
    }
}

pub(crate) fn delegated_meta(session_id: SessionId, root: SessionId, agent: &str) -> SessionMeta {
    SessionMeta {
        origin: SessionOrigin::Delegated {
            root_session_id: root,
            parent_session_id: root,
            parent_run_id: RunId::new_v7(),
            parent_tool_call_id: ToolCallId::new_v7(),
            invocation_id: cookie_agent_protocol::InvocationId::new_v7(),
            depth: 1,
        },
        creation_selection: RunSelection {
            agent: AgentId::new(agent).expect("agent id"),
            model: ModelSelection {
                model: model_key(),
                variant: None,
            },
            preset: None,
        },
        ..session_meta(session_id)
    }
}

pub(crate) fn run_started_with_suffix(
    session_id: SessionId,
    seq: u64,
    run: RunId,
    suffix: Vec<cookie_agent_protocol::ResolvedModelRef>,
) -> StoredEvent {
    let snapshot_chain = suffix
        .iter()
        .cloned()
        .map(frozen_binding)
        .collect::<Vec<_>>();
    let selection = RunSelection {
        agent: agent_id(),
        model: suffix[0].selection.clone(),
        preset: None,
    };
    event(
        session_id,
        seq,
        run,
        EventPayload::RunStarted {
            client_run_id: cookie_agent_protocol::ClientRunId::new("run-1").expect("run id"),
            selection: selection.clone(),
            agent: Box::new(cookie_agent_protocol::AgentSnapshot {
                agent: agent_id(),
                schema: cookie_agent_protocol::AgentSchemaVersion::current(),
                mode: cookie_agent_protocol::AgentMode::Primary,
                description: "Test primary agent".into(),
                document_source: cookie_agent_protocol::AgentDocumentSource::Workspace,
                document_fingerprint: Sha256Digest::of_bytes(b"document"),
                composed_prompt: "You are the primary test agent.\n".into(),
                prompt_fingerprint: Sha256Digest::of_bytes(b"prompt"),
                max_output_tokens: 0,
                permissions: Vec::new(),
                delegation: None,
                fallback_chain: snapshot_chain.clone(),
                selected_suffix_start: 0,
            }),
            runtime_revision: protocol_revision("1"),
            catalog_revision: protocol_revision("2"),
            provider_state_revision: protocol_revision("3"),
            model_revision: protocol_revision("4"),
            agent_revision: protocol_revision("5"),
            recipe_registry_revision: protocol_revision("6"),
            manifest_revision: protocol_revision("7"),
            selected_suffix: snapshot_chain,
            internal_agents: Vec::new(),
            input_through_seq: seq,
        },
    )
}

pub(crate) fn tall_transcript_state(lines: usize) -> SessionState {
    SessionState {
        transcript: (0..lines)
            .map(|index| TranscriptItem::Event {
                id: index as u64 + 1,
                version: 0,
                level: crate::state::EventLevel::Warning,
                text: format!("line {index}"),
            })
            .collect(),
        ..SessionState::default()
    }
}

pub(crate) fn conversation_rows(app: &mut App, width: u16, height: u16) -> Vec<String> {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| app.draw_for_test(frame)).unwrap();
    let viewport = app.hit_map.conversation.unwrap();
    let buffer = terminal.backend().buffer();
    (viewport.y..viewport.bottom())
        .map(|y| {
            (viewport.x..viewport.right())
                .map(|x| buffer[(x, y)].symbol())
                .collect()
        })
        .collect()
}

pub(crate) async fn expansion_scroll_app(kind: &str) -> (App, SessionId, BlockId) {
    let mut app = test_app().await;
    let session = SessionId::new_v7();
    let body = "expanded\n".repeat(70);
    let (mut state, block) = match kind {
        "thinking" => (
            assistant_state(vec![AssistantChild::Thinking {
                id: 2,
                version: 0,
                text: body.clone(),
            }]),
            BlockId::Thinking(2),
        ),
        "tool" => {
            let state = read_tool_state("src/a/long/path/main.rs", ToolStatus::Completed, &body);
            let block = BlockId::Tool(read_tool_id(&state));
            (state, block)
        }
        _ => {
            let mut store = StateStore::default();
            let payload = match kind {
                "plugin" => EventPayload::MessageInjected {
                    role: cookie_agent_protocol::ExtensionMessageRole::User,
                    input: body.clone(),
                },
                "compaction" => EventPayload::ContextCheckpointCommitted {
                    commit: checkpoint_commit(&body),
                },
                "system" => {
                    let EventPayload::RunStarted { mut agent, .. } =
                        run_started_with_suffix(session, 1, run_id(), vec![resolved_model(None)])
                            .payload
                    else {
                        unreachable!()
                    };
                    agent.composed_prompt = body.clone();
                    let state = SessionState {
                        run_snapshot: Some(agent),
                        ..SessionState::default()
                    };
                    app.store.sessions.insert(session, state);
                    app.selected = Some(session);
                    app.tree_root = Some(session);
                    return (app, session, BlockId::SystemPrompt);
                }
                "producer" => {
                    producer_accepted(
                        session,
                        1,
                        ProducerMessageId::new_v7(),
                        ProducerOwner::Plugin {
                            plugin: "build".into(),
                        },
                        ProducerDeliveryMode::Queue,
                        &body,
                        None,
                    )
                    .payload
                }
                _ => unreachable!(),
            };
            store.apply_event(runless_event(session, 1, payload));
            let mut state = store.sessions.remove(&session).unwrap();
            if kind == "producer" {
                for item in &mut state.transcript {
                    if let TranscriptItem::ProducerMessage { status, .. } = item {
                        *status = ProducerMessageStatus::Consumed;
                    }
                }
            }
            let block = transcript_layout(&state, None, 80).regions[0].id;
            (state, block)
        }
    };
    let mut prefix = tall_transcript_state(40).transcript;
    // Keep synthetic prefix identities separate from the fixture's items.
    for item in &mut prefix {
        if let TranscriptItem::Event { id, .. } = item {
            *id += 10_000;
        }
    }
    prefix.append(&mut state.transcript);
    state.transcript = prefix;
    app.store.sessions.insert(session, state);
    app.selected = Some(session);
    app.tree_root = Some(session);
    (app, session, block)
}

pub(crate) fn drawn_styles(app: &mut App, width: u16, height: u16) -> Vec<Vec<Style>> {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("test terminal");
    terminal
        .draw(|frame| app.draw_for_test(frame))
        .expect("app render");
    let buffer = terminal.backend().buffer();
    (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].style())
                .collect()
        })
        .collect()
}

pub(crate) fn frame_cells(app: &mut App, width: u16, height: u16) -> Vec<Vec<String>> {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("test terminal");
    terminal
        .draw(|frame| app.draw_for_test(frame))
        .expect("app render");
    let buffer = terminal.backend().buffer();
    (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol().to_owned())
                .collect()
        })
        .collect()
}

pub(crate) async fn app_with_overflowing_composer()
-> (App, crate::ui::transcript::ScrollbarGeometry) {
    let mut app = test_app().await;
    app.handle_paste("a\nb\nc\nd\ne\nf\ng\nh");
    rendered_frame(&mut app, 80, 50);
    let geometry = app
        .hit_map
        .input
        .expect("input hit")
        .scrollbar
        .expect("composer scrollbar at the overflowing ceiling");
    assert_eq!(geometry.track.width, 1);
    (app, geometry)
}

pub(crate) async fn app_with_active_run() -> (App, SessionId, RunId) {
    let mut app = test_app().await;
    let session = SessionId::new_v7();
    let run = run_id();
    app.selected = Some(session);
    app.store.sessions.insert(
        session,
        SessionState {
            active_run: Some(run),
            run_agent: Some(agent_id()),
            initial_input_submitted: HashSet::from([run]),
            ..SessionState::default()
        },
    );
    (app, session, run)
}

pub(crate) fn live_event(event: StoredEvent) -> ClientDelivery {
    ClientDelivery::Live {
        message: Box::new(EventSubscriptionMessage::Event {
            event: Box::new(event),
        }),
        generation: 0,
    }
}

pub(crate) fn admitted(session: SessionId, seq: u64, run: RunId, input: &str) -> StoredEvent {
    event(
        session,
        seq,
        run,
        EventPayload::UserInputAdmitted {
            input: input.into(),
        },
    )
}

pub(crate) fn recalled(session: SessionId, seq: u64, run: RunId, input: &str) -> StoredEvent {
    event(
        session,
        seq,
        run,
        EventPayload::UserInputRecalled {
            input: input.into(),
        },
    )
}

pub(crate) fn user_input(session: SessionId, seq: u64, run: RunId, input: &str) -> StoredEvent {
    event(
        session,
        seq,
        run,
        EventPayload::UserInputSubmitted {
            input: input.into(),
        },
    )
}

pub(crate) fn producer_accepted(
    session: SessionId,
    seq: u64,
    message_id: ProducerMessageId,
    owner: ProducerOwner,
    mode: ProducerDeliveryMode,
    body: &str,
    reminder: Option<cookie_agent_protocol::GoalReminderIdentity>,
) -> StoredEvent {
    runless_event(
        session,
        seq,
        EventPayload::ProducerMessageAccepted {
            description: Default::default(),
            message_id,
            producer_owner: owner,
            mode,
            idempotency_key: cookie_agent_protocol::ProducerIdempotencyKey::new(format!(
                "message-{seq}"
            ))
            .expect("idempotency key"),
            body: body.to_owned(),
            reminder,
            agent_hop: None,
        },
    )
}

pub(crate) fn set_producer_status(
    app: &mut App,
    session: SessionId,
    message_id: ProducerMessageId,
    next: crate::state::ProducerMessageStatus,
) {
    let item = app
        .store
        .sessions
        .get_mut(&session)
        .expect("session")
        .transcript
        .iter_mut()
        .find(|item| {
            matches!(
                item,
                TranscriptItem::ProducerMessage {
                    message_id: candidate,
                    ..
                } if *candidate == message_id
            )
        })
        .expect("producer transcript item");
    let TranscriptItem::ProducerMessage { status, .. } = item else {
        unreachable!("matched producer transcript item")
    };
    *status = next;
}

pub(crate) fn pending_texts(app: &App, session: SessionId) -> Vec<&str> {
    app.store
        .sessions
        .get(&session)
        .map(|state| {
            state
                .pending_inputs
                .iter()
                .map(|pending| pending.text.as_str())
                .collect()
        })
        .unwrap_or_default()
}

pub(crate) async fn wait_for_recorded_request(
    recorded: &Arc<Mutex<Vec<Value>>>,
    method: &str,
    count: usize,
) -> i64 {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let found = recorded
                .lock()
                .expect("recorded")
                .iter()
                .filter(|value| value["method"].as_str() == Some(method))
                .count();
            if found >= count {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("recorded request timeout");
    recorded
        .lock()
        .expect("recorded")
        .iter()
        .rfind(|value| value["method"].as_str() == Some(method))
        .and_then(|value| value["id"].as_i64())
        .expect("request id")
}

pub(crate) async fn drive_until_recorded_request(
    app: &mut App,
    recorded: &Arc<Mutex<Vec<Value>>>,
    method: &str,
    count: usize,
) -> i64 {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if recorded_method_count(recorded, method) >= count {
                break;
            }
            tokio::select! {
                update = app.rpc_updates_rx.recv() => {
                    app.handle_rpc_update(update.expect("RPC update channel"));
                }
                () = tokio::time::sleep(Duration::from_millis(10)) => {}
            }
        }
    })
    .await
    .expect("recorded request timeout");
    recorded
        .lock()
        .expect("recorded")
        .iter()
        .rfind(|value| value["method"].as_str() == Some(method))
        .and_then(|value| value["id"].as_i64())
        .expect("request id")
}

/// Run a command the way a user does: Ctrl-P, type the search, Enter.
pub(crate) async fn run_palette_command(app: &mut App, query: &str) {
    app.handle_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL))
        .await;
    type_input(app, query).await;
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .await;
}

pub(crate) async fn settle_recording() {
    for _ in 0..3 {
        tokio::task::yield_now().await;
    }
}

pub(crate) async fn wait_for_method(recorded: &Arc<Mutex<Vec<Value>>>, method: &str, count: usize) {
    for _ in 0..100 {
        if recorded_method_count(recorded, method) == count {
            return;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(recorded_method_count(recorded, method), count, "{method}");
}

pub(crate) fn read_tool_state(path: &str, status: ToolStatus, detail: &str) -> SessionState {
    let call_id = ToolCallId::new_v7();
    let mut state = assistant_state(vec![AssistantChild::Tool { call_id }]);
    state.tools.insert(
        call_id,
        ToolCallState {
            id: call_id,
            owner: owner(1, "call-1"),
            presentation: presentation("read", None),
            arguments: format!(r#"{{"path": "{path}"}}"#),
            status,
            detail: detail.into(),
            has_output_chunks: false,
        },
    );
    state
}

pub(crate) fn read_detail(rows: &[(usize, &str)]) -> String {
    let mut detail =
        "Read file src/main.rs\n<path>src/main.rs</path>\n<type>file</type>\n<content>\n"
            .to_owned();
    for (number, text) in rows {
        detail.push_str(&format!("{number}: {text}\n"));
    }
    detail.push_str("</content>\nmetadata: {\"kind\":\"text\"}");
    detail
}

pub(crate) fn expanded_read_layout(state: &SessionState, theme: &Theme) -> Vec<Line<'static>> {
    let call_id = read_tool_id(state);
    let expanded = std::collections::HashSet::from([BlockId::Tool(call_id)]);
    transcript_layout_with(
        state,
        Some(&expanded),
        80,
        theme,
        &crate::markdown::SyntectHighlighter::default(),
    )
    .lines
}

pub(crate) fn read_tool_id(state: &SessionState) -> ToolCallId {
    state
        .tools
        .keys()
        .next()
        .copied()
        .expect("read tool present")
}

pub(crate) fn tool_output_id(call_id: ToolCallId, section: ToolOutputSection) -> BlockId {
    BlockId::ToolOutput { call_id, section }
}

pub(crate) fn tool_layout_at_clock(
    state: &SessionState,
    expanded: Option<&HashSet<BlockId>>,
    width: u16,
    clock_bucket: u8,
) -> TranscriptLayout {
    transcript_layout_at_clock(
        state,
        expanded,
        width,
        &Theme::default(),
        &PlainHighlighter,
        crate::state::EventLevel::Debug,
        clock_bucket,
    )
}

pub(crate) fn render_to_buffer(lines: &[Line<'static>], width: u16) -> Vec<String> {
    use ratatui::{
        buffer::Buffer,
        widgets::{Paragraph, Widget},
    };
    let area = Rect::new(
        0,
        0,
        width,
        u16::try_from(lines.len()).unwrap_or(u16::MAX - 1).max(1),
    );
    let mut buffer = Buffer::empty(area);
    Paragraph::new(lines.to_vec()).render(area, &mut buffer);
    (0..area.height)
        .map(|row| {
            (0..width)
                .map(|column| buffer[(column, row)].symbol())
                .collect::<String>()
                .trim_end()
                .to_owned()
        })
        .filter(|row| !row.is_empty())
        .collect()
}

pub(crate) fn catalog_provider() -> cookie_agent_protocol::ProviderDescriptor {
    provider_descriptor("acme-ai", "supported", "current", false)
}

pub(crate) async fn type_input(app: &mut App, text: &str) {
    for character in text.chars() {
        app.handle_key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
            .await;
    }
}

pub(crate) fn push_model_warning(store: &mut StateStore, session_id: SessionId, text: &str) {
    let run = run_id();
    let attempt = AttemptId::new_v7();
    assert!(store.apply_event(attempt_started(session_id, 1, run, attempt, None)));
    assert!(store.apply_event(turn_committed(
        session_id,
        2,
        run,
        attempt,
        1,
        Vec::new(),
        vec![text],
        None,
    )));
}

pub(crate) fn parallel_tools_state() -> (StateStore, SessionId, ToolCallId, ToolCallId) {
    let session = SessionId::new_v7();
    let run = run_id();
    let attempt = AttemptId::new_v7();
    let first = ToolCallId::new_v7();
    let second = ToolCallId::new_v7();
    let mut store = StateStore::default();
    let events = [
        session_created(session, 1),
        attempt_started(session, 2, run, attempt, None),
        text_delta(session, 3, run, attempt, "launching both"),
        turn_committed(
            session,
            4,
            run,
            attempt,
            5,
            vec![
                cookie_agent_protocol::PersistedAssistantPart::Text {
                    text: "launching both".into(),
                    metadata: None,
                },
                cookie_agent_protocol::PersistedAssistantPart::ToolCall {
                    id: ModelCallId::new("call-a").expect("call"),
                    provider_item_id: None,
                    name: SafeCode::new("bash").expect("tool"),
                    input: serde_json::json!({"command": "sleep 2"}),
                    raw_input: None,
                    metadata: None,
                },
                cookie_agent_protocol::PersistedAssistantPart::Reasoning {
                    text: "waiting on both".into(),
                    metadata: None,
                },
                cookie_agent_protocol::PersistedAssistantPart::ToolCall {
                    id: ModelCallId::new("call-b").expect("call"),
                    provider_item_id: None,
                    name: SafeCode::new("read").expect("tool"),
                    input: serde_json::json!({"path": "src/lib.rs"}),
                    raw_input: None,
                    metadata: None,
                },
            ],
            Vec::new(),
            None,
        ),
        // Out-of-order: call-b starts first and completes before call-a
        // even starts.
        tool_started_at(
            session,
            5,
            run,
            second,
            5,
            "call-b",
            3,
            "read",
            Some("src/lib.rs"),
        ),
        tool_terminated(
            session,
            6,
            run,
            second,
            5,
            "call-b",
            cookie_agent_protocol::ToolTerminationOutcome::Completed,
        ),
        tool_started_at(
            session,
            7,
            run,
            first,
            5,
            "call-a",
            1,
            "bash",
            Some("sleep 2"),
        ),
    ];
    for event in events {
        assert!(store.apply_event(event));
    }
    (store, session, first, second)
}

pub(crate) fn snapshot_lines(lines: &[Line<'static>]) -> String {
    lines
        .iter()
        .map(|line| line.to_string().trim_end().to_owned())
        .collect::<Vec<_>>()
        .join("\n")
}

pub(crate) fn stable_approval_snapshot(approval: &ApprovalState, mut snapshot: String) -> String {
    snapshot = snapshot.replace(&approval.approval_id.to_string(), "<approval-id>");
    snapshot = snapshot.replace(
        approval.operation_fingerprint.digest().as_str(),
        "<operation-fingerprint>",
    );
    snapshot = snapshot.replace(
        approval.normalized_arguments_digest.as_str(),
        "<normalized-arguments-digest>",
    );
    snapshot = snapshot.replace(
        approval.execution_context_digest.as_str(),
        "<execution-context-digest>",
    );
    for (index, resource) in approval.resources.iter().enumerate() {
        snapshot = snapshot.replace(
            resource.binding_digest.digest().as_str(),
            &format!("<resource-{}-binding-digest>", index + 1),
        );
    }
    snapshot
}

pub(crate) fn bash_approval_state() -> ApprovalState {
    approval(SessionId::new_v7())
}

pub(crate) async fn app_with_approval() -> App {
    let mut app = test_app().await;
    let approval = bash_approval_state();
    app.selected = Some(approval.session_id);
    app.store
        .sessions
        .entry(approval.session_id)
        .or_default()
        .approvals
        .push(approval);
    app
}

pub(crate) async fn app_with_user_messages() -> (App, SessionId, Arc<Mutex<Vec<String>>>) {
    let mut app = test_app().await;
    app.theme = Theme::new(ThemeKind::Default, ColorLevel::TrueColor);
    let session = SessionId::new_v7();
    let run = run_id();
    let copied = Arc::new(Mutex::new(Vec::new()));
    app.clipboard_sink = ClipboardSink::Capture(copied.clone());
    app.selected = Some(session);
    app.tree_root = Some(session);
    app.store.sessions.insert(session, SessionState::default());
    assert!(
        app.store
            .apply_event(user_input(session, 1, run, "first question"))
    );
    assert!(
        app.store
            .apply_event(user_input(session, 2, run, "second question"))
    );
    (app, session, copied)
}

pub(crate) fn user_hit(app: &App, seq: u64) -> UserMessageHit {
    app.hit_map
        .user_messages
        .iter()
        .copied()
        .find(|hit| hit.seq == seq)
        .expect("user message hit")
}

pub(crate) fn mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
    MouseEvent {
        kind,
        column,
        row,
        modifiers: KeyModifiers::NONE,
    }
}

pub(crate) async fn drag_selection(app: &mut App, viewport: Rect, dx: u16, dy: u16) {
    app.handle_mouse(mouse(
        MouseEventKind::Down(MouseButton::Left),
        viewport.x,
        viewport.y + 1,
    ))
    .await;
    app.handle_mouse(mouse(
        MouseEventKind::Drag(MouseButton::Left),
        viewport.x + dx,
        viewport.y + 1 + dy,
    ))
    .await;
    app.handle_mouse(mouse(
        MouseEventKind::Up(MouseButton::Left),
        viewport.x + dx,
        viewport.y + 1 + dy,
    ))
    .await;
}

pub(crate) async fn composer_drag_selection(app: &mut App, text_rect: Rect, from: u16, to: u16) {
    app.handle_mouse(mouse(
        MouseEventKind::Down(MouseButton::Left),
        text_rect.x + from,
        text_rect.y,
    ))
    .await;
    app.handle_mouse(mouse(
        MouseEventKind::Drag(MouseButton::Left),
        text_rect.x + to,
        text_rect.y,
    ))
    .await;
    app.handle_mouse(mouse(
        MouseEventKind::Up(MouseButton::Left),
        text_rect.x + to,
        text_rect.y,
    ))
    .await;
}
