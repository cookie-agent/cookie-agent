use std::sync::Arc;

use sha2::{Digest as _, Sha256};

use super::{OnboardingRequest, OnboardingRequirement};
use crate::{
    EngineError,
    runtime::{
        official_skills::tests::{Routes, serve},
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
            .map(|model| model.model.to_string()),
        Some("custom.test/z-model".into())
    );
    fixture.engine.shutdown().await;
}
