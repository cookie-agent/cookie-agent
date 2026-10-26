use cookie_agent_protocol::{EventPayload, VariantId};

use super::support::*;

#[tokio::test]
async fn no_auth_openai_chat_automatic_replay_persists_and_replays_after_restart() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
    let captured = tokio::spawn(async move {
        let mut requests = Vec::new();
        for _ in 0..2 {
            let (mut socket, _) = listener.accept().await.unwrap();
            requests
                .push(String::from_utf8(read_scripted_http_request(&mut socket).await).unwrap());
            write_scripted_sse(&mut socket, "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"answer\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n").await;
        }
        requests
    });
    let primary = "---\ndescription: No-auth replay test\nmode: primary\nenabled: true\nmodels: [{ model: \"custom.test/group/model\", variant: base }]\npermissions: {}\n---\nTest replay.\n";
    let capabilities = "input = [\"text\"]\noutput = [\"text\"]\ncontext_tokens = 4096\noutput_tokens = 1024\ntool_calling = true\nparallel_tool_calls = true\nstructured_output = false\nreasoning = false\ntemperature = true\ntop_p = true\nseed = false\nmedia = {}";
    let (mut fixture, selection) = custom_fixture_with_capabilities_and_variants(
        &endpoint,
        primary,
        None,
        None,
        false,
        None,
        None,
        4096,
        None,
        "openai-chat",
        Some(capabilities),
        Some("model_id = \"wire-noauth\""),
    );
    let session = fixture.engine.create_session(selection.clone()).unwrap();
    run_replay_test_turn(&fixture, session.session_id, &selection, "first").await;
    fixture.engine.shutdown().await;
    fixture.engine = reopen_engine(&fixture);
    run_replay_test_turn(&fixture, session.session_id, &selection, "second").await;
    let events = fixture
        .engine
        .inner
        .store
        .get(session.session_id)
        .unwrap()
        .log
        .events();
    let turns = events
        .iter()
        .filter_map(|event| match &event.payload {
            EventPayload::ModelTurnCommitted { turn, .. } => Some(turn),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(turns.len(), 2);
    for turn in turns {
        let artifact = turn.native_replay.as_ref().unwrap();
        assert_eq!(artifact.adapter_id().as_str(), "oven.openai.chat");
        assert_eq!(
            artifact.payload()["format"],
            "oven.openai.chat.assistant.v1"
        );
        assert_eq!(
            turn.provider_metadata["cookie_agent.replay_source_wire_model_id"],
            "wire-noauth"
        );
    }
    assert!(events.iter().any(|event| matches!(&event.payload, EventPayload::ModelReplayEvaluated { ordered_decisions, .. } if ordered_decisions.iter().any(|decision| matches!(decision.disposition, cookie_agent_protocol::ReplayDisposition::Replayed)))));
    let requests = tokio::time::timeout(std::time::Duration::from_secs(5), captured)
        .await
        .unwrap()
        .unwrap();
    for request in &requests {
        assert!(!request.to_ascii_lowercase().contains("authorization:"));
        assert_eq!(request_body(request)["model"], "wire-noauth");
    }
    assert!(
        request_body(&requests[1])["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|message| message["role"] == "assistant" && message["content"] == "answer")
    );
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn rejected_unsigned_replay_is_not_silently_removed_after_restart_or_variant_switch() {
    let (endpoint, captured) = anthropic_replay_server(vec![
        AnthropicReplayResponse::Thinking(None),
        AnthropicReplayResponse::Status400,
        AnthropicReplayResponse::Text("later run"),
    ])
    .await;
    let primary = "---\ndescription: Replay recovery test\nmode: primary\nenabled: true\nmodels: [{ model: \"custom.test/group/model\", variant: base }]\npermissions: { memory: deny, cron: deny }\n---\nReplay recovery.\n";
    let (mut fixture, selection) = custom_fixture_with_capabilities_and_variants(
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
        Some(ANTHROPIC_REPLAY_CAPABILITIES),
        Some("variants = { recovery = { } }"),
    );
    let session = fixture.engine.create_session(selection.clone()).unwrap();
    for id in ["unsigned-seed", "unsigned-recover"] {
        run_replay_test_turn(&fixture, session.session_id, &selection, id).await;
    }
    fixture.engine.shutdown().await;
    fixture.engine = reopen_engine(&fixture);
    let mut variant_selection = selection.clone();
    variant_selection.model.variant = Some(VariantId::new("recovery").unwrap());
    run_replay_test_turn(
        &fixture,
        session.session_id,
        &variant_selection,
        "unsigned-later-run",
    )
    .await;

    let requests = with_watchdog("captured fixture completion", captured)
        .await
        .expect("unsigned replay requests");
    assert_eq!(
        request_body(&requests[1])["messages"][1]["content"][0],
        serde_json::json!({"type":"thinking","thinking":"reason","signature":""})
    );
    assert_eq!(requests.len(), 3);
    assert!(anthropic_request_has_unsigned_thinking(&requests[2]));
    let events = fixture
        .engine
        .inner
        .store
        .get(session.session_id)
        .unwrap()
        .log
        .events();
    assert_eq!(rejected_unsigned_replay_recovery_count(&events), 0);
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn signed_anthropic_replay_never_triggers_degradation() {
    let (endpoint, captured) = anthropic_replay_server(vec![
        AnthropicReplayResponse::Thinking(Some("signed")),
        AnthropicReplayResponse::Status400,
    ])
    .await;
    let primary = "---\ndescription: Signed replay test\nmode: primary\nenabled: true\nmodels: [{ model: \"custom.test/group/model\", variant: base }]\npermissions: { memory: deny, cron: deny }\n---\nSigned replay.\n";
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
        Some(ANTHROPIC_REPLAY_CAPABILITIES),
    );
    let session = fixture.engine.create_session(selection.clone()).unwrap();
    run_replay_test_turn(&fixture, session.session_id, &selection, "signed-seed").await;
    run_replay_test_turn(&fixture, session.session_id, &selection, "signed-reject").await;

    let requests = with_watchdog("captured fixture completion", captured)
        .await
        .expect("signed replay requests");
    assert_eq!(requests.len(), 2);
    assert_eq!(
        request_body(&requests[1])["messages"][1]["content"][0]["signature"],
        "signed"
    );
    let events = fixture
        .engine
        .inner
        .store
        .get(session.session_id)
        .unwrap()
        .log
        .events();
    assert_eq!(rejected_unsigned_replay_recovery_count(&events), 0);
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn unsigned_replay_rejection_uses_normal_fallback_without_reasoning_removal() {
    let (endpoint, captured) = anthropic_replay_server(vec![
        AnthropicReplayResponse::Thinking(None),
        AnthropicReplayResponse::Status400,
        AnthropicReplayResponse::Text("variant fallback"),
    ])
    .await;
    let (fixture, selection) = anthropic_replay_fallback_fixture(&endpoint).await;
    let session = fixture.engine.create_session(selection.clone()).unwrap();
    run_replay_test_turn(&fixture, session.session_id, &selection, "variant-seed").await;
    run_replay_test_turn(&fixture, session.session_id, &selection, "variant-reject").await;

    let requests = with_watchdog("captured fixture completion", captured)
        .await
        .expect("variant replay requests");
    assert_eq!(requests.len(), 3);
    assert!(anthropic_request_has_unsigned_thinking(&requests[1]));
    assert!(anthropic_request_has_unsigned_thinking(&requests[2]));
    let events = fixture
        .engine
        .inner
        .store
        .get(session.session_id)
        .unwrap()
        .log
        .events();
    assert_eq!(rejected_unsigned_replay_recovery_count(&events), 0);
    assert!(events.iter().any(|event| matches!(
        event.payload,
        EventPayload::ModelFallback {
            attempts_on_from: 1,
            ..
        }
    )));
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn later_attempts_of_a_run_write_replay_decisions_against_a_base() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
    let tool_call = |id: &str| {
        format!(
            "data: {{\"choices\":[{{\"delta\":{{\"tool_calls\":[{{\"index\":0,\"id\":\"{id}\",\"type\":\"function\",\"function\":{{\"name\":\"missing\",\"arguments\":\"{{}}\"}}}}]}},\"finish_reason\":null}}]}}\n\ndata: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\ndata: [DONE]\n\n"
        )
    };
    let responses = [
        tool_call("call-1"),
        tool_call("call-2"),
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"answer\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n".to_owned(),
    ];
    tokio::spawn(async move {
        for response in responses {
            let (mut socket, _) = listener.accept().await.unwrap();
            read_scripted_http_request(&mut socket).await;
            write_scripted_sse(&mut socket, &response).await;
        }
    });
    let primary = "---\ndescription: Replay base test\nmode: primary\nenabled: true\nmodels: [{ model: \"custom.test/group/model\", variant: base }]\npermissions: {}\n---\nTest replay.\n";
    let capabilities = "input = [\"text\"]\noutput = [\"text\"]\ncontext_tokens = 4096\noutput_tokens = 1024\ntool_calling = true\nparallel_tool_calls = true\nstructured_output = false\nreasoning = false\ntemperature = true\ntop_p = true\nseed = false\nmedia = {}";
    let (mut fixture, selection) = custom_fixture_with_capabilities_and_variants(
        &endpoint,
        primary,
        None,
        None,
        false,
        None,
        None,
        4096,
        None,
        "openai-chat",
        Some(capabilities),
        None,
    );
    let session = fixture.engine.create_session(selection.clone()).unwrap();
    run_replay_test_turn(&fixture, session.session_id, &selection, "only").await;

    // Reopening re-validates the whole log, base references included.
    fixture.engine.shutdown().await;
    fixture.engine = reopen_engine(&fixture);
    let projection = fixture.engine.inner.store.get(session.session_id).unwrap();
    assert!(projection.metadata().skipped_events.is_empty());
    let evaluations = projection
        .log
        .events()
        .into_iter()
        .filter_map(|event| match event.payload {
            EventPayload::ModelReplayEvaluated {
                attempt_id,
                ordered_decisions,
                base_attempt_id,
                ..
            } => Some((attempt_id, ordered_decisions, base_attempt_id)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(evaluations.len(), 3, "one evaluation per attempt");
    let (_, first, first_base) = &evaluations[0];
    assert!(first.is_empty() && first_base.is_none());
    let (second_attempt, second, second_base) = &evaluations[1];
    assert!(!second.is_empty(), "the tool-call turn is evaluated");
    assert!(second_base.is_none(), "an empty list is never a base");
    let (_, third, third_base) = &evaluations[2];
    assert_eq!(third_base.as_ref(), Some(second_attempt));
    assert!(!third.is_empty());
    assert!(
        third
            .iter()
            .all(|decision| decision.history_index > second.last().unwrap().history_index),
        "only decisions after the base's are written: {third:?}"
    );
}

/// Whether an Anthropic request replays any thinking block.
fn anthropic_request_has_thinking(request: &str) -> bool {
    request_body(request)["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|message| message["content"].as_array())
        .flatten()
        .any(|block| block["type"] == "thinking" || block["type"] == "redacted_thinking")
}

fn signed_replay_primary() -> &'static str {
    "---\ndescription: Binding test\nmode: primary\nenabled: true\nmodels: [{ model: \"custom.test/group/model\", variant: base }]\npermissions: { memory: deny, cron: deny }\n---\nBinding.\n"
}

fn binding_fixture(endpoint: &str) -> (Fixture, cookie_agent_protocol::RunSelection) {
    custom_fixture_with_capabilities(
        endpoint,
        signed_replay_primary(),
        None,
        None,
        false,
        None,
        None,
        4_096,
        None,
        "anthropic-compatible",
        Some(ANTHROPIC_REPLAY_CAPABILITIES),
    )
}

fn run_outcomes(events: &[cookie_agent_protocol::StoredEvent]) -> Vec<&str> {
    events
        .iter()
        .filter_map(|event| match &event.payload {
            EventPayload::RunFinished {
                outcome: cookie_agent_protocol::Open::Known(outcome),
            } => Some(match outcome {
                cookie_agent_protocol::RunOutcome::Completed { .. } => "completed",
                cookie_agent_protocol::RunOutcome::Failed { .. } => "failed",
                cookie_agent_protocol::RunOutcome::Cancelled { .. } => "cancelled",
                cookie_agent_protocol::RunOutcome::Interrupted { .. } => "interrupted",
            }),
            _ => None,
        })
        .collect()
}

/// A replay the provider refuses as bound to a different conversation is
/// sent again at once without the earlier reasoning, and later requests
/// leave that reasoning out too (`docs/site/specs/thinking-replay-recovery.md`).
#[tokio::test]
async fn a_thinking_binding_mismatch_is_retried_once_without_the_earlier_reasoning() {
    let (endpoint, captured) = anthropic_replay_server(vec![
        AnthropicReplayResponse::Thinking(Some("signed")),
        AnthropicReplayResponse::BindingMismatch400,
        AnthropicReplayResponse::Thinking(Some("after-cut")),
        AnthropicReplayResponse::Text("later"),
    ])
    .await;
    let (fixture, selection) = binding_fixture(&endpoint);
    let session = fixture.engine.create_session(selection.clone()).unwrap();
    run_replay_test_turn(&fixture, session.session_id, &selection, "seed").await;
    run_replay_test_turn(&fixture, session.session_id, &selection, "mismatch").await;
    run_replay_test_turn(&fixture, session.session_id, &selection, "later").await;

    let requests = with_watchdog("captured fixture completion", captured)
        .await
        .expect("binding requests");
    assert_eq!(requests.len(), 4);
    assert!(
        anthropic_request_has_thinking(&requests[1]),
        "the refused replay"
    );
    assert!(!anthropic_request_has_thinking(&requests[2]), "the retry");
    // The next turn leaves out the reasoning before the cut and replays the
    // reasoning made after it.
    let later = request_body(&requests[3]);
    let signatures = later["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|message| message["content"].as_array())
        .flatten()
        .filter(|block| block["type"] == "thinking")
        .map(|block| block["signature"].as_str().unwrap_or_default().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(signatures, ["after-cut"]);

    let events = fixture
        .engine
        .inner
        .store
        .get(session.session_id)
        .unwrap()
        .log
        .events()
        .to_vec();
    assert_eq!(
        run_outcomes(&events),
        ["completed", "completed", "completed"]
    );
    let cuts = events
        .iter()
        .filter(|event| matches!(event.payload, EventPayload::NativeReplayCut { .. }))
        .count();
    assert_eq!(cuts, 1);
    let notices = events
        .iter()
        .filter(|event| matches!(&event.payload, EventPayload::Diagnostic { message, .. } if message.contains("refused the earlier reasoning")))
        .count();
    assert_eq!(notices, 1);
    fixture.engine.shutdown().await;
}

/// The one retry refused again fails the attempt as any refused request
/// does, with a single cut.
#[tokio::test]
async fn a_second_binding_mismatch_fails_the_attempt() {
    let (endpoint, captured) = anthropic_replay_server(vec![
        AnthropicReplayResponse::Thinking(Some("signed")),
        AnthropicReplayResponse::BindingMismatch400,
        AnthropicReplayResponse::BindingMismatch400,
    ])
    .await;
    let (fixture, selection) = binding_fixture(&endpoint);
    let session = fixture.engine.create_session(selection.clone()).unwrap();
    run_replay_test_turn(&fixture, session.session_id, &selection, "seed").await;
    run_replay_test_turn(&fixture, session.session_id, &selection, "mismatch").await;
    let requests = with_watchdog("captured fixture completion", captured)
        .await
        .expect("binding requests");
    assert_eq!(requests.len(), 3);
    let events = fixture
        .engine
        .inner
        .store
        .get(session.session_id)
        .unwrap()
        .log
        .events()
        .to_vec();
    assert_eq!(run_outcomes(&events), ["completed", "failed"]);
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event.payload, EventPayload::NativeReplayCut { .. }))
            .count(),
        1
    );
    fixture.engine.shutdown().await;
}

/// Another 400, such as a tampered signature, is not recovered.
#[tokio::test]
async fn a_tampered_signature_is_not_recovered() {
    let (endpoint, captured) = anthropic_replay_server(vec![
        AnthropicReplayResponse::Thinking(Some("signed")),
        AnthropicReplayResponse::Status400,
    ])
    .await;
    let (fixture, selection) = binding_fixture(&endpoint);
    let session = fixture.engine.create_session(selection.clone()).unwrap();
    run_replay_test_turn(&fixture, session.session_id, &selection, "seed").await;
    run_replay_test_turn(&fixture, session.session_id, &selection, "tampered").await;
    let requests = with_watchdog("captured fixture completion", captured)
        .await
        .expect("binding requests");
    assert_eq!(requests.len(), 2);
    let events = fixture
        .engine
        .inner
        .store
        .get(session.session_id)
        .unwrap()
        .log
        .events()
        .to_vec();
    assert!(
        !events
            .iter()
            .any(|event| matches!(event.payload, EventPayload::NativeReplayCut { .. }))
    );
    fixture.engine.shutdown().await;
}

/// A revert to before a cut removes it: the reasoning before it replays
/// again.
#[tokio::test]
async fn a_revert_to_before_a_cut_replays_the_earlier_reasoning_again() {
    let (endpoint, captured) = anthropic_replay_server(vec![
        AnthropicReplayResponse::Thinking(Some("signed")),
        AnthropicReplayResponse::BindingMismatch400,
        AnthropicReplayResponse::Text("recovered"),
        AnthropicReplayResponse::Text("after revert"),
    ])
    .await;
    let (fixture, selection) = binding_fixture(&endpoint);
    let session = fixture.engine.create_session(selection.clone()).unwrap();
    run_replay_test_turn(&fixture, session.session_id, &selection, "seed").await;
    let seed_end = fixture
        .engine
        .inner
        .store
        .get(session.session_id)
        .unwrap()
        .log
        .events()
        .last()
        .unwrap()
        .seq;
    run_replay_test_turn(&fixture, session.session_id, &selection, "mismatch").await;
    let last = fixture
        .engine
        .inner
        .store
        .get(session.session_id)
        .unwrap()
        .log
        .events()
        .last()
        .unwrap()
        .seq;
    fixture
        .engine
        .revert_session(
            session.session_id,
            seed_end,
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
            Some(last),
        )
        .await
        .expect("revert to before the cut");
    run_replay_test_turn(&fixture, session.session_id, &selection, "again").await;
    let requests = with_watchdog("captured fixture completion", captured)
        .await
        .expect("binding requests");
    assert_eq!(requests.len(), 4);
    assert!(anthropic_request_has_thinking(&requests[3]));
    fixture.engine.shutdown().await;
}
