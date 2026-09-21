use std::collections::HashSet;

use cookie_agent_protocol::{
    ArtifactReference, ContextCheckpoint, ContextCheckpointBoundaries, ContextCheckpointBudgets,
    ContextCheckpointCommit, InternalSummaryCheckpoint, MimeType,
    PersistedToolResult as ToolResult, RunId, SafeDisplayText, SessionId, Sha256Digest,
    StoredEvent, SummaryByteLimit, ToolAttachment, ToolEmittedContent, ToolEmittedMessage,
    ToolEmittedMessageRole,
};
use oven_sdk::{
    CompactionCapability, FilePart, FileSource, HistoryTurn, InputPart, JsonSchema,
    Request as ModelRequest, SystemMessage, SystemPart, TextPart, ToolContent, ToolDefinition,
    ToolMessage, ToolResultPart, UserMessage,
};

use super::{
    COMPACTION_INSTRUCTION, DEFAULT_COMPACTION_OUTPUT_RESERVE_TOKENS, FitPart,
    TOOL_OUTPUT_ELISION_MIN_BYTES, VIDEO_FILE_FIT_SURROGATE_BYTES, checkpoint_covers_input,
    compaction_gate, compaction_history, compaction_input_fits, compaction_instruction,
    effective_recent_budget, elidable_bytes, estimated_request_tokens, fit_part_bytes,
    native_compaction_input_budget, raw_fit_from_real_usage, resolve_compaction_trigger,
    select_recent_tail, serialized_fit_request_bytes, should_elide_tool_output,
    usage_reaches_compaction_trigger,
};
use crate::{
    model_history::assemble_model_context,
    runtime::{ContextTokenEstimator, Event, FrozenInternalAgentPolicy, InternalAgentLimits},
};
use cookie_agent_config::ContextCompactionTrigger;

#[test]
fn compaction_trigger_math_supports_percent_and_saturating_buffer() {
    assert_eq!(
        resolve_compaction_trigger(200_000, &ContextCompactionTrigger::Percent { percent: 70 }),
        140_000
    );
    assert_eq!(
        resolve_compaction_trigger(
            200_000,
            &ContextCompactionTrigger::BufferTokens {
                buffer_tokens: 33_000
            }
        ),
        167_000
    );
    assert_eq!(
        resolve_compaction_trigger(
            8_192,
            &ContextCompactionTrigger::BufferTokens {
                buffer_tokens: 33_000
            }
        ),
        0
    );
}

#[test]
fn request_estimate_counts_tool_prompt_section_bytes() {
    let base = vec![HistoryTurn::system(SystemMessage::new(vec![
        SystemPart::Text(TextPart::new("Base prompt.")),
    ]))];
    let with_section = vec![HistoryTurn::system(SystemMessage::new(vec![
        SystemPart::Text(TextPart::new(
            "Base prompt.\n<tool_instructions provider=\"test\">\nProvider policy text.\n</tool_instructions>",
        )),
    ]))];
    assert!(
        estimated_request_tokens(&with_section, &[]).unwrap()
            > estimated_request_tokens(&base, &[]).unwrap()
    );
}

#[test]
fn automatic_compaction_gate_changes_at_proportional_threshold() {
    let trigger =
        resolve_compaction_trigger(200_000, &ContextCompactionTrigger::Percent { percent: 70 });
    assert!(compaction_gate(false, true, trigger));
    assert!(!usage_reaches_compaction_trigger(139_999, trigger));
    assert!(usage_reaches_compaction_trigger(140_000, trigger));
}

#[test]
fn auto_off_blocks_automatic_compaction_but_not_manual_force() {
    assert!(!compaction_gate(false, false, 100));
    assert!(compaction_gate(true, false, 0));
}

fn file_history(files: Vec<FilePart>) -> Vec<HistoryTurn> {
    vec![HistoryTurn::user(UserMessage::new(
        files.into_iter().map(InputPart::File).collect(),
    ))]
}

#[test]
fn one_megabyte_image_contributes_zero_to_fit_estimate() {
    let media = file_history(vec![FilePart::image(
        "image/png",
        FileSource::Bytes(bytes::Bytes::from(vec![0_u8; 1024 * 1024])),
    )]);
    let empty = file_history(Vec::new());

    assert_eq!(
        estimated_request_tokens(&media, &[]).unwrap(),
        estimated_request_tokens(&empty, &[]).unwrap()
    );
}

#[test]
fn several_images_do_not_trigger_but_text_heavy_history_still_does() {
    let text = TextPart::new("small text-only context");
    let mut media_parts = vec![InputPart::Text(text.clone())];
    media_parts.extend(
        (0..3)
            .map(|_| {
                FilePart::image(
                    "image/png",
                    FileSource::Bytes(bytes::Bytes::from(vec![0_u8; 1024 * 1024])),
                )
            })
            .map(InputPart::File),
    );
    let images = vec![HistoryTurn::user(UserMessage::new(media_parts))];
    let text_only = vec![HistoryTurn::user(UserMessage::new(vec![InputPart::Text(
        text,
    )]))];
    let text_heavy = vec![HistoryTurn::user(UserMessage::new(vec![InputPart::Text(
        TextPart::new("x".repeat(128 * 1024)),
    )]))];
    let trigger = 10_000;

    assert_eq!(
        estimated_request_tokens(&images, &[]).unwrap(),
        estimated_request_tokens(&text_only, &[]).unwrap()
    );
    assert!(!usage_reaches_compaction_trigger(
        estimated_request_tokens(&images, &[]).unwrap(),
        trigger
    ));
    assert!(usage_reaches_compaction_trigger(
        estimated_request_tokens(&text_heavy, &[]).unwrap(),
        trigger
    ));
}

#[test]
fn media_calibration_uses_text_bytes_and_still_triggers_for_heavy_text() {
    let calibration_text = TextPart::new("x".repeat(16 * 1024));
    let text_only = vec![HistoryTurn::user(UserMessage::new(vec![InputPart::Text(
        calibration_text.clone(),
    )]))];
    let with_image = vec![HistoryTurn::user(UserMessage::new(vec![
        InputPart::Text(calibration_text),
        InputPart::File(FilePart::image(
            "image/png",
            FileSource::Bytes(bytes::Bytes::from(vec![0_u8; 1024 * 1024])),
        )),
    ]))];
    let text_bytes = serialized_fit_request_bytes(&text_only, &[]).unwrap();
    let media_bytes = serialized_fit_request_bytes(&with_image, &[]).unwrap();
    assert_eq!(media_bytes, text_bytes);

    let observed_tokens = (text_bytes as u64).div_ceil(4);
    let mut text_estimator = ContextTokenEstimator::default();
    text_estimator.record_committed_turn(text_bytes, Some(observed_tokens));
    let mut media_estimator = ContextTokenEstimator::default();
    media_estimator.record_committed_turn(media_bytes, Some(observed_tokens));
    assert!(
        (media_estimator.tokens_per_byte - text_estimator.tokens_per_byte).abs() < f64::EPSILON
    );

    let heavy = vec![HistoryTurn::user(UserMessage::new(vec![
        InputPart::Text(TextPart::new("x".repeat(128 * 1024))),
        InputPart::File(FilePart::image(
            "image/png",
            FileSource::Bytes(bytes::Bytes::from(vec![0_u8; 1024 * 1024])),
        )),
    ]))];
    let heavy_bytes = serialized_fit_request_bytes(&heavy, &[]).unwrap();
    assert!(
        media_estimator
            .estimated_context_tokens(heavy_bytes)
            .is_some_and(|tokens| tokens >= 10_000)
    );
}

#[test]
fn large_pdf_contributes_zero_to_fit_estimate() {
    let pdf = file_history(vec![FilePart::document(
        "application/pdf",
        FileSource::Bytes(bytes::Bytes::from(vec![0_u8; 2 * 1024 * 1024])),
    )]);
    let empty = file_history(Vec::new());

    assert_eq!(
        estimated_request_tokens(&pdf, &[]).unwrap(),
        estimated_request_tokens(&empty, &[]).unwrap()
    );
}

#[test]
fn video_uses_flat_fit_cost_and_multiple_videos_trigger() {
    let video = || {
        FilePart::video(
            "video/mp4",
            FileSource::Bytes(bytes::Bytes::from(vec![0_u8; 1024 * 1024])),
        )
    };
    let empty_tokens = estimated_request_tokens(&file_history(Vec::new()), &[]).unwrap();
    let one_video_tokens = estimated_request_tokens(&file_history(vec![video()]), &[]).unwrap();
    let two_video_tokens =
        estimated_request_tokens(&file_history(vec![video(), video()]), &[]).unwrap();

    assert_eq!(VIDEO_FILE_FIT_SURROGATE_BYTES, 160_000);
    assert_eq!(one_video_tokens - empty_tokens, 40_000);
    assert_eq!(two_video_tokens - empty_tokens, 80_000);
    assert!(!usage_reaches_compaction_trigger(one_video_tokens, 50_000));
    assert!(usage_reaches_compaction_trigger(two_video_tokens, 50_000));
}

#[test]
fn real_usage_fit_is_inclusive_and_overflow_recovery_uses_elision_path() {
    let fits = |tokens| tokens <= 100;
    assert_eq!(raw_fit_from_real_usage(false, Some(99), fits), Some(true));
    assert_eq!(raw_fit_from_real_usage(false, Some(100), fits), Some(true));
    assert_eq!(raw_fit_from_real_usage(false, Some(101), fits), None);
    assert_eq!(raw_fit_from_real_usage(false, None, fits), None);
    assert_eq!(raw_fit_from_real_usage(true, Some(1), fits), Some(false));
}

#[test]
fn compaction_budget_uses_harness_or_native_limit_and_reserves_output() {
    let mut harness_binding = crate::test_support::model_binding();
    harness_binding.descriptor.capabilities.limits.context = Some(100_000);
    let mut policy = FrozenInternalAgentPolicy {
        agent: crate::test_support::agent_snapshot(
            "compaction",
            cookie_agent_protocol::AgentMode::Internal,
        ),
        models: vec![harness_binding.clone()],
        runtime: None,
        limits: InternalAgentLimits {
            max_output_tokens: 2_048,
            inherited_max_output_tokens: 0,
            timeout_ms: 30_000,
        },
        cache_strategies: vec![None],
    };
    assert!(compaction_input_fits(&harness_binding, &policy, 97_952));
    assert!(compaction_input_fits(&harness_binding, &policy, 97_953));

    let mut native_binding = harness_binding;
    native_binding.descriptor.capabilities.compaction = CompactionCapability::Native;
    native_binding.descriptor.capabilities.limits.context = Some(50_000);
    assert_eq!(
        native_compaction_input_budget(&native_binding, &policy),
        47_952
    );

    policy.limits.max_output_tokens = 0;
    assert_eq!(
        native_compaction_input_budget(&native_binding, &policy),
        47_952
    );
    native_binding.descriptor.capabilities.limits.output = None;
    assert_eq!(
        native_compaction_input_budget(&native_binding, &policy),
        50_000 - DEFAULT_COMPACTION_OUTPUT_RESERVE_TOKENS
    );
    native_binding.descriptor.capabilities.limits.context = Some(10_000);
    assert_eq!(native_compaction_input_budget(&native_binding, &policy), 1);
}

#[test]
fn compaction_raw_fit_accepts_any_fitting_fallback_order() {
    let mut small = crate::test_support::model_binding_named("fallback-zero");
    small.descriptor.capabilities.limits.context = Some(4_096);
    let mut large = crate::test_support::model_binding_named("fallback-one");
    large.descriptor.capabilities.limits.context = Some(200_000);
    let owner = crate::test_support::model_binding();
    let mut policy = FrozenInternalAgentPolicy {
        agent: crate::test_support::agent_snapshot(
            "compaction",
            cookie_agent_protocol::AgentMode::Internal,
        ),
        models: vec![small.clone(), large.clone()],
        runtime: None,
        limits: InternalAgentLimits {
            max_output_tokens: 2_048,
            inherited_max_output_tokens: 0,
            timeout_ms: 30_000,
        },
        cache_strategies: vec![None, None],
    };

    assert!(compaction_input_fits(&owner, &policy, 10_000));
    policy.models = vec![large, small];
    assert!(compaction_input_fits(&owner, &policy, 10_000));
    policy.models.truncate(1);
    policy.models[0].descriptor.capabilities.limits.context = Some(4_096);
    assert!(compaction_input_fits(&owner, &policy, 10_000));
}

#[test]
fn checkpoint_dedup_includes_the_exact_snapshot_boundary() {
    let session = SessionId::new_v7();
    let run = RunId::new_v7();
    let checkpoint = InternalSummaryCheckpoint::new(
        "summary".into(),
        cookie_agent_protocol::InternalAgentInvocationId::new_v7(),
        cookie_agent_protocol::InternalAgentRunId::new_v7(),
        SummaryByteLimit::new(1_024).unwrap(),
    )
    .unwrap();
    let events = vec![StoredEvent {
        engine_version: None,
        origin: None,
        session_id: session,
        run_id: Some(run),
        seq: 11,
        timestamp: jiff::Timestamp::now(),
        payload: Event::ContextCheckpointCommitted {
            commit: ContextCheckpointCommit {
                checkpoint: ContextCheckpoint::InternalSummary { checkpoint },
                boundaries: ContextCheckpointBoundaries {
                    source_from_seq: 1,
                    source_through_seq: 10,
                    input_through_seq: 10,
                    prior_checkpoint_seq: None,
                    recent_from_seq: None,
                },
                budgets: ContextCheckpointBudgets {
                    context_limit_tokens: 100,
                    trigger_tokens: 70,
                    input_tokens_before: 50,
                    input_tokens_after: 10,
                    max_summary_bytes: SummaryByteLimit::new(1_024).unwrap(),
                    keep_recent_tokens: 0,
                },
            },
        },
    }];
    assert!(checkpoint_covers_input(&events, 10));
    assert!(!checkpoint_covers_input(&events, 11));
}

#[test]
fn focus_is_appended_without_changing_the_fixed_instruction() {
    assert_eq!(compaction_instruction(None), COMPACTION_INSTRUCTION);
    assert_eq!(
        compaction_instruction(Some("preserve parser work")),
        format!("{COMPACTION_INSTRUCTION}\n\nUser-requested focus: preserve parser work")
    );
}

#[test]
fn compact_provider_request_is_the_assembled_normal_prefix_plus_one_instruction() {
    let temporary = tempfile::TempDir::new().expect("temp directory");
    let artifacts = crate::ArtifactRouter::open_flat(temporary.path().join("artifacts"))
        .expect("artifact store");
    let (runtime, binding) = crate::test_support::model_runtime_and_binding();
    let session = SessionId::new_v7();
    let run = RunId::new_v7();
    let events = vec![StoredEvent {
        engine_version: None,
        origin: None,
        session_id: session,
        run_id: Some(run),
        seq: 1,
        timestamp: jiff::Timestamp::now(),
        payload: Event::UserInputSubmitted {
            input: "work".into(),
        },
    }];
    let context =
        assemble_model_context(&events, &artifacts, &binding, "system").expect("assembled context");
    let tools = vec![ToolDefinition::new(
        "read",
        "Read a file",
        JsonSchema::new(serde_json::json!({
            "type": "object",
            "properties": {"filePath": {"type": "string"}},
            "required": ["filePath"]
        }))
        .expect("schema"),
    )];
    let model = runtime.resolve(&binding.selection).expect("resolved model");
    let normal_request =
        model.prepare_request(ModelRequest::new(context.history.clone()).with_tools(tools.clone()));
    let (compact_history, _) = compaction_history(context.history, None, "system");
    let compact_request =
        model.prepare_request(ModelRequest::new(compact_history).with_tools(tools));
    let normal = serde_json::to_value(normal_request).expect("normal provider request");
    let compact = serde_json::to_value(compact_request).expect("compact provider request");
    let normal_history = normal["history"].as_array().expect("normal history");
    let compact_history = compact["history"].as_array().expect("compact history");
    assert_eq!(
        serde_json::to_vec(normal_history).unwrap(),
        serde_json::to_vec(&compact_history[..normal_history.len()]).unwrap()
    );
    assert_eq!(compact_history.len(), normal_history.len() + 1);
    let mut normal_without_history = normal;
    let mut compact_without_history = compact;
    normal_without_history
        .as_object_mut()
        .unwrap()
        .remove("history");
    compact_without_history
        .as_object_mut()
        .unwrap()
        .remove("history");
    assert_eq!(normal_without_history, compact_without_history);
}

#[test]
fn elision_protects_recent_turns_and_requires_bulky_output() {
    let protected = HashSet::from([8, 9]);
    assert!(!should_elide_tool_output(
        9,
        &protected,
        false,
        TOOL_OUTPUT_ELISION_MIN_BYTES
    ));
    assert!(!should_elide_tool_output(
        7,
        &protected,
        false,
        TOOL_OUTPUT_ELISION_MIN_BYTES - 1
    ));
    assert!(should_elide_tool_output(
        7,
        &protected,
        false,
        TOOL_OUTPUT_ELISION_MIN_BYTES
    ));
    assert!(!should_elide_tool_output(
        7,
        &protected,
        true,
        TOOL_OUTPUT_ELISION_MIN_BYTES
    ));
}

#[test]
fn elision_and_estimator_share_emitted_part_byte_accounting() {
    let attachment = ToolAttachment {
        mime_type: MimeType::new("video/mp4").unwrap(),
        filename: Some("clip.mp4".into()),
        byte_length: 4,
        sha256: Sha256Digest::of_bytes(b"clip"),
        reference: ArtifactReference {
            uri: format!("artifact://sha256/{}", "a".repeat(64)),
        },
    };
    let result = ToolResult {
        display: None,
        retained_output: None,
        title: SafeDisplayText::new("Video").unwrap(),
        output: "x".repeat(60),
        metadata: serde_json::Value::Null,
        truncation: None,
        attachments: Vec::new(),
        additional_messages: vec![
            ToolEmittedMessage::new(
                ToolEmittedMessageRole::User,
                vec![
                    ToolEmittedContent::Text("e".repeat(4 * 1024)),
                    ToolEmittedContent::File(attachment.clone()),
                ],
            )
            .unwrap(),
        ],
    };
    let expected = fit_part_bytes(FitPart::Text(&result.output))
        + fit_part_bytes(FitPart::Text("e".repeat(4 * 1024).as_str()))
        + fit_part_bytes(FitPart::File(attachment.mime_type.as_str()));
    assert_eq!(
        fit_part_bytes(FitPart::File("video/mp4")),
        VIDEO_FILE_FIT_SURROGATE_BYTES
    );
    assert_eq!(elidable_bytes(&result), expected);
    assert!(expected >= TOOL_OUTPUT_ELISION_MIN_BYTES);
    assert!(should_elide_tool_output(
        1,
        &HashSet::new(),
        false,
        elidable_bytes(&result)
    ));

    let baseline = vec![
        HistoryTurn::tool(ToolMessage::new(vec![ToolResultPart::new(
            "call",
            ToolContent::Text(String::new()),
        )])),
        HistoryTurn::user(UserMessage::new(Vec::new())),
    ];
    let with_emitted_parts = vec![
        HistoryTurn::tool(ToolMessage::new(vec![ToolResultPart::new(
            "call",
            ToolContent::Text(result.output.clone()),
        )])),
        HistoryTurn::user(UserMessage::new(vec![
            InputPart::Text(TextPart::new("e".repeat(4 * 1024))),
            InputPart::File(FilePart::video(
                "video/mp4",
                FileSource::Bytes(b"clip".to_vec().into()),
            )),
        ])),
    ];
    let baseline_bytes = serialized_fit_request_bytes(&baseline, &[]).unwrap();
    let with_emitted_bytes = serialized_fit_request_bytes(&with_emitted_parts, &[]).unwrap();
    assert_eq!(
        with_emitted_bytes - baseline_bytes,
        expected,
        "estimator and selector must charge the same shared per-part bytes"
    );
}

#[test]
fn recent_budget_handles_zero_exact_fit_overbudget_and_huge_targets() {
    assert_eq!(effective_recent_budget(0, 100_000, 20_000), 0);
    assert_eq!(effective_recent_budget(16_384, 100_000, 16_384), 16_384);
    assert_eq!(effective_recent_budget(16_384, 100_000, 123), 123);
    assert_eq!(effective_recent_budget(16_384, 100_000, 0), 0);
    assert_eq!(effective_recent_budget(u64::MAX, 100_000, u64::MAX), 25_000);
    assert_eq!(
        effective_recent_budget(u64::MAX, u64::MAX, u64::MAX),
        u64::MAX / 4
    );
}

#[test]
fn recent_selection_keeps_only_fitting_complete_suffixes() {
    let select = |budget, limit| {
        select_recent_tail(vec![10, 20, 30], budget, 100, limit, |seq| {
            Ok(match seq {
                10 => 400,
                20 => 300,
                _ => 200,
            })
        })
        .unwrap()
    };
    assert_eq!(select(0, 1_000), None);
    assert_eq!(select(200, 1_000), Some(20));
    assert_eq!(select(199, 1_000), Some(30));
    assert_eq!(select(99, 1_000), None);
    assert_eq!(select(u64::MAX, 199), None);
    assert_eq!(select(u64::MAX, 300), Some(20));
    assert_eq!(select(u64::MAX, u64::MAX), Some(10));
}
