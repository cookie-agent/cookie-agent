use std::sync::Arc;

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
        owner.result_limits,
        owner.model_retry,
        owner.cache_layer,
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
/// The title agent sends no rolling marker, compaction shares the owner's,
/// and the approval reviewer's follows `[approval] cache_ttl` (approval
/// reviewer spec section 5), read at each call.
async fn internal_agent_cache_strategy_by_kind_and_the_reviewer_ttl() {
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
    // Compaction shares the owner's cached prefix, rolling marker included.
    let owner_rolling = match anthropic_ttls(&owner, parent).2 {
        Some(cookie_agent_models::adapters::AnthropicCacheTtlConfig::OneHour) => "one_hour",
        Some(cookie_agent_models::adapters::AnthropicCacheTtlConfig::FiveMinutes) => "five_minutes",
        None => panic!("owner rolling marker"),
    };
    let marker = |options: &oven_sdk::ProviderOptions| {
        options
            .get("anthropic")
            .and_then(|value| value.get("cache_control"))
            .and_then(|value| value.get("ttl"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
    };

    for (kind, ttl, expected) in [
        (
            InternalAgentKind::SessionTitle,
            cookie_agent_config::ApprovalCacheTtl::OneHour,
            (Some("one_hour"), Some("one_hour"), None),
        ),
        (
            InternalAgentKind::ContextCompaction,
            cookie_agent_config::ApprovalCacheTtl::Off,
            (Some("one_hour"), Some("one_hour"), Some(owner_rolling)),
        ),
        (
            InternalAgentKind::Approval,
            cookie_agent_config::ApprovalCacheTtl::OneHour,
            (Some("one_hour"), Some("one_hour"), Some("one_hour")),
        ),
        (
            InternalAgentKind::Approval,
            cookie_agent_config::ApprovalCacheTtl::FiveMinutes,
            (Some("one_hour"), Some("one_hour"), Some("five_minutes")),
        ),
        (
            InternalAgentKind::Approval,
            cookie_agent_config::ApprovalCacheTtl::Provider,
            (Some("one_hour"), Some("one_hour"), Some(owner_rolling)),
        ),
        (
            InternalAgentKind::Approval,
            cookie_agent_config::ApprovalCacheTtl::Off,
            (None, None, None),
        ),
    ] {
        let mut config = (*fixture.engine.inner.config.load_full()).clone();
        config.runtime.approval.cache_ttl = ttl;
        fixture
            .engine
            .inner
            .config
            .store(std::sync::Arc::new(config));
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
            (
                marker(&system.provider_options).as_deref(),
                marker(&prepared.tools[0].provider_options).as_deref(),
                marker(&user.provider_options).as_deref(),
            ),
            expected,
            "{kind:?} with {ttl:?}"
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
fn twelve_hour_openai_cache_ttl_applies_only_to_responses() {
    let primary = "---\ndescription: Cached owner\nmode: primary\nenabled: true\nmodels:\n  - model: custom.test/group/model\n    variant: base\n    cache:\n      openai:\n        ttl: 12h\n        system: true\npermissions: {}\n---\nStable owner prompt.\n";
    for adaptor in ["openai-responses", "openai-chat"] {
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
                adaptor,
            );
        if adaptor == "openai-responses" {
            let policy = frozen_root_policy(&fixture, &selection);
            let binding = policy.selected_suffix.first().unwrap();
            let Some(cookie_agent_models::adapters::CacheStrategyConfig::OpenAi(strategy)) =
                policy.cache_strategy(binding, SessionId::new_v7())
            else {
                panic!("OpenAI cache strategy");
            };
            assert_eq!(
                strategy.ttl,
                Some(cookie_agent_models::adapters::OpenAiPromptCacheTtl::TwelveHours)
            );
        } else {
            let Err(EngineError::CacheStrategy(message)) =
                try_frozen_root_policy(&fixture, &selection)
            else {
                panic!("12h on Chat Completions must be a cache strategy error");
            };
            assert!(message.contains("Responses"), "{message}");
        }
    }
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

type AnthropicTtls = (
    Option<cookie_agent_models::adapters::AnthropicCacheTtlConfig>,
    Option<cookie_agent_models::adapters::AnthropicCacheTtlConfig>,
    Option<cookie_agent_models::adapters::AnthropicCacheTtlConfig>,
);

/// The (system, tools, rolling) TTLs `policy` frozen for `binding`.
fn anthropic_ttls(
    policy: &crate::policy::FrozenRunPolicy,
    binding: &cookie_agent_protocol::FrozenModelBinding,
) -> AnthropicTtls {
    let Some(cookie_agent_models::adapters::CacheStrategyConfig::Anthropic(strategy)) =
        policy.raw_cache_strategy(binding)
    else {
        panic!("Anthropic cache strategy");
    };
    (strategy.system, strategy.tools, strategy.rolling)
}

/// The root policy of `selection` and the policy of a model-less `worker`
/// child delegated from it, which inherits the root's models.
fn root_and_delegated_policies(
    fixture: &Fixture,
    selection: &RunSelection,
) -> (
    crate::policy::FrozenRunPolicy,
    crate::policy::FrozenRunPolicy,
) {
    let root = frozen_root_policy(fixture, selection);
    let worker = root
        .registry
        .get(&cookie_agent_protocol::AgentId::new("worker").unwrap())
        .expect("worker agent")
        .clone();
    let child = crate::runtime::delegation::freeze_delegated_child_policy(
        &worker,
        &root,
        &selection.model,
        &root.selected_suffix,
        1,
        root.result_limits,
        &crate::permissions::MemoryVariables::default(),
    )
    .expect("delegated policy");
    (root, child)
}

fn anthropic_layer_fixture(primary: &str, provider_cache: Option<&str>) -> (Fixture, RunSelection) {
    let worker = "---\ndescription: Inheriting worker\nmode: subagent\nenabled: true\nmodels: []\npermissions: {}\n---\nWorker prompt.\n";
    custom_fixture_with_capabilities_and_worker_name(
        "http://127.0.0.1:9/v1",
        primary,
        None,
        None,
        false,
        None,
        None,
        4_096,
        Some(worker),
        "anthropic-compatible",
        None,
        None,
        provider_cache,
        "worker",
    )
}

const LAYER_PRIMARY: &str = "---\ndescription: Layered cache owner\nmode: primary\nenabled: true\nmodels: [{ model: \"custom.test/group/model\", variant: base }]\npermissions:\n  delegate:\n    worker: allow\n    \"*\": deny\n---\nStable owner prompt.\n";

#[test]
fn rolling_cache_default_follows_the_session_layer() {
    use cookie_agent_models::adapters::AnthropicCacheTtlConfig::{FiveMinutes, OneHour};
    let (fixture, selection) = anthropic_layer_fixture(LAYER_PRIMARY, None);
    let (root, child) = root_and_delegated_policies(&fixture, &selection);
    assert_eq!(root.cache_layer, cookie_agent_config::CacheLayer::Root);
    assert_eq!(
        child.cache_layer,
        cookie_agent_config::CacheLayer::Delegated
    );
    let binding = root.selected_suffix.first().unwrap();
    let (root_system, root_tools, root_rolling) = anthropic_ttls(&root, binding);
    let (child_system, child_tools, child_rolling) = anthropic_ttls(&child, binding);
    assert_eq!(root_rolling, Some(OneHour));
    assert_eq!(child_rolling, Some(FiveMinutes));
    assert_eq!((root_system, root_tools), (child_system, child_tools));
}

#[test]
fn authored_cache_values_apply_to_both_layers() {
    use cookie_agent_models::adapters::AnthropicCacheTtlConfig::{FiveMinutes, OneHour};
    for (provider_cache, expected) in [
        (
            "rolling = \"5m\"",
            (Some(OneHour), Some(OneHour), Some(FiveMinutes)),
        ),
        (
            "rolling = \"1h\"",
            (Some(OneHour), Some(OneHour), Some(OneHour)),
        ),
        (
            "system = \"5m\"\nrolling = \"off\"",
            (Some(FiveMinutes), Some(OneHour), None),
        ),
    ] {
        let (fixture, selection) = anthropic_layer_fixture(LAYER_PRIMARY, Some(provider_cache));
        let (root, child) = root_and_delegated_policies(&fixture, &selection);
        let binding = root.selected_suffix.first().unwrap();
        assert_eq!(anthropic_ttls(&root, binding), expected, "{provider_cache}");
        assert_eq!(
            anthropic_ttls(&child, binding),
            expected,
            "{provider_cache}"
        );
    }

    // An agent's own entry travels with the models a model-less child
    // inherits.
    let primary = "---\ndescription: Layered cache owner\nmode: primary\nenabled: true\nmodels:\n  - model: custom.test/group/model\n    variant: base\n    cache:\n      anthropic: { system: 5m, rolling: 5m }\npermissions:\n  delegate:\n    worker: allow\n    \"*\": deny\n---\nStable owner prompt.\n";
    let (fixture, selection) = anthropic_layer_fixture(primary, Some("rolling = \"1h\""));
    let (root, child) = root_and_delegated_policies(&fixture, &selection);
    let binding = root.selected_suffix.first().unwrap();
    let expected = (Some(FiveMinutes), Some(OneHour), Some(FiveMinutes));
    assert_eq!(anthropic_ttls(&root, binding), expected);
    assert_eq!(anthropic_ttls(&child, binding), expected);
}

#[test]
fn anthropic_one_hour_markers_need_no_beta_option() {
    use cookie_agent_models::adapters::AnthropicCacheTtlConfig::OneHour;
    let primary = "---\ndescription: Anthropic cache owner\nmode: primary\nenabled: true\nmodels:\n  - model: custom.test/group/model\n    variant: base\n    cache:\n      anthropic: { system: 1h, tools: off, rolling: 1h }\npermissions: {}\n---\nStable owner prompt.\n";
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
    let policy = try_frozen_root_policy(&fixture, &selection).expect("policy without beta");
    let binding = policy.selected_suffix.first().unwrap();
    assert!(!matches!(
        &binding.options,
        cookie_agent_protocol::ProviderOptions::Anthropic { beta, .. } if !beta.is_empty()
    ));
    assert_eq!(
        anthropic_ttls(&policy, binding),
        (Some(OneHour), None, Some(OneHour))
    );
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
    fixture.engine.register_tool_provider(
        crate::ToolLayer::Base,
        Arc::new(TestDelegateProvider {
            engine: fixture.engine.clone(),
        }),
    );
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
                content: "delegate this task".into(),
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
    let events = events
        .into_iter()
        .map(std::sync::Arc::new)
        .collect::<Vec<_>>();
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
                content: "prime".into(),
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
    let model_capabilities = r#"{"producer_messaging":false,"subscribe_events":[],"subscribe_bus":false,"publish_bus":false,"intercept":[{"hook":"model_before_request"}]}"#;
    let replacement = serde_json::json!({
        "action": "replace",
        "messages": [
            {"role": "system", "content": [{"type": "text", "text": "Replacement system"}]},
            {"role": "user", "content": [{"type": "text", "text": "Replacement user"}]}
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
        ],
    )
    .await;
    fixture.engine.register_tool_provider(
        crate::ToolLayer::Local,
        Arc::new(TestWriteProvider {
            executed: Arc::new(TestFlag::default()),
        }),
    );
    let session = fixture.engine.create_session(selection.clone()).unwrap();
    fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: session.session_id,
                client_run_id: ClientRunId::new("intercepted-cache").unwrap(),
                selection,
                content: "original user".into(),
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
    // A root run's rolling marker.
    assert_eq!(last_content["cache_control"]["ttl"], "1h");
    assert_eq!(cache_marker_count(&body), 3);

    await_projection(
        &fixture.engine,
        session.session_id,
        "plugin diagnostics",
        |projection| {
            projection.log.events().iter().any(|event| {
                matches!(
                    &event.payload,
                    EventPayload::Diagnostic { kind, message, .. }
                        if *kind == cookie_agent_protocol::PluginDiagnosticKind::InvalidModification
                            && message.contains("replace requires messages")
                )
            })
        },
    )
    .await;
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
                content: "respond briefly".into(),
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

/// Configuration status notes a reviewer TTL its model's provider does not
/// offer, and one that changes nothing there (approval reviewer spec
/// section 5).
#[tokio::test]
async fn configuration_status_notes_what_the_reviewer_ttl_does() {
    let primary = "---\ndescription: Owner\nmode: primary\nenabled: true\nmodels: [{ model: \"custom.test/group/model\", variant: base }]\npermissions: {}\n---\nOwner.\n";
    let approval = "---\ndescription: Reviewer\nmode: internal\nenabled: true\nmodels: [{ model: \"custom.test/group/model\" }]\npermissions: {}\n---\n{{default_prompt}}\n";
    for (adaptor, ttl, expected) in [
        (
            "anthropic-compatible",
            cookie_agent_config::ApprovalCacheTtl::ThirtyMinutes,
            Some(
                "`[approval] cache_ttl = \"30m\"`: the reviewer's model `custom.test/group/model` caches for 1h, the nearest TTL its provider offers",
            ),
        ),
        (
            "anthropic-compatible",
            cookie_agent_config::ApprovalCacheTtl::OneHour,
            None,
        ),
        (
            "openai-compatible",
            cookie_agent_config::ApprovalCacheTtl::OneHour,
            Some(
                "`[approval] cache_ttl = \"1h\"` has no effect on the reviewer's model `custom.test/group/model`: its provider manages caching itself",
            ),
        ),
    ] {
        let (fixture, _) =
            custom_fixture_with_endpoint_primary_internal_concurrency_context_and_adaptor(
                "http://127.0.0.1:9/v1",
                primary,
                Some(("approval.md", approval)),
                None,
                false,
                None,
                None,
                4_096,
                None,
                adaptor,
            );
        let mut config = (*fixture.engine.inner.config.load_full()).clone();
        config.runtime.approval.cache_ttl = ttl;
        fixture
            .engine
            .inner
            .config
            .store(std::sync::Arc::new(config));
        let diagnostics = fixture.engine.configuration_status().diagnostics;
        let notes = diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.contains("[approval] cache_ttl"))
            .map(String::as_str)
            .collect::<Vec<_>>();
        assert_eq!(
            notes,
            expected.into_iter().collect::<Vec<_>>(),
            "{adaptor} {ttl:?}"
        );
        fixture.engine.shutdown().await;
    }
}
