use std::sync::Arc;

use sha2::{Digest as _, Sha256};

use super::{OnboardingRequest, OnboardingRequirement};
use crate::{
    EngineError,
    runtime::{
        official::skills::tests::{Routes, serve},
        tests::support::{
            Fixture, create_private_test_dir, synthetic_default_fixture_with_config,
            write_private_test_file,
        },
    },
};

const PRIMARY: &[u8] =
    b"---\ndescription: The assistant\nmode: primary\nmodels:\n  - ${default_model}\n---\nHelp.\n";

/// The synthetic fixture: provider `custom.test` with `a-model` and
/// `z-model` in its `config.toml`, and no agents.
fn fixture() -> Fixture {
    synthetic_default_fixture_with_config(None, "http://127.0.0.1:9/v1", "").expect("engine")
}

fn root(fixture: &Fixture) -> std::path::PathBuf {
    fixture._directory.path().join(".cookie-agent")
}

fn ready_request() -> OnboardingRequest {
    OnboardingRequest {
        default_model: Some(
            serde_json::json!({"model": "custom.test/a-model", "variant": "alpha"}),
        ),
        small_model: None,
        utility_model: Some(serde_json::json!("custom.test/z-model")),
        ..OnboardingRequest::default()
    }
}

/// Serves the official agents: `primary` alone.
async fn official_agents(fixture: &Fixture) {
    let routes = Routes::default();
    let url = serve(Arc::clone(&routes)).await;
    {
        let mut routes = routes.lock().unwrap();
        routes.insert("/skills/primary.md".into(), PRIMARY.to_vec());
        routes.insert(
            "/skills/index.json".into(),
            serde_json::to_vec(&serde_json::json!({"version": 1, "agents": [{
                "id": "primary", "version": "1.0.0", "requires_cookie": ">=0.1.0",
                "sha256": format!("{:x}", Sha256::digest(PRIMARY)),
            }]}))
            .unwrap(),
        );
    }
    fixture.engine.set_official_agents_url(url);
}

#[tokio::test]
async fn readiness_follows_the_configuration_the_providers_and_the_default_agent() {
    let fixture = fixture();
    let state = fixture.engine.onboarding_state();
    assert_eq!(
        state.missing,
        [
            OnboardingRequirement::DefaultModel,
            OnboardingRequirement::DefaultAgent
        ],
        "the optional small and utility models are never required"
    );
    assert_eq!(state.providers_connected.len(), 1);

    // Set up by hand: config.toml edited and an agent written count the same.
    let config = root(&fixture).join("config.toml");
    let text = std::fs::read_to_string(&config).expect("config");
    write_private_test_file(
        &config,
        format!("default_model = \"custom.test/a-model\"\n{text}"),
    );
    let agents = root(&fixture).join("agents");
    create_private_test_dir(&agents);
    write_private_test_file(&agents.join("primary.md"), PRIMARY);
    fixture.engine.reload_configuration().await.expect("reload");
    let state = fixture.engine.onboarding_state();
    assert!(state.ready(), "{state:?}");

    // A default agent that names no document.
    write_private_test_file(
        &config,
        format!("default_model = \"custom.test/a-model\"\n[agents]\ndefault = \"nobody\"\n{text}"),
    );
    fixture.engine.reload_configuration().await.expect("reload");
    assert_eq!(
        fixture.engine.onboarding_state().missing,
        [OnboardingRequirement::DefaultAgent]
    );

    // A model that is gone, then no provider at all.
    write_private_test_file(
        &config,
        format!("default_model = \"custom.test/gone\"\n{text}"),
    );
    fixture.engine.reload_configuration().await.expect("reload");
    assert_eq!(
        fixture.engine.onboarding_state().missing,
        [OnboardingRequirement::DefaultModel]
    );
    write_private_test_file(&config, "default_model = \"custom.test/a-model\"\n");
    fixture.engine.reload_configuration().await.expect("reload");
    assert_eq!(
        fixture.engine.onboarding_state().missing,
        [
            OnboardingRequirement::Provider,
            OnboardingRequirement::DefaultModel
        ]
    );
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn a_run_that_would_not_be_ready_writes_nothing() {
    let fixture = fixture();
    official_agents(&fixture).await;
    let before = std::fs::read(root(&fixture).join("config.toml")).expect("config");
    let unavailable = OnboardingRequest {
        default_model: Some(serde_json::json!("custom.test/gone")),
        ..ready_request()
    };
    let error = fixture
        .engine
        .run_onboarding(unavailable)
        .await
        .expect_err("the default model is not available");
    assert!(matches!(
        error,
        EngineError::OnboardingIncomplete(ref missing)
            if missing == &[OnboardingRequirement::DefaultModel]
    ));
    assert_eq!(
        std::fs::read(root(&fixture).join("config.toml")).expect("config"),
        before
    );
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn a_default_model_list_is_ready_when_any_of_its_models_is_available() {
    let fixture = fixture();
    official_agents(&fixture).await;
    let outcome = fixture
        .engine
        .run_onboarding(OnboardingRequest {
            default_model: Some(serde_json::json!([
                "custom.test/gone",
                "custom.test/a-model"
            ])),
            small_model: Some(serde_json::json!("${default_model}")),
            utility_model: None,
            ..OnboardingRequest::default()
        })
        .await
        .expect("a later model is available");
    assert!(outcome.state.ready());
    let default = outcome.state.default_model.expect("default_model");
    assert_eq!(default.configured.entries().len(), 2);
    assert_eq!(
        default.first.map(|(model, _)| model.to_string()),
        Some("custom.test/gone".into())
    );
    // `small_model` reports what it reaches through `${default_model}`.
    assert_eq!(
        outcome
            .state
            .small_model
            .and_then(|field| field.first)
            .map(|(model, _)| model.to_string()),
        Some("custom.test/gone".into())
    );

    let before = std::fs::read(root(&fixture).join("config.toml")).expect("config");
    let error = fixture
        .engine
        .run_onboarding(OnboardingRequest {
            default_model: Some(serde_json::json!(["${small_model}", "custom.test/a-model"])),
            ..OnboardingRequest::default()
        })
        .await
        .expect_err("a cycle");
    assert!(error.to_string().contains("cycle"), "{error}");
    assert_eq!(
        std::fs::read(root(&fixture).join("config.toml")).expect("config"),
        before
    );
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn a_run_without_official_agents_reachable_is_refused() {
    let fixture = fixture();
    // An address nothing listens on.
    fixture
        .engine
        .set_official_agents_url("http://127.0.0.1:9/agents");
    let error = fixture
        .engine
        .run_onboarding(ready_request())
        .await
        .expect_err("no default agent");
    assert!(matches!(
        error,
        EngineError::OnboardingIncomplete(ref missing)
            if missing == &[OnboardingRequirement::DefaultAgent]
    ));
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn a_run_fetches_the_official_agents_and_sets_the_models() {
    let fixture = fixture();
    official_agents(&fixture).await;
    let outcome = fixture
        .engine
        .run_onboarding(ready_request())
        .await
        .expect("run");
    assert!(outcome.state.ready(), "{:?}", outcome.state);
    assert_eq!(outcome.settings, ["default_model", "utility_model"]);
    assert!(root(&fixture).join("official_agents/primary.md").exists());
    assert!(
        !root(&fixture).join("agents").exists(),
        "onboarding writes no agent document"
    );
    let config = std::fs::read_to_string(root(&fixture).join("config.toml")).expect("config");
    assert!(config.contains("utility_model"), "{config}");
    assert!(!config.contains("small_model"), "{config}");
    let state = fixture.engine.onboarding_state();
    assert_eq!(
        state
            .utility_model
            .as_ref()
            .and_then(|field| field.first.as_ref())
            .map(|(model, _)| model.to_string()),
        Some("custom.test/z-model".into())
    );
    fixture.engine.shutdown().await;
}

/// The `config.toml` of a fixture without providers: only what `extra`
/// says.
fn no_provider_fixture(extra: &str) -> Fixture {
    let fixture = fixture();
    let config = root(&fixture).join("config.toml");
    write_private_test_file(&config, extra);
    fixture
}

#[tokio::test]
async fn an_acp_agent_alone_sets_the_daemon_up() {
    let fixture = no_provider_fixture("");
    fixture.engine.reload_configuration().await.expect("reload");
    let state = fixture.engine.onboarding_state();
    assert!(!state.ready());
    assert!(!state.models_complete());
    assert!(!state.acp_complete());
    assert_eq!(
        state.models_missing,
        [
            OnboardingRequirement::Provider,
            OnboardingRequirement::DefaultModel
        ]
    );

    // An ACP agent on the daemon's own machine completes the ACP path, and
    // what only the models path needs is no longer missing.
    write_private_test_file(
        &root(&fixture).join("config.toml"),
        "[runtimes.local.acp.agents.claude]\ncommand = [\"npx\", \"-y\", \"claude-agent-acp\"]\nenv = { TOKEN = \"secret\" }\n",
    );
    fixture.engine.reload_configuration().await.expect("reload");
    let state = fixture.engine.onboarding_state();
    assert!(state.acp_complete(), "{state:?}");
    assert!(state.ready(), "{state:?}");
    assert_eq!(state.acp_agents.len(), 1);
    let claude = &state.acp_agents[0];
    assert_eq!(claude.id.as_str(), "claude");
    assert_eq!(claude.runtime.as_ref().map(|id| id.as_str()), Some("local"));
    assert_eq!(claude.env_keys, ["TOKEN"], "names, never values");

    // Turned off, it no longer counts.
    write_private_test_file(
        &root(&fixture).join("config.toml"),
        "[runtimes.local.acp.agents.claude]\ncommand = [\"npx\"]\nenabled = false\n",
    );
    fixture.engine.reload_configuration().await.expect("reload");
    assert!(!fixture.engine.onboarding_state().ready());
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn a_run_declares_acp_agents_adds_mcp_servers_and_sets_the_default_agent() {
    let fixture = no_provider_fixture("");
    official_agents(&fixture).await;
    fixture.engine.reload_configuration().await.expect("reload");
    let outcome = fixture
        .engine
        .run_onboarding(OnboardingRequest {
            acp_agents: vec![super::OnboardingAcpRequest {
                id: "claude".parse().unwrap(),
                runtime: Some("local".parse().unwrap()),
                command: vec!["npx".into(), "-y".into(), "claude-agent-acp".into()],
                env: Default::default(),
                description: Some("Claude Code".into()),
                model: Some("opus".into()),
                effort: Some("high".into()),
            }],
            mcp_servers: vec![super::OnboardingMcpRequest {
                name: "exa".into(),
                runtime: None,
                server: serde_json::json!({"url": "https://mcp.exa.ai/mcp"})
                    .as_object()
                    .unwrap()
                    .clone(),
            }],
            default_agent: Some("claude".parse().unwrap()),
            ..OnboardingRequest::default()
        })
        .await
        .expect("set up");
    assert!(outcome.state.ready(), "{:?}", outcome.state);
    assert_eq!(
        outcome.settings,
        [
            "runtimes.local.acp.agents.claude",
            "agents/claude.md",
            "mcp.servers.exa",
            "agents.default"
        ]
    );
    let text = std::fs::read_to_string(root(&fixture).join("config.toml")).unwrap();
    assert!(
        text.contains("[runtimes.local.acp.agents.claude]"),
        "{text}"
    );
    assert!(text.contains("[mcp.servers.exa]"), "{text}");
    assert!(text.contains("default = \"claude\""), "{text}");
    let document = std::fs::read_to_string(root(&fixture).join("agents/claude.md")).unwrap();
    assert!(
        document.contains("inherit: ../official_agents/_acp.md"),
        "{document}"
    );
    assert!(document.contains("\"opus\""), "{document}");
    assert_eq!(outcome.state.mcp_servers, [("exa".to_owned(), None)]);

    // A server name already taken there is refused, and nothing written.
    let refused = fixture
        .engine
        .run_onboarding(OnboardingRequest {
            mcp_servers: vec![super::OnboardingMcpRequest {
                name: "exa".into(),
                runtime: None,
                server: serde_json::json!({"url": "https://mcp.exa.ai/mcp"})
                    .as_object()
                    .unwrap()
                    .clone(),
            }],
            ..OnboardingRequest::default()
        })
        .await;
    assert!(
        matches!(refused, Err(EngineError::ConfigChangeRefused(ref reason)) if reason.contains("already configured")),
        "{refused:?}"
    );
    fixture.engine.shutdown().await;
}
