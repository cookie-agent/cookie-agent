use std::sync::Arc;

use base64::{Engine as _, engine::general_purpose::STANDARD};

use cookie_agent_protocol::{
    ClientRunId, EventPayload, PermissionMode, RunStartParams, RunToolStdinParams,
    ToolTerminationOutcome,
};

use super::support::*;

#[tokio::test]
async fn pending_steering_promotes_after_tools_and_compaction_in_admission_order() {
    let bodies = vec![
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"write-call\",\"type\":\"function\",\"function\":{\"name\":\"write\",\"arguments\":\"{}\"}}]},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}],\"usage\":{\"prompt_tokens\":4000,\"completion_tokens\":1,\"total_tokens\":4001}}\n\n".to_owned(),
        "data: {\"choices\":[{\"delta\":{\"content\":\"{\\\"decision\\\":\\\"ask\\\"}\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n".to_owned(),
        "data: {\"choices\":[{\"delta\":{\"content\":\"compacted before steering\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n".to_owned(),
        "data: {\"choices\":[{\"delta\":{\"content\":\"continued after steering\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n".to_owned(),
        "data: {\"choices\":[{\"delta\":{\"content\":\"answered late steering\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n".to_owned(),
    ];
    let (endpoint, captured, compaction_reached, release_compaction) =
        scripted_server_with_delayed_response(bodies, 2).await;
    // A small summary cap leaves recent-history room for the steering inputs.
    let (fixture, selection) = custom_fixture_with_endpoint_primary_and_internal(
        &endpoint,
        "---\ndescription: Steering compaction test\nmode: primary\nenabled: true\nmodels: [{ model: \"custom.test/group/model\", variant: base }]\npermissions:\n  write: ask\n---\nTest steering compaction.\n",
        Some((
            "compaction.md",
            "---\ndescription: compaction\nmode: internal\nenabled: true\nmodels: [{ model: \"${parent_model}\" }]\nlimits: { timeout_ms: 30000, max_output_tokens: 64 }\npermissions: {}\n---\nSummarize.\n",
        )),
        Some(500),
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
        .expect("steering session");
    let initial_input = format!("begin {}", "historical context ".repeat(300));
    let run = fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: session.session_id,
                client_run_id: cookie_agent_protocol::ClientRunId::new("steering-compaction")
                    .expect("run ID"),
                selection,
                input: initial_input.clone(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("started steering run");
    let approval = wait_for_escalated_approval(&fixture.engine, session.session_id).await;
    assert!(
        fixture
            .engine
            .steer(
                run.run_id,
                "recall me".into(),
                cookie_agent_protocol::EventOrigin::new("client:test").unwrap()
            )
            .await
            .expect("first admission")
            .accepted
    );
    assert_eq!(
        fixture
            .engine
            .recall_steer(run.run_id)
            .await
            .expect("recall pending input")
            .recalled
            .as_deref(),
        Some("recall me")
    );
    assert_eq!(
        fixture
            .engine
            .recall_steer(run.run_id)
            .await
            .expect("empty recall")
            .recalled,
        None
    );
    let first_pending = "first pending input";
    for input in [first_pending, "second pending", "third pending"] {
        assert!(
            fixture
                .engine
                .steer(
                    run.run_id,
                    input.into(),
                    cookie_agent_protocol::EventOrigin::new("client:test").unwrap()
                )
                .await
                .expect("admission")
                .accepted
        );
    }
    assert_eq!(
        fixture
            .engine
            .recall_steer(run.run_id)
            .await
            .expect("LIFO recall")
            .recalled
            .as_deref(),
        Some("third pending")
    );
    assert!(
        fixture
            .engine
            .steer(
                run.run_id,
                "third pending".into(),
                cookie_agent_protocol::EventOrigin::new("client:test").unwrap()
            )
            .await
            .expect("replacement admission")
            .accepted
    );
    let before_boundary = fixture
        .engine
        .inner
        .store
        .get(session.session_id)
        .expect("pending projection")
        .log
        .events();
    assert!(before_boundary.iter().any(|event| {
        matches!(
            &event.payload,
            EventPayload::UserInputAdmitted { input } if input == first_pending
        ) && event
            .origin
            .as_ref()
            .map(cookie_agent_protocol::EventOrigin::as_str)
            == Some("client:test")
    }));
    assert!(!before_boundary.iter().any(|event| matches!(
        &event.payload,
        EventPayload::UserInputSubmitted { input } if input != &initial_input
    )));
    approve_once(&fixture.engine, &approval, "steering-race-approval").await;
    wait_for_tool_execution(&fixture.engine, session.session_id, &executed).await;
    with_watchdog("compaction_reached fixture completion", compaction_reached)
        .await
        .expect("usage compaction started");
    let during_reservation = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        fixture.engine.steer(
            run.run_id,
            "fourth pending".into(),
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        ),
    )
    .await
    .expect("steer is not blocked by compaction")
    .expect("steer during compaction");
    assert!(during_reservation.accepted);
    release_compaction.notify_one();
    let requests = with_watchdog("captured fixture completion", captured)
        .await
        .expect("steering server task");
    assert_eq!(requests.len(), 5);
    assert!(!requests[0].contains("first pending"));
    // Promoted before compaction, the steering inputs fit the verbatim tail.
    assert!(requests[3].contains("compacted before steering"));
    for input in [first_pending, "second pending", "third pending"] {
        assert!(
            requests[3].contains(input),
            "missing {input:?}: {}",
            requests[3]
        );
    }
    assert!(!requests[3].contains("fourth pending"));
    assert!(requests[4].contains("fourth pending"));
    assert!(!requests[3].contains("recall me"));
    let events = fixture
        .engine
        .inner
        .store
        .get(session.session_id)
        .expect("steering projection")
        .log
        .events();
    let checkpoint = events
        .iter()
        .find(|event| {
            matches!(
                event.payload,
                EventPayload::ContextCheckpointCommitted { .. }
            )
        })
        .expect("usage checkpoint");
    assert_eq!(
        checkpoint.origin.as_ref().map(|origin| origin.as_str()),
        Some("engine:auto-compact")
    );
    let checkpoint_seq = checkpoint.seq;
    let tool_result_seq = events
        .iter()
        .find_map(|event| {
            matches!(event.payload, EventPayload::ToolCallTerminated { .. }).then_some(event.seq)
        })
        .expect("tool result");
    let submitted = events
        .iter()
        .filter_map(|event| match &event.payload {
            EventPayload::UserInputSubmitted { input } if input != &initial_input => {
                Some((event.seq, input.as_str(), event.origin.as_ref()))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        submitted
            .iter()
            .map(|(_, input, _)| *input)
            .collect::<Vec<_>>(),
        vec![
            first_pending,
            "second pending",
            "third pending",
            "fourth pending"
        ]
    );
    assert!(submitted.iter().all(|(_, _, origin)| {
        origin.map(cookie_agent_protocol::EventOrigin::as_str) == Some("client:test")
    }));
    let first_steering_seq = submitted[0].0;
    let next_attempt_seq = events
        .iter()
        .find_map(|event| {
            (event.seq > first_steering_seq
                && matches!(event.payload, EventPayload::ModelAttemptStarted { .. }))
            .then_some(event.seq)
        })
        .expect("next model request");
    assert!(
        tool_result_seq < first_steering_seq
            && submitted[2].0 < checkpoint_seq
            && checkpoint_seq < next_attempt_seq
            && next_attempt_seq < submitted[3].0
    );
    assert!(events.iter().any(|event| {
        matches!(
            event.payload,
            EventPayload::ApprovalUserDecisionRecorded { .. }
        ) && event
            .origin
            .as_ref()
            .map(cookie_agent_protocol::EventOrigin::as_str)
            == Some("user")
    }));
    assert!(events.iter().any(|event| {
        matches!(event.payload, EventPayload::ApprovalFinalized { .. })
            && event
                .origin
                .as_ref()
                .map(cookie_agent_protocol::EventOrigin::as_str)
                == Some("engine:approvals")
    }));
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn cancelling_interactive_stream_drains_chunks_before_tool_termination() {
    let (endpoint, responses, captured) = scripted_channel_server(1).await;
    responses
        .send(MatchedScriptedResponse::last_message_role(
            "user",
            scripted_tool_body(
                "interactive-cancel",
                "bash",
                serde_json::json!({"command":"stream", "interactive":true}),
            ),
        ))
        .expect("scripted tool response");
    let (fixture, selection) = custom_fixture_with_endpoint_and_primary_agent(
        &endpoint,
        "---\ndescription: Streaming cancellation test\nmode: primary\nenabled: true\nmodels: [{ model: \"custom.test/group/model\", variant: base }]\npermissions:\n  bash: allow\n---\nTest interactive streaming cancellation.\n",
    );
    let output_started = Arc::new(tokio::sync::Notify::new());
    let stdin_received = Arc::new(tokio::sync::Notify::new());
    let cleanup_progress_sent = Arc::new(tokio::sync::Notify::new());
    fixture
        .engine
        .register_tool_provider(Arc::new(TestStreamingBashProvider {
            output_started: Arc::clone(&output_started),
            stdin_received: Arc::clone(&stdin_received),
            cleanup_progress_sent,
        }));
    let session = fixture
        .engine
        .create_session(selection.clone())
        .expect("streaming session");
    fixture
        .engine
        .set_permission_mode(session.session_id, PermissionMode::Yolo)
        .expect("yolo mode");
    let live = record_live_messages(&fixture.engine, session.session_id).await;
    let run = fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: session.session_id,
                client_run_id: ClientRunId::new("interactive-stream-cancel")
                    .expect("client run id"),
                selection,
                input: "start interactive stream".into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("run started")
        .run_id;
    if tokio::time::timeout(test_timeout(2), output_started.notified())
        .await
        .is_err()
    {
        panic!(
            "first output chunk timed out: {:#?}",
            fixture
                .engine
                .inner
                .store
                .get(session.session_id)
                .expect("timed out projection")
                .log
                .events()
        );
    }
    let call_id = fixture
        .engine
        .inner
        .store
        .get(session.session_id)
        .expect("streaming projection")
        .log
        .events()
        .iter()
        .find_map(|event| match &event.payload {
            EventPayload::ToolCallStarted { start } if event.run_id == Some(run) => {
                Some(start.tool_call_id)
            }
            _ => None,
        })
        .expect("started tool call");
    fixture
        .engine
        .tool_stdin(RunToolStdinParams {
            run_id: run,
            call_id,
            data: Some(STANDARD.encode(b"input\n")),
            eof: false,
        })
        .await
        .expect("interactive stdin accepted");
    tokio::time::timeout(test_timeout(2), stdin_received.notified())
        .await
        .expect("executor received stdin");
    fixture.engine.cancel_run(run).await.expect("cancel run");

    await_event(
        &fixture.engine,
        session.session_id,
        "tool termination after cancellation cleanup",
        |event| {
            matches!(
                &event.payload,
                EventPayload::ToolCallTerminated { termination }
                    if termination.tool_call_id == call_id
            )
        },
    )
    .await;
    let events = fixture
        .engine
        .inner
        .store
        .get(session.session_id)
        .expect("final projection")
        .log
        .events();
    events
        .iter()
        .find_map(|event| match &event.payload {
            EventPayload::ToolCallTerminated { termination }
                if termination.tool_call_id == call_id =>
            {
                assert_eq!(termination.outcome, ToolTerminationOutcome::Cancelled);
                let result = termination.result.as_ref().expect("incomplete output");
                let retained = result.retained_output.as_ref().unwrap();
                assert!(retained.incomplete);
                let page = fixture
                    .engine
                    .read_artifact(
                        session.session_id,
                        &format!("artifact://{}", retained.streams[0].sha256),
                        0,
                        10,
                    )
                    .unwrap();
                assert_eq!(page.content, "authoritative start\nauthoritative cleanup\n");
                assert!(!result.output.contains("before cancellation"));
                assert_eq!(
                    termination.error.as_ref().unwrap().message.as_str(),
                    "tool call cancelled after it started"
                );
                Some(event.seq)
            }
            _ => None,
        })
        .expect("terminal sequence");
    // Progress is live-only: every chunk, including the cleanup's, was
    // delivered ahead of the termination, and none was stored.
    assert!(!events.iter().any(|event| event.payload.is_transient()));
    assert_eq!(
        live_progress_chunks(&live, call_id).await,
        ["before cancellation", "during cancellation cleanup"]
    );
    assert!(events.iter().any(|event| matches!(
        event.payload,
        EventPayload::ToolStdinSubmitted { tool_call_id, byte_count }
            if tool_call_id == call_id && byte_count == 6
    )));

    assert_eq!(
        with_watchdog("captured fixture completion", captured)
            .await
            .expect("scripted server")
            .len(),
        1
    );
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn cancelling_non_delegate_with_session_shaped_metadata_stays_generic() {
    for command in ["session-shaped", "null-session-shaped"] {
        let (fixture, session_id, run_id, call_id, stdin_received, _, captured, _) =
            start_streaming_bash_test_run(command, true).await;
        fixture
            .engine
            .tool_stdin(RunToolStdinParams {
                run_id,
                call_id,
                data: Some(STANDARD.encode(b"input\n")),
                eof: false,
            })
            .await
            .expect("stdin accepted");
        tokio::time::timeout(test_timeout(2), stdin_received.notified())
            .await
            .expect("executor received stdin");
        fixture
            .engine
            .cancel_run(run_id)
            .await
            .expect("cancel external tool");
        let terminal = await_event(
            &fixture.engine,
            session_id,
            "external tool cancellation",
            |event| {
                matches!(&event.payload, EventPayload::ToolCallTerminated { termination }
                if termination.tool_call_id == call_id)
            },
        )
        .await;
        let EventPayload::ToolCallTerminated { termination } = terminal.payload else {
            unreachable!("awaited tool termination");
        };
        assert_eq!(termination.outcome, ToolTerminationOutcome::Cancelled);
        let result = termination.result.as_ref().expect("incomplete output");
        assert!(result.retained_output.as_ref().unwrap().incomplete);
        assert!(result.metadata.is_null());
        assert!(!result.output.contains("external cleanup result"));
        assert_eq!(
            termination.error.unwrap().message.as_str(),
            "tool call cancelled after it started"
        );
        assert_eq!(
            with_watchdog("captured fixture completion", captured)
                .await
                .expect("scripted server")
                .len(),
            1
        );
        fixture.engine.shutdown().await;
    }
}

#[tokio::test]
async fn cancellation_deadline_bounds_a_wedged_tool_without_hanging() {
    use futures_util::FutureExt as _;

    let (fixture, session_id, run_id, call_id, stdin_received, cleanup_progress_sent, captured, _) =
        start_streaming_bash_test_run("wedge", true).await;
    fixture
        .engine
        .tool_stdin(RunToolStdinParams {
            run_id,
            call_id,
            data: Some(STANDARD.encode(b"input\n")),
            eof: false,
        })
        .await
        .expect("interactive stdin accepted");
    tokio::time::timeout(test_timeout(2), stdin_received.notified())
        .await
        .expect("executor received stdin");
    // Hold the blocking job after it enqueues progress but before send().await can
    // resume. Progress receipt, not the producer's continuation, proves acceptance.
    let (delivery_enqueued, release_delivery) = crate::runtime::block_artifact_io_for_test(
        fixture.engine.inner.artifacts.io_test_hook(),
        "capture_delivery",
        None,
    );
    let cancelled_at = std::time::Instant::now();
    fixture
        .engine
        .cancel_run(run_id)
        .await
        .expect("cancel wedged run");
    tokio::time::timeout(test_timeout(1), delivery_enqueued)
        .await
        .expect("blocking delivery reached")
        .expect("blocking job enqueued progress");
    assert!(cleanup_progress_sent.notified().now_or_never().is_none());
    release_delivery
        .send(())
        .expect("release delivered I/O job");
    let terminal = await_event(
        &fixture.engine,
        session_id,
        "bounded cancellation cleanup",
        |event| {
            matches!(
                &event.payload,
                EventPayload::ToolCallTerminated { termination }
                    if termination.tool_call_id == call_id && termination.error.is_some()
            )
        },
    )
    .await;
    let EventPayload::ToolCallTerminated { termination } = terminal.payload else {
        unreachable!("awaited tool termination")
    };
    assert_eq!(termination.outcome, ToolTerminationOutcome::Cancelled);
    let result = termination
        .result
        .as_ref()
        .expect("incomplete output survives display discard");
    let retained = result.retained_output.as_ref().unwrap();
    assert!(retained.incomplete);
    assert_eq!(
        fixture
            .engine
            .read_artifact(
                session_id,
                &format!("artifact://{}", retained.streams[0].sha256),
                0,
                10
            )
            .unwrap()
            .content,
        "authoritative start\nauthoritative cleanup\n"
    );
    let error_message = termination
        .error
        .expect("termination error")
        .message
        .to_string();
    assert!(cancelled_at.elapsed() < std::time::Duration::from_secs(3));
    assert!(
        error_message.contains("cleanup deadline elapsed"),
        "{error_message}"
    );
    assert_eq!(
        with_watchdog("captured fixture completion", captured)
            .await
            .expect("scripted server")
            .len(),
        1
    );
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn user_input_transform_audit_uses_the_final_chain_value() {
    let capabilities = r#"{"producer_messaging":false,"tools":false,"resources":false,"subscribe_events":false,"subscribe_bus":false,"publish_bus":false,"publish_session_events":false,"intercept":["user_before_input"]}"#;
    let cases = [
        (
            "noop",
            vec![("only", r#"{"action":"transform","new_text":"original"}"#)],
            "original",
            None,
        ),
        (
            "returned",
            vec![
                (
                    "first",
                    r#"{"action":"transform","new_text":"intermediate"}"#,
                ),
                ("second", r#"{"action":"transform","new_text":"original"}"#),
            ],
            "original",
            None,
        ),
        (
            "changed",
            vec![("only", r#"{"action":"transform","new_text":"transformed"}"#)],
            "transformed",
            Some(("original", "transformed")),
        ),
    ];

    for (name, results, expected_input, expected_audit) in cases {
        let (endpoint, captured) = scripted_model_server().await;
        let (mut fixture, selection) = custom_fixture_with_endpoint(&endpoint);
        let plugins = results
            .into_iter()
            .map(|(plugin, result)| {
                (
                    plugin.into(),
                    interception_plugin(
                        plugin,
                        &[
                            ("FIXTURE_CAPABILITIES", capabilities.into()),
                            ("FIXTURE_USER_BEFORE_INPUT_RESULT", result.into()),
                        ],
                    ),
                )
            })
            .collect();
        reopen_with_interception_plugins(&mut fixture, plugins).await;
        let session = fixture.engine.create_session(selection.clone()).unwrap();
        fixture
            .engine
            .start_run(
                RunStartParams {
                    reset_fallback: false,
                    session_id: session.session_id,
                    client_run_id: ClientRunId::new(format!("user-transform-{name}")).unwrap(),
                    selection,
                    input: "original".into(),
                },
                cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
            )
            .await
            .expect("run starts after transform chain");
        wait_for_session_not_running(&fixture.engine, session.session_id).await;
        let request = with_watchdog("captured fixture completion", captured)
            .await
            .unwrap();
        assert!(request.contains(expected_input));
        let events = fixture
            .engine
            .inner
            .store
            .get(session.session_id)
            .unwrap()
            .log
            .events();
        assert!(events.iter().any(|event| matches!(
            &event.payload,
            EventPayload::UserInputSubmitted { input } if input == expected_input
        )));
        let audits = events
            .iter()
            .filter_map(|event| match &event.payload {
                EventPayload::UserInputTransformed {
                    original_input,
                    input,
                } => Some((original_input.as_str(), input.as_str())),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(audits, expected_audit.into_iter().collect::<Vec<_>>());
        fixture.engine.shutdown().await;
    }
}

#[tokio::test]
async fn active_run_steering_uses_user_input_interception_and_audit() {
    let (endpoint, responses, captured) = scripted_channel_server(2).await;
    let (mut fixture, selection) = custom_fixture_with_endpoint(&endpoint);
    let capabilities = r#"{"producer_messaging":false,"tools":false,"resources":false,"subscribe_events":false,"subscribe_bus":false,"publish_bus":false,"publish_session_events":false,"intercept":["user_before_input"]}"#;
    reopen_with_interception_plugins(
        &mut fixture,
        vec![(
            "steer".into(),
            interception_plugin(
                "steer",
                &[
                    ("FIXTURE_CAPABILITIES", capabilities.into()),
                    ("FIXTURE_USER_TRANSFORM_FROM", "steer original".into()),
                    ("FIXTURE_USER_TRANSFORM_TO", "steer transformed".into()),
                ],
            ),
        )],
    )
    .await;
    let session = fixture.engine.create_session(selection.clone()).unwrap();
    let started = fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: session.session_id,
                client_run_id: ClientRunId::new("intercepted-steer").unwrap(),
                selection,
                input: "initial prompt".into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .unwrap();
    let steered = fixture
        .engine
        .steer(
            started.run_id,
            "steer original".into(),
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .unwrap();
    assert!(steered.accepted);
    assert!(steered.handled_reason.is_none());
    responses
        .send(MatchedScriptedResponse::last_message_contains(
            "initial prompt",
            scripted_text_body("first"),
        ))
        .unwrap();
    responses
        .send(MatchedScriptedResponse::last_message_contains(
            "steer transformed",
            scripted_text_body("second"),
        ))
        .unwrap();
    wait_for_session_not_running(&fixture.engine, session.session_id).await;
    let requests = with_watchdog("captured fixture completion", captured)
        .await
        .unwrap();
    assert_eq!(requests.len(), 2);
    assert!(requests[1].contains("steer transformed"));
    let events = fixture
        .engine
        .inner
        .store
        .get(session.session_id)
        .unwrap()
        .log
        .events();
    assert!(events.iter().any(|event| matches!(
        &event.payload,
        EventPayload::UserInputTransformed { original_input, input }
            if original_input == "steer original" && input == "steer transformed"
    )));
    assert!(events.iter().any(|event| matches!(
        &event.payload,
        EventPayload::UserInputAdmitted { input } if input == "steer transformed"
    )));
    fixture.engine.shutdown().await;
}
