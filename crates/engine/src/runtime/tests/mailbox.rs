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
    ];
    let (endpoint, captured, compaction_reached, release_compaction) =
        scripted_server_with_delayed_response(bodies, 2).await;
    let (fixture, selection) = custom_fixture_with_endpoint_primary_and_internal(
        &endpoint,
        "---\ndescription: Steering compaction test\nmode: primary\nenabled: true\nmodels: [{ model: \"custom.test/group/model\", variant: base }]\npermissions:\n  write: ask\n---\nTest steering compaction.\n",
        None,
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
    let first_pending = "first pending input with enough additional text to cross the learned predictive compaction threshold";
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
        .expect("promotion compaction started");
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
    assert_eq!(requests.len(), 4);
    assert!(!requests[0].contains("first pending"));
    for input in [first_pending, "third pending", "fourth pending"] {
        assert!(
            requests[3].contains(input),
            "missing {input:?}: {}",
            requests[3]
        );
    }
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
        .expect("predictive checkpoint");
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
        tool_result_seq < checkpoint_seq
            && checkpoint_seq < first_steering_seq
            && submitted.last().expect("submitted inputs").0 < next_attempt_seq
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
async fn cancel_during_start_prediction_aborts_compaction_without_appending_input() {
    let bodies = vec![
        "data: {\"choices\":[{\"delta\":{\"content\":\"first run complete\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":4000,\"completion_tokens\":1,\"total_tokens\":4001}}\n\n".to_owned(),
        "data: {\"choices\":[{\"delta\":{\"content\":\"late summary\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n".to_owned(),
    ];
    let (endpoint, captured, compaction_reached, release_compaction) =
        scripted_server_with_delayed_response(bodies, 1).await;
    let (fixture, selection) = custom_fixture_with_endpoint_primary_and_internal(
        &endpoint,
        "---\ndescription: Start cancellation test\nmode: primary\nenabled: true\nmodels: [{ model: \"custom.test/group/model\", variant: base }]\npermissions: {}\n---\nTest start cancellation.\n",
        None,
        Some(500),
        false,
    );
    let session = fixture
        .engine
        .create_session(selection.clone())
        .expect("cancellation session");
    fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: session.session_id,
                client_run_id: ClientRunId::new("prime-predictor").expect("client run ID"),
                selection: selection.clone(),
                input: "prime predictor".into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("first run started");
    wait_for_session_not_running(&fixture.engine, session.session_id).await;

    let start_engine = fixture.engine.clone();
    let second_selection = selection.clone();
    let start = tokio::spawn(async move {
        start_engine
            .start_run(
                RunStartParams {
                    reset_fallback: false,
                    session_id: session.session_id,
                    client_run_id: ClientRunId::new("cancel-prediction").expect("client run ID"),
                    selection: second_selection,
                    input: "must never be appended".into(),
                },
                cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
            )
            .await
    });
    with_watchdog("compaction_reached fixture completion", compaction_reached)
        .await
        .expect("start compaction reached");
    let run = fixture
        .engine
        .inner
        .store
        .get(session.session_id)
        .expect("start projection")
        .log
        .events()
        .iter()
        .rev()
        .find_map(|event| {
            matches!(event.payload, EventPayload::RunStarted { .. }).then_some(event.run_id)
        })
        .flatten()
        .expect("second run ID");
    assert!(fixture.engine.run_active_for_test(run));
    assert!(
        fixture
            .engine
            .compaction_reserved_for_test(session.session_id)
    );
    fixture
        .engine
        .cancel_run(run)
        .await
        .expect("cancel during prediction");
    assert_eq!(
        start
            .await
            .expect("start task")
            .expect("cancelled start result")
            .run_id,
        run
    );
    release_compaction.notify_one();
    let events = fixture
        .engine
        .inner
        .store
        .get(session.session_id)
        .expect("cancelled projection")
        .log
        .events();
    assert!(events.iter().any(|event| {
        event.run_id == Some(run)
            && matches!(event.payload, EventPayload::InternalAgentCancelled { .. })
    }));
    assert!(events.iter().any(|event| {
        event.run_id == Some(run) && matches!(event.payload, EventPayload::RunCancelled { .. })
    }));
    assert!(!events.iter().any(|event| {
        event.run_id == Some(run)
            && matches!(
                &event.payload,
                EventPayload::UserInputSubmitted { input } if input == "must never be appended"
            )
    }));
    assert!(!fixture.engine.run_active_for_test(run));
    assert!(
        !fixture
            .engine
            .compaction_reserved_for_test(session.session_id)
    );
    assert_eq!(
        with_watchdog("captured fixture completion", captured)
            .await
            .expect("cancel server task")
            .len(),
        2
    );
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
    let terminal_seq = events
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
    let chunks = events
        .iter()
        .filter_map(|event| match &event.payload {
            EventPayload::ToolCallProgress {
                tool_call_id,
                display: Some(chunk),
                ..
            } if *tool_call_id == call_id => Some((event.seq, chunk.as_str())),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        chunks.iter().map(|(_, chunk)| *chunk).collect::<Vec<_>>(),
        ["before cancellation", "during cancellation cleanup"]
    );
    assert!(chunks.iter().all(|(seq, _)| *seq < terminal_seq));
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
        let (fixture, session_id, run_id, call_id, stdin_received, _, captured) =
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
async fn cancellation_deadline_discards_wedged_progress_without_hanging() {
    use futures_util::FutureExt as _;

    let (fixture, session_id, run_id, call_id, stdin_received, cleanup_progress_sent, captured) =
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
    let cleanup_progress_blocked = fixture.engine.block_tool_progress_appends_for_test();
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
    tokio::time::timeout(test_timeout(1), cleanup_progress_blocked.notified())
        .await
        .expect("cleanup progress reached the wedged appender");
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
    assert!(
        cleanup_progress_sent.notified().now_or_never().is_none(),
        "the wedged consumer must not need the producer continuation"
    );
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
    assert!(
        error_message
            .contains("1 progress record(s) never entered the session mailbox and were discarded"),
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
async fn steer_during_start_prediction_survives_initial_submission_and_reaches_model() {
    let bodies = vec![
        "data: {\"choices\":[{\"delta\":{\"content\":\"prime complete\"},\"finish_reason\":null}],\"usage\":{\"prompt_tokens\":4000,\"completion_tokens\":1,\"total_tokens\":4001}}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n".to_owned(),
        "data: {\"choices\":[{\"delta\":{\"content\":\"start-time summary\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n".to_owned(),
        "data: {\"choices\":[{\"delta\":{\"content\":\"initial turn\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n".to_owned(),
        "data: {\"choices\":[{\"delta\":{\"content\":\"steered turn\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n".to_owned(),
    ];
    let (endpoint, captured, compaction_reached, release_compaction) =
        scripted_server_with_delayed_response(bodies, 1).await;
    let (fixture, selection) = custom_fixture_with_endpoint_primary_and_internal(
        &endpoint,
        "---\ndescription: Start steering race test\nmode: primary\nenabled: true\nmodels: [{ model: \"custom.test/group/model\", variant: base }]\npermissions: {}\n---\nTest start steering.\n",
        None,
        Some(500),
        false,
    );
    let session = fixture
        .engine
        .create_session(selection.clone())
        .expect("steering race session");
    let prime_input = format!(
        "prime predictor {}",
        "compressible historical context ".repeat(300)
    );
    fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: session.session_id,
                client_run_id: ClientRunId::new("prime-start-steer").expect("client run ID"),
                selection: selection.clone(),
                input: prime_input,
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("prime run");
    wait_for_session_not_running(&fixture.engine, session.session_id).await;

    let start_engine = fixture.engine.clone();
    let start = tokio::spawn(async move {
        start_engine
            .start_run(
                RunStartParams {
                    reset_fallback: false,
                    session_id: session.session_id,
                    client_run_id: ClientRunId::new("start-steer-race").expect("client run ID"),
                    selection,
                    input: "initial second-run input".into(),
                },
                cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
            )
            .await
    });
    with_watchdog("compaction_reached fixture completion", compaction_reached)
        .await
        .expect("start compaction reached");
    let run = fixture
        .engine
        .inner
        .store
        .get(session.session_id)
        .expect("start projection")
        .log
        .events()
        .iter()
        .rev()
        .find_map(|event| {
            matches!(event.payload, EventPayload::RunStarted { .. }).then_some(event.run_id)
        })
        .flatten()
        .expect("second run ID");
    let steering = "steer admitted before initial submission";
    assert!(
        fixture
            .engine
            .steer(
                run,
                steering.into(),
                cookie_agent_protocol::EventOrigin::new("client:test").unwrap()
            )
            .await
            .expect("steer during start compaction")
            .accepted
    );
    let during_compaction = fixture
        .engine
        .inner
        .store
        .get(session.session_id)
        .expect("admitted projection")
        .log
        .events();
    assert!(during_compaction.iter().any(|event| matches!(
        &event.payload,
        EventPayload::UserInputAdmitted { input } if input == steering
    )));
    assert!(!during_compaction.iter().any(|event| {
        event.run_id == Some(run)
            && matches!(event.payload, EventPayload::UserInputSubmitted { .. })
    }));
    release_compaction.notify_one();
    assert_eq!(
        start
            .await
            .expect("start task")
            .expect("started run")
            .run_id,
        run
    );
    wait_for_session_not_running(&fixture.engine, session.session_id).await;

    let requests = with_watchdog("captured fixture completion", captured)
        .await
        .expect("scripted requests");
    assert_eq!(requests.len(), 4);
    assert!(requests[2].contains("initial second-run input"));
    assert!(!requests[2].contains(steering));
    assert!(requests[3].contains("initial second-run input"));
    assert!(requests[3].contains(steering));
    let events = fixture
        .engine
        .inner
        .store
        .get(session.session_id)
        .expect("completed projection")
        .log
        .events();
    let submissions = events
        .iter()
        .filter_map(|event| match &event.payload {
            EventPayload::UserInputSubmitted { input } if event.run_id == Some(run) => {
                Some(input.as_str())
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(submissions, vec!["initial second-run input", steering]);
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

#[tokio::test]
async fn blocking_steering_uses_the_same_input_interception_and_audit() {
    let (endpoint, responses, captured) = scripted_channel_server(2).await;
    let (mut fixture, selection) = custom_fixture_with_endpoint(&endpoint);
    let capabilities = r#"{"producer_messaging":false,"tools":false,"resources":false,"subscribe_events":false,"subscribe_bus":false,"publish_bus":false,"publish_session_events":false,"intercept":["user_before_input"]}"#;
    reopen_with_interception_plugins(
        &mut fixture,
        vec![(
            "blocking".into(),
            interception_plugin(
                "blocking",
                &[
                    ("FIXTURE_CAPABILITIES", capabilities.into()),
                    ("FIXTURE_USER_TRANSFORM_FROM", "blocking original".into()),
                    ("FIXTURE_USER_TRANSFORM_TO", "blocking transformed".into()),
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
                client_run_id: ClientRunId::new("blocking-intercepted-steer").unwrap(),
                selection,
                input: "initial prompt".into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .unwrap();
    let blocking_engine = fixture.engine.clone();
    let steered = tokio::task::spawn_blocking(move || {
        blocking_engine.steer_blocking(
            started.run_id,
            "blocking original".into(),
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
    })
    .await
    .unwrap()
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
            "blocking transformed",
            scripted_text_body("second"),
        ))
        .unwrap();
    wait_for_session_not_running(&fixture.engine, session.session_id).await;
    let requests = with_watchdog("captured fixture completion", captured)
        .await
        .unwrap();
    assert!(requests[1].contains("blocking transformed"));
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
            if original_input == "blocking original" && input == "blocking transformed"
    )));
    assert!(events.iter().any(|event| matches!(
        &event.payload,
        EventPayload::UserInputAdmitted { input } if input == "blocking transformed"
    )));
    fixture.engine.shutdown().await;
}
