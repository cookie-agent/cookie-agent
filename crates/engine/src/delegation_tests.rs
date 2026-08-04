use std::collections::BTreeMap;

use cookie_agent_protocol::{
    AgentMode, AssistantToolCallRef, AttemptId, ClientRunId, EventPayload as Event, InvocationId,
    ModelCallId, ModelFinishReason, OperationFingerprint, PermissionAction, PersistedAssistantPart,
    PersistedModelTurn, PersistedToolResult as ToolResult, PreparedApprovalResource,
    PreparedBindingLifetime, PreparedCapabilityOperation, PreparedOperationIdentity,
    PreparedResourceDigest, PreparedResourceIdentity, ReplayDecision, ReplayDisposition, RunId,
    SafeCode, SafeDisplayText, SafeErrorMessage, SessionId, SessionOrigin, SessionStatus,
    Sha256Digest, ToolCallId, ToolCallPresentation, ToolCallStart, ToolTerminationOutcome, Usage,
};
use uuid::Uuid;

use crate::{
    DelegateHandle, Engine, completed_delegate_result, cwd_identity, invocation_id,
    model_history::wire_model,
    session_meta,
    test_support::{agent_snapshot, engine as test_engine, model_binding, run_selection},
};

const FINAL_REPORT: &str = "valid child final report";
const CHILD_WARNING: &str = "child replay warning";
const PARENT_WARNING: &str = "parent-owned warning";
const REPLAY_DIAGNOSTIC: &str = "child replay diagnostic";

#[derive(Clone, Copy)]
struct ParentLink {
    session: SessionId,
    run: RunId,
    call: ToolCallId,
    invocation: InvocationId,
}

impl ParentLink {
    fn new(session: SessionId, run: RunId, call: ToolCallId) -> Self {
        Self {
            session,
            run,
            call,
            invocation: invocation_id(session, run, call),
        }
    }
}

fn session_id(value: u128) -> SessionId {
    SessionId(Uuid::from_u128(value))
}

fn run_id(value: u128) -> RunId {
    RunId(Uuid::from_u128(value))
}

fn tool_call_id(value: u128) -> ToolCallId {
    ToolCallId(Uuid::from_u128(value))
}

fn operation() -> PreparedOperationIdentity {
    PreparedOperationIdentity::new(
        Sha256Digest::of_bytes(b"delegate args"),
        vec![cookie_agent_protocol::ApprovalCapability {
            action: PermissionAction::Delegate,
            operation: PreparedCapabilityOperation::new("delegate:spawn").expect("operation"),
        }],
        vec![PreparedApprovalResource {
            capability: PermissionAction::Delegate,
            canonical: PreparedResourceIdentity::new("agent:worker").expect("identity"),
            binding_digest: PreparedResourceDigest::from_canonical_binding_bytes(b"worker"),
            binding_lifetime: PreparedBindingLifetime::ProcessLocal,
            boundary: cookie_agent_protocol::ApprovalBoundary::Exact,
            source: cookie_agent_protocol::ApprovalResourceSource::PrimaryOperation,
        }],
        Sha256Digest::of_bytes(b"workspace"),
    )
    .expect("prepared operation")
}

fn turn(content: PersistedAssistantPart) -> PersistedModelTurn {
    PersistedModelTurn {
        content: vec![content],
        provider_options: BTreeMap::new(),
        finish_reason: ModelFinishReason::Stop,
        usage: Usage::default(),
        response_metadata: BTreeMap::new(),
        provider_metadata: BTreeMap::new(),
        native_replay: None,
    }
}

fn create_session(
    engine: &Engine,
    root: &std::path::Path,
    session: SessionId,
    origin: SessionOrigin,
    agent_name: &str,
    mode: AgentMode,
) {
    let cwd = cwd_identity(root).expect("cwd identity");
    let selection = run_selection(agent_name);
    let agent = agent_snapshot(agent_name, mode);
    engine
        .inner
        .store
        .create(
            session_meta(session, origin.clone(), cwd.clone(), selection.clone()),
            Event::SessionCreated {
                origin,
                cwd_identity: cwd,
                creation_selection: selection,
                creation_agent: Box::new(agent),
                model_snapshot_fingerprint: Sha256Digest::of_bytes(b"test models"),
            },
        )
        .expect("session");
    engine.spawn_actor(session);
}

fn start_run(engine: &Engine, session: SessionId, run: RunId, agent_name: &str, mode: AgentMode) {
    let agent = agent_snapshot(agent_name, mode);
    engine
        .append_direct(
            session,
            Some(run),
            Event::RunStarted {
                client_run_id: ClientRunId::new(format!("{agent_name}-run"))
                    .expect("client run id"),
                selection: run_selection(agent_name),
                selected_suffix: agent.fallback_chain.clone(),
                agent: Box::new(agent),
                input_through_seq: 1,
            },
        )
        .expect("run started");
}

fn install_parent(
    engine: &Engine,
    root: &std::path::Path,
    parent_session: SessionId,
    parent_run: RunId,
    call: ToolCallId,
) {
    create_session(
        engine,
        root,
        parent_session,
        SessionOrigin::Root,
        "parent",
        AgentMode::Primary,
    );
    start_run(
        engine,
        parent_session,
        parent_run,
        "parent",
        AgentMode::Primary,
    );
    let attempt = AttemptId::new_v7();
    let resolved = wire_model(&model_binding());
    engine
        .append_direct(
            parent_session,
            Some(parent_run),
            Event::ModelAttemptStarted {
                attempt_id: attempt,
                attempt_ordinal: 1,
                fallback_index: 0,
                retry_ordinal: 0,
                resolved_model: resolved.clone(),
                prompt_fingerprint: Sha256Digest::of_bytes(b"parent prompt"),
            },
        )
        .expect("parent attempt");
    let model_call = ModelCallId::new("delegate-model-call").expect("model call id");
    engine
        .append_direct(
            parent_session,
            Some(parent_run),
            Event::ModelTurnCommitted {
                attempt_id: attempt,
                model_turn_seq: 1,
                resolved_model: resolved,
                input_through_seq: 1,
                turn: turn(PersistedAssistantPart::ToolCall {
                    id: model_call.clone(),
                    provider_item_id: None,
                    name: SafeCode::new("delegate").expect("tool name"),
                    input: serde_json::json!({"agent":"worker","task":"report"}),
                    raw_input: None,
                    metadata: None,
                }),
                warnings: vec![SafeErrorMessage::new(PARENT_WARNING).expect("warning")],
            },
        )
        .expect("parent model turn");
    engine
        .append_direct(
            parent_session,
            Some(parent_run),
            Event::ToolCallStarted {
                start: ToolCallStart {
                    tool_call_id: call,
                    owner: AssistantToolCallRef {
                        model_turn_seq: 1,
                        content_index: 0,
                        model_call_id: model_call,
                        provider_item_id: None,
                    },
                    presentation: ToolCallPresentation {
                        title: SafeDisplayText::new("Delegate to worker").expect("title"),
                        primary_argument: Some(
                            SafeDisplayText::new("report").expect("primary argument"),
                        ),
                    },
                    operation_fingerprint: OperationFingerprint::from_prepared_operation(
                        &operation(),
                    ),
                },
            },
        )
        .expect("delegate call");
}

fn install_completed_child(
    engine: &Engine,
    root: &std::path::Path,
    parent: ParentLink,
    child_session: SessionId,
    child_run: RunId,
) {
    create_session(
        engine,
        root,
        child_session,
        SessionOrigin::Delegated {
            root_session_id: parent.session,
            parent_session_id: parent.session,
            parent_run_id: parent.run,
            parent_tool_call_id: parent.call,
            invocation_id: parent.invocation,
            depth: 1,
        },
        "worker",
        AgentMode::Subagent,
    );
    start_run(
        engine,
        child_session,
        child_run,
        "worker",
        AgentMode::Subagent,
    );
    let attempt = AttemptId::new_v7();
    let resolved = wire_model(&model_binding());
    engine
        .append_direct(
            child_session,
            Some(child_run),
            Event::ModelAttemptStarted {
                attempt_id: attempt,
                attempt_ordinal: 1,
                fallback_index: 0,
                retry_ordinal: 0,
                resolved_model: resolved.clone(),
                prompt_fingerprint: Sha256Digest::of_bytes(b"worker prompt"),
            },
        )
        .expect("child attempt");
    engine
        .append_direct(
            child_session,
            Some(child_run),
            Event::ModelReplayEvaluated {
                attempt_id: attempt,
                resolved_model: resolved.clone(),
                ordered_decisions: vec![ReplayDecision {
                    history_index: 0,
                    disposition: ReplayDisposition::DiscardedInvalidPayload {
                        reason: SafeErrorMessage::new(REPLAY_DIAGNOSTIC).expect("diagnostic"),
                    },
                }],
            },
        )
        .expect("child replay decision");
    engine
        .append_direct(
            child_session,
            Some(child_run),
            Event::ModelTurnCommitted {
                attempt_id: attempt,
                model_turn_seq: 1,
                resolved_model: resolved,
                input_through_seq: 1,
                turn: turn(PersistedAssistantPart::Text {
                    text: FINAL_REPORT.into(),
                    metadata: None,
                }),
                warnings: vec![SafeErrorMessage::new(CHILD_WARNING).expect("warning")],
            },
        )
        .expect("child model turn");
    engine
        .append_direct(
            child_session,
            Some(child_run),
            Event::RunCompleted {
                final_text: Some(FINAL_REPORT.into()),
            },
        )
        .expect("child completion");
}

fn completed_result(engine: &Engine, parent_session: SessionId, call: ToolCallId) -> ToolResult {
    engine
        .inner
        .store
        .get(parent_session)
        .expect("parent projection")
        .log
        .events()
        .into_iter()
        .find_map(|event| match event.payload {
            Event::ToolCallTerminated { termination }
                if termination.tool_call_id == call
                    && termination.outcome == ToolTerminationOutcome::Completed =>
            {
                termination.result
            }
            _ => None,
        })
        .expect("completed delegate result")
}

fn assert_session_local_ownership(
    engine: &Engine,
    parent_session: SessionId,
    child_session: SessionId,
    call: ToolCallId,
) -> ToolResult {
    let parent = engine
        .inner
        .store
        .get(parent_session)
        .expect("parent projection");
    let child = engine
        .inner
        .store
        .get(child_session)
        .expect("child projection");
    assert!(
        parent
            .log
            .events()
            .iter()
            .all(|event| event.session_id == parent_session)
    );
    assert!(
        child
            .log
            .events()
            .iter()
            .all(|event| event.session_id == child_session)
    );
    assert_eq!(parent.meta.creation_selection.agent.as_str(), "parent");
    assert_eq!(child.meta.creation_selection.agent.as_str(), "worker");
    assert!(matches!(
        child.meta.origin,
        SessionOrigin::Delegated {
            parent_session_id: found_parent,
            parent_tool_call_id: found_call,
            ..
        } if found_parent == parent_session && found_call == call
    ));

    let parent_warnings = parent
        .log
        .events()
        .into_iter()
        .find_map(|event| match event.payload {
            Event::ModelTurnCommitted { warnings, .. } => Some(warnings),
            _ => None,
        });
    assert_eq!(
        parent_warnings.expect("parent warning")[0].as_str(),
        PARENT_WARNING
    );
    let child_events = child.log.events();
    assert!(child_events.iter().any(|event| matches!(
        &event.payload,
        Event::ModelTurnCommitted { warnings, .. }
            if warnings.first().is_some_and(|warning| warning.as_str() == CHILD_WARNING)
    )));
    assert!(child_events.iter().any(|event| matches!(
        &event.payload,
        Event::ModelReplayEvaluated { ordered_decisions, .. }
            if matches!(ordered_decisions.as_slice(), [ReplayDecision {
                history_index: 0,
                disposition: ReplayDisposition::DiscardedInvalidPayload { reason },
            }] if reason.as_str() == REPLAY_DIAGNOSTIC)
    )));

    let result = completed_result(engine, parent_session, call);
    assert_eq!(result.title.as_str(), "Delegate report");
    assert_eq!(result.output, FINAL_REPORT);
    assert_eq!(
        result.metadata,
        serde_json::json!({
            "status": "completed",
            "child_session_id": child_session,
        })
    );
    assert_eq!(child.status, SessionStatus::Completed);
    result
}

#[tokio::test]
async fn live_delegate_result_keeps_child_warning_and_replay_diagnostics_child_local() {
    let root = tempfile::tempdir().expect("root");
    let engine = test_engine(root.path());
    let parent_session = session_id(1);
    let parent_run = run_id(2);
    let call = tool_call_id(3);
    let child_session = session_id(4);
    let child_run = run_id(5);
    let parent = ParentLink::new(parent_session, parent_run, call);
    install_parent(&engine, root.path(), parent_session, parent_run, call);
    install_completed_child(&engine, root.path(), parent, child_session, child_run);

    let result = engine
        .await_delegate(DelegateHandle {
            invocation_id: parent.invocation,
            child_session_id: child_session,
            child_run_id: child_run,
        })
        .await
        .expect("delegate result");
    engine
        .submit_tool_result(parent_session, parent_run, call, Ok(result))
        .await
        .expect("commit parent result");

    assert_session_local_ownership(&engine, parent_session, child_session, call);
    engine.shutdown().await;
}

#[tokio::test]
async fn restart_recovery_uses_the_same_session_local_completed_delegate_result() {
    let root = tempfile::tempdir().expect("root");
    let engine = test_engine(root.path());
    let parent_session = session_id(11);
    let parent_run = run_id(12);
    let call = tool_call_id(13);
    let child_run = run_id(15);
    let parent = ParentLink::new(parent_session, parent_run, call);
    install_parent(&engine, root.path(), parent_session, parent_run, call);
    let child_agent = agent_snapshot("worker", AgentMode::Subagent);
    let entry = engine
        .inner
        .journal
        .reserve(
            parent.invocation,
            parent_session,
            parent_run,
            call,
            child_agent.clone(),
            child_agent.fallback_chain.clone(),
            "restart-fixture".into(),
            crate::journal::DelegateRequestPayload {
                task: "report".into(),
                ..Default::default()
            },
        )
        .expect("journal reservation");
    let child_session = entry.reservation.child_session_id;
    install_completed_child(&engine, root.path(), parent, child_session, child_run);
    engine
        .ensure_parent_link_blocking(parent_session, parent_run, call, child_session)
        .expect("parent link");
    engine
        .inner
        .journal
        .mark_linked(parent.invocation)
        .expect("linked journal");
    engine
        .inner
        .journal
        .mark_run_started(parent.invocation, child_run)
        .expect("run journal");
    engine.shutdown().await;
    drop(engine);

    let reopened = test_engine(root.path());
    reopened
        .resume(parent_session)
        .await
        .expect("resume parent");
    let recovered = assert_session_local_ownership(&reopened, parent_session, child_session, call);
    let direct = completed_delegate_result(
        &reopened
            .inner
            .store
            .get(child_session)
            .expect("child projection"),
        Some(child_run),
        &reopened.inner.artifacts,
    )
    .expect("direct completed result");
    assert_eq!(recovered, direct);
    reopened.shutdown().await;
}
