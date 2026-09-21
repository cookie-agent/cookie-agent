use std::{fs, sync::Arc};

use cookie_agent_config::ModelRetryConfig;

use cookie_agent_protocol::{
    ClientRunId, EventPayload, PermissionMode, RunStartParams, SessionStatus,
    ToolTerminationOutcome,
};

use crate::{EngineError, EngineHistoryView, runtime::ModelRetrySleepMode};

use super::support::*;

#[tokio::test]
async fn retry_loop_uses_exact_standard_and_overload_budgets_then_falls_back() {
    assert_retry_budget_and_fallback(500, 4).await;
    assert_retry_budget_and_fallback(503, 6).await;
}

#[tokio::test]
async fn mid_stream_failure_still_consumes_standard_retries_before_fallback() {
    let retry = ModelRetryConfig {
        backoff_ceiling_ms: 1,
        ..ModelRetryConfig::default()
    };
    let attempts_on_first = (ModelRetryConfig::default().standard_retries.max(0) as usize) + 1;
    let mut responses = vec![RetryModelResponse::PartialError; attempts_on_first];
    responses.push(RetryModelResponse::Success);
    let (endpoint, captured) = retry_model_server(responses).await;
    let (fixture, selection) = retry_fixture_with_endpoint(&endpoint, retry).await;
    fixture
        .engine
        .inner
        .test_hooks
        .model_retry_sleep_hook
        .set_mode(ModelRetrySleepMode::Immediate);
    let session = fixture.engine.create_session(selection.clone()).unwrap();
    fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: session.session_id,
                client_run_id: ClientRunId::new("mid-stream-retry-budget").unwrap(),
                selection,
                input: "exercise mid-stream retry budget".into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .unwrap();
    let projection = await_projection(
        &fixture.engine,
        session.session_id,
        "mid-stream retry fallback completion",
        |projection| projection.status == SessionStatus::Completed,
    )
    .await;
    let requests = with_watchdog("captured fixture completion", captured)
        .await
        .expect("mid-stream retry requests");
    assert_eq!(requests.len(), attempts_on_first + 1);
    assert!(
        requests[..attempts_on_first]
            .iter()
            .all(|request| request_body(request)["model"] == "group/model")
    );
    assert_eq!(
        request_body(requests.last().unwrap())["model"],
        "group/fallback"
    );

    let events = projection.log.events();
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event.payload, EventPayload::AttemptAbandoned { .. }))
            .count(),
        attempts_on_first
    );
    for model_error in events.iter().filter_map(|event| match &event.payload {
        EventPayload::AttemptAbandoned { model_error, .. } => Some(model_error),
        _ => None,
    }) {
        let error = model_error
            .as_ref()
            .expect("abandoned attempt records its cause");
        assert_eq!(
            error.stage,
            cookie_agent_protocol::ModelErrorStage::StreamEvent
        );
        assert!(error.retryable, "mid-stream failure stays retryable");
    }
    assert!(events.iter().any(|event| matches!(
        event.payload,
        EventPayload::ModelFallback {
            attempts_on_from,
            ..
        } if attempts_on_from as usize == attempts_on_first
    )));
    let committed = events
        .iter()
        .filter(|event| matches!(event.payload, EventPayload::ModelTurnCommitted { .. }))
        .count();
    assert_eq!(committed, 1, "partial streams never commit a turn");
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn infinite_overload_retry_is_cancelled_during_backoff_without_fallback() {
    let (endpoint, captured) = retry_model_server(vec![RetryModelResponse::Status(503)]).await;
    let retry = ModelRetryConfig {
        overload_retries: -1,
        ..ModelRetryConfig::default()
    };
    let (fixture, selection) = retry_fixture_with_endpoint(&endpoint, retry).await;
    fixture
        .engine
        .inner
        .test_hooks
        .model_retry_sleep_hook
        .set_mode(ModelRetrySleepMode::Blocked);
    let session = fixture.engine.create_session(selection.clone()).unwrap();
    let run = fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: session.session_id,
                client_run_id: ClientRunId::new("infinite-overload-cancel").unwrap(),
                selection,
                input: "cancel overloaded model".into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .unwrap();
    fixture
        .engine
        .inner
        .test_hooks
        .model_retry_sleep_hook
        .wait_until_reached(1)
        .await;
    fixture
        .engine
        .cancel_run(run.run_id)
        .await
        .expect("cancel run");
    let projection = await_projection(
        &fixture.engine,
        session.session_id,
        "cancelled overload retry",
        |projection| projection.status == SessionStatus::Cancelled,
    )
    .await;
    assert_eq!(
        with_watchdog("captured fixture completion", captured)
            .await
            .expect("overload request")
            .len(),
        1
    );
    assert_eq!(
        projection
            .log
            .events()
            .iter()
            .filter(|event| matches!(event.payload, EventPayload::AttemptAbandoned { .. }))
            .count(),
        1
    );
    assert!(
        !projection
            .log
            .events()
            .iter()
            .any(|event| matches!(event.payload, EventPayload::ModelFallback { .. }))
    );
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn empty_stream_deltas_are_not_logged() {
    let (endpoint, responses, _captured) = scripted_channel_server(1).await;
    // Some providers emit empty content/reasoning chunks ahead of the real
    // stream; they must not reach the durable event log. (The fixture
    // provider's reasoning field is `none`, so the reasoning chunks are
    // ignored by the adaptor and only document the real-world shape.)
    let body = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"\"},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"\"},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"thinking\"},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
    );
    responses
        .send(MatchedScriptedResponse::last_message_role(
            "user",
            body.to_owned(),
        ))
        .unwrap();
    let (fixture, selection) = custom_fixture_with_endpoint(&endpoint);
    let session = fixture.engine.create_session(selection.clone()).unwrap();
    fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: session.session_id,
                client_run_id: ClientRunId::new("empty-deltas").unwrap(),
                selection: selection.clone(),
                input: "say hi".into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .unwrap();
    wait_for_session_not_running(&fixture.engine, session.session_id).await;
    let events = fixture
        .engine
        .inner
        .store
        .get(session.session_id)
        .unwrap()
        .log
        .events();
    let text = events
        .iter()
        .filter_map(|event| match &event.payload {
            EventPayload::TextDelta { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(text, ["hi"]);
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn responses_message_transport_fields_allow_approval_and_summary_checkpoint() {
    let arguments = serde_json::json!({"value":"approved"}).to_string();
    let call = serde_json::json!({"type":"function_call","id":"function","call_id":"write-call","name":"write","arguments":arguments});
    let tool = responses_metadata_sse(vec![
        serde_json::json!({"type":"response.created","response":{"id":"tool-response","model":"group/model"}}),
        serde_json::json!({"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"function","call_id":"write-call","name":"write","arguments":""}}),
        serde_json::json!({"type":"response.function_call_arguments.delta","item_id":"function","output_index":0,"delta":arguments}),
        serde_json::json!({"type":"response.output_item.done","output_index":0,"item":call}),
        serde_json::json!({"type":"response.completed","response":{"id":"tool-response","status":"completed","output":[call]}}),
    ]);
    let (endpoint, captured, _, _) = scripted_server_with_delayed_response(
        vec![
            tool,
            responses_text_with_transport_fields(r#"{"decision":"allow"}"#, None),
            responses_text_with_transport_fields("write completed", Some("final_answer")),
            responses_text_with_transport_fields("summary with transport fields", None),
        ],
        usize::MAX,
    )
    .await;
    let (mut fixture, selection) = custom_fixture_with_capabilities(
        &endpoint,
        "---\ndescription: Responses internal output\nmode: primary\nenabled: true\nmodels: [{ model: \"custom.test/group/model\", variant: base }]\npermissions:\n  write: ask\n---\nTest internal output.\n",
        Some((
            "approval.md",
            "---\ndescription: Responses approval\nmode: internal\nenabled: true\nmodels: [{ model: \"${parent_model}\" }]\nlimits: { timeout_ms: 30000, max_output_tokens: 128 }\npermissions: {}\n---\nEvaluate approval requests.\n",
        )),
        None,
        false,
        None,
        None,
        8192,
        None,
        "openai-responses",
        Some(RESPONSES_REPLAY_CAPABILITIES),
    );
    fixture.engine.shutdown().await;
    fixture.config.runtime.context_compaction.keep_recent_tokens = 0;
    fixture.engine = reopen_engine(&fixture);
    let executed = Arc::new(TestFlag::default());
    fixture
        .engine
        .register_tool_provider(Arc::new(TestWriteProvider {
            executed: Arc::clone(&executed),
        }));
    let session = fixture.engine.create_session(selection.clone()).unwrap();
    fixture
        .engine
        .set_permission_mode(session.session_id, PermissionMode::AutoApprove)
        .unwrap();
    fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: session.session_id,
                client_run_id: ClientRunId::new("responses-internal-output").unwrap(),
                selection,
                input: "write a test value".into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .unwrap();
    await_projection(
        &fixture.engine,
        session.session_id,
        "approved Responses tool completion",
        |session| session.status == SessionStatus::Completed,
    )
    .await;
    assert!(
        executed.is_set(),
        "valid approval text must not degrade to ask"
    );
    assert!(
        fixture
            .engine
            .compact_session(
                session.session_id,
                None,
                cookie_agent_protocol::EventOrigin::new("client:test").unwrap()
            )
            .await
            .unwrap()
    );
    let events = fixture
        .engine
        .inner
        .store
        .get(session.session_id)
        .unwrap()
        .log
        .events();
    assert!(events.iter().any(|event| matches!(&event.payload, EventPayload::ContextCheckpointCommitted { commit }
        if matches!(&commit.checkpoint, cookie_agent_protocol::ContextCheckpoint::InternalSummary { checkpoint } if checkpoint.summary() == "summary with transport fields"))));
    assert!(!events.iter().any(|event| matches!(
        &event.payload,
        EventPayload::InternalAgentFailed { .. } | EventPayload::ApprovalEscalated { .. }
    )));
    assert!(events.iter().any(|event| matches!(&event.payload, EventPayload::ModelTurnCommitted { turn, .. }
        if turn.content.iter().any(|part| matches!(part, cookie_agent_protocol::PersistedAssistantPart::Custom { kind, data, metadata }
            if kind.as_str() == "openai.responses.message_continuation" && metadata.is_none() && data["item_sha256"].as_str().is_some_and(|digest| digest.len() == 64))))),
        "the real SDK witness must remain in persisted history");
    let history = fixture
        .engine
        .get_history(session.session_id, EngineHistoryView::Full)
        .await
        .unwrap();
    assert!(
        serde_json::to_string(&history)
            .unwrap()
            .contains("openai.responses.message_continuation"),
        "restoration must retain the witness"
    );
    let requests = with_watchdog("captured fixture completion", captured)
        .await
        .unwrap();
    assert_eq!(requests.len(), 4);
    assert!(
        requests
            .iter()
            .all(|request| request.starts_with("POST /v1/responses "))
    );
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn responses_message_transport_fields_allow_generated_titles() {
    let body = responses_text_with_transport_fields("Transport title", None);
    let (endpoint, captured, _, _) =
        scripted_server_with_delayed_response(vec![body.clone(), body], usize::MAX).await;
    let (fixture, selection) = custom_fixture_with_capabilities(
        &endpoint,
        "---\ndescription: Responses title output\nmode: primary\nenabled: true\nmodels: [{ model: \"custom.test/group/model\", variant: base }]\npermissions: {}\n---\nTest title output.\n",
        None,
        None,
        true,
        None,
        None,
        8192,
        None,
        "openai-responses",
        Some(RESPONSES_REPLAY_CAPABILITIES),
    );
    let session = fixture.engine.create_session(selection.clone()).unwrap();
    fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: session.session_id,
                client_run_id: ClientRunId::new("responses-title-output").unwrap(),
                selection,
                input: "Create a transport title".into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .unwrap();
    await_projection(&fixture.engine, session.session_id, "Responses generated title", |session| {
        session.status == SessionStatus::Completed && session.log.events().iter().any(|event| matches!(&event.payload,
            EventPayload::SessionTitleCommitted { change: cookie_agent_protocol::SessionTitleChange::InternalAgentSet { title, .. }, .. }
            if title.as_str() == "Transport title"))
    }).await;
    assert_eq!(
        with_watchdog("captured fixture completion", captured)
            .await
            .unwrap()
            .len(),
        2
    );
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn scripted_root_run_completes_through_the_real_adapter_and_reopens() {
    let (endpoint, captured) = scripted_model_server().await;
    let (fixture, selection) = custom_fixture_with_endpoint(&endpoint);
    let session = fixture
        .engine
        .create_session(selection.clone())
        .expect("session");
    assert!(matches!(
        fixture
            .engine
            .get_history(session.session_id, EngineHistoryView::Assembled)
            .await,
        Err(EngineError::NoRunnableModel)
    ));
    fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: session.session_id,
                client_run_id: cookie_agent_protocol::ClientRunId::new("scripted-root")
                    .expect("run ID"),
                selection,
                input: "hello scripted model".to_owned(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("accepted run");
    await_projection(
        &fixture.engine,
        session.session_id,
        "scripted run completion",
        |projection| projection.status == SessionStatus::Completed,
    )
    .await;
    let request = with_watchdog("captured fixture completion", captured)
        .await
        .expect("scripted server task");
    assert!(request.starts_with("POST /v1/chat/completions? HTTP/1.1"));
    assert!(
        fixture
            .engine
            .inner
            .store
            .get(session.session_id)
            .expect("completed projection")
            .log
            .events()
            .iter()
            .any(|event| matches!(
                &event.payload,
                EventPayload::RunCompleted { final_text: Some(text) }
                    if text == "scripted root complete"
            ))
    );
    let assembled = fixture
        .engine
        .get_history(session.session_id, EngineHistoryView::Assembled)
        .await
        .expect("assembled tool history");
    let serialized = serde_json::to_string(&assembled).expect("serialize assembled history");
    assert!(serialized.contains("hello scripted model"));
    assert!(serialized.contains("scripted root complete"));
    assert_eq!(
        fixture
            .engine
            .get_history(session.session_id, EngineHistoryView::Full)
            .await
            .expect("full tool history"),
        assembled
    );
    fixture.engine.shutdown().await;
    let reopened = reopen_engine(&fixture);
    assert_eq!(
        reopened
            .get_session(session.session_id)
            .expect("reopened scripted session")
            .status,
        cookie_agent_protocol::SessionStatus::Completed
    );
    reopened.shutdown().await;
}

#[tokio::test]
async fn scripted_read_media_attaches_when_capable_and_fails_cleanly_when_incapable() {
    const PNG: &[u8] = &[
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x04, 0x00, 0x00, 0x00, 0xb5,
        0x1c, 0x0c, 0x02, 0x00, 0x00, 0x00, 0x0b, 0x49, 0x44, 0x41, 0x54, 0x78, 0xda, 0x63, 0x64,
        0xf8, 0x0f, 0x00, 0x01, 0x05, 0x01, 0x01, 0x27, 0x18, 0xe3, 0x66, 0x00, 0x00, 0x00, 0x00,
        0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
    ];
    let primary = "---\ndescription: Media read test\nmode: primary\nenabled: true\nmodels: [{ model: \"custom.test/group/model\", variant: base }]\npermissions:\n  read: allow\n---\nRead media.\n";
    let image_capabilities = "input = [\"text\", \"image\"]\noutput = [\"text\"]\ncontext_tokens = 4096\noutput_tokens = 1024\ntool_calling = true\nparallel_tool_calls = true\nstructured_output = false\nreasoning = false\ntemperature = true\ntop_p = true\nseed = false\nnative_replay = \"unsupported\"\nmedia = { image = { mime_types = [\"image/png\"], max_bytes = 20971520, max_count = 1 } }";
    let text_capabilities = "input = [\"text\"]\noutput = [\"text\"]\ncontext_tokens = 4096\noutput_tokens = 1024\ntool_calling = true\nparallel_tool_calls = true\nstructured_output = false\nreasoning = false\ntemperature = true\ntop_p = true\nseed = false\nnative_replay = \"unsupported\"\nmedia = {}";

    for (capabilities, capable) in [(image_capabilities, true), (text_capabilities, false)] {
        let bodies = vec![
            anthropic_tool_body(
                "read-image",
                "read",
                serde_json::json!({"filePath":"pixel.png"}),
            ),
            anthropic_usage_body("continued after read", 1, 0, 0),
        ];
        let (endpoint, captured, _reached, _release) =
            scripted_server_with_delayed_response(bodies, usize::MAX).await;
        let (fixture, selection) = custom_fixture_with_capabilities(
            &endpoint,
            primary,
            None,
            None,
            false,
            None,
            None,
            4_096,
            None,
            "anthropic-compatible",
            Some(capabilities),
        );
        fs::write(fixture._directory.path().join("pixel.png"), PNG).unwrap();
        fixture
            .engine
            .register_tool_provider(Arc::new(TestMediaReadProvider));
        let session = fixture.engine.create_session(selection.clone()).unwrap();
        fixture
            .engine
            .start_run(
                RunStartParams {
                    reset_fallback: false,
                    session_id: session.session_id,
                    client_run_id: ClientRunId::new(if capable {
                        "media-capable"
                    } else {
                        "media-incapable"
                    })
                    .unwrap(),
                    selection,
                    input: "read the image".into(),
                },
                cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
            )
            .await
            .unwrap();
        wait_for_session_not_running(&fixture.engine, session.session_id).await;
        assert_eq!(
            fixture
                .engine
                .get_session(session.session_id)
                .unwrap()
                .status,
            SessionStatus::Completed
        );
        let requests = with_watchdog("captured fixture completion", captured)
            .await
            .unwrap();
        assert_eq!(requests.len(), 2);
        let follow_up = request_body(&requests[1]);
        if capable {
            let tool_result = &follow_up["messages"][2]["content"][0];
            assert_eq!(tool_result["content"][2]["type"], "image");
            assert_eq!(
                tool_result["content"][2]["source"]["media_type"],
                "image/png"
            );
            assert_eq!(
                follow_up["messages"][2]["content"]
                    .as_array()
                    .unwrap()
                    .last()
                    .unwrap()["cache_control"]["ttl"],
                "5m"
            );
        } else {
            let tool_result = &follow_up["messages"][2]["content"][0];
            assert_eq!(tool_result["is_error"], true);
            let parts: Vec<oven_sdk::ContentValue> = serde_json::from_str(
                tool_result["content"]
                    .as_str()
                    .expect("serialized error content"),
            )
            .unwrap();
            let text = parts
                .iter()
                .filter_map(|part| match part {
                    oven_sdk::ContentValue::Text(text) => Some(text.as_str()),
                    _ => None,
                })
                .collect::<String>();
            assert!(text.contains("Cannot attach image/png: the active model \"custom.test/group/model\" does not accept image inputs"), "{text}");
            let projection = fixture.engine.inner.store.get(session.session_id).unwrap();
            let events = projection.log.events();
            let termination = events
                .iter()
                .find_map(|event| match &event.payload {
                    EventPayload::ToolCallTerminated { termination } => Some(termination),
                    _ => None,
                })
                .unwrap();
            assert_eq!(termination.outcome, ToolTerminationOutcome::Failed);
            assert!(
                termination
                    .error
                    .as_ref()
                    .unwrap()
                    .message
                    .as_str()
                    .contains("does not accept image inputs")
            );
        }
        fixture.engine.shutdown().await;
    }
}

#[tokio::test]
async fn shutdown_joins_in_flight_run_tasks_and_records_run_cancelled() {
    // The fixture answers nothing until it is released, so the run is parked in
    // its provider stream for the whole of shutdown.
    let (endpoint, server, reached, release) =
        scripted_server_with_delayed_response(vec![scripted_text_body("never delivered")], 0).await;
    let (fixture, selection) = custom_fixture_with_endpoint(&endpoint);
    let session = fixture
        .engine
        .create_session(selection.clone())
        .expect("shutdown cancellation session");
    let started = fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: session.session_id,
                client_run_id: ClientRunId::new("shutdown-cancellation").expect("run ID"),
                selection,
                input: "stall in the provider stream".into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("accepted stalled run");
    with_watchdog("stalled request reached server", reached)
        .await
        .expect("stalled request reached server");

    with_watchdog("engine shutdown", fixture.engine.shutdown()).await;

    let projection = fixture
        .engine
        .inner
        .store
        .get(session.session_id)
        .expect("stalled session projection");
    assert_eq!(
        projection
            .runs
            .get(&started.run_id)
            .map(|run| run.status)
            .expect("stalled run record"),
        SessionStatus::Cancelled,
        "a clean shutdown terminalizes an in-flight run as cancelled"
    );
    let terminal = projection
        .log
        .events()
        .iter()
        .rfind(|event| event.run_id == Some(started.run_id))
        .map(|event| event.payload.clone())
        .expect("terminal run event");
    assert!(
        matches!(terminal, EventPayload::RunCancelled { .. }),
        "the run's last event is its cancellation, not an unterminated run: {terminal:?}"
    );

    release.notify_waiters();
    server.abort();
}
