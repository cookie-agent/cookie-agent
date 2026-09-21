use std::sync::{Arc, atomic::Ordering};

use cookie_agent_protocol::{
    ApprovalDecisionSource, ApprovalFinalOutcome, ApprovalId, ApprovalInternalDecisionKind,
    ApprovalReasonCode, ApprovalRespondErrorCode, ApprovalRespondParams, ApprovalStatus,
    ApprovalUserDecision, ClientResponseId, ClientRunId, EventPayload, InternalAgentKind,
    PermissionAction, PermissionEffect, PermissionMode, RunStartParams, SessionStatus,
    ToolTerminationOutcome, WildcardPattern,
};

use jiff::Timestamp;

use crate::EngineError;

use super::support::*;

#[tokio::test]
async fn approval_batch_blocks_auto_allowed_tools_and_serializes_asks() {
    let (endpoint, responses, captured) = scripted_channel_server(2).await;
    responses
        .send(MatchedScriptedResponse::last_message_role(
            "user",
            scripted_tool_batch_body(&[
                (
                    "allowed-read",
                    "parallel_read",
                    serde_json::json!({"name":"allowed-read"}),
                ),
                (
                    "first-ask",
                    "parallel_bash",
                    serde_json::json!({"name":"ask-one"}),
                ),
                (
                    "denied-write",
                    "parallel_write",
                    serde_json::json!({"name":"blocked"}),
                ),
                (
                    "second-ask",
                    "parallel_bash",
                    serde_json::json!({"name":"ask-two"}),
                ),
            ]),
        ))
        .expect("approval batch response");
    responses
        .send(MatchedScriptedResponse::last_message_role(
            "tool",
            scripted_text_body("approval batch complete"),
        ))
        .expect("approval completion response");
    let (fixture, selection) = custom_fixture_with_endpoint_and_primary_agent(
        &endpoint,
        "---\ndescription: Approval batch test\nmode: primary\nenabled: true\nmodels: [{ model: \"custom.test/group/model\", variant: base }]\npermissions:\n  read: allow\n  bash: ask\n  write:\n    allowed: allow\n    \"*\": deny\n---\nResolve all asks before tools run.\n",
    );
    let state = Arc::new(ParallelToolState::default());
    fixture
        .engine
        .register_tool_provider(Arc::new(TestParallelToolProvider {
            state: Arc::clone(&state),
            barrier: None,
        }));
    let session = fixture
        .engine
        .create_session(selection.clone())
        .expect("approval batch session");
    fixture
        .engine
        .set_permission_mode(session.session_id, PermissionMode::Ask)
        .expect("ask mode");
    let run = fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: session.session_id,
                client_run_id: ClientRunId::new("approval-batch").expect("client run ID"),
                selection,
                input: "run approval batch".into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("approval batch run")
        .run_id;

    let first = wait_for_escalated_approval(&fixture.engine, session.session_id).await;
    let first_json = serde_json::to_value(&first.request).expect("first approval JSON");
    assert_eq!(
        first_json["evaluations"][0]["trace"]["normalized_resource"],
        "ask-one"
    );
    assert_eq!(state.started.load(Ordering::SeqCst), 0);
    assert_eq!(
        fixture
            .engine
            .list_approvals(session.session_id, Some(ApprovalStatus::Escalated))
            .approvals
            .len(),
        1
    );
    approve_once(&fixture.engine, &first, "approval-batch-first").await;
    let second = wait_for_escalated_approval(&fixture.engine, session.session_id).await;
    let second_json = serde_json::to_value(&second.request).expect("second approval JSON");
    assert_eq!(
        second_json["evaluations"][0]["trace"]["normalized_resource"],
        "ask-two"
    );
    assert_eq!(state.started.load(Ordering::SeqCst), 0);
    approve_once(&fixture.engine, &second, "approval-batch-second").await;
    wait_for_session_not_running(&fixture.engine, session.session_id).await;

    assert_eq!(state.started.load(Ordering::SeqCst), 3);
    let events = fixture
        .engine
        .inner
        .store
        .get(session.session_id)
        .expect("approval batch projection")
        .log
        .events();
    let terminations = events
        .iter()
        .filter_map(|event| match &event.payload {
            EventPayload::ToolCallTerminated { termination } if event.run_id == Some(run) => {
                Some((
                    termination.owner.model_call_id.as_str(),
                    termination.outcome,
                ))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(terminations.len(), 4);
    assert!(terminations.iter().any(|(id, outcome)| {
        *id == "denied-write" && *outcome == ToolTerminationOutcome::Failed
    }));
    assert_eq!(
        with_watchdog("captured fixture completion", captured)
            .await
            .expect("approval batch server")
            .len(),
        2
    );
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn cancellation_during_approval_terminates_batch_without_execution() {
    let (endpoint, responses, captured) = scripted_channel_server(1).await;
    responses
        .send(MatchedScriptedResponse::last_message_role(
            "user",
            scripted_tool_batch_body(&[
                (
                    "approval-cancel-read-one",
                    "parallel_read",
                    serde_json::json!({"name":"read-one"}),
                ),
                (
                    "approval-cancel-ask",
                    "parallel_bash",
                    serde_json::json!({"name":"needs-approval"}),
                ),
                (
                    "approval-cancel-read-two",
                    "parallel_read",
                    serde_json::json!({"name":"read-two"}),
                ),
            ]),
        ))
        .expect("approval cancellation response");
    let (fixture, selection) = custom_fixture_with_endpoint_and_primary_agent(
        &endpoint,
        "---\ndescription: Approval cancellation test\nmode: primary\nenabled: true\nmodels: [{ model: \"custom.test/group/model\", variant: base }]\npermissions:\n  read: allow\n  bash: ask\n---\nCancel while approval is pending.\n",
    );
    let state = Arc::new(ParallelToolState::default());
    fixture
        .engine
        .register_tool_provider(Arc::new(TestParallelToolProvider {
            state: Arc::clone(&state),
            barrier: None,
        }));
    let session = fixture
        .engine
        .create_session(selection.clone())
        .expect("approval cancellation session");
    fixture
        .engine
        .set_permission_mode(session.session_id, PermissionMode::Ask)
        .expect("ask mode");
    let run = fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: session.session_id,
                client_run_id: ClientRunId::new("approval-cancellation").expect("client run ID"),
                selection,
                input: "start approval batch".into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("approval cancellation run")
        .run_id;
    let _approval = wait_for_escalated_approval(&fixture.engine, session.session_id).await;
    assert_eq!(state.started.load(Ordering::SeqCst), 0);
    fixture.engine.cancel_run(run).await.expect("cancel run");
    wait_for_session_not_running(&fixture.engine, session.session_id).await;

    assert_eq!(state.started.load(Ordering::SeqCst), 0);
    let events = fixture
        .engine
        .inner
        .store
        .get(session.session_id)
        .expect("approval cancellation projection")
        .log
        .events();
    let starts = events
        .iter()
        .filter(|event| {
            event.run_id == Some(run)
                && matches!(event.payload, EventPayload::ToolCallStarted { .. })
        })
        .count();
    let terminations = events
        .iter()
        .filter(|event| {
            event.run_id == Some(run)
                && matches!(event.payload, EventPayload::ToolCallTerminated { .. })
        })
        .count();
    assert_eq!((starts, terminations), (3, 3));
    assert_eq!(
        with_watchdog("captured fixture completion", captured)
            .await
            .expect("approval cancellation server")
            .len(),
        1
    );
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn internal_agent_ask_transaction_persists_escalation_and_pending_approval() {
    let (endpoint, captured) = scripted_approval_server(r#"{"decision":"ask"}"#).await;
    let (fixture, selection) = approval_fixture_with_endpoint(&endpoint);
    let executed = Arc::new(TestFlag::default());
    fixture
        .engine
        .register_tool_provider(Arc::new(TestWriteProvider {
            executed: Arc::clone(&executed),
        }));
    let session = fixture
        .engine
        .create_session(selection.clone())
        .expect("approval session");
    fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: session.session_id,
                client_run_id: cookie_agent_protocol::ClientRunId::new("ask-transaction")
                    .expect("run ID"),
                selection,
                input: "request the write tool".to_owned(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("accepted approval run");

    let approval = wait_for_escalated_approval(&fixture.engine, session.session_id).await;
    let approval_id = approval.request.approval_id();
    let events = fixture
        .engine
        .inner
        .store
        .get(session.session_id)
        .expect("approval projection")
        .log
        .events();
    let lifecycle = events
        .iter()
        .filter(|event| match &event.payload {
            EventPayload::ApprovalRequested { request } => request.approval_id() == approval_id,
            EventPayload::ApprovalEvaluated {
                approval_id: event_approval_id,
                ..
            }
            | EventPayload::ApprovalEscalated {
                approval_id: event_approval_id,
                ..
            } => *event_approval_id == approval_id,
            _ => false,
        })
        .collect::<Vec<_>>();
    assert_eq!(lifecycle.len(), 3);
    assert!(matches!(
        lifecycle[0].payload,
        EventPayload::ApprovalRequested { .. }
    ));
    assert!(matches!(
        &lifecycle[1].payload,
        EventPayload::ApprovalEvaluated {
            decision,
            ..
        }
            if decision.decision == ApprovalInternalDecisionKind::Escalate
                && decision.source == ApprovalDecisionSource::InternalAgent
                && decision.reason_code == ApprovalReasonCode::Escalated
    ));
    assert!(matches!(
        lifecycle[2].payload,
        EventPayload::ApprovalEscalated { .. }
    ));
    assert!(
        fixture
            .engine
            .inner
            .pending_approvals
            .lock()
            .expect("pending approvals lock")
            .contains_key(&(session.session_id, approval_id))
    );
    assert_eq!(
        fixture
            .engine
            .list_approvals(session.session_id, Some(ApprovalStatus::Escalated))
            .approvals
            .len(),
        1
    );
    assert!(!events.iter().any(|event| matches!(
        &event.payload,
        EventPayload::ToolCallTerminated { termination }
            if termination.outcome == ToolTerminationOutcome::Failed
    )));

    approve_once(&fixture.engine, &approval, "ask-transaction-approval").await;
    wait_for_tool_execution(&fixture.engine, session.session_id, &executed).await;
    captured.abort();
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn overlay_epoch_change_rejects_pending_tree_grant_commit() {
    let (endpoint, captured) = scripted_approval_server(r#"{"decision":"ask"}"#).await;
    let (fixture, selection) = approval_fixture_with_endpoint(&endpoint);
    fixture
        .engine
        .register_tool_provider(Arc::new(TestWriteProvider {
            executed: Arc::new(TestFlag::default()),
        }));
    let session = fixture
        .engine
        .create_session(selection.clone())
        .expect("approval session");
    fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: session.session_id,
                client_run_id: ClientRunId::new("overlay-epoch").expect("run ID"),
                selection,
                input: "request the write tool".into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("run");
    let approval = wait_for_escalated_approval(&fixture.engine, session.session_id).await;
    fixture
        .engine
        .set_session_permission(
            session.session_id,
            PermissionAction::Write,
            WildcardPattern::new("*").expect("wildcard"),
            PermissionEffect::Deny,
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("tighten overlay");
    let request_revision = serde_json::to_value(&approval.request)
        .expect("approval request JSON")
        .get("revision")
        .and_then(serde_json::Value::as_u64)
        .expect("approval request revision");

    let error = fixture
        .engine
        .approval_respond(
            ApprovalRespondParams {
                session_id: session.session_id,
                approval_id: approval.request.approval_id(),
                request_revision,
                operation_fingerprint: approval.request.operation_fingerprint().clone(),
                client_response_id: ClientResponseId::new("overlay-epoch-tree")
                    .expect("response ID"),
                decision: ApprovalUserDecision::ApproveTree,
                feedback: None,
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect_err("changed overlay must reject tree grant");

    assert!(matches!(
        error,
        EngineError::ApprovalResponse(failure)
            if failure.code == ApprovalRespondErrorCode::OperationChanged
    ));
    assert!(
        fixture
            .engine
            .inner
            .approvals
            .for_root(session.session_id)
            .is_empty()
    );
    assert!(
        !fixture
            .engine
            .inner
            .pending_approvals
            .lock()
            .expect("pending approvals")
            .contains_key(&(session.session_id, approval.request.approval_id()))
    );
    captured.abort();
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn repeated_approvals_remain_stateless_and_reuse_the_user_request_prefix() {
    let (endpoint, captured) =
        scripted_two_evaluated_writes_server(r#"{"decision":"allow"}"#).await;
    let (fixture, selection) = custom_fixture_with_endpoint_primary_and_internal(
        &endpoint,
        "---\ndescription: Approval test agent\nmode: primary\nenabled: true\nmodels: [{ model: \"custom.test/group/model\", variant: base }]\npermissions:\n  write: ask\n---\nTest approval flow.\n",
        Some((
            "approval.md",
            "---\ndescription: Persistent approval evaluator\nmode: internal\nenabled: true\nmodels: [{ model: \"${parent_model}\" }]\nlimits: { timeout_ms: 30000, max_output_tokens: 128 }\npermissions: {}\n---\nEvaluate approval requests conservatively.\n",
        )),
        None,
        false,
    );
    let executed = Arc::new(TestFlag::default());
    fixture
        .engine
        .register_tool_provider(Arc::new(TestWriteProvider {
            executed: Arc::clone(&executed),
        }));
    let session = fixture
        .engine
        .create_session(selection.clone())
        .expect("persistent approval session");
    fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: session.session_id,
                client_run_id: cookie_agent_protocol::ClientRunId::new("persistent-approval")
                    .expect("run ID"),
                selection,
                input: "request two writes".to_owned(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("accepted persistent approval run");

    await_projection(
        &fixture.engine,
        session.session_id,
        "stateless approval completion",
        |projection| projection.status == SessionStatus::Completed,
    )
    .await;
    assert!(executed.is_set());

    let events = fixture
        .engine
        .inner
        .store
        .get(session.session_id)
        .expect("projection")
        .log
        .events();
    let evaluations = events
        .iter()
        .filter(|event| matches!(event.payload, EventPayload::ApprovalEvaluated { .. }))
        .count();
    assert_eq!(evaluations, 2);
    let requests = with_watchdog("captured fixture completion", captured)
        .await
        .expect("persistent approval server task");
    assert_eq!(requests.len(), 5);
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn ask_permission_mode_escalates_without_starting_internal_approval_agent() {
    let (endpoint, captured) = scripted_approval_server(r#"{"decision":"allow"}"#).await;
    let (fixture, selection) = approval_fixture_with_endpoint(&endpoint);
    let executed = Arc::new(TestFlag::default());
    fixture
        .engine
        .register_tool_provider(Arc::new(TestWriteProvider {
            executed: Arc::clone(&executed),
        }));
    let session = fixture
        .engine
        .create_session(selection.clone())
        .expect("session");
    fixture
        .engine
        .set_permission_mode(session.session_id, PermissionMode::Ask)
        .expect("ask mode");
    fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: session.session_id,
                client_run_id: cookie_agent_protocol::ClientRunId::new("ask-mode").expect("run ID"),
                selection,
                input: "request the write tool".into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("run");

    let approval = wait_for_escalated_approval(&fixture.engine, session.session_id).await;
    let events = fixture
        .engine
        .inner
        .store
        .get(session.session_id)
        .expect("projection")
        .log
        .events();
    assert!(!events.iter().any(|event| matches!(
        event.payload,
        EventPayload::InternalAgentStarted {
            kind: InternalAgentKind::Approval,
            ..
        }
    )));
    assert!(
        events
            .iter()
            .any(|event| matches!(event.payload, EventPayload::ApprovalEscalated { .. }))
    );
    approve_once(&fixture.engine, &approval, "ask-mode-approval").await;
    wait_for_tool_execution(&fixture.engine, session.session_id, &executed).await;
    captured.abort();
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn approval_request_window_uses_the_configured_timeout() {
    let (endpoint, captured) = scripted_approval_server(r#"{"decision":"ask"}"#).await;
    let (mut fixture, selection) = approval_fixture_with_endpoint(&endpoint);
    fixture.engine.shutdown().await;
    fixture.config.runtime.approval.timeout_ms = 120_000;
    fixture.engine = reopen_engine(&fixture);
    fixture
        .engine
        .register_tool_provider(Arc::new(TestWriteProvider {
            executed: Arc::new(TestFlag::default()),
        }));
    let session = fixture
        .engine
        .create_session(selection.clone())
        .expect("session");
    fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: session.session_id,
                client_run_id: ClientRunId::new("approval-window").expect("run ID"),
                selection,
                input: "request the write tool".into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("run");

    let approval = wait_for_escalated_approval(&fixture.engine, session.session_id).await;
    let approval_id = approval.request.approval_id();
    let events = fixture
        .engine
        .inner
        .store
        .get(session.session_id)
        .expect("projection")
        .log
        .events();
    let requested = events
        .iter()
        .find_map(|event| match &event.payload {
            EventPayload::ApprovalRequested { request } if request.approval_id() == approval_id => {
                Some(request)
            }
            _ => None,
        })
        .expect("approval requested event");
    let constraints: cookie_agent_protocol::ApprovalConstraints = serde_json::from_value(
        serde_json::to_value(requested).expect("request JSON")["constraints"].clone(),
    )
    .expect("request constraints");
    let expires_at = constraints
        .expires_at
        .expect("configured window must set an expiry");
    let remaining = expires_at.duration_since(Timestamp::now()).unsigned_abs();
    assert!(
        remaining > std::time::Duration::from_secs(100),
        "the configured 120s window must not collapse to the internal classifier budget, got {remaining:?}"
    );
    captured.abort();
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn yolo_permission_mode_durably_approves_and_executes_without_escalation() {
    let (endpoint, captured) = scripted_approval_server(r#"{"decision":"deny"}"#).await;
    let (fixture, selection) = approval_fixture_with_endpoint(&endpoint);
    let executed = Arc::new(TestFlag::default());
    fixture
        .engine
        .register_tool_provider(Arc::new(TestWriteProvider {
            executed: Arc::clone(&executed),
        }));
    let session = fixture
        .engine
        .create_session(selection.clone())
        .expect("session");
    fixture
        .engine
        .set_permission_mode(session.session_id, PermissionMode::Yolo)
        .expect("yolo mode");
    fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: session.session_id,
                client_run_id: cookie_agent_protocol::ClientRunId::new("yolo-mode")
                    .expect("run ID"),
                selection,
                input: "request the write tool".into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("run");
    wait_for_tool_execution(&fixture.engine, session.session_id, &executed).await;

    let events = fixture
        .engine
        .inner
        .store
        .get(session.session_id)
        .expect("projection")
        .log
        .events();
    let approval_lifecycle = events
        .iter()
        .filter(|event| {
            matches!(
                event.payload,
                EventPayload::ApprovalRequested { .. }
                    | EventPayload::ApprovalEvaluated { .. }
                    | EventPayload::ApprovalFinalized { .. }
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(approval_lifecycle.len(), 3);
    assert!(matches!(
        &approval_lifecycle[1].payload,
        EventPayload::ApprovalEvaluated {
            decision,
            ..
        }
            if decision.decision == ApprovalInternalDecisionKind::Allow
                && decision.source == ApprovalDecisionSource::Policy
                && decision.reason_code == ApprovalReasonCode::YoloApproved
    ));
    assert!(matches!(
        &approval_lifecycle[2].payload,
        EventPayload::ApprovalFinalized { decision, .. }
            if decision.outcome == ApprovalFinalOutcome::Approved
                && decision.source == ApprovalDecisionSource::Policy
                && decision.reason_code == ApprovalReasonCode::YoloApproved
    ));
    assert!(!events.iter().any(|event| matches!(
        event.payload,
        EventPayload::ApprovalEscalated { .. }
            | EventPayload::InternalAgentStarted {
                kind: InternalAgentKind::Approval,
                ..
            }
    )));
    captured.abort();
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn auto_approve_n_rejects_classifier_escalation_with_feedback_without_prompting() {
    let (endpoint, captured) = scripted_approval_server(r#"{"decision":"ask"}"#).await;
    let (fixture, selection) = approval_fixture_with_endpoint(&endpoint);
    let executed = Arc::new(TestFlag::default());
    fixture
        .engine
        .register_tool_provider(Arc::new(TestWriteProvider {
            executed: Arc::clone(&executed),
        }));
    let session = fixture
        .engine
        .create_session(selection.clone())
        .expect("session");
    fixture
        .engine
        .set_permission_mode(session.session_id, PermissionMode::AutoApproveN)
        .expect("auto-n mode");
    fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: session.session_id,
                client_run_id: ClientRunId::new("auto-n-mode").expect("run ID"),
                selection,
                input: "request the write tool".into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("run");
    wait_for_session_not_running(&fixture.engine, session.session_id).await;

    let events = fixture
        .engine
        .inner
        .store
        .get(session.session_id)
        .expect("projection")
        .log
        .events();
    assert!(!executed.is_set());
    assert!(events.iter().any(|event| matches!(
        &event.payload,
        EventPayload::ApprovalEvaluated { decision, .. }
            if decision.decision == ApprovalInternalDecisionKind::Escalate
                && decision.source == ApprovalDecisionSource::InternalAgent
                && decision.reason_code == ApprovalReasonCode::Escalated
    )));
    assert!(events.iter().any(|event| matches!(
        &event.payload,
        EventPayload::ApprovalFinalized { decision, .. }
            if decision.outcome == ApprovalFinalOutcome::Rejected
                && decision.source == ApprovalDecisionSource::PermissionMode
                && decision.reason_code == ApprovalReasonCode::AutoApproveNRejected
                && decision.feedback.as_ref().is_some_and(|feedback| {
                    feedback.message.as_str() == "rejected by auto-approve(N) mode"
                })
                && decision.tree_grant_id.is_none()
    )));
    assert!(!events.iter().any(|event| matches!(
        event.payload,
        EventPayload::ApprovalEscalated { .. } | EventPayload::TreeApprovalGrantCommitted { .. }
    )));
    assert!(events.iter().any(|event| matches!(
        &event.payload,
        EventPayload::ToolCallTerminated { termination }
            if termination.error.as_ref().is_some_and(|error| {
                error.message.as_str().contains("rejected by auto-approve(N) mode")
            })
    )));
    assert!(
        fixture
            .engine
            .inner
            .pending_approvals
            .lock()
            .expect("pending approvals lock")
            .is_empty()
    );
    let requests = with_watchdog("captured fixture completion", captured)
        .await
        .expect("approval server task");
    assert!(requests[2].contains("rejected by auto-approve(N) mode"));
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn auto_approve_y_approves_classifier_escalation_once_without_prompting() {
    let (endpoint, captured) = scripted_approval_server(r#"{"decision":"ask"}"#).await;
    let (fixture, selection) = approval_fixture_with_endpoint(&endpoint);
    let executed = Arc::new(TestFlag::default());
    fixture
        .engine
        .register_tool_provider(Arc::new(TestWriteProvider {
            executed: Arc::clone(&executed),
        }));
    let session = fixture
        .engine
        .create_session(selection.clone())
        .expect("session");
    fixture
        .engine
        .set_permission_mode(session.session_id, PermissionMode::AutoApproveY)
        .expect("auto-y mode");
    fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: session.session_id,
                client_run_id: ClientRunId::new("auto-y-mode").expect("run ID"),
                selection,
                input: "request the write tool".into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("run");
    wait_for_tool_execution(&fixture.engine, session.session_id, &executed).await;
    wait_for_session_not_running(&fixture.engine, session.session_id).await;

    let events = fixture
        .engine
        .inner
        .store
        .get(session.session_id)
        .expect("projection")
        .log
        .events();
    assert!(events.iter().any(|event| matches!(
        &event.payload,
        EventPayload::ApprovalFinalized { decision, .. }
            if decision.outcome == ApprovalFinalOutcome::Approved
                && decision.source == ApprovalDecisionSource::PermissionMode
                && decision.reason_code == ApprovalReasonCode::AutoApproveYApproved
                && decision.feedback.is_none()
                && decision.tree_grant_id.is_none()
    )));
    assert!(!events.iter().any(|event| matches!(
        event.payload,
        EventPayload::ApprovalEscalated { .. } | EventPayload::TreeApprovalGrantCommitted { .. }
    )));
    assert!(
        fixture
            .engine
            .inner
            .pending_approvals
            .lock()
            .expect("pending approvals lock")
            .is_empty()
    );
    assert_eq!(
        with_watchdog("captured fixture completion", captured)
            .await
            .expect("approval server task")
            .len(),
        3
    );
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn auto_approve_y_rechecks_identical_calls_without_creating_a_tree_grant() {
    let (endpoint, captured) = scripted_two_evaluated_writes_server(r#"{"decision":"ask"}"#).await;
    let (fixture, selection) = approval_fixture_with_endpoint(&endpoint);
    let executed = Arc::new(TestFlag::default());
    fixture
        .engine
        .register_tool_provider(Arc::new(TestWriteProvider {
            executed: Arc::clone(&executed),
        }));
    let session = fixture
        .engine
        .create_session(selection.clone())
        .expect("session");
    fixture
        .engine
        .set_permission_mode(session.session_id, PermissionMode::AutoApproveY)
        .expect("auto-y mode");
    fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: session.session_id,
                client_run_id: ClientRunId::new("auto-y-identical-calls").expect("run ID"),
                selection,
                input: "request two identical writes".into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("run");
    wait_for_session_not_running(&fixture.engine, session.session_id).await;

    let events = fixture
        .engine
        .inner
        .store
        .get(session.session_id)
        .expect("projection")
        .log
        .events();
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(
                &event.payload,
                EventPayload::ApprovalEvaluated { decision, .. }
                    if decision.source == ApprovalDecisionSource::InternalAgent
            ))
            .count(),
        2
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(
                &event.payload,
                EventPayload::ToolCallTerminated { termination }
                    if termination.outcome == ToolTerminationOutcome::Completed
            ))
            .count(),
        2
    );
    assert!(!events.iter().any(|event| matches!(
        event.payload,
        EventPayload::TreeApprovalGrantCommitted { .. }
    )));
    assert_eq!(
        with_watchdog("captured fixture completion", captured)
            .await
            .expect("approval server task")
            .len(),
        5
    );
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn auto_approve_n_and_y_preserve_classifier_allow_and_deny() {
    for (mode, internal_output, should_execute, expected_reason) in [
        (
            PermissionMode::AutoApproveN,
            r#"{"decision":"allow"}"#,
            true,
            ApprovalReasonCode::InternalAgentAllowed,
        ),
        (
            PermissionMode::AutoApproveN,
            r#"{"decision":"deny"}"#,
            false,
            ApprovalReasonCode::InternalAgentDenied,
        ),
        (
            PermissionMode::AutoApproveY,
            r#"{"decision":"allow"}"#,
            true,
            ApprovalReasonCode::InternalAgentAllowed,
        ),
        (
            PermissionMode::AutoApproveY,
            r#"{"decision":"deny"}"#,
            false,
            ApprovalReasonCode::InternalAgentDenied,
        ),
    ] {
        let (endpoint, captured) = scripted_approval_server(internal_output).await;
        let (fixture, selection) = approval_fixture_with_endpoint(&endpoint);
        let executed = Arc::new(TestFlag::default());
        fixture
            .engine
            .register_tool_provider(Arc::new(TestWriteProvider {
                executed: Arc::clone(&executed),
            }));
        let session = fixture
            .engine
            .create_session(selection.clone())
            .expect("session");
        fixture
            .engine
            .set_permission_mode(session.session_id, mode)
            .expect("permission mode");
        fixture
            .engine
            .start_run(
                RunStartParams {
                    reset_fallback: false,
                    session_id: session.session_id,
                    client_run_id: ClientRunId::new(format!(
                        "mode-agent-{mode:?}-{should_execute}"
                    ))
                    .expect("run ID"),
                    selection,
                    input: "request the write tool".into(),
                },
                cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
            )
            .await
            .expect("run");
        wait_for_session_not_running(&fixture.engine, session.session_id).await;

        assert_eq!(executed.is_set(), should_execute);
        let events = fixture
            .engine
            .inner
            .store
            .get(session.session_id)
            .expect("projection")
            .log
            .events();
        assert!(events.iter().any(|event| matches!(
            &event.payload,
            EventPayload::ApprovalFinalized { decision, .. }
                if decision.source == ApprovalDecisionSource::InternalAgent
                    && decision.reason_code == expected_reason
                    && (decision.outcome == ApprovalFinalOutcome::Approved) == should_execute
        )));
        assert!(
            !events
                .iter()
                .any(|event| matches!(event.payload, EventPayload::ApprovalEscalated { .. }))
        );
        assert_eq!(
            with_watchdog("captured fixture completion", captured)
                .await
                .expect("approval server task")
                .len(),
            3
        );
        fixture.engine.shutdown().await;
    }
}

#[tokio::test]
async fn yolo_permission_mode_does_not_override_hard_deny_rules() {
    let (endpoint, captured) = scripted_approval_server(r#"{"decision":"allow"}"#).await;
    let (fixture, selection) = denied_approval_fixture_with_endpoint(&endpoint);
    let executed = Arc::new(TestFlag::default());
    fixture
        .engine
        .register_tool_provider(Arc::new(TestWriteProvider {
            executed: Arc::clone(&executed),
        }));
    let session = fixture
        .engine
        .create_session(selection.clone())
        .expect("session");
    fixture
        .engine
        .set_permission_mode(session.session_id, PermissionMode::Yolo)
        .expect("yolo mode");
    fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: session.session_id,
                client_run_id: cookie_agent_protocol::ClientRunId::new("yolo-deny")
                    .expect("run ID"),
                selection,
                input: "request the denied write tool".into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("run");
    await_event(
        &fixture.engine,
        session.session_id,
        "denied tool termination",
        |event| matches!(event.payload, EventPayload::ToolCallTerminated { .. }),
    )
    .await;

    let events = fixture
        .engine
        .inner
        .store
        .get(session.session_id)
        .expect("projection")
        .log
        .events();
    assert!(!executed.is_set());
    assert!(!events.iter().any(|event| matches!(
        event.payload,
        EventPayload::ApprovalRequested { .. }
            | EventPayload::ApprovalEvaluated { .. }
            | EventPayload::ApprovalFinalized { .. }
            | EventPayload::ApprovalEscalated { .. }
    )));
    captured.abort();
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn yolo_permission_mode_still_triggers_the_doom_loop_guard() {
    let (endpoint, captured) = scripted_repeated_write_server(4).await;
    let (fixture, selection) = approval_fixture_with_endpoint(&endpoint);
    fixture
        .engine
        .register_tool_provider(Arc::new(TestWriteProvider {
            executed: Arc::new(TestFlag::default()),
        }));
    let session = fixture
        .engine
        .create_session(selection.clone())
        .expect("session");
    fixture
        .engine
        .set_permission_mode(session.session_id, PermissionMode::Yolo)
        .expect("yolo mode");
    fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: session.session_id,
                client_run_id: cookie_agent_protocol::ClientRunId::new("yolo-doom-loop")
                    .expect("run ID"),
                selection,
                input: "repeat the same write".into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("run");
    await_event(
        &fixture.engine,
        session.session_id,
        "doom-loop rejection",
        |event| {
            matches!(
                &event.payload,
                EventPayload::ApprovalFinalized { decision, .. }
                    if decision.outcome == ApprovalFinalOutcome::Rejected
                        && decision.reason_code == ApprovalReasonCode::DoomLoopDetected
            )
        },
    )
    .await;
    let events = fixture
        .engine
        .inner
        .store
        .get(session.session_id)
        .expect("projection")
        .log
        .events();
    assert!(events.iter().any(|event| matches!(
        &event.payload,
        EventPayload::ApprovalFinalized { decision, .. }
            if decision.outcome == ApprovalFinalOutcome::Rejected
                && decision.reason_code == ApprovalReasonCode::DoomLoopDetected
    )));
    captured.abort();
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn permission_mode_change_applies_to_the_next_operation_only() {
    let (endpoint, captured) = scripted_repeated_write_server(2).await;
    let (fixture, selection) = approval_fixture_with_endpoint(&endpoint);
    let executed = Arc::new(TestFlag::default());
    fixture
        .engine
        .register_tool_provider(Arc::new(TestWriteProvider {
            executed: Arc::clone(&executed),
        }));
    let session = fixture
        .engine
        .create_session(selection.clone())
        .expect("session");
    fixture
        .engine
        .set_permission_mode(session.session_id, PermissionMode::Ask)
        .expect("ask mode");
    fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: session.session_id,
                client_run_id: cookie_agent_protocol::ClientRunId::new("live-mode-change")
                    .expect("run ID"),
                selection,
                input: "perform two writes".into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("run");

    let first = wait_for_escalated_approval(&fixture.engine, session.session_id).await;
    fixture
        .engine
        .set_permission_mode(session.session_id, PermissionMode::Yolo)
        .expect("yolo mode");
    assert_eq!(
        fixture
            .engine
            .list_approvals(session.session_id, Some(ApprovalStatus::Escalated))
            .approvals
            .len(),
        1
    );
    approve_once(&fixture.engine, &first, "live-mode-first").await;
    await_event(
        &fixture.engine,
        session.session_id,
        "next operation uses yolo",
        |event| {
            matches!(
                &event.payload,
                EventPayload::ApprovalFinalized { decision, .. }
                    if decision.reason_code == ApprovalReasonCode::YoloApproved
            )
        },
    )
    .await;
    assert!(executed.is_set());
    let events = fixture
        .engine
        .inner
        .store
        .get(session.session_id)
        .expect("projection")
        .log
        .events();
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event.payload, EventPayload::ApprovalEscalated { .. }))
            .count(),
        1
    );
    captured.abort();
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn malformed_internal_approval_output_falls_back_to_escalation_transaction() {
    let (endpoint, captured) = scripted_approval_server("not-json").await;
    let (fixture, selection) = approval_fixture_with_endpoint(&endpoint);
    let executed = Arc::new(TestFlag::default());
    fixture
        .engine
        .register_tool_provider(Arc::new(TestWriteProvider {
            executed: Arc::clone(&executed),
        }));
    let session = fixture
        .engine
        .create_session(selection.clone())
        .expect("approval session");
    fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: session.session_id,
                client_run_id: cookie_agent_protocol::ClientRunId::new("malformed-approval")
                    .expect("run ID"),
                selection,
                input: "request the write tool".to_owned(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("accepted approval run");

    let approval = wait_for_escalated_approval(&fixture.engine, session.session_id).await;
    let events = fixture
        .engine
        .inner
        .store
        .get(session.session_id)
        .expect("approval projection")
        .log
        .events();
    assert!(events.iter().any(|event| matches!(
        &event.payload,
        EventPayload::ApprovalEvaluated {
            approval_id,
            decision,
            ..
        }
            if *approval_id == approval.request.approval_id()
                && decision.decision == ApprovalInternalDecisionKind::Escalate
                && decision.source == ApprovalDecisionSource::InternalAgent
                && decision.reason_code == ApprovalReasonCode::Escalated
    )));

    approve_once(&fixture.engine, &approval, "malformed-approval-response").await;
    wait_for_tool_execution(&fixture.engine, session.session_id, &executed).await;
    captured.abort();
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn internal_agent_ask_escalates_to_user_approval_then_executes_tool() {
    let (endpoint, captured) = scripted_approval_server(r#"{"decision":"ask"}"#).await;
    let (fixture, selection) = approval_fixture_with_endpoint(&endpoint);
    let executed = Arc::new(TestFlag::default());
    fixture
        .engine
        .register_tool_provider(Arc::new(TestWriteProvider {
            executed: Arc::clone(&executed),
        }));
    let session = fixture
        .engine
        .create_session(selection.clone())
        .expect("approval session");
    fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: session.session_id,
                client_run_id: cookie_agent_protocol::ClientRunId::new("approval-e2e")
                    .expect("run ID"),
                selection,
                input: "request the write tool".to_owned(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("accepted approval run");

    let approval = wait_for_escalated_approval(&fixture.engine, session.session_id).await;
    let response = approve_once(&fixture.engine, &approval, "approval-e2e-response").await;
    assert_eq!(response.approval.status, ApprovalStatus::Approved);
    wait_for_tool_execution(&fixture.engine, session.session_id, &executed).await;

    let events = fixture
        .engine
        .inner
        .store
        .get(session.session_id)
        .expect("completed approval projection")
        .log
        .events();
    let approval_id = approval.request.approval_id();
    assert!(events.iter().any(|event| matches!(
        &event.payload,
        EventPayload::ApprovalUserDecisionRecorded { approval_id: event_id, decision, .. }
            if *event_id == approval_id && *decision == ApprovalUserDecision::ApproveOnce
    )));
    assert!(events.iter().any(|event| matches!(
        &event.payload,
        EventPayload::ApprovalFinalized { approval_id: event_id, decision }
            if *event_id == approval_id
                && decision.outcome == cookie_agent_protocol::ApprovalFinalOutcome::Approved
                && decision.source == ApprovalDecisionSource::User
                && decision.reason_code == ApprovalReasonCode::UserApprovedOnce
    )));
    assert!(events.iter().any(|event| matches!(
        &event.payload,
        EventPayload::ToolCallTerminated { termination }
            if termination.outcome == ToolTerminationOutcome::Completed
    )));
    captured.abort();
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn session_resume_does_not_sweep_approvals_of_a_live_running_run() {
    let primary_tool_call = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"write-call\",\"type\":\"function\",\"function\":{\"name\":\"write\",\"arguments\":\"{}\"}}]},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n".to_owned();
    let internal_ask = "data: {\"choices\":[{\"delta\":{\"content\":\"{\\\"decision\\\":\\\"ask\\\"}\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n".to_owned();
    let final_text = "data: {\"choices\":[{\"delta\":{\"content\":\"approval flow complete\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n".to_owned();
    let (endpoint, captured, reached, release) =
        scripted_server_with_delayed_response(vec![primary_tool_call, internal_ask, final_text], 1)
            .await;
    let (fixture, selection) = approval_fixture_with_endpoint(&endpoint);
    let executed = Arc::new(TestFlag::default());
    fixture
        .engine
        .register_tool_provider(Arc::new(TestWriteProvider {
            executed: Arc::clone(&executed),
        }));
    let session = fixture
        .engine
        .create_session(selection.clone())
        .expect("session");
    let run = fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: session.session_id,
                client_run_id: ClientRunId::new("live-resume").expect("run ID"),
                selection,
                input: "request the write tool".to_owned(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("accepted run");
    tokio::time::timeout(test_timeout(30), reached)
        .await
        .expect("internal approval evaluation started")
        .expect("evaluation reached signal");

    // The TUI resumes sessions merely to open or watch them; the still
    // Pending approval on the live run must survive the resume sweep.
    fixture
        .engine
        .resume(session.session_id)
        .await
        .expect("resume live session");
    assert_eq!(
        fixture
            .engine
            .list_approvals(session.session_id, Some(ApprovalStatus::Pending))
            .approvals
            .len(),
        1,
        "resume must not finalize a pending approval on a running run"
    );
    assert_eq!(
        fixture
            .engine
            .inner
            .store
            .get(session.session_id)
            .expect("projection")
            .runs
            .get(&run.run_id)
            .expect("run")
            .status,
        SessionStatus::Running,
        "resume must not disturb a healthy run"
    );
    release.notify_one();

    let approval = wait_for_escalated_approval(&fixture.engine, session.session_id).await;
    // An escalated (still live) approval must survive resume as well.
    fixture
        .engine
        .resume(session.session_id)
        .await
        .expect("resume live session again");
    assert_eq!(
        fixture
            .engine
            .list_approvals(session.session_id, Some(ApprovalStatus::Escalated))
            .approvals
            .len(),
        1,
        "resume must not finalize an escalated approval on a running run"
    );

    approve_once(&fixture.engine, &approval, "live-resume-approval").await;
    wait_for_tool_execution(&fixture.engine, session.session_id, &executed).await;
    captured.abort();
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn resume_sweep_finalizes_interrupted_run_approvals_and_preserves_live_ones() {
    let (fixture, selection) = custom_fixture();
    let session = fixture
        .engine
        .create_session(selection.clone())
        .expect("session");
    let projection = fixture
        .engine
        .inner
        .store
        .get(session.session_id)
        .expect("projection");
    let selected_suffix = projection.creation_agent.fallback_chain.clone();
    let run_started =
        |label: &str, agent: Box<cookie_agent_protocol::AgentSnapshot>| EventPayload::RunStarted {
            client_run_id: ClientRunId::new(label).expect("client run ID"),
            selection: selection.clone(),
            agent,
            runtime_revision: projection.meta.runtime_revision.clone(),
            catalog_revision: projection.meta.catalog_revision.clone(),
            provider_state_revision: projection.meta.provider_state_revision.clone(),
            model_revision: projection.meta.model_revision.clone(),
            agent_revision: projection.meta.agent_revision.clone(),
            recipe_registry_revision: projection.meta.recipe_registry_revision.clone(),
            manifest_revision: projection.meta.manifest_revision.clone(),
            selected_suffix: selected_suffix.clone(),
            internal_agents: Vec::new(),
            input_through_seq: 1,
        };
    let dead_run = cookie_agent_protocol::RunId::new_v7();
    let live_run = cookie_agent_protocol::RunId::new_v7();
    let dead_approval = resume_sweep_test_request();
    let live_approval = resume_sweep_test_request();
    let store = &fixture.engine.inner.store;
    store
        .append(
            session.session_id,
            Some(dead_run),
            cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
            run_started("dead-run", Box::new(projection.creation_agent.clone())),
        )
        .expect("start dead run");
    store
        .append(
            session.session_id,
            Some(live_run),
            cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
            run_started("live-run", Box::new(projection.creation_agent.clone())),
        )
        .expect("start live run");
    for (run_id, request) in [(dead_run, &dead_approval), (live_run, &live_approval)] {
        store
            .append(
                session.session_id,
                Some(run_id),
                cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
                EventPayload::ApprovalRequested {
                    request: request.clone(),
                },
            )
            .expect("request approval");
    }
    store
        .append(
            session.session_id,
            Some(dead_run),
            cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
            EventPayload::RunInterrupted { reason: None },
        )
        .expect("interrupt dead run");

    fixture
        .engine
        .resume(session.session_id)
        .await
        .expect("resume after interruption");

    let records = fixture.engine.list_approvals(session.session_id, None);
    let status = |approval_id: ApprovalId| {
        records
            .approvals
            .iter()
            .find(|record| record.request.approval_id() == approval_id)
            .map(|record| record.status)
    };
    assert_eq!(
        status(dead_approval.approval_id()),
        Some(ApprovalStatus::Cancelled),
        "approvals of interrupted runs must still be swept"
    );
    assert_eq!(
        status(live_approval.approval_id()),
        Some(ApprovalStatus::Pending),
        "approvals of running runs must survive resume"
    );
    let events = fixture
        .engine
        .inner
        .store
        .get(session.session_id)
        .expect("projection")
        .log
        .events();
    let cancelled: Vec<_> = events
        .iter()
        .filter_map(|event| match &event.payload {
            EventPayload::ApprovalCancelled {
                approval_id,
                reason_code,
            } => Some((*approval_id, *reason_code)),
            _ => None,
        })
        .collect();
    assert_eq!(
        cancelled,
        vec![(
            dead_approval.approval_id(),
            ApprovalReasonCode::PreparedCapabilityLost
        )]
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(
                &event.payload,
                EventPayload::ApprovalFinalized { approval_id, .. }
                    if *approval_id == dead_approval.approval_id()
            ))
            .count(),
        1
    );
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn finalized_approval_during_internal_evaluation_yields_clean_denial() {
    let primary_tool_call = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"write-call\",\"type\":\"function\",\"function\":{\"name\":\"write\",\"arguments\":\"{}\"}}]},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n".to_owned();
    let internal_ask = "data: {\"choices\":[{\"delta\":{\"content\":\"{\\\"decision\\\":\\\"ask\\\"}\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n".to_owned();
    let final_text = "data: {\"choices\":[{\"delta\":{\"content\":\"approval flow complete\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n".to_owned();
    let (endpoint, captured, reached, release) =
        scripted_server_with_delayed_response(vec![primary_tool_call, internal_ask, final_text], 1)
            .await;
    let (fixture, selection) = approval_fixture_with_endpoint(&endpoint);
    let executed = Arc::new(TestFlag::default());
    fixture
        .engine
        .register_tool_provider(Arc::new(TestWriteProvider {
            executed: Arc::clone(&executed),
        }));
    let session = fixture
        .engine
        .create_session(selection.clone())
        .expect("session");
    let run = fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: session.session_id,
                client_run_id: ClientRunId::new("late-evaluation").expect("run ID"),
                selection,
                input: "request the write tool".to_owned(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("accepted run");
    tokio::time::timeout(test_timeout(30), reached)
        .await
        .expect("internal approval evaluation started")
        .expect("evaluation reached signal");
    let approval = fixture
        .engine
        .list_approvals(session.session_id, Some(ApprovalStatus::Pending))
        .approvals
        .pop()
        .expect("pending approval during evaluation");
    let approval_id = approval.request.approval_id();

    // Reproduce the incident shape: a recovery-style sweep finalizes the
    // still-Pending approval while the internal approval agent is mid
    // evaluation, then the late verdict arrives at a terminal record.
    let origin = cookie_agent_protocol::EventOrigin::new("engine:recovery").unwrap();
    fixture
        .engine
        .inner
        .store
        .append(
            session.session_id,
            Some(run.run_id),
            origin.clone(),
            EventPayload::ApprovalCancelled {
                approval_id,
                reason_code: ApprovalReasonCode::PreparedCapabilityLost,
            },
        )
        .expect("sweep cancellation");
    fixture
        .engine
        .inner
        .store
        .append(
            session.session_id,
            Some(run.run_id),
            origin,
            EventPayload::ApprovalFinalized {
                approval_id,
                decision: cookie_agent_protocol::ApprovalFinalDecision {
                    outcome: ApprovalFinalOutcome::Cancelled,
                    source: ApprovalDecisionSource::System,
                    reason_code: ApprovalReasonCode::PreparedCapabilityLost,
                    feedback: None,
                    tree_grant_id: None,
                },
            },
        )
        .expect("sweep finalization");
    release.notify_one();

    let termination = await_event(
        &fixture.engine,
        session.session_id,
        "denied tool termination",
        |event| matches!(&event.payload, EventPayload::ToolCallTerminated { .. }),
    )
    .await;
    let EventPayload::ToolCallTerminated { termination } = &termination.payload else {
        unreachable!("matched termination");
    };
    assert_eq!(termination.outcome, ToolTerminationOutcome::Failed);
    let error = termination.error.as_ref().expect("denial error payload");
    let message = error.message.as_str();
    assert!(
        message.contains("tool_denied"),
        "tool result must reuse the denial envelope: {message}"
    );
    assert!(
        message.contains("the session was interrupted while this operation awaited approval"),
        "denial feedback must name the finalized reason: {message}"
    );
    assert!(
        !message.contains("is not pending for session"),
        "raw invariant error must never reach the tool result: {message}"
    );
    assert!(!executed.is_set(), "denied tool must not execute");
    // The late evaluation completes the approval's lifecycle exactly once.
    let events = fixture
        .engine
        .inner
        .store
        .get(session.session_id)
        .expect("projection")
        .log
        .events();
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(
                &event.payload,
                EventPayload::ApprovalFinalized {
                    approval_id: event_approval_id,
                    ..
                } if *event_approval_id == approval_id
            ))
            .count(),
        1
    );
    assert!(
        events.iter().all(
            |event| !matches!(&event.payload, EventPayload::ToolCallTerminated { termination }
                if termination.error.as_ref().is_some_and(|error| error
                    .message
                    .as_str()
                    .contains("is not pending for session")))
        ),
        "no termination may surface the raw approval invariant"
    );
    wait_for_session_not_running(&fixture.engine, session.session_id).await;
    captured.abort();
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn escalated_approval_finalized_externally_still_wakes_the_waiter() {
    let (endpoint, captured) = scripted_approval_server(r#"{"decision":"ask"}"#).await;
    let (fixture, selection) = approval_fixture_with_endpoint(&endpoint);
    fixture
        .engine
        .register_tool_provider(Arc::new(TestWriteProvider {
            executed: Arc::new(TestFlag::default()),
        }));
    let session = fixture
        .engine
        .create_session(selection.clone())
        .expect("session");
    let run = fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: session.session_id,
                client_run_id: ClientRunId::new("terminal-race").expect("run ID"),
                selection,
                input: "request the write tool".to_owned(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("accepted run");
    let approval = wait_for_escalated_approval(&fixture.engine, session.session_id).await;
    let approval_id = approval.request.approval_id();

    // A terminal sweep (as performed by recovery) can finalize the record
    // without notifying the escalation responder. The later terminal pass
    // must still drain the responder so the tool wakes up.
    let origin = cookie_agent_protocol::EventOrigin::new("engine:recovery").unwrap();
    fixture
        .engine
        .inner
        .store
        .append(
            session.session_id,
            Some(run.run_id),
            origin.clone(),
            EventPayload::ApprovalCancelled {
                approval_id,
                reason_code: ApprovalReasonCode::PreparedCapabilityLost,
            },
        )
        .expect("finalize cancelled");
    fixture
        .engine
        .inner
        .store
        .append(
            session.session_id,
            Some(run.run_id),
            origin,
            EventPayload::ApprovalFinalized {
                approval_id,
                decision: cookie_agent_protocol::ApprovalFinalDecision {
                    outcome: ApprovalFinalOutcome::Cancelled,
                    source: ApprovalDecisionSource::System,
                    reason_code: ApprovalReasonCode::PreparedCapabilityLost,
                    feedback: None,
                    tree_grant_id: None,
                },
            },
        )
        .expect("finalize decision");
    await_event(
        &fixture.engine,
        session.session_id,
        "externally finalized approval",
        |event| {
            matches!(
                &event.payload,
                EventPayload::ApprovalFinalized {
                    approval_id: event_approval_id,
                    ..
                } if *event_approval_id == approval_id
            )
        },
    )
    .await;

    // Cancelling the run drives the already-terminal approval through the
    // racing terminal path; without the responder drain the tool would hang.
    fixture.engine.cancel_run(run.run_id).await.expect("cancel");
    tokio::time::timeout(
        test_timeout(60),
        await_event(
            &fixture.engine,
            session.session_id,
            "woken tool termination",
            |event| matches!(&event.payload, EventPayload::ToolCallTerminated { .. }),
        ),
    )
    .await
    .expect("escalated waiter must wake after an external finalize");
    wait_for_run_inactive(&fixture.engine, run.run_id).await;
    captured.abort();
    fixture.engine.shutdown().await;
}
