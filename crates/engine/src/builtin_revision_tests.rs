use std::{collections::BTreeMap, sync::Arc};

use cookie_agent_config::AgentRegistry;
use cookie_agent_protocol::{
    AgentMode, ApprovalBoundary, ApprovalCapability, ApprovalConstraints, ApprovalEvaluation,
    ApprovalId, ApprovalResourceSource, ApprovalTrigger, AssistantToolCallRef, DecisionTrace,
    EventPayload, EventSchemaVersion, ModelCallId, PermissionAction, PermissionEffect,
    PreparedApprovalResource, PreparedBindingLifetime, PreparedCapabilityOperation,
    PreparedOperationIdentity, PreparedResourceDigest, PreparedResourceIdentity, RunId, SessionId,
    Sha256Digest, StoredEvent, ToolCallId, ToolCallPresentation, ToolCallStart,
    ToolCallTermination, ToolTerminationOutcome,
};

use super::{
    BOUNDED_SUMMARY_BUILTIN_REVISION, FrozenRunPolicy, InternalAgentKind, InternalAgentRuntime,
    UNAVAILABLE_BUILTIN_REVISION, active_fallback_index, doom_loop_repetitions,
    safe_tool_presentation,
};
use crate::{
    ToolCall,
    policy::{ResultLimits, wire_binding},
    test_support::{
        agent_snapshot, model_binding, model_set, model_snapshot, variant_model_binding,
    },
};

#[test]
fn builtin_revisions_describe_semantic_contracts_not_protocol_versions() {
    assert_eq!(
        BOUNDED_SUMMARY_BUILTIN_REVISION,
        "context-compaction.bounded-summary.prompt-runtime.1"
    );
    assert_eq!(
        UNAVAILABLE_BUILTIN_REVISION,
        "internal-agent.unavailable.runtime.1"
    );
    for revision in [
        BOUNDED_SUMMARY_BUILTIN_REVISION,
        UNAVAILABLE_BUILTIN_REVISION,
    ] {
        assert!(!revision.starts_with('v'));
        assert!(!revision.contains("protocol"));
        assert!(!revision.contains("event-schema"));
    }
}

#[test]
fn tool_presentations_accept_camel_and_snake_file_paths_and_delegate_excerpt() {
    for arguments in [
        serde_json::json!({"filePath":"nested/value.txt"}),
        serde_json::json!({"file_path":"nested/value.txt"}),
    ] {
        let presentation = safe_tool_presentation(&ToolCall {
            id: ToolCallId::new_v7(),
            name: "read".into(),
            arguments,
        });
        assert_eq!(
            presentation.primary_argument.expect("path").as_str(),
            "nested/value.txt"
        );
    }
    let presentation = safe_tool_presentation(&ToolCall {
        id: ToolCallId::new_v7(),
        name: "delegate".into(),
        arguments: serde_json::json!({
            "agent":"worker\nagent",
            "task":format!("inspect token safely {}", "x".repeat(600))
        }),
    });
    let primary = presentation
        .primary_argument
        .expect("delegate presentation");
    assert!(primary.as_str().starts_with("worker agent: <redacted>"));
    assert!(primary.as_str().len() <= cookie_agent_protocol::SafeDisplayText::MAX_BYTES);
}

#[test]
fn every_internal_agent_kind_inherits_only_the_active_variant_suffix_and_prompt() {
    let set = model_set();
    let registry = Arc::new(AgentRegistry::resolve(BTreeMap::new(), &set).expect("empty registry"));
    let base = model_binding();
    let variant = variant_model_binding();
    let mut agent = agent_snapshot("owner", AgentMode::Primary);
    agent.composed_prompt = "Frozen owner prompt.\n".into();
    agent.prompt_fingerprint = Sha256Digest::of_bytes(agent.composed_prompt.as_bytes());
    let owner = FrozenRunPolicy {
        agent: agent.clone(),
        selected_suffix: vec![base, variant.clone()],
        selected_suffix_wire: vec![wire_binding(&variant).expect("wire binding")],
        model_snapshot: model_snapshot(),
        registry,
        result_limits: ResultLimits {
            tool_output_max_lines: 1,
            tool_output_max_bytes: 1,
        },
    };
    let runtime = InternalAgentRuntime::freeze();
    for kind in [
        InternalAgentKind::Approval,
        InternalAgentKind::ContextCompaction,
        InternalAgentKind::SessionTitle,
    ] {
        let inherited = runtime.policy(kind, &owner, &owner.selected_suffix[1..]);
        assert_eq!(inherited.models.as_slice(), std::slice::from_ref(&variant));
        assert_eq!(inherited.agent.composed_prompt, agent.composed_prompt);
        assert_eq!(
            inherited.models[0].resolved.selection.variant,
            variant.resolved.selection.variant
        );
    }
}

fn operation(label: &str) -> PreparedOperationIdentity {
    let digest = PreparedResourceDigest::from_canonical_binding_bytes(label.as_bytes());
    PreparedOperationIdentity::new(
        Sha256Digest::of_bytes(label.as_bytes()),
        vec![ApprovalCapability {
            action: PermissionAction::Read,
            operation: PreparedCapabilityOperation::new("read:read").expect("operation"),
        }],
        vec![PreparedApprovalResource {
            capability: PermissionAction::Read,
            canonical: PreparedResourceIdentity::new(format!(
                "file:{}",
                Sha256Digest::of_bytes(label.as_bytes()).as_str()
            ))
            .expect("identity"),
            binding_digest: digest.clone(),
            binding_lifetime: PreparedBindingLifetime::ProcessLocal,
            boundary: ApprovalBoundary::Exact,
            source: ApprovalResourceSource::PrimaryOperation,
        }],
        Sha256Digest::of_bytes(b"context"),
    )
    .expect("operation")
}

fn approval(label: &str) -> cookie_agent_protocol::ApprovalRequest {
    let operation = operation(label);
    cookie_agent_protocol::ApprovalRequest::new(
        ApprovalId::new_v7(),
        1,
        ApprovalTrigger::PermissionPolicy,
        operation.clone(),
        vec![ApprovalEvaluation {
            resource_digest: operation.resources()[0].binding_digest.clone(),
            effect: PermissionEffect::Ask,
            trace: DecisionTrace {
                action: PermissionAction::Read,
                normalized_resource: label.into(),
                candidates: Vec::new(),
                effect: PermissionEffect::Ask,
                precedence_reason: "test".into(),
            },
        }],
        ApprovalConstraints {
            allow_once: true,
            allow_tree_grant: false,
            cancellable: true,
            expires_at: None,
        },
    )
    .expect("approval")
}

fn stored(run: RunId, seq: u64, payload: EventPayload) -> StoredEvent {
    StoredEvent {
        event_schema_version: EventSchemaVersion::current(),
        session_id: SessionId::new_v7(),
        run_id: Some(run),
        seq,
        timestamp: jiff::Timestamp::new(seq as i64, 0).expect("timestamp"),
        payload,
    }
}

#[test]
fn doom_loop_fires_on_four_and_resets_on_input_or_different_success() {
    let run = RunId::new_v7();
    let request = approval("same");
    let fingerprint = request.operation_fingerprint().clone();
    let mut events = (1..=4)
        .map(|seq| {
            stored(
                run,
                seq,
                EventPayload::ApprovalRequested {
                    request: request.clone(),
                },
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(doom_loop_repetitions(&events[..3], run, &fingerprint), 3);
    assert_eq!(doom_loop_repetitions(&events, run, &fingerprint), 4);

    events.push(stored(
        run,
        5,
        EventPayload::UserInputSubmitted {
            input: "continue".into(),
        },
    ));
    events.push(stored(
        run,
        6,
        EventPayload::ApprovalRequested {
            request: request.clone(),
        },
    ));
    assert_eq!(doom_loop_repetitions(&events, run, &fingerprint), 1);

    let other = operation("different");
    let call = ToolCallId::new_v7();
    let owner = AssistantToolCallRef {
        model_turn_seq: 1,
        content_index: 0,
        model_call_id: ModelCallId::new("different-call").expect("model call"),
        provider_item_id: None,
    };
    events.push(stored(
        run,
        7,
        EventPayload::ToolCallStarted {
            start: ToolCallStart {
                tool_call_id: call,
                owner: owner.clone(),
                presentation: ToolCallPresentation {
                    title: cookie_agent_protocol::SafeDisplayText::new("Read other")
                        .expect("title"),
                    primary_argument: None,
                },
                operation_fingerprint:
                    cookie_agent_protocol::OperationFingerprint::from_prepared_operation(&other),
            },
        },
    ));
    events.push(stored(
        run,
        8,
        EventPayload::ToolCallTerminated {
            termination: ToolCallTermination {
                tool_call_id: call,
                owner,
                outcome: ToolTerminationOutcome::Completed,
                result: Some(cookie_agent_protocol::PersistedToolResult {
                    title: cookie_agent_protocol::SafeDisplayText::new("Read other")
                        .expect("title"),
                    output: String::new(),
                    metadata: serde_json::Value::Null,
                    truncation: None,
                    attachments: Vec::new(),
                }),
                error: None,
            },
        },
    ));
    events.push(stored(run, 9, EventPayload::ApprovalRequested { request }));
    assert_eq!(doom_loop_repetitions(&events, run, &fingerprint), 1);
}

#[test]
fn direct_title_policy_uses_latest_attempt_suffix() {
    let run = RunId::new_v7();
    let binding = variant_model_binding();
    let events = vec![stored(
        run,
        1,
        EventPayload::ModelAttemptStarted {
            attempt_id: cookie_agent_protocol::AttemptId::new_v7(),
            attempt_ordinal: 1,
            fallback_index: 1,
            retry_ordinal: 0,
            resolved_model: crate::model_history::wire_model(&binding),
            prompt_fingerprint: Sha256Digest::of_bytes(b"prompt"),
        },
    )];
    assert_eq!(active_fallback_index(&events, run), 1);
}
