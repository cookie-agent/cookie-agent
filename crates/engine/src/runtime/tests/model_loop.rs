use std::{fs, sync::Arc};

use cookie_agent_config::ModelRetryConfig;

use cookie_agent_protocol::{
    ClientRunId, EventPayload, EventSubscriptionMessage, PermissionMode, RunStartParams,
    SessionStatus, ToolTerminationOutcome,
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
async fn interrupted_stream_commits_its_partial_turn_before_the_abandonment() {
    let (endpoint, server) = scripted_stalled_stream_server("partial answer").await;
    let (fixture, selection) = custom_fixture_with_endpoint(&endpoint);
    let session = fixture.engine.create_session(selection.clone()).unwrap();
    let run = fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: session.session_id,
                client_run_id: ClientRunId::new("interrupted-partial-turn").unwrap(),
                selection,
                input: "start a long answer".into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .unwrap();
    // Interrupt only once the attempt durably began streaming text, so it is
    // genuinely mid-stream when the abort lands.
    await_event(
        &fixture.engine,
        session.session_id,
        "streamed text started",
        |event| matches!(event.payload, EventPayload::ModelOutputStarted { .. }),
    )
    .await;
    fixture
        .engine
        .cancel_run(run.run_id)
        .await
        .expect("cancel run");
    let projection = await_projection(
        &fixture.engine,
        session.session_id,
        "interrupted partial turn",
        |projection| projection.status == SessionStatus::Cancelled,
    )
    .await;
    let events = projection.log.events();
    let commit = events
        .iter()
        .find(|event| matches!(event.payload, EventPayload::ModelTurnCommitted { .. }))
        .expect("the partial turn is committed");
    let EventPayload::ModelTurnCommitted {
        attempt_id, turn, ..
    } = &commit.payload
    else {
        unreachable!()
    };
    assert_eq!(
        turn.finish_reason,
        cookie_agent_protocol::ModelFinishReason::Aborted
    );
    assert!(
        turn.content.iter().any(|part| matches!(
            part,
            cookie_agent_protocol::PersistedAssistantPart::Text { text, .. }
                if text == "partial answer"
        )),
        "the committed turn keeps the partial text: {:?}",
        turn.content
    );
    assert_eq!(
        turn.response_metadata
            .get("oven.http_status")
            .and_then(serde_json::Value::as_u64),
        Some(200),
        "the abort commit keeps the telemetry from the response head"
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event.payload, EventPayload::ModelUsageRecorded { .. })),
        "an interrupted turn records no usage"
    );
    let abandoned = events
        .iter()
        .find(|event| matches!(event.payload, EventPayload::AttemptAbandoned { .. }))
        .expect("the attempt is abandoned");
    let EventPayload::AttemptAbandoned {
        attempt_id: abandoned_attempt,
        model_error,
    } = &abandoned.payload
    else {
        unreachable!()
    };
    assert_eq!(abandoned_attempt, attempt_id);
    assert_eq!(
        model_error.as_ref().map(|error| error.kind),
        Some(cookie_agent_protocol::ModelErrorKind::Abort)
    );
    assert!(
        commit.seq < abandoned.seq,
        "the commit precedes the abandonment"
    );
    let cancelled = events
        .iter()
        .find(|event| matches!(event.payload, EventPayload::RunCancelled { .. }))
        .expect("the run is cancelled");
    assert!(abandoned.seq < cancelled.seq);
    // The partial output reached disk only as the committed turn: none of the
    // live-only deltas did, and the reloaded history is the same.
    assert!(!events.iter().any(|event| event.payload.is_transient()));
    let reloaded = crate::events::EventLog::open_read_only(
        projection.log.path().to_owned(),
        session.session_id,
    )
    .expect("reload event log");
    assert_eq!(reloaded.events(), projection.log.events());
    server.abort();
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn stream_deltas_are_delivered_live_but_never_logged() {
    let (endpoint, responses, _captured) = scripted_channel_server(1).await;
    // Some providers emit empty content/reasoning chunks ahead of the real
    // stream; they carry nothing to deliver. (The fixture provider's
    // reasoning field is `none`, so the reasoning chunks are ignored by the
    // adaptor and only document the real-world shape.)
    let body = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"\"},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"\"},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"thinking\"},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"content\":\" there\"},\"finish_reason\":null}]}\n\n",
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
    let (snapshot, mut live) = fixture
        .engine
        .subscribe(session.session_id, None)
        .await
        .expect("subscribe");
    fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: session.session_id,
                client_run_id: ClientRunId::new("live-deltas").unwrap(),
                selection: selection.clone(),
                input: "say hi".into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .unwrap();
    let mut delivered = snapshot.events;
    let mut live_text = Vec::new();
    with_watchdog("live run delivery", async {
        loop {
            match live.recv().await.expect("live subscription") {
                EventSubscriptionMessage::Event { event } => {
                    let done = matches!(event.payload, EventPayload::RunCompleted { .. });
                    delivered.push(*event);
                    if done {
                        break;
                    }
                }
                EventSubscriptionMessage::Transient { event } => {
                    event.validate().expect("valid transient event");
                    // Live output sits right after the durable event it
                    // follows, and never takes a sequence of its own.
                    assert_eq!(
                        Some(event.after_seq),
                        delivered.last().map(|durable| durable.seq)
                    );
                    let EventPayload::TextDelta { text, .. } = event.payload else {
                        panic!("unexpected live output {:?}", event.payload);
                    };
                    live_text.push(text);
                }
                EventSubscriptionMessage::Gap { .. } => panic!("unexpected gap"),
                EventSubscriptionMessage::Rewound { .. } => panic!("unexpected rewind"),
            }
        }
    })
    .await;
    assert_eq!(live_text, ["hi", " there"]);

    // Durable sequences stay gap-free across the streamed turn.
    let log = fixture
        .engine
        .inner
        .store
        .get(session.session_id)
        .unwrap()
        .log
        .clone();
    let events = log.events();
    assert!(!events.iter().any(|event| event.payload.is_transient()));
    assert!(
        events
            .iter()
            .enumerate()
            .all(|(index, event)| event.seq == index as u64 + 1)
    );
    assert_eq!(delivered, events);
    // One durable mark records where the streamed text began.
    let marks = events
        .iter()
        .filter_map(|event| match &event.payload {
            EventPayload::ModelOutputStarted { kind, .. } => Some((*kind, event.seq)),
            _ => None,
        })
        .collect::<Vec<_>>();
    let commit_seq = events
        .iter()
        .find(|event| matches!(event.payload, EventPayload::ModelTurnCommitted { .. }))
        .expect("committed turn")
        .seq;
    assert!(matches!(
        marks.as_slice(),
        [(cookie_agent_protocol::StreamedOutputKind::Text, seq)] if *seq < commit_seq
    ));

    // Neither the file nor a reload of it holds the deltas.
    let bytes = fs::read(log.path()).expect("read events.jsonl");
    let text = String::from_utf8(bytes).expect("utf-8 log");
    assert!(!text.contains("\"text_delta\""));
    assert!(!text.contains("\"reasoning_delta\""));
    let reloaded =
        crate::events::EventLog::open_read_only(log.path().to_owned(), session.session_id)
            .expect("reload event log");
    assert_eq!(reloaded.events(), events);
    assert!(reloaded.diagnostics().is_empty());
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
async fn history_media_over_the_model_count_limit_elides_oldest_and_proceeds() {
    const PNG: &[u8] = &[
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x04, 0x00, 0x00, 0x00, 0xb5,
        0x1c, 0x0c, 0x02, 0x00, 0x00, 0x00, 0x0b, 0x49, 0x44, 0x41, 0x54, 0x78, 0xda, 0x63, 0x64,
        0xf8, 0x0f, 0x00, 0x01, 0x05, 0x01, 0x01, 0x27, 0x18, 0xe3, 0x66, 0x00, 0x00, 0x00, 0x00,
        0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
    ];
    let primary = "---\ndescription: Media limit test\nmode: primary\nenabled: true\nmodels: [{ model: \"custom.test/group/model\", variant: base }]\npermissions:\n  read: allow\n---\nRead media.\n";
    let capabilities = "input = [\"text\", \"image\"]\noutput = [\"text\"]\ncontext_tokens = 4096\noutput_tokens = 1024\ntool_calling = true\nparallel_tool_calls = true\nstructured_output = false\nreasoning = false\ntemperature = true\ntop_p = true\nseed = false\nnative_replay = \"unsupported\"\nmedia = { image = { mime_types = [\"image/png\"], max_bytes = 20971520, max_count = 1 } }";
    let bodies = vec![
        anthropic_tool_body(
            "read-first",
            "read",
            serde_json::json!({"filePath":"first.png"}),
        ),
        anthropic_tool_body(
            "read-second",
            "read",
            serde_json::json!({"filePath":"second.png"}),
        ),
        anthropic_usage_body("read both", 1, 0, 0),
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
    fs::write(fixture._directory.path().join("first.png"), PNG).unwrap();
    fs::write(fixture._directory.path().join("second.png"), PNG).unwrap();
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
                client_run_id: ClientRunId::new("media-limit").unwrap(),
                selection,
                input: "read both images".into(),
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
    let requests = with_watchdog("captured media limit completion", captured)
        .await
        .unwrap();
    assert_eq!(requests.len(), 3);
    let last = serde_json::to_string(&request_body(&requests[2])).unwrap();
    assert_eq!(last.matches("\"type\":\"image\"").count(), 1, "{last}");
    assert_eq!(
        last.matches("[image omitted: over the model's per-request image limit]")
            .count(),
        1,
        "{last}"
    );
    let placeholder = last
        .find("[image omitted: over the model's per-request image limit]")
        .unwrap();
    assert!(placeholder < last.find("\"type\":\"image\"").unwrap());
    // Durable history keeps both images; elision is request assembly only.
    let projection = fixture.engine.inner.store.get(session.session_id).unwrap();
    let durable = serde_json::to_string(&projection.log.events()).unwrap();
    assert!(!durable.contains("image omitted"));
    fixture.engine.shutdown().await;
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

#[tokio::test]
async fn model_tools_are_recorded_once_while_unchanged() {
    let (endpoint, responses, captured) = scripted_channel_server(2).await;
    for text in ["first reply", "second reply"] {
        responses
            .send(MatchedScriptedResponse::last_message_role(
                "user",
                scripted_text_body(text),
            ))
            .expect("scripted reply");
    }
    let (fixture, selection) = custom_fixture_with_endpoint_and_primary_agent(
        &endpoint,
        "---\ndescription: Model tools test\nmode: primary\nenabled: true\nmodels: [{ model: \"custom.test/group/model\", variant: base }]\npermissions:\n  read: allow\n  bash: ask\n---\nRecord the tools.\n",
    );
    fixture
        .engine
        .register_tool_provider(Arc::new(TestParallelToolProvider {
            state: Arc::new(ParallelToolState::default()),
            barrier: None,
        }));
    let session = fixture
        .engine
        .create_session(selection.clone())
        .expect("model tools session");
    for client_run in ["model-tools-1", "model-tools-2"] {
        fixture
            .engine
            .start_run(
                RunStartParams {
                    reset_fallback: false,
                    session_id: session.session_id,
                    client_run_id: ClientRunId::new(client_run).unwrap(),
                    selection: selection.clone(),
                    input: format!("run {client_run}"),
                },
                cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
            )
            .await
            .expect("model tools run");
        wait_for_session_not_running(&fixture.engine, session.session_id).await;
    }
    with_watchdog("captured model tools requests", captured)
        .await
        .expect("captured requests");

    let events = fixture
        .engine
        .inner
        .store
        .get(session.session_id)
        .unwrap()
        .log
        .events();
    let published = events
        .iter()
        .filter_map(|event| match &event.payload {
            EventPayload::ModelToolsPublished {
                attempt_id,
                tool_names,
            } => Some((event.seq, attempt_id, tool_names)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(published.len(), 1, "an unchanged tool set is recorded once");
    let (seq, attempt_id, tool_names) = published[0];
    assert!(tool_names.contains(&"parallel_read".to_owned()));
    assert!(tool_names.contains(&"parallel_bash".to_owned()));
    assert!(!tool_names.contains(&"parallel_write".to_owned()));
    let prepared = events
        .iter()
        .find(|event| {
            matches!(
                &event.payload,
                EventPayload::ModelRequestPrepared { attempt_id: prepared, .. }
                    if prepared == attempt_id
            )
        })
        .expect("request prepared for the recorded attempt");
    assert!(seq < prepared.seq, "tools are recorded before the request");
}

/// Runs a model that makes the same allowed write three times and returns the
/// outputs the engine committed for those calls.
async fn repeated_write_outputs(loop_warning: bool) -> Vec<String> {
    let (endpoint, captured) = scripted_repeated_write_server(3).await;
    let (mut fixture, selection) = custom_fixture_with_endpoint_and_primary_agent(
        &endpoint,
        "---\ndescription: Loop test agent\nmode: primary\nenabled: true\nmodels: [{ model: \"custom.test/group/model\", variant: base }]\npermissions:\n  write: allow\n---\nTest loop warnings.\n",
    );
    if !loop_warning {
        fixture.engine.shutdown().await;
        fixture.config.runtime.loop_warning.enabled = false;
        fixture.engine = reopen_engine(&fixture);
    }
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
                client_run_id: ClientRunId::new("loop-warning").expect("run ID"),
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
        "run completion",
        |event| matches!(event.payload, EventPayload::RunCompleted { .. }),
    )
    .await;
    let outputs = fixture
        .engine
        .inner
        .store
        .get(session.session_id)
        .expect("projection")
        .log
        .events()
        .iter()
        .filter_map(|event| match &event.payload {
            EventPayload::ToolCallTerminated { termination } => termination
                .result
                .as_ref()
                .map(|result| result.output.clone()),
            _ => None,
        })
        .collect();
    captured.abort();
    fixture.engine.shutdown().await;
    outputs
}

#[tokio::test]
async fn repeating_identical_tool_calls_warns_the_model() {
    let outputs = repeated_write_outputs(true).await;
    assert_eq!(outputs.len(), 3);
    assert_eq!(outputs[..2], ["executed", "executed"]);
    assert!(outputs[2].starts_with("executed\n\n<system-reminder>\nRepeated tool calls detected"));
    assert!(outputs[2].contains("the same tool call 3 times in a row"));
}

#[tokio::test]
async fn loop_warning_can_be_disabled() {
    let outputs = repeated_write_outputs(false).await;
    assert_eq!(outputs, ["executed", "executed", "executed"]);
}
