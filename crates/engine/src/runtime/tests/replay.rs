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
    let primary = "---\ndescription: Replay recovery test\nmode: primary\nenabled: true\nmodels: [{ model: \"custom.test/group/model\", variant: base }]\npermissions: {}\n---\nReplay recovery.\n";
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
    let primary = "---\ndescription: Signed replay test\nmode: primary\nenabled: true\nmodels: [{ model: \"custom.test/group/model\", variant: base }]\npermissions: {}\n---\nSigned replay.\n";
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
