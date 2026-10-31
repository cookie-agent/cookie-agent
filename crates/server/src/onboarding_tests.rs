//! Onboarding from a plain protocol client: `onboarding.get`, `.preview`,
//! `.run`, and `onboarding.changed`, with nothing of the TUI or web.

use std::{collections::BTreeMap, sync::Arc};

use cookie_agent_engine::{Engine, EngineOptions};
use cookie_agent_models::{
    ModelManager,
    catalog::{
        CatalogAgeState, CatalogAvailability, CatalogRuntimeState, CatalogSnapshot, CatalogSource,
    },
    provider_store::ProviderStore,
};
use cookie_agent_protocol::{
    CatalogRevision, OnboardingModelInput, OnboardingRequirement, OnboardingRunParams,
};
use cookie_agent_protocol_transport::ClientDelivery;
use jiff::Timestamp;

use crate::{
    Server,
    tests::{create_private_test_dir, private_tempdir},
};

const PROVIDER: &str = r#"
[providers."custom.test"]
source = "custom"
endpoint = "http://127.0.0.1:9/v1"
adaptor = "openai-compatible"
auth = { method = "no-auth-v1", values = {} }

[providers."custom.test".models."a-model"]
display_name = "A Model"
capabilities = { input = ["text"], output = ["text"], context_tokens = 4096, output_tokens = 1024, tool_calling = true, parallel_tool_calls = true, structured_output = false, reasoning = false, temperature = true, top_p = true, seed = true, native_replay = "unsupported", media = {} }
variants = { low = { }, high = { } }
default_variant = "high"

[providers."custom.test".models."z-model"]
display_name = "Z Model"
capabilities = { input = ["text"], output = ["text"], context_tokens = 4096, output_tokens = 1024, tool_calling = true, parallel_tool_calls = true, structured_output = false, reasoning = false, temperature = true, top_p = true, seed = true, native_replay = "unsupported", media = {} }
"#;

struct Fixture {
    directory: tempfile::TempDir,
    engine: Engine,
    server: Arc<Server>,
}

fn fixture() -> Fixture {
    let directory = private_tempdir();
    let root = directory.path().join(".cookie-agent");
    create_private_test_dir(&root);
    std::fs::write(root.join("config.toml"), PROVIDER).expect("config");
    let config = cookie_agent_config::load_from_user_root(Some(&root)).expect("config");
    let store = directory.path().join("provider-store");
    create_private_test_dir(&store);
    let now = Timestamp::now();
    let catalog = Arc::new(CatalogSnapshot {
        revision: CatalogRevision::new(format!("sha256:{}", "0".repeat(64))).expect("revision"),
        source: CatalogSource::Bootstrap,
        state: CatalogRuntimeState {
            availability: CatalogAvailability::Bootstrap,
            age: CatalogAgeState::Current,
            last_error: None,
        },
        validated_at: now,
        last_checked_at: now,
        etag: None,
        providers: Arc::new(BTreeMap::new()),
        canonical_models: Arc::new(BTreeMap::new()),
        quarantine: Arc::new(Vec::new()),
    });
    let manager = Arc::new(
        ModelManager::new(
            config.runtime.providers.clone(),
            catalog,
            ProviderStore::open(&store).expect("provider store"),
        )
        .expect("model manager"),
    );
    let engine = Engine::open(EngineOptions {
        data_dir: directory.path().join("data"),
        cwd: directory.path().to_owned(),
        config,
        model_manager: manager,
        runtimes: cookie_agent_runtime_api::RuntimeOptions::new(directory.path()),
    })
    .expect("engine");
    let server = Arc::new(Server::new(engine.clone()));
    Fixture {
        directory,
        engine,
        server,
    }
}

const AGENT: &[u8] =
    b"---\ndescription: Primary\nmode: primary\nmodels: [\"${default_model}\"]\n---\nWork.\n";

#[tokio::test]
async fn a_plain_client_onboards_through_the_daemon() {
    let fixture = fixture();
    // No official agents can be fetched here: the address answers nothing.
    fixture
        .engine
        .set_official_agents_url("http://127.0.0.1:9/agents");
    let client = Arc::clone(&fixture.server).connect_in_process();
    client.handshake().await.expect("handshake");
    let mut deliveries = client.subscribe_deliveries().expect("deliveries");

    let status = client.get_onboarding().await.expect("onboarding.get");
    assert!(!status.ready);
    assert_eq!(
        status.missing,
        [
            OnboardingRequirement::DefaultModel,
            OnboardingRequirement::DefaultAgent
        ]
    );
    assert_eq!(status.providers_connected[0].as_str(), "custom.test");

    // Without a default agent the daemon would not be set up: refused, and
    // nothing is written.
    let models = OnboardingRunParams {
        install_kit: false,
        source: None,
        default_model: Some(OnboardingModelInput::Key(
            "custom.test/a-model".parse().unwrap(),
        )),
        small_model: Some(OnboardingModelInput::Table(
            cookie_agent_protocol::OnboardingModelTable {
                model: "custom.test/a-model".parse().unwrap(),
                variant: Some(serde_json::from_value(serde_json::json!("low")).unwrap()),
            },
        )),
        utility_model: None,
    };
    let refused = client
        .run_onboarding(models.clone())
        .await
        .expect_err("refused");
    assert!(refused.is(&cookie_agent_protocol::errors::ONBOARDING_INCOMPLETE));
    let data = refused.error_data().expect("error data");
    assert_eq!(data.fields["missing"], serde_json::json!(["default_agent"]));
    let root = fixture.directory.path().join(".cookie-agent");
    let before = std::fs::read(root.join("config.toml")).expect("config");

    // The user's own agent is the default agent: the run sets the models.
    std::fs::create_dir_all(root.join("agents")).unwrap();
    std::fs::write(root.join("agents/primary.md"), AGENT).unwrap();
    fixture.engine.reload_configuration().await.expect("reload");
    assert_eq!(
        std::fs::read(root.join("config.toml")).expect("config"),
        before,
        "the refused run wrote nothing"
    );
    let result = client.run_onboarding(models).await.expect("onboarding.run");
    assert!(result.status.ready);
    assert!(result.written.is_empty() && result.written_new.is_empty());
    assert_eq!(result.settings, ["default_model", "small_model"]);

    let changed = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            match deliveries.recv().await {
                Some(ClientDelivery::OnboardingChanged(status)) if status.ready => {
                    return status;
                }
                Some(_) => {}
                None => panic!("deliveries ended"),
            }
        }
    })
    .await
    .expect("onboarding.changed");
    assert_eq!(
        serde_json::to_value(&changed.small_model).unwrap(),
        serde_json::json!({"model": "custom.test/a-model", "variant": "low"})
    );
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn there_is_no_starter_kit_to_preview() {
    let fixture = fixture();
    let client = Arc::clone(&fixture.server).connect_in_process();
    client.handshake().await.expect("handshake");
    let error = client.preview_onboarding(None).await.expect_err("refused");
    assert!(error.is(&cookie_agent_protocol::errors::ONBOARDING_KIT_UNAVAILABLE));
    fixture.engine.shutdown().await;
}
