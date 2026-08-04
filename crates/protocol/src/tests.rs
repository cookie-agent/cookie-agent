use std::collections::{BTreeMap, BTreeSet};

use insta::{assert_json_snapshot, assert_snapshot};
use jiff::Timestamp;
use schemars::schema_for;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use ts_rs::TS;
use uuid::Uuid;

use super::*;

fn round_trip<T>(value: T)
where
    T: fmt::Debug + PartialEq + Serialize + for<'de> Deserialize<'de>,
{
    let json = serde_json::to_string(&value).expect("serialize");
    let decoded = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(value, decoded);
}
use std::fmt;

fn session_id() -> SessionId {
    SessionId(Uuid::from_u128(1))
}
fn run_id() -> RunId {
    RunId(Uuid::from_u128(2))
}
fn attempt_id() -> AttemptId {
    AttemptId(Uuid::from_u128(3))
}
fn call_id() -> ToolCallId {
    ToolCallId(Uuid::from_u128(4))
}
fn approval_id() -> ApprovalId {
    ApprovalId(Uuid::from_u128(5))
}
fn digest(value: &str) -> Sha256Digest {
    Sha256Digest::of_bytes(value.as_bytes())
}
fn agent_id(value: &str) -> AgentId {
    AgentId::new(value).expect("agent")
}
fn provider_id(value: &str) -> ProviderId {
    ProviderId::new(value).expect("provider")
}
fn model_id(value: &str) -> ProviderModelId {
    ProviderModelId::new(value).expect("model")
}
fn variant_id(value: &str) -> VariantId {
    VariantId::new(value).expect("variant")
}
fn model_key(value: &str) -> ModelKey {
    value.parse().expect("key")
}
fn selection(model: &str, variant: Option<&str>) -> ModelSelection {
    ModelSelection {
        model: model_key(model),
        variant: variant.map(variant_id),
    }
}
fn safe(value: &str) -> SafeDisplayText {
    SafeDisplayText::new(value).expect("safe text")
}
fn error_text(value: &str) -> SafeErrorMessage {
    SafeErrorMessage::new(value).expect("safe error")
}
fn code(value: &str) -> SafeCode {
    SafeCode::new(value).expect("safe code")
}
fn revision(value: &str) -> SnapshotRevision {
    SnapshotRevision::new(format!("sha256:{}", digest(value).as_str())).expect("revision")
}
fn catalog_revision() -> CatalogRevision {
    CatalogRevision::current()
}
fn credential_field(value: &str) -> CredentialFieldName {
    CredentialFieldName::new(value).expect("credential field")
}

fn capabilities() -> ModelCapabilities {
    ModelCapabilities {
        input: BTreeSet::from([Modality::Text]),
        output: BTreeSet::from([Modality::Text]),
        context_tokens: 128_000,
        output_tokens: 16_000,
        tool_calling: true,
        parallel_tool_calls: true,
        structured_output: true,
        reasoning: true,
        temperature: true,
        top_p: true,
        seed: true,
        native_replay: ReplayCapability::Optional,
        native_compaction: CompactionCapability::Optional,
        cancellation: CancellationCapability::Provider,
        media: BTreeMap::new(),
    }
}

fn resolved_for(model: &str, variant: Option<&str>) -> ResolvedModelRef {
    let selection = selection(model, variant);
    ResolvedModelRef {
        provider_id: selection.model.provider_id(),
        model_id: selection.model.model_id(),
        selection,
        adapter_id: AdaptorId::OpenaiResponses,
        selection_fingerprint: digest(&format!("selection:{model}:{variant:?}")),
    }
}

fn binding_for(model: &str, variant: Option<&str>) -> FrozenModelBinding {
    let mut resolved = resolved_for(model, variant);
    let descriptor = oven_sdk::LanguageModelDescriptor::new(
        oven_sdk::ModelIdentity::new(
            oven_sdk::ProviderId::new(resolved.provider_id.as_str()),
            oven_sdk::ModelId::new(resolved.model_id.as_str()),
        )
        .unwrap(),
        oven_sdk::AdapterId::new(resolved.adapter_id.as_str()),
        oven_sdk::ModelCapabilities::conservative(),
    )
    .unwrap();
    let behavior_fingerprint = digest("behavior");
    resolved.selection_fingerprint = FrozenModelBinding::expected_selection_fingerprint(
        &resolved.selection,
        resolved.adapter_id,
        &descriptor,
        &behavior_fingerprint,
    )
    .unwrap();
    FrozenModelBinding {
        descriptor,
        resolved,
        defaults: ResolvedRequestDefaults {
            request: RequestDefaults {
                temperature: Some(FiniteF32::new(0.5).unwrap()),
                top_p: Some(FiniteF32::new(0.9).unwrap()),
                max_output_tokens: Some(1024),
                stop: vec!["STOP".into()],
                seed: Some(7),
                tool_choice: Some(ToolChoice::Auto),
            },
            reasoning: Some(CompiledReasoningBehavior::Effort {
                value: ReasoningEffort::High,
            }),
        },
        provider_options: ProviderOptions::OpenAiResponses {
            organization: Some("org".into()),
            project: Some("project".into()),
            store: Some(false),
        },
        behavior_fingerprint,
    }
}

fn agent_snapshot() -> AgentSnapshot {
    AgentSnapshot {
        agent: agent_id("primary"),
        schema: AgentSchemaVersion::current(),
        mode: AgentMode::Primary,
        description: "Primary implementation agent".into(),
        document_source: AgentDocumentSource::Workspace,
        document_fingerprint: digest("document"),
        composed_prompt: "You are the implementation agent.\n".into(),
        prompt_fingerprint: digest("prompt"),
        tools: vec![ToolName::Read, ToolName::Write],
        permissions: vec![PermissionRule {
            id: code("allow-read"),
            action: PermissionAction::Read,
            resource: WildcardPattern::new("*").unwrap(),
            effect: PermissionEffect::Allow,
        }],
        delegation: Some(FrozenDelegationPolicy {
            targets: vec![agent_id("worker")],
            max_depth: 3,
            effective_depth_ceiling: 3,
        }),
        fallback_chain: vec![
            binding_for("openai/gpt-5.6-sol", Some("high")),
            binding_for("anthropic/claude-sonnet-4-6", None),
        ],
        selected_suffix_start: 0,
    }
}

fn run_selection() -> RunSelection {
    RunSelection {
        agent: agent_id("primary"),
        model: selection("openai/gpt-5.6-sol", Some("high")),
    }
}

fn owner() -> AssistantToolCallRef {
    AssistantToolCallRef {
        model_turn_seq: 1,
        content_index: 1,
        model_call_id: ModelCallId::new("model-call-1").unwrap(),
        provider_item_id: Some(ProviderItemId::new("provider-item-1").unwrap()),
    }
}

fn artifact() -> ArtifactReference {
    ArtifactReference {
        uri: format!("artifact://sha256/{}", "00".repeat(32)),
    }
}
fn tool_result() -> PersistedToolResult {
    PersistedToolResult {
        title: safe("Read README.md"),
        output: "contents".into(),
        metadata: json!({"path":"README.md"}),
        truncation: None,
        attachments: vec![ToolAttachment {
            mime_type: MimeType::new("image/png").unwrap(),
            filename: Some("image.png".into()),
            byte_length: 8,
            sha256: digest("image"),
            reference: artifact(),
        }],
    }
}

fn replay_scope() -> NativeContextScope {
    NativeContextScope {
        provider_id: provider_id("openai"),
        model_id: model_id("gpt-5.6-sol"),
        resource_id: safe("responses-state"),
    }
}
fn native_replay() -> NativeReplayArtifact {
    NativeReplayArtifact::new(
        code("openai-responses"),
        resolved_for("openai/gpt-5.6-sol", Some("high")).selection_fingerprint,
        replay_scope(),
        json!({"state":[1,2,3]}),
    )
    .unwrap()
}
fn persisted_turn() -> PersistedModelTurn {
    PersistedModelTurn {
        content: vec![
            PersistedAssistantPart::Text {
                text: "answer".into(),
                metadata: None,
            },
            PersistedAssistantPart::ToolCall {
                id: ModelCallId::new("model-call-1").unwrap(),
                provider_item_id: Some(ProviderItemId::new("provider-item-1").unwrap()),
                name: code("read"),
                input: json!({"filePath":"README.md"}),
                raw_input: Some("{\"filePath\":\"README.md\"}".into()),
                metadata: None,
            },
        ],
        provider_options: BTreeMap::new(),
        finish_reason: ModelFinishReason::ToolCalls,
        usage: Usage {
            input_tokens: Some(10),
            output_tokens: Some(5),
            ..Usage::default()
        },
        response_metadata: BTreeMap::new(),
        provider_metadata: BTreeMap::new(),
        native_replay: Some(native_replay()),
    }
}

fn model_error() -> ModelErrorSummary {
    ModelErrorSummary {
        kind: ModelErrorKind::RateLimited,
        message: error_text("rate limited"),
        retryable: true,
        stage: ModelErrorStage::ResponseHeaders,
        http_status: Some(429),
        bytes_received: 0,
        vendor_code: Some(code("rate_limit")),
        request_id: Some(safe("request-1")),
        retry_after_ms: Some(100),
    }
}

fn prepared_operation() -> PreparedOperationIdentity {
    PreparedOperationIdentity::new(
        digest("args"),
        vec![ApprovalCapability {
            action: PermissionAction::Bash,
            operation: PreparedResourceIdentity::new("execute").unwrap(),
        }],
        vec![PreparedApprovalResource {
            capability: PermissionAction::Bash,
            canonical: PreparedResourceIdentity::new("command:git-status").unwrap(),
            binding_digest: PreparedResourceDigest::from_canonical_binding_bytes(b"binding"),
            binding_lifetime: PreparedBindingLifetime::ProcessLocal,
            boundary: ApprovalBoundary::CommandPrefix {
                prefix: "git status".into(),
            },
            source: ApprovalResourceSource::PrimaryOperation,
        }],
        digest("context"),
    )
    .unwrap()
}
fn trace() -> DecisionTrace {
    DecisionTrace {
        action: PermissionAction::Bash,
        normalized_resource: "git status".into(),
        candidates: vec![MatchedPermissionRule {
            rule_id: Some(code("allow-status")),
            source_layer: code("agent"),
            effect: PermissionEffect::Ask,
        }],
        effect: PermissionEffect::Ask,
        precedence_reason: "last matching rule".into(),
    }
}
fn approval_request() -> ApprovalRequest {
    let operation = prepared_operation();
    let resource_digest = operation.resources()[0].binding_digest.clone();
    ApprovalRequest::new(
        approval_id(),
        1,
        ApprovalTrigger::PermissionPolicy,
        operation,
        vec![ApprovalEvaluation {
            resource_digest,
            effect: PermissionEffect::Ask,
            trace: trace(),
        }],
        ApprovalConstraints {
            allow_once: true,
            allow_tree_grant: false,
            cancellable: true,
            expires_at: None,
        },
    )
    .unwrap()
}

fn meta() -> SessionMeta {
    SessionMeta {
        meta_schema_version: SessionMetaSchemaVersion::current(),
        session_id: session_id(),
        origin: SessionOrigin::Root,
        cwd_identity: CwdIdentity::new("workspace:0123456789").unwrap(),
        creation_selection: run_selection(),
        title: Some(SessionTitle::new("Protocol v7").unwrap()),
        title_updated_seq: 2,
        last_event_seq: 3,
        status: SessionStatus::Idle,
    }
}

fn stored(payload: EventPayload, seq: u64, run_scoped: bool) -> StoredEvent {
    StoredEvent {
        event_schema_version: EventSchemaVersion::current(),
        session_id: session_id(),
        run_id: run_scoped.then(run_id),
        seq,
        timestamp: Timestamp::now(),
        payload,
    }
}

#[test]
fn exact_versions_are_seven_and_six_is_strictly_rejected() {
    assert_eq!(PROTOCOL_VERSION, 7);
    assert_eq!(EVENT_SCHEMA_VERSION, 7);
    assert_eq!(SESSION_META_SCHEMA_VERSION, 7);
    assert_eq!(DELEGATION_JOURNAL_SCHEMA_VERSION, 7);
    assert!(supports_protocol_version(7));
    assert!(supports_event_schema_version(7));
    assert!(supports_session_meta_schema_version(7));
    assert!(supports_delegation_journal_schema_version(7));
    assert!(!supports_protocol_version(6));
    assert!(!supports_event_schema_version(6));
    assert!(!supports_session_meta_schema_version(6));
    assert!(!supports_delegation_journal_schema_version(6));
    assert!(serde_json::from_value::<ProtocolVersion>(json!(6)).is_err());
    assert!(serde_json::from_value::<EventSchemaVersion>(json!(6)).is_err());
    assert!(serde_json::from_value::<SessionMetaSchemaVersion>(json!(6)).is_err());
    for wire in [json!({"protocol_version":6}), json!({"protocol_version":8})] {
        assert!(serde_json::from_value::<ClientHello>(wire.clone()).is_err());
        assert!(serde_json::from_value::<ServerHello>(wire).is_err());
    }
    assert!(serde_json::from_value::<StoredEvent>(json!({"event_schema_version":6,"session_id":session_id(),"run_id":null,"seq":1,"timestamp":Timestamp::now(),"payload":{"type":"session_created"}})).is_err());
    assert!(serde_json::from_value::<SessionMeta>(json!({"meta_schema_version":6,"session_id":session_id(),"origin":{"type":"root"},"cwd_identity":"workspace","creation_selection":run_selection(),"title":null,"title_updated_seq":0,"last_event_seq":1,"status":"idle"})).is_err());
    assert!(serde_json::from_value::<DelegationJournalSchemaVersion>(json!(6)).is_err());
    assert_eq!(ProtocolVersion::inline(&typescript_config()), "7");
    assert_eq!(EventSchemaVersion::inline(&typescript_config()), "7");
}

#[test]
fn shared_identity_wire_contract_is_exact_and_bounded() {
    for valid in ["worker", "worker-2", "2worker"] {
        let protocol = AgentId::new(valid).unwrap();
        let shared = cookie_agent_identity::AgentId::new(valid).unwrap();
        assert_eq!(
            serde_json::to_value(protocol).unwrap(),
            serde_json::to_value(shared).unwrap()
        );
    }
    for invalid in ["Worker", "worker--two", "worker-", ""] {
        assert!(
            AgentId::new(invalid).is_err(),
            "protocol accepted {invalid:?}"
        );
        assert!(
            cookie_agent_identity::AgentId::new(invalid).is_err(),
            "shared identity accepted {invalid:?}"
        );
    }
    assert!(AgentId::new(format!("a{}", "b".repeat(63))).is_ok());
    assert!(AgentId::new(format!("a{}", "b".repeat(64))).is_err());
    assert!(ProviderId::new("a".repeat(128)).is_ok());
    assert!(ProviderId::new("a".repeat(129)).is_err());
    assert!(ProviderModelId::new("m".repeat(384)).is_ok());
    assert!(ProviderModelId::new("m".repeat(385)).is_err());
    assert!(ProviderModelId::new(" model").is_err());
    assert!(ProviderModelId::new("model/child").is_err());
    assert!(VariantId::new("v".repeat(64)).is_ok());
    assert!(VariantId::new("v".repeat(65)).is_err());
    assert!(VariantId::new("base").is_err());
    let max_key = format!("{}/{}", "p".repeat(128), "m".repeat(383));
    assert_eq!(max_key.len(), 512);
    assert!(max_key.parse::<ModelKey>().is_ok());
    assert!(
        format!("{}/{}", "p".repeat(128), "m".repeat(384))
            .parse::<ModelKey>()
            .is_err()
    );
    round_trip(selection("openai/gpt-5.6-sol", None));
    round_trip(selection("openai/gpt-5.6-sol", Some("default")));
}

#[test]
fn model_variant_selection_and_descriptor_schemas_are_strict() {
    round_trip(binding_for("openai/gpt-5.6-sol", Some("high")));
    let descriptor = AvailableModelDescriptor {
        key: model_key("openai/gpt-5.6-sol"),
        display_name: "GPT 5.6 Sol".into(),
        capabilities: capabilities(),
        variants: vec![AvailableVariantDescriptor {
            id: variant_id("high"),
            display_name: "High".into(),
            origin: VariantOrigin::Explicit,
            behavior_fingerprint: digest("high"),
        }],
        default_variant: Some(variant_id("high")),
        behavior_fingerprint: digest("base"),
    };
    round_trip(descriptor.clone());
    let mut unknown = serde_json::to_value(descriptor).unwrap();
    unknown
        .as_object_mut()
        .unwrap()
        .insert("alias".into(), json!("fast"));
    assert!(serde_json::from_value::<AvailableModelDescriptor>(unknown).is_err());
    let mut mismatch = serde_json::to_value(binding_for("openai/gpt-5.6-sol", None)).unwrap();
    mismatch["resolved"]["provider_id"] = json!("anthropic");
    assert!(serde_json::from_value::<FrozenModelBinding>(mismatch).is_err());
    let invalid_caps = json!({"input":["text"],"output":["text"],"context_tokens":100,"output_tokens":101,"tool_calling":false,"parallel_tool_calls":true,"structured_output":false,"reasoning":false,"temperature":false,"top_p":false,"seed":false,"native_replay":"unsupported","native_compaction":"unsupported","cancellation":"local_only","media":{}});
    assert!(serde_json::from_value::<ModelCapabilities>(invalid_caps).is_err());
    assert!(
        serde_json::from_value::<RequestDefaults>(json!({"temperature":2.1,"stop":[]})).is_err()
    );
    assert!(serde_json::from_value::<RequestDefaults>(json!({"top_p":1.1,"stop":[]})).is_err());
    assert!(serde_json::from_value::<RequestDefaults>(json!({"stop":vec!["x";9]})).is_err());
}

#[test]
fn agent_snapshot_selection_and_lists_enforce_ownership_and_order() {
    round_trip(agent_snapshot());
    round_trip(run_selection());
    let snapshot = agent_snapshot();
    let mut duplicate = serde_json::to_value(&snapshot).unwrap();
    duplicate["fallback_chain"] = json!([snapshot.fallback_chain[0], snapshot.fallback_chain[0]]);
    assert!(serde_json::from_value::<AgentSnapshot>(duplicate).is_err());
    let mut invalid_suffix = serde_json::to_value(&snapshot).unwrap();
    invalid_suffix["selected_suffix_start"] = json!(2);
    assert!(serde_json::from_value::<AgentSnapshot>(invalid_suffix).is_err());
    let descriptor = AgentDescriptor {
        id: agent_id("primary"),
        description: "Primary".into(),
        mode: AgentMode::Primary,
        enabled: true,
        runnable_as_root: true,
        resolved_fallback: vec![selection("openai/gpt-5.6-sol", Some("high"))],
        tools: vec![ToolName::Read],
        delegation_targets: vec![agent_id("worker")],
    };
    round_trip(descriptor.clone());
    let agents = AgentListResult {
        revision: revision("agents"),
        model_revision: revision("models"),
        generated_at: Timestamp::now(),
        agents: vec![descriptor],
    };
    round_trip(agents);
    let unsorted = json!({"revision":revision("agents"),"model_revision":revision("models"),"generated_at":Timestamp::now(),"agents":[{"id":"z-agent","description":"Z","mode":"primary","enabled":true,"runnable_as_root":true,"resolved_fallback":[{"model":"openai/gpt-5.6-sol","variant":null}],"tools":[],"delegation_targets":[]},{"id":"a-agent","description":"A","mode":"primary","enabled":true,"runnable_as_root":true,"resolved_fallback":[{"model":"openai/gpt-5.6-sol","variant":null}],"tools":[],"delegation_targets":[]}]});
    assert!(serde_json::from_value::<AgentListResult>(unsorted).is_err());
}

#[test]
fn session_and_run_wire_has_selection_and_no_profile_alias_or_fork() {
    round_trip(SessionCreateParams {
        selection: run_selection(),
    });
    round_trip(RunStartParams {
        session_id: session_id(),
        client_run_id: ClientRunId::new("run-1").unwrap(),
        selection: run_selection(),
        input: "implement".into(),
    });
    round_trip(meta());
    assert!(
        serde_json::from_value::<SessionCreateParams>(
            json!({"cwd":"/workspace","profile":"primary"})
        )
        .is_err()
    );
    assert!(serde_json::from_value::<RunStartParams>(json!({"session_id":session_id(),"client_run_id":"run-1","input":"x","profile":"reviewer"})).is_err());
    assert!(
        serde_json::from_value::<SessionOrigin>(
            json!({"type":"forked","source_session_id":session_id(),"source_event_seq":1})
        )
        .is_err()
    );
    let mut missing_title = serde_json::to_value(meta()).unwrap();
    missing_title.as_object_mut().unwrap().remove("title");
    assert!(serde_json::from_value::<SessionMeta>(missing_title).is_err());
    let mut stale_title = serde_json::to_value(meta()).unwrap();
    stale_title["title_updated_seq"] = json!(4);
    assert!(serde_json::from_value::<SessionMeta>(stale_title).is_err());
}

#[test]
fn required_attempt_replay_turn_and_fallback_events_round_trip() {
    let events = vec![
        stored(
            EventPayload::ModelAttemptStarted {
                attempt_id: attempt_id(),
                attempt_ordinal: 1,
                fallback_index: 0,
                retry_ordinal: 0,
                resolved_model: resolved_for("openai/gpt-5.6-sol", Some("high")),
                prompt_fingerprint: digest("prompt"),
            },
            2,
            true,
        ),
        stored(
            EventPayload::TextDelta {
                attempt_id: attempt_id(),
                text: "hello".into(),
            },
            3,
            true,
        ),
        stored(
            EventPayload::ReasoningDelta {
                attempt_id: attempt_id(),
                text: "thinking".into(),
            },
            4,
            true,
        ),
        stored(
            EventPayload::ModelReplayEvaluated {
                attempt_id: attempt_id(),
                resolved_model: resolved_for("openai/gpt-5.6-sol", Some("high")),
                ordered_decisions: vec![ReplayDecision {
                    history_index: 0,
                    disposition: ReplayDisposition::DiscardedForeignVariant {
                        found: None,
                        expected: Some(variant_id("high")),
                    },
                }],
            },
            5,
            true,
        ),
        stored(
            EventPayload::ModelTurnCommitted {
                attempt_id: attempt_id(),
                model_turn_seq: 1,
                resolved_model: resolved_for("openai/gpt-5.6-sol", Some("high")),
                input_through_seq: 1,
                turn: persisted_turn(),
                warnings: vec![error_text("safe warning")],
            },
            6,
            true,
        ),
        stored(
            EventPayload::ModelFallback {
                from: resolved_for("openai/gpt-5.6-sol", Some("high")),
                to: resolved_for("anthropic/claude-sonnet-4-6", None),
                from_fallback_index: 0,
                to_fallback_index: 1,
                attempts_on_from: 2,
                error: model_error(),
            },
            7,
            true,
        ),
    ];
    for event in events {
        round_trip(event);
    }
    let no_attempt = json!({"event_schema_version":7,"session_id":session_id(),"run_id":run_id(),"seq":2,"timestamp":Timestamp::now(),"payload":{"type":"text_delta","text":"legacy"}});
    assert!(serde_json::from_value::<StoredEvent>(no_attempt).is_err());
    let backwards = json!({"event_schema_version":7,"session_id":session_id(),"run_id":run_id(),"seq":7,"timestamp":Timestamp::now(),"payload":{"type":"model_fallback","from":resolved_for("openai/gpt-5.6-sol",None),"to":resolved_for("anthropic/claude-sonnet-4-6",None),"from_fallback_index":1,"to_fallback_index":0,"attempts_on_from":1,"error":model_error()}});
    assert!(serde_json::from_value::<StoredEvent>(backwards).is_err());
}

#[test]
fn session_created_and_run_started_are_authoritative_and_strict() {
    round_trip(stored(
        EventPayload::SessionCreated {
            origin: SessionOrigin::Root,
            cwd_identity: CwdIdentity::new("workspace:0123").unwrap(),
            creation_selection: run_selection(),
            creation_agent: Box::new(agent_snapshot()),
            model_snapshot_fingerprint: digest("models"),
        },
        1,
        false,
    ));
    round_trip(stored(
        EventPayload::RunStarted {
            client_run_id: ClientRunId::new("run-1").unwrap(),
            selection: run_selection(),
            agent: Box::new(agent_snapshot()),
            selected_suffix: agent_snapshot().fallback_chain,
            input_through_seq: 1,
        },
        2,
        true,
    ));
    let invalid = stored(
        EventPayload::SessionCreated {
            origin: SessionOrigin::Root,
            cwd_identity: CwdIdentity::new("workspace:0123").unwrap(),
            creation_selection: run_selection(),
            creation_agent: Box::new(agent_snapshot()),
            model_snapshot_fingerprint: digest("models"),
        },
        2,
        false,
    );
    assert!(serde_json::from_value::<StoredEvent>(serde_json::to_value(invalid).unwrap()).is_err());
    let missing_run = stored(
        EventPayload::ModelAttemptStarted {
            attempt_id: attempt_id(),
            attempt_ordinal: 1,
            fallback_index: 0,
            retry_ordinal: 0,
            resolved_model: resolved_for("openai/gpt-5.6-sol", Some("high")),
            prompt_fingerprint: digest("prompt"),
        },
        2,
        false,
    );
    assert!(
        serde_json::from_value::<StoredEvent>(serde_json::to_value(missing_run).unwrap()).is_err()
    );
}

#[test]
fn tool_owner_start_and_single_termination_shape_are_exact() {
    let start = ToolCallStart {
        tool_call_id: call_id(),
        owner: owner(),
        presentation: ToolCallPresentation {
            title: safe("read README.md"),
            primary_argument: Some(safe("README.md")),
        },
        operation_fingerprint: OperationFingerprint::from_prepared_operation(&prepared_operation()),
    };
    let started = EventPayload::ToolCallStarted {
        start: start.clone(),
    };
    let wire = serde_json::to_value(&started).unwrap();
    assert_eq!(wire["type"], "tool_call_started");
    assert!(wire.get("tool").is_none());
    assert!(wire.get("arguments").is_none());
    assert!(wire.get("model_call_id").is_none());
    round_trip(stored(started, 7, true));
    let terminated = ToolCallTermination {
        tool_call_id: call_id(),
        owner: owner(),
        outcome: ToolTerminationOutcome::Completed,
        result: Some(tool_result()),
        error: None,
    };
    assert!(terminated.matches_start(&start));
    let different = ToolCallStart {
        tool_call_id: ToolCallId(Uuid::from_u128(44)),
        ..start
    };
    assert!(!terminated.matches_start(&different));
    round_trip(stored(
        EventPayload::ToolCallTerminated {
            termination: terminated,
        },
        8,
        true,
    ));
    for invalid in [
        json!({"tool_call_id":call_id(),"owner":owner(),"outcome":"completed","result":null,"error":null}),
        json!({"tool_call_id":call_id(),"owner":owner(),"outcome":"failed","result":null,"error":null}),
        json!({"tool_call_id":call_id(),"owner":owner(),"outcome":"completed","result":tool_result(),"error":{"code":"failed","message":"bad"}}),
    ] {
        assert!(serde_json::from_value::<ToolCallTermination>(invalid).is_err());
    }
    assert!(serde_json::from_value::<EventPayload>(json!({"type":"tool_call_started","tool_call_id":call_id(),"model_call_id":"old","tool":"read","arguments":{}})).is_err());
    assert!(serde_json::from_value::<EventPayload>(json!({"type":"tool_call_failed","tool_call_id":call_id(),"code":"execution_failed","message":"old"})).is_err());
    assert!(
        serde_json::from_value::<EventPayload>(
            json!({"type":"tool_call_completed","tool_call_id":call_id(),"result":tool_result()})
        )
        .is_err()
    );
}

#[test]
fn native_replay_and_context_artifacts_require_selection_fingerprints_and_bounds() {
    round_trip(native_replay());
    let wire = serde_json::to_value(native_replay()).unwrap();
    assert!(wire.get("selection_fingerprint").is_some());
    let mut missing = wire.clone();
    missing
        .as_object_mut()
        .unwrap()
        .remove("selection_fingerprint");
    assert!(serde_json::from_value::<NativeReplayArtifact>(missing).is_err());
    let boundary = NativeReplayArtifact::new(
        code("openai-responses"),
        digest("selection"),
        replay_scope(),
        Value::String("x".repeat(NativeReplayArtifact::MAX_PAYLOAD_BYTES - 2)),
    )
    .unwrap();
    assert_eq!(
        serde_json::to_vec(boundary.payload()).unwrap().len(),
        NativeReplayArtifact::MAX_PAYLOAD_BYTES
    );
    assert!(
        NativeReplayArtifact::new(
            code("openai-responses"),
            digest("selection"),
            replay_scope(),
            Value::String("x".repeat(NativeReplayArtifact::MAX_PAYLOAD_BYTES - 1))
        )
        .is_err()
    );
    let native = NativeContextArtifact {
        adapter_id: code("openai-responses"),
        selection_fingerprint: resolved_for("openai/gpt-5.6-sol", Some("high"))
            .selection_fingerprint,
        scope: replay_scope(),
        byte_length: 128,
        sha256: digest("context"),
        reference: artifact(),
    };
    round_trip(native);
}

#[test]
fn approval_connect_compaction_and_title_semantics_are_preserved() {
    round_trip(approval_request());
    let respond = ApprovalRespondParams {
        session_id: session_id(),
        approval_id: approval_id(),
        request_revision: 1,
        operation_fingerprint: approval_request().operation_fingerprint().clone(),
        client_response_id: ClientResponseId::new("response-1").unwrap(),
        decision: ApprovalUserDecision::ApproveOnce,
        feedback: None,
    };
    round_trip(respond);
    let secret = "secret-sentinel";
    let connect = ProviderConnectParams {
        client_connect_id: ClientConnectId::new("connect-1").unwrap(),
        provider_id: provider_id("openai"),
        catalog_revision: catalog_revision(),
        credentials: ProviderCredentials {
            values: BTreeMap::from([(credential_field("OPENAI_API_KEY"), secret.into())]),
        },
    };
    assert!(!format!("{connect:?}").contains(secret));
    assert!(serde_json::to_string(&connect).unwrap().contains(secret));
    round_trip(connect);
    let summary = InternalSummaryCheckpoint::new(
        "summary".into(),
        InternalAgentInvocationId(Uuid::from_u128(9)),
        InternalAgentRunId(Uuid::from_u128(10)),
        SummaryByteLimit::new(1024).unwrap(),
    )
    .unwrap();
    let commit = ContextCheckpointCommit {
        checkpoint: ContextCheckpoint::InternalSummary {
            checkpoint: summary,
        },
        boundaries: ContextCheckpointBoundaries {
            source_from_seq: 1,
            source_through_seq: 10,
            input_through_seq: 10,
            prior_checkpoint_seq: None,
        },
        budgets: ContextCheckpointBudgets {
            context_limit_tokens: 100,
            trigger_tokens: 90,
            target_tokens: 50,
            input_tokens_before: 91,
            input_tokens_after: 49,
            max_summary_bytes: SummaryByteLimit::new(1024).unwrap(),
        },
    };
    round_trip(commit);
    for change in [
        SessionTitleChange::UserSet {
            title: SessionTitle::new("User title").unwrap(),
            client_rename_id: ClientRenameId::new("rename-1").unwrap(),
        },
        SessionTitleChange::UserClear {
            client_rename_id: ClientRenameId::new("rename-2").unwrap(),
        },
        SessionTitleChange::UserReset {
            client_rename_id: ClientRenameId::new("rename-3").unwrap(),
        },
        SessionTitleChange::InternalAgentSet {
            title: SessionTitle::new("Generated").unwrap(),
            invocation_id: InternalAgentInvocationId(Uuid::from_u128(9)),
        },
        SessionTitleChange::FallbackSet {
            title: SessionTitle::new("Fallback").unwrap(),
        },
    ] {
        round_trip(change);
    }
    assert!(SessionTitle::new(" ").is_err());
    assert!(SessionTitle::new("bad\ntitle").is_err());
    assert!(SessionTitle::new("x".repeat(513)).is_err());
    assert!(ClientRenameId::new("x".repeat(257)).is_err());
}

#[test]
fn provider_connect_error_missing_fields_are_code_exact_sorted_and_unique() {
    let base = json!({
        "code": "missing_credential",
        "provider_id": "openai",
        "client_connect_id": "connect-1",
        "missing_credential_fields": ["OPENAI_API_KEY"]
    });
    assert!(serde_json::from_value::<ProviderConnectError>(base).is_ok());
    for invalid in [
        json!({
            "code": "missing_credential",
            "provider_id": "openai",
            "client_connect_id": "connect-1",
            "missing_credential_fields": []
        }),
        json!({
            "code": "unknown_provider",
            "provider_id": "openai",
            "client_connect_id": "connect-1",
            "missing_credential_fields": ["OPENAI_API_KEY"]
        }),
        json!({
            "code": "missing_credential",
            "provider_id": "openai",
            "client_connect_id": "connect-1",
            "missing_credential_fields": ["Z_KEY", "A_KEY"]
        }),
    ] {
        assert!(serde_json::from_value::<ProviderConnectError>(invalid).is_err());
    }
}

#[test]
fn every_remaining_event_and_ephemeral_delivery_variant_round_trips() {
    let internal_invocation = InternalAgentInvocationId(Uuid::from_u128(9));
    let internal_run = InternalAgentRunId(Uuid::from_u128(10));
    let backend = InternalAgentBackend::Model {
        resolved_model: resolved_for("openai/gpt-5.6-sol", Some("high")),
    };
    let failure = InternalAgentFailure {
        code: code("model_unavailable"),
        message: error_text("model unavailable"),
        retryable: true,
        model_error: Some(model_error()),
    };
    let summary = InternalSummaryCheckpoint::new(
        "summary".into(),
        internal_invocation,
        internal_run,
        SummaryByteLimit::new(1024).unwrap(),
    )
    .unwrap();
    let checkpoint = ContextCheckpointCommit {
        checkpoint: ContextCheckpoint::InternalSummary {
            checkpoint: summary,
        },
        boundaries: ContextCheckpointBoundaries {
            source_from_seq: 1,
            source_through_seq: 10,
            input_through_seq: 10,
            prior_checkpoint_seq: None,
        },
        budgets: ContextCheckpointBudgets {
            context_limit_tokens: 100,
            trigger_tokens: 90,
            target_tokens: 50,
            input_tokens_before: 91,
            input_tokens_after: 49,
            max_summary_bytes: SummaryByteLimit::new(1024).unwrap(),
        },
    };
    let restart_resource = PreparedApprovalResource {
        binding_lifetime: PreparedBindingLifetime::RestartStable,
        ..prepared_operation().resources()[0].clone()
    };
    let grant = TreeApprovalGrant {
        grant_id: TreeApprovalGrantId(Uuid::from_u128(11)),
        root_session_id: session_id(),
        approval_id: approval_id(),
        operation_fingerprint: approval_request().operation_fingerprint().clone(),
        capabilities: vec![ApprovalCapability {
            action: PermissionAction::Bash,
            operation: PreparedResourceIdentity::new("execute").unwrap(),
        }],
        resources: vec![restart_resource],
        created_at: Timestamp::now(),
    };
    let final_decision = ApprovalFinalDecision {
        outcome: ApprovalFinalOutcome::Approved,
        source: ApprovalDecisionSource::User,
        reason_code: ApprovalReasonCode::UserApprovedOnce,
        feedback: None,
        tree_grant_id: None,
    };
    let payloads = vec![
        EventPayload::UserInputSubmitted {
            input: "steer".into(),
        },
        EventPayload::UserInputApplied { user_input_seq: 2 },
        EventPayload::RunCompleted {
            final_text: Some("done".into()),
        },
        EventPayload::RunFailed {
            error: error_text("failed"),
        },
        EventPayload::RunCancelled { reason: None },
        EventPayload::RunInterrupted {
            reason: Some(error_text("restart")),
        },
        EventPayload::AttemptAbandoned {
            attempt_id: attempt_id(),
        },
        EventPayload::ToolCallProgress {
            tool_call_id: call_id(),
            message: safe("working"),
        },
        EventPayload::ToolStdinSubmitted {
            tool_call_id: call_id(),
            byte_count: 4,
        },
        EventPayload::ToolCallLinked {
            tool_call_id: call_id(),
            child_session_id: SessionId(Uuid::from_u128(12)),
        },
        EventPayload::ApprovalRequested {
            request: approval_request(),
        },
        EventPayload::ApprovalEvaluated {
            approval_id: approval_id(),
            decision: ApprovalInternalDecision {
                decision: ApprovalInternalDecisionKind::Ask,
                source: ApprovalDecisionSource::Policy,
                reason_code: ApprovalReasonCode::PolicyRequiresApproval,
                evaluations: vec![],
            },
        },
        EventPayload::ApprovalEscalated {
            approval_id: approval_id(),
            reason_code: ApprovalReasonCode::Escalated,
        },
        EventPayload::ApprovalUserDecisionRecorded {
            approval_id: approval_id(),
            client_response_id: ClientResponseId::new("response-1").unwrap(),
            decision: ApprovalUserDecision::ApproveOnce,
            feedback: None,
        },
        EventPayload::ApprovalFinalized {
            approval_id: approval_id(),
            decision: final_decision,
        },
        EventPayload::ApprovalCancelled {
            approval_id: approval_id(),
            reason_code: ApprovalReasonCode::RequestCancelled,
        },
        EventPayload::ApprovalDoomLoopDetected {
            approval_id: approval_id(),
            operation_fingerprint: approval_request().operation_fingerprint().clone(),
            repetitions: 4,
        },
        EventPayload::TreeApprovalGrantCommitted { grant },
        EventPayload::InternalAgentStarted {
            invocation_id: internal_invocation,
            internal_run_id: internal_run,
            kind: InternalAgentKind::ContextCompaction,
            backend: backend.clone(),
            call: SafeInternalAgentCall {
                name: code("compact_context"),
                input_summary: safe("history through seq 10"),
                input_digest: digest("input"),
            },
        },
        EventPayload::InternalAgentCompleted {
            invocation_id: internal_invocation,
            internal_run_id: internal_run,
            kind: InternalAgentKind::ContextCompaction,
            result: SafeInternalAgentResult {
                output_summary: safe("checkpoint committed"),
                output_digest: digest("output"),
            },
        },
        EventPayload::InternalAgentFailed {
            invocation_id: internal_invocation,
            internal_run_id: internal_run,
            kind: InternalAgentKind::ContextCompaction,
            failure: failure.clone(),
        },
        EventPayload::InternalAgentCancelled {
            invocation_id: internal_invocation,
            internal_run_id: internal_run,
            kind: InternalAgentKind::ContextCompaction,
            reason: None,
        },
        EventPayload::InternalAgentInterrupted {
            invocation_id: internal_invocation,
            internal_run_id: internal_run,
            kind: InternalAgentKind::ContextCompaction,
            reason: Some(error_text("restart")),
        },
        EventPayload::InternalAgentFallback {
            invocation_id: internal_invocation,
            internal_run_id: internal_run,
            kind: InternalAgentKind::ContextCompaction,
            from: backend,
            to: InternalAgentBackend::Builtin {
                name: code("summary_fallback"),
                revision: safe("1"),
            },
            failure,
            attempts: 1,
        },
        EventPayload::ContextCheckpointCommitted { commit: checkpoint },
    ];
    for (index, payload) in payloads.into_iter().enumerate() {
        round_trip(stored(payload, index as u64 + 2, true));
    }
    round_trip(stored(
        EventPayload::SessionTitleCommitted {
            change: SessionTitleChange::UserClear {
                client_rename_id: ClientRenameId::new("rename-clear").unwrap(),
            },
            input_through_seq: 10,
        },
        40,
        false,
    ));
    for outcome in [
        ToolTerminationOutcome::Failed,
        ToolTerminationOutcome::Cancelled,
        ToolTerminationOutcome::Interrupted,
    ] {
        round_trip(stored(
            EventPayload::ToolCallTerminated {
                termination: ToolCallTermination {
                    tool_call_id: call_id(),
                    owner: owner(),
                    outcome,
                    result: None,
                    error: Some(SafeToolError {
                        code: code("terminated"),
                        message: error_text("terminated"),
                    }),
                },
            },
            41,
            true,
        ));
    }
    let delta = OutputDelta {
        call_id: call_id(),
        stream: OutputStream::Stdout,
        byte_offset: 0,
        data: "b3V0".into(),
    };
    round_trip(delta.clone());
    round_trip(OutputGap {
        call_id: call_id(),
        stream: OutputStream::Stderr,
        next_offset: 4,
    });
    round_trip(OutputSnapshot {
        call_id: call_id(),
        start_offset: 0,
        end_offset: 3,
        chunks: vec![delta],
    });
    round_trip(OutputSnapshotEnvelope {
        stream: OutputStream::Stdout,
        snapshot: OutputSnapshot {
            call_id: call_id(),
            start_offset: 0,
            end_offset: 0,
            chunks: vec![],
        },
    });
    round_trip(EventSubscriptionMessage::Gap {
        session_id: session_id(),
        last_delivered_seq: 3,
    });
}

#[test]
fn global_bounds_fail_closed() {
    let mut prompt = serde_json::to_value(agent_snapshot()).unwrap();
    prompt["composed_prompt"] = json!("x".repeat(AgentSnapshot::MAX_PROMPT_BYTES + 1));
    assert!(serde_json::from_value::<AgentSnapshot>(prompt).is_err());
    let mut output = serde_json::to_value(tool_result()).unwrap();
    output["output"] = json!("x".repeat(PersistedToolResult::MAX_OUTPUT_BYTES + 1));
    assert!(serde_json::from_value::<PersistedToolResult>(output).is_err());
    let native = json!({"adapter_id":"openai-responses","selection_fingerprint":"00".repeat(32),"scope":replay_scope(),"byte_length":NativeContextWindow::MAX_PAYLOAD_BYTES as u64+1,"sha256":"00".repeat(32),"reference":artifact()});
    assert!(serde_json::from_value::<NativeContextArtifact>(native).is_err());
    let cleared = SessionMeta {
        title: None,
        title_updated_seq: 3,
        last_event_seq: 3,
        ..meta()
    };
    round_trip(cleared);
}

#[test]
fn strict_unknown_fields_and_old_profile_alias_types_are_absent() {
    assert!(serde_json::from_value::<RunSelection>(json!({"agent":"primary","model":{"model":"openai/gpt-5.6-sol","variant":null},"profile":"legacy"})).is_err());
    assert!(serde_json::from_value::<ResolvedModelRef>(json!({"selection":{"model":"openai/gpt-5.6-sol","variant":null},"provider_id":"openai","model_id":"gpt-5.6-sol","adapter_id":"openai-responses","selection_fingerprint":"00".repeat(32),"name":"alias"})).is_err());
    let event_ts = EventPayload::export_to_string(&typescript_config()).unwrap();
    assert!(!event_ts.contains("ProfileSnapshot"));
    assert!(!event_ts.contains("ModelRef, model_id"));
    assert!(!event_ts.contains("ToolCallFailed"));
    assert!(!event_ts.contains("ToolCallCompleted"));
}

#[test]
fn protocol_schema_snapshots() {
    assert_json_snapshot!("stored_event_schema", schema_for!(StoredEvent));
    assert_json_snapshot!("agent_snapshot_schema", schema_for!(AgentSnapshot));
    assert_json_snapshot!("model_list_schema", schema_for!(ModelListResult));
    assert_json_snapshot!("agent_list_schema", schema_for!(AgentListResult));
    assert_json_snapshot!("session_meta_schema", schema_for!(SessionMeta));
    assert_json_snapshot!("run_start_schema", schema_for!(RunStartParams));
    assert_json_snapshot!("tool_termination_schema", schema_for!(ToolCallTermination));
    assert_json_snapshot!("title_change_schema", schema_for!(SessionTitleChange));
    assert_json_snapshot!(
        "approval_respond_schema",
        schema_for!(ApprovalRespondParams)
    );
    assert_json_snapshot!(
        "provider_connect_schema",
        schema_for!(ProviderConnectParams)
    );
    assert_json_snapshot!(
        "context_checkpoint_schema",
        schema_for!(ContextCheckpointCommit)
    );
    assert_json_snapshot!("persisted_turn_schema", schema_for!(PersistedModelTurn));
}

#[test]
fn typescript_binding_snapshots_and_number_gate() {
    let config = typescript_config();
    assert_snapshot!(
        "stored_event_typescript",
        StoredEvent::export_to_string(&config).unwrap()
    );
    assert_snapshot!(
        "agent_snapshot_typescript",
        AgentSnapshot::export_to_string(&config).unwrap()
    );
    assert_snapshot!(
        "model_list_typescript",
        ModelListResult::export_to_string(&config).unwrap()
    );
    assert_snapshot!(
        "agent_list_typescript",
        AgentListResult::export_to_string(&config).unwrap()
    );
    assert_snapshot!(
        "session_meta_typescript",
        SessionMeta::export_to_string(&config).unwrap()
    );
    assert_snapshot!(
        "run_start_typescript",
        RunStartParams::export_to_string(&config).unwrap()
    );
    assert_snapshot!(
        "tool_termination_typescript",
        ToolCallTermination::export_to_string(&config).unwrap()
    );
    assert_snapshot!(
        "title_change_typescript",
        SessionTitleChange::export_to_string(&config).unwrap()
    );
    assert_snapshot!(
        "approval_respond_typescript",
        ApprovalRespondParams::export_to_string(&config).unwrap()
    );
    assert_snapshot!(
        "provider_connect_typescript",
        ProviderConnectParams::export_to_string(&config).unwrap()
    );
    assert_snapshot!(
        "context_checkpoint_typescript",
        ContextCheckpointCommit::export_to_string(&config).unwrap()
    );
    assert_snapshot!(
        "persisted_turn_typescript",
        PersistedModelTurn::export_to_string(&config).unwrap()
    );
    for declaration in [
        StoredEvent::export_to_string(&config).unwrap(),
        AgentSnapshot::export_to_string(&config).unwrap(),
        PersistedModelTurn::export_to_string(&config).unwrap(),
    ] {
        assert!(!declaration.contains("bigint"));
    }
}

#[test]
fn shared_agent_id_is_the_phase_one_type() {
    assert_eq!(
        std::any::TypeId::of::<AgentId>(),
        std::any::TypeId::of::<cookie_agent_identity::AgentId>()
    );
}

#[test]
fn shared_provider_id_is_the_phase_one_type() {
    assert_eq!(
        std::any::TypeId::of::<ProviderId>(),
        std::any::TypeId::of::<cookie_agent_identity::ProviderId>()
    );
}

#[test]
fn shared_model_key_is_the_phase_one_type() {
    assert_eq!(
        std::any::TypeId::of::<ModelKey>(),
        std::any::TypeId::of::<cookie_agent_identity::ModelKey>()
    );
}

#[test]
fn shared_variant_id_is_the_phase_one_type() {
    assert_eq!(
        std::any::TypeId::of::<VariantId>(),
        std::any::TypeId::of::<cookie_agent_identity::VariantId>()
    );
}

#[test]
fn shared_model_selection_is_the_phase_one_type() {
    assert_eq!(
        std::any::TypeId::of::<ModelSelection>(),
        std::any::TypeId::of::<cookie_agent_identity::ModelSelection>()
    );
}

#[test]
fn session_created_rejects_selection_outside_fallback() {
    let mut snapshot = agent_snapshot();
    snapshot.selected_suffix_start = 1;
    let event = stored(
        EventPayload::SessionCreated {
            origin: SessionOrigin::Root,
            cwd_identity: CwdIdentity::new("workspace").unwrap(),
            creation_selection: run_selection(),
            creation_agent: Box::new(snapshot),
            model_snapshot_fingerprint: digest("models"),
        },
        1,
        false,
    );
    assert!(serde_json::from_value::<StoredEvent>(serde_json::to_value(event).unwrap()).is_err());
}

#[test]
fn session_created_rejects_selection_agent_mismatch() {
    let mut selection = run_selection();
    selection.agent = agent_id("worker");
    let event = stored(
        EventPayload::SessionCreated {
            origin: SessionOrigin::Root,
            cwd_identity: CwdIdentity::new("workspace").unwrap(),
            creation_selection: selection,
            creation_agent: Box::new(agent_snapshot()),
            model_snapshot_fingerprint: digest("models"),
        },
        1,
        false,
    );
    assert!(serde_json::from_value::<StoredEvent>(serde_json::to_value(event).unwrap()).is_err());
}

#[test]
fn run_started_rejects_truncated_selected_suffix() {
    let event = stored(
        EventPayload::RunStarted {
            client_run_id: ClientRunId::new("run-1").unwrap(),
            selection: run_selection(),
            agent: Box::new(agent_snapshot()),
            selected_suffix: vec![binding_for("openai/gpt-5.6-sol", Some("high"))],
            input_through_seq: 1,
        },
        2,
        true,
    );
    assert!(serde_json::from_value::<StoredEvent>(serde_json::to_value(event).unwrap()).is_err());
}

#[test]
fn run_started_rejects_reordered_selected_suffix() {
    let event = stored(
        EventPayload::RunStarted {
            client_run_id: ClientRunId::new("run-1").unwrap(),
            selection: run_selection(),
            agent: Box::new(agent_snapshot()),
            selected_suffix: vec![
                binding_for("anthropic/claude-sonnet-4-6", None),
                binding_for("openai/gpt-5.6-sol", Some("high")),
            ],
            input_through_seq: 1,
        },
        2,
        true,
    );
    assert!(serde_json::from_value::<StoredEvent>(serde_json::to_value(event).unwrap()).is_err());
}

#[test]
fn run_started_accepts_exact_later_suffix() {
    let mut snapshot = agent_snapshot();
    snapshot.selected_suffix_start = 1;
    let selection = RunSelection {
        agent: snapshot.agent.clone(),
        model: selection("anthropic/claude-sonnet-4-6", None),
    };
    let event = stored(
        EventPayload::RunStarted {
            client_run_id: ClientRunId::new("run-2").unwrap(),
            selection,
            agent: Box::new(snapshot),
            selected_suffix: vec![binding_for("anthropic/claude-sonnet-4-6", None)],
            input_through_seq: 1,
        },
        2,
        true,
    );
    round_trip(event);
}

#[test]
fn run_started_accepts_exact_head_variant_override() {
    let selection = RunSelection {
        agent: agent_id("primary"),
        model: selection("openai/gpt-5.6-sol", Some("low")),
    };
    let event = stored(
        EventPayload::RunStarted {
            client_run_id: ClientRunId::new("run-low").unwrap(),
            selection,
            agent: Box::new(agent_snapshot()),
            selected_suffix: vec![
                binding_for("openai/gpt-5.6-sol", Some("low")),
                binding_for("anthropic/claude-sonnet-4-6", None),
            ],
            input_through_seq: 1,
        },
        2,
        true,
    );
    round_trip(event);
}

#[test]
fn run_started_rejects_mutated_unchanged_head_binding() {
    let mut head = binding_for("openai/gpt-5.6-sol", Some("high"));
    head.behavior_fingerprint = digest("mutated");
    let event = stored(
        EventPayload::RunStarted {
            client_run_id: ClientRunId::new("run-1").unwrap(),
            selection: run_selection(),
            agent: Box::new(agent_snapshot()),
            selected_suffix: vec![head, binding_for("anthropic/claude-sonnet-4-6", None)],
            input_through_seq: 1,
        },
        2,
        true,
    );
    assert!(serde_json::from_value::<StoredEvent>(serde_json::to_value(event).unwrap()).is_err());
}

#[test]
fn native_replay_rejects_foreign_adapter() {
    let artifact = native_replay();
    let mut binding = binding_for("openai/gpt-5.6-sol", Some("high"));
    binding.descriptor.adapter_id = oven_sdk::AdapterId::new("foreign.adapter");
    assert!(
        artifact
            .validate_for_binding(&binding, &replay_scope())
            .is_err()
    );
}

#[test]
fn replay_disposition_preserves_exact_safe_adapter_ids() {
    round_trip(ReplayDisposition::DiscardedForeignAdapter {
        found: code("vendor.custom-adapter.v2"),
        expected: code("cookie.openai-responses.v1"),
    });
}

#[test]
fn native_replay_rejects_foreign_selection_fingerprint() {
    let artifact = native_replay();
    let mut resolved = resolved_for("openai/gpt-5.6-sol", Some("high"));
    resolved.selection_fingerprint = digest("foreign");
    assert!(artifact.validate_resolved(&resolved).is_err());
}

#[test]
fn native_replay_rejects_foreign_scope() {
    let artifact = native_replay();
    let mut expected = replay_scope();
    expected.resource_id = safe("different-state");
    assert!(
        artifact
            .validate_for(&resolved_for("openai/gpt-5.6-sol", Some("high")), &expected)
            .is_err()
    );
}

#[test]
fn native_context_rejects_foreign_provider_scope() {
    let resolved = resolved_for("openai/gpt-5.6-sol", Some("high"));
    let artifact = NativeContextArtifact {
        adapter_id: code("openai-responses"),
        selection_fingerprint: resolved.selection_fingerprint.clone(),
        scope: NativeContextScope {
            provider_id: provider_id("anthropic"),
            ..replay_scope()
        },
        byte_length: 1,
        sha256: digest("native"),
        reference: artifact(),
    };
    assert!(artifact.validate_for(&resolved, &artifact.scope).is_err());
}

#[test]
fn native_context_requires_exact_adapter_scope_and_variant_fingerprint() {
    let binding = binding_for("openai/gpt-5.6-sol", Some("high"));
    let artifact = NativeContextArtifact {
        adapter_id: code(binding.descriptor.adapter_id.as_str()),
        selection_fingerprint: binding.resolved.selection_fingerprint.clone(),
        scope: replay_scope(),
        byte_length: 1,
        sha256: digest("native"),
        reference: artifact(),
    };
    artifact
        .validate_for_binding(&binding, &replay_scope())
        .unwrap();
    assert!(
        artifact
            .validate_for_binding(
                &binding_for("openai/gpt-5.6-sol", Some("low")),
                &replay_scope()
            )
            .is_err()
    );
    let mut foreign_adapter = artifact.clone();
    foreign_adapter.adapter_id = code("foreign.adapter");
    assert!(
        foreign_adapter
            .validate_for_binding(&binding, &replay_scope())
            .is_err()
    );
}

#[test]
fn native_replay_validates_against_complete_frozen_binding() {
    let binding = binding_for("openai/gpt-5.6-sol", Some("high"));
    let artifact = NativeReplayArtifact::new(
        code(binding.descriptor.adapter_id.as_str()),
        binding.resolved.selection_fingerprint.clone(),
        replay_scope(),
        json!({"state":1}),
    )
    .unwrap();
    artifact
        .validate_for_binding(&binding, &replay_scope())
        .unwrap();
    let mut forged = binding;
    forged.resolved.selection_fingerprint = digest("forged");
    assert!(
        artifact
            .validate_for_binding(&forged, &replay_scope())
            .is_err()
    );
}

#[test]
fn model_turn_rejects_foreign_native_replay() {
    let mut resolved = resolved_for("openai/gpt-5.6-sol", Some("high"));
    resolved.selection_fingerprint = digest("foreign");
    let event = stored(
        EventPayload::ModelTurnCommitted {
            attempt_id: attempt_id(),
            model_turn_seq: 1,
            resolved_model: resolved,
            input_through_seq: 1,
            turn: persisted_turn(),
            warnings: Vec::new(),
        },
        3,
        true,
    );
    assert!(serde_json::from_value::<StoredEvent>(serde_json::to_value(event).unwrap()).is_err());
}

#[test]
fn catalog_revision_accepts_only_the_pinned_snapshot() {
    round_trip(catalog_revision());
    assert!(CatalogRevision::new(format!("sha256:{}", "0".repeat(64))).is_err());
}

#[test]
fn credential_field_name_is_strict_and_bounded() {
    assert!(CredentialFieldName::new("OPENAI_API_KEY").is_ok());
    assert!(CredentialFieldName::new("302AI_API_KEY").is_ok());
    assert!(CredentialFieldName::new("openai_api_key").is_err());
    assert!(CredentialFieldName::new(format!("A{}", "B".repeat(1024))).is_err());
}

#[test]
fn catalog_provider_rejects_unsorted_credential_fields() {
    let value = json!({"id":"openai","name":"OpenAI","credential_fields":["Z_KEY","A_KEY"],"npm":"@ai-sdk/openai","api":null,"documentation_url":"https://example.test"});
    assert!(serde_json::from_value::<CatalogProvider>(value).is_err());
}

#[test]
fn catalog_provider_rejects_duplicate_credential_fields() {
    let value = json!({"id":"openai","name":"OpenAI","credential_fields":["API_KEY","API_KEY"],"npm":"@ai-sdk/openai","api":null,"documentation_url":"https://example.test"});
    assert!(serde_json::from_value::<CatalogProvider>(value).is_err());
}

#[test]
fn catalog_provider_rejects_empty_credential_fields() {
    let value = json!({"id":"openai","name":"OpenAI","credential_fields":[],"npm":"@ai-sdk/openai","api":null,"documentation_url":"https://example.test"});
    assert!(serde_json::from_value::<CatalogProvider>(value).is_err());
}

#[test]
fn catalog_model_limits_reject_zero() {
    assert!(
        serde_json::from_value::<CatalogModelLimits>(json!({"context":0,"input":null,"output":1}))
            .is_err()
    );
}

#[test]
fn catalog_model_limits_reject_output_over_context() {
    assert!(
        serde_json::from_value::<CatalogModelLimits>(
            json!({"context":10,"input":null,"output":11})
        )
        .is_err()
    );
}

#[test]
fn catalog_provider_list_rejects_unsorted_records() {
    let provider = |id: &str| json!({"id":id,"name":id,"credential_fields":["API_KEY"],"npm":"package","api":null,"documentation_url":"https://example.test"});
    let value = json!({"snapshot":{"revision":catalog_revision(),"source":PINNED_CATALOG_SOURCE,"fetched_at":PINNED_CATALOG_FETCHED_AT},"providers":[provider("z"),provider("a")]});
    assert!(serde_json::from_value::<CatalogProviderListResult>(value).is_err());
}

#[test]
fn catalog_model_list_rejects_unsorted_records() {
    let model = |id: &str| json!({"provider_id":"openai","model_id":id,"canonical_model_id":null,"name":id,"family":null,"capabilities":{"attachment":false,"reasoning":false,"tool_call":true,"structured_output":false,"temperature":true},"reasoning_options":[],"limits":{"context":100,"input":null,"output":10},"modalities":{"input":["text"],"output":["text"]},"status":"stable","release_date":"2026-01-01","last_updated":"2026-01-01"});
    let value = json!({"snapshot":{"revision":catalog_revision(),"source":PINNED_CATALOG_SOURCE,"fetched_at":PINNED_CATALOG_FETCHED_AT},"models":[model("z"),model("a")]});
    assert!(serde_json::from_value::<CatalogModelListResult>(value).is_err());
}

#[test]
fn provider_connection_rejects_unsorted_credentials() {
    let value = json!({"provider_id":"openai","credential_fields":["Z_KEY","A_KEY"],"connected_at":Timestamp::now(),"catalog_revision":catalog_revision()});
    assert!(serde_json::from_value::<ProviderConnection>(value).is_err());
}

#[test]
fn provider_connect_rejects_unpinned_revision() {
    let value = json!({"client_connect_id":"connect","provider_id":"openai","catalog_revision":format!("sha256:{}","0".repeat(64)),"credentials":{"values":{"OPENAI_API_KEY":"secret"}}});
    assert!(serde_json::from_value::<ProviderConnectParams>(value).is_err());
}

#[test]
fn provider_connect_rejects_duplicate_credential_object_keys() {
    let wire = format!(
        r#"{{"client_connect_id":"connect","provider_id":"openai","catalog_revision":"{PINNED_CATALOG_REVISION}","credentials":{{"values":{{"OPENAI_API_KEY":"one","OPENAI_API_KEY":"two"}}}}}}"#
    );
    assert!(serde_json::from_str::<ProviderConnectParams>(&wire).is_err());
}

#[test]
fn approval_internal_allow_rejects_denied_reason() {
    let value = json!({"decision":"allow","source":"policy","reason_code":"policy_denied","evaluations":[]});
    assert!(serde_json::from_value::<ApprovalInternalDecision>(value).is_err());
}

#[test]
fn approval_internal_ask_rejects_tree_grant_source() {
    let value = json!({"decision":"ask","source":"tree_grant","reason_code":"policy_requires_approval","evaluations":[]});
    assert!(serde_json::from_value::<ApprovalInternalDecision>(value).is_err());
}

#[test]
fn approval_final_rejects_outcome_reason_contradiction() {
    let value = json!({"outcome":"approved","source":"user","reason_code":"user_rejected","feedback":null,"tree_grant_id":null});
    assert!(serde_json::from_value::<ApprovalFinalDecision>(value).is_err());
}

#[test]
fn approval_final_tree_requires_grant_id() {
    let value = json!({"outcome":"approved","source":"user","reason_code":"user_approved_tree","feedback":null,"tree_grant_id":null});
    assert!(serde_json::from_value::<ApprovalFinalDecision>(value).is_err());
}

#[test]
fn approval_record_rejects_pending_final_decision() {
    let record = json!({"session_id":session_id(),"request":approval_request(),"status":"pending","internal_decision":null,"user_decision":null,"final_decision":{"outcome":"rejected","source":"user","reason_code":"user_rejected","feedback":null,"tree_grant_id":null}});
    assert!(serde_json::from_value::<ApprovalRecord>(record).is_err());
}

#[test]
fn approval_escalated_requires_escalated_reason() {
    let event = stored(
        EventPayload::ApprovalEscalated {
            approval_id: approval_id(),
            reason_code: ApprovalReasonCode::PolicyRequiresApproval,
        },
        2,
        true,
    );
    assert!(serde_json::from_value::<StoredEvent>(serde_json::to_value(event).unwrap()).is_err());
}

#[test]
fn approval_feedback_is_reject_only() {
    let event = stored(
        EventPayload::ApprovalUserDecisionRecorded {
            approval_id: approval_id(),
            client_response_id: ClientResponseId::new("response").unwrap(),
            decision: ApprovalUserDecision::ApproveOnce,
            feedback: Some(ApprovalFeedback {
                message: error_text("not needed"),
            }),
        },
        2,
        true,
    );
    assert!(serde_json::from_value::<StoredEvent>(serde_json::to_value(event).unwrap()).is_err());
}

#[test]
fn doom_loop_counter_starts_at_four() {
    let event = stored(
        EventPayload::ApprovalDoomLoopDetected {
            approval_id: approval_id(),
            operation_fingerprint: approval_request().operation_fingerprint().clone(),
            repetitions: 3,
        },
        2,
        true,
    );
    assert!(serde_json::from_value::<StoredEvent>(serde_json::to_value(event).unwrap()).is_err());
}

#[test]
fn internal_fallback_rejects_zero_attempts() {
    let backend = InternalAgentBackend::Builtin {
        name: code("fallback"),
        revision: safe("v1"),
    };
    let event = stored(
        EventPayload::InternalAgentFallback {
            invocation_id: InternalAgentInvocationId(Uuid::from_u128(9)),
            internal_run_id: InternalAgentRunId(Uuid::from_u128(10)),
            kind: InternalAgentKind::SessionTitle,
            from: backend.clone(),
            to: InternalAgentBackend::Model {
                resolved_model: resolved_for("openai/gpt-5.6-sol", None),
            },
            failure: InternalAgentFailure {
                code: code("failed"),
                message: error_text("failed"),
                retryable: true,
                model_error: None,
            },
            attempts: 0,
        },
        2,
        true,
    );
    assert!(serde_json::from_value::<StoredEvent>(serde_json::to_value(event).unwrap()).is_err());
}

#[test]
fn internal_fallback_rejects_same_backend() {
    let backend = InternalAgentBackend::Builtin {
        name: code("fallback"),
        revision: safe("v1"),
    };
    let event = stored(
        EventPayload::InternalAgentFallback {
            invocation_id: InternalAgentInvocationId(Uuid::from_u128(9)),
            internal_run_id: InternalAgentRunId(Uuid::from_u128(10)),
            kind: InternalAgentKind::SessionTitle,
            from: backend.clone(),
            to: backend,
            failure: InternalAgentFailure {
                code: code("failed"),
                message: error_text("failed"),
                retryable: false,
                model_error: None,
            },
            attempts: 1,
        },
        2,
        true,
    );
    assert!(serde_json::from_value::<StoredEvent>(serde_json::to_value(event).unwrap()).is_err());
}

#[test]
fn internal_failure_rejects_retryable_contradiction() {
    let mut model_error = model_error();
    model_error.retryable = true;
    let event = stored(
        EventPayload::InternalAgentFailed {
            invocation_id: InternalAgentInvocationId(Uuid::from_u128(9)),
            internal_run_id: InternalAgentRunId(Uuid::from_u128(10)),
            kind: InternalAgentKind::Approval,
            failure: InternalAgentFailure {
                code: code("failed"),
                message: error_text("failed"),
                retryable: false,
                model_error: Some(model_error),
            },
        },
        2,
        true,
    );
    assert!(serde_json::from_value::<StoredEvent>(serde_json::to_value(event).unwrap()).is_err());
}

#[test]
fn context_checkpoint_rejects_inverted_token_budget() {
    let commit = ContextCheckpointCommit {
        checkpoint: ContextCheckpoint::InternalSummary {
            checkpoint: InternalSummaryCheckpoint::new(
                "summary".into(),
                InternalAgentInvocationId(Uuid::from_u128(9)),
                InternalAgentRunId(Uuid::from_u128(10)),
                SummaryByteLimit::new(100).unwrap(),
            )
            .unwrap(),
        },
        boundaries: ContextCheckpointBoundaries {
            source_from_seq: 1,
            source_through_seq: 2,
            input_through_seq: 2,
            prior_checkpoint_seq: None,
        },
        budgets: ContextCheckpointBudgets {
            context_limit_tokens: 100,
            trigger_tokens: 80,
            target_tokens: 90,
            input_tokens_before: 90,
            input_tokens_after: 50,
            max_summary_bytes: SummaryByteLimit::new(100).unwrap(),
        },
    };
    assert!(commit.validate().is_err());
}

#[test]
fn title_user_set_requires_client_rename_id() {
    let value = json!({"type":"user_set","title":"Title"});
    assert!(serde_json::from_value::<SessionTitleChange>(value).is_err());
}

#[test]
fn title_internal_set_rejects_client_rename_id() {
    let value = json!({"type":"internal_agent_set","title":"Title","invocation_id":InternalAgentInvocationId(Uuid::from_u128(9)),"client_rename_id":"rename"});
    assert!(serde_json::from_value::<SessionTitleChange>(value).is_err());
}

#[test]
fn title_rejects_control_characters() {
    assert!(SessionTitle::new("bad\ntitle").is_err());
}

#[test]
fn title_meta_sequence_cannot_exceed_last_event() {
    let mut value = serde_json::to_value(meta()).unwrap();
    value["title_updated_seq"] = json!(4);
    value["last_event_seq"] = json!(3);
    assert!(serde_json::from_value::<SessionMeta>(value).is_err());
}

#[test]
fn title_change_golden() {
    assert_json_snapshot!(
        "title_change_v7_golden",
        vec![
            SessionTitleChange::UserSet {
                title: SessionTitle::new("Named").unwrap(),
                client_rename_id: ClientRenameId::new("rename-1").unwrap(),
            },
            SessionTitleChange::UserClear {
                client_rename_id: ClientRenameId::new("rename-2").unwrap(),
            },
            SessionTitleChange::UserReset {
                client_rename_id: ClientRenameId::new("rename-3").unwrap(),
            },
            SessionTitleChange::FallbackSet {
                title: SessionTitle::new("Fallback").unwrap(),
            },
        ]
    );
}

#[test]
fn catalog_golden() {
    let value = CatalogProviderListResult {
        snapshot: CatalogSnapshot {
            revision: catalog_revision(),
            source: CatalogText::new(PINNED_CATALOG_SOURCE).unwrap(),
            fetched_at: "2026-08-01T17:34:27Z".parse().unwrap(),
        },
        providers: vec![CatalogProvider {
            id: CatalogIdentifier::new("openai").unwrap(),
            name: CatalogText::new("OpenAI").unwrap(),
            credential_fields: vec![credential_field("OPENAI_API_KEY")],
            npm: CatalogText::new("@ai-sdk/openai").unwrap(),
            api: Some(CatalogText::new("https://api.openai.com/v1").unwrap()),
            documentation_url: CatalogText::new("https://platform.openai.com/docs").unwrap(),
        }],
    };
    assert_json_snapshot!("catalog_provider_list_v7_golden", value);
}

#[test]
fn generated_schema_set_contains_all_protocol_roots() {
    let names = json_schema_documents()
        .into_iter()
        .map(|(name, _)| name)
        .collect::<BTreeSet<_>>();
    assert!(names.contains("StoredEvent.schema.json"));
    assert!(names.contains("CatalogProviderListResult.schema.json"));
    assert!(names.contains("CatalogModelListResult.schema.json"));
    assert!(names.contains("AgentListResult.schema.json"));
    assert!(names.len() >= 50);
}

#[test]
fn generated_typescript_set_is_exportable_and_consumable() {
    let output = std::env::temp_dir().join(format!("cookie-protocol-ts-{}", Uuid::now_v7()));
    export_typescript_binding_set(&output).unwrap();
    let index = std::fs::read_to_string(output.join("index.ts")).unwrap();
    let fixture = std::fs::read_to_string(output.join("compile-fixture.ts")).unwrap();
    let globals = std::fs::read_to_string(output.join("globals.d.ts")).unwrap();
    assert!(index.contains("CatalogProviderListResult.js"));
    assert!(index.contains("StoredEvent.js"));
    assert!(fixture.contains("from \"./index.js\""));
    assert!(globals.contains("type ModelSelection"));
    std::fs::remove_dir_all(output).unwrap();
}

#[test]
fn schema_runtime_minimums_are_aligned() {
    let stored_schema = serde_json::to_value(schema_for!(StoredEvent)).unwrap();
    assert_eq!(stored_schema["properties"]["seq"]["minimum"], json!(1));
    let capability_schema = serde_json::to_value(schema_for!(ModelCapabilities)).unwrap();
    assert_eq!(
        capability_schema["properties"]["context_tokens"]["minimum"],
        json!(1)
    );
    let request_schema = serde_json::to_value(schema_for!(RequestDefaults)).unwrap();
    assert_eq!(
        request_schema["properties"]["max_output_tokens"]["anyOf"][0]["minimum"],
        json!(1)
    );
}

#[test]
fn provider_options_are_exact_tagged_adapter_surfaces() {
    let valid = json!({
        "type":"openai-responses",
        "organization":null,
        "project":"project",
        "store":false
    });
    assert!(serde_json::from_value::<ProviderOptions>(valid).is_ok());

    let impossible = json!({
        "type":"openai-responses",
        "organization":null,
        "project":null,
        "store":null,
        "beta":[]
    });
    assert!(serde_json::from_value::<ProviderOptions>(impossible).is_err());

    let removed_internal_fields = json!({
        "type":"openai-responses",
        "organization":null,
        "project":null,
        "store":null,
        "compiled_reasoning":null
    });
    assert!(serde_json::from_value::<ProviderOptions>(removed_internal_fields).is_err());

    let schema = serde_json::to_value(schema_for!(ProviderOptions)).unwrap();
    let schema_text = serde_json::to_string(&schema).unwrap();
    assert!(!schema_text.contains("compiled_reasoning"));
    assert!(!schema_text.contains("compiled_adaptor"));
    assert!(schema_text.contains("openai-responses"));
}

#[test]
fn frozen_provider_options_must_match_resolved_adapter() {
    let mut binding = binding_for("openai/gpt-5.6-sol", Some("high"));
    binding.provider_options = ProviderOptions::Anthropic {
        api_version: None,
        beta: Vec::new(),
    };
    assert!(binding.validate().is_err());
}

#[test]
fn shared_identity_schemas_are_reused_exactly() {
    let shared_model = serde_json::to_value(schema_for!(ProviderModelId)).unwrap();
    let resolved = serde_json::to_value(schema_for!(ResolvedModelRef)).unwrap();
    let projected_model = &resolved["properties"]["model_id"];
    assert_eq!(projected_model["maxLength"], shared_model["maxLength"]);
    assert_eq!(projected_model["pattern"], shared_model["pattern"]);

    let shared_key = serde_json::to_value(schema_for!(ModelKey)).unwrap();
    let selection = &resolved["properties"]["selection"];
    assert_eq!(
        selection["properties"]["model"]["pattern"],
        shared_key["pattern"]
    );
    assert!(
        selection["required"]
            .as_array()
            .unwrap()
            .iter()
            .any(|field| field == "variant")
    );
}

#[test]
fn frozen_nullable_fields_reject_omission() {
    let selection = json!({"agent":"default","model":{"model":"openai/gpt-5.6-sol"}});
    assert!(serde_json::from_value::<RunSelection>(selection).is_err());

    let mut snapshot = serde_json::to_value(agent_snapshot()).unwrap();
    snapshot.as_object_mut().unwrap().remove("delegation");
    assert!(serde_json::from_value::<AgentSnapshot>(snapshot).is_err());

    let mut defaults = serde_json::to_value(RequestDefaults::default()).unwrap();
    defaults.as_object_mut().unwrap().remove("temperature");
    assert!(serde_json::from_value::<RequestDefaults>(defaults).is_err());
}

#[test]
fn collection_schema_bounds_match_runtime_bounds() {
    let defaults = serde_json::to_value(schema_for!(RequestDefaults)).unwrap();
    assert_eq!(defaults["properties"]["stop"]["maxItems"], json!(8));

    let turn = serde_json::to_value(schema_for!(PersistedModelTurn)).unwrap();
    assert_eq!(turn["properties"]["content"]["maxItems"], json!(4096));

    let snapshot = serde_json::to_value(schema_for!(AgentSnapshot)).unwrap();
    assert_eq!(
        snapshot["properties"]["fallback_chain"]["minItems"],
        json!(1)
    );
    assert_eq!(
        snapshot["properties"]["fallback_chain"]["maxItems"],
        json!(256)
    );

    let delegation = &snapshot["$defs"]["FrozenDelegationPolicy"];
    assert_eq!(delegation["properties"]["targets"]["minItems"], json!(1));
    assert_eq!(delegation["properties"]["targets"]["maxItems"], json!(256));
}

#[test]
fn approval_user_final_requires_matching_user_decision() {
    let record = json!({
        "session_id":session_id(),
        "request":approval_request(),
        "status":"approved",
        "internal_decision":{"decision":"escalate","source":"policy","reason_code":"escalated","evaluations":[]},
        "user_decision":null,
        "final_decision":{"outcome":"approved","source":"user","reason_code":"user_approved_once","feedback":null,"tree_grant_id":null}
    });
    assert!(serde_json::from_value::<ApprovalRecord>(record).is_err());
}

#[test]
fn approval_internal_final_forbids_user_decision() {
    let record = json!({
        "session_id":session_id(),
        "request":approval_request(),
        "status":"approved",
        "internal_decision":{"decision":"allow","source":"policy","reason_code":"policy_allowed","evaluations":[]},
        "user_decision":"approve_once",
        "final_decision":{"outcome":"approved","source":"policy","reason_code":"policy_allowed","feedback":null,"tree_grant_id":null}
    });
    assert!(serde_json::from_value::<ApprovalRecord>(record).is_err());
}

#[test]
fn approval_records_reconstruct_every_status_strictly() {
    let records = [
        json!({"status":"pending","internal_decision":null,"user_decision":null,"final_decision":null}),
        json!({"status":"escalated","internal_decision":{"decision":"escalate","source":"policy","reason_code":"escalated","evaluations":[]},"user_decision":null,"final_decision":null}),
        json!({"status":"approved","internal_decision":{"decision":"allow","source":"policy","reason_code":"policy_allowed","evaluations":[]},"user_decision":null,"final_decision":{"outcome":"approved","source":"policy","reason_code":"policy_allowed","feedback":null,"tree_grant_id":null}}),
        json!({"status":"rejected","internal_decision":{"decision":"deny","source":"policy","reason_code":"policy_denied","evaluations":[]},"user_decision":null,"final_decision":{"outcome":"rejected","source":"policy","reason_code":"policy_denied","feedback":null,"tree_grant_id":null}}),
        json!({"status":"cancelled","internal_decision":null,"user_decision":null,"final_decision":{"outcome":"cancelled","source":"system","reason_code":"request_cancelled","feedback":null,"tree_grant_id":null}}),
        json!({"status":"expired","internal_decision":null,"user_decision":null,"final_decision":{"outcome":"expired","source":"system","reason_code":"approval_expired","feedback":null,"tree_grant_id":null}}),
    ];
    for fields in records {
        let mut record = fields.as_object().unwrap().clone();
        record.insert("session_id".into(), json!(session_id()));
        record.insert("request".into(), json!(approval_request()));
        assert!(serde_json::from_value::<ApprovalRecord>(Value::Object(record)).is_ok());
    }

    let invalid_pending = json!({
        "session_id":session_id(),
        "request":approval_request(),
        "status":"pending",
        "internal_decision":{"decision":"allow","source":"policy","reason_code":"policy_allowed","evaluations":[]},
        "user_decision":null,
        "final_decision":null
    });
    assert!(serde_json::from_value::<ApprovalRecord>(invalid_pending).is_err());
}

#[test]
fn checked_in_bindings_match_fresh_generation_byte_for_byte() {
    fn collect(root: &std::path::Path) -> BTreeMap<std::path::PathBuf, Vec<u8>> {
        fn visit(
            root: &std::path::Path,
            path: &std::path::Path,
            files: &mut BTreeMap<std::path::PathBuf, Vec<u8>>,
        ) {
            for entry in std::fs::read_dir(path).unwrap() {
                let entry = entry.unwrap();
                let path = entry.path();
                if path.is_dir() {
                    visit(root, &path, files);
                } else {
                    files.insert(
                        path.strip_prefix(root).unwrap().to_owned(),
                        std::fs::read(path).unwrap(),
                    );
                }
            }
        }
        let mut files = BTreeMap::new();
        visit(root, root, &mut files);
        files
    }

    let fresh = std::env::temp_dir().join(format!("cookie-protocol-bindings-{}", Uuid::now_v7()));
    export_json_schema_set(&fresh.join("json-schema")).unwrap();
    export_typescript_binding_set(&fresh.join("typescript")).unwrap();
    let checked = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("generated");
    assert_eq!(collect(&checked), collect(&fresh));
    std::fs::remove_dir_all(fresh).unwrap();
}

#[test]
fn typescript_optional_fields_match_omittable_json_fields() {
    let request = Request::export_to_string(&typescript_config()).unwrap();
    let catalog = CatalogModelListParams::export_to_string(&typescript_config()).unwrap();
    assert!(request.contains("params?: JsonValue | null"));
    assert!(catalog.contains("provider_id?: CatalogIdentifier | null"));
    let snapshot = AgentSnapshot::export_to_string(&typescript_config()).unwrap();
    assert!(snapshot.contains("delegation: FrozenDelegationPolicy | null"));
    assert!(!snapshot.contains("delegation?:"));
}

#[test]
fn credential_schema_matches_runtime_bounds() {
    let schema = serde_json::to_value(schema_for!(ProviderConnectParams)).unwrap();
    let values = &schema["$defs"]["ProviderCredentials"]["properties"]["values"];
    assert_eq!(values["minProperties"], json!(0));
    assert_eq!(values["maxProperties"], json!(32));
    assert_eq!(
        values["patternProperties"]["^[A-Z0-9_]+$"]["maxLength"],
        json!(16_384)
    );
}

#[test]
fn catalog_snapshot_schema_is_exactly_pinned() {
    let schema = serde_json::to_value(schema_for!(CatalogSnapshot)).unwrap();
    assert_eq!(
        schema["properties"]["revision"]["const"],
        json!(PINNED_CATALOG_REVISION)
    );
    assert_eq!(
        schema["properties"]["source"]["const"],
        json!(PINNED_CATALOG_SOURCE)
    );
    assert_eq!(
        schema["properties"]["fetched_at"]["const"],
        json!(PINNED_CATALOG_FETCHED_AT)
    );
}

#[test]
fn model_list_requires_coherent_snapshot_revisions() {
    let value = json!({"revision":revision("models"),"generated_at":Timestamp::now(),"catalog_revision":catalog_revision(),"models":[]});
    round_trip(serde_json::from_value::<ModelListResult>(value).unwrap());
    assert!(serde_json::from_value::<ModelListResult>(json!({"models":[]})).is_err());
}

#[test]
fn agent_list_requires_model_snapshot_revision() {
    let value = json!({"revision":revision("agents"),"model_revision":revision("models"),"generated_at":Timestamp::now(),"agents":[]});
    round_trip(serde_json::from_value::<AgentListResult>(value).unwrap());
    assert!(
        serde_json::from_value::<AgentListResult>(
            json!({"revision":revision("agents"),"generated_at":Timestamp::now(),"agents":[]})
        )
        .is_err()
    );
}

#[test]
fn catalog_rpc_method_names_are_exact_v7_surfaces() {
    assert_eq!(CATALOG_PROVIDER_LIST_METHOD, "catalog.provider.list");
    assert_eq!(CATALOG_MODEL_LIST_METHOD, "catalog.model.list");
    assert_eq!(MODEL_LIST_METHOD, "model.list");
    assert_eq!(AGENT_LIST_METHOD, "agent.list");
}
