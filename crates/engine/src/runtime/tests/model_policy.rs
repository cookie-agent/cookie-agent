use std::{fs, sync::Arc};

use cookie_agent_protocol::{
    ClientRunId, EventPayload, InternalAgentKind, RunSelection, RunStartParams, SessionId,
    SessionStatus, Sha256Digest,
};

use jiff::Timestamp;

use crate::EngineError;

use super::support::*;

#[test]
fn parent_model_resolves_exact_binding_skips_parentless_and_replays_historically() {
    let fixture = synthetic_default_fixture(None);
    let descriptor = fixture
        .engine
        .runtime_snapshot()
        .expect("runtime")
        .snapshot
        .agents
        .into_iter()
        .find(|agent| agent.id.as_str() == "default")
        .expect("default agent");
    let selection = RunSelection {
        agent: descriptor.id,
        model: descriptor.resolved_fallback[0].clone(),
        preset: None,
    };
    let owner = frozen_root_policy(&fixture, &selection);
    let parent = owner.selected_suffix[0].clone();
    assert_eq!(
        parent
            .selection
            .variant
            .as_ref()
            .map(|variant| variant.as_str()),
        Some("precise")
    );

    let policy = fixture
        .engine
        .internal_agent_policy(InternalAgentKind::ContextCompaction, &owner, Some(&parent))
        .expect("internal policy");
    assert_eq!(policy.models, vec![parent.clone()]);

    let parentless = fixture
        .engine
        .internal_agent_policy(InternalAgentKind::ContextCompaction, &owner, None)
        .expect("parentless policy");
    assert!(parentless.models.is_empty());

    let replayed_owner = crate::policy::policy_from_snapshot(
        owner.agent.clone(),
        owner.selected_suffix.clone(),
        Arc::clone(&owner.registry),
        Arc::clone(&owner.runtime),
        owner.result_limits.tool_output_max_lines,
        owner.result_limits.tool_output_max_bytes,
        owner.model_retry,
    )
    .expect("replayed owner policy");
    let replayed = fixture
        .engine
        .internal_agent_policy(
            InternalAgentKind::ContextCompaction,
            &replayed_owner,
            replayed_owner.selected_suffix.first(),
        )
        .expect("replayed internal policy");
    assert_eq!(replayed.models, vec![parent]);
}

#[tokio::test]
async fn internal_agent_cache_strategy_omits_rolling_for_stateless_kinds() {
    let primary = "---\ndescription: Cache policy owner\nmode: primary\nenabled: true\nmodels: [{ model: \"custom.test/group/model\", variant: base }]\npermissions: {}\n---\nStable owner prompt.\n";
    let (fixture, selection) =
        custom_fixture_with_endpoint_primary_internal_concurrency_context_and_adaptor(
            "http://127.0.0.1:9/v1",
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
    let owner = frozen_root_policy(&fixture, &selection);
    let parent = owner.selected_suffix.first().unwrap();
    let marker = |options: &oven_sdk::ProviderOptions| {
        options
            .get("anthropic")
            .and_then(|value| value.get("cache_control"))
            .and_then(|value| value.get("ttl"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
    };

    for kind in [
        InternalAgentKind::SessionTitle,
        InternalAgentKind::Approval,
        InternalAgentKind::ContextCompaction,
    ] {
        let policy = fixture
            .engine
            .internal_agent_policy(kind, &owner, Some(parent))
            .unwrap();
        let binding = policy.models.first().unwrap();
        let model =
            crate::policy::resolve_model(binding, policy.runtime.as_ref().unwrap()).unwrap();
        let request = oven_sdk::Request::new(vec![
            oven_sdk::HistoryTurn::system(oven_sdk::SystemMessage::new(vec![
                oven_sdk::SystemPart::Text(oven_sdk::TextPart::new("reusable system")),
            ])),
            oven_sdk::HistoryTurn::user(oven_sdk::UserMessage::new(vec![
                oven_sdk::InputPart::Text(oven_sdk::TextPart::new("unique payload")),
            ])),
        ])
        .with_tools(vec![oven_sdk::ToolDefinition::new(
            "lookup",
            "lookup tool",
            oven_sdk::JsonSchema::new(serde_json::json!({"type":"object"})).unwrap(),
        )]);
        let strategy = policy.cache_strategy(binding, SessionId::new_v7());
        let prepared = model.prepare_request_with_cache_strategy(request, strategy.as_ref());
        let oven_sdk::HistoryTurn::System(system) = &prepared.history[0] else {
            panic!("system turn");
        };
        let oven_sdk::HistoryTurn::User(user) = &prepared.history[1] else {
            panic!("user turn");
        };
        assert_eq!(
            marker(&system.provider_options).as_deref(),
            Some("one_hour")
        );
        assert_eq!(
            marker(&prepared.tools[0].provider_options).as_deref(),
            Some("one_hour")
        );
        assert_eq!(
            marker(&user.provider_options).as_deref(),
            (kind == InternalAgentKind::ContextCompaction).then_some("five_minutes")
        );
    }
    fixture.engine.shutdown().await;
}

#[test]
fn first_party_openai_cache_key_is_always_the_session_id() {
    let primary = "---\ndescription: Cached owner\nmode: primary\nenabled: true\nmodels:\n  - model: custom.test/group/model\n    variant: base\n    cache:\n      openai:\n        prompt_cache_retention: 24h\npermissions: {}\n---\nStable owner prompt.\n";
    let (fixture, selection) =
        custom_fixture_with_endpoint_primary_internal_concurrency_context_and_adaptor(
            "http://127.0.0.1:9/v1",
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
    let policy = frozen_root_policy(&fixture, &selection);
    let binding = policy.selected_suffix.first().unwrap();
    let session = SessionId::new_v7();
    let cookie_agent_models::adapters::CacheStrategyConfig::OpenAi(strategy) =
        policy.cache_strategy(binding, session).unwrap()
    else {
        panic!("OpenAI cache strategy");
    };
    assert_eq!(
        strategy.prompt_cache_key.as_deref(),
        Some(session.to_string().as_str())
    );

    let internal = fixture
        .engine
        .internal_agent_policy(InternalAgentKind::ContextCompaction, &policy, Some(binding))
        .unwrap();
    let internal_binding = internal.models.first().unwrap();
    let cookie_agent_models::adapters::CacheStrategyConfig::OpenAi(strategy) =
        internal.cache_strategy(internal_binding, session).unwrap()
    else {
        panic!("inherited OpenAI cache strategy");
    };
    assert_eq!(
        strategy.prompt_cache_key.as_deref(),
        Some(session.to_string().as_str())
    );
}

#[test]
fn compatible_openai_cache_key_defaults_disables_and_expands() {
    let primary = "---\ndescription: Compatible cache owner\nmode: primary\nenabled: true\nmodels: [{ model: \"custom.test/group/model\", variant: base }]\npermissions: {}\n---\nStable owner prompt.\n";
    for (provider_cache, expected_prefix) in [
        (None, Some("")),
        (Some("prompt_cache_key = \"\""), None),
        (
            Some("prompt_cache_key = \"tenant-${session_id}\""),
            Some("tenant-"),
        ),
    ] {
        let (fixture, selection) = custom_fixture_with_capabilities_and_worker_name(
            "http://127.0.0.1:9/v1",
            primary,
            None,
            None,
            false,
            None,
            None,
            4_096,
            None,
            "openai-compatible",
            None,
            None,
            provider_cache,
            "worker",
        );
        let policy = frozen_root_policy(&fixture, &selection);
        let binding = policy.selected_suffix.first().unwrap();
        let session = SessionId::new_v7();
        let cookie_agent_models::adapters::CacheStrategyConfig::OpenAi(strategy) =
            policy.cache_strategy(binding, session).unwrap()
        else {
            panic!("compatible OpenAI cache strategy");
        };
        let expected = expected_prefix.map(|prefix| format!("{prefix}{session}"));
        assert_eq!(strategy.prompt_cache_key, expected);
    }
}

#[test]
fn explicit_anthropic_one_hour_requires_authored_beta() {
    let primary = "---\ndescription: Anthropic cache owner\nmode: primary\nenabled: true\nmodels:\n  - model: custom.test/group/model\n    variant: base\n    cache:\n      anthropic: { system: 1h, tools: off, rolling: off }\npermissions: {}\n---\nStable owner prompt.\n";
    let (without_beta, selection) =
        custom_fixture_with_endpoint_primary_internal_concurrency_context_and_adaptor(
            "http://127.0.0.1:9/v1",
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
    assert!(matches!(
        try_frozen_root_policy(&without_beta, &selection),
        Err(EngineError::CacheStrategy(_))
    ));

    let (with_beta, selection) = custom_fixture_with_capabilities_and_variants(
        "http://127.0.0.1:9/v1",
        primary,
        None,
        None,
        false,
        None,
        None,
        4_096,
        None,
        "anthropic-compatible",
        None,
        Some("adaptor_options = { beta = [\"extended-cache-ttl-2025-04-11\"] }"),
    );
    assert!(try_frozen_root_policy(&with_beta, &selection).is_ok());
}

#[tokio::test]
async fn model_less_delegated_child_first_request_inherits_parent_cache_strategy() {
    let primary = "---\ndescription: Cached owner\nmode: primary\nenabled: true\nmodels: [{ model: \"custom.test/group/model\", variant: base }]\npermissions:\n  delegate:\n    worker: allow\n    \"*\": deny\n---\nStable owner prompt.\n";
    let worker = "---\ndescription: Inheriting worker\nmode: subagent\nenabled: true\nmodels: []\npermissions: {}\n---\nWorker prompt.\n";
    let (endpoint, captured) = scripted_delegation_server().await;
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
            Some(worker),
            "openai-chat",
        );
    fixture
        .engine
        .register_tool_provider(Arc::new(TestDelegateProvider {
            engine: fixture.engine.clone(),
        }));
    let parent = fixture.engine.create_session(selection.clone()).unwrap();
    fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: parent.session_id,
                client_run_id: cookie_agent_protocol::ClientRunId::new(
                    "delegated-cache-inheritance",
                )
                .unwrap(),
                selection,
                input: "delegate this task".into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .unwrap();
    await_projection(
        &fixture.engine,
        parent.session_id,
        "delegated cache completion",
        |projection| projection.status == SessionStatus::Completed,
    )
    .await;

    let child = fixture
        .engine
        .children(parent.session_id)
        .expect("children")[0]
        .session_id;
    let requests = with_watchdog("captured fixture completion", captured)
        .await
        .unwrap();
    assert_eq!(requests.len(), 3);
    assert_eq!(
        request_body(&requests[1])["prompt_cache_key"],
        child.to_string()
    );
    let parent = fixture.engine.inner.store.get(parent.session_id).unwrap();
    assert!(parent.log.events().iter().any(|event| {
        matches!(
            &event.payload,
            EventPayload::DelegationReserved { cache_strategies, .. }
                if matches!(
                    cache_strategies.as_slice(),
                    [Some(cookie_agent_protocol::FrozenCacheStrategy::OpenAi { .. })]
                )
        )
    }));
    fixture.engine.shutdown().await;
}

#[test]
fn authored_cache_strategy_for_unsupported_family_fails_policy_freeze() {
    let primary = "---\ndescription: Cached owner\nmode: primary\nenabled: true\nmodels:\n  - model: custom.test/group/model\n    variant: base\n    cache:\n      openai:\n        mode: explicit\n        ttl: 30m\npermissions: {}\n---\nStable owner prompt.\n";
    let (fixture, selection) =
        custom_fixture_with_endpoint_primary_internal_concurrency_context_and_adaptor(
            "http://127.0.0.1:9/v1",
            primary,
            None,
            None,
            false,
            None,
            None,
            4_096,
            None,
            "openai-compatible",
        );
    assert!(matches!(
        try_frozen_root_policy(&fixture, &selection),
        Err(EngineError::CacheStrategy(_))
    ));
}

#[test]
fn fifth_openai_cache_write_surfaces_as_engine_invalid_request() {
    let primary = "---\ndescription: Cache overflow owner\nmode: primary\nenabled: true\nmodels: [{ model: \"custom.test/group/model\", variant: base }]\npermissions: {}\n---\nStable owner prompt.\n";
    let (fixture, selection) =
        custom_fixture_with_endpoint_primary_internal_concurrency_context_and_adaptor(
            "http://127.0.0.1:9/v1",
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
    let policy = frozen_root_policy(&fixture, &selection);
    let binding = policy.selected_suffix.first().unwrap();
    let model = crate::policy::resolve_model(binding, &policy.runtime).unwrap();
    let marked = (0..4)
        .map(|index| {
            oven_sdk::InputPart::Text(
                oven_sdk_openai::OpenAiPromptCacheBreakpointExt::with_openai_prompt_cache_breakpoint(
                    oven_sdk::TextPart::new(format!("marked-{index}")),
                ),
            )
        })
        .collect();
    let request = oven_sdk::Request::new(vec![
        oven_sdk::HistoryTurn::system(oven_sdk::SystemMessage::new(vec![
            oven_sdk::SystemPart::Text(oven_sdk::TextPart::new("system breakpoint")),
        ])),
        oven_sdk::HistoryTurn::user(oven_sdk::UserMessage::new(marked)),
    ]);
    let strategy = cookie_agent_models::adapters::CacheStrategyConfig::OpenAi(
        cookie_agent_models::adapters::OpenAiCacheStrategyConfig {
            prompt_cache_key: Some("overflow".into()),
            prompt_cache_retention: None,
            mode: Some(cookie_agent_models::adapters::OpenAiCacheMode::Explicit),
            ttl: Some(cookie_agent_models::adapters::OpenAiPromptCacheTtl::ThirtyMinutes),
            system: true,
            rolling: false,
        },
    );
    let prepared = model.prepare_request_with_cache_strategy(request, Some(&strategy));
    let error = model.model().validate_request(&prepared).unwrap_err();
    let EngineError::Model(error) = EngineError::from(error) else {
        panic!("engine model error");
    };
    assert_eq!(error.kind, oven_sdk::ModelErrorKind::InvalidRequest);
    assert!(error.message.contains("at most four"));
}

#[test]
fn model_capabilities_follow_the_exact_fallback_binding() {
    let fixture = synthetic_default_fixture(None);
    let descriptor = fixture
        .engine
        .runtime_snapshot()
        .expect("runtime")
        .snapshot
        .agents
        .into_iter()
        .find(|agent| agent.id.as_str() == "default")
        .expect("default agent");
    let selection = RunSelection {
        agent: descriptor.id,
        model: descriptor.resolved_fallback[0].clone(),
        preset: None,
    };
    let mut owner = frozen_root_policy(&fixture, &selection);
    let fallback = crate::test_support::model_binding_named("fallback-one");
    let mut runtime = (*owner.runtime).clone();
    let mut fallback_descriptor = runtime.result.snapshot.models[0].clone();
    fallback_descriptor.key = fallback.selection.model.clone();
    fallback_descriptor.capabilities.context_tokens = 16_384;
    let expected = fallback_descriptor.capabilities.clone();
    runtime.result.snapshot.models.push(fallback_descriptor);
    owner.runtime = Arc::new(runtime);

    assert_eq!(owner.model_capabilities(&fallback), Some(expected));
    assert_ne!(
        owner.model_capabilities(&owner.selected_suffix[0]),
        owner.model_capabilities(&fallback)
    );
}

#[test]
fn manual_compaction_resolves_parent_model_from_nonzero_active_fallback() {
    let fixture = synthetic_default_fixture(None);
    let descriptor = fixture
        .engine
        .runtime_snapshot()
        .expect("runtime")
        .snapshot
        .agents
        .into_iter()
        .find(|agent| agent.id.as_str() == "default")
        .expect("default agent");
    let selection = RunSelection {
        agent: descriptor.id,
        model: descriptor.resolved_fallback[0].clone(),
        preset: None,
    };
    let mut owner = frozen_root_policy(&fixture, &selection);
    let fallback = crate::test_support::model_binding_named("fallback-one");
    owner.selected_suffix.push(fallback.clone());
    let run = cookie_agent_protocol::RunId::new_v7();
    let events = vec![cookie_agent_protocol::StoredEvent {
        engine_version: None,
        origin: None,
        session_id: SessionId::new_v7(),
        run_id: Some(run),
        seq: 1,
        timestamp: Timestamp::now(),
        payload: EventPayload::ModelAttemptStarted {
            attempt_id: cookie_agent_protocol::AttemptId::new_v7(),
            attempt_ordinal: 2,
            fallback_index: 1,
            retry_ordinal: 0,
            resolved_model: crate::policy::wire_resolved(&fallback),
            prompt_fingerprint: Sha256Digest::of_bytes(b"fallback prompt"),
        },
    }];
    let binding = crate::runtime::compaction::active_compaction_binding(&owner, &events, run)
        .expect("active compaction binding");
    assert_eq!(binding.selection, fallback.selection);
    let internal = fixture
        .engine
        .internal_agent_policy(InternalAgentKind::ContextCompaction, &owner, Some(binding))
        .expect("compaction policy");
    assert_eq!(internal.models, vec![fallback]);
}

#[tokio::test]
async fn mixed_binding_fallback_preserves_configured_provider_order() {
    let root = scripted_text_usage_body("root", 8_192, Some(1), 0);
    let context_error = (400, r#"{"error":{"message":"maximum context length exceeded","type":"invalid_request_error","code":"context_length_exceeded"}}"#.to_owned());
    let summary = scripted_text_usage_body("fallback checkpoint", 1, Some(1), 0);
    let (endpoint, captured, ..) = scripted_server_with_status_and_delay(
        vec![(200, root), context_error, (200, summary)],
        usize::MAX,
    )
    .await;
    let (mut fixture, selection) =
        custom_fixture_with_endpoint_primary_internal_concurrency_and_context(
            &endpoint,
            "---\ndescription: ordered fallback\nmode: primary\nenabled: true\nmodels: [{ model: \"custom.test/group/model\", variant: base }]\npermissions: {}\n---\nTest fallback.\n",
            Some((
                "compaction.md",
                "---\ndescription: compaction\nmode: internal\nenabled: true\nmodels: [{ model: \"custom.test/group/model\", variant: base }, { model: \"custom.test/group/fallback\", variant: null }]\nlimits: { timeout_ms: 30000, max_output_tokens: 256 }\npermissions: {}\n---\nSummarize.\n",
            )),
            Some(0),
            false,
            None,
            None,
            100_000,
            None,
        );
    fixture.engine.shutdown().await;
    fixture.engine = reopen_engine(&fixture);
    let session = fixture.engine.create_session(selection.clone()).unwrap();
    fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: session.session_id,
                client_run_id: ClientRunId::new("fallback").unwrap(),
                selection: selection.clone(),
                input: "prime".into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .unwrap();
    wait_for_session_not_running(&fixture.engine, session.session_id).await;
    fixture
        .engine
        .compact_session(
            session.session_id,
            None,
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .unwrap();
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
    assert!(events.iter().any(|event| matches!(
        event.payload,
        EventPayload::ContextCheckpointCommitted { .. }
    )));
    let summary_requests = requests
        .iter()
        .filter(|request| request.contains("Summarize."))
        .collect::<Vec<_>>();
    assert_eq!(summary_requests.len(), 2);
    let model = |request: &str| {
        let body = request.split_once("\r\n\r\n").unwrap().1;
        serde_json::from_str::<serde_json::Value>(body).unwrap()["model"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    assert_eq!(model(summary_requests[0]), "group/model");
    assert_eq!(model(summary_requests[1]), "group/fallback");
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn model_request_replacement_precedes_cache_and_keep_adjustments_chain() {
    let bodies = vec![anthropic_usage_body("done", 10, 0, 0)];
    let (endpoint, captured, _reached, _release) =
        scripted_server_with_delayed_response(bodies, usize::MAX).await;
    let primary = "---\ndescription: Intercepted Anthropic cache test\nmode: primary\nenabled: true\nmodels: [{ model: \"custom.test/group/model\", variant: base }]\npermissions:\n  write: allow\n---\nOriginal system prompt.\n";
    let (mut fixture, selection) =
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
    let marker = tempfile::tempdir().unwrap();
    let provider_file = marker.path().join("provider.jsonl");
    let model_capabilities = r#"{"producer_messaging":false,"tools":false,"resources":false,"subscribe_events":false,"subscribe_bus":false,"publish_bus":false,"publish_session_events":false,"intercept":["model_before_request"]}"#;
    let provider_capabilities = r#"{"producer_messaging":false,"tools":false,"resources":false,"subscribe_events":false,"subscribe_bus":false,"publish_bus":false,"publish_session_events":false,"intercept":["provider_before_headers","provider_after_response"]}"#;
    let replacement = serde_json::json!({
        "action": "replace",
        "messages": [
            {
                "role": "system",
                "content": {
                    "content": [{"type":"text","value":{"text":"Replacement system","metadata":null}}],
                    "provider_options": {}
                }
            },
            {
                "role": "user",
                "content": {
                    "content": [{"type":"text","value":{"text":"Replacement user","metadata":null}}],
                    "provider_options": {}
                }
            }
        ]
    })
    .to_string();
    reopen_with_interception_plugins(
        &mut fixture,
        vec![
            (
                "invalid".into(),
                interception_plugin(
                    "invalid",
                    &[
                        ("FIXTURE_CAPABILITIES", model_capabilities.into()),
                        (
                            "FIXTURE_MODEL_BEFORE_REQUEST_RESULT",
                            r#"{"action":"replace","params_adjustments":{"max_tokens":7}}"#.into(),
                        ),
                    ],
                ),
            ),
            (
                "replace".into(),
                interception_plugin(
                    "replace",
                    &[
                        ("FIXTURE_CAPABILITIES", model_capabilities.into()),
                        ("FIXTURE_MODEL_BEFORE_REQUEST_RESULT", replacement),
                    ],
                ),
            ),
            (
                "adjust".into(),
                interception_plugin(
                    "adjust",
                    &[
                        ("FIXTURE_CAPABILITIES", model_capabilities.into()),
                        (
                            "FIXTURE_MODEL_BEFORE_REQUEST_RESULT",
                            r#"{"action":"keep","params_adjustments":{"max_tokens":19}}"#.into(),
                        ),
                    ],
                ),
            ),
            (
                "provider".into(),
                interception_plugin(
                    "provider",
                    &[
                        ("FIXTURE_CAPABILITIES", provider_capabilities.into()),
                        (
                            "FIXTURE_PROVIDER_BEFORE_HEADERS_RESULT",
                            r#"{"set":{"x-test":"value"},"delete":[]}"#.into(),
                        ),
                        (
                            "FIXTURE_INTERCEPT_FILE",
                            provider_file.display().to_string(),
                        ),
                    ],
                ),
            ),
        ],
    )
    .await;
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
                client_run_id: ClientRunId::new("intercepted-cache").unwrap(),
                selection,
                input: "original user".into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .unwrap();
    wait_for_session_not_running(&fixture.engine, session.session_id).await;
    let requests = with_watchdog("captured fixture completion", captured)
        .await
        .unwrap();
    let body = request_body(&requests[0]);
    assert_eq!(body["max_tokens"], 19);
    assert_eq!(body["system"][0]["text"], "Replacement system");
    assert_eq!(body["system"][0]["cache_control"]["ttl"], "1h");
    let last_content = body["messages"].as_array().unwrap().last().unwrap()["content"]
        .as_array()
        .unwrap()
        .last()
        .unwrap()
        .clone();
    assert_eq!(last_content["text"], "Replacement user");
    assert_eq!(last_content["cache_control"]["ttl"], "5m");
    assert_eq!(cache_marker_count(&body), 3);

    await_projection(
        &fixture.engine,
        session.session_id,
        "plugin diagnostics",
        |projection| {
            let events = projection.log.events();
            let invalid = events.iter().any(|event| {
                matches!(
                    &event.payload,
                    EventPayload::PluginDiagnostic { kind, message, .. }
                        if *kind == cookie_agent_protocol::PluginDiagnosticKind::InvalidModification
                            && message.contains("replace requires messages")
                )
            });
            let unsupported = events.iter().any(|event| matches!(
                &event.payload,
                EventPayload::PluginDiagnostic { kind, .. }
                    if *kind == cookie_agent_protocol::PluginDiagnosticKind::UnsupportedCapability
            ));
            invalid && unsupported
        },
    )
    .await;
    let provider_calls = fs::read_to_string(provider_file).unwrap();
    let after = provider_calls
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .find(|call| call["method"] == "plugin/intercept/provider_after_response")
        .expect("provider response observation");
    assert_eq!(after["params"]["status"], 200);
    assert_eq!(after["params"]["headers"], serde_json::json!({}));
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn primary_agent_max_output_tokens_caps_model_requests() {
    let (endpoint, captured, _reached, _release) = scripted_server_with_delayed_response(
        vec![scripted_text_body("capped response")],
        usize::MAX,
    )
    .await;
    let (fixture, selection) = custom_fixture_with_endpoint_and_primary_agent(
        &endpoint,
        "---\ndescription: Output-capped primary\nmode: primary\nenabled: true\nmodels: [{ model: \"custom.test/group/model\", variant: base }]\nlimits: { max_output_tokens: 128 }\npermissions: {}\n---\nKeep the response bounded.\n",
    );
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
                client_run_id: ClientRunId::new("primary-output-cap").expect("run ID"),
                selection,
                input: "respond briefly".into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("start capped run");
    wait_for_session_not_running(&fixture.engine, session.session_id).await;

    let requests = with_watchdog("captured fixture completion", captured)
        .await
        .expect("captured capped request");
    let body = requests[0]
        .split_once("\r\n\r\n")
        .expect("HTTP request body")
        .1;
    let request: serde_json::Value = serde_json::from_str(body).expect("request JSON");
    assert_eq!(
        request
            .get("max_tokens")
            .and_then(serde_json::Value::as_u64),
        Some(128)
    );
    fixture.engine.shutdown().await;
}

/// Resolve the effective per-request output cap the internal agent `kind` would
/// carry when it runs under `selection` on the parent's own binding.
fn internal_output_cap(
    fixture: &Fixture,
    selection: &RunSelection,
    kind: InternalAgentKind,
) -> Option<u64> {
    let owner = frozen_root_policy(fixture, selection);
    let parent_binding = owner.selected_suffix.first().expect("parent binding");
    let policy = fixture
        .engine
        .internal_agent_policy(kind, &owner, Some(parent_binding))
        .expect("internal agent policy");
    let binding = policy.models.first().expect("internal binding").clone();
    crate::runtime::internal_agents::internal_agent_output_limit(&binding, &policy)
}

const INTERNAL_KINDS: [InternalAgentKind; 3] = [
    InternalAgentKind::Approval,
    InternalAgentKind::ContextCompaction,
    InternalAgentKind::SessionTitle,
];

#[test]
fn built_in_internal_agents_inherit_the_parent_run_output_cap() {
    let (fixture, selection) = custom_fixture_with_endpoint_and_primary_agent(
        "http://127.0.0.1:9/v1",
        "---\ndescription: Output-capped primary\nmode: primary\nenabled: true\nmodels: [{ model: \"custom.test/group/model\", variant: base }]\nlimits: { max_output_tokens: 128 }\npermissions: {}\n---\nKeep the response bounded.\n",
    );
    for kind in INTERNAL_KINDS {
        let policy = {
            let owner = frozen_root_policy(&fixture, &selection);
            fixture
                .engine
                .internal_agent_policy(kind, &owner, owner.selected_suffix.first())
                .expect("internal agent policy")
        };
        // The document itself records no cap; inheritance supplies the bound.
        assert_eq!(policy.agent.max_output_tokens, 0, "{kind:?} document cap");
        assert_eq!(policy.limits.max_output_tokens, 0, "{kind:?} document cap");
        assert_eq!(
            policy.limits.inherited_max_output_tokens, 128,
            "{kind:?} inherited cap"
        );
        assert_eq!(
            internal_output_cap(&fixture, &selection, kind),
            Some(128),
            "{kind:?} effective cap"
        );
    }
}

#[test]
fn uncapped_parent_leaves_internal_agents_bounded_by_the_model_output_limit() {
    // The `custom.test/group/model` fixture declares `output_tokens = 1024`.
    let (fixture, selection) = custom_fixture_with_endpoint_and_primary_agent(
        "http://127.0.0.1:9/v1",
        "---\ndescription: Uncapped primary\nmode: primary\nenabled: true\nmodels: [{ model: \"custom.test/group/model\", variant: base }]\npermissions: {}\n---\nRespond.\n",
    );
    for kind in INTERNAL_KINDS {
        assert_eq!(
            internal_output_cap(&fixture, &selection, kind),
            Some(1_024),
            "{kind:?} effective cap"
        );
    }
}

#[test]
fn explicit_internal_output_cap_wins_over_the_inherited_parent_cap() {
    let (fixture, selection) = custom_fixture_with_endpoint_primary_and_internal(
        "http://127.0.0.1:9/v1",
        "---\ndescription: Output-capped primary\nmode: primary\nenabled: true\nmodels: [{ model: \"custom.test/group/model\", variant: base }]\nlimits: { max_output_tokens: 128 }\npermissions: {}\n---\nKeep the response bounded.\n",
        Some((
            "approval.md",
            "---\ndescription: Tightly capped approval\nmode: internal\nenabled: true\nmodels: [{ model: \"${parent_model}\" }]\nlimits: { max_output_tokens: 64 }\npermissions: {}\n---\nEvaluate approvals.\n",
        )),
        None,
        false,
    );
    assert_eq!(
        internal_output_cap(&fixture, &selection, InternalAgentKind::Approval),
        Some(64)
    );
    // The sibling built-ins still inherit the parent's cap.
    assert_eq!(
        internal_output_cap(&fixture, &selection, InternalAgentKind::SessionTitle),
        Some(128)
    );
}

#[test]
fn explicit_internal_output_cap_is_still_clamped_to_the_model_output_limit() {
    let (fixture, selection) = custom_fixture_with_endpoint_primary_and_internal(
        "http://127.0.0.1:9/v1",
        "---\ndescription: Output-capped primary\nmode: primary\nenabled: true\nmodels: [{ model: \"custom.test/group/model\", variant: base }]\nlimits: { max_output_tokens: 128 }\npermissions: {}\n---\nKeep the response bounded.\n",
        Some((
            "approval.md",
            "---\ndescription: Loosely capped approval\nmode: internal\nenabled: true\nmodels: [{ model: \"${parent_model}\" }]\nlimits: { max_output_tokens: 8192 }\npermissions: {}\n---\nEvaluate approvals.\n",
        )),
        None,
        false,
    );
    assert_eq!(
        internal_output_cap(&fixture, &selection, InternalAgentKind::Approval),
        Some(1_024)
    );
}
