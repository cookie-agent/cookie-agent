//! Onboarding from a plain protocol client: `onboarding.get`, `.preview`,
//! `.run`, and `onboarding.changed`, with nothing of the TUI or web.

use std::{collections::BTreeMap, sync::Arc};

use base64::Engine as _;
use cookie_agent_engine::{Engine, EngineOptions};
use cookie_agent_models::{
    ModelManager,
    catalog::{
        CatalogAgeState, CatalogAvailability, CatalogRuntimeState, CatalogSnapshot, CatalogSource,
    },
    provider_store::ProviderStore,
};
use cookie_agent_protocol::{
    CatalogRevision, ClientDelivery, OnboardingFileAction, OnboardingKitSource,
    OnboardingModelInput, OnboardingRequirement, OnboardingRunParams,
};
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

fn kit() -> String {
    base64::engine::general_purpose::STANDARD.encode(cookie_agent_engine::testing::pack_kit(&[(
        "agents/primary.md",
        AGENT,
    )]))
}

#[tokio::test]
async fn a_plain_client_onboards_through_the_daemon() {
    let fixture = fixture();
    let client = Arc::clone(&fixture.server).connect_in_process();
    client.handshake().await.expect("handshake");
    let mut deliveries = client.subscribe_deliveries().expect("deliveries");

    let status = client.get_onboarding().await.expect("onboarding.get");
    assert!(!status.ready);
    assert_eq!(
        status.missing,
        [
            OnboardingRequirement::DefaultModel,
            OnboardingRequirement::SmallModel
        ]
    );
    assert_eq!(status.providers_connected[0].as_str(), "custom.test");

    let source = OnboardingKitSource::Archive { data: kit() };
    let preview = client
        .preview_onboarding(Some(source.clone()))
        .await
        .expect("onboarding.preview");
    assert_eq!(preview.origin, "archive");
    assert_eq!(preview.files.len(), 1);
    assert_eq!(preview.files[0].path, "agents/primary.md");
    assert_eq!(preview.files[0].action, OnboardingFileAction::Write);

    // Without a small model the daemon would not be set up: refused, and
    // nothing is written.
    let refused = client
        .run_onboarding(OnboardingRunParams {
            install_kit: true,
            source: Some(source.clone()),
            default_model: Some(OnboardingModelInput::Key(
                "custom.test/a-model".parse().unwrap(),
            )),
            small_model: None,
        })
        .await
        .expect_err("refused");
    assert!(refused.is(&cookie_agent_protocol::errors::ONBOARDING_INCOMPLETE));
    let data = refused.error_data().expect("error data");
    assert_eq!(data.fields["missing"], serde_json::json!(["small_model"]));
    let root = fixture.directory.path().join(".cookie-agent");
    assert!(!root.join("agents").exists());

    let result = client
        .run_onboarding(OnboardingRunParams {
            install_kit: true,
            source: Some(source),
            default_model: Some(OnboardingModelInput::Key(
                "custom.test/a-model".parse().unwrap(),
            )),
            small_model: Some(OnboardingModelInput::Table(
                cookie_agent_protocol::OnboardingModelTable {
                    model: "custom.test/a-model".parse().unwrap(),
                    variant: Some(serde_json::from_value(serde_json::json!("low")).unwrap()),
                },
            )),
        })
        .await
        .expect("onboarding.run");
    assert!(result.status.ready);
    assert_eq!(result.written, ["agents/primary.md"]);
    assert_eq!(result.settings, ["default_model", "small_model"]);
    assert!(root.join("agents/primary.md").exists());

    let changed = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            match deliveries.recv().await {
                Some(ClientDelivery::OnboardingChanged(status)) => return status,
                Some(_) => {}
                None => panic!("deliveries ended"),
            }
        }
    })
    .await
    .expect("onboarding.changed");
    assert!(changed.ready);
    assert_eq!(
        serde_json::to_value(&changed.small_model).unwrap(),
        serde_json::json!({"model": "custom.test/a-model", "variant": "low"})
    );
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn a_bad_archive_and_a_bad_model_value_are_refused() {
    let fixture = fixture();
    let client = Arc::clone(&fixture.server).connect_in_process();
    client.handshake().await.expect("handshake");
    let error = client
        .preview_onboarding(Some(OnboardingKitSource::Archive {
            data: base64::engine::general_purpose::STANDARD.encode(b"not a kit"),
        }))
        .await
        .expect_err("refused");
    assert!(error.is(&cookie_agent_protocol::errors::ONBOARDING_KIT_UNAVAILABLE));

    let result = client
        .run_onboarding(OnboardingRunParams {
            install_kit: false,
            source: None,
            default_model: Some(OnboardingModelInput::Table(
                cookie_agent_protocol::OnboardingModelTable {
                    model: "custom.test/a-model".parse().unwrap(),
                    variant: None,
                },
            )),
            small_model: Some(OnboardingModelInput::Key(
                "custom.test/z-model".parse().unwrap(),
            )),
        })
        .await
        .expect("setting the models alone completes onboarding");
    assert!(result.status.ready);
    fixture.engine.shutdown().await;
}
