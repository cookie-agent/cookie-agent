use std::sync::Arc;

use cookie_agent_protocol::{ClientRunId, EventPayload, RunStartParams, Sha256Digest};

use crate::EngineHistoryView;

use super::support::*;

#[tokio::test]
async fn native_compaction_commits_window_and_failure_falls_back_to_summary() {
    for fail_native in [false, true] {
        let (endpoint, captured) = native_compaction_server(fail_native).await;
        let (fixture, selection) = managed_openai_compaction_fixture(&endpoint);
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
                    client_run_id: ClientRunId::new(if fail_native {
                        "native-fallback"
                    } else {
                        "native-success"
                    })
                    .unwrap(),
                    selection,
                    input: "compact this context".into(),
                },
                cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
            )
            .await
            .expect("run");
        wait_for_session_not_running(&fixture.engine, session.session_id).await;
        assert!(
            fixture
                .engine
                .compact_session(
                    session.session_id,
                    Some("preserve focus"),
                    cookie_agent_protocol::EventOrigin::new("client:rpc").unwrap()
                )
                .await
                .expect("compaction")
        );
        let events = fixture
            .engine
            .inner
            .store
            .get(session.session_id)
            .expect("projection")
            .log
            .events();
        let commit = events.iter().find_map(|event| match &event.payload {
            EventPayload::ContextCheckpointCommitted { commit } => Some(commit),
            _ => None,
        });
        let compaction_events = events
            .iter()
            .filter(|event| {
                matches!(
                    event.payload,
                    EventPayload::ContextCheckpointCommitted { .. }
                        | EventPayload::ToolOutputElided { .. }
                )
            })
            .collect::<Vec<_>>();
        assert!(!compaction_events.is_empty());
        assert!(compaction_events.iter().all(|event| {
            event
                .origin
                .as_ref()
                .is_some_and(|origin| origin.as_str() == "client:rpc")
        }));
        let assembled = fixture
            .engine
            .get_history(session.session_id, EngineHistoryView::Assembled)
            .await
            .expect("assembled checkpoint history");
        let full = fixture
            .engine
            .get_history(session.session_id, EngineHistoryView::Full)
            .await
            .expect("full checkpoint history");
        let assembled = serde_json::to_string(&assembled).expect("serialize assembled history");
        let full = serde_json::to_string(&full).expect("serialize full history");
        assert_ne!(assembled, full);
        assert!(full.contains("compact this context"));
        assert!(!assembled.contains("compact this context"));
        assert!(
            events
                .iter()
                .all(|event| !matches!(event.payload, EventPayload::ContextRehydrated { .. }))
        );
        let commit = commit.expect("compaction checkpoint");
        if fail_native {
            let Some(cookie_agent_protocol::ContextCheckpoint::InternalSummary { checkpoint }) =
                Some(&commit.checkpoint)
            else {
                panic!("native failure must commit the harness checkpoint");
            };
            assert_eq!(checkpoint.summary(), "fallback summary");
            assert!(assembled.contains("fallback summary"));
        } else {
            assert_eq!(commit.boundaries.recent_from_seq, None);
            assert_eq!(commit.budgets.keep_recent_tokens, 0);
            assert!(matches!(
                Some(&commit.checkpoint),
                Some(cookie_agent_protocol::ContextCheckpoint::NativeWindow { .. })
            ));
        }
        let requests = with_watchdog("captured fixture completion", captured)
            .await
            .expect("captured requests");
        assert!(requests[1].starts_with("POST /v1/responses/compact "));
        assert!(requests[1].contains("compact this context"));
        if fail_native {
            assert!(requests[2].starts_with("POST /v1/responses "));
        }
        fixture.engine.shutdown().await;
    }
}

#[tokio::test]
async fn compaction_uses_raw_context_when_it_fits_and_prunes_retry_without_persisting_elision() {
    const RAW_MARKER: &str = "RAW_COMPACTION_TOOL_OUTPUT";
    const FULL_OUTPUT_MARKER: &str = "FULL_OUTPUT_BEYOND_TRUNCATED_PREVIEW";
    const SMALL_MARKER: &str = "SMALL_RECENT_TOOL_OUTPUT";
    const RETRIEVED_MARKER: &str = "SMALL_RECENT_RETRIEVED_OUTPUT";
    const EMITTED_MARKER: &str = "TOOL_EMITTED_CONTENT_TO_REMOVE";
    let root_body = "data: {\"choices\":[{\"delta\":{\"content\":\"initial complete\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n";
    let summary_body = "data: {\"choices\":[{\"delta\":{\"content\":\"compacted summary\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n";

    let context_error = (400, r#"{"error":{"message":"maximum context length exceeded","type":"invalid_request_error","code":"context_length_exceeded"}}"#.to_owned());
    let other_error = (400, r#"{"error":{"message":"unrelated invalid request","type":"invalid_request_error","code":"invalid_request"}}"#.to_owned());
    let success = (200, summary_body.to_owned());
    let empty = (200, summary_body.replace("compacted summary", ""));
    let oversized = (
        200,
        summary_body.replace("compacted summary", &"x".repeat(2_000)),
    );
    let non_text = (200, "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call\",\"type\":\"function\",\"function\":{\"name\":\"bash\",\"arguments\":\"{}\"}}]},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n".to_owned());

    for (context_tokens, responses, expect_elision, expect_checkpoint) in [
        (100_000, vec![success.clone()], false, true),
        (4_096, vec![success.clone()], false, true),
        (100_000, vec![context_error.clone(), success], true, true),
        (100_000, vec![other_error.clone()], false, false),
        (100_000, vec![empty], false, false),
        (100_000, vec![oversized], false, false),
        (100_000, vec![non_text], false, false),
        (
            100_000,
            vec![context_error.clone(), other_error],
            true,
            false,
        ),
        (
            100_000,
            vec![context_error.clone(), context_error],
            true,
            false,
        ),
    ] {
        let provider_retry = responses.len() == 2;
        let mut bodies = vec![(200, root_body.to_owned())];
        bodies.extend(responses);
        let (endpoint, captured, _reached, _release) =
            scripted_server_with_status_and_delay(bodies, usize::MAX).await;
        let (mut fixture, selection) =
            custom_fixture_with_endpoint_primary_internal_concurrency_and_context(
                &endpoint,
                "---\ndescription: Raw-first compaction test\nmode: primary\nenabled: true\nmodels: [{ model: \"custom.test/group/model\", variant: base }]\npermissions:\n  read: allow\n---\nTest raw-first compaction.\n",
                Some((
                    "compaction.md",
                    "---\ndescription: Test compaction\nmode: internal\nenabled: true\nmodels: [{ model: \"${parent_model}\" }]\nlimits: { timeout_ms: 30000, max_output_tokens: 256 }\npermissions: {}\n---\nSummarize faithfully.\n",
                )),
                None,
                false,
                None,
                None,
                context_tokens,
                None,
            );
        fixture.engine.shutdown().await;
        fixture.config.runtime.context_compaction.keep_recent_tokens =
            if provider_retry { 1_000 } else { 0 };
        fixture.engine = reopen_engine(&fixture);
        // Publish at least one tool so the summarizer tool-carrying behavior
        // (first trial keeps definitions, pruned retry drops them) is
        // observable in the captured requests.
        fixture
            .engine
            .register_tool_provider(Arc::new(TestParallelToolProvider {
                state: Arc::new(ParallelToolState::default()),
                barrier: None,
            }));
        let session = fixture
            .engine
            .create_session(selection.clone())
            .expect("compaction session");
        let run = fixture
            .engine
            .start_run(
                RunStartParams {
                    reset_fallback: false,
                    session_id: session.session_id,
                    client_run_id: ClientRunId::new(format!("raw-first-{expect_elision}"))
                        .expect("run ID"),
                    selection: selection.clone(),
                    input: "prepare compaction history".into(),
                },
                cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
            )
            .await
            .expect("start compaction run");
        wait_for_session_not_running(&fixture.engine, session.session_id).await;
        let mut owner_policy = frozen_root_policy(&fixture, &selection);
        let binding = owner_policy.selected_suffix.first().expect("binding");
        owner_policy.agent.composed_prompt = fixture
            .engine
            .inner
            .store
            .get(session.session_id)
            .expect("compaction session")
            .runs
            .get(&run.run_id)
            .expect("admitted compaction run")
            .agent
            .composed_prompt
            .clone();
        let output = format!("{RAW_MARKER}{}", "x".repeat(80 * 1024 - RAW_MARKER.len()));
        let full_output = format!("{output}\n{FULL_OUTPUT_MARKER}\n");
        let (full_output_reference, _) = fixture
            .engine
            .inner
            .artifacts
            .retain(session.session_id, full_output.as_bytes())
            .expect("retain full output behind truncated preview");
        let image_bytes = vec![7_u8; 1024 * 1024];
        let (image_reference, image_digest) = fixture
            .engine
            .inner
            .artifacts
            .retain(session.session_id, &image_bytes)
            .expect("retain compaction image");
        let image_attachment = cookie_agent_protocol::ToolAttachment {
            mime_type: cookie_agent_protocol::MimeType::new("image/png").unwrap(),
            filename: Some("context.png".into()),
            byte_length: image_bytes.len() as u64,
            sha256: Sha256Digest::new(image_digest).unwrap(),
            reference: image_reference,
        };
        append_compaction_tool_history(
            &fixture,
            session.session_id,
            run.run_id,
            binding,
            cookie_agent_protocol::PersistedToolResult {
                display: None,
                retained_output: None,
                title: cookie_agent_protocol::SafeDisplayText::new("Historical output").unwrap(),
                output,
                metadata: serde_json::Value::Null,
                truncation: Some(cookie_agent_protocol::ToolOutputTruncation {
                    original_bytes: full_output.len() as u64,
                    original_lines: 2,
                    retained: full_output_reference,
                }),
                attachments: vec![image_attachment.clone()],
                additional_messages: Vec::new(),
            },
            20_000,
        );
        for (tool, output) in [
            (
                (
                    "read",
                    serde_json::json!({"filePath": format!("artifact://{}", "a".repeat(64))}),
                ),
                format!("LARGE_RETRIEVED_OUTPUT{}", "r".repeat(9 * 1024)),
            ),
            (
                (
                    "read",
                    serde_json::json!({"filePath": "/tmp/ordinary-file.txt"}),
                ),
                SMALL_MARKER.to_owned(),
            ),
            (
                (
                    "read",
                    serde_json::json!({"filePath": format!("artifact://{}/results", "b".repeat(64))}),
                ),
                RETRIEVED_MARKER.to_owned(),
            ),
        ] {
            append_named_compaction_tool_history(
                &fixture,
                session.session_id,
                run.run_id,
                binding,
                cookie_agent_protocol::PersistedToolResult {
                    display: None,
                    retained_output: None,
                    title: cookie_agent_protocol::SafeDisplayText::new("Recent output").unwrap(),
                    output,
                    metadata: serde_json::Value::Null,
                    truncation: None,
                    attachments: vec![image_attachment.clone()],
                    additional_messages: [
                        cookie_agent_protocol::ToolEmittedMessageRole::User,
                        cookie_agent_protocol::ToolEmittedMessageRole::System,
                    ]
                    .into_iter()
                    .map(|role| {
                        cookie_agent_protocol::ToolEmittedMessage::new(
                            role,
                            vec![
                                cookie_agent_protocol::ToolEmittedContent::Text(
                                    EMITTED_MARKER.into(),
                                ),
                                cookie_agent_protocol::ToolEmittedContent::File(
                                    image_attachment.clone(),
                                ),
                            ],
                        )
                        .unwrap()
                    })
                    .collect(),
                },
                tool,
                None,
            );
        }
        let before = fixture
            .engine
            .inner
            .store
            .get(session.session_id)
            .unwrap()
            .log
            .events();

        assert_eq!(
            fixture
                .engine
                .compact_session(
                    session.session_id,
                    None,
                    cookie_agent_protocol::EventOrigin::new("client:test").unwrap()
                )
                .await
                .expect("manual compaction"),
            expect_checkpoint
        );
        let events = fixture
            .engine
            .inner
            .store
            .get(session.session_id)
            .expect("compacted projection")
            .log
            .events();
        assert!(
            !events
                .iter()
                .any(|event| matches!(event.payload, EventPayload::ToolOutputElided { .. }))
        );
        assert_eq!(
            &events[..before.len()],
            before.as_slice(),
            "compaction must preserve every original event, including tool output and emitted content"
        );
        let requests = with_watchdog("captured fixture completion", captured)
            .await
            .expect("captured compaction requests");
        assert_eq!(requests.len(), if provider_retry { 3 } else { 2 });
        if provider_retry {
            for marker in [
                RAW_MARKER,
                SMALL_MARKER,
                RETRIEVED_MARKER,
                EMITTED_MARKER,
                "LARGE_RETRIEVED_OUTPUT",
            ] {
                assert!(
                    requests[1].contains(marker),
                    "raw first trial lost {marker}"
                );
            }
            assert!(!requests[1].contains("[tool output elided; retained at "));
        }
        let summary_request = requests.last().expect("summarizer request");
        // The first summarizer trial extends the conversation as-is, tools
        // included, for prefix-cache affinity; only the pruned retry drops
        // the tool definitions.
        let first_trial = if provider_retry {
            &requests[1]
        } else {
            summary_request
        };
        assert!(
            first_trial.contains("\"tools\":[{"),
            "first summarizer trial must keep session tool definitions"
        );
        if provider_retry {
            assert!(
                !summary_request.contains("\"tools\":[{"),
                "pruned summarizer retry must drop session tool definitions"
            );
        }
        for marker in [
            RAW_MARKER,
            SMALL_MARKER,
            RETRIEVED_MARKER,
            EMITTED_MARKER,
            "LARGE_RETRIEVED_OUTPUT",
        ] {
            assert_eq!(
                summary_request.contains(marker),
                !expect_elision,
                "unexpected pruning for {marker}"
            );
        }
        assert_eq!(
            summary_request.contains("[artifact read output omitted for compaction]"),
            expect_elision
        );
        assert_eq!(
            summary_request.contains("[tool output elided; retained at "),
            expect_elision
        );
        assert_eq!(
            summary_request.contains("\u{27e6}elided media attachment: image/png\u{27e7}"),
            !expect_elision
        );
        assert!(!summary_request.contains("input_image"));
        assert!(!summary_request.contains("cookie_agent.compaction.tool_call_id"));
        assert!(!summary_request.contains(FULL_OUTPUT_MARKER));
        if expect_elision {
            let body = request_body(summary_request);
            let messages = body["messages"].as_array().unwrap();
            let results = messages
                .iter()
                .filter(|message| message["role"] == "tool")
                .collect::<Vec<_>>();
            assert_eq!(results.len(), 4);
            assert_eq!(
                messages
                    .iter()
                    .filter_map(|message| message["tool_calls"].as_array())
                    .map(Vec::len)
                    .sum::<usize>(),
                4
            );
            for (result, marker) in [(results[0], RAW_MARKER), (results[2], SMALL_MARKER)] {
                let text = result["content"].as_str().unwrap();
                // Read the retry artifact using only the marker, without a session or UUID.
                let hint: serde_json::Value =
                    serde_json::from_str(text.split_once('\n').unwrap().1)
                        .expect("structured retrieval hint");
                assert_eq!(hint["read_more"]["tool"], "read");
                assert!(hint["read_more"]["arguments"].get("tool_call_id").is_none());
                let artifact_path = hint["read_more"]["arguments"]["filePath"].as_str().unwrap();
                assert!(cookie_agent_protocol::ArtifactReadPath::parse(artifact_path).is_ok());
                assert!(text.contains(&format!("retained at {artifact_path};")));
                let page = fixture
                    .engine
                    .read_artifact(session.session_id, artifact_path, 0, 10)
                    .expect("public readback using only the marker hint");
                assert_eq!(page.source, "artifact");
                assert!(text.contains("serialized tool content (JSON)"));
                let oven_sdk::ToolContent::Mixed(values) =
                    serde_json::from_str(&page.content).unwrap()
                else {
                    panic!("retry artifact preserves serialized tool content");
                };
                assert!(
                    matches!(&values[0], oven_sdk::ContentValue::Text(text) if text.contains(marker))
                );
                if marker == RAW_MARKER {
                    let oven_sdk::ContentValue::Json(metadata) = &values[1] else {
                        panic!("serialized result metadata");
                    };
                    let original_path =
                        metadata["truncation"]["read_more"]["arguments"]["filePath"]
                            .as_str()
                            .unwrap();
                    assert!(cookie_agent_protocol::ArtifactReadPath::parse(original_path).is_ok());
                    let full_page = fixture
                        .engine
                        .read_artifact(session.session_id, original_path, 0, 10)
                        .unwrap();
                    assert_eq!(full_page.content, full_output);
                    assert_ne!(original_path, artifact_path);
                }
            }
            for result in [results[1], results[3]] {
                assert_eq!(
                    result["content"],
                    "[artifact read output omitted for compaction]"
                );
            }
            if expect_checkpoint {
                let retained = fixture
                    .engine
                    .get_history(session.session_id, EngineHistoryView::Assembled)
                    .await
                    .unwrap();
                assert!(
                    !serde_json::to_string(&retained)
                        .unwrap()
                        .contains(RAW_MARKER)
                );
            }
        }
        let commit = events.iter().find_map(|event| match &event.payload {
            EventPayload::ContextCheckpointCommitted { commit } => Some(commit),
            _ => None,
        });
        assert_eq!(commit.is_some(), expect_checkpoint);
        let Some(commit) = commit else {
            fixture.engine.shutdown().await;
            continue;
        };
        assert_eq!(
            commit.budgets.keep_recent_tokens,
            if provider_retry { 1_000 } else { 0 }
        );
        assert_eq!(commit.boundaries.recent_from_seq.is_some(), provider_retry);
        if provider_retry {
            let retained = fixture
                .engine
                .get_history(session.session_id, EngineHistoryView::Assembled)
                .await
                .unwrap();
            let retained = serde_json::to_string(&retained).unwrap();
            assert!(retained.contains(RETRIEVED_MARKER));
            assert!(retained.contains(EMITTED_MARKER));
            assert!(!retained.contains("[artifact read output omitted for compaction]"));
        }

        let input_events = events
            .iter()
            .filter(|event| event.seq <= commit.boundaries.input_through_seq)
            .cloned()
            .collect::<Vec<_>>();
        let context = crate::model_history::assemble_model_context(
            &input_events,
            &fixture.engine.inner.artifacts,
            binding,
            &owner_policy.agent.composed_prompt,
        )
        .expect("selected compaction context");
        let tools = fixture
            .engine
            .tool_definitions(session.session_id, &owner_policy)
            .expect("session tool definitions");
        assert!(!tools.is_empty(), "fixture must publish tools");
        let serialized_bytes =
            crate::runtime::compaction::serialized_fit_request_bytes(&context.history, &tools)
                .expect("measure canonical owner request");
        assert_eq!(
            commit.budgets.input_tokens_before,
            (serialized_bytes as u64).div_ceil(4)
        );

        fixture.engine.shutdown().await;
    }
}

#[tokio::test]
async fn automatic_compaction_failure_limit_suppresses_after_three_and_resets() {
    let root = scripted_text_usage_body("root", 8_192, Some(1), 0);
    let failure = (400, r#"{"error":{"message":"invalid request","type":"invalid_request_error","code":"invalid_request"}}"#.to_owned());
    let summary = scripted_text_usage_body("checkpoint", 1, Some(1), 0);
    let mut bodies = vec![(200, root.clone())];
    for _ in 0..3 {
        bodies.push(failure.clone());
        bodies.push((200, root.clone()));
    }
    bodies.push((200, root.clone()));
    bodies.push((200, summary));
    bodies.push(failure);
    bodies.push((200, root));
    let (endpoint, captured, ..) = scripted_server_with_status_and_delay(bodies, usize::MAX).await;
    let (mut fixture, selection) =
        custom_fixture_with_endpoint_primary_internal_concurrency_and_context(
            &endpoint,
            "---\ndescription: auto compaction\nmode: primary\nenabled: true\nmodels: [{ model: \"custom.test/group/model\", variant: base }]\npermissions: {}\n---\nTest auto compaction.\n",
            Some((
                "compaction.md",
                "---\ndescription: compaction\nmode: internal\nenabled: true\nmodels: [{ model: \"${parent_model}\" }]\nlimits: { timeout_ms: 30000, max_output_tokens: 256 }\npermissions: {}\n---\nSummarize.\n",
            )),
            Some(0),
            false,
            None,
            None,
            8_192,
            None,
        );
    fixture.engine.shutdown().await;
    fixture.engine = reopen_engine(&fixture);
    let session = fixture.engine.create_session(selection.clone()).unwrap();
    for index in 0..7 {
        fixture
            .engine
            .start_run(
                RunStartParams {
                    reset_fallback: false,
                    session_id: session.session_id,
                    client_run_id: ClientRunId::new(format!("auto-{index}")).unwrap(),
                    selection: selection.clone(),
                    input: "trigger".into(),
                },
                cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
            )
            .await
            .unwrap();
        wait_for_session_not_running(&fixture.engine, session.session_id).await;
    }
    let requests = with_watchdog("captured fixture completion", captured)
        .await
        .unwrap();
    let events = fixture
        .engine
        .inner
        .store
        .get(session.session_id)
        .unwrap()
        .log
        .events();
    assert_eq!(events.iter().filter(|event| matches!(event.payload,
        EventPayload::PluginDiagnostic { ref message, .. } if message.contains("disabled after 3 consecutive failures")
    )).count(), 1);
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(
                event.payload,
                EventPayload::ContextCheckpointCommitted { .. }
            ))
            .count(),
        1
    );
    let summary_requests = requests
        .iter()
        .filter(|request| request.contains("Summarize."))
        .count();
    assert_eq!(summary_requests, 5);
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn auto_compaction_commits_checkpoint_before_the_attempt_that_uses_it() {
    // Attempt 1 calls the write tool reporting 7000 prompt tokens against an
    // 8192 context — over the default 70% trigger (5734). Attempt 2 must
    // compact first. The checkpoint has to precede that attempt's start and
    // request: consumers anchor a turn's transcript item to
    // `ModelAttemptStarted`, so emitting the attempt before compaction renders
    // post-compaction output above the compaction marker.
    let tool_turn = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"write-call\",\"type\":\"function\",\"function\":{\"name\":\"write\",\"arguments\":\"{}\"}}]},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}],\"usage\":{\"prompt_tokens\":7000,\"completion_tokens\":10,\"total_tokens\":7010}}\n\n".to_owned();
    let summary = "data: {\"choices\":[{\"delta\":{\"content\":\"checkpoint summary\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n".to_owned();
    let after = "data: {\"choices\":[{\"delta\":{\"content\":\"after compaction\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":900,\"completion_tokens\":5,\"total_tokens\":905}}\n\n".to_owned();
    let (endpoint, _captured, ..) = scripted_server_with_status_and_delay(
        vec![(200, tool_turn), (200, summary), (200, after)],
        usize::MAX,
    )
    .await;
    let (fixture, selection) =
        custom_fixture_with_endpoint_primary_internal_concurrency_and_context(
            &endpoint,
            "---\ndescription: compaction ordering\nmode: primary\nenabled: true\nmodels: [{ model: \"custom.test/group/model\", variant: base }]\npermissions:\n  write: allow\n---\nTest compaction ordering.\n",
            None,
            None,
            false,
            None,
            None,
            8_192,
            None,
        );
    let executed = Arc::new(TestFlag::default());
    fixture
        .engine
        .register_tool_provider(Arc::new(TestWriteProvider {
            executed: Arc::clone(&executed),
        }));
    let session = fixture.engine.create_session(selection.clone()).unwrap();
    fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: session.session_id,
                client_run_id: ClientRunId::new("compaction-ordering").unwrap(),
                selection: selection.clone(),
                input: format!(
                    "trigger compaction ordering {}",
                    "context padding ".repeat(1500)
                ),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .unwrap();
    wait_for_session_not_running(&fixture.engine, session.session_id).await;
    assert!(executed.is_set(), "write tool executed");
    let events = fixture
        .engine
        .inner
        .store
        .get(session.session_id)
        .unwrap()
        .log
        .events();
    let checkpoint_seq = events
        .iter()
        .find_map(|event| match event.payload {
            EventPayload::ContextCheckpointCommitted { .. } => Some(event.seq),
            _ => None,
        })
        .expect("auto-compaction checkpoint");
    let attempt_starts = events
        .iter()
        .filter(|event| matches!(event.payload, EventPayload::ModelAttemptStarted { .. }))
        .map(|event| event.seq)
        .collect::<Vec<_>>();
    assert_eq!(attempt_starts.len(), 2, "tool turn then compacted turn");
    assert!(
        attempt_starts[0] < checkpoint_seq,
        "pre-compaction attempt starts before the checkpoint"
    );
    assert!(
        attempt_starts[1] > checkpoint_seq,
        "post-compaction attempt starts after the checkpoint"
    );
    let second_attempt = events
        .iter()
        .filter_map(|event| match &event.payload {
            EventPayload::ModelAttemptStarted { attempt_id, .. } => Some((*attempt_id, event.seq)),
            _ => None,
        })
        .nth(1)
        .map(|(attempt_id, _)| attempt_id)
        .expect("second attempt");
    assert!(
        events.iter().any(|event| {
            matches!(
                &event.payload,
                EventPayload::ModelRequestPrepared { attempt_id, .. }
                    if *attempt_id == second_attempt && event.seq > checkpoint_seq
            )
        }),
        "post-compaction request prepared after the checkpoint"
    );
    let committed = events
        .iter()
        .find_map(|event| match &event.payload {
            EventPayload::ModelTurnCommitted {
                attempt_id,
                input_through_seq,
                ..
            } if *attempt_id == second_attempt => Some(*input_through_seq),
            _ => None,
        })
        .expect("post-compaction turn committed");
    assert!(
        committed >= checkpoint_seq,
        "post-compaction turn consumes the checkpoint"
    );
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn summary_compaction_retains_recent_tail_across_new_input_and_repeat_compaction() {
    const OLD_USER: &str = "OLD_PREFIX_USER";
    const OLD_ASSISTANT: &str = "OLD_PREFIX_ASSISTANT";
    const RECENT_USER: &str = "ORIGINAL_RECENT_USER";
    const RECENT_ASSISTANT: &str = "ORIGINAL_RECENT_ASSISTANT";
    const NEW_USER: &str = "NEW_POST_CHECKPOINT_USER";
    const NEW_ASSISTANT: &str = "NEW_POST_CHECKPOINT_ASSISTANT";

    let response = |text: String| {
        format!(
            "data: {{\"choices\":[{{\"delta\":{{\"content\":{}}},\"finish_reason\":null}}]}}\n\ndata: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"stop\"}}]}}\n\n",
            serde_json::to_string(&text).expect("response text")
        )
    };
    let oversized_old_assistant = format!("{OLD_ASSISTANT} {}", "old ".repeat(600));
    let (endpoint, captured, _reached, _release) = scripted_server_with_delayed_response(
        vec![
            response(oversized_old_assistant),
            response(RECENT_ASSISTANT.into()),
            response("first checkpoint summary".into()),
            response(NEW_ASSISTANT.into()),
            response("second checkpoint summary".into()),
        ],
        usize::MAX,
    )
    .await;
    let (mut fixture, selection) =
        custom_fixture_with_endpoint_primary_internal_concurrency_and_context(
            &endpoint,
            "---\ndescription: Recent-tail compaction test\nmode: primary\nenabled: true\nmodels: [{ model: \"custom.test/group/model\", variant: base }]\npermissions: {}\n---\nTest recent-tail compaction.\n",
            None,
            None,
            false,
            None,
            None,
            8_192,
            None,
        );
    fixture.engine.shutdown().await;
    fixture.config.runtime.context_compaction.max_summary_bytes = 256;
    fixture.config.runtime.context_compaction.keep_recent_tokens = 300;
    fixture.engine = reopen_engine(&fixture);

    let session = fixture.engine.create_session(selection.clone()).unwrap();
    for (client_run_id, input) in [("old-prefix", OLD_USER), ("recent-tail", RECENT_USER)] {
        fixture
            .engine
            .start_run(
                RunStartParams {
                    reset_fallback: false,
                    session_id: session.session_id,
                    client_run_id: ClientRunId::new(client_run_id).unwrap(),
                    selection: selection.clone(),
                    input: input.into(),
                },
                cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
            )
            .await
            .unwrap();
        wait_for_session_not_running(&fixture.engine, session.session_id).await;
    }

    let mut owner_policy = frozen_root_policy(&fixture, &selection);
    let binding = owner_policy.selected_suffix.first().unwrap();
    owner_policy.agent.composed_prompt = fixture
        .engine
        .inner
        .store
        .get(session.session_id)
        .unwrap()
        .runs
        .values()
        .find(|run| run.client_run_id.as_str() == "recent-tail")
        .expect("admitted recent-tail run")
        .agent
        .composed_prompt
        .clone();
    let before_events = fixture
        .engine
        .inner
        .store
        .get(session.session_id)
        .unwrap()
        .log
        .events();
    let canonical_before = crate::model_history::assemble_model_context(
        &before_events,
        &fixture.engine.inner.artifacts,
        binding,
        &owner_policy.agent.composed_prompt,
    )
    .unwrap();
    let expected_before =
        crate::runtime::compaction::serialized_fit_request_bytes(&canonical_before.history, &[])
            .unwrap()
            .div_ceil(4) as u64;

    assert!(
        fixture
            .engine
            .compact_session(
                session.session_id,
                None,
                cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
            )
            .await
            .unwrap()
    );
    let first_events = fixture
        .engine
        .inner
        .store
        .get(session.session_id)
        .unwrap()
        .log
        .events();
    let first_commit = first_events
        .iter()
        .find_map(|event| match &event.payload {
            EventPayload::ContextCheckpointCommitted { commit } => Some(commit),
            _ => None,
        })
        .expect("first checkpoint");
    assert_eq!(first_commit.budgets.input_tokens_before, expected_before);
    assert_eq!(first_commit.budgets.keep_recent_tokens, 300);
    assert!(first_commit.boundaries.recent_from_seq.is_some());
    assert!(
        first_events
            .iter()
            .all(|event| !matches!(event.payload, EventPayload::ContextRehydrated { .. }))
    );

    let assembled = fixture
        .engine
        .get_history(session.session_id, EngineHistoryView::Assembled)
        .await
        .unwrap();
    let assembled = serde_json::to_string(&assembled).unwrap();
    assert!(assembled.contains("first checkpoint summary"));
    assert!(assembled.contains(RECENT_USER));
    assert!(assembled.contains(RECENT_ASSISTANT));
    assert!(!assembled.contains(OLD_USER));
    assert!(!assembled.contains(OLD_ASSISTANT));

    let projected_after = crate::model_history::assemble_model_context(
        &first_events,
        &fixture.engine.inner.artifacts,
        binding,
        &owner_policy.agent.composed_prompt,
    )
    .unwrap();
    let expected_after =
        crate::runtime::compaction::serialized_fit_request_bytes(&projected_after.history, &[])
            .unwrap()
            .div_ceil(4) as u64;
    assert_eq!(first_commit.budgets.input_tokens_after, expected_after);

    fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: session.session_id,
                client_run_id: ClientRunId::new("new-after-checkpoint").unwrap(),
                selection: selection.clone(),
                input: NEW_USER.into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .unwrap();
    wait_for_session_not_running(&fixture.engine, session.session_id).await;
    assert!(
        fixture
            .engine
            .compact_session(
                session.session_id,
                None,
                cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
            )
            .await
            .unwrap()
    );

    let final_events = fixture
        .engine
        .inner
        .store
        .get(session.session_id)
        .unwrap()
        .log
        .events();
    assert_eq!(
        final_events
            .iter()
            .filter(|event| matches!(
                event.payload,
                EventPayload::ContextCheckpointCommitted { .. }
            ))
            .count(),
        2
    );
    assert!(
        final_events
            .iter()
            .all(|event| !matches!(event.payload, EventPayload::ContextRehydrated { .. }))
    );
    let assembled = fixture
        .engine
        .get_history(session.session_id, EngineHistoryView::Assembled)
        .await
        .unwrap();
    let assembled = serde_json::to_string(&assembled).unwrap();
    for marker in ["second checkpoint summary", NEW_USER, NEW_ASSISTANT] {
        assert!(
            assembled.contains(marker),
            "missing retained marker {marker}"
        );
    }
    for marker in [OLD_USER, OLD_ASSISTANT, RECENT_USER, RECENT_ASSISTANT] {
        assert!(
            !assembled.contains(marker),
            "unexpected compacted marker {marker}"
        );
    }

    let requests = with_watchdog("captured fixture completion", captured)
        .await
        .unwrap();
    assert_eq!(requests.len(), 5);
    let first_summary_request = &requests[2];
    assert!(first_summary_request.contains(OLD_USER));
    assert!(first_summary_request.contains(OLD_ASSISTANT));
    assert!(first_summary_request.contains(RECENT_USER));
    assert!(first_summary_request.contains(RECENT_ASSISTANT));
    let owner_replay_request = &requests[3];
    assert!(owner_replay_request.contains("first checkpoint summary"));
    assert!(owner_replay_request.contains(RECENT_USER));
    assert!(owner_replay_request.contains(RECENT_ASSISTANT));
    assert!(!owner_replay_request.contains(OLD_USER));
    assert!(!owner_replay_request.contains(OLD_ASSISTANT));
    let second_summary_request = &requests[4];
    assert!(second_summary_request.contains("first checkpoint summary"));
    assert!(second_summary_request.contains(RECENT_USER));
    assert!(second_summary_request.contains(RECENT_ASSISTANT));
    assert!(second_summary_request.contains(NEW_USER));
    assert!(second_summary_request.contains(NEW_ASSISTANT));
    assert!(!second_summary_request.contains(OLD_USER));
    assert!(!second_summary_request.contains(OLD_ASSISTANT));
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn compact_cancellation_reason_reaches_the_engine_result() {
    let (endpoint, captured) = scripted_model_server().await;
    let (mut fixture, selection) = custom_fixture_with_endpoint(&endpoint);
    let capabilities = r#"{"producer_messaging":false,"tools":false,"resources":false,"subscribe_events":false,"subscribe_bus":false,"publish_bus":false,"publish_session_events":false,"intercept":["session_before_compact"]}"#;
    reopen_with_interception_plugins(
        &mut fixture,
        vec![(
            "compact".into(),
            interception_plugin(
                "compact",
                &[
                    ("FIXTURE_CAPABILITIES", capabilities.into()),
                    (
                        "FIXTURE_COMPACT_BEFORE_RESULT",
                        r#"{"cancel":true,"reason":"keep this context"}"#.into(),
                    ),
                ],
            ),
        )],
    )
    .await;
    let session = fixture.engine.create_session(selection.clone()).unwrap();
    fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: session.session_id,
                client_run_id: ClientRunId::new("compact-cancel-reason").unwrap(),
                selection,
                input: "complete before compacting".into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .unwrap();
    wait_for_session_not_running(&fixture.engine, session.session_id).await;
    with_watchdog("captured fixture completion", captured)
        .await
        .unwrap();
    let result = fixture
        .engine
        .compact_session_result(
            session.session_id,
            None,
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .unwrap();
    assert!(!result.compacted);
    assert_eq!(
        result.cancellation_reason.as_deref(),
        Some("keep this context")
    );
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn compaction_full_history_preserves_eligible_reasoning_and_recent_tail() {
    const OLD_PREFIX: &str = "UNSIGNED_REPLAY_OLD_PREFIX";
    const RECENT_TAIL: &str = "UNSIGNED_REPLAY_RECENT_TAIL";

    let (endpoint, captured) = anthropic_replay_server(vec![
        AnthropicReplayResponse::Thinking(None),
        AnthropicReplayResponse::TextWithInputTokens("recovered answer", 5_000),
        AnthropicReplayResponse::Text("checkpoint summary"),
    ])
    .await;
    let primary = "---\ndescription: Replay compaction regression\nmode: primary\nenabled: true\nmodels: [{ model: \"custom.test/group/model\", variant: base }]\npermissions: {}\n---\nTest replay compaction.\n";
    let capabilities =
        ANTHROPIC_REPLAY_CAPABILITIES.replace("context_tokens = 4096", "context_tokens = 8192");
    let (mut fixture, selection) = custom_fixture_with_capabilities(
        &endpoint,
        primary,
        None,
        None,
        false,
        None,
        None,
        8_192,
        None,
        "anthropic-compatible",
        Some(&capabilities),
    );
    fixture.engine.shutdown().await;
    fixture.config.runtime.context_compaction.max_summary_bytes = 256;
    fixture.config.runtime.context_compaction.keep_recent_tokens = 300;
    fixture.engine = reopen_engine(&fixture);
    let session = fixture.engine.create_session(selection.clone()).unwrap();

    fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: session.session_id,
                client_run_id: ClientRunId::new("unsigned-compaction-seed").unwrap(),
                selection: selection.clone(),
                input: format!("{OLD_PREFIX} {}", "old context ".repeat(1_500)),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .unwrap();
    wait_for_session_not_running(&fixture.engine, session.session_id).await;
    run_replay_test_turn(&fixture, session.session_id, &selection, RECENT_TAIL).await;

    let before = fixture
        .engine
        .inner
        .store
        .get(session.session_id)
        .unwrap()
        .log
        .events();
    let recent_seq = before
        .iter()
        .find_map(|event| match &event.payload {
            EventPayload::UserInputSubmitted { input } if input == RECENT_TAIL => Some(event.seq),
            _ => None,
        })
        .expect("recent-tail user event");
    let replayed_seq = before
        .iter()
        .find_map(|event| match &event.payload {
            EventPayload::ModelReplayEvaluated {
                ordered_decisions, ..
            } if ordered_decisions.iter().any(|decision| {
                matches!(
                    &decision.disposition,
                    cookie_agent_protocol::ReplayDisposition::Replayed
                )
            }) =>
            {
                Some(event.seq)
            }
            _ => None,
        })
        .expect("native replay metadata");
    assert!(recent_seq < replayed_seq);
    assert_eq!(rejected_unsigned_replay_recovery_count(&before), 0);
    assert!(
        crate::model_history::compaction_tail_candidates(&before).contains(&recent_seq),
        "subsequent user must be an eligible recent-tail boundary"
    );

    assert!(
        fixture
            .engine
            .compact_session(
                session.session_id,
                None,
                cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
            )
            .await
            .expect("manual compaction")
    );
    let events = fixture
        .engine
        .inner
        .store
        .get(session.session_id)
        .unwrap()
        .log
        .events();
    let commit = events
        .iter()
        .find_map(|event| match &event.payload {
            EventPayload::ContextCheckpointCommitted { commit } => Some(commit),
            _ => None,
        })
        .expect("compaction checkpoint");
    assert_eq!(
        commit.boundaries.recent_from_seq,
        Some(recent_seq),
        "unexpected compaction commit: {commit:?}"
    );

    let requests = with_watchdog("captured fixture completion", captured)
        .await
        .expect("unsigned replay compaction requests");
    assert_eq!(requests.len(), 3);
    let rejected_artifact = request_body(&requests[1])["messages"][1]["content"][0].clone();
    assert_eq!(
        rejected_artifact,
        serde_json::json!({"type":"thinking","thinking":"reason","signature":""})
    );
    let summary_request = requests.last().expect("internal summary request");
    let summary_body = request_body(summary_request);
    assert!(
        summary_body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|message| message["content"].as_array())
            .flatten()
            .any(|block| block == &rejected_artifact),
        "internal summary request lost eligible native reasoning"
    );
    assert!(summary_request.contains(OLD_PREFIX));
    assert!(summary_request.contains(RECENT_TAIL));
    assert!(anthropic_request_has_unsigned_thinking(summary_request));
    fixture.engine.shutdown().await;
}
