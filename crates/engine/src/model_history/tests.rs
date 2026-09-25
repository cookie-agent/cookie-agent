use std::collections::{BTreeMap, HashSet};

use cookie_agent_protocol::{
    AgentMdEntry, ArtifactReference, AssistantToolCallRef, BoundedDisplayText, ContextCheckpoint,
    ContextCheckpointBoundaries, ContextCheckpointBudgets, ContextCheckpointCommit,
    DelegatedContextRole, DelegatedContextTurn, EventPayload, FrozenModelBinding,
    InternalAgentInvocationId, InternalAgentRunId, InternalSummaryCheckpoint, ModelCallId,
    ModelFinishReason, ModelKey, ModelSelection, NativeContextScope, NativeReplayArtifact,
    OperationFingerprint, PermissionAction, PersistedAssistantPart, PersistedModelTurn,
    PersistedToolResult, PreparedApprovalResource, PreparedBindingLifetime,
    PreparedCapabilityOperation, PreparedOperationIdentity, PreparedResourceDigest,
    PreparedResourceIdentity, ProducerDeliveryMode, ProducerIdempotencyKey, ProducerMessageId,
    ProducerOwner, ProviderId, ReplayDisposition, ResolvedModelRef, RunId, SafeCode,
    SafeDisplayText, SessionId, SessionStatus, Sha256Digest, StoredEvent, SummaryByteLimit,
    ToolCallId, ToolCallPresentation, ToolCallStart, ToolCallTermination, ToolEmittedContent,
    ToolEmittedMessage, ToolEmittedMessageRole, ToolOutputTruncation, ToolTerminationOutcome,
    Usage,
};
use oven_sdk::{
    AdapterId, HistoryTurn, NativeContextScope as OvenNativeContextScope,
    NativeContextWindow as OvenNativeContextWindow, ReplayDecision as OvenReplayDecision,
    ReplayDisposition as OvenReplayDisposition, ResourceId, SystemMessage, SystemPart, TextPart,
};

use crate::goal_projection::GoalProducerProjection;

use super::{
    COMPACTION_SUMMARY_PREFIX, COMPACTION_SUMMARY_SUFFIX, INTERRUPTED_TURN_MARKER,
    TOOL_EMITTED_SYSTEM_USER_MARKER, assemble_full_history, assemble_model_context,
    checkpoint_retained_history, compaction_prefix_history, compaction_tail_candidates,
    framed_compaction_summary, project_summary_context, replay_decisions,
    replay_decisions_with_preflight, restore_replay, tool_output_elision_marker, tool_result_part,
    wire_model,
};

#[test]
fn replay_source_wire_id_round_trips_without_becoming_a_local_selection_key() {
    let mut binding = binding();
    let wire = "vendor/model[wire]:v1";
    binding
        .descriptor
        .provider_metadata
        .insert("cookie_agent.wire_model_id".into(), serde_json::json!(wire));
    let artifact = oven_sdk::NativeReplayArtifact::capture(
        binding.descriptor.adapter_id.clone(),
        OvenNativeContextScope::new(
            binding.descriptor.identity.provider_id.clone(),
            oven_sdk::ModelId::new(wire),
            ResourceId::new("resource").unwrap(),
        )
        .unwrap(),
        serde_json::json!({"opaque":true}),
    )
    .unwrap();
    let directory = tempfile::tempdir().unwrap();
    let store = crate::ArtifactRouter::open_flat(directory.path().join("artifacts")).unwrap();
    let mut finish = oven_sdk::Finish::new(Default::default(), oven_sdk::FinishReason::Stop);
    finish.native_replay = Some(artifact);
    let turn = oven_sdk::CompletedTurn::new(
        oven_sdk::AssistantMessage::new(vec![oven_sdk::AssistantPart::Text(
            oven_sdk::TextPart::new("text"),
        )]),
        finish,
    );
    let (mut persisted, _) =
        super::persist_turn(turn, &store, crate::test_session_id(), &binding).unwrap();
    assert_eq!(
        persisted.native_replay.as_ref().unwrap().scope().model_id,
        binding.selection.model.model_id()
    );
    let (restored, _) = super::restore_turn_with_store(
        &persisted,
        &wire_model(&binding),
        store.for_session(Some(crate::test_session_id())),
        &binding,
    )
    .unwrap();
    assert_eq!(
        restored
            .finish
            .native_replay
            .unwrap()
            .source_wire_model_id()
            .unwrap()
            .as_str(),
        wire
    );
    persisted
        .provider_metadata
        .remove("cookie_agent.replay_source_wire_model_id");
    let (legacy, _) = super::restore_turn_with_store(
        &persisted,
        &wire_model(&binding),
        store.for_session(Some(crate::test_session_id())),
        &binding,
    )
    .unwrap();
    assert!(
        legacy
            .finish
            .native_replay
            .unwrap()
            .source_wire_model_id()
            .is_none()
    );
}

#[test]
fn required_vertex_call_witness_survives_persistence_without_native_artifact() {
    let binding = binding();
    let mut call = oven_sdk::ToolCallPart::new("call", "inspect", serde_json::json!({}));
    oven_sdk::replay::mark_required_vertex_signature(
        &mut call,
        &serde_json::json!({"functionCall":{"name":"inspect","args":{}},"thoughtSignature":"opaque-required-state"}),
    );
    let turn = oven_sdk::CompletedTurn::new(
        oven_sdk::AssistantMessage::new(vec![oven_sdk::AssistantPart::ToolCall(call)]),
        oven_sdk::Finish::new(Default::default(), oven_sdk::FinishReason::ToolCalls),
    );
    let directory = tempfile::tempdir().unwrap();
    let store = crate::ArtifactRouter::open_flat(directory.path().join("artifacts")).unwrap();
    let (persisted, _) =
        super::persist_turn(turn, &store, crate::test_session_id(), &binding).unwrap();
    let encoded = serde_json::to_string(&persisted).unwrap();
    assert!(!encoded.contains("opaque-required-state"));
    let persisted = serde_json::from_str(&encoded).unwrap();
    let (restored, _) = super::restore_turn_with_store(
        &persisted,
        &wire_model(&binding),
        store.for_session(Some(crate::test_session_id())),
        &binding,
    )
    .unwrap();
    assert!(restored.finish.native_replay.is_none());
    assert!(oven_sdk::replay::has_required_vertex_signature(
        &restored.message.content
    ));
}

#[test]
fn invalid_model_identifiers_are_normalized_and_warned() {
    let binding = binding();
    let directory = tempfile::tempdir().unwrap();
    let store = crate::ArtifactRouter::open_flat(directory.path().join("artifacts")).unwrap();
    let turn = oven_sdk::CompletedTurn::new(
        oven_sdk::AssistantMessage::new(vec![
            oven_sdk::AssistantPart::ToolCall(oven_sdk::ToolCallPart::new(
                "call-empty",
                "",
                serde_json::json!({}),
            )),
            oven_sdk::AssistantPart::ToolCall(oven_sdk::ToolCallPart::new(
                "call-invalid",
                "Bad Name",
                serde_json::json!({}),
            )),
            oven_sdk::AssistantPart::ToolCall(oven_sdk::ToolCallPart::new(
                "call-prefixed",
                " Bad Name",
                serde_json::json!({}),
            )),
            oven_sdk::AssistantPart::ToolCall(oven_sdk::ToolCallPart::new(
                "call-valid",
                "inspect",
                serde_json::json!({}),
            )),
            oven_sdk::AssistantPart::Custom(oven_sdk::CustomPart::new(
                "Bad Kind",
                serde_json::json!({}),
            )),
        ]),
        oven_sdk::Finish::new(Default::default(), oven_sdk::FinishReason::ToolCalls),
    );

    let (persisted, warnings) =
        super::persist_turn(turn, &store, crate::test_session_id(), &binding).unwrap();

    let identifiers = persisted
        .content
        .iter()
        .map(|part| match part {
            PersistedAssistantPart::ToolCall { name, .. } => name.as_str().to_owned(),
            PersistedAssistantPart::Custom { kind, .. } => kind.as_str().to_owned(),
            other => panic!("unexpected persisted part: {other:?}"),
        })
        .collect::<Vec<_>>();
    assert_eq!(
        identifiers,
        vec![
            "unnamed-tool",
            "bad_name",
            "x_bad_name",
            "inspect",
            "bad_kind"
        ]
    );
    assert_eq!(warnings.len(), 4);
    assert!(
        warnings
            .iter()
            .all(|warning| warning.as_str().contains("was normalized to"))
    );
    assert!(
        warnings
            .iter()
            .any(|warning| warning.as_str().contains("\"Bad Name\""))
    );
}

#[test]
fn normalize_safe_code_truncates_long_and_multibyte_identifiers() {
    let binding = binding();
    let directory = tempfile::tempdir().unwrap();
    let store = crate::ArtifactRouter::open_flat(directory.path().join("artifacts")).unwrap();
    let long = "a".repeat(SafeCode::MAX_BYTES + 1);
    let multibyte = "工具Name";
    let turn = oven_sdk::CompletedTurn::new(
        oven_sdk::AssistantMessage::new(vec![
            oven_sdk::AssistantPart::ToolCall(oven_sdk::ToolCallPart::new(
                "call-long",
                long.clone(),
                serde_json::json!({}),
            )),
            oven_sdk::AssistantPart::ToolCall(oven_sdk::ToolCallPart::new(
                "call-multibyte",
                multibyte,
                serde_json::json!({}),
            )),
        ]),
        oven_sdk::Finish::new(Default::default(), oven_sdk::FinishReason::ToolCalls),
    );

    let (persisted, warnings) =
        super::persist_turn(turn, &store, crate::test_session_id(), &binding).unwrap();

    let names = persisted
        .content
        .iter()
        .map(|part| match part {
            PersistedAssistantPart::ToolCall { name, .. } => name.as_str().to_owned(),
            other => panic!("unexpected persisted part: {other:?}"),
        })
        .collect::<Vec<_>>();
    assert_eq!(names[0].len(), SafeCode::MAX_BYTES);
    assert!(names[0].bytes().all(|byte| byte == b'a'));
    assert_eq!(names[1], "x______name");
    assert!(
        warnings[0].as_str().contains(&format!("\"{long}\"")),
        "the original provider name must stay quoted in the warning"
    );
    assert!(
        warnings[1].as_str().contains(&format!("\"{multibyte}\"")),
        "multibyte provider names must stay quoted in the warning"
    );
}

#[test]
fn excessive_normalization_warnings_are_deduplicated_and_capped() {
    let binding = binding();
    let directory = tempfile::tempdir().unwrap();
    let store = crate::ArtifactRouter::open_flat(directory.path().join("artifacts")).unwrap();
    let content = (0..300)
        .map(|index| {
            oven_sdk::AssistantPart::ToolCall(oven_sdk::ToolCallPart::new(
                format!("call-{index}"),
                format!("Bad Name {index}"),
                serde_json::json!({}),
            ))
        })
        .collect();
    let turn = oven_sdk::CompletedTurn::new(
        oven_sdk::AssistantMessage::new(content),
        oven_sdk::Finish::new(Default::default(), oven_sdk::FinishReason::ToolCalls),
    );

    let (_, warnings) =
        super::persist_turn(turn, &store, crate::test_session_id(), &binding).unwrap();

    assert_eq!(warnings.len(), 256);
    assert_eq!(
        warnings.last().unwrap().as_str(),
        "additional warnings truncated (45 omitted)"
    );
}

#[test]
fn duplicate_normalization_warnings_collapse_before_capping() {
    let binding = binding();
    let directory = tempfile::tempdir().unwrap();
    let store = crate::ArtifactRouter::open_flat(directory.path().join("artifacts")).unwrap();
    let content = (0..300)
        .map(|index| {
            oven_sdk::AssistantPart::ToolCall(oven_sdk::ToolCallPart::new(
                format!("call-{index}"),
                "",
                serde_json::json!({}),
            ))
        })
        .collect();
    let turn = oven_sdk::CompletedTurn::new(
        oven_sdk::AssistantMessage::new(content),
        oven_sdk::Finish::new(Default::default(), oven_sdk::FinishReason::ToolCalls),
    );

    let (_, warnings) =
        super::persist_turn(turn, &store, crate::test_session_id(), &binding).unwrap();

    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].as_str().contains("unnamed-tool"));
}

#[test]
fn tool_result_materializes_output_and_metadata_as_separate_values() {
    let directory = tempfile::tempdir().unwrap();
    let store = crate::ArtifactRouter::open_flat(directory.path().join("artifacts")).unwrap();
    let session_id = SessionId::new_v7();
    let result = PersistedToolResult {
        display: None,
        retained_output: None,
        title: SafeDisplayText::new("Subagent steered").unwrap(),
        output: format!("Subagent steered. [subagent session {session_id}; running]"),
        metadata: serde_json::json!({"session_id": session_id, "status": "running"}),
        truncation: None,
        attachments: Vec::new(),
        additional_messages: Vec::new(),
    };
    let part =
        tool_result_part(&result, store.for_session(Some(crate::test_session_id()))).unwrap();
    let oven_sdk::ToolContent::Mixed(values) = part.content else {
        panic!("expected mixed tool content");
    };
    assert_eq!(values.len(), 2);
    assert!(matches!(&values[0], oven_sdk::ContentValue::Text(text) if text == &result.output));
    assert!(
        matches!(&values[1], oven_sdk::ContentValue::Json(value) if value == &serde_json::json!({
            "title": result.title,
            "metadata": result.metadata,
            "truncation": null,
        }))
    );
}

#[test]
fn truncated_tool_result_names_the_readback_tool() {
    let directory = tempfile::tempdir().unwrap();
    let store = crate::ArtifactRouter::open_flat(directory.path().join("artifacts")).unwrap();
    let result = PersistedToolResult {
        display: None,
        retained_output: None,
        title: SafeDisplayText::new("Truncated").unwrap(),
        output: "preview".into(),
        metadata: serde_json::Value::Null,
        truncation: Some(ToolOutputTruncation {
            original_bytes: 100,
            original_lines: 10,
            retained: ArtifactReference {
                uri: format!("artifact://sha256/{}", "a".repeat(64)),
            },
        }),
        attachments: Vec::new(),
        additional_messages: Vec::new(),
    };
    let oven_sdk::ToolContent::Mixed(values) =
        tool_result_part(&result, store.for_session(Some(crate::test_session_id())))
            .unwrap()
            .content
    else {
        panic!("expected mixed tool content");
    };
    let oven_sdk::ContentValue::Json(metadata) = &values[1] else {
        panic!("expected tool result metadata");
    };
    assert_eq!(
        metadata["truncation"]["read_more"],
        serde_json::json!({
            "tool": "read",
            "arguments": {"filePath": format!("artifact://{}", "a".repeat(64))}
        })
    );
    assert_eq!(metadata["truncation"]["artifact_id"], "a".repeat(64));
    assert!(!metadata.to_string().contains("artifact://sha256/"));
    assert_eq!(
        result.truncation.as_ref().unwrap().retained.uri,
        format!("artifact://sha256/{}", "a".repeat(64))
    );
}

#[test]
fn compaction_summary_framing_is_byte_stable() {
    assert_eq!(
        COMPACTION_SUMMARY_PREFIX,
        "This session is being continued from a previous conversation that ran out of context. The summary below covers the earlier portion of the conversation.\n\n<summary>\n"
    );
    assert_eq!(
        COMPACTION_SUMMARY_SUFFIX,
        "\n</summary>\n\nPlease continue the conversation from where we left off without asking the user any further questions."
    );
    assert_eq!(
        framed_compaction_summary("state"),
        format!("{COMPACTION_SUMMARY_PREFIX}state{COMPACTION_SUMMARY_SUFFIX}")
    );
}

#[test]
fn checkpoint_accounting_retains_agent_md_and_every_skill_body() {
    let run = RunId::new_v7();
    let skill_event = |seq, name: &str, body: &str| {
        event(
            seq,
            run,
            EventPayload::SkillLoaded {
                name: name.into(),
                rendered_body: body.into(),
                source_path: format!("/{name}/SKILL.md"),
                args: String::new(),
                base_dir: format!("/{name}"),
                supporting_files: Vec::new(),
            },
        )
    };
    let events = vec![
        event(
            1,
            run,
            EventPayload::AgentMdLoaded {
                entries: vec![AgentMdEntry {
                    source: SafeDisplayText::new("/workspace/AGENTS.md").unwrap(),
                    content: "pinned AGENTS.md context".into(),
                    byte_length: 22,
                }],
            },
        ),
        skill_event(2, "one", "first pinned body"),
        skill_event(3, "two", "second pinned body"),
    ];
    let history = vec![
        HistoryTurn::system(SystemMessage::new(vec![SystemPart::Text(TextPart::new(
            "system",
        ))])),
        HistoryTurn::user(super::user_text("pinned AGENTS.md context")),
        HistoryTurn::user(super::user_text(&framed_compaction_summary(
            "stale summary",
        ))),
        HistoryTurn::user(super::user_text("first pinned body")),
        HistoryTurn::user(super::user_text("second pinned body")),
        HistoryTurn::user(super::user_text("discarded conversation")),
    ];
    let native = checkpoint_retained_history(&history, &events, None);
    assert_eq!(native.len(), 4);
    let summarized = checkpoint_retained_history(&history, &events, Some("summary"));
    assert_eq!(summarized.len(), 5);
    let encoded = serde_json::to_string(&summarized).expect("history JSON");
    assert!(encoded.contains("first pinned body"));
    assert!(encoded.contains("second pinned body"));
    assert!(encoded.contains("pinned AGENTS.md context"));
    assert!(encoded.contains("<summary>\\nsummary"));
    assert!(!encoded.contains("stale summary"));
    assert!(!encoded.contains("discarded conversation"));
}

#[test]
fn agent_md_turn_uses_system_reminder_with_contents_blocks() {
    let rendered = super::agent_md_turn(&[AgentMdEntry {
        source: SafeDisplayText::new("/workspace/AGENTS.md").unwrap(),
        content: "project rules".into(),
        byte_length: 13,
    }]);
    assert_eq!(
        rendered,
        "<system-reminder>\nAs you answer the user's questions, you can use the following context:\n# AGENTS.md\nCodebase and user instructions are shown below. Be sure to adhere to these instructions.\nIMPORTANT: These instructions OVERRIDE any default behavior and you MUST follow them exactly as written.\n\n<contents from=\"/workspace/AGENTS.md\">\nproject rules\n</contents>\n\nIMPORTANT: this context may or may not be relevant to your tasks. You should not respond to this context unless it is highly relevant to your task.\n</system-reminder>"
    );
}

#[test]
fn replay_orders_agent_md_then_skills_then_delegated_seed() {
    let run = RunId::new_v7();
    let mut delegated = event(
        1,
        run,
        EventPayload::DelegatedContextSeeded {
            invocation_id: cookie_agent_protocol::InvocationId::new_v7(),
            turns: vec![DelegatedContextTurn {
                role: DelegatedContextRole::User,
                text: "delegated seed".into(),
            }],
        },
    );
    delegated.run_id = None;
    let events = vec![
        delegated,
        event(
            2,
            run,
            EventPayload::SkillLoaded {
                name: "ordered-skill".into(),
                rendered_body: "loaded skill body".into(),
                source_path: "/skills/ordered-skill/SKILL.md".into(),
                args: String::new(),
                base_dir: "/skills/ordered-skill".into(),
                supporting_files: Vec::new(),
            },
        ),
        event(
            3,
            run,
            EventPayload::AgentMdLoaded {
                entries: vec![AgentMdEntry {
                    source: SafeDisplayText::new("/workspace/AGENTS.md").unwrap(),
                    content: "AGENTS.md context body".into(),
                    byte_length: 20,
                }],
            },
        ),
    ];
    let directory = tempfile::tempdir().unwrap();
    let store = crate::ArtifactRouter::open_flat(directory.path().join("artifacts")).unwrap();
    let history = assemble_full_history(&events, &store, &binding(), "system prompt").unwrap();
    assert_eq!(history.len(), 4);
    let rendered = history
        .iter()
        .map(|turn| serde_json::to_string(turn).unwrap())
        .collect::<Vec<_>>();
    assert!(rendered[0].contains("system prompt"));
    assert!(rendered[1].contains("AGENTS.md context body"));
    assert!(rendered[2].contains("loaded skill body"));
    assert!(rendered[3].contains("delegated seed"));
}

#[test]
fn tool_elision_marker_is_stable_and_contains_the_bare_artifact_id() {
    let artifact_id = "a".repeat(64);
    assert_eq!(
        tool_output_elision_marker(&artifact_id, 12_345, 0),
        "[tool output elided; retained at artifact://aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa; 12345 bytes]"
    );
    assert_eq!(
        tool_output_elision_marker(&artifact_id, 12_345, 2),
        "[tool output elided; retained at artifact://aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa; 12345 bytes] 2 tool-emitted message(s) were elided with this result and are not recoverable."
    );
}
use crate::test_support::{model_binding as binding, model_binding_named, variant_model_binding};

fn event(seq: u64, run: RunId, payload: EventPayload) -> StoredEvent {
    StoredEvent {
        engine_version: None,
        origin: None,
        session_id: SessionId(uuid::Uuid::from_u128(1)),
        run_id: Some(run),
        seq,
        timestamp: jiff::Timestamp::new(seq as i64, 0).expect("timestamp"),
        payload,
    }
}

fn producer_accepted_event(
    seq: u64,
    message_id: ProducerMessageId,
    owner: ProducerOwner,
    key: &str,
    body: &str,
) -> StoredEvent {
    let mut event = event(
        seq,
        RunId::new_v7(),
        EventPayload::ProducerMessageAccepted {
            message_id,
            producer_owner: owner,
            mode: ProducerDeliveryMode::Steer,
            idempotency_key: ProducerIdempotencyKey::new(key).unwrap(),
            description: cookie_agent_protocol::SafeDisplayText::new("Producer result").unwrap(),
            body: body.into(),
            reminder: None,
            agent_hop: None,
        },
    );
    event.run_id = None;
    event
}

fn run_started_event(seq: u64, run: RunId, binding: &FrozenModelBinding) -> StoredEvent {
    let agent =
        crate::test_support::agent_snapshot("test", cookie_agent_protocol::AgentMode::Primary);
    let revision = format!("sha256:{}", "0".repeat(64));
    event(
        seq,
        run,
        EventPayload::RunStarted {
            client_run_id: cookie_agent_protocol::ClientRunId::new(format!("run-{seq}")).unwrap(),
            selection: crate::test_support::run_selection("test"),
            runtime_revision: cookie_agent_protocol::RuntimeRevision::new(revision.clone())
                .unwrap(),
            catalog_revision: cookie_agent_protocol::CatalogRevision::new(revision.clone())
                .unwrap(),
            provider_state_revision: cookie_agent_protocol::ProviderStateRevision::new(
                revision.clone(),
            )
            .unwrap(),
            model_revision: cookie_agent_protocol::ModelRevision::new(revision.clone()).unwrap(),
            agent_revision: cookie_agent_protocol::AgentRevision::new(revision.clone()).unwrap(),
            recipe_registry_revision: cookie_agent_protocol::RecipeRegistryRevision::new(revision)
                .unwrap(),
            manifest_revision: binding.manifest_revision.clone(),
            selected_suffix: agent.fallback_chain.clone(),
            internal_agents: Vec::new(),
            agent: Box::new(agent),
            input_through_seq: seq,
        },
    )
}

#[test]
fn producer_body_materializes_only_at_the_effective_admission() {
    let message_id = ProducerMessageId::new_v7();
    let first_run = RunId::new_v7();
    let retry_run = RunId::new_v7();
    let accepted = producer_accepted_event(
        1,
        message_id,
        ProducerOwner::Plugin {
            plugin: "jobs".into(),
        },
        "job:7",
        "durable producer result",
    );
    let directory = tempfile::tempdir().unwrap();
    let store = crate::ArtifactRouter::open_flat(directory.path().join("artifacts")).unwrap();
    let current_binding = binding();

    let accepted_history = assemble_full_history(
        std::slice::from_ref(&accepted),
        &store,
        &current_binding,
        "system",
    )
    .unwrap();
    assert_eq!(accepted_history.len(), 1);

    let events = vec![
        accepted,
        event(
            2,
            first_run,
            EventPayload::ProducerMessageAdmitted { message_id },
        ),
        event(3, first_run, EventPayload::RunInterrupted { reason: None }),
        event(
            4,
            retry_run,
            EventPayload::ProducerMessageAdmitted { message_id },
        ),
    ];
    let history = assemble_full_history(&events, &store, &current_binding, "system").unwrap();
    assert_eq!(history.len(), 2);
    let rendered = serde_json::to_string(&history).unwrap();
    assert_eq!(rendered.matches("durable producer result").count(), 1);
}

#[test]
fn pending_producer_body_requires_admission_to_the_current_run() {
    let message_id = ProducerMessageId::new_v7();
    let run_a = RunId::new_v7();
    let run_b = RunId::new_v7();
    let current_binding = binding();
    let mut events = vec![
        producer_accepted_event(
            1,
            message_id,
            ProducerOwner::Plugin {
                plugin: "jobs".into(),
            },
            "job:run-boundary",
            "pending producer body",
        ),
        run_started_event(2, run_a, &current_binding),
        event(
            3,
            run_a,
            EventPayload::ProducerMessageAdmitted { message_id },
        ),
        event(4, run_a, EventPayload::RunCancelled { reason: None }),
        run_started_event(5, run_b, &current_binding),
    ];
    let directory = tempfile::tempdir().unwrap();
    let store = crate::ArtifactRouter::open_flat(directory.path().join("artifacts")).unwrap();

    let old_admission_history =
        assemble_full_history(&events, &store, &current_binding, "system").unwrap();
    assert!(
        !serde_json::to_string(&old_admission_history)
            .unwrap()
            .contains("pending producer body")
    );

    events.push(event(
        6,
        run_b,
        EventPayload::ProducerMessageAdmitted { message_id },
    ));
    let readmitted_history =
        assemble_full_history(&events, &store, &current_binding, "system").unwrap();
    assert_eq!(
        serde_json::to_string(&readmitted_history)
            .unwrap()
            .matches("pending producer body")
            .count(),
        1
    );
    let projection = crate::goal_projection::GoalProducerProjection::from_events(&events);
    assert_eq!(projection.messages[0].admission, Some((run_b, 6)));
    assert!(!projection.messages[0].consumed);
}

#[test]
fn discarded_producer_body_is_absent_from_history() {
    let message_id = ProducerMessageId::new_v7();
    let run = RunId::new_v7();
    let owner = ProducerOwner::Plugin {
        plugin: "jobs".into(),
    };
    let accepted = producer_accepted_event(
        1,
        message_id,
        owner.clone(),
        "job:discarded",
        "discarded producer body",
    );
    let mut discarded = event(
        3,
        run,
        EventPayload::ProducerMessageDiscarded {
            message_id,
            reminder: None,
            producer_owner: Some(owner),
        },
    );
    discarded.run_id = None;
    let events = vec![
        accepted,
        event(2, run, EventPayload::ProducerMessageAdmitted { message_id }),
        discarded,
    ];
    let directory = tempfile::tempdir().unwrap();
    let store = crate::ArtifactRouter::open_flat(directory.path().join("artifacts")).unwrap();

    let history = assemble_full_history(&events, &store, &binding(), "system").unwrap();
    assert!(
        !serde_json::to_string(&history)
            .unwrap()
            .contains("discarded producer body")
    );
}

#[test]
fn claimed_snapshot_retains_covered_producer_input() {
    let message_id = ProducerMessageId::new_v7();
    let run = RunId::new_v7();
    let current_binding = binding();
    let events = vec![
        producer_accepted_event(
            1,
            message_id,
            ProducerOwner::Plugin {
                plugin: "jobs".into(),
            },
            "job:claimed",
            "claimed producer body",
        ),
        event(2, run, EventPayload::ProducerMessageAdmitted { message_id }),
        event(
            3,
            run,
            EventPayload::ProducerMessagesClaimed {
                message_ids: vec![message_id],
            },
        ),
        event(
            4,
            run,
            EventPayload::ModelTurnCommitted {
                attempt_id: cookie_agent_protocol::AttemptId::new_v7(),
                model_turn_seq: 1,
                resolved_model: wire_model(&current_binding),
                input_through_seq: 2,
                turn: PersistedModelTurn {
                    content: Vec::new(),
                    provider_options: BTreeMap::new(),
                    finish_reason: ModelFinishReason::Stop,
                    usage: Usage::default(),
                    response_metadata: BTreeMap::new(),
                    provider_metadata: BTreeMap::new(),
                    native_replay: None,
                },
                warnings: Vec::new(),
            },
        ),
    ];
    let directory = tempfile::tempdir().unwrap();
    let store = crate::ArtifactRouter::open_flat(directory.path().join("artifacts")).unwrap();

    let history = assemble_full_history(&events, &store, &current_binding, "system").unwrap();
    assert!(
        serde_json::to_string(&history)
            .unwrap()
            .contains("claimed producer body")
    );
    let projection = crate::goal_projection::GoalProducerProjection::from_events(&events);
    assert!(projection.messages[0].consumed);
    assert_eq!(
        projection.messages[0].claims,
        std::collections::HashSet::from([3])
    );
    assert_eq!(projection.claims[&3].message_ids, vec![message_id]);
}

#[test]
fn delegation_v2_notification_is_suppressed_only_for_producer_owned_invocation() {
    let producer_invocation = cookie_agent_protocol::InvocationId::new_v7();
    let legacy_invocation = cookie_agent_protocol::InvocationId::new_v7();
    let child = SessionId::new_v7();
    let events = vec![
        producer_accepted_event(
            1,
            ProducerMessageId::new_v7(),
            ProducerOwner::Delegation {
                invocation_id: producer_invocation,
            },
            "delegate-result",
            "producer result",
        ),
        event(
            2,
            RunId::new_v7(),
            EventPayload::DelegateFinishedV2 {
                short_id: None,
                invocation_id: producer_invocation,
                session_id: child,
                status: SessionStatus::Completed,
                preview: "duplicate".into(),
                total_lines: 1,
            },
        ),
        event(
            3,
            RunId::new_v7(),
            EventPayload::DelegateFinishedV2 {
                short_id: Some("explore_1a2b3c4d".into()),
                invocation_id: legacy_invocation,
                session_id: child,
                status: SessionStatus::Completed,
                preview: "legacy result".into(),
                total_lines: 1,
            },
        ),
    ];
    let directory = tempfile::tempdir().unwrap();
    let store = crate::ArtifactRouter::open_flat(directory.path().join("artifacts")).unwrap();
    let history = assemble_full_history(&events, &store, &binding(), "system").unwrap();
    let rendered = serde_json::to_string(&history).unwrap();
    assert!(!rendered.contains("duplicate"));
    // Replay and the runtime renderer share one body: the handle-form
    // notification, byte for byte.
    let expected = crate::runtime::render_subagent_notification(
        "legacy result",
        SessionStatus::Completed,
        1,
        "explore_1a2b3c4d",
    );
    assert!(rendered.contains(&serde_json::to_string(&expected).unwrap()));
}

#[test]
fn compaction_preserves_goal_projection_and_unconsumed_producer_input() {
    let run = RunId::new_v7();
    let goal_id = cookie_agent_protocol::GoalId::new_v7();
    let message_id = ProducerMessageId::new_v7();
    let items = vec![cookie_agent_protocol::GoalItem {
        description: "Verify retained evidence".into(),
        finished: false,
    }];
    let events = vec![
        event(
            1,
            run,
            EventPayload::GoalActivated {
                goal_id,
                objective: "Keep the durable goal".into(),
                revision: 0,
                selection: None,
            },
        ),
        event(
            2,
            run,
            EventPayload::GoalChecklistRevised {
                goal_id,
                items: items.clone(),
                revision: 1,
            },
        ),
        producer_accepted_event(
            3,
            message_id,
            ProducerOwner::Plugin {
                plugin: "jobs".into(),
            },
            "pending-before-checkpoint",
            "Exact unconsumed producer evidence",
        ),
        event(4, run, EventPayload::ProducerMessageAdmitted { message_id }),
        event(
            5,
            run,
            EventPayload::ContextCheckpointCommitted {
                commit: summary_commit(
                    "Summary does not contain the goal or evidence",
                    1,
                    4,
                    None,
                    None,
                ),
            },
        ),
    ];
    let directory = tempfile::tempdir().unwrap();
    let store = crate::ArtifactRouter::open_flat(directory.path().join("artifacts")).unwrap();
    let context = assemble_model_context(&events, &store, &binding(), "frozen system").unwrap();
    let rendered = serde_json::to_string(&context.history).unwrap();
    assert!(rendered.contains("Summary does not contain"));
    assert_eq!(
        rendered
            .matches("Exact unconsumed producer evidence")
            .count(),
        1
    );
    let projection = crate::goal_projection::GoalProducerProjection::from_events(&events);
    let goal = projection.goal.unwrap();
    assert_eq!(goal.goal_id, goal_id);
    assert_eq!(goal.objective, "Keep the durable goal");
    assert_eq!(goal.items, items);
    assert_eq!(goal.revision, 1);
    assert!(
        !projection.messages[0].consumed,
        "a checkpoint is not committed model input coverage"
    );
}

fn user_events(seq: u64, run: RunId, input: &str) -> [StoredEvent; 2] {
    [
        event(
            seq,
            run,
            EventPayload::UserInputSubmitted {
                input: input.into(),
            },
        ),
        event(
            seq + 1,
            run,
            EventPayload::UserInputApplied {
                user_input_seq: seq,
            },
        ),
    ]
}

fn summary_commit(
    summary: &str,
    source_from_seq: u64,
    source_through_seq: u64,
    recent_from_seq: Option<u64>,
    prior_checkpoint_seq: Option<u64>,
) -> ContextCheckpointCommit {
    let max_summary_bytes = SummaryByteLimit::new(1024).expect("limit");
    ContextCheckpointCommit {
        checkpoint: ContextCheckpoint::InternalSummary {
            checkpoint: InternalSummaryCheckpoint::new(
                summary.into(),
                InternalAgentInvocationId::new_v7(),
                InternalAgentRunId::new_v7(),
                max_summary_bytes,
            )
            .expect("checkpoint"),
        },
        boundaries: ContextCheckpointBoundaries {
            source_from_seq,
            source_through_seq,
            recent_from_seq,
            input_through_seq: source_through_seq,
            prior_checkpoint_seq,
        },
        budgets: ContextCheckpointBudgets {
            context_limit_tokens: 100,
            trigger_tokens: 70,
            input_tokens_before: 60,
            input_tokens_after: 20,
            keep_recent_tokens: u64::from(recent_from_seq.is_some()) * 10,
            max_summary_bytes,
        },
    }
}

fn replay_turn_event(binding: &FrozenModelBinding) -> StoredEvent {
    let resolved = wire_model(binding);
    let artifact = NativeReplayArtifact::new(
        SafeCode::new(binding.descriptor.adapter_id.as_str()).expect("adapter id"),
        resolved.selection_fingerprint.clone(),
        NativeContextScope {
            provider_id: resolved.provider_id.clone(),
            model_id: resolved.model_id.clone(),
            resource_id: SafeDisplayText::new("resource").expect("resource"),
        },
        serde_json::json!({"opaque": true}),
    )
    .expect("artifact");
    event(
        1,
        RunId::new_v7(),
        EventPayload::ModelTurnCommitted {
            attempt_id: cookie_agent_protocol::AttemptId::new_v7(),
            model_turn_seq: 1,
            resolved_model: resolved,
            input_through_seq: 1,
            turn: PersistedModelTurn {
                content: vec![
                    PersistedAssistantPart::Reasoning {
                        text: "historical reasoning".into(),
                        metadata: None,
                    },
                    PersistedAssistantPart::Text {
                        text: "answer".into(),
                        metadata: None,
                    },
                ],
                provider_options: BTreeMap::new(),
                finish_reason: ModelFinishReason::Stop,
                usage: Usage::default(),
                response_metadata: BTreeMap::new(),
                provider_metadata: BTreeMap::new(),
                native_replay: Some(artifact),
            },
            warnings: Vec::new(),
        },
    )
}

fn reasoning_replay_binding(model_id: &str) -> FrozenModelBinding {
    let mut binding = model_binding_named(model_id);
    binding
        .descriptor
        .capabilities
        .features
        .insert(oven_sdk::Capability::REASONING);
    binding.descriptor.capabilities.replay.reasoning = true;
    binding
}

fn switched_context(current: &FrozenModelBinding) -> super::ModelContext {
    let directory = tempfile::tempdir().expect("tempdir");
    let store =
        crate::ArtifactRouter::open_flat(directory.path().join("artifacts")).expect("store");
    let original = binding();
    assemble_model_context(
        &[replay_turn_event(&original)],
        &store,
        current,
        "System prompt.",
    )
    .expect("switched context")
}

fn context_has_reasoning(context: &super::ModelContext) -> bool {
    let HistoryTurn::Assistant(turn) = &context.history[1] else {
        panic!("assistant turn");
    };
    turn.message
        .content
        .iter()
        .any(|part| matches!(part, oven_sdk::AssistantPart::Reasoning(_)))
}

fn operation() -> PreparedOperationIdentity {
    let digest = PreparedResourceDigest::from_canonical_binding_bytes(b"binding");
    PreparedOperationIdentity::new(
        Sha256Digest::of_bytes(b"args"),
        vec![cookie_agent_protocol::ApprovalCapability {
            action: PermissionAction::Read,
            operation: PreparedCapabilityOperation::new("read:read").expect("operation"),
        }],
        vec![PreparedApprovalResource {
            capability: PermissionAction::Read,
            canonical: PreparedResourceIdentity::new("file:readme").expect("identity"),
            binding_digest: digest,
            binding_lifetime: PreparedBindingLifetime::ProcessLocal,
            boundary: cookie_agent_protocol::ApprovalBoundary::Exact,
            source: cookie_agent_protocol::ApprovalResourceSource::PrimaryOperation,
        }],
        Sha256Digest::of_bytes(b"context"),
    )
    .expect("prepared operation")
}

#[test]
fn normalized_history_keeps_native_artifacts_for_adapter_scoped_replay() {
    let directory = tempfile::tempdir().expect("tempdir");
    let store =
        crate::ArtifactRouter::open_flat(directory.path().join("artifacts")).expect("store");
    let binding = binding();
    let resolved = wire_model(&binding);
    let artifact = NativeReplayArtifact::new(
        SafeCode::new(binding.descriptor.adapter_id.as_str()).expect("adapter id"),
        resolved.selection_fingerprint.clone(),
        NativeContextScope {
            provider_id: resolved.provider_id.clone(),
            model_id: resolved.model_id.clone(),
            resource_id: SafeDisplayText::new("resource").expect("resource"),
        },
        serde_json::json!({"opaque": true}),
    )
    .expect("artifact");
    let turn = PersistedModelTurn {
        content: vec![PersistedAssistantPart::Text {
            text: "answer".into(),
            metadata: None,
        }],
        provider_options: BTreeMap::new(),
        finish_reason: ModelFinishReason::Stop,
        usage: Usage::default(),
        response_metadata: BTreeMap::new(),
        provider_metadata: BTreeMap::new(),
        native_replay: Some(artifact),
    };
    let run = RunId::new_v7();
    let events = vec![event(
        1,
        run,
        EventPayload::ModelTurnCommitted {
            attempt_id: cookie_agent_protocol::AttemptId::new_v7(),
            model_turn_seq: 1,
            resolved_model: resolved,
            input_through_seq: 1,
            turn,
            warnings: Vec::new(),
        },
    )];
    let history =
        assemble_full_history(&events, &store, &binding, "System prompt.").expect("history");
    let oven_sdk::HistoryTurn::Assistant(turn) = &history[1] else {
        panic!("assistant turn");
    };
    let replay = turn.finish.native_replay.as_ref().expect("native replay");
    assert_eq!(
        replay.adapter_id().as_str(),
        binding.descriptor.adapter_id.as_str()
    );
    assert_eq!(replay.scope().resource_id.as_str(), "resource");
}

#[test]
fn native_replay_is_reused_across_variants_with_the_same_protocol() {
    let directory = tempfile::tempdir().expect("tempdir");
    let store =
        crate::ArtifactRouter::open_flat(directory.path().join("artifacts")).expect("store");
    let base = binding();
    let resolved = wire_model(&base);
    let artifact = NativeReplayArtifact::new(
        SafeCode::new(base.descriptor.adapter_id.as_str()).expect("adapter id"),
        resolved.selection_fingerprint,
        NativeContextScope {
            provider_id: resolved.provider_id,
            model_id: resolved.model_id,
            resource_id: SafeDisplayText::new("resource").expect("resource"),
        },
        serde_json::json!({"opaque": true}),
    )
    .expect("artifact");
    let run = RunId::new_v7();
    let events = vec![event(
        1,
        run,
        EventPayload::ModelTurnCommitted {
            attempt_id: cookie_agent_protocol::AttemptId::new_v7(),
            model_turn_seq: 1,
            resolved_model: wire_model(&base),
            input_through_seq: 1,
            turn: PersistedModelTurn {
                content: vec![PersistedAssistantPart::Text {
                    text: "answer".into(),
                    metadata: None,
                }],
                provider_options: BTreeMap::new(),
                finish_reason: ModelFinishReason::Stop,
                usage: Usage::default(),
                response_metadata: BTreeMap::new(),
                provider_metadata: BTreeMap::new(),
                native_replay: Some(artifact),
            },
            warnings: Vec::new(),
        },
    )];
    let variant = variant_model_binding();
    assert_ne!(base.selection_fingerprint, variant.selection_fingerprint);
    assert_eq!(base.selection.model, variant.selection.model);
    let context = assemble_model_context(&events, &store, &variant, "System prompt.")
        .expect("same-protocol variant reuses native history");
    let oven_sdk::HistoryTurn::Assistant(turn) = &context.history[1] else {
        panic!("assistant turn");
    };
    let replay = turn.finish.native_replay.as_ref().expect("native replay");
    assert_eq!(
        replay.scope().model_id,
        variant.descriptor.identity.model_id
    );
    assert!(context.replay_decisions.is_empty());
    let merged = replay_decisions_with_preflight(
        &[OvenReplayDecision {
            history_index: 1,
            disposition: OvenReplayDisposition::Replayed,
        }],
        &variant,
        &context.replay_decisions,
    );
    assert!(matches!(
        merged.as_slice(),
        [cookie_agent_protocol::ReplayDecision {
            disposition: ReplayDisposition::Replayed,
            ..
        }]
    ));
}

#[test]
fn native_replay_with_adapter_mismatching_persisted_turn_is_discarded() {
    let directory = tempfile::tempdir().expect("tempdir");
    let store =
        crate::ArtifactRouter::open_flat(directory.path().join("artifacts")).expect("store");
    let base = binding();
    let resolved = wire_model(&base);
    // The artifact matches the CURRENT binding's adapter, but the
    // persisted turn was recorded under a different adapter: the payload
    // must never be exposed to an adapter other than the recorded one.
    let artifact = NativeReplayArtifact::new(
        SafeCode::new(base.descriptor.adapter_id.as_str()).expect("adapter id"),
        resolved.selection_fingerprint.clone(),
        NativeContextScope {
            provider_id: resolved.provider_id.clone(),
            model_id: resolved.model_id.clone(),
            resource_id: SafeDisplayText::new("resource").expect("resource"),
        },
        serde_json::json!({"opaque": true}),
    )
    .expect("artifact");
    let mut persisted_resolved = resolved;
    persisted_resolved.adapter_id = cookie_agent_protocol::AdaptorId::Anthropic;
    let events = vec![event(
        1,
        RunId::new_v7(),
        EventPayload::ModelTurnCommitted {
            attempt_id: cookie_agent_protocol::AttemptId::new_v7(),
            model_turn_seq: 1,
            resolved_model: persisted_resolved,
            input_through_seq: 1,
            turn: PersistedModelTurn {
                content: vec![PersistedAssistantPart::Text {
                    text: "answer".into(),
                    metadata: None,
                }],
                provider_options: BTreeMap::new(),
                finish_reason: ModelFinishReason::Stop,
                usage: Usage::default(),
                response_metadata: BTreeMap::new(),
                provider_metadata: BTreeMap::new(),
                native_replay: Some(artifact),
            },
            warnings: Vec::new(),
        },
    )];
    let context = assemble_model_context(&events, &store, &base, "System prompt.")
        .expect("mismatched persisted adapter discards artifact");
    let oven_sdk::HistoryTurn::Assistant(turn) = &context.history[1] else {
        panic!("assistant turn");
    };
    assert!(turn.finish.native_replay.is_none());
    assert!(matches!(
        context.replay_decisions.as_slice(),
        [cookie_agent_protocol::ReplayDecision {
            disposition: ReplayDisposition::DiscardedInvalidPayload { .. },
            ..
        }]
    ));
}

#[test]
fn model_switch_to_reasoning_replay_target_keeps_normalized_reasoning() {
    let context = switched_context(&reasoning_replay_binding("fallback-zero"));
    let HistoryTurn::Assistant(turn) = &context.history[1] else {
        panic!("assistant turn");
    };
    assert!(turn.finish.native_replay.is_some());
    assert!(context_has_reasoning(&context));
    assert!(context.replay_decisions.is_empty());
}

#[test]
fn model_switch_defers_native_block_filtering_to_target_codec() {
    let context = switched_context(&model_binding_named("fallback-zero"));
    let HistoryTurn::Assistant(turn) = &context.history[1] else {
        panic!("assistant turn");
    };
    assert!(turn.finish.native_replay.is_some());
    assert!(context_has_reasoning(&context));
    assert!(
        turn.message
            .content
            .iter()
            .any(|part| matches!(part, oven_sdk::AssistantPart::Text(_)))
    );
}

#[test]
fn cross_protocol_reasoning_replay_target_keeps_normalized_reasoning() {
    let mut current = reasoning_replay_binding("fallback-zero");
    current.descriptor.adapter_id = AdapterId::new("anthropic");
    let context = switched_context(&current);
    assert!(context_has_reasoning(&context));
    assert!(context.replay_decisions.is_empty());
}

#[test]
fn cross_protocol_target_receives_complete_source_for_payload_validation() {
    let mut current = model_binding_named("fallback-zero");
    current.descriptor.adapter_id = AdapterId::new("other-protocol");
    let context = switched_context(&current);
    assert!(context_has_reasoning(&context));
    assert!(context.replay_decisions.is_empty());
}

#[test]
fn cross_provider_reasoning_replay_target_keeps_normalized_reasoning() {
    let mut current = reasoning_replay_binding("fallback-zero");
    let provider_id = ProviderId::new("other").expect("provider");
    current.selection.model = ModelKey::new(
        provider_id.clone(),
        current.selection.model.model_id().clone(),
    )
    .expect("model key");
    current.descriptor.identity.provider_id = oven_sdk::ProviderId::new(provider_id.as_str());
    let context = switched_context(&current);
    assert!(context_has_reasoning(&context));
    assert!(context.replay_decisions.is_empty());
}

#[test]
fn cross_provider_non_reasoning_target_defers_filtering_to_codec() {
    let mut current = model_binding_named("fallback-zero");
    let provider_id = ProviderId::new("other").expect("provider");
    current.selection.model = ModelKey::new(
        provider_id.clone(),
        current.selection.model.model_id().clone(),
    )
    .expect("model key");
    current.descriptor.identity.provider_id = oven_sdk::ProviderId::new(provider_id.as_str());
    let context = switched_context(&current);
    assert!(context_has_reasoning(&context));
    assert!(context.replay_decisions.is_empty());
}

#[test]
fn artifact_with_invalid_source_adapter_attribution_is_discarded() {
    let binding = binding();
    let resolved = wire_model(&binding);
    let artifact = NativeReplayArtifact::new(
        SafeCode::new("vendor.custom-adapter.v2").expect("adapter"),
        resolved.selection_fingerprint.clone(),
        NativeContextScope {
            provider_id: resolved.provider_id.clone(),
            model_id: resolved.model_id.clone(),
            resource_id: SafeDisplayText::new("resource").expect("resource"),
        },
        serde_json::json!({"opaque": true}),
    )
    .expect("artifact");
    let (restored, disposition) = restore_replay(&artifact, &resolved, &binding, None);
    assert!(restored.is_none());
    assert!(matches!(
        disposition,
        Some(ReplayDisposition::DiscardedInvalidPayload { .. })
    ));
}

#[test]
fn identical_foreign_dispositions_on_distinct_history_entries_are_preserved() {
    let binding = binding();
    let foreign = ReplayDisposition::DiscardedForeignAdapter {
        found: SafeCode::new("anthropic").expect("adapter"),
        expected: SafeCode::new(binding.descriptor.adapter_id.as_str()).expect("adapter"),
    };
    let preflight = vec![
        cookie_agent_protocol::ReplayDecision {
            history_index: 1,
            disposition: foreign.clone(),
        },
        cookie_agent_protocol::ReplayDecision {
            history_index: 3,
            disposition: foreign,
        },
    ];
    let merged = replay_decisions_with_preflight(
        &[
            OvenReplayDecision {
                history_index: 1,
                disposition: OvenReplayDisposition::NoArtifact,
            },
            OvenReplayDecision {
                history_index: 1,
                disposition: OvenReplayDisposition::ReconstructedNormalized,
            },
            OvenReplayDecision {
                history_index: 3,
                disposition: OvenReplayDisposition::NoArtifact,
            },
            OvenReplayDecision {
                history_index: 3,
                disposition: OvenReplayDisposition::ReconstructedNormalized,
            },
        ],
        &binding,
        &preflight,
    );
    assert!(matches!(
        merged.as_slice(),
        [
            cookie_agent_protocol::ReplayDecision {
                history_index: 1,
                disposition: ReplayDisposition::DiscardedForeignAdapter { .. },
            },
            cookie_agent_protocol::ReplayDecision {
                history_index: 1,
                disposition: ReplayDisposition::ReconstructedNormalizedHistory,
            },
            cookie_agent_protocol::ReplayDecision {
                history_index: 3,
                disposition: ReplayDisposition::DiscardedForeignAdapter { .. },
            },
            cookie_agent_protocol::ReplayDecision {
                history_index: 3,
                disposition: ReplayDisposition::ReconstructedNormalizedHistory,
            },
        ]
    ));
}

#[test]
fn cross_provider_native_replay_is_restored_for_target_validation() {
    let binding = binding();
    let current = wire_model(&binding);
    let provider_id = ProviderId::new("other").expect("provider");
    let model_id = current.model_id.clone();
    let selection = ModelSelection {
        model: ModelKey::new(provider_id.clone(), model_id.clone()).expect("model key"),
        variant: None,
    };
    let found = ResolvedModelRef {
        selection: selection.clone(),
        provider_id: provider_id.clone(),
        model_id: model_id.clone(),
        adapter_id: current.adapter_id,
        selection_fingerprint: Sha256Digest::of_bytes(b"foreign selection"),
    };
    let artifact = NativeReplayArtifact::new(
        SafeCode::new(binding.descriptor.adapter_id.as_str()).expect("adapter"),
        found.selection_fingerprint.clone(),
        NativeContextScope {
            provider_id,
            model_id,
            resource_id: SafeDisplayText::new("resource").expect("resource"),
        },
        serde_json::json!({"opaque": true}),
    )
    .expect("artifact");
    let (restored, disposition) = restore_replay(&artifact, &found, &binding, None);
    assert!(restored.is_some());
    assert!(restored.unwrap().source_wire_model_id().is_none());
    assert!(disposition.is_none());
}

#[test]
fn invalid_same_selection_payload_discards_and_reconstructs() {
    let binding = binding();
    let resolved = wire_model(&binding);
    let artifact = NativeReplayArtifact::new(
        SafeCode::new(binding.descriptor.adapter_id.as_str()).expect("adapter"),
        resolved.selection_fingerprint.clone(),
        NativeContextScope {
            provider_id: resolved.provider_id.clone(),
            model_id: resolved.model_id.clone(),
            resource_id: SafeDisplayText::new("resource").expect("wire resource"),
        },
        serde_json::json!({"semantically":"invalid"}),
    )
    .expect("artifact");
    let (restored, preflight) = restore_replay(&artifact, &resolved, &binding, None);
    assert!(restored.is_some());
    assert!(preflight.is_none());
    let merged = replay_decisions_with_preflight(
        &[
            OvenReplayDecision {
                history_index: 1,
                disposition: OvenReplayDisposition::DiscardedInvalidPayload {
                    reason: "payload did not match normalized content".into(),
                },
            },
            OvenReplayDecision {
                history_index: 1,
                disposition: OvenReplayDisposition::ReconstructedNormalized,
            },
        ],
        &binding,
        &[],
    );
    assert!(matches!(
        merged.as_slice(),
        [
            cookie_agent_protocol::ReplayDecision {
                disposition: ReplayDisposition::DiscardedInvalidPayload { .. },
                ..
            },
            cookie_agent_protocol::ReplayDecision {
                disposition: ReplayDisposition::ReconstructedNormalizedHistory,
                ..
            }
        ]
    ));
}

#[test]
fn foreign_scope_replay_decision_is_persisted_as_model_selection() {
    let binding = binding();
    let found = OvenNativeContextScope::new(
        oven_sdk::ProviderId::new("provider"),
        oven_sdk::ModelId::new("one"),
        ResourceId::new("resource-one").expect("resource"),
    )
    .expect("scope");
    let expected = OvenNativeContextScope::new(
        oven_sdk::ProviderId::new("provider"),
        oven_sdk::ModelId::new("two"),
        ResourceId::new("resource-two").expect("resource"),
    )
    .expect("scope");
    let decisions = replay_decisions(
        &[OvenReplayDecision {
            history_index: 3,
            disposition: OvenReplayDisposition::DiscardedForeignScope {
                found: found.clone(),
                expected,
            },
        }],
        &binding,
    );
    assert!(matches!(
        &decisions[0].disposition,
        ReplayDisposition::DiscardedForeignModelSelection { found: persisted_found, expected: persisted_expected }
            if persisted_found.model.to_string() == "provider/one"
                && persisted_expected == &binding.selection
    ));
}

#[test]
fn checkpoint_before_new_user_keeps_summary_and_user_live() {
    let directory = tempfile::tempdir().expect("tempdir");
    let store =
        crate::ArtifactRouter::open_flat(directory.path().join("artifacts")).expect("store");
    let binding = binding();
    let run = RunId::new_v7();
    let summary_limit = SummaryByteLimit::new(1024).expect("limit");
    let checkpoint = InternalSummaryCheckpoint::new(
        "predictive summary".into(),
        InternalAgentInvocationId::new_v7(),
        InternalAgentRunId::new_v7(),
        summary_limit,
    )
    .expect("checkpoint");
    let events = vec![
        event(
            1,
            run,
            EventPayload::UserInputSubmitted {
                input: "old compacted input".into(),
            },
        ),
        event(
            2,
            run,
            EventPayload::ContextCheckpointCommitted {
                commit: ContextCheckpointCommit {
                    checkpoint: ContextCheckpoint::InternalSummary { checkpoint },
                    boundaries: ContextCheckpointBoundaries {
                        source_from_seq: 1,
                        source_through_seq: 1,
                        recent_from_seq: None,
                        input_through_seq: 1,
                        prior_checkpoint_seq: None,
                    },
                    budgets: ContextCheckpointBudgets {
                        context_limit_tokens: 100,
                        trigger_tokens: 70,
                        input_tokens_before: 60,
                        input_tokens_after: 5,
                        keep_recent_tokens: 0,
                        max_summary_bytes: summary_limit,
                    },
                },
            },
        ),
        event(
            3,
            run,
            EventPayload::UserInputSubmitted {
                input: "extremely long live user input".into(),
            },
        ),
        event(4, run, EventPayload::UserInputApplied { user_input_seq: 3 }),
    ];

    assert!(events[1].seq < events[2].seq);
    let context = assemble_model_context(&events, &store, &binding, "System prompt.")
        .expect("assembled context");
    let serialized = serde_json::to_string(&context.history).expect("serialized history");
    assert!(serialized.contains("predictive summary"));
    assert!(serialized.contains("extremely long live user input"));
    assert!(!serialized.contains("old compacted input"));
}

#[test]
fn injected_and_transformed_messages_replay_from_durable_events() {
    let directory = tempfile::tempdir().expect("tempdir");
    let store =
        crate::ArtifactRouter::open_flat(directory.path().join("artifacts")).expect("store");
    let binding = binding();
    let run = RunId::new_v7();
    let events = vec![
        event(
            1,
            run,
            EventPayload::MessageInjected {
                role: cookie_agent_protocol::ExtensionMessageRole::User,
                input: "durable injected context".into(),
            },
        ),
        event(
            2,
            run,
            EventPayload::UserInputTransformed {
                original_input: "original command".into(),
                input: "committed transformed input".into(),
            },
        ),
        event(
            3,
            run,
            EventPayload::UserInputSubmitted {
                input: "committed transformed input".into(),
            },
        ),
        event(4, run, EventPayload::UserInputApplied { user_input_seq: 3 }),
    ];

    let context = assemble_model_context(&events, &store, &binding, "System prompt.")
        .expect("assembled context");
    let serialized = serde_json::to_string(&context.history).expect("serialized history");
    assert!(serialized.contains("durable injected context"));
    assert!(serialized.contains("committed transformed input"));
    assert!(!serialized.contains("original command"));
}

#[test]
fn native_checkpoint_carries_window_and_drops_pre_checkpoint_history() {
    let directory = tempfile::tempdir().expect("tempdir");
    let store =
        crate::ArtifactRouter::open_flat(directory.path().join("artifacts")).expect("store");
    let mut binding = binding();
    binding.descriptor.capabilities.compaction = oven_sdk::CompactionCapability::Native;
    let run = RunId::new_v7();
    let sdk_window = OvenNativeContextWindow::new(
        AdapterId::new(binding.descriptor.adapter_id.as_str()),
        OvenNativeContextScope::new(
            oven_sdk::ProviderId::new(binding.selection.model.provider_id().as_str()),
            oven_sdk::ModelId::new(binding.selection.model.model_id().as_str()),
            ResourceId::new("native-window-v1").expect("resource"),
        )
        .expect("scope"),
        serde_json::json!({"type": "compaction", "id": "cmp_1"}),
    )
    .expect("SDK window");
    let window =
        super::persist_native_context(sdk_window.clone(), &binding).expect("persisted window");
    let summary_limit = SummaryByteLimit::new(1024).expect("limit");
    let events = vec![
        event(
            1,
            run,
            EventPayload::UserInputSubmitted {
                input: "old compacted input".into(),
            },
        ),
        event(
            2,
            run,
            EventPayload::ContextCheckpointCommitted {
                commit: ContextCheckpointCommit {
                    checkpoint: ContextCheckpoint::NativeWindow { window },
                    boundaries: ContextCheckpointBoundaries {
                        source_from_seq: 1,
                        source_through_seq: 1,
                        recent_from_seq: None,
                        input_through_seq: 1,
                        prior_checkpoint_seq: None,
                    },
                    budgets: ContextCheckpointBudgets {
                        context_limit_tokens: 100,
                        trigger_tokens: 70,
                        input_tokens_before: 60,
                        input_tokens_after: 5,
                        keep_recent_tokens: 0,
                        max_summary_bytes: summary_limit,
                    },
                },
            },
        ),
        event(
            3,
            run,
            EventPayload::UserInputSubmitted {
                input: "live input".into(),
            },
        ),
        event(4, run, EventPayload::UserInputApplied { user_input_seq: 3 }),
    ];

    let context = assemble_model_context(&events, &store, &binding, "System prompt.")
        .expect("assembled context");
    let serialized = serde_json::to_string(&context.history).expect("serialized history");
    assert!(!serialized.contains("old compacted input"));
    assert!(serialized.contains("live input"));
    assert_eq!(context.native_context, Some(sdk_window));
    let request = oven_sdk::Request::new(context.history)
        .with_native_context(context.native_context.expect("native context"));
    assert!(request.native_context.is_some());
}

#[test]
fn native_checkpoint_does_not_reappend_covered_pending_producer_input() {
    let directory = tempfile::tempdir().expect("tempdir");
    let store =
        crate::ArtifactRouter::open_flat(directory.path().join("artifacts")).expect("store");
    let mut binding = binding();
    binding.descriptor.capabilities.compaction = oven_sdk::CompactionCapability::Native;
    let run = RunId::new_v7();
    let covered_id = ProducerMessageId::new_v7();
    let post_boundary_id = ProducerMessageId::new_v7();
    let sdk_window = OvenNativeContextWindow::new(
        AdapterId::new(binding.descriptor.adapter_id.as_str()),
        OvenNativeContextScope::new(
            oven_sdk::ProviderId::new(binding.selection.model.provider_id().as_str()),
            oven_sdk::ModelId::new(binding.selection.model.model_id().as_str()),
            ResourceId::new("native-producer-window").expect("resource"),
        )
        .expect("scope"),
        serde_json::json!({"type": "compaction", "id": "cmp_producer"}),
    )
    .expect("SDK window");
    let window =
        super::persist_native_context(sdk_window.clone(), &binding).expect("persisted window");
    let summary_limit = SummaryByteLimit::new(1024).expect("limit");
    let mut events = vec![
        run_started_event(1, run, &binding),
        producer_accepted_event(
            2,
            covered_id,
            ProducerOwner::Plugin {
                plugin: "jobs".into(),
            },
            "covered",
            "producer body covered by native window",
        ),
        event(
            3,
            run,
            EventPayload::ProducerMessageAdmitted {
                message_id: covered_id,
            },
        ),
        event(
            4,
            run,
            EventPayload::ProducerMessagesClaimed {
                message_ids: vec![covered_id],
            },
        ),
    ];
    let pre_checkpoint = assemble_model_context(&events, &store, &binding, "System prompt.")
        .expect("pre-checkpoint context");
    let retained = checkpoint_retained_history(&pre_checkpoint.history, &events, None);
    events.push(event(
        5,
        run,
        EventPayload::ContextCheckpointCommitted {
            commit: ContextCheckpointCommit {
                checkpoint: ContextCheckpoint::NativeWindow { window },
                boundaries: ContextCheckpointBoundaries {
                    source_from_seq: 1,
                    source_through_seq: 4,
                    recent_from_seq: None,
                    input_through_seq: 4,
                    prior_checkpoint_seq: None,
                },
                budgets: ContextCheckpointBudgets {
                    context_limit_tokens: 100,
                    trigger_tokens: 70,
                    input_tokens_before: 60,
                    input_tokens_after: 5,
                    keep_recent_tokens: 0,
                    max_summary_bytes: summary_limit,
                },
            },
        },
    ));

    let compacted = assemble_model_context(&events, &store, &binding, "System prompt.")
        .expect("compacted context");
    assert_eq!(compacted.native_context, Some(sdk_window.clone()));
    assert_eq!(compacted.history, retained);
    assert_eq!(
        crate::runtime::compaction::serialized_fit_request_bytes(&compacted.history, &[])
            .expect("compacted fit bytes"),
        crate::runtime::compaction::serialized_fit_request_bytes(&retained, &[])
            .expect("retained fit bytes")
    );
    assert!(
        !serde_json::to_string(&compacted.history)
            .unwrap()
            .contains("producer body covered by native window")
    );

    events.extend([
        producer_accepted_event(
            6,
            post_boundary_id,
            ProducerOwner::Plugin {
                plugin: "jobs".into(),
            },
            "post-boundary",
            "producer body after native boundary",
        ),
        event(
            7,
            run,
            EventPayload::ProducerMessageAdmitted {
                message_id: post_boundary_id,
            },
        ),
    ]);
    let continued = assemble_model_context(&events, &store, &binding, "System prompt.")
        .expect("continued native context");
    let rendered = serde_json::to_string(&continued.history).unwrap();
    assert!(!rendered.contains("producer body covered by native window"));
    assert_eq!(
        rendered
            .matches("producer body after native boundary")
            .count(),
        1
    );
    assert_eq!(
        continued
            .history
            .iter()
            .filter(|turn| matches!(turn, HistoryTurn::User(_)))
            .count(),
        1
    );

    let projection = GoalProducerProjection::from_events(&events);
    assert_eq!(projection.messages.len(), 2);
    assert_eq!(projection.messages[0].claims, HashSet::from([4]));
    assert!(!projection.messages[0].consumed);
    assert!(!projection.messages[0].consumption_recorded);
    assert_eq!(projection.messages[1].admission, Some((run, 7)));
    assert!(!projection.messages[1].consumed);

    assert!(compaction_tail_candidates(&events).is_empty());
    let prefix = compaction_prefix_history(&events, &store, &binding, "System prompt.", Some(7))
        .expect("native compaction prefix");
    let prefix = serde_json::to_string(&prefix).unwrap();
    assert!(!prefix.contains("producer body covered by native window"));
    assert!(!prefix.contains("producer body after native boundary"));
}

#[test]
fn revert_voids_checkpoint_beyond_boundary_and_keeps_older_checkpoint() {
    let directory = tempfile::tempdir().expect("tempdir");
    let store =
        crate::ArtifactRouter::open_flat(directory.path().join("artifacts")).expect("store");
    let binding = binding();
    let run = RunId::new_v7();
    let summary_limit = SummaryByteLimit::new(1024).expect("limit");
    let checkpoint = |summary: &str, through_seq| ContextCheckpointCommit {
        checkpoint: ContextCheckpoint::InternalSummary {
            checkpoint: InternalSummaryCheckpoint::new(
                summary.into(),
                InternalAgentInvocationId::new_v7(),
                InternalAgentRunId::new_v7(),
                summary_limit,
            )
            .expect("checkpoint"),
        },
        boundaries: ContextCheckpointBoundaries {
            source_from_seq: 1,
            source_through_seq: through_seq,
            recent_from_seq: None,
            input_through_seq: through_seq,
            prior_checkpoint_seq: None,
        },
        budgets: ContextCheckpointBudgets {
            context_limit_tokens: 100,
            trigger_tokens: 70,
            input_tokens_before: 60,
            input_tokens_after: 5,
            keep_recent_tokens: 0,
            max_summary_bytes: summary_limit,
        },
    };
    let session = SessionId(uuid::Uuid::from_u128(1));
    let mut events = vec![
        event(
            1,
            run,
            EventPayload::UserInputSubmitted {
                input: "old input".into(),
            },
        ),
        event(
            2,
            run,
            EventPayload::ContextCheckpointCommitted {
                commit: checkpoint("older summary", 1),
            },
        ),
        event(
            3,
            run,
            EventPayload::UserInputSubmitted {
                input: "void input".into(),
            },
        ),
        event(
            4,
            run,
            EventPayload::ContextCheckpointCommitted {
                commit: checkpoint("void summary", 3),
            },
        ),
    ];
    events.push(StoredEvent {
        engine_version: None,
        origin: None,
        session_id: session,
        run_id: None,
        seq: 5,
        timestamp: jiff::Timestamp::new(5, 0).expect("timestamp"),
        payload: EventPayload::SessionReverted { through_seq: 2 },
    });
    let visible = cookie_agent_protocol::visible_events(&events);
    let context = assemble_model_context(&visible, &store, &binding, "System prompt.")
        .expect("assembled context");
    let serialized = serde_json::to_string(&context.history).expect("serialized history");
    assert!(serialized.contains("older summary"));
    assert!(!serialized.contains("void summary"));
    assert!(!serialized.contains("void input"));
}

#[test]
fn projected_summary_matches_persisted_checkpoint_replay() {
    let directory = tempfile::tempdir().expect("tempdir");
    let store =
        crate::ArtifactRouter::open_flat(directory.path().join("artifacts")).expect("store");
    let binding = binding();
    let run = RunId::new_v7();
    let mut events = Vec::from(user_events(1, run, "discarded"));
    events.extend(user_events(3, run, "retained tail"));

    let projected = project_summary_context(
        &events,
        &store,
        &binding,
        "System prompt.",
        4,
        Some(3),
        "projected summary",
    )
    .expect("projection");
    events.push(event(
        5,
        run,
        EventPayload::ContextCheckpointCommitted {
            commit: summary_commit("projected summary", 1, 4, Some(3), None),
        },
    ));
    let replayed = assemble_model_context(&events, &store, &binding, "System prompt.")
        .expect("persisted replay");

    assert_eq!(projected.history, replayed.history);
    assert_eq!(projected.replay_decisions, replayed.replay_decisions);
}

#[test]
fn compaction_prefix_uses_current_agent_md_across_run_boundary() {
    let directory = tempfile::tempdir().expect("tempdir");
    let store =
        crate::ArtifactRouter::open_flat(directory.path().join("artifacts")).expect("store");
    let binding = binding();
    let run_a = RunId::new_v7();
    let run_b = RunId::new_v7();
    let run_started = |seq, run| run_started_event(seq, run, &binding);
    let agent_md = |seq, run, content: &str| {
        event(
            seq,
            run,
            EventPayload::AgentMdLoaded {
                entries: vec![AgentMdEntry {
                    source: SafeDisplayText::new("/workspace/AGENTS.md").unwrap(),
                    content: content.into(),
                    byte_length: content.len() as u64,
                }],
            },
        )
    };
    for current_instructions in [Some("run B instructions"), None] {
        let mut events = vec![
            run_started(1, run_a),
            agent_md(2, run_a, "stale run A instructions"),
        ];
        events.extend(user_events(3, run_a, "discarded run A user"));
        let mut assistant = replay_turn_event(&binding);
        assistant.seq = 5;
        assistant.run_id = Some(run_a);
        events.push(assistant);
        events.push(run_started(6, run_b));
        if let Some(instructions) = current_instructions {
            events.push(agent_md(7, run_b, instructions));
        }
        events.extend(user_events(8, run_b, "retained run B user"));
        assert!(compaction_tail_candidates(&events).contains(&5));

        let prefix = compaction_prefix_history(&events, &store, &binding, "system", Some(5))
            .expect("cross-run summarizer prefix");
        let encoded = serde_json::to_string(&prefix).unwrap();
        assert!(!encoded.contains("stale run A instructions"), "{encoded}");
        assert_eq!(
            encoded.contains("run B instructions"),
            current_instructions.is_some()
        );
        assert!(encoded.contains("discarded run A user"));
        assert!(!encoded.contains("retained run B user"));
        assert!(!encoded.contains("historical reasoning"));

        let projected =
            project_summary_context(&events, &store, &binding, "system", 9, Some(5), "summary")
                .expect("projected checkpoint");
        let encoded = serde_json::to_string(&projected.history).unwrap();
        assert!(!encoded.contains("stale run A instructions"));
        assert_eq!(
            encoded.contains("run B instructions"),
            current_instructions.is_some()
        );
        let summary_index = 1 + usize::from(current_instructions.is_some());
        assert!(
            serde_json::to_string(&projected.history[summary_index])
                .unwrap()
                .contains("<summary>")
        );
        events.push(event(
            10,
            run_b,
            EventPayload::ContextCheckpointCommitted {
                commit: summary_commit("summary", 1, 9, Some(5), None),
            },
        ));
        let replayed = assemble_model_context(&events, &store, &binding, "system").unwrap();
        assert_eq!(projected.history, replayed.history);
        assert_eq!(projected.replay_decisions, replayed.replay_decisions);
    }
}

#[test]
fn summary_projection_orders_and_deduplicates_pinned_context() {
    let directory = tempfile::tempdir().expect("tempdir");
    let store =
        crate::ArtifactRouter::open_flat(directory.path().join("artifacts")).expect("store");
    let binding = binding();
    let run = RunId::new_v7();
    let mut events = vec![
        event(
            1,
            run,
            EventPayload::AgentMdLoaded {
                entries: vec![AgentMdEntry {
                    source: SafeDisplayText::new("/workspace/AGENTS.md").unwrap(),
                    content: "pinned agent rules".into(),
                    byte_length: 18,
                }],
            },
        ),
        event(
            2,
            run,
            EventPayload::SkillLoaded {
                name: "tail-skill".into(),
                rendered_body: "pinned skill body".into(),
                source_path: "/tail-skill/SKILL.md".into(),
                args: String::new(),
                base_dir: "/tail-skill".into(),
                supporting_files: Vec::new(),
            },
        ),
    ];
    events.extend(user_events(3, run, "tail user"));

    let context =
        project_summary_context(&events, &store, &binding, "system", 4, Some(2), "summary")
            .expect("projection");
    let turns = context
        .history
        .iter()
        .map(|turn| serde_json::to_string(turn).unwrap())
        .collect::<Vec<_>>();
    assert!(turns[1].contains("pinned agent rules"));
    assert!(turns[2].contains("pinned skill body"));
    assert!(turns[3].contains("<summary>\\nsummary"));
    assert!(turns[4].contains("tail user"));
    let encoded = turns.join("\n");
    assert_eq!(encoded.matches("pinned agent rules").count(), 1);
    assert_eq!(encoded.matches("pinned skill body").count(), 1);
}

#[test]
fn repeated_checkpoint_skips_old_tail_as_first_candidate_but_summarizes_it() {
    let directory = tempfile::tempdir().expect("tempdir");
    let store =
        crate::ArtifactRouter::open_flat(directory.path().join("artifacts")).expect("store");
    let binding = binding();
    let run = RunId::new_v7();
    let mut events = Vec::from(user_events(1, run, "first discarded"));
    events.extend(user_events(3, run, "old retained tail"));
    events.push(event(
        5,
        run,
        EventPayload::ContextCheckpointCommitted {
            commit: summary_commit("first summary", 1, 4, Some(3), None),
        },
    ));
    events.extend(user_events(6, run, "new retained tail"));

    assert_eq!(compaction_tail_candidates(&events), vec![6]);
    let prefix = compaction_prefix_history(&events, &store, &binding, "system", Some(6))
        .expect("summary prefix");
    let prefix = serde_json::to_string(&prefix).unwrap();
    assert!(prefix.contains("first summary"));
    assert!(prefix.contains("old retained tail"));
    assert!(!prefix.contains("new retained tail"));

    events.push(event(
        8,
        run,
        EventPayload::ContextCheckpointCommitted {
            commit: summary_commit("second summary", 3, 7, Some(6), Some(5)),
        },
    ));
    let replayed = assemble_model_context(&events, &store, &binding, "system")
        .expect("second checkpoint replay");
    let replayed = serde_json::to_string(&replayed.history).unwrap();
    assert!(replayed.contains("second summary"));
    assert!(replayed.contains("new retained tail"));
    assert!(!replayed.contains("first summary"));
    assert!(!replayed.contains("old retained tail"));
}

#[test]
fn tail_candidates_do_not_split_queued_user_application() {
    let run = RunId::new_v7();
    let mut events = Vec::from(user_events(1, run, "older prefix"));
    events.push(event(
        3,
        run,
        EventPayload::UserInputSubmitted {
            input: "queued input".into(),
        },
    ));
    events.push(event(
        4,
        run,
        EventPayload::MessageInjected {
            role: cookie_agent_protocol::ExtensionMessageRole::Assistant,
            input: "interleaved assistant".into(),
        },
    ));
    events.push(event(
        5,
        run,
        EventPayload::UserInputApplied { user_input_seq: 3 },
    ));
    events.extend(user_events(6, run, "newest input"));
    assert_eq!(compaction_tail_candidates(&events), vec![3, 6]);
}

#[test]
fn tail_candidates_stop_at_pending_tool_group_until_late_termination() {
    let directory = tempfile::tempdir().expect("tempdir");
    let store =
        crate::ArtifactRouter::open_flat(directory.path().join("artifacts")).expect("store");
    let binding = binding();
    let run = RunId::new_v7();
    let tool_call_id = ToolCallId::new_v7();
    let model_call_id = ModelCallId::new("pending-call").unwrap();
    let owner = AssistantToolCallRef {
        model_turn_seq: 1,
        content_index: 0,
        model_call_id: model_call_id.clone(),
        provider_item_id: None,
    };
    let mut events = Vec::from(user_events(1, run, "before tool"));
    events.push(event(
        3,
        run,
        EventPayload::ModelTurnCommitted {
            attempt_id: cookie_agent_protocol::AttemptId::new_v7(),
            model_turn_seq: 1,
            resolved_model: wire_model(&binding),
            input_through_seq: 2,
            turn: PersistedModelTurn {
                content: vec![PersistedAssistantPart::ToolCall {
                    id: model_call_id,
                    provider_item_id: None,
                    name: SafeCode::new("read").unwrap(),
                    input: serde_json::json!({}),
                    raw_input: None,
                    metadata: None,
                }],
                provider_options: BTreeMap::new(),
                finish_reason: ModelFinishReason::ToolCalls,
                usage: Usage::default(),
                response_metadata: BTreeMap::new(),
                provider_metadata: BTreeMap::new(),
                native_replay: None,
            },
            warnings: Vec::new(),
        },
    ));
    events.push(event(
        4,
        run,
        EventPayload::ToolCallStarted {
            start: ToolCallStart {
                output: Default::default(),
                tool_call_id,
                owner: owner.clone(),
                presentation: ToolCallPresentation {
                    title: SafeDisplayText::new("pending").unwrap(),
                    primary_argument: None,
                },
                operation_fingerprint: OperationFingerprint::from_prepared_operation(&operation()),
            },
        },
    ));
    events.extend(user_events(5, run, "after pending tool"));
    assert_eq!(compaction_tail_candidates(&events), vec![3]);

    events.push(event(
        7,
        run,
        EventPayload::ToolCallTerminated {
            termination: ToolCallTermination {
                tool_call_id,
                owner,
                outcome: ToolTerminationOutcome::Completed,
                result: Some(PersistedToolResult {
                    display: None,
                    retained_output: None,
                    title: SafeDisplayText::new("late result").unwrap(),
                    output: "late tool output".into(),
                    metadata: serde_json::Value::Null,
                    truncation: None,
                    attachments: Vec::new(),
                    additional_messages: Vec::new(),
                }),
                error: None,
            },
        },
    ));
    assert_eq!(compaction_tail_candidates(&events), vec![3]);
    events.extend(user_events(8, run, "after late termination"));
    assert_eq!(compaction_tail_candidates(&events), vec![3, 8]);
    for recent_from_seq in [None, Some(3)] {
        let replay = project_summary_context(
            &events,
            &store,
            &binding,
            "system",
            6,
            recent_from_seq,
            "summary",
        )
        .expect("late termination replay");
        let encoded = serde_json::to_string(&replay.history).unwrap();
        assert!(encoded.contains("late tool output"));
        let result_index = replay
            .history
            .iter()
            .position(|turn| matches!(turn, HistoryTurn::Tool(_)))
            .expect("late tool result");
        assert!(matches!(
            replay.history[result_index - 1],
            HistoryTurn::Assistant(_)
        ));
        oven_sdk::Request::new(replay.history)
            .validate_for(&binding.descriptor.capabilities)
            .expect("paired late result");
    }
}

#[test]
fn assembled_tool_transcript_snapshot_is_stable() {
    let directory = tempfile::tempdir().expect("tempdir");
    let artifact_path = directory.path().join("artifacts");
    let store = crate::ArtifactRouter::open_flat(artifact_path.clone()).expect("store");
    let binding = binding();
    let resolved = wire_model(&binding);
    let run = RunId(uuid::Uuid::from_u128(2));
    let call = ToolCallId(uuid::Uuid::from_u128(8));
    let owner = AssistantToolCallRef {
        model_turn_seq: 1,
        content_index: 0,
        model_call_id: ModelCallId::new("provider-call").expect("model call id"),
        provider_item_id: None,
    };
    let result = PersistedToolResult {
        display: None,
        retained_output: None,
        title: SafeDisplayText::new("Read README.md").expect("title"),
        output: "contents".into(),
        metadata: serde_json::json!({}),
        truncation: None,
        attachments: Vec::new(),
        additional_messages: vec![
            ToolEmittedMessage::new(
                ToolEmittedMessageRole::System,
                vec![ToolEmittedContent::Text("emitted system context".into())],
            )
            .expect("system emission"),
            ToolEmittedMessage::new(
                ToolEmittedMessageRole::User,
                vec![ToolEmittedContent::Text("emitted user context".into())],
            )
            .expect("user emission"),
        ],
    };
    let termination = EventPayload::ToolCallTerminated {
        termination: ToolCallTermination {
            tool_call_id: call,
            owner: owner.clone(),
            outcome: ToolTerminationOutcome::Completed,
            result: Some(result),
            error: None,
        },
    };
    let mut events = vec![
        event(
            1,
            run,
            EventPayload::UserInputSubmitted {
                input: "inspect the workspace".into(),
            },
        ),
        event(2, run, EventPayload::UserInputApplied { user_input_seq: 1 }),
        event(
            3,
            run,
            EventPayload::ModelTurnCommitted {
                attempt_id: cookie_agent_protocol::AttemptId::new_v7(),
                model_turn_seq: 1,
                resolved_model: resolved,
                input_through_seq: 1,
                turn: PersistedModelTurn {
                    content: vec![PersistedAssistantPart::ToolCall {
                        id: owner.model_call_id.clone(),
                        provider_item_id: None,
                        name: SafeCode::new("read").expect("tool name"),
                        input: serde_json::json!({"filePath":"README.md"}),
                        raw_input: None,
                        metadata: None,
                    }],
                    provider_options: BTreeMap::new(),
                    finish_reason: ModelFinishReason::ToolCalls,
                    usage: Usage::default(),
                    response_metadata: BTreeMap::new(),
                    provider_metadata: BTreeMap::new(),
                    native_replay: None,
                },
                warnings: Vec::new(),
            },
        ),
        event(
            4,
            run,
            EventPayload::ToolCallStarted {
                start: ToolCallStart {
                    output: Default::default(),
                    tool_call_id: call,
                    owner: owner.clone(),
                    presentation: ToolCallPresentation {
                        title: SafeDisplayText::new("Read README.md").expect("title"),
                        primary_argument: Some(
                            BoundedDisplayText::new("README.md").expect("argument"),
                        ),
                    },
                    operation_fingerprint: OperationFingerprint::from_prepared_operation(
                        &operation(),
                    ),
                },
            },
        ),
        event(5, run, termination.clone()),
        event(6, run, termination),
        event(
            7,
            run,
            EventPayload::ModelTurnCommitted {
                attempt_id: cookie_agent_protocol::AttemptId::new_v7(),
                model_turn_seq: 2,
                resolved_model: wire_model(&binding),
                input_through_seq: 6,
                turn: PersistedModelTurn {
                    content: vec![PersistedAssistantPart::Text {
                        text: "next assistant".into(),
                        metadata: None,
                    }],
                    provider_options: BTreeMap::new(),
                    finish_reason: ModelFinishReason::Stop,
                    usage: Usage::default(),
                    response_metadata: BTreeMap::new(),
                    provider_metadata: BTreeMap::new(),
                    native_replay: None,
                },
                warnings: Vec::new(),
            },
        ),
    ];
    let history = assemble_full_history(&events, &store, &binding, "System prompt.")
        .expect("assembled history");
    drop(store);
    let restarted_store =
        crate::ArtifactRouter::open_flat(artifact_path.clone()).expect("restarted store");
    let replayed = assemble_full_history(&events, &restarted_store, &binding, "System prompt.")
        .expect("replayed history");
    assert_eq!(history, replayed);
    assert!(matches!(history[3], HistoryTurn::Tool(_)));
    assert!(matches!(history[4], HistoryTurn::User(_)));
    assert!(matches!(history[5], HistoryTurn::User(_)));
    assert!(matches!(history[6], HistoryTurn::Assistant(_)));
    let encoded = serde_json::to_string(&history).expect("history JSON");
    assert!(encoded.contains(TOOL_EMITTED_SYSTEM_USER_MARKER));
    assert_eq!(encoded.matches("emitted user context").count(), 1);
    assert_eq!(encoded.matches("emitted system context").count(), 1);
    oven_sdk::Request::new(history.clone())
        .validate_for(&binding.descriptor.capabilities)
        .expect("user history may follow a paired tool result");
    insta::with_settings!({ prepend_module_to_snapshot => false }, {
        insta::assert_json_snapshot!(
            "cookie_agent_engine__tests__assembled_tool_transcript_snapshot_is_stable",
            history
        );
    });

    // Cancellation alone (or a success-looking display) is not evidence that an
    // opt-out result finished. The terminal commit's lifecycle reason is required.
    for finalized in [false, true] {
        let mut cancelled = events.clone();
        for event in &mut cancelled {
            if let EventPayload::ToolCallTerminated { termination } = &mut event.payload {
                termination.outcome = ToolTerminationOutcome::Cancelled;
                termination.error = Some(cookie_agent_protocol::SafeToolError {
                    code: SafeCode::new(if finalized {
                        crate::runtime::CANCELLED_AFTER_COMPLETION
                    } else {
                        "execution_failed"
                    })
                    .unwrap(),
                    message: cookie_agent_protocol::SafeErrorMessage::new("cancelled").unwrap(),
                });
                let result = termination.result.as_mut().unwrap();
                assert!(result.retained_output.is_none());
                result.display = Some("looks successfully completed".into());
            }
        }
        let history =
            assemble_full_history(&cancelled, &restarted_store, &binding, "System prompt.")
                .unwrap();
        let encoded = serde_json::to_string(&history).unwrap();
        assert_eq!(encoded.contains("contents"), finalized);
        assert_eq!(encoded.contains("emitted user context"), finalized);
        assert_eq!(encoded.contains("emitted system context"), finalized);
        assert!(!encoded.contains("looks successfully completed"));
        assert!(
            history
                .iter()
                .filter_map(|turn| match turn {
                    HistoryTurn::Tool(message) => Some(&message.results),
                    _ => None,
                })
                .flatten()
                .all(|result| result.is_error)
        );
    }

    let (retained, _) = restarted_store
        .retain(crate::test_session_id(), b"contents")
        .expect("retain output");
    events.push(event(
        8,
        run,
        EventPayload::ContextCheckpointCommitted {
            commit: summary_commit("tool checkpoint", 1, 7, Some(3), None),
        },
    ));
    events.push(event(
        9,
        run,
        EventPayload::ToolOutputElided {
            tool_call_id: call,
            original_bytes: 8,
            retained,
        },
    ));
    let elided = assemble_full_history(&events, &restarted_store, &binding, "System prompt.")
        .expect("elided history");
    drop(restarted_store);
    let restarted_store = crate::ArtifactRouter::open_flat(artifact_path).expect("second restart");
    let replayed_elision =
        assemble_full_history(&events, &restarted_store, &binding, "System prompt.")
            .expect("replayed elision");
    assert_eq!(elided, replayed_elision);
    let encoded = serde_json::to_string(&elided).expect("elided history JSON");
    assert!(!encoded.contains("emitted system context"));
    assert!(!encoded.contains("emitted user context"));
    assert!(encoded.contains("2 tool-emitted message(s) were elided"));

    let prefix = compaction_prefix_history(
        &events,
        &restarted_store,
        &binding,
        "System prompt.",
        Some(7),
    )
    .expect("elided compaction prefix");
    let encoded = serde_json::to_string(&prefix).expect("prefix JSON");
    assert!(!encoded.contains("contents"));
    assert!(encoded.contains("2 tool-emitted message(s) were elided"));
}

#[test]
fn aborted_committed_turn_keeps_its_partial_text_and_gains_an_interrupt_marker() {
    let binding = binding();
    let resolved = wire_model(&binding);
    let run = RunId::new_v7();
    let events = [
        run_started_event(1, run, &binding),
        event(
            2,
            run,
            EventPayload::ModelTurnCommitted {
                attempt_id: cookie_agent_protocol::AttemptId::new_v7(),
                model_turn_seq: 1,
                resolved_model: resolved,
                input_through_seq: 1,
                turn: PersistedModelTurn {
                    content: vec![PersistedAssistantPart::Text {
                        text: "partial answer".into(),
                        metadata: None,
                    }],
                    provider_options: BTreeMap::new(),
                    finish_reason: ModelFinishReason::Aborted,
                    usage: Usage::default(),
                    response_metadata: BTreeMap::new(),
                    provider_metadata: BTreeMap::new(),
                    native_replay: None,
                },
                warnings: Vec::new(),
            },
        ),
    ];
    let directory = tempfile::tempdir().expect("tempdir");
    let store =
        crate::ArtifactRouter::open_flat(directory.path().join("artifacts")).expect("store");
    let context = assemble_model_context(&events, &store, &binding, "System prompt.")
        .expect("interrupted context");
    let HistoryTurn::Assistant(assistant) = &context.history[1] else {
        panic!("the partial turn survives as an assistant turn");
    };
    assert_eq!(
        assistant.finish.finish_reason,
        super::FinishReason::Aborted,
        "the interrupted turn keeps its finish reason"
    );
    let HistoryTurn::User(marker) = &context.history[2] else {
        panic!("the interrupted turn is annotated for the next request");
    };
    assert_eq!(
        serde_json::to_string(marker).expect("marker JSON"),
        serde_json::to_string(&super::user_text(INTERRUPTED_TURN_MARKER)).expect("marker JSON")
    );
    let encoded = serde_json::to_string(&context.history).expect("history JSON");
    assert_eq!(encoded.matches("partial answer").count(), 1);
}

#[test]
fn provider_cancelled_turn_gains_no_interrupt_marker() {
    // Chat Completions providers may report a `cancelled` finish themselves;
    // only the engine's salvage (`Aborted`) is an interrupt.
    let binding = binding();
    let resolved = wire_model(&binding);
    let run = RunId::new_v7();
    let events = [
        run_started_event(1, run, &binding),
        event(
            2,
            run,
            EventPayload::ModelTurnCommitted {
                attempt_id: cookie_agent_protocol::AttemptId::new_v7(),
                model_turn_seq: 1,
                resolved_model: resolved,
                input_through_seq: 1,
                turn: PersistedModelTurn {
                    content: vec![PersistedAssistantPart::Text {
                        text: "provider stopped here".into(),
                        metadata: None,
                    }],
                    provider_options: BTreeMap::new(),
                    finish_reason: ModelFinishReason::Cancelled,
                    usage: Usage::default(),
                    response_metadata: BTreeMap::new(),
                    provider_metadata: BTreeMap::new(),
                    native_replay: None,
                },
                warnings: Vec::new(),
            },
        ),
    ];
    let directory = tempfile::tempdir().expect("tempdir");
    let store =
        crate::ArtifactRouter::open_flat(directory.path().join("artifacts")).expect("store");
    let context = assemble_model_context(&events, &store, &binding, "System prompt.")
        .expect("cancelled context");
    assert_eq!(context.history.len(), 2, "{:?}", context.history);
    assert!(matches!(context.history[1], HistoryTurn::Assistant(_)));
    let encoded = serde_json::to_string(&context.history).expect("history JSON");
    assert!(!encoded.contains(INTERRUPTED_TURN_MARKER));
}

#[test]
fn aborted_reasoning_only_turn_is_skipped_without_an_interrupt_marker() {
    // The target cannot replay provider reasoning, so `restore_turn_with_store`
    // drops the only part the aborted commit carried. The resulting empty
    // assistant turn — and the marker that would annotate it — must not reach
    // history.
    let binding = binding();
    let resolved = wire_model(&binding);
    let run = RunId::new_v7();
    let events = [
        run_started_event(1, run, &binding),
        event(
            2,
            run,
            EventPayload::ModelTurnCommitted {
                attempt_id: cookie_agent_protocol::AttemptId::new_v7(),
                model_turn_seq: 1,
                resolved_model: resolved,
                input_through_seq: 1,
                turn: PersistedModelTurn {
                    content: vec![PersistedAssistantPart::Reasoning {
                        text: "hidden reasoning".into(),
                        metadata: None,
                    }],
                    provider_options: BTreeMap::new(),
                    finish_reason: ModelFinishReason::Aborted,
                    usage: Usage::default(),
                    response_metadata: BTreeMap::new(),
                    provider_metadata: BTreeMap::new(),
                    native_replay: None,
                },
                warnings: Vec::new(),
            },
        ),
    ];
    let directory = tempfile::tempdir().expect("tempdir");
    let store =
        crate::ArtifactRouter::open_flat(directory.path().join("artifacts")).expect("store");
    let context = assemble_model_context(&events, &store, &binding, "System prompt.")
        .expect("interrupted context");
    assert_eq!(
        context.history.len(),
        1,
        "only the system turn survives: {:?}",
        context.history
    );
    assert!(matches!(context.history[0], HistoryTurn::System(_)));
    let encoded = serde_json::to_string(&context.history).expect("history JSON");
    assert!(!encoded.contains(INTERRUPTED_TURN_MARKER));
    assert!(!encoded.contains("hidden reasoning"));
}
