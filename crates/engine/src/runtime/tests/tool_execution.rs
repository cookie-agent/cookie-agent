use std::{
    fs,
    sync::{Arc, atomic::Ordering},
};

use cookie_agent_protocol::{
    ClientRunId, EventPayload, RunStartParams, SessionStatus, Sha256Digest, ToolTerminationOutcome,
};

use crate::EngineHistoryView;

use super::support::*;

#[tokio::test]
async fn normalized_tool_call_name_is_refused_before_execution() {
    let (endpoint, responses, server) = scripted_channel_server(2).await;
    responses
        .send(MatchedScriptedResponse::last_message_role(
            "user",
            scripted_tool_body("alias-call", "Bad Name", serde_json::json!({})),
        ))
        .expect("normalized alias response");
    responses
        .send(MatchedScriptedResponse::last_message_role(
            "tool",
            scripted_text_body("normalized alias refused"),
        ))
        .expect("follow-up response");
    let primary = "---\ndescription: Alias test agent\nmode: primary\nenabled: true\nmodels: [{ model: \"custom.test/group/model\", variant: base }]\npermissions:\n  write: allow\n---\nAlias test prompt.\n";
    let (fixture, selection) = custom_fixture_with_endpoint_and_primary_agent(&endpoint, primary);
    let executed = Arc::new(TestFlag::default());
    fixture
        .engine
        .register_tool_provider(Arc::new(TestAliasProvider {
            executed: Arc::clone(&executed),
        }));
    let session = fixture
        .engine
        .create_session(selection.clone())
        .expect("alias session");
    fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: session.session_id,
                client_run_id: ClientRunId::new("normalized-alias").expect("client run id"),
                selection,
                input: "call the aliased tool".into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("alias run started");
    await_projection(
        &fixture.engine,
        session.session_id,
        "normalized alias refusal completion",
        |projection| projection.status == SessionStatus::Completed,
    )
    .await;

    let requests = with_watchdog("server fixture completion", server)
        .await
        .expect("alias server");
    assert_eq!(
        requests.len(),
        2,
        "the failure must be fed back to the model"
    );
    assert!(requests[1].contains("\"role\":\"tool\""));

    let projection = fixture
        .engine
        .inner
        .store
        .get(session.session_id)
        .expect("alias projection");
    let events = projection.log.events();
    assert!(
        events.iter().any(|event| matches!(
            &event.payload,
            EventPayload::ModelTurnCommitted { warnings, .. }
                if warnings.iter().any(|warning| {
                    warning.as_str().contains("Bad Name")
                        && warning.as_str().contains("bad_name")
                })
        )),
        "the committed turn must carry the normalization warning"
    );
    let termination = events
        .iter()
        .find_map(|event| match &event.payload {
            EventPayload::ToolCallTerminated { termination }
                if termination.owner.model_call_id.as_str() == "alias-call" =>
            {
                Some(termination)
            }
            _ => None,
        })
        .expect("alias tool termination");
    assert_eq!(termination.outcome, ToolTerminationOutcome::Failed);
    let error = termination.error.as_ref().expect("alias tool error");
    assert_eq!(
        error.message.as_str(),
        "tool call name was normalized from invalid provider output"
    );
    assert!(
        !executed.is_set(),
        "the real tool behind the normalized alias must never execute"
    );
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn parallel_tools_start_in_model_order_and_terminate_in_completion_order() {
    let (endpoint, responses, captured) = scripted_channel_server(2).await;
    responses
        .send(MatchedScriptedResponse::last_message_role(
            "user",
            scripted_tool_batch_body(&[
                (
                    "model-first",
                    "parallel_read",
                    serde_json::json!({"name":"first","delay_ms":80}),
                ),
                (
                    "model-failure",
                    "parallel_read",
                    serde_json::json!({"name":"failure","fail":true}),
                ),
                (
                    "model-third",
                    "parallel_read",
                    serde_json::json!({"name":"third","delay_ms":10}),
                ),
            ]),
        ))
        .expect("parallel batch response");
    responses
        .send(MatchedScriptedResponse::last_message_role(
            "tool",
            scripted_text_body("parallel batch complete"),
        ))
        .expect("parallel completion response");
    let (fixture, selection) = custom_fixture_with_endpoint_and_primary_agent(
        &endpoint,
        "---\ndescription: Parallel tool test\nmode: primary\nenabled: true\nmodels: [{ model: \"custom.test/group/model\", variant: base }]\npermissions:\n  read: allow\n---\nRun parallel tools.\n",
    );
    let state = Arc::new(ParallelToolState::default());
    let barrier = Arc::new(tokio::sync::Barrier::new(4));
    fixture
        .engine
        .register_tool_provider(Arc::new(TestParallelToolProvider {
            state: Arc::clone(&state),
            barrier: Some(Arc::clone(&barrier)),
        }));
    let session = fixture
        .engine
        .create_session(selection.clone())
        .expect("parallel session");
    let run = fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: session.session_id,
                client_run_id: ClientRunId::new("parallel-order").expect("client run ID"),
                selection,
                input: "run the batch".into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("parallel run")
        .run_id;
    tokio::time::timeout(test_timeout(2), barrier.wait())
        .await
        .expect("all parallel executors reached the barrier");
    wait_for_session_not_running(&fixture.engine, session.session_id).await;

    assert_eq!(state.max_active.load(Ordering::SeqCst), 3);
    let events = fixture
        .engine
        .inner
        .store
        .get(session.session_id)
        .expect("parallel projection")
        .log
        .events();
    let starts = events
        .iter()
        .filter_map(|event| match &event.payload {
            EventPayload::ToolCallStarted { start } if event.run_id == Some(run) => {
                Some(start.owner.model_call_id.as_str())
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(starts, ["model-first", "model-failure", "model-third"]);
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
    assert_eq!(terminations.len(), 3);
    assert_eq!(
        terminations.last().expect("last termination").0,
        "model-first"
    );
    assert!(terminations.iter().any(|(id, outcome)| {
        *id == "model-failure" && *outcome == ToolTerminationOutcome::Failed
    }));
    assert!(terminations.iter().any(|(id, outcome)| {
        *id == "model-third" && *outcome == ToolTerminationOutcome::Completed
    }));
    assert!(
        fixture
            .engine
            .inner
            .store
            .get(session.session_id)
            .expect("completed projection")
            .runs
            .get(&run)
            .expect("parallel run projection")
            .pending_calls
            .is_empty()
    );
    let requests = with_watchdog("captured fixture completion", captured)
        .await
        .expect("parallel server");
    assert!(requests[1].contains("first completed"));
    assert!(requests[1].contains("failure failed"));
    assert!(requests[1].contains("third completed"));
    let live_history = serde_json::to_vec(
        &fixture
            .engine
            .get_history(session.session_id, EngineHistoryView::Assembled)
            .await
            .expect("live parallel history"),
    )
    .expect("serialize live parallel history");
    fixture.engine.shutdown().await;
    let reopened = reopen_engine(&fixture);
    let replayed_history = serde_json::to_vec(
        &reopened
            .get_history(session.session_id, EngineHistoryView::Assembled)
            .await
            .expect("replayed parallel history"),
    )
    .expect("serialize replayed parallel history");
    assert_eq!(replayed_history, live_history);
    assert!(
        reopened
            .inner
            .store
            .get(session.session_id)
            .expect("replayed parallel projection")
            .runs
            .get(&run)
            .expect("replayed parallel run")
            .pending_calls
            .is_empty()
    );
    reopened.shutdown().await;
}

#[tokio::test]
async fn opt_out_completion_never_allocates_capture_files_or_waits_for_publication() {
    let (endpoint, responses, captured) = scripted_channel_server(2).await;
    responses
        .send(MatchedScriptedResponse::last_message_role(
            "user",
            scripted_tool_batch_body(&[(
                "page-call",
                "parallel_read",
                serde_json::json!({"name":"page"}),
            )]),
        ))
        .unwrap();
    responses
        .send(MatchedScriptedResponse::last_message_role(
            "tool",
            scripted_text_body("done"),
        ))
        .unwrap();
    let (fixture, selection) = custom_fixture_with_endpoint_and_primary_agent(
        &endpoint,
        "---\ndescription: Opt-out capture admission\nmode: primary\nenabled: true\nmodels: [{ model: \"custom.test/group/model\", variant: base }]\npermissions:\n  read: allow\n---\nRead a page.\n",
    );
    fixture
        .engine
        .register_tool_provider(Arc::new(TestOptOutProvider(TestParallelToolProvider {
            state: Arc::new(ParallelToolState::default()),
            barrier: None,
        })));
    let attempted = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let attempts = attempted.clone();
    fixture
        .engine
        .inner
        .artifacts
        .io_test_hook()
        .set(Arc::new(move |operation, _| {
            if operation.starts_with("capture_") {
                attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                return Err(std::io::Error::other("capture storage is unavailable"));
            }
            Ok(())
        }));
    let publication = fixture
        .engine
        .inner
        .artifacts
        .publication()
        .write_owned()
        .await;
    let session = fixture.engine.create_session(selection.clone()).unwrap();
    fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: session.session_id,
                client_run_id: ClientRunId::new("no-capture-page").unwrap(),
                selection,
                input: "read the page".into(),
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
    let terminal = events
        .iter()
        .find_map(|event| match &event.payload {
            EventPayload::ToolCallTerminated { termination } => Some(termination),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        terminal.outcome,
        ToolTerminationOutcome::Completed,
        "{:?}",
        terminal.error
    );
    let result = terminal.result.as_ref().unwrap();
    assert_eq!(result.output, "page completed");
    assert_eq!(result.display.as_deref(), Some("page completed"));
    assert!(result.retained_output.is_none());
    assert!(result.truncation.is_none());
    assert_eq!(attempted.load(std::sync::atomic::Ordering::SeqCst), 0);
    assert_eq!(
        with_watchdog("captured fixture completion", captured)
            .await
            .unwrap()
            .len(),
        2
    );
    drop(publication);
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn cancellation_after_successful_finalization_is_reconciled_at_terminal_commit() {
    let (endpoint, responses, captured) = scripted_channel_server(1).await;
    responses
        .send(MatchedScriptedResponse::last_message_role(
            "user",
            scripted_tool_batch_body(&[(
                "race-call",
                "parallel_read",
                serde_json::json!({"name":"race"}),
            )]),
        ))
        .unwrap();
    let (mut fixture, selection) = custom_fixture_with_endpoint_and_primary_agent(
        &endpoint,
        "---\ndescription: Cancellation commit race\nmode: primary\nenabled: true\nmodels: [{ model: \"custom.test/group/model\", variant: base }]\npermissions:\n  read: allow\n---\nTest terminal commit cancellation.\n",
    );
    let markers = tempfile::tempdir().unwrap();
    let reached = markers.path().join("after-result.jsonl");
    let release = markers.path().join("release");
    let mut plugin = interception_plugin(
        "commit_gate",
        &[
            (
                "FIXTURE_CAPABILITIES",
                serde_json::json!({
                    "producer_messaging":false,"tools":false,"resources":false,
                    "subscribe_events":false,"subscribe_bus":false,"publish_bus":false,
                    "publish_session_events":false,"intercept":["tool_after_result"]
                })
                .to_string(),
            ),
            ("FIXTURE_INTERCEPT_FILE", reached.display().to_string()),
            (
                "FIXTURE_INTERCEPT_RELEASE_FILE",
                release.display().to_string(),
            ),
        ],
    );
    plugin.interception_timeout_ms = 30_000;
    reopen_with_interception_plugins(&mut fixture, vec![("commit_gate".into(), plugin)]).await;
    fixture
        .engine
        .register_tool_provider(Arc::new(TestParallelToolProvider {
            state: Arc::new(ParallelToolState::default()),
            barrier: None,
        }));
    let session = fixture.engine.create_session(selection.clone()).unwrap();
    let run = fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: session.session_id,
                client_run_id: ClientRunId::new("commit-race").unwrap(),
                selection: selection.clone(),
                input: "finish then cancel".into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .unwrap()
        .run_id;

    // The hook is reached only after successful capture finalization and the caller's
    // cancellation snapshot. Keep it blocked until the actor has processed cancellation.
    let hook = tokio::time::timeout(test_timeout(5), async {
        loop {
            if let Some(value) = fs::read_to_string(&reached)
                .ok()
                .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
            {
                break value;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("after-result hook reached");
    assert_eq!(hook["params"]["result_content"], "race completed");
    assert_eq!(hook["params"]["is_error"], false);
    fixture.engine.cancel_run(run).await.unwrap();
    fs::write(&release, b"release").unwrap();
    wait_for_session_not_running(&fixture.engine, session.session_id).await;
    wait_for_run_inactive(&fixture.engine, run).await;

    let events = fixture
        .engine
        .inner
        .store
        .get(session.session_id)
        .unwrap()
        .log
        .events();
    let terminations = events
        .iter()
        .filter_map(|event| match &event.payload {
            EventPayload::ToolCallTerminated { termination } if event.run_id == Some(run) => {
                Some(termination)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(terminations.len(), 1);
    let terminal = terminations[0];
    assert_eq!(terminal.outcome, ToolTerminationOutcome::Cancelled);
    assert!(terminal.error.is_some());
    let result = terminal.result.as_ref().unwrap();
    assert_eq!(result.output, "race completed");
    assert_eq!(
        result.display.as_deref(),
        Some("Tool cancelled; retained output is incomplete.")
    );
    let retained = result.retained_output.as_ref().unwrap();
    assert!(retained.incomplete);
    assert_eq!(
        retained.streams[0].sha256,
        Sha256Digest::of_bytes(b"race completed")
    );
    assert_eq!(
        fixture
            .engine
            .read_artifact(
                session.session_id,
                &format!("artifact://{}", retained.streams[0].sha256),
                0,
                10
            )
            .unwrap()
            .content,
        "race completed"
    );
    let policy = frozen_root_policy(&fixture, &selection);
    let context = crate::model_history::assemble_model_context(
        &events,
        &fixture.engine.inner.artifacts,
        policy.selected_suffix.first().unwrap(),
        &policy.agent.composed_prompt,
    )
    .unwrap();
    let results = context
        .history
        .iter()
        .filter_map(|turn| match turn {
            oven_sdk::HistoryTurn::Tool(message) => Some(&message.results),
            _ => None,
        })
        .flatten()
        .collect::<Vec<_>>();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].tool_call_id, "race-call");
    assert!(results[0].is_error);
    assert!(
        matches!(&results[0].content, oven_sdk::ToolContent::Mixed(parts)
        if parts.iter().any(|part| matches!(part, oven_sdk::ContentValue::Text(text) if text == "race completed")))
    );
    let history = serde_json::to_string(&context.history).unwrap();
    assert!(
        !history.contains("retained output is incomplete"),
        "display must remain UI-only"
    );
    assert_eq!(
        with_watchdog("captured fixture completion", captured)
            .await
            .unwrap()
            .len(),
        1
    );
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn plugin_named_output_contract_reaches_capture_manifest_and_model_history() {
    let (endpoint, responses, captured) = scripted_channel_server(2).await;
    responses
        .send(MatchedScriptedResponse::last_message_role(
            "user",
            scripted_tool_batch_body(&[(
                "plugin-named-call",
                "fixture_named",
                serde_json::json!({}),
            )]),
        ))
        .unwrap();
    responses
        .send(MatchedScriptedResponse::last_message_role(
            "tool",
            scripted_text_body("done"),
        ))
        .unwrap();
    let (mut fixture, selection) = custom_fixture_with_endpoint_and_primary_agent(
        &endpoint,
        "---\ndescription: Plugin named output\nmode: primary\nenabled: true\nmodels: [{ model: \"custom.test/group/model\", variant: base }]\npermissions:\n  plugin: allow\n---\nUse the named plugin output.\n",
    );
    let full = "r".repeat(70_000);
    let plugin = interception_plugin(
        "named_plugin",
        &[
            (
                "FIXTURE_CAPABILITIES",
                serde_json::json!({
                    "producer_messaging":false,"tools":true,"resources":false,
                    "subscribe_events":false,"subscribe_bus":false,"publish_bus":false,
                    "publish_session_events":false,"intercept":[]
                })
                .to_string(),
            ),
            (
                "FIXTURE_TOOLS",
                serde_json::json!([{
                    "name":"fixture_named","description":"Named output fixture",
                    "parameters":{"type":"object","additionalProperties":false},
                    "permission_name":"named_output","primary_resource_param":null,
                    "output":{"kind":"named","streams":["results","diagnostics","empty"]}
                }])
                .to_string(),
            ),
            (
                "FIXTURE_TOOL_RESULT",
                serde_json::json!({
                    "output":{"kind":"named","streams":[
                        {"stream":"empty","text":""},
                        {"stream":"diagnostics","text":"warning\n"},
                        {"stream":"results","text":full}
                    ]},
                    "display":"PLUGIN_UI_ONLY","is_error":false
                })
                .to_string(),
            ),
        ],
    );
    reopen_with_interception_plugins(&mut fixture, vec![("named_plugin".into(), plugin)]).await;
    let session = fixture.engine.create_session(selection.clone()).unwrap();
    fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: session.session_id,
                client_run_id: ClientRunId::new("plugin-named-output").unwrap(),
                selection,
                input: "produce named output".into(),
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
    let terminal = events
        .iter()
        .find_map(|event| match &event.payload {
            EventPayload::ToolCallTerminated { termination } => Some(termination),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        terminal.outcome,
        ToolTerminationOutcome::Completed,
        "{:?}",
        terminal.error
    );
    let result = terminal.result.as_ref().unwrap();
    assert_eq!(result.display.as_deref(), Some("PLUGIN_UI_ONLY"));
    let retained = result.retained_output.as_ref().unwrap();
    assert!(!retained.incomplete);
    assert_eq!(
        retained
            .streams
            .iter()
            .map(|stream| stream.name.as_deref().unwrap())
            .collect::<Vec<_>>(),
        ["results", "diagnostics", "empty"]
    );
    assert!(retained.streams[0].truncated);
    assert_eq!(retained.streams[0].next_offset, Some(0));
    assert!(!retained.streams[1].truncated);
    assert_eq!(retained.streams[2].byte_length, 0);
    let manifest_id = retained
        .reference
        .uri
        .strip_prefix("artifact://sha256/")
        .unwrap();
    for (name, expected) in [
        ("results", full.as_str()),
        ("diagnostics", "warning\n"),
        ("empty", ""),
    ] {
        assert_eq!(
            fixture
                .engine
                .read_artifact(
                    session.session_id,
                    &format!("artifact://{manifest_id}/{name}"),
                    0,
                    1
                )
                .unwrap()
                .content,
            expected
        );
    }
    let requests = with_watchdog("captured fixture completion", captured)
        .await
        .unwrap();
    assert!(requests[1].contains(&format!("artifact://{manifest_id}/results")));
    assert!(requests[1].contains("[diagnostics]"));
    assert!(!requests[1].contains("PLUGIN_UI_ONLY"));
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn cancelling_parallel_tools_terminates_every_started_call_once() {
    let (endpoint, responses, captured) = scripted_channel_server(1).await;
    responses
        .send(MatchedScriptedResponse::last_message_role(
            "user",
            scripted_tool_batch_body(&[
                (
                    "cancel-one",
                    "parallel_read",
                    serde_json::json!({"name":"one","wait_for_cancellation":true}),
                ),
                (
                    "cancel-two",
                    "parallel_read",
                    serde_json::json!({"name":"two","wait_for_cancellation":true}),
                ),
                (
                    "cancel-three",
                    "parallel_read",
                    serde_json::json!({"name":"three","wait_for_cancellation":true}),
                ),
            ]),
        ))
        .expect("cancellation batch response");
    let (fixture, selection) = custom_fixture_with_endpoint_and_primary_agent(
        &endpoint,
        "---\ndescription: Parallel cancellation test\nmode: primary\nenabled: true\nmodels: [{ model: \"custom.test/group/model\", variant: base }]\npermissions:\n  read: allow\n---\nCancel parallel tools.\n",
    );
    let state = Arc::new(ParallelToolState::default());
    let barrier = Arc::new(tokio::sync::Barrier::new(4));
    fixture
        .engine
        .register_tool_provider(Arc::new(TestParallelToolProvider {
            state: Arc::clone(&state),
            barrier: Some(Arc::clone(&barrier)),
        }));
    let session = fixture
        .engine
        .create_session(selection.clone())
        .expect("cancellation session");
    let run = fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: session.session_id,
                client_run_id: ClientRunId::new("parallel-cancellation").expect("client run ID"),
                selection: selection.clone(),
                input: "start cancellable tools".into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("cancellable run")
        .run_id;
    tokio::time::timeout(test_timeout(2), barrier.wait())
        .await
        .expect("all cancellable executors started");
    fixture.engine.cancel_run(run).await.expect("cancel run");
    wait_for_session_not_running(&fixture.engine, session.session_id).await;
    wait_for_run_inactive(&fixture.engine, run).await;

    let events = fixture
        .engine
        .inner
        .store
        .get(session.session_id)
        .expect("cancelled projection")
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
    let run_cancelled = events
        .iter()
        .filter(|event| {
            event.run_id == Some(run) && matches!(event.payload, EventPayload::RunCancelled { .. })
        })
        .count();
    assert_eq!((starts, terminations, run_cancelled), (3, 3, 1));
    let policy = frozen_root_policy(&fixture, &selection);
    let context = crate::model_history::assemble_model_context(
        &events,
        &fixture.engine.inner.artifacts,
        policy.selected_suffix.first().unwrap(),
        &policy.agent.composed_prompt,
    )
    .unwrap();
    let model_results = context
        .history
        .iter()
        .filter_map(|turn| match turn {
            oven_sdk::HistoryTurn::Tool(message) => Some(&message.results),
            _ => None,
        })
        .flatten()
        .collect::<Vec<_>>();
    assert_eq!(model_results.len(), 3);
    for terminal in events.iter().filter_map(|event| match &event.payload {
        EventPayload::ToolCallTerminated { termination } if event.run_id == Some(run) => {
            Some(termination)
        }
        _ => None,
    }) {
        assert_eq!(terminal.outcome, ToolTerminationOutcome::Cancelled);
        let result = terminal.result.as_ref().expect("retained cancelled output");
        assert!(result.retained_output.as_ref().unwrap().incomplete);
        assert_eq!(
            result.display.as_deref(),
            Some("Tool cancelled; retained output is incomplete.")
        );
        let model_result = model_results
            .iter()
            .find(|result| result.tool_call_id == terminal.owner.model_call_id.as_str())
            .unwrap();
        assert!(model_result.is_error);
        assert!(
            matches!(&model_result.content, oven_sdk::ToolContent::Mixed(parts)
            if parts.iter().any(|part| matches!(part, oven_sdk::ContentValue::Text(text) if text == &result.output)))
        );
    }
    assert_eq!(
        with_watchdog("captured fixture completion", captured)
            .await
            .expect("cancellation server")
            .len(),
        1
    );
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn same_file_write_and_edit_serialize_while_distinct_files_overlap() {
    for (suffix, keys, expected_max) in [
        ("same", ["same.txt", "same.txt"], 1),
        ("distinct", ["one.txt", "two.txt"], 2),
    ] {
        let delay_ms = if expected_max == 1 { 60 } else { 0 };
        let barrier = (expected_max == 2).then(|| Arc::new(tokio::sync::Barrier::new(3)));
        let (endpoint, responses, captured) = scripted_channel_server(2).await;
        responses
            .send(MatchedScriptedResponse::last_message_role(
                "user",
                scripted_tool_batch_body(&[
                    (
                        "write-call",
                        "parallel_write",
                        serde_json::json!({"name":"write-target","delay_ms":delay_ms,"serialization_key":keys[0]}),
                    ),
                    (
                        "edit-call",
                        "parallel_edit",
                        serde_json::json!({"name":"edit-target","delay_ms":delay_ms,"serialization_key":keys[1]}),
                    ),
                ]),
            ))
            .expect("mutation batch response");
        responses
            .send(MatchedScriptedResponse::last_message_role(
                "tool",
                scripted_text_body("mutations complete"),
            ))
            .expect("mutation completion response");
        let (fixture, selection) = custom_fixture_with_endpoint_and_primary_agent(
            &endpoint,
            "---\ndescription: Mutation serialization test\nmode: primary\nenabled: true\nmodels: [{ model: \"custom.test/group/model\", variant: base }]\npermissions:\n  write: allow\n---\nRun mutations.\n",
        );
        let state = Arc::new(ParallelToolState::default());
        fixture
            .engine
            .register_tool_provider(Arc::new(TestParallelToolProvider {
                state: Arc::clone(&state),
                barrier: barrier.clone(),
            }));
        let session = fixture
            .engine
            .create_session(selection.clone())
            .expect("mutation session");
        fixture
            .engine
            .start_run(
                RunStartParams {
                    reset_fallback: false,
                    session_id: session.session_id,
                    client_run_id: ClientRunId::new(format!("mutation-{suffix}"))
                        .expect("client run ID"),
                    selection,
                    input: "mutate files".into(),
                },
                cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
            )
            .await
            .expect("mutation run");
        if let Some(barrier) = barrier {
            with_watchdog("distinct-file executors overlap", barrier.wait()).await;
        }
        wait_for_session_not_running(&fixture.engine, session.session_id).await;
        assert_eq!(state.max_active.load(Ordering::SeqCst), expected_max);
        assert_eq!(
            with_watchdog("captured fixture completion", captured)
                .await
                .expect("mutation server")
                .len(),
            2
        );
        fixture.engine.shutdown().await;
    }
}

#[tokio::test]
async fn same_key_parallel_calls_prepare_as_batch_and_execute_in_call_order() {
    let (endpoint, responses, captured) = scripted_channel_server(2).await;
    responses
        .send(MatchedScriptedResponse::last_message_role(
            "user",
            scripted_tool_batch_body(&[
                (
                    "first-call",
                    "parallel_write",
                    serde_json::json!({"name":"first","delay_ms":60,"serialization_key":"same.txt"}),
                ),
                (
                    "second-call",
                    "parallel_write",
                    serde_json::json!({"name":"second","delay_ms":0,"serialization_key":"same.txt"}),
                ),
            ]),
        ))
        .expect("batch response");
    responses
        .send(MatchedScriptedResponse::last_message_role(
            "tool",
            scripted_text_body("batch complete"),
        ))
        .expect("completion response");
    let (fixture, selection) = custom_fixture_with_endpoint_and_primary_agent(
        &endpoint,
        "---\ndescription: Batch ordering test\nmode: primary\nenabled: true\nmodels: [{ model: \"custom.test/group/model\", variant: base }]\npermissions:\n  write: allow\n---\nRun ordered batch.\n",
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
        .expect("batch session");
    fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: session.session_id,
                client_run_id: ClientRunId::new("batch-ordering").expect("client run ID"),
                selection,
                input: "run ordered batch".into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("batch run");
    wait_for_session_not_running(&fixture.engine, session.session_id).await;

    // One batch preparation per provider, in model call order.
    assert_eq!(
        state
            .prepare_batches
            .lock()
            .expect("prepare batches lock")
            .as_slice(),
        &[vec!["first".to_owned(), "second".to_owned()]]
    );
    // Same-key calls execute sequentially in call order: without grouping the
    // zero-delay second call would finish first.
    assert_eq!(
        state
            .completed_names
            .lock()
            .expect("completed names lock")
            .as_slice(),
        &["first".to_owned(), "second".to_owned()]
    );
    assert_eq!(state.max_active.load(Ordering::SeqCst), 1);
    assert_eq!(
        with_watchdog("captured fixture completion", captured)
            .await
            .expect("batch server")
            .len(),
        2
    );
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn named_output_streams_publish_readable_manifests_without_display_leaking_to_model() {
    for failed in [false, true] {
        let call = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"named-call\",\"type\":\"function\",\"function\":{\"name\":\"named_output\",\"arguments\":\"{}\"}}]},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n";
        let done = "data: {\"choices\":[{\"delta\":{\"content\":\"done\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n";
        let (endpoint, requests, _, _) =
            scripted_server_with_delayed_response(vec![call.into(), done.into()], usize::MAX).await;
        let (fixture, selection) = custom_fixture_with_endpoint_and_primary_agent(
            &endpoint,
            "---\ndescription: Named output\nmode: primary\nenabled: true\nmodels: [{ model: \"custom.test/group/model\", variant: base }]\npermissions:\n  read: allow\n---\nUse named output.\n",
        );
        fixture
            .engine
            .register_tool_provider(Arc::new(NamedOutputProvider { failed }));
        let session = fixture.engine.create_session(selection.clone()).unwrap();
        fixture
            .engine
            .start_run(
                RunStartParams {
                    reset_fallback: false,
                    session_id: session.session_id,
                    client_run_id: ClientRunId::new("named-output").unwrap(),
                    selection,
                    input: "produce output".into(),
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
        let termination = events
            .iter()
            .find_map(|event| match &event.payload {
                EventPayload::ToolCallTerminated { termination } => Some(termination),
                _ => None,
            })
            .unwrap();
        assert_eq!(
            termination.outcome,
            if failed {
                ToolTerminationOutcome::Failed
            } else {
                ToolTerminationOutcome::Completed
            },
            "{:?}",
            termination.error
        );
        let result = termination.result.as_ref().unwrap();
        assert_eq!(result.display.as_deref(), Some("UI_FINAL_ONLY"));
        let retained = result.retained_output.as_ref().unwrap();
        assert_eq!(retained.incomplete, failed);
        assert_eq!(
            retained
                .streams
                .iter()
                .map(|stream| stream.name.as_deref().unwrap())
                .collect::<Vec<_>>(),
            ["results", "diagnostics", "empty"]
        );
        let manifest = retained
            .reference
            .uri
            .strip_prefix("artifact://sha256/")
            .unwrap();
        assert_eq!(
            fixture
                .engine
                .read_artifact(
                    session.session_id,
                    &format!("artifact://{manifest}/results"),
                    0,
                    1
                )
                .unwrap()
                .content,
            "x".repeat(128 * 1024)
        );
        assert_eq!(
            fixture
                .engine
                .read_artifact(
                    session.session_id,
                    &format!("artifact://{manifest}/diagnostics"),
                    0,
                    1
                )
                .unwrap()
                .content,
            "diagnostic\n"
        );
        assert!(
            fixture
                .engine
                .read_artifact(
                    session.session_id,
                    &format!("artifact://{manifest}/empty"),
                    0,
                    1
                )
                .unwrap()
                .content
                .is_empty()
        );
        assert!(
            result
                .output
                .contains(&format!("artifact://{manifest}/results"))
        );
        assert!(
            !result
                .output
                .contains(&format!("artifact://{manifest}/diagnostics"))
        );
        assert_eq!(
            fixture
                .engine
                .tool_output_streams(termination.tool_call_id)
                .unwrap()
                .iter()
                .map(|stream| stream.name())
                .collect::<Vec<_>>(),
            ["results", "diagnostics", "empty"]
        );
        let requests = requests.await.unwrap();
        assert!(requests[1].contains("[results]"));
        assert!(requests[1].contains("[diagnostics]"));
        assert!(!requests[1].contains("UI_LIVE_ONLY"));
        assert!(!requests[1].contains("UI_FINAL_ONLY"));
        fixture.engine.shutdown().await;
    }
}

#[tokio::test]
async fn bash_internal_timeout_commits_all_chunks_before_terminal_event() {
    let (fixture, session_id, _run_id, call_id, _stdin_received, _cleanup_progress_sent, captured) =
        start_streaming_bash_test_run("timeout", false).await;
    await_event(
        &fixture.engine,
        session_id,
        "bash timeout terminal event",
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
        .get(session_id)
        .expect("final timeout projection")
        .log
        .events();
    let terminal = events
        .iter()
        .find(|event| {
            matches!(
                &event.payload,
                EventPayload::ToolCallTerminated { termination }
                    if termination.tool_call_id == call_id
            )
        })
        .expect("timeout termination");
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
        [
            "stdout before internal timeout",
            "stderr before internal timeout"
        ]
    );
    assert!(chunks.iter().all(|(seq, _)| *seq < terminal.seq));
    let EventPayload::ToolCallTerminated { termination } = &terminal.payload else {
        unreachable!()
    };
    assert!(
        termination
            .error
            .as_ref()
            .is_some_and(|error| error.message.as_str() == "bash timed out")
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
async fn tool_before_hooks_run_only_after_permission_and_approval() {
    let capabilities_marker = tempfile::tempdir().expect("hook markers");

    let (denied_endpoint, _) = scripted_zero_resource_tool_server().await;
    let (mut denied, denied_selection) = denied_approval_fixture_with_endpoint(&denied_endpoint);
    let denied_file = capabilities_marker.path().join("denied.jsonl");
    reopen_with_interception_plugins(
        &mut denied,
        vec![(
            "hook".into(),
            interception_plugin(
                "hook",
                &[("FIXTURE_INTERCEPT_FILE", denied_file.display().to_string())],
            ),
        )],
    )
    .await;
    denied
        .engine
        .register_tool_provider(Arc::new(TestWriteProvider {
            executed: Arc::new(TestFlag::default()),
        }));
    let denied_session = denied
        .engine
        .create_session(denied_selection.clone())
        .expect("denied session");
    denied
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: denied_session.session_id,
                client_run_id: ClientRunId::new("denied-hook").expect("run ID"),
                selection: denied_selection,
                input: "try denied write".into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("denied run");
    wait_for_session_not_running(&denied.engine, denied_session.session_id).await;
    assert!(!denied_file.exists(), "denied call reached plugin hook");
    denied.engine.shutdown().await;

    for (decision, marker_name) in [(true, "approved"), (false, "rejected")] {
        let (endpoint, _) = scripted_zero_resource_tool_server().await;
        let (mut fixture, selection) = approval_fixture_with_endpoint(&endpoint);
        let marker = capabilities_marker
            .path()
            .join(format!("{marker_name}.jsonl"));
        reopen_with_interception_plugins(
            &mut fixture,
            vec![(
                "hook".into(),
                interception_plugin(
                    "hook",
                    &[("FIXTURE_INTERCEPT_FILE", marker.display().to_string())],
                ),
            )],
        )
        .await;
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
                    client_run_id: ClientRunId::new(format!("{marker_name}-hook")).expect("run ID"),
                    selection,
                    input: "try approved write".into(),
                },
                cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
            )
            .await
            .expect("approval run");
        let approval = wait_for_escalated_approval(&fixture.engine, session.session_id).await;
        assert!(!marker.exists(), "hook ran before approval decision");
        if decision {
            approve_once(&fixture.engine, &approval, "approve-hook").await;
            wait_for_tool_execution(&fixture.engine, session.session_id, &executed).await;
            assert!(marker.exists(), "approved call did not reach hook");
        } else {
            reject_approval(&fixture.engine, &approval, "reject-hook").await;
            wait_for_session_not_running(&fixture.engine, session.session_id).await;
            assert!(!marker.exists(), "rejected approval reached hook");
        }
        fixture.engine.shutdown().await;
    }
}

#[tokio::test]
async fn validated_tool_modification_reprepares_before_the_next_hook() {
    let (endpoint, _) = scripted_zero_resource_tool_server().await;
    let (mut fixture, selection) = custom_fixture_with_endpoint_and_primary_agent(
        &endpoint,
        "---\ndescription: Hook chain test\nmode: primary\nenabled: true\nmodels: [{ model: \"custom.test/group/model\", variant: base }]\npermissions:\n  write: allow\n---\nTest hook chaining.\n",
    );
    let marker = tempfile::tempdir().expect("hook marker");
    let alpha_file = marker.path().join("alpha.jsonl");
    reopen_with_interception_plugins(
        &mut fixture,
        vec![
            (
                "zeta".into(),
                interception_plugin(
                    "zeta",
                    &[(
                        "FIXTURE_TOOL_BEFORE_RESULT",
                        r#"{"action":"allow","modified_arguments":{"value":"zeta"}}"#.into(),
                    )],
                ),
            ),
            (
                "alpha".into(),
                interception_plugin(
                    "alpha",
                    &[("FIXTURE_INTERCEPT_FILE", alpha_file.display().to_string())],
                ),
            ),
        ],
    )
    .await;
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
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: session.session_id,
                client_run_id: ClientRunId::new("validated-hook-chain").expect("run ID"),
                selection,
                input: "run write".into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("run");
    wait_for_tool_execution(&fixture.engine, session.session_id, &executed).await;
    let alpha: serde_json::Value = serde_json::from_str(
        fs::read_to_string(alpha_file)
            .expect("alpha hook")
            .lines()
            .next()
            .expect("alpha hook line"),
    )
    .expect("alpha hook JSON");
    assert_eq!(alpha["params"]["arguments"]["value"], "zeta");
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn registered_external_tool_must_declare_resource_and_cannot_bypass_deny() {
    let (endpoint, captured) = scripted_zero_resource_tool_server().await;
    let (fixture, selection) = custom_fixture_with_endpoint_and_primary_agent(
        &endpoint,
        "---\ndescription: Resource-bound test agent\nmode: primary\nenabled: true\nmodels: [{ model: \"custom.test/group/model\", variant: base }]\npermissions:\n  write: deny\n---\nReject denied tools.\n",
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
        .expect("zero-resource session");
    fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: session.session_id,
                client_run_id: ClientRunId::new("zero-resource-run").expect("run ID"),
                selection,
                input: "attempt the write tool".into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("accepted run");

    await_projection(
        &fixture.engine,
        session.session_id,
        "zero-resource run completion",
        |projection| projection.status == SessionStatus::Completed,
    )
    .await;

    assert!(!executed.is_set());
    let events = fixture
        .engine
        .inner
        .store
        .get(session.session_id)
        .expect("completed resource-bound projection")
        .log
        .events();
    assert!(events.iter().any(|event| matches!(
        &event.payload,
        EventPayload::ToolCallTerminated { termination }
            if termination.outcome == ToolTerminationOutcome::Failed
                && termination.error.as_ref().is_some_and(|error| {
                    error.code.as_str() == "execution_failed"
                })
    )));
    assert_eq!(
        with_watchdog("captured fixture completion", captured)
            .await
            .expect("resource-bound server")
            .len(),
        2
    );
    fixture.engine.shutdown().await;
}
