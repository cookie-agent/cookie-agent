use std::{
    collections::BTreeMap,
    fs,
    io::{Cursor, Write},
    path::{Path, PathBuf},
};

use async_trait::async_trait;
use cookie_agent_protocol::{
    AgentMode, ApprovalBoundary, ApprovalCapability, ApprovalConstraints, ApprovalDecisionSource,
    ApprovalEvaluation, ApprovalId, ApprovalInternalDecision, ApprovalInternalDecisionKind,
    ApprovalReasonCode, ApprovalRequest, ApprovalResourceSource, ApprovalRespondErrorCode,
    ApprovalRespondParams, ApprovalStatus, ApprovalTrigger, ApprovalUserDecision,
    AssistantToolCallRef, AttemptId, ClientResponseId, ClientRunId, DecisionTrace,
    EventPayload as Event, ModelCallId, ModelFinishReason, OperationFingerprint, PermissionAction,
    PermissionEffect, PersistedAssistantPart, PersistedModelTurn, PreparedApprovalResource,
    PreparedBindingLifetime, PreparedCapabilityOperation, PreparedOperationIdentity,
    PreparedResourceDigest, PreparedResourceIdentity, RunId, SafeCode, SafeDisplayText, SessionId,
    SessionOrigin, Sha256Digest, ToolCallId, ToolCallPresentation, ToolCallStart,
    ToolTerminationOutcome, Usage,
};

use crate::{
    ApprovalOutcome, ArtifactStore, Engine, EngineError, PendingApproval, PreparedExecutor,
    PreparedTool, ToolCallFailureCode, ToolError, ToolExecutionContext, ToolResult,
    approval_records, approved_media_type, cwd_identity,
    model_history::wire_model,
    restart_approval_decision, restart_tool_failure, session_meta,
    test_support::{agent_snapshot, engine as test_engine, model_binding, run_selection},
    validate_attachment,
};

fn png() -> Vec<u8> {
    image(image::ImageFormat::Png)
}

fn gif() -> Vec<u8> {
    image(image::ImageFormat::Gif)
}

fn webp() -> Vec<u8> {
    image(image::ImageFormat::WebP)
}

fn jpeg() -> Vec<u8> {
    image(image::ImageFormat::Jpeg)
}

fn image(format: image::ImageFormat) -> Vec<u8> {
    let image = image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
        2,
        2,
        image::Rgba([10, 20, 30, 255]),
    ));
    let mut bytes = Cursor::new(Vec::new());
    image.write_to(&mut bytes, format).expect("encode image");
    bytes.into_inner()
}

fn pdf() -> Vec<u8> {
    use lopdf::xref::XrefType;

    let mut document = pdf_document();
    document.reference_table.cross_reference_type = XrefType::CrossReferenceTable;
    let mut bytes = Vec::new();
    document.save_to(&mut bytes).expect("encode classic PDF");
    bytes
}

fn modern_pdf() -> Vec<u8> {
    let mut document = pdf_document();
    let mut bytes = Vec::new();
    document.save_modern(&mut bytes).expect("encode modern PDF");
    bytes
}

fn pdf_document() -> lopdf::Document {
    use lopdf::{Document, Object, dictionary};

    let mut document = Document::with_version("1.5");
    let pages = document.new_object_id();
    let page = document.new_object_id();
    document.objects.insert(
        page,
        Object::Dictionary(dictionary! {
            "Type" => "Page",
            "Parent" => pages,
            "MediaBox" => vec![0.into(), 0.into(), 100.into(), 100.into()],
            "Resources" => dictionary! {},
        }),
    );
    document.objects.insert(
        pages,
        Object::Dictionary(dictionary! {
            "Type" => "Pages",
            "Kids" => vec![page.into()],
            "Count" => 1,
        }),
    );
    let catalog = document.add_object(dictionary! {
        "Type" => "Catalog",
        "Pages" => pages,
    });
    document.trailer.set("Root", catalog);
    document
}

fn pdf_with_compressed_streams(stream_sizes: &[usize]) -> Vec<u8> {
    use lopdf::{Object, Stream, dictionary, xref::XrefType};

    let mut document = pdf_document();
    document.reference_table.cross_reference_type = XrefType::CrossReferenceTable;
    for (index, size) in stream_sizes.iter().copied().enumerate() {
        let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::none());
        encoder
            .write_all(&vec![index as u8; size])
            .expect("encode stream payload");
        let compressed = encoder.finish().expect("finish stream payload");
        let stream = Stream::new(dictionary! {"Filter" => "FlateDecode"}, compressed);
        document.add_object(Object::Stream(stream));
    }
    let mut bytes = Vec::new();
    document.save_to(&mut bytes).expect("encode stream PDF");
    bytes
}

fn pdf_with_object_stream_bomb() -> Vec<u8> {
    use lopdf::{Object, SaveOptions, StringFormat, xref::XrefType};

    let mut document = pdf_document();
    document.reference_table.cross_reference_type = XrefType::CrossReferenceTable;
    document.add_object(Object::String(
        vec![b'A'; crate::media::PDF_DECOMPRESSION_BUDGET_FOR_TESTS + 1],
        StringFormat::Literal,
    ));
    let options = SaveOptions::builder()
        .use_object_streams(true)
        .use_xref_streams(false)
        .build();
    let mut bytes = Vec::new();
    document
        .save_with_options(&mut bytes, options)
        .expect("encode object-stream PDF");
    bytes
}

#[test]
fn structurally_valid_png_is_accepted() {
    assert_eq!(
        approved_media_type(Path::new("a.png"), &png()).expect("PNG"),
        Some("image/png")
    );
}

#[test]
fn structurally_valid_jpeg_is_accepted() {
    assert_eq!(
        approved_media_type(Path::new("a.jpg"), &jpeg()).expect("JPEG"),
        Some("image/jpeg")
    );
}

#[test]
fn structurally_valid_gif_is_accepted() {
    assert_eq!(
        approved_media_type(Path::new("a.gif"), &gif()).expect("GIF"),
        Some("image/gif")
    );
}

#[test]
fn structurally_valid_webp_is_accepted() {
    assert_eq!(
        approved_media_type(Path::new("a.webp"), &webp()).expect("WebP"),
        Some("image/webp")
    );
}

#[test]
fn structurally_valid_pdf_is_accepted() {
    let fixture = pdf();
    assert_eq!(
        approved_media_type(Path::new("a.pdf"), &fixture).expect("PDF"),
        Some("application/pdf")
    );
}

#[test]
fn valid_modern_pdf_is_rejected_by_classic_only_bounded_contract() {
    let fixture = modern_pdf();
    let (accepted, decode_work) = crate::media::pdf_validation_stats(&fixture);
    assert!(!accepted);
    assert_eq!(decode_work, 0);
    assert!(approved_media_type(Path::new("a.pdf"), &fixture).is_err());
}

#[test]
fn multi_stream_pdf_bomb_stays_within_shared_aggregate_decode_budget() {
    let fixture = pdf_with_compressed_streams(&[6 * 1024 * 1024; 3]);
    assert!(fixture.len() <= 20 * 1024 * 1024);
    let (accepted, decode_work) = crate::media::pdf_validation_stats(&fixture);
    assert!(!accepted);
    assert!(decode_work > 0);
    assert!(decode_work <= crate::media::PDF_DECOMPRESSION_BUDGET_FOR_TESTS);
}

#[test]
fn object_stream_bomb_is_rejected_before_any_decompression() {
    let fixture = pdf_with_object_stream_bomb();
    assert!(fixture.len() <= 20 * 1024 * 1024);
    let (accepted, decode_work) = crate::media::pdf_validation_stats(&fixture);
    assert!(!accepted);
    assert_eq!(decode_work, 0);
}

#[test]
fn truncated_png_is_rejected() {
    let mut bytes = png();
    bytes.pop();
    assert!(approved_media_type(Path::new("a.png"), &bytes).is_err());
}

#[test]
fn truncated_jpeg_is_rejected() {
    let mut bytes = jpeg();
    bytes.pop();
    assert!(approved_media_type(Path::new("a.jpg"), &bytes).is_err());
}

#[test]
fn truncated_gif_is_rejected() {
    let mut bytes = gif();
    bytes.pop();
    assert!(approved_media_type(Path::new("a.gif"), &bytes).is_err());
}

#[test]
fn truncated_webp_is_rejected() {
    let mut bytes = webp();
    bytes.pop();
    assert!(approved_media_type(Path::new("a.webp"), &bytes).is_err());
}

#[test]
fn truncated_pdf_is_rejected() {
    let mut bytes = pdf();
    bytes.truncate(bytes.len() - 8);
    assert!(approved_media_type(Path::new("a.pdf"), &bytes).is_err());
}

#[test]
fn malformed_jpeg_sof_and_sos_tables_are_rejected() {
    let mut bad_sof = jpeg();
    let sof = bad_sof
        .windows(2)
        .position(|window| window == [0xff, 0xc0])
        .expect("SOF");
    bad_sof[sof + 2..sof + 4].copy_from_slice(&2_u16.to_be_bytes());
    assert!(approved_media_type(Path::new("a.jpg"), &bad_sof).is_err());

    let mut bad_sos = jpeg();
    let sos = bad_sos
        .windows(2)
        .position(|window| window == [0xff, 0xda])
        .expect("SOS");
    bad_sos[sos + 6] = 0xff;
    assert!(approved_media_type(Path::new("a.jpg"), &bad_sos).is_err());
}

#[test]
fn header_only_vp8x_is_rejected() {
    let mut bytes = b"RIFF".to_vec();
    bytes.extend_from_slice(&22_u32.to_le_bytes());
    bytes.extend_from_slice(b"WEBPVP8X");
    bytes.extend_from_slice(&10_u32.to_le_bytes());
    bytes.extend_from_slice(&[0; 10]);
    assert!(approved_media_type(Path::new("a.webp"), &bytes).is_err());
}

#[test]
fn invalid_png_crc_and_pdf_xref_are_rejected() {
    let mut bad_png = png();
    bad_png[29] ^= 1;
    assert!(approved_media_type(Path::new("a.png"), &bad_png).is_err());

    let mut bad_pdf = pdf();
    let xref = bad_pdf
        .windows(5)
        .position(|window| window == b"xref\n")
        .expect("xref");
    bad_pdf[xref] = b'X';
    assert!(approved_media_type(Path::new("a.pdf"), &bad_pdf).is_err());
}

#[test]
fn invalid_container_and_segment_lengths_are_rejected() {
    let mut bad_png = png();
    bad_png[8..12].copy_from_slice(&u32::MAX.to_be_bytes());
    assert!(approved_media_type(Path::new("a.png"), &bad_png).is_err());

    let mut bad_jpeg = jpeg();
    let dqt = bad_jpeg
        .windows(2)
        .position(|window| window == [0xff, 0xdb])
        .expect("DQT");
    bad_jpeg[dqt + 2..dqt + 4].copy_from_slice(&1_u16.to_be_bytes());
    assert!(approved_media_type(Path::new("a.jpg"), &bad_jpeg).is_err());

    let mut bad_webp = webp();
    bad_webp[16..20].copy_from_slice(&u32::MAX.to_le_bytes());
    assert!(approved_media_type(Path::new("a.webp"), &bad_webp).is_err());

    let mut bad_gif = gif();
    let image_separator = bad_gif
        .iter()
        .position(|byte| *byte == 0x2c)
        .expect("image separator");
    bad_gif.truncate(image_separator + 11);
    assert!(approved_media_type(Path::new("a.gif"), &bad_gif).is_err());
}

#[test]
fn malformed_pdf_object_structure_is_rejected() {
    let mut bytes = pdf();
    let endobj = bytes
        .windows(6)
        .position(|window| window == b"endobj")
        .expect("endobj");
    bytes[endobj] = b'X';
    assert!(approved_media_type(Path::new("a.pdf"), &bytes).is_err());
}

#[test]
fn image_polyglot_trailer_is_rejected() {
    let mut bytes = png();
    bytes.extend_from_slice(b"<script>alert(1)</script>");
    assert!(approved_media_type(Path::new("a.png"), &bytes).is_err());
}

#[test]
fn pdf_polyglot_trailer_is_rejected() {
    let mut bytes = pdf();
    bytes.extend_from_slice(b"PK\x03\x04");
    assert!(approved_media_type(Path::new("a.pdf"), &bytes).is_err());
}

#[test]
fn attachment_mime_mismatch_is_rejected() {
    assert!(validate_attachment("image/jpeg", Path::new("a.png"), &png()).is_err());
}

#[test]
fn unsupported_attachment_is_rejected() {
    assert!(validate_attachment("text/plain", Path::new("a.txt"), b"hello").is_err());
}

#[test]
fn attachment_limit_is_twenty_mib_and_fails_before_retention() {
    let bytes = vec![0_u8; 20 * 1024 * 1024 + 1];
    let error = validate_attachment("image/png", Path::new("a.png"), &bytes)
        .expect_err("oversize attachment");
    assert!(matches!(error, ToolError::ResourceLimit(_)));
    assert!(matches!(error.code(), ToolCallFailureCode::ExecutionFailed));
}

#[test]
fn retained_attachment_survives_store_reopen() {
    let root = tempfile::tempdir().expect("artifact directory");
    let bytes = png();
    let store = ArtifactStore::open(root.path().to_owned()).expect("open store");
    let (_, digest) = store.retain(&bytes).expect("retain attachment");
    drop(store);
    let reopened = ArtifactStore::open(root.path().to_owned()).expect("reopen store");
    let mut file = reopened
        .open_existing(&digest)
        .expect("open retained attachment")
        .expect("attachment exists");
    let mut restored = Vec::new();
    std::io::Read::read_to_end(&mut file, &mut restored).expect("read attachment");
    assert_eq!(restored, bytes);
}

#[test]
fn answered_approval_without_execution_loses_process_local_capability_on_restart() {
    let failure = restart_tool_failure();
    assert!(matches!(
        failure.code,
        ToolCallFailureCode::PreparedCapabilityLost
    ));
}

#[test]
fn pending_escalation_is_finalized_as_prepared_capability_lost() {
    let decision = restart_approval_decision();
    assert_eq!(
        decision.reason_code,
        ApprovalReasonCode::PreparedCapabilityLost
    );
}

struct FileIdentityExecutor {
    path: PathBuf,
    expected: Sha256Digest,
}

#[async_trait]
impl PreparedExecutor for FileIdentityExecutor {
    async fn revalidate(&self) -> Result<(), ToolError> {
        let bytes = fs::read(&self.path)
            .map_err(|error| ToolError::operation_changed(error.to_string()))?;
        if Sha256Digest::of_bytes(&bytes) != self.expected {
            return Err(ToolError::operation_changed(
                "prepared filesystem content changed before approval response",
            ));
        }
        Ok(())
    }

    async fn execute(self: Box<Self>, _: ToolExecutionContext) -> Result<ToolResult, ToolError> {
        unreachable!("approval response test never executes the tool")
    }
}

fn approval_policy() -> cookie_agent_protocol::AgentSnapshot {
    agent_snapshot("test", AgentMode::Primary)
}

fn approval_operation(binding: &[u8]) -> PreparedOperationIdentity {
    approval_operation_with_lifetime(binding, PreparedBindingLifetime::ProcessLocal)
}

fn approval_operation_with_lifetime(
    binding: &[u8],
    lifetime: PreparedBindingLifetime,
) -> PreparedOperationIdentity {
    let (action, operation, resource) = if lifetime == PreparedBindingLifetime::RestartStable {
        (
            PermissionAction::Bash,
            "bash:execute",
            "command:approval-fixture",
        )
    } else {
        (PermissionAction::Read, "read:read", "file:approval-fixture")
    };
    PreparedOperationIdentity::new(
        Sha256Digest::of_bytes(b"read fixture"),
        vec![ApprovalCapability {
            action,
            operation: PreparedCapabilityOperation::new(operation).expect("operation"),
        }],
        vec![PreparedApprovalResource {
            capability: action,
            canonical: PreparedResourceIdentity::new(resource).expect("identity"),
            binding_digest: PreparedResourceDigest::from_canonical_binding_bytes(binding),
            binding_lifetime: lifetime,
            boundary: ApprovalBoundary::Exact,
            source: ApprovalResourceSource::PrimaryOperation,
        }],
        Sha256Digest::of_bytes(b"workspace"),
    )
    .expect("prepared operation")
}

fn install_approval_session(
    engine: &Engine,
    root: &Path,
    session: SessionId,
    run: RunId,
    tool_call: Option<ToolCallId>,
) {
    let policy = approval_policy();
    let prompt_fingerprint = policy.prompt_fingerprint.clone();
    let cwd = cwd_identity(root).expect("cwd identity");
    let selection = run_selection("test");
    engine
        .inner
        .store
        .create(
            session_meta(session, SessionOrigin::Root, cwd.clone(), selection.clone()),
            Event::SessionCreated {
                origin: SessionOrigin::Root,
                cwd_identity: cwd,
                creation_selection: selection.clone(),
                creation_agent: Box::new(policy.clone()),
                model_snapshot_fingerprint: Sha256Digest::of_bytes(b"test models"),
            },
        )
        .expect("session");
    engine.spawn_actor(session);
    engine
        .append_direct(
            session,
            Some(run),
            Event::RunStarted {
                client_run_id: ClientRunId::new("approval-fixture").expect("client run id"),
                selection,
                selected_suffix: policy.fallback_chain.clone(),
                agent: Box::new(policy),
                input_through_seq: 1,
            },
        )
        .expect("run started");
    let Some(tool_call) = tool_call else {
        return;
    };
    let attempt = AttemptId::new_v7();
    let resolved = wire_model(&model_binding());
    engine
        .append_direct(
            session,
            Some(run),
            Event::ModelAttemptStarted {
                attempt_id: attempt,
                attempt_ordinal: 1,
                fallback_index: 0,
                retry_ordinal: 0,
                resolved_model: resolved.clone(),
                prompt_fingerprint,
            },
        )
        .expect("model attempt");
    let model_call_id = ModelCallId::new("model-call").expect("model call id");
    engine
        .append_direct(
            session,
            Some(run),
            Event::ModelTurnCommitted {
                attempt_id: attempt,
                model_turn_seq: 1,
                resolved_model: resolved,
                input_through_seq: 1,
                turn: PersistedModelTurn {
                    content: vec![PersistedAssistantPart::ToolCall {
                        id: model_call_id.clone(),
                        provider_item_id: None,
                        name: SafeCode::new("write").expect("tool name"),
                        input: serde_json::json!({"filePath":"target","content":"must-not-run"}),
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
        )
        .expect("model turn");
    engine
        .append_direct(
            session,
            Some(run),
            Event::ToolCallStarted {
                start: ToolCallStart {
                    tool_call_id: tool_call,
                    owner: AssistantToolCallRef {
                        model_turn_seq: 1,
                        content_index: 0,
                        model_call_id,
                        provider_item_id: None,
                    },
                    presentation: ToolCallPresentation {
                        title: SafeDisplayText::new("Write target").expect("title"),
                        primary_argument: Some(
                            SafeDisplayText::new("target").expect("primary argument"),
                        ),
                    },
                    operation_fingerprint: OperationFingerprint::from_prepared_operation(
                        &approval_operation(b"tool start"),
                    ),
                },
            },
        )
        .expect("tool started");
}

async fn persisted_reopen_case(
    root: &Path,
    user_decision: Option<ApprovalUserDecision>,
    allow_tree_grant: bool,
) -> (Engine, SessionId, ToolCallId, ApprovalId) {
    let engine = approval_test_engine(root);
    let session = SessionId::new_v7();
    let run = RunId::new_v7();
    let tool_call = ToolCallId::new_v7();
    install_approval_session(&engine, root, session, run, Some(tool_call));
    let lifetime = if allow_tree_grant {
        PreparedBindingLifetime::RestartStable
    } else {
        PreparedBindingLifetime::ProcessLocal
    };
    let operation = approval_operation_with_lifetime(b"persisted binding", lifetime);
    let evaluation = ApprovalEvaluation {
        resource_digest: operation.resources()[0].binding_digest.clone(),
        effect: PermissionEffect::Ask,
        trace: DecisionTrace {
            action: operation.resources()[0].capability,
            normalized_resource: "fixture".into(),
            candidates: Vec::new(),
            effect: PermissionEffect::Ask,
            precedence_reason: "fixture".into(),
        },
    };
    let approval_id = ApprovalId::new_v7();
    let request = ApprovalRequest::new(
        approval_id,
        1,
        ApprovalTrigger::PermissionPolicy,
        operation,
        vec![evaluation.clone()],
        ApprovalConstraints {
            allow_once: true,
            allow_tree_grant,
            cancellable: true,
            expires_at: None,
        },
    )
    .expect("request");
    for event in [
        Event::ApprovalRequested {
            request: request.clone(),
        },
        Event::ApprovalEvaluated {
            approval_id,
            decision: ApprovalInternalDecision {
                decision: ApprovalInternalDecisionKind::Escalate,
                source: ApprovalDecisionSource::InternalAgent,
                reason_code: ApprovalReasonCode::Escalated,
                evaluations: vec![evaluation],
            },
        },
        Event::ApprovalEscalated {
            approval_id,
            reason_code: ApprovalReasonCode::Escalated,
        },
    ] {
        engine
            .append_direct(session, Some(run), event)
            .expect("approval lifecycle");
    }
    if let Some(decision) = user_decision {
        engine
            .append_direct(
                session,
                Some(run),
                Event::ApprovalUserDecisionRecorded {
                    approval_id,
                    client_response_id: ClientResponseId::new("persisted-response")
                        .expect("client response id"),
                    decision,
                    feedback: None,
                },
            )
            .expect("user decision");
    }
    engine.shutdown().await;
    drop(engine);
    let reopened = approval_test_engine(root);
    (reopened, session, tool_call, approval_id)
}

fn assert_reopen_capability_lost(
    engine: &Engine,
    session: SessionId,
    tool_call: ToolCallId,
    approval_id: ApprovalId,
) {
    let events = engine
        .inner
        .store
        .get(session)
        .expect("session")
        .log
        .events();
    assert!(events.iter().any(|event| {
        matches!(
            &event.payload,
            Event::ToolCallTerminated { termination }
                if termination.tool_call_id == tool_call
                    && termination.outcome == ToolTerminationOutcome::Interrupted
                    && termination.error.as_ref().is_some_and(|error| {
                        error.code.as_str() == "prepared_capability_lost"
                    })
        )
    }));
    assert!(!events.iter().any(|event| {
        matches!(
            &event.payload,
            Event::ToolCallTerminated { termination }
                if termination.tool_call_id == tool_call
                    && termination.outcome == ToolTerminationOutcome::Completed
        )
    }));
    let record = approval_records(session, &events)
        .remove(&approval_id)
        .expect("approval record");
    assert_eq!(record.status, ApprovalStatus::Cancelled);
    assert_eq!(
        record
            .final_decision
            .expect("terminal decision")
            .reason_code,
        ApprovalReasonCode::PreparedCapabilityLost
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event.payload, Event::TreeApprovalGrantCommitted { .. }))
    );
    assert!(engine.list_approvals(session, None).tree_grants.is_empty());
}

#[tokio::test]
async fn reopen_terminalizes_escalated_pending_capability_as_lost() {
    let root = tempfile::tempdir().expect("root");
    let (engine, session, tool_call, approval_id) =
        persisted_reopen_case(root.path(), None, false).await;
    assert_reopen_capability_lost(&engine, session, tool_call, approval_id);
    engine.shutdown().await;
}

#[tokio::test]
async fn reopen_discards_recorded_approval_before_execution() {
    let root = tempfile::tempdir().expect("root");
    let (engine, session, tool_call, approval_id) =
        persisted_reopen_case(root.path(), Some(ApprovalUserDecision::ApproveOnce), false).await;
    assert_reopen_capability_lost(&engine, session, tool_call, approval_id);
    engine.shutdown().await;
}

#[tokio::test]
async fn reopen_discards_tree_scope_decision_without_creating_grant() {
    let root = tempfile::tempdir().expect("root");
    let (engine, session, tool_call, approval_id) =
        persisted_reopen_case(root.path(), Some(ApprovalUserDecision::ApproveTree), true).await;
    assert_reopen_capability_lost(&engine, session, tool_call, approval_id);
    engine.shutdown().await;
}

fn approval_test_engine(root: &Path) -> Engine {
    test_engine(root)
}

fn install_escalated_approval(
    engine: &Engine,
    root: &Path,
    executor: Box<dyn PreparedExecutor>,
) -> (
    ApprovalRespondParams,
    SessionId,
    tokio::sync::oneshot::Receiver<ApprovalOutcome>,
) {
    let session = SessionId::new_v7();
    let run = RunId::new_v7();
    install_approval_session(engine, root, session, run, None);
    let operation = approval_operation(b"filesystem binding");
    let evaluation = ApprovalEvaluation {
        resource_digest: operation.resources()[0].binding_digest.clone(),
        effect: PermissionEffect::Ask,
        trace: DecisionTrace {
            action: PermissionAction::Read,
            normalized_resource: root.join("target.txt").display().to_string(),
            candidates: Vec::new(),
            effect: PermissionEffect::Ask,
            precedence_reason: "fixture".into(),
        },
    };
    let approval_id = ApprovalId::new_v7();
    let request = ApprovalRequest::new(
        approval_id,
        1,
        ApprovalTrigger::PermissionPolicy,
        operation,
        vec![evaluation.clone()],
        ApprovalConstraints {
            allow_once: true,
            allow_tree_grant: false,
            cancellable: true,
            expires_at: None,
        },
    )
    .expect("request");
    engine
        .append_direct(
            session,
            Some(run),
            Event::ApprovalRequested {
                request: request.clone(),
            },
        )
        .expect("requested");
    engine
        .append_direct(
            session,
            Some(run),
            Event::ApprovalEvaluated {
                approval_id,
                decision: ApprovalInternalDecision {
                    decision: ApprovalInternalDecisionKind::Escalate,
                    source: ApprovalDecisionSource::InternalAgent,
                    reason_code: ApprovalReasonCode::Escalated,
                    evaluations: vec![evaluation],
                },
            },
        )
        .expect("evaluated");
    engine
        .append_direct(
            session,
            Some(run),
            Event::ApprovalEscalated {
                approval_id,
                reason_code: ApprovalReasonCode::Escalated,
            },
        )
        .expect("escalated");
    let prepared = PreparedTool::new(request.operation().clone(), None, executor);
    let (sender, receiver) = tokio::sync::oneshot::channel();
    engine
        .inner
        .pending_approvals
        .lock()
        .expect("pending approvals")
        .insert(
            (session, approval_id),
            PendingApproval {
                sender,
                executor: prepared.executor.clone(),
            },
        );
    (
        ApprovalRespondParams {
            session_id: session,
            approval_id,
            request_revision: 1,
            operation_fingerprint: OperationFingerprint::from_prepared_operation(
                request.operation(),
            ),
            client_response_id: ClientResponseId::new("response-fixture")
                .expect("client response id"),
            decision: ApprovalUserDecision::ApproveOnce,
            feedback: None,
        },
        session,
        receiver,
    )
}

#[tokio::test]
async fn approval_respond_revalidates_filesystem_binding_before_recording_user_decision() {
    let root = tempfile::tempdir().expect("root");
    let target = root.path().join("target.txt");
    fs::write(&target, "original").expect("fixture");
    let engine = approval_test_engine(root.path());
    let expected = Sha256Digest::of_bytes(b"original");
    let (params, session, receiver) = install_escalated_approval(
        &engine,
        root.path(),
        Box::new(FileIdentityExecutor {
            path: target.clone(),
            expected,
        }),
    );
    fs::rename(&target, root.path().join("old-target.txt")).expect("swap old target");
    fs::write(&target, "replacement").expect("swap replacement");

    let error = engine
        .approval_respond(params)
        .await
        .expect_err("changed capability must reject approval response");
    let EngineError::ApprovalResponse(failure) = error else {
        panic!("unexpected error: {error}")
    };
    assert_eq!(failure.code, ApprovalRespondErrorCode::OperationChanged);
    let outcome = receiver.await.expect("approval outcome");
    assert!(!outcome.approved);
    let events = engine
        .inner
        .store
        .get(session)
        .expect("session")
        .log
        .events();
    let record = approval_records(session, &events)
        .into_values()
        .next()
        .expect("approval record");
    assert_eq!(record.status, ApprovalStatus::Cancelled);
    assert_eq!(
        record.final_decision.expect("final decision").reason_code,
        ApprovalReasonCode::OperationChanged
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event.payload, Event::ApprovalUserDecisionRecorded { .. }))
    );
    engine.shutdown().await;
}
