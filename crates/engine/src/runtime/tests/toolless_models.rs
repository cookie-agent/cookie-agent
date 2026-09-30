//! Models without tool calling are unavailable to agents that publish tools
//! and stay usable by agents (and internal agents) that publish none.

use std::sync::Arc;

use cookie_agent_models::{ModelManager, ProviderDefinition, provider_store::ProviderStore};
use cookie_agent_protocol::{
    AgentId, ClientRunId, EventPayload, ModelKey, ModelSelection, PermissionAction,
    PermissionEffect, ProviderId, ProviderModelId, RunSelection, RunStartParams, SessionStatus,
    WildcardPattern,
};

use crate::{Engine, EngineError, EngineOptions};

use super::support::*;

const PLAIN_MODEL: &str = r#"
[providers."custom.test".models."plain"]
display_name = "Plain"
capabilities = { input = ["text"], output = ["text"], context_tokens = 4096, output_tokens = 1024, tool_calling = false, parallel_tool_calls = false, structured_output = false, reasoning = false, temperature = true, top_p = true, seed = true, native_replay = "unsupported", media = {} }
"#;

const TOOLLESS_CAPABILITIES: &str = "input = [\"text\"]\noutput = [\"text\"]\ncontext_tokens = 8192\noutput_tokens = 1024\ntool_calling = false\nparallel_tool_calls = false\nstructured_output = false\nreasoning = false\ntemperature = true\ntop_p = true\nseed = true\nnative_replay = \"unsupported\"\nmedia = {}";

fn model(key: &str) -> ModelKey {
    key.parse().expect("model key")
}

fn selection(key: &str) -> RunSelection {
    RunSelection {
        agent: AgentId::new("primary").expect("agent ID"),
        model: ModelSelection {
            model: model(key),
            variant: None,
        },
        preset: None,
    }
}

fn agent_document(permissions: &str, models: &[&str]) -> String {
    let models = models
        .iter()
        .map(|model| format!("{{ model: \"{model}\", variant: base }}"))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "---\ndescription: Tool-calling test agent\nmode: primary\nenabled: true\nmodels: [{models}]\npermissions:{permissions}\n---\nTest tool calling.\n"
    )
}

fn plain_fixture(permissions: &str, models: &[&str]) -> Fixture {
    synthetic_default_fixture_with_config(
        Some(&agent_document(permissions, models)),
        "http://127.0.0.1:9/v1",
        PLAIN_MODEL,
    )
    .expect("engine")
}

fn primary_descriptor(fixture: &Fixture) -> cookie_agent_protocol::AgentDescriptor {
    fixture
        .engine
        .runtime_snapshot()
        .expect("runtime")
        .snapshot
        .agents
        .into_iter()
        .find(|agent| agent.id.as_str() == "primary")
        .expect("primary agent")
}

#[test]
fn tool_agents_reject_models_without_tool_calling_by_name() {
    let fixture = plain_fixture(
        "\n  read: allow",
        &[
            "custom.test/a-model",
            "custom.test/plain",
            "custom.test/z-model",
        ],
    );
    let descriptor = primary_descriptor(&fixture);
    assert!(descriptor.publishes_tools);
    assert!(descriptor.runnable_as_root);

    let error = fixture
        .engine
        .create_session(selection("custom.test/plain"))
        .expect_err("a tool agent cannot start on a model without tool calling");
    assert!(matches!(error, EngineError::ModelWithoutToolCalling { .. }));
    assert_eq!(
        error.to_string(),
        "model `custom.test/plain` is not available: no tool calling: agent `primary` uses tools"
    );

    // The fallback chain skips the tool-less model between the other two.
    let policy = frozen_root_policy(&fixture, &selection("custom.test/a-model"));
    assert_eq!(
        policy
            .selected_suffix
            .iter()
            .map(|binding| binding.selection.model.to_string())
            .collect::<Vec<_>>(),
        ["custom.test/a-model", "custom.test/z-model"]
    );

    // Best-effort repair of a remembered tool-less selection falls back to
    // the agent's first model it can run.
    let repaired = crate::policy::best_effort_root_selection(
        &fixture.engine.current_runtime(),
        &selection("custom.test/plain"),
    )
    .expect("repaired selection");
    assert_eq!(repaired.model.model, model("custom.test/a-model"));
}

#[test]
fn tool_agent_with_only_toolless_models_is_not_root_runnable() {
    let fixture = plain_fixture("\n  bash: ask", &["custom.test/plain"]);
    let descriptor = primary_descriptor(&fixture);
    assert!(descriptor.publishes_tools);
    assert!(!descriptor.runnable_as_root);
}

#[test]
fn agents_without_tools_run_models_without_tool_calling() {
    for permissions in [" {}", "\n  read: deny"] {
        let fixture = plain_fixture(permissions, &["custom.test/plain", "custom.test/a-model"]);
        let descriptor = primary_descriptor(&fixture);
        assert!(!descriptor.publishes_tools, "{permissions}");
        assert!(descriptor.runnable_as_root, "{permissions}");
        let policy = frozen_root_policy(&fixture, &selection("custom.test/plain"));
        assert_eq!(
            policy
                .selected_suffix
                .iter()
                .map(|binding| binding.selection.model.to_string())
                .collect::<Vec<_>>(),
            ["custom.test/plain", "custom.test/a-model"],
            "{permissions}"
        );
        fixture
            .engine
            .create_session(selection("custom.test/plain"))
            .expect("a tool-less agent starts on a model without tool calling");
    }
}

#[tokio::test]
async fn session_permission_tools_reject_a_toolless_head_at_admission() {
    let fixture = plain_fixture(" {}", &["custom.test/plain"]);
    fixture
        .engine
        .register_tool_provider(Arc::new(TestParallelToolProvider {
            state: Arc::new(ParallelToolState::default()),
            barrier: None,
        }));
    let session = fixture
        .engine
        .create_session(selection("custom.test/plain"))
        .expect("session");
    fixture
        .engine
        .set_session_permission(
            session.session_id,
            PermissionAction::Read,
            WildcardPattern::new("*").expect("pattern"),
            PermissionEffect::Allow,
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("session permission");
    let error = fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: session.session_id,
                client_run_id: ClientRunId::new("overlay-tools").unwrap(),
                selection: selection("custom.test/plain"),
                input: "hello".into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect_err("session tools need tool calling");
    assert!(
        matches!(error, EngineError::ModelWithoutToolCalling { .. }),
        "{error}"
    );
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn title_agent_runs_on_a_model_without_tool_calling() {
    let body = scripted_text_body("Plain title");
    let (endpoint, captured, _, _) =
        scripted_server_with_delayed_response(vec![body.clone(), body], usize::MAX).await;
    let (fixture, selection) = custom_fixture_with_capabilities(
        &endpoint,
        "---\ndescription: Tool-less agent\nmode: primary\nenabled: true\nmodels: [{ model: \"custom.test/group/model\", variant: base }]\npermissions: {}\n---\nAnswer without tools.\n",
        None,
        None,
        true,
        None,
        None,
        8192,
        None,
        "openai-compatible",
        Some(TOOLLESS_CAPABILITIES),
    );
    let session = fixture.engine.create_session(selection.clone()).unwrap();
    fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: session.session_id,
                client_run_id: ClientRunId::new("toolless-title").unwrap(),
                selection,
                input: "Name this session".into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("a tool-less agent runs a model without tool calling");
    await_projection(
        &fixture.engine,
        session.session_id,
        "tool-less generated title",
        |session| {
            session.status == SessionStatus::Completed
                && session.log.events().iter().any(|event| {
                    matches!(
                        &event.payload,
                        EventPayload::SessionTitleCommitted {
                            change: cookie_agent_protocol::SessionTitleChange::InternalAgentSet {
                                title,
                                ..
                            },
                            ..
                        } if title.as_str() == "Plain title"
                    )
                })
        },
    )
    .await;
    let requests = with_watchdog("captured requests", captured).await.unwrap();
    assert_eq!(requests.len(), 2);
    assert!(
        requests
            .iter()
            .all(|request| !request.contains("\"tools\""))
    );
    fixture.engine.shutdown().await;
}

/// Makes `custom.test/group/fallback` a model without tool calling.
async fn make_fallback_toolless(fixture: &mut Fixture) {
    fixture.engine.shutdown().await;
    let provider_id = ProviderId::new("custom.test").expect("provider ID");
    let ProviderDefinition::Custom(provider) = fixture
        .config
        .runtime
        .providers
        .get_mut(&provider_id)
        .expect("custom provider")
    else {
        panic!("custom provider");
    };
    let fallback = provider
        .models
        .get_mut(&ProviderModelId::new("group/fallback").expect("model ID"))
        .expect("fallback model");
    fallback.capabilities.tool_calling = false;
    fallback.capabilities.parallel_tool_calls = false;
    let current = fixture.manager.current();
    let manager = Arc::new(
        ModelManager::new(
            fixture.config.runtime.providers.clone(),
            Arc::clone(current.catalog()),
            ProviderStore::open(fixture._directory.path().join("provider-store"))
                .expect("provider store"),
        )
        .expect("model manager"),
    );
    fixture.engine = Engine::open(EngineOptions {
        data_dir: fixture._directory.path().join("data"),
        cwd: fixture._directory.path().to_owned(),
        config: fixture.config.clone(),
        model_manager: Arc::clone(&manager),
        tools: Vec::new(),
    })
    .expect("engine");
    fixture.manager = manager;
}

#[tokio::test]
async fn compaction_on_a_model_without_tool_calling_drops_the_session_tools() {
    let root = scripted_text_usage_body("root", 1_024, Some(1), 0);
    let summary = scripted_text_usage_body("toolless checkpoint", 1, Some(1), 0);
    let (endpoint, captured, ..) =
        scripted_server_with_status_and_delay(vec![(200, root), (200, summary)], usize::MAX).await;
    let (mut fixture, selection) =
        custom_fixture_with_endpoint_primary_internal_concurrency_and_context(
            &endpoint,
            "---\ndescription: Tool agent\nmode: primary\nenabled: true\nmodels: [{ model: \"custom.test/group/model\", variant: base }]\npermissions:\n  read: allow\n---\nUse tools.\n",
            Some((
                "compaction.md",
                "---\ndescription: compaction\nmode: internal\nenabled: true\nmodels: [{ model: \"custom.test/group/fallback\", variant: base }]\nlimits: { timeout_ms: 30000, max_output_tokens: 256 }\npermissions: {}\n---\nSummarize.\n",
            )),
            None,
            false,
            None,
            None,
            100_000,
            None,
        );
    make_fallback_toolless(&mut fixture).await;
    fixture
        .engine
        .register_tool_provider(Arc::new(TestParallelToolProvider {
            state: Arc::new(ParallelToolState::default()),
            barrier: None,
        }));
    let session = fixture.engine.create_session(selection.clone()).unwrap();
    fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: session.session_id,
                client_run_id: ClientRunId::new("toolless-compaction").unwrap(),
                selection,
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
        .expect("compaction on a model without tool calling");
    let requests = with_watchdog("captured requests", captured).await.unwrap();
    assert_eq!(requests.len(), 2);
    assert!(
        requests[0].contains("\"tools\""),
        "the root run sends its tools"
    );
    assert!(requests[1].contains("Summarize."));
    assert!(
        !requests[1].contains("\"tools\""),
        "the tool-less summarizer gets no tool definitions"
    );
    assert!(
        fixture
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
                EventPayload::ContextCheckpointCommitted { .. }
            ))
    );
    fixture.engine.shutdown().await;
}
