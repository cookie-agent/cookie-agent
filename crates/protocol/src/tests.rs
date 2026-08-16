use std::collections::{BTreeMap, BTreeSet};

use schemars::schema_for;
use serde_json::json;
use ts_rs::TS;

use super::*;

fn revision<T>(value: &str) -> T
where
    T: TryFromRevision,
{
    T::from_revision(format!(
        "sha256:{}",
        Sha256Digest::of_bytes(value.as_bytes()).as_str()
    ))
}

trait TryFromRevision: Sized {
    fn from_revision(value: String) -> Self;
}

macro_rules! revision_constructor {
    ($($name:ty),+ $(,)?) => {$(
        impl TryFromRevision for $name {
            fn from_revision(value: String) -> Self { <$name>::new(value).expect("revision") }
        }
    )+};
}
revision_constructor!(
    CatalogRevision,
    RecipeRegistryRevision,
    ProviderStoreRevision,
    ProviderStateRevision,
    ModelRevision,
    ModelSnapshotRevision,
    AgentRevision,
    RuntimeRevision
);

fn frozen_binding() -> FrozenModelBinding {
    let selection = ModelSelection {
        model: "openai/gpt-5.6-sol".parse().unwrap(),
        variant: None,
    };
    let descriptor = oven_sdk::LanguageModelDescriptor::new(
        oven_sdk::ModelIdentity::new(
            oven_sdk::ProviderId::new("openai"),
            oven_sdk::ModelId::new("gpt-5.6-sol"),
        )
        .unwrap(),
        oven_sdk::AdapterId::new("openai-responses"),
        oven_sdk::ModelCapabilities::conservative(),
    )
    .unwrap();
    FrozenModelBinding {
        manifest_revision: revision("manifest"),
        blueprint_fingerprint: Sha256Digest::of_bytes(b"blueprint"),
        selection,
        source: FrozenProviderSource::Managed {
            provider_recipe: ProviderRecipeId::new("openai.responses.v1").unwrap(),
            source_record_digest: Sha256Digest::of_bytes(b"source"),
            recipe_fingerprint: Sha256Digest::of_bytes(b"recipe"),
            package_claim: "@ai-sdk/openai".into(),
        },
        config_override_fingerprint: Sha256Digest::of_bytes(b"override"),
        credential_binding: FrozenCredentialBinding {
            source: FrozenCredentialSource::ProviderStore,
            auth_method: AuthMethodId::new("bearer-api-key-v1").unwrap(),
            fields: vec![AuthFieldName::new("api_key").unwrap()],
            parameters: BTreeMap::new(),
            owned_headers: vec![HeaderName::new("authorization").unwrap()],
        },
        setup_binding: FrozenSetupBinding {
            setup_recipe: ProviderSetupRecipeId::new("empty-setup-v1").unwrap(),
            values: BTreeMap::new(),
            setup_fingerprint: Sha256Digest::of_bytes(b"setup"),
        },
        endpoint_identity: SafeEndpointIdentity::new("https://api.openai.com/v1").unwrap(),
        provider_recipe: ProviderRecipeId::new("openai.responses.v1").unwrap(),
        protocol_recipe: ProtocolRecipeId::new("oven.openai.responses").unwrap(),
        setup_recipe: ProviderSetupRecipeId::new("empty-setup-v1").unwrap(),
        compiler_version: RecipeCompilerVersion::new("family-registry").unwrap(),
        descriptor,
        defaults: FrozenResolvedRequestDefaults {
            request: FrozenRequestDefaults::default(),
            reasoning: None,
        },
        options: ProviderOptions::OpenAiResponses {
            organization: None,
            project: None,
            store: None,
        },
        static_headers: BTreeMap::new(),
        behavior_fingerprint: Sha256Digest::of_bytes(b"behavior"),
        selection_fingerprint: Sha256Digest::of_bytes(b"selection"),
    }
}

fn child_agent(binding: FrozenModelBinding) -> AgentSnapshot {
    AgentSnapshot {
        agent: AgentId::new("worker").unwrap(),
        schema: AgentSchemaVersion::current(),
        mode: AgentMode::Subagent,
        description: "Delegated worker".into(),
        document_source: AgentDocumentSource::Workspace,
        document_fingerprint: Sha256Digest::of_bytes(b"document"),
        composed_prompt: "Complete the delegated task.".into(),
        prompt_fingerprint: Sha256Digest::of_bytes(b"prompt"),
        permissions: Vec::new(),
        delegation: None,
        fallback_chain: vec![binding],
        selected_suffix_start: 0,
    }
}

fn runtime() -> RuntimeSnapshotV1 {
    RuntimeSnapshotV1 {
        snapshot_schema_version: RuntimeSnapshotSchemaVersion::current(),
        recipe_registry_revision: revision("recipes"),
        catalog_revision: revision("catalog"),
        catalog_source: CatalogSource::Network,
        catalog_state: CatalogRuntimeState {
            stale: false,
            provider_quarantine_count: 0,
            model_quarantine_count: 0,
            quarantine_digest: Sha256Digest::of_bytes(b"quarantine"),
            last_error: None,
        },
        provider_state_revision: revision("providers"),
        provider_store_generation: ProviderStoreGeneration::new(1).unwrap(),
        model_revision: revision("models"),
        agent_revision: revision("agents"),
        runtime_revision: revision("runtime"),
        providers: Vec::new(),
        models: Vec::new(),
        agents: Vec::new(),
    }
}

#[test]
fn wire_versions_accept_only_documented_event_and_delegation_history() {
    assert_eq!(PROTOCOL_VERSION, 9);
    assert_eq!(EVENT_SCHEMA_VERSION, 18);
    assert_eq!(SESSION_META_SCHEMA_VERSION, 9);
    assert_eq!(DELEGATION_JOURNAL_SCHEMA_VERSION, 14);
    assert_eq!(RUNTIME_SNAPSHOT_SCHEMA_VERSION, 4);
    assert!(serde_json::from_value::<ProtocolVersion>(json!(8)).is_err());
    assert!(serde_json::from_value::<AgentSchemaVersion>(json!(4)).is_err());
    for version in [15, 16, 17, 18] {
        assert_eq!(
            serde_json::from_value::<EventSchemaVersion>(json!(version))
                .unwrap()
                .value(),
            version
        );
    }
    assert!(serde_json::from_value::<EventSchemaVersion>(json!(14)).is_err());
    assert!(serde_json::from_value::<EventSchemaVersion>(json!(19)).is_err());
    assert_eq!(
        serde_json::from_value::<DelegationJournalSchemaVersion>(json!(11))
            .unwrap()
            .value(),
        11
    );
    assert_eq!(
        serde_json::from_value::<DelegationJournalSchemaVersion>(json!(14))
            .unwrap()
            .value(),
        14
    );
    assert!(serde_json::from_value::<DelegationJournalSchemaVersion>(json!(10)).is_err());
    assert!(serde_json::from_value::<DelegationJournalSchemaVersion>(json!(15)).is_err());
    assert!(serde_json::from_value::<RuntimeSnapshotSchemaVersion>(json!(3)).is_err());
    assert!(serde_json::from_value::<ModelSnapshotManifestSchemaVersion>(json!(2)).is_err());
}

#[test]
fn legacy_agent_snapshot_upconversion_is_exact_and_writes_schema_five() {
    let mut legacy = serde_json::to_value(child_agent(frozen_binding())).unwrap();
    legacy["schema"] = json!(4);
    legacy["tools"] = json!(["read", "write", "edit", "bash"]);

    let converted = serde_json::from_value::<AgentSnapshot>(legacy.clone()).unwrap();
    assert_eq!(converted.schema.value(), 5);
    let current = serde_json::to_value(converted).unwrap();
    assert_eq!(current["schema"], 5);
    assert!(current.get("tools").is_none());

    let mut unknown_field = legacy.clone();
    unknown_field["legacy_extra"] = json!(true);
    assert!(serde_json::from_value::<AgentSnapshot>(unknown_field).is_err());

    let mut missing_tools = legacy.clone();
    missing_tools.as_object_mut().unwrap().remove("tools");
    assert!(serde_json::from_value::<AgentSnapshot>(missing_tools).is_err());

    let mut duplicate_tools = legacy.clone();
    duplicate_tools["tools"] = json!(["read", "read"]);
    assert!(serde_json::from_value::<AgentSnapshot>(duplicate_tools).is_err());

    let mut unknown_tool = legacy;
    unknown_tool["tools"] = json!(["grep"]);
    assert!(serde_json::from_value::<AgentSnapshot>(unknown_tool).is_err());
}

#[test]
fn pending_input_events_and_recall_rpc_round_trip() {
    let run_id = RunId::new_v7();
    for payload in [
        EventPayload::UserInputAdmitted {
            input: "pending".into(),
        },
        EventPayload::UserInputRecalled {
            input: "pending".into(),
        },
        EventPayload::UserInputRecalledV2 {
            user_input_seq: 1,
            input: "pending".into(),
        },
        EventPayload::UserInputSubmitted {
            input: "pending".into(),
        },
    ] {
        let value = serde_json::to_value(&payload).expect("serialize pending-input event");
        assert_eq!(
            serde_json::from_value::<EventPayload>(value).expect("deserialize pending-input event"),
            payload
        );
    }
    let runless_admission = StoredEvent {
        event_schema_version: EventSchemaVersion::current(),
        session_id: SessionId::new_v7(),
        run_id: None,
        seq: 1,
        timestamp: jiff::Timestamp::now(),
        payload: EventPayload::UserInputAdmitted {
            input: "queued steer".into(),
        },
    };
    assert!(runless_admission.validate().is_ok());
    let params = RunRecallSteerParams { run_id };
    assert_eq!(
        serde_json::from_value::<RunRecallSteerParams>(
            serde_json::to_value(params).expect("serialize recall params")
        )
        .expect("deserialize recall params")
        .run_id,
        run_id
    );
    for recalled in [Some("pending".to_owned()), None] {
        let result = RunRecallSteerResult {
            recalled: recalled.clone(),
        };
        assert_eq!(
            serde_json::from_value::<RunRecallSteerResult>(
                serde_json::to_value(result).expect("serialize recall result")
            )
            .expect("deserialize recall result")
            .recalled,
            recalled
        );
    }
}

#[test]
fn session_revert_reduces_visible_branch_and_rpc_types_round_trip() {
    let session_id = SessionId::new_v7();
    let event = |seq, payload| StoredEvent {
        event_schema_version: EventSchemaVersion::current(),
        session_id,
        run_id: None,
        seq,
        timestamp: jiff::Timestamp::now(),
        payload,
    };
    let events = vec![
        event(
            1,
            EventPayload::SessionTitleCommitted {
                change: SessionTitleChange::FallbackSet {
                    title: SessionTitle::new("creation").expect("title"),
                },
                input_through_seq: 1,
            },
        ),
        event(
            2,
            EventPayload::SessionTitleCommitted {
                change: SessionTitleChange::FallbackSet {
                    title: SessionTitle::new("first").expect("title"),
                },
                input_through_seq: 1,
            },
        ),
        event(3, EventPayload::SessionReverted { through_seq: 1 }),
        event(
            4,
            EventPayload::SessionTitleCommitted {
                change: SessionTitleChange::FallbackSet {
                    title: SessionTitle::new("branch").expect("title"),
                },
                input_through_seq: 1,
            },
        ),
        event(5, EventPayload::SessionReverted { through_seq: 3 }),
    ];
    assert_eq!(
        visible_events(&events)
            .iter()
            .map(|event| event.seq)
            .collect::<Vec<_>>(),
        vec![1, 5]
    );
    let invalid = StoredEvent {
        event_schema_version: EventSchemaVersion::current(),
        session_id,
        run_id: None,
        seq: 6,
        timestamp: jiff::Timestamp::now(),
        payload: EventPayload::SessionReverted { through_seq: 0 },
    };
    assert!(serde_json::from_value::<StoredEvent>(serde_json::to_value(invalid).unwrap()).is_err());

    let revert = SessionRevertParams {
        session_id,
        through_seq: 1,
    };
    assert_eq!(
        serde_json::from_value::<SessionRevertParams>(serde_json::to_value(revert).unwrap())
            .unwrap()
            .through_seq,
        1
    );
    let fork = SessionForkParams {
        session_id,
        through_seq: 1,
    };
    assert_eq!(
        serde_json::from_value::<SessionForkParams>(serde_json::to_value(fork).unwrap())
            .unwrap()
            .session_id,
        session_id
    );
}

fn test_resource(binding: &[u8]) -> PreparedApprovalResource {
    PreparedApprovalResource {
        capability: PermissionAction::Read,
        canonical: PreparedResourceIdentity::new(format!(
            "file:{}",
            Sha256Digest::of_bytes(binding)
        ))
        .expect("resource identity"),
        binding_digest: PreparedResourceDigest::from_canonical_binding_bytes(binding),
        binding_lifetime: PreparedBindingLifetime::ProcessLocal,
        boundary: ApprovalBoundary::Exact,
        source: ApprovalResourceSource::PrimaryOperation,
    }
}

#[test]
fn prepared_operation_wire_requires_nonempty_resources_and_accepts_multiple() {
    let capability = ApprovalCapability {
        action: PermissionAction::Read,
        operation: PreparedCapabilityOperation::new("read:file").expect("capability operation"),
    };
    let operation = PreparedOperationIdentity::new(
        Sha256Digest::of_bytes(b"arguments"),
        vec![capability],
        vec![test_resource(b"first"), test_resource(b"second")],
        Sha256Digest::of_bytes(b"context"),
    )
    .expect("multiple resources");
    let valid = serde_json::to_value(operation).expect("operation JSON");
    assert_eq!(valid["resources"].as_array().expect("resources").len(), 2);
    assert!(valid.get("resource").is_none());
    assert!(serde_json::from_value::<PreparedOperationIdentity>(valid.clone()).is_ok());

    let mut missing = valid.clone();
    missing
        .as_object_mut()
        .expect("operation object")
        .remove("resources");
    assert!(serde_json::from_value::<PreparedOperationIdentity>(missing).is_err());

    let mut empty = valid.clone();
    empty["resources"] = json!([]);
    assert!(serde_json::from_value::<PreparedOperationIdentity>(empty).is_err());

    let mut singular = valid;
    let object = singular.as_object_mut().expect("operation object");
    object.remove("resources");
    object.insert("resource".into(), json!(test_resource(b"first")));
    assert!(serde_json::from_value::<PreparedOperationIdentity>(singular).is_err());
}

fn test_evaluation(binding: &[u8]) -> ApprovalEvaluation {
    let resource = test_resource(binding);
    ApprovalEvaluation {
        resource_digest: resource.binding_digest,
        effect: PermissionEffect::Ask,
        trace: DecisionTrace {
            action: PermissionAction::Read,
            normalized_resource: "README.md".into(),
            candidates: Vec::new(),
            effect: PermissionEffect::Ask,
            precedence_reason: "test".into(),
        },
    }
}

#[test]
fn approval_wire_requires_nonempty_evaluations_and_accepts_multiple() {
    let first = b"request-first";
    let second = b"request-second";
    let operation = PreparedOperationIdentity::new(
        Sha256Digest::of_bytes(b"arguments"),
        vec![ApprovalCapability {
            action: PermissionAction::Read,
            operation: PreparedCapabilityOperation::new("read:file").expect("capability operation"),
        }],
        vec![test_resource(first), test_resource(second)],
        Sha256Digest::of_bytes(b"context"),
    )
    .expect("operation");
    let request = ApprovalRequest::new(
        ApprovalId::new_v7(),
        1,
        ApprovalTrigger::PermissionPolicy,
        operation,
        vec![test_evaluation(first), test_evaluation(second)],
        ApprovalConstraints {
            allow_once: true,
            allow_tree_grant: false,
            cancellable: true,
            expires_at: None,
        },
    )
    .expect("approval request");
    let valid = serde_json::to_value(request).expect("request JSON");
    assert_eq!(
        valid["evaluations"].as_array().expect("evaluations").len(),
        2
    );
    assert!(valid.get("evaluation").is_none());
    assert!(serde_json::from_value::<ApprovalRequest>(valid.clone()).is_ok());

    let mut empty = valid.clone();
    empty["evaluations"] = json!([]);
    assert!(serde_json::from_value::<ApprovalRequest>(empty).is_err());

    let decision = ApprovalInternalDecision {
        decision: ApprovalInternalDecisionKind::Ask,
        source: ApprovalDecisionSource::Policy,
        reason_code: ApprovalReasonCode::PolicyRequiresApproval,
        evaluations: vec![test_evaluation(first), test_evaluation(second)],
    };
    let valid_decision = serde_json::to_value(decision).expect("decision JSON");
    assert!(serde_json::from_value::<ApprovalInternalDecision>(valid_decision.clone()).is_ok());
    let mut empty_decision = valid_decision;
    empty_decision["evaluations"] = json!([]);
    assert!(serde_json::from_value::<ApprovalInternalDecision>(empty_decision).is_err());
}

#[test]
fn checkpoint_validation_allows_predictive_trigger_and_requires_shrink() {
    let checkpoint = InternalSummaryCheckpoint::new(
        "summary".into(),
        InternalAgentInvocationId::new_v7(),
        InternalAgentRunId::new_v7(),
        SummaryByteLimit::new(1024).unwrap(),
    )
    .unwrap();
    let mut commit = ContextCheckpointCommit {
        checkpoint: ContextCheckpoint::InternalSummary { checkpoint },
        boundaries: ContextCheckpointBoundaries {
            source_from_seq: 1,
            source_through_seq: 2,
            input_through_seq: 2,
            prior_checkpoint_seq: None,
        },
        budgets: ContextCheckpointBudgets {
            context_limit_tokens: 100,
            trigger_tokens: 70,
            input_tokens_before: 60,
            input_tokens_after: 2,
            max_summary_bytes: SummaryByteLimit::new(1024).unwrap(),
        },
    };

    assert!(commit.validate().is_ok());
    commit.budgets.input_tokens_after = commit.budgets.input_tokens_before;
    assert_eq!(
        commit.validate(),
        Err(EventSchemaError::InvalidCheckpointBoundaries)
    );
}

#[test]
fn native_compaction_is_limited_to_responses_recipes() {
    let mut binding = frozen_binding();
    binding.descriptor.capabilities.compaction = oven_sdk::CompactionCapability::Native;
    assert!(binding.validate().is_ok());

    binding.protocol_recipe = ProtocolRecipeId::new("oven.azure.openai.responses").unwrap();
    assert!(binding.validate().is_ok());

    binding.protocol_recipe = ProtocolRecipeId::new("oven.openai.chat").unwrap();
    assert_eq!(
        binding.validate(),
        Err(ModelSchemaError::NativeCompactionUnsupported)
    );
}

#[test]
fn available_model_descriptor_serde_round_trip_preserves_variant_order() {
    let descriptor = AvailableModelDescriptor {
        key: "test/model".parse().expect("model key"),
        display_name: "Test model".into(),
        capabilities: ModelCapabilities {
            input: BTreeSet::from([Modality::Text]),
            output: BTreeSet::from([Modality::Text]),
            context_tokens: 4096,
            output_tokens: 1024,
            tool_calling: false,
            parallel_tool_calls: false,
            structured_output: false,
            reasoning: true,
            temperature: true,
            top_p: true,
            seed: false,
            native_replay: ReplayCapability::Unsupported,
            cancellation: CancellationCapability::LocalOnly,
            media: BTreeMap::new(),
        },
        variants: ["alpha", "zeta"]
            .into_iter()
            .map(|id| AvailableVariantDescriptor {
                id: VariantId::new(id).expect("variant ID"),
                display_name: id.into(),
                origin: VariantOrigin::Explicit,
                behavior_fingerprint: Sha256Digest::of_bytes(id.as_bytes()),
            })
            .collect(),
        variant_order: ["zeta", "alpha"]
            .into_iter()
            .map(|id| VariantId::new(id).expect("variant ID"))
            .collect(),
        default_variant: None,
        behavior_fingerprint: Sha256Digest::of_bytes(b"model behavior"),
    };

    let encoded = serde_json::to_value(&descriptor).expect("serialize model descriptor");
    let decoded: AvailableModelDescriptor =
        serde_json::from_value(encoded).expect("deserialize model descriptor");
    assert_eq!(decoded, descriptor);
}

#[test]
fn session_meta_serde_round_trip_preserves_last_activity() {
    let runtime = runtime();
    let last_activity = "2026-08-06T12:34:56Z".parse().expect("timestamp");
    let meta = SessionMeta {
        meta_schema_version: SessionMetaSchemaVersion::current(),
        session_id: SessionId::new_v7(),
        origin: SessionOrigin::Root,
        cwd_identity: CwdIdentity::new("test-workspace").expect("cwd identity"),
        creation_selection: RunSelection {
            agent: AgentId::new("primary").expect("agent ID"),
            model: ModelSelection {
                model: "openai/gpt-5.6-sol".parse().expect("model key"),
                variant: None,
            },
        },
        runtime_revision: runtime.runtime_revision,
        catalog_revision: runtime.catalog_revision,
        provider_state_revision: runtime.provider_state_revision,
        model_revision: runtime.model_revision,
        agent_revision: runtime.agent_revision,
        recipe_registry_revision: runtime.recipe_registry_revision,
        manifest_revision: revision("manifest"),
        title: None,
        title_updated_seq: 0,
        last_event_seq: 1,
        last_activity,
        status: SessionStatus::Idle,
    };

    let encoded = serde_json::to_value(&meta).expect("serialize session metadata");
    let decoded: SessionMeta =
        serde_json::from_value(encoded).expect("deserialize session metadata");
    assert_eq!(decoded, meta);
}

#[test]
fn revisions_use_dynamic_sha256_grammar() {
    let a = CatalogRevision::new(format!("sha256:{}", "a".repeat(64))).unwrap();
    let b = CatalogRevision::new(format!("sha256:{}", "b".repeat(64))).unwrap();
    assert_ne!(a, b);
    for invalid in [
        "sha256:abc".to_owned(),
        format!("sha256:{}", "A".repeat(64)),
        "0".repeat(64),
    ] {
        assert!(CatalogRevision::new(invalid).is_err());
    }
    let schema = serde_json::to_value(schema_for!(CatalogRevision)).unwrap();
    assert_eq!(schema["pattern"], "^sha256:[0-9a-f]{64}$");
    assert!(schema.get("const").is_none());
}

fn internal_approval_decision(
    decision: ApprovalInternalDecisionKind,
    source: ApprovalDecisionSource,
    reason_code: ApprovalReasonCode,
) -> ApprovalInternalDecision {
    ApprovalInternalDecision {
        decision,
        source,
        reason_code,
        evaluations: vec![test_evaluation(b"decision")],
    }
}

#[test]
fn internal_agent_escalation_decision_is_coherent() {
    assert!(
        internal_approval_decision(
            ApprovalInternalDecisionKind::Escalate,
            ApprovalDecisionSource::InternalAgent,
            ApprovalReasonCode::Escalated,
        )
        .validate()
        .is_ok()
    );
}

#[test]
fn internal_agent_ask_with_escalated_reason_is_rejected() {
    assert_eq!(
        internal_approval_decision(
            ApprovalInternalDecisionKind::Ask,
            ApprovalDecisionSource::InternalAgent,
            ApprovalReasonCode::Escalated,
        )
        .validate(),
        Err(ApprovalSchemaError::ContradictoryDecision)
    );
}

#[test]
fn policy_ask_decision_is_coherent() {
    assert!(
        internal_approval_decision(
            ApprovalInternalDecisionKind::Ask,
            ApprovalDecisionSource::Policy,
            ApprovalReasonCode::PolicyRequiresApproval,
        )
        .validate()
        .is_ok()
    );
}

#[test]
fn model_ask_decision_is_coherent() {
    assert!(
        internal_approval_decision(
            ApprovalInternalDecisionKind::Ask,
            ApprovalDecisionSource::Model,
            ApprovalReasonCode::ModelRequested,
        )
        .validate()
        .is_ok()
    );
}

#[test]
fn permission_mode_uses_snake_case_wire_values() {
    assert_eq!(
        serde_json::to_value(PermissionMode::AutoApprove).unwrap(),
        json!("auto_approve")
    );
    assert_eq!(
        serde_json::to_value(PermissionMode::Ask).unwrap(),
        json!("ask")
    );
    assert_eq!(
        serde_json::to_value(PermissionMode::Yolo).unwrap(),
        json!("yolo")
    );
    assert_eq!(
        serde_json::from_value::<PermissionMode>(json!("auto_approve")).unwrap(),
        PermissionMode::AutoApprove
    );
}

#[test]
fn yolo_approved_is_coherent_only_for_policy_allow_and_approved() {
    assert!(
        internal_approval_decision(
            ApprovalInternalDecisionKind::Allow,
            ApprovalDecisionSource::Policy,
            ApprovalReasonCode::YoloApproved,
        )
        .validate()
        .is_ok()
    );
    assert_eq!(
        internal_approval_decision(
            ApprovalInternalDecisionKind::Deny,
            ApprovalDecisionSource::Policy,
            ApprovalReasonCode::YoloApproved,
        )
        .validate(),
        Err(ApprovalSchemaError::ContradictoryDecision)
    );
    let approved = ApprovalFinalDecision {
        outcome: ApprovalFinalOutcome::Approved,
        source: ApprovalDecisionSource::Policy,
        reason_code: ApprovalReasonCode::YoloApproved,
        feedback: None,
        tree_grant_id: None,
    };
    assert!(approved.validate().is_ok());
    assert_eq!(
        ApprovalFinalDecision {
            outcome: ApprovalFinalOutcome::Rejected,
            ..approved
        }
        .validate(),
        Err(ApprovalSchemaError::ContradictoryDecision)
    );
}

#[test]
fn runtime_snapshot_is_strict_and_source_coherent() {
    let mut value = serde_json::to_value(runtime()).unwrap();
    value["unknown"] = json!(true);
    assert!(serde_json::from_value::<RuntimeSnapshotV1>(value).is_err());

    let mut bootstrap = runtime();
    bootstrap.catalog_source = CatalogSource::Bootstrap;
    assert!(bootstrap.validate().is_err());
    bootstrap.catalog_state.stale = true;
    assert!(bootstrap.validate().is_ok());
}

#[test]
fn runtime_change_reasons_are_nonempty_sorted_unique() {
    let base = json!({"previous_revision":null,"snapshot":runtime(),"reasons":[]});
    assert!(serde_json::from_value::<RuntimeChangedNotification>(base).is_err());
    let unsorted = json!({"previous_revision":null,"snapshot":runtime(),"reasons":["provider_store_reloaded","provider_store_changed"]});
    assert!(serde_json::from_value::<RuntimeChangedNotification>(unsorted).is_err());
    let valid = json!({"previous_revision":null,"snapshot":runtime(),"reasons":["provider_connected","provider_disconnected","provider_store_changed","provider_store_reloaded"]});
    assert!(serde_json::from_value::<RuntimeChangedNotification>(valid).is_ok());
}

#[test]
fn provider_connect_is_boundary_only_and_redacted() {
    let revision = revision::<CatalogRevision>("catalog");
    let json = format!(
        r#"{{"provider_id":"openai","expected_catalog_revision":"{revision}","setup_values":{{}},"auth_method":"bearer-api-key-v1","auth_values":{{"api_key":"super-secret"}},"client_connect_id":"connect-1"}}"#
    );
    let params: ProviderConnectParams = serde_json::from_str(&json).unwrap();
    assert_eq!(
        params
            .auth_values
            .get(&AuthFieldName::new("api_key").unwrap()),
        Some("super-secret")
    );
    assert!(!format!("{params:?}").contains("super-secret"));

    let duplicate = format!(
        r#"{{"provider_id":"openai","expected_catalog_revision":"{revision}","setup_values":{{}},"auth_method":"bearer-api-key-v1","auth_values":{{"api_key":"one","api_key":"two"}},"client_connect_id":"connect-1"}}"#
    );
    assert!(serde_json::from_str::<ProviderConnectParams>(&duplicate).is_err());
    let unknown = json.replace(
        "\"client_connect_id\"",
        "\"unknown\":true,\"client_connect_id\"",
    );
    assert!(serde_json::from_str::<ProviderConnectParams>(&unknown).is_err());
}

#[test]
fn disconnect_params_and_success_are_exact() {
    let params = ProviderDisconnectParams {
        provider_id: ProviderId::new("openai").unwrap(),
        expected_runtime_revision: revision("runtime"),
        expected_provider_state_revision: revision("providers"),
        expected_connection_generation: None,
        client_request_id: ClientRequestId::new("request-1").unwrap(),
    };
    let mut value = serde_json::to_value(params).unwrap();
    value["extra"] = json!(1);
    assert!(serde_json::from_value::<ProviderDisconnectParams>(value).is_err());

    let invalid_result = json!({
        "durable_receipt": {"receipt_id":"00000000-0000-0000-0000-000000000001","store_revision":revision::<ProviderStoreRevision>("store"),"provider_state_revision":revision::<ProviderStateRevision>("providers"),"committed_at":"2026-08-04T00:00:00Z"},
        "provider_id":"openai","disconnected":false,"effective_auth_state":"unavailable","runtime":{"snapshot":runtime()},"replayed":false
    });
    assert!(serde_json::from_value::<ProviderDisconnectResult>(invalid_result).is_err());
}

#[test]
fn frozen_sources_and_credentials_are_strict() {
    assert!(serde_json::from_value::<FrozenCredentialSource>(json!("provider_store")).is_ok());
    assert!(serde_json::from_value::<FrozenCredentialSource>(json!("ambient_env")).is_err());
    let binding = json!({
        "source":"provider_store",
        "auth_method":"bearer-api-key-v1",
        "fields":["z","a"],
        "parameters":{},
        "owned_headers":["authorization"]
    });
    assert!(serde_json::from_value::<FrozenCredentialBinding>(binding).is_err());
    assert!(
        serde_json::from_value::<FrozenProviderSource>(json!({
            "type":"managed",
            "provider_recipe":"openai.responses.v1",
            "source_record_digest":Sha256Digest::of_bytes(b"source"),
            "package_claim":"@ai-sdk/openai"
        }))
        .is_err()
    );
}

#[test]
fn current_delegation_journal_roundtrips_actual_start_record() {
    let binding = frozen_binding();
    let manifest_revision = binding.manifest_revision.clone();
    let record = StoredDelegationJournalRecord {
        delegation_journal_schema_version: DelegationJournalSchemaVersion::current(),
        record: DelegationJournalRecord::DelegationStartedV4 {
            reservation: DelegationReservation {
                invocation_id: InvocationId::new_v7(),
                parent_session_id: SessionId::new_v7(),
                parent_run_id: RunId::new_v7(),
                parent_tool_call_id: ToolCallId::new_v7(),
                child_session_id: SessionId::new_v7(),
            },
            child_agent: Box::new(child_agent(binding.clone())),
            manifest_revision,
            runtime_revision: revision("runtime"),
            catalog_revision: revision("catalog"),
            provider_state_revision: revision("providers"),
            model_revision: revision("models"),
            agent_revision: revision("agents"),
            recipe_registry_revision: revision("recipes"),
            selected_suffix: vec![binding],
            request_fingerprint: Sha256Digest::of_bytes(b"request"),
            prompt: "Review the implementation".into(),
            request: DelegateRequestPayloadV4 {
                description: "Review implementation".into(),
                prompt: "Review the implementation".into(),
                title: SessionTitle::new("Review implementation").unwrap(),
                resume_session_id: None,
                inherit_context: false,
                seeded_context: Vec::new(),
                background: true,
            },
        },
    };
    let encoded = serde_json::to_value(&record).unwrap();
    assert_eq!(
        serde_json::from_value::<StoredDelegationJournalRecord>(encoded.clone()).unwrap(),
        record
    );

    for field in [
        "manifest_revision",
        "runtime_revision",
        "catalog_revision",
        "provider_state_revision",
        "model_revision",
        "agent_revision",
        "recipe_registry_revision",
    ] {
        let mut missing = encoded.clone();
        missing["record"].as_object_mut().unwrap().remove(field);
        assert!(serde_json::from_value::<StoredDelegationJournalRecord>(missing).is_err());
    }
    let mut unknown = encoded;
    unknown["record"]["legacy_revision"] = json!(revision::<RuntimeRevision>("legacy"));
    assert!(serde_json::from_value::<StoredDelegationJournalRecord>(unknown).is_err());
}

#[test]
fn delegation_v4_is_additive_to_all_exported_request_shapes() {
    let legacy = json!({
        "task":"Review the implementation",
        "context":[],
        "success_criteria":["Report blockers"],
        "expected_output":{"type":"report"}
    });
    let decoded: DelegateRequestPayload = serde_json::from_value(legacy.clone()).unwrap();
    assert_eq!(serde_json::to_value(decoded).unwrap(), legacy);

    let v2 = json!({
        "description":"Review implementation",
        "prompt":"Review the implementation",
        "title":"Review implementation"
    });
    let decoded: DelegateRequestPayloadV2 = serde_json::from_value(v2.clone()).unwrap();
    assert_eq!(serde_json::to_value(decoded).unwrap(), v2);

    let v3 = json!({
        "description":"Review implementation",
        "prompt":"Review the implementation",
        "title":"Review implementation",
        "resume_session_id":null,
        "inherit_context":true,
        "seeded_context":[{"role":"user","text":"Recent parent context"}]
    });
    let decoded: DelegateRequestPayloadV3 = serde_json::from_value(v3.clone()).unwrap();
    assert_eq!(serde_json::to_value(decoded).unwrap(), v3);

    let mut v4 = v3;
    v4["background"] = json!(true);
    let decoded: DelegateRequestPayloadV4 = serde_json::from_value(v4.clone()).unwrap();
    assert_eq!(serde_json::to_value(decoded).unwrap(), v4);

    let schema = serde_json::to_string(&json_schema_documents()).unwrap();
    assert!(schema.contains("delegation_started"));
    assert!(schema.contains("delegation_started_v2"));
    assert!(schema.contains("delegation_started_v3"));
    assert!(schema.contains("delegation_started_v4"));
}

#[test]
fn open_responses_is_not_a_current_protocol_surface() {
    assert!(serde_json::from_value::<AdaptorId>(json!("open-responses")).is_err());
    assert!(
        serde_json::from_value::<ProviderOptions>(
            json!({"type":"open-responses","protocol_mode":"standard"})
        )
        .is_err()
    );
    let schemas = serde_json::to_string(&json_schema_documents()).unwrap();
    assert!(!schemas.contains("open-responses"));
    assert!(!schemas.contains("OpenResponses"));
}

#[test]
fn manifest_decimals_are_normalized_strings_only() {
    for value in [0.0_f32, 0.7, 0.125, 1.25, 2.0] {
        let decimal = NormalizedDecimal::from_f32(value).unwrap();
        let encoded = serde_json::to_value(&decimal).unwrap();
        assert_eq!(encoded, json!(value.to_string()));
        assert_eq!(
            serde_json::from_value::<NormalizedDecimal>(encoded).unwrap(),
            decimal
        );
    }
    for invalid in [json!(0.7), json!("0.70"), json!("7e-1"), json!("NaN")] {
        assert!(serde_json::from_value::<NormalizedDecimal>(invalid).is_err());
    }
    let defaults = FrozenResolvedRequestDefaults {
        request: FrozenRequestDefaults {
            temperature: Some(NormalizedDecimal::from_f32(0.7).unwrap()),
            top_p: Some(NormalizedDecimal::from_f32(0.125).unwrap()),
            max_output_tokens: Some(128),
            stop: Vec::new(),
            seed: None,
            tool_choice: None,
        },
        reasoning: None,
    };
    let encoded = serde_json::to_string(&defaults).unwrap();
    assert!(encoded.contains("\"temperature\":\"0.7\""));
    assert!(encoded.contains("\"top_p\":\"0.125\""));
}

#[test]
fn generated_roots_are_current_only_and_secret_free() {
    let names = json_schema_documents()
        .into_iter()
        .map(|(name, _)| name)
        .collect::<Vec<_>>();
    for removed in [
        "CatalogProviderListParams.schema.json",
        "CatalogProviderListResult.schema.json",
        "CatalogModelListParams.schema.json",
        "CatalogModelListResult.schema.json",
        "ModelListParams.schema.json",
        "ModelListResult.schema.json",
        "AgentListParams.schema.json",
        "AgentListResult.schema.json",
        "ProviderConnectParams.schema.json",
        "McpApprovalListParams.schema.json",
        "McpApprovalListResult.schema.json",
        "McpApprovalRespondParams.schema.json",
        "McpApprovalRespondResult.schema.json",
    ] {
        assert!(
            !names.contains(&removed),
            "obsolete or secret-bearing root {removed}"
        );
    }
    for current in [
        "RuntimeSnapshotResult.schema.json",
        "RuntimeChangedNotification.schema.json",
        "ProviderDisconnectParams.schema.json",
        "ProviderDisconnectResult.schema.json",
        "ModelSnapshotManifestV1.schema.json",
        "StoredDelegationJournalRecord.schema.json",
    ] {
        assert!(names.contains(&current), "missing current root {current}");
    }
    let encoded = serde_json::to_string(&json_schema_documents()).unwrap();
    assert!(!encoded.contains("auth_values"));
    assert!(!encoded.contains("super-secret"));
}

#[test]
fn setup_bounds_and_shared_identity_types_are_enforced() {
    assert!(BoundedSetupString::new("x".repeat(2048)).is_ok());
    assert!(BoundedSetupString::new("x".repeat(2049)).is_err());
    assert!(SetupFieldId::new("resource_name").is_ok());
    assert!(SetupFieldId::new("RESOURCE").is_err());
    assert!(ProviderStoreGeneration::new(0).is_err());
    assert!(ProviderStoreGeneration::new(9_007_199_254_740_992).is_err());
    let mut values = BTreeMap::new();
    values.insert(
        SetupFieldId::new("region").unwrap(),
        SafeSetupValue::String(BoundedSetupString::new("us-east-1").unwrap()),
    );
    assert_eq!(values.len(), 1);
}

#[test]
fn runtime_snapshot_schema_snapshot() {
    insta::assert_json_snapshot!(schema_for!(RuntimeSnapshotResult));
}

#[test]
fn runtime_snapshot_typescript_snapshot() {
    insta::assert_snapshot!(RuntimeSnapshotResult::export_to_string(&typescript_config()).unwrap());
}
