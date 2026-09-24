use std::sync::Arc;

use cookie_agent_config::{ModelPricing, PicoUsdPerMillion};

use cookie_agent_protocol::{ClientRunId, EventPayload, RunStartParams};

use crate::{Engine, EngineOptions};

use super::support::*;

#[tokio::test]
async fn anthropic_prompt_caching_records_wire_markers_usage_and_rollup() {
    let bodies = vec![
        anthropic_usage_body("first", 10, 0, 20),
        anthropic_usage_body("second", 5, 25, 0),
        anthropic_usage_body("third", 5, 25, 0),
    ];
    let (endpoint, captured, _reached, _release) =
        scripted_server_with_delayed_response(bodies, usize::MAX).await;
    let primary = "---\ndescription: Anthropic cache test\nmode: primary\nenabled: true\nmodels: [{ model: \"custom.test/group/model\", variant: base }]\npermissions:\n  write: allow\n---\nStable cache system prompt.\n";
    let (fixture, selection) =
        custom_fixture_with_endpoint_primary_internal_concurrency_context_and_adaptor(
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
        );
    fixture
        .engine
        .register_tool_provider(Arc::new(TestWriteProvider {
            executed: Arc::new(TestFlag::default()),
        }));
    let session = fixture.engine.create_session(selection.clone()).unwrap();
    for index in 0..3 {
        fixture
            .engine
            .start_run(
                RunStartParams {
                    reset_fallback: false,
                    session_id: session.session_id,
                    client_run_id: ClientRunId::new(format!("anthropic-cache-{index}")).unwrap(),
                    selection: selection.clone(),
                    input: format!("turn {index}"),
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
    assert_eq!(requests.len(), 3);
    for request in &requests {
        let body = request_body(request);
        assert_eq!(body["system"][0]["cache_control"]["ttl"], "1h");
        assert_eq!(body["tools"][0]["cache_control"]["ttl"], "1h");
        assert_eq!(cache_marker_count(&body), 3);
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(
            messages.last().unwrap()["content"]
                .as_array()
                .unwrap()
                .last()
                .unwrap()["cache_control"]["ttl"],
            "5m"
        );
    }

    let events = fixture
        .engine
        .inner
        .store
        .get(session.session_id)
        .unwrap()
        .log
        .events();
    let usage = events
        .iter()
        .filter_map(|event| match &event.payload {
            EventPayload::ModelUsageRecorded { usage, .. } => Some(usage.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(usage.len(), 3);
    assert_eq!(usage[0].input_tokens_cache_write, Some(20));
    assert_eq!(usage[0].input_tokens_cache_read, Some(0));
    assert_eq!(usage[1].input_tokens_cache_write, Some(0));
    assert_eq!(usage[1].input_tokens_cache_read, Some(25));
    assert_eq!(usage[2].input_tokens_cache_read, Some(25));

    let rollup = fixture
        .engine
        .session_usage(session.session_id)
        .unwrap()
        .usage;
    assert_eq!(rollup.cache_write_tokens, 20);
    assert_eq!(rollup.cache_read_tokens, 50);
    assert_eq!(rollup.request_count, 3);
    assert_eq!(rollup.cache_hit_rate, Some(50.0 / 90.0));
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn openai_adapter_cache_write_usage_reaches_events_and_rollup() {
    let body = scripted_text_cache_write_usage_body("cached", 100, 5, 20, 30);
    let (endpoint, _captured, _reached, _release) =
        scripted_server_with_delayed_response(vec![body], usize::MAX).await;
    let primary = "---\ndescription: OpenAI cache usage\nmode: primary\nenabled: true\nmodels: [{ model: \"custom.test/group/model\", variant: base }]\npermissions: {}\n---\nStable system prompt.\n";
    let (fixture, selection) =
        custom_fixture_with_endpoint_primary_internal_concurrency_context_and_adaptor(
            &endpoint,
            primary,
            None,
            None,
            false,
            None,
            None,
            4_096,
            None,
            "openai-chat",
        );
    let session = fixture.engine.create_session(selection.clone()).unwrap();
    fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: session.session_id,
                client_run_id: ClientRunId::new("openai-cache-write-usage").unwrap(),
                selection,
                input: "exercise cache accounting".into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .unwrap();
    wait_for_session_not_running(&fixture.engine, session.session_id).await;

    let projection = fixture.engine.inner.store.get(session.session_id).unwrap();
    let usage = projection
        .log
        .events()
        .iter()
        .find_map(|event| match &event.payload {
            EventPayload::ModelUsageRecorded { usage, .. } => Some(usage.clone()),
            _ => None,
        })
        .expect("OpenAI usage event");
    assert_eq!(usage.input_tokens_cache_read, Some(20));
    assert_eq!(usage.input_tokens_cache_write, Some(30));
    assert_eq!(projection.usage_rollup.cache_write_tokens, 30);
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn openai_chat_qwen_usage_reaches_events_and_rollup() {
    let bodies = vec![
        scripted_qwen_usage_body("cached", Some(1216)),
        scripted_qwen_usage_body("uncached", None),
    ];
    let (endpoint, _captured, _reached, _release) =
        scripted_server_with_delayed_response(bodies, usize::MAX).await;
    let primary = "---\ndescription: Qwen usage\nmode: primary\nenabled: true\nmodels: [{ model: \"custom.test/group/model\", variant: base }]\npermissions: {}\n---\nStable system prompt.\n";
    let (fixture, selection) =
        custom_fixture_with_endpoint_primary_internal_concurrency_context_and_adaptor(
            &endpoint,
            primary,
            None,
            None,
            false,
            None,
            None,
            4_096,
            None,
            "openai-chat",
        );

    let cached_session = fixture.engine.create_session(selection.clone()).unwrap();
    fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: cached_session.session_id,
                client_run_id: ClientRunId::new("qwen-cached-usage").unwrap(),
                selection: selection.clone(),
                input: "cached usage".into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .unwrap();
    wait_for_session_not_running(&fixture.engine, cached_session.session_id).await;
    let cached_projection = fixture
        .engine
        .inner
        .store
        .get(cached_session.session_id)
        .unwrap();
    let cached_usage = cached_projection
        .log
        .events()
        .iter()
        .find_map(|event| match &event.payload {
            EventPayload::ModelUsageRecorded { usage, .. } => Some(usage.clone()),
            _ => None,
        })
        .expect("Qwen cached usage event");
    assert_eq!(cached_usage.input_tokens, Some(1264));
    assert_eq!(cached_usage.input_tokens_cache_read, Some(1216));
    assert_eq!(cached_usage.input_tokens_no_cache, Some(48));
    assert_eq!(cached_usage.output_tokens, Some(30));
    assert_eq!(cached_usage.output_tokens_reasoning, Some(29));
    assert_eq!(cached_usage.output_tokens_text, Some(1));
    assert_eq!(cached_projection.usage_rollup.input_tokens, 1264);
    assert_eq!(cached_projection.usage_rollup.cache_read_tokens, 1216);
    assert_eq!(cached_projection.usage_rollup.reasoning_tokens, 29);

    let uncached_session = fixture.engine.create_session(selection.clone()).unwrap();
    fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: uncached_session.session_id,
                client_run_id: ClientRunId::new("qwen-uncached-usage").unwrap(),
                selection,
                input: "uncached usage".into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .unwrap();
    wait_for_session_not_running(&fixture.engine, uncached_session.session_id).await;
    let uncached_projection = fixture
        .engine
        .inner
        .store
        .get(uncached_session.session_id)
        .unwrap();
    let uncached_usage = uncached_projection
        .log
        .events()
        .iter()
        .find_map(|event| match &event.payload {
            EventPayload::ModelUsageRecorded { usage, .. } => Some(usage.clone()),
            _ => None,
        })
        .expect("Qwen uncached usage event");
    assert_eq!(uncached_usage.input_tokens, Some(1264));
    assert_eq!(uncached_usage.input_tokens_cache_read, None);
    assert_eq!(uncached_usage.input_tokens_no_cache, None);
    assert_eq!(uncached_usage.output_tokens, Some(30));
    assert_eq!(uncached_usage.output_tokens_reasoning, Some(29));
    assert_eq!(uncached_usage.output_tokens_text, Some(1));
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn anthropic_cache_markers_survive_real_checkpoint_reopen() {
    let bodies = vec![
        anthropic_usage_body("first turn", 600, 0, 0),
        anthropic_usage_body("checkpoint summary", 600, 0, 0),
        anthropic_usage_body("after reopen", 100, 0, 0),
    ];
    let (endpoint, captured, _reached, _release) =
        scripted_server_with_delayed_response(bodies, usize::MAX).await;
    let primary = "---\ndescription: Anthropic checkpoint cache test\nmode: primary\nenabled: true\nmodels: [{ model: \"custom.test/group/model\", variant: base }]\npermissions:\n  write: allow\n---\nStable checkpoint system prompt.\n";
    let (fixture, selection) =
        custom_fixture_with_endpoint_primary_internal_concurrency_context_and_adaptor(
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
        );
    fixture
        .engine
        .register_tool_provider(Arc::new(TestWriteProvider {
            executed: Arc::new(TestFlag::default()),
        }));
    let session = fixture.engine.create_session(selection.clone()).unwrap();
    fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: session.session_id,
                client_run_id: ClientRunId::new("anthropic-before-checkpoint").unwrap(),
                selection: selection.clone(),
                input: "old context ".repeat(300),
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
                cookie_agent_protocol::EventOrigin::new("client:test").unwrap()
            )
            .await
            .unwrap()
    );
    assert!(fixture
        .engine
        .inner
        .store
        .get(session.session_id)
        .unwrap()
        .log
        .events()
        .iter()
        .any(|event| matches!(
            event.payload,
            EventPayload::ContextCheckpointCommitted {
                commit: cookie_agent_protocol::ContextCheckpointCommit {
                    checkpoint: cookie_agent_protocol::ContextCheckpoint::InternalSummary { .. },
                    ..
                }
            }
        )));

    fixture.engine.shutdown().await;
    drop(fixture.engine);
    let reopened = reopen_engine_parts(&fixture._directory, &fixture.config, &fixture.manager);
    reopened.register_tool_provider(Arc::new(TestWriteProvider {
        executed: Arc::new(TestFlag::default()),
    }));
    reopened
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: session.session_id,
                client_run_id: ClientRunId::new("anthropic-after-checkpoint").unwrap(),
                selection,
                input: "new live turn".into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .unwrap();
    wait_for_session_not_running(&reopened, session.session_id).await;

    let requests = with_watchdog("captured fixture completion", captured)
        .await
        .unwrap();
    assert_eq!(requests.len(), 3);
    let compaction_body = request_body(&requests[1]);
    assert_eq!(cache_marker_count(&compaction_body), 3);
    let body = request_body(&requests[2]);
    assert_eq!(cache_marker_count(&body), 3);
    assert_eq!(body["system"][0]["cache_control"]["ttl"], "1h");
    assert_eq!(body["tools"][0]["cache_control"]["ttl"], "1h");
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(messages[0]["role"], "user");
    assert!(
        messages[0]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("checkpoint summary")
    );
    assert_eq!(
        messages.last().unwrap()["content"]
            .as_array()
            .unwrap()
            .last()
            .unwrap()["cache_control"]["ttl"],
        "5m"
    );
    reopened.shutdown().await;
}

#[tokio::test]
async fn anthropic_prompt_caching_disabled_emits_no_markers_or_cache_usage() {
    let (endpoint, captured, _reached, _release) = scripted_server_with_delayed_response(
        vec![anthropic_usage_body("uncached", 10, 0, 0)],
        usize::MAX,
    )
    .await;
    let primary = "---\ndescription: Anthropic cache baseline\nmode: primary\nenabled: true\nmodels:\n  - model: custom.test/group/model\n    variant: base\n    cache:\n      anthropic: { system: off, tools: off, rolling: off }\npermissions: {}\n---\nUncached system prompt.\n";
    let (fixture, selection) =
        custom_fixture_with_endpoint_primary_internal_concurrency_context_and_adaptor(
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
        );
    let session = fixture.engine.create_session(selection.clone()).unwrap();
    fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: session.session_id,
                client_run_id: ClientRunId::new("anthropic-cache-disabled").unwrap(),
                selection,
                input: "baseline".into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .unwrap();
    wait_for_session_not_running(&fixture.engine, session.session_id).await;

    let requests = with_watchdog("captured fixture completion", captured)
        .await
        .unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(cache_marker_count(&request_body(&requests[0])), 0);
    let rollup = fixture
        .engine
        .session_usage(session.session_id)
        .unwrap()
        .usage;
    assert_eq!(rollup.cache_read_tokens, 0);
    assert_eq!(rollup.cache_write_tokens, 0);
    assert_eq!(rollup.cache_hit_rate, Some(0.0));
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn session_tree_usage_aggregates_nested_and_evicted_children() {
    let bodies = vec![
        scripted_tool_usage_body(
            "tree-root-child",
            serde_json::json!({
                "agent_type": "worker",
                "description": "Tree child",
                "prompt": "delegate one level deeper"
            }),
            10,
            1,
            1,
        ),
        scripted_tool_usage_body(
            "tree-child-grandchild",
            serde_json::json!({
                "agent_type": "worker",
                "description": "Tree grandchild",
                "prompt": "finish the nested task"
            }),
            20,
            2,
            10,
        ),
        scripted_text_usage_body("grandchild complete", 30, None, 15),
        scripted_text_usage_body("child complete", 40, Some(4), 0),
        scripted_text_usage_body("root complete", 50, Some(5), 25),
        scripted_text_usage_body("unrelated complete", 60, Some(6), 60),
    ];
    let (endpoint, captured, _reached, _release) =
        scripted_server_with_delayed_response(bodies, usize::MAX).await;
    let primary = "---\ndescription: Tree usage root\nmode: primary\nenabled: true\nmodels: [{ model: \"custom.test/group/model\", variant: base }]\npermissions:\n  delegate:\n    worker: allow\n---\nBuild a usage tree.\n";
    let worker = "---\ndescription: Tree usage worker\nmode: subagent\nenabled: true\nmodels: [{ model: \"custom.test/group/model\", variant: base }]\npermissions:\n  delegate:\n    worker: allow\n---\nContinue a usage tree.\n";
    let (mut fixture, selection) =
        custom_fixture_with_endpoint_primary_internal_concurrency_and_context(
            &endpoint,
            primary,
            None,
            None,
            false,
            None,
            None,
            4_096,
            Some(worker),
        );
    fixture.engine.shutdown().await;
    fixture.config.runtime.delegation.max_depth = 2;
    fixture.config.runtime.pricing.models.insert(
        "custom.test/group/model".parse().expect("priced model"),
        ModelPricing {
            input_per_million_usd: Some(PicoUsdPerMillion::from_decimal_str("1").unwrap()),
            output_per_million_usd: Some(PicoUsdPerMillion::from_decimal_str("2").unwrap()),
            cache_read_per_million_usd: Some(PicoUsdPerMillion::from_decimal_str("0.5").unwrap()),
            ..ModelPricing::default()
        },
    );
    fixture.engine = Engine::open(EngineOptions {
        data_dir: fixture._directory.path().join("data"),
        cwd: fixture._directory.path().to_owned(),
        config: fixture.config.clone(),
        model_manager: Arc::clone(&fixture.manager),
        tools: Vec::new(),
    })
    .expect("tree usage engine");
    fixture
        .engine
        .register_tool_provider(Arc::new(TestDelegateProvider {
            engine: fixture.engine.clone(),
        }));

    let root = fixture
        .engine
        .create_session(selection.clone())
        .expect("tree root");
    fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: root.session_id,
                client_run_id: ClientRunId::new("tree-usage-root").unwrap(),
                selection: selection.clone(),
                input: "build nested tree".into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("tree root run");
    wait_for_session_not_running(&fixture.engine, root.session_id).await;
    let child_id = fixture.engine.children(root.session_id).expect("children")[0].session_id;
    let grandchild_id = fixture.engine.children(child_id).expect("children")[0].session_id;

    let unrelated = fixture
        .engine
        .create_session(selection.clone())
        .expect("unrelated root");
    fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: unrelated.session_id,
                client_run_id: ClientRunId::new("tree-usage-unrelated").unwrap(),
                selection,
                input: "not part of the tree".into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("unrelated run");
    wait_for_session_not_running(&fixture.engine, unrelated.session_id).await;
    assert_eq!(
        with_watchdog("captured fixture completion", captured)
            .await
            .expect("tree usage requests")
            .len(),
        6
    );

    fixture.engine.shutdown().await;
    drop(fixture.engine);

    let reopened = reopen_engine_parts(&fixture._directory, &fixture.config, &fixture.manager);
    // §8.2 #8: startup folds root logs only, so the nested invocation recorded
    // in the child's own log is still unknown to the delegation registry.
    assert!(
        !reopened.inner.store.is_tree_loaded(root.session_id),
        "a reopened engine must not have loaded any tree"
    );
    assert!(
        reopened
            .inner
            .delegation_events
            .entries()
            .iter()
            .all(|entry| entry.reservation.child_session_id != grandchild_id),
        "nested delegation records live in a child log"
    );
    reopened
        .inner
        .store
        .open_for_write(child_id)
        .expect("adopt child before eviction");
    assert!(reopened.inner.store.evict(child_id).expect("evict child"));
    assert!(!reopened.inner.store.is_resident(child_id));

    let individual = [root.session_id, child_id, grandchild_id]
        .map(|session_id| reopened.session_usage(session_id).unwrap().usage);
    let expected_requests = individual
        .iter()
        .map(|usage| usage.request_count)
        .sum::<u64>();
    let expected_input = individual
        .iter()
        .map(|usage| usage.input_tokens)
        .sum::<u64>();
    let expected_output = individual
        .iter()
        .map(|usage| usage.output_tokens)
        .sum::<u64>();
    let tree = reopened
        .session_tree_usage(root.session_id)
        .expect("tree usage");
    assert_eq!(tree.session_count, 3);
    assert_eq!(tree.usage.request_count, expected_requests);
    assert_eq!(tree.usage.input_tokens, expected_input);
    assert_eq!(tree.usage.output_tokens, expected_output);
    assert_eq!(tree.usage.cache_read_tokens, 51);
    assert_eq!(tree.usage.cache_hit_rate, Some(51.0 / 150.0));
    assert_eq!(tree.usage.estimated_cost_usd, None);
    assert!(
        reopened.inner.store.is_tree_loaded(root.session_id),
        "addressing the tree loaded it once"
    );
    assert!(
        reopened
            .inner
            .delegation_events
            .entries()
            .iter()
            .any(|entry| entry.reservation.child_session_id == grandchild_id
                && entry.terminal_status.is_some()),
        "the tree load restored the nested delegation with its terminal state"
    );
    let unrelated_usage = reopened
        .session_usage(unrelated.session_id)
        .expect("unrelated usage")
        .usage;
    assert_ne!(
        tree.usage.input_tokens,
        expected_input + unrelated_usage.input_tokens
    );
    reopened.shutdown().await;
}
