#![cfg(unix)]

use std::{
    collections::BTreeMap,
    fs,
    os::unix::fs::PermissionsExt as _,
    sync::{Arc, Mutex},
};

use cookie_agent_identity::{
    AuthFieldName, AuthMethodId, CatalogRevision, ProtocolRecipeId, ProviderId, ProviderModelId,
    ProviderRecipeId, ProviderSetupRecipeId, RecipeCompilerVersion, SetupFieldId,
};
use cookie_agent_models::{
    BoundedSetupString, ProviderDefinition, SafeSetupValue,
    adapters::OvenAdapterFamily,
    catalog::{
        CatalogAgeState, CatalogAvailability, CatalogLimits, CatalogModalities, CatalogModelCost,
        CatalogModelEntry, CatalogModelRecord, CatalogModelStatus, CatalogProviderEntry,
        CatalogProviderRecord, CatalogRequest, CatalogRuntimeState, CatalogSnapshot, CatalogSource,
        CatalogTransport, CatalogTransportFuture, CatalogTransportResponse, PicoUsdPerMillion,
    },
    manager::{
        EffectiveCredentialSource, ModelManager, ModelManagerError, ProviderConnectRequest,
        ProviderDisconnectRequest, RetainedFamilyMatch, retained_family_match,
        safe_definition_fingerprint,
    },
    manifests::{
        FrozenProviderSource, ManifestError, ModelSnapshotManifestSchemaVersion,
        ModelSnapshotManifestStore, ModelSnapshotManifestV1, NormalizedDecimal, RehydrationError,
        canonical_payload_bytes, frozen_binding,
    },
    provider_store::{
        ClientConnectId, ClientRequestId, ConnectMutation, ConnectProposal, ProviderAuthValues,
        ProviderStore, SafePolicyString,
    },
};
use jiff::Timestamp;
use sha2::{Digest as _, Sha256};
use tempfile::TempDir;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

fn revision(label: &str) -> CatalogRevision {
    CatalogRevision::new(format!("sha256:{:x}", Sha256::digest(label.as_bytes()))).unwrap()
}

fn catalog() -> Arc<CatalogSnapshot> {
    let provider_id = ProviderId::new("openai").unwrap();
    let model_id = ProviderModelId::new("gpt-5-mini").unwrap();
    let model = CatalogModelRecord {
        id: model_id.clone(),
        name: "GPT-5 mini".to_owned(),
        description: "test".to_owned(),
        family: None,
        attachment: false,
        reasoning: false,
        tool_call: true,
        structured_output: Some(true),
        temperature: Some(true),
        open_weights: false,
        status: CatalogModelStatus::Stable,
        release_date: "2026-01-01".to_owned(),
        last_updated: "2026-01-01".to_owned(),
        modalities: CatalogModalities {
            input: vec!["text".to_owned()],
            output: vec!["text".to_owned()],
        },
        limits: CatalogLimits {
            context: 128_000,
            input: None,
            output: 16_384,
        },
        shape: None,
        provider: None,
        reasoning_options: Vec::new(),
        cost: None,
        interleaved: None,
        canonical_provenance: None,
    };
    let record = CatalogProviderRecord {
        id: provider_id.clone(),
        name: "OpenAI".to_owned(),
        environment: vec!["OPENAI_API_KEY".to_owned()],
        npm: "@ai-sdk/openai".to_owned(),
        api: None,
        shape: None,
        documentation_url: "https://example.test/openai".to_owned(),
        models: BTreeMap::from([(
            model_id.clone(),
            CatalogModelEntry {
                id: model_id,
                record: Some(model),
                quarantine: None,
            },
        )]),
    };
    let now = Timestamp::now();
    Arc::new(CatalogSnapshot {
        revision: revision("catalog"),
        source: CatalogSource::Network,
        state: CatalogRuntimeState {
            availability: CatalogAvailability::Ready,
            age: CatalogAgeState::Current,
            last_error: None,
        },
        validated_at: now,
        last_checked_at: now,
        etag: None,
        providers: BTreeMap::from([(
            provider_id.clone(),
            CatalogProviderEntry {
                id: provider_id,
                record: Some(record),
                quarantine: None,
            },
        )]),
        canonical_models: BTreeMap::new(),
        quarantine: Vec::new(),
    })
}

fn cloud_catalog(
    provider: &str,
    npm: &str,
    environment: &[&str],
    model_id: &str,
    family: Option<&str>,
) -> Arc<CatalogSnapshot> {
    let provider_id = ProviderId::new(provider).unwrap();
    let model_id = ProviderModelId::new(model_id).unwrap();
    let environment = environment
        .iter()
        .map(|value| (*value).to_owned())
        .collect::<Vec<_>>();
    let model = CatalogModelRecord {
        id: model_id.clone(),
        name: model_id.to_string(),
        description: "cloud test".to_owned(),
        family: family.map(str::to_owned),
        attachment: false,
        reasoning: false,
        tool_call: true,
        structured_output: Some(true),
        temperature: Some(true),
        open_weights: false,
        status: CatalogModelStatus::Stable,
        release_date: "2026-01-01".to_owned(),
        last_updated: "2026-01-01".to_owned(),
        modalities: CatalogModalities {
            input: vec!["text".to_owned()],
            output: vec!["text".to_owned()],
        },
        limits: CatalogLimits {
            context: 128_000,
            input: None,
            output: 16_384,
        },
        shape: None,
        provider: None,
        reasoning_options: Vec::new(),
        cost: None,
        interleaved: None,
        canonical_provenance: None,
    };
    let record = CatalogProviderRecord {
        id: provider_id.clone(),
        name: provider.to_owned(),
        environment: environment.clone(),
        npm: npm.to_owned(),
        api: None,
        shape: None,
        documentation_url: "https://example.test/cloud".to_owned(),
        models: BTreeMap::from([(
            model_id.clone(),
            CatalogModelEntry {
                id: model_id,
                record: Some(model),
                quarantine: None,
            },
        )]),
    };
    let now = Timestamp::now();
    Arc::new(CatalogSnapshot {
        revision: revision(provider),
        source: CatalogSource::Network,
        state: CatalogRuntimeState {
            availability: CatalogAvailability::Ready,
            age: CatalogAgeState::Current,
            last_error: None,
        },
        validated_at: now,
        last_checked_at: now,
        etag: None,
        providers: BTreeMap::from([(
            provider_id.clone(),
            CatalogProviderEntry {
                id: provider_id,
                record: Some(record),
                quarantine: None,
            },
        )]),
        canonical_models: BTreeMap::new(),
        quarantine: Vec::new(),
    })
}

fn setup_values(values: &[(&str, &str)]) -> BTreeMap<SetupFieldId, SafeSetupValue> {
    values
        .iter()
        .map(|(field, value)| {
            (
                SetupFieldId::new(*field).unwrap(),
                SafeSetupValue::String(BoundedSetupString::new(*value).unwrap()),
            )
        })
        .collect()
}

#[test]
fn catalog_cost_reaches_the_compiled_runtime_model() {
    let temporary = TempDir::new().unwrap();
    let mut snapshot = (*catalog()).clone();
    snapshot
        .providers
        .get_mut(&ProviderId::new("openai").unwrap())
        .unwrap()
        .record
        .as_mut()
        .unwrap()
        .models
        .get_mut(&ProviderModelId::new("gpt-5-mini").unwrap())
        .unwrap()
        .record
        .as_mut()
        .unwrap()
        .cost = Some(CatalogModelCost {
        input: PicoUsdPerMillion::new(2_000_000_000_000),
        output: PicoUsdPerMillion::new(12_000_000_000_000),
        reasoning: None,
        cache_read: Some(PicoUsdPerMillion::new(200_000_000_000)),
        cache_write: Some(PicoUsdPerMillion::new(2_500_000_000_000)),
        context_over_200k: None,
        tiers: Vec::new(),
    });
    let manager =
        ModelManager::new(BTreeMap::new(), Arc::new(snapshot), store(&temporary)).unwrap();
    let runtime = manager.current();
    let cost = runtime
        .model(&"openai/gpt-5-mini".parse().unwrap())
        .unwrap()
        .model
        .cost
        .as_ref()
        .unwrap();
    assert_eq!(cost.input.value(), 2_000_000_000_000);
    assert_eq!(cost.output.value(), 12_000_000_000_000);
    assert_eq!(cost.cache_read.unwrap().value(), 200_000_000_000);
    assert_eq!(cost.cache_write.unwrap().value(), 2_500_000_000_000);
}

#[test]
fn stored_alternative_bearer_auth_is_effective_for_anthropic_and_azure() {
    for (provider, npm, environment, setup) in [
        (
            "anthropic",
            "@ai-sdk/anthropic",
            vec!["ANTHROPIC_API_KEY"],
            Vec::new(),
        ),
        (
            "azure",
            "@ai-sdk/azure",
            vec!["AZURE_API_KEY", "AZURE_RESOURCE_NAME"],
            vec![("resource_name", "example")],
        ),
    ] {
        let temporary = TempDir::new().unwrap();
        let catalog = cloud_catalog(provider, npm, &environment, "model-1", None);
        let manager =
            ModelManager::new(BTreeMap::new(), Arc::clone(&catalog), store(&temporary)).unwrap();
        manager
            .connect(
                ProviderConnectRequest {
                    provider_id: ProviderId::new(provider).unwrap(),
                    expected_catalog_revision: catalog.revision.clone(),
                    setup_values: setup_values(&setup),
                    auth_method: AuthMethodId::new("bearer-api-key-v1").unwrap(),
                    auth_values: auth_values(&[("api_key", "secret")]),
                    client_connect_id: ClientConnectId::new(format!("{provider}-bearer")).unwrap(),
                },
                |_, _| Ok(()),
            )
            .unwrap();
        let runtime = manager.current();
        assert_eq!(
            runtime.providers()[0].effective_auth,
            EffectiveCredentialSource::ProviderStore
        );
        assert!(runtime.models().values().all(|model| {
            model.model.status == cookie_agent_models::compiler::CompiledModelStatus::Available
                && model.model.auth.method == "bearer-api-key-v1"
        }));
    }
}

fn auth_values(values: &[(&str, &str)]) -> ProviderAuthValues {
    ProviderAuthValues::new(
        values
            .iter()
            .map(|(field, value)| (AuthFieldName::new(*field).unwrap(), (*value).to_owned()))
            .collect(),
    )
    .unwrap()
}

fn store(temporary: &TempDir) -> ProviderStore {
    fs::set_permissions(temporary.path(), fs::Permissions::from_mode(0o700)).unwrap();
    ProviderStore::open(temporary.path().join("providers")).unwrap()
}

#[derive(Clone)]
struct FixtureCatalogTransport {
    body: Arc<Mutex<Option<Vec<u8>>>>,
}

impl CatalogTransport for FixtureCatalogTransport {
    fn fetch(&self, _request: CatalogRequest) -> CatalogTransportFuture<'_> {
        let body = self.body.lock().unwrap().take().unwrap();
        Box::pin(async move { Ok(CatalogTransportResponse::from_bytes(200, body)) })
    }
}

async fn fixture_catalog(
    temporary: &TempDir,
    directory: &str,
    body: &[u8],
) -> Arc<CatalogSnapshot> {
    fs::set_permissions(temporary.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let catalog_directory =
        cookie_agent_models::secure_store::SecureDirectory::open_in(temporary.path(), directory)
            .unwrap();
    Arc::new(
        cookie_agent_models::catalog::CatalogManager::new(
            FixtureCatalogTransport {
                body: Arc::new(Mutex::new(Some(body.to_vec()))),
            },
            catalog_directory,
        )
        .refresh_at("2026-08-06T00:00:00Z".parse().unwrap())
        .await
        .unwrap(),
    )
}

#[tokio::test]
async fn kimi_for_coding_catalog_connect_compiles_full_runtime() {
    let temporary = TempDir::new().unwrap();
    let catalog = fixture_catalog(
        &temporary,
        "catalog",
        include_bytes!("fixtures/models-dev-kimi-for-coding.json"),
    )
    .await;
    let manager =
        ModelManager::new(BTreeMap::new(), Arc::clone(&catalog), store(&temporary)).unwrap();
    let result = manager.connect(
        ProviderConnectRequest {
            provider_id: ProviderId::new("kimi-for-coding").unwrap(),
            expected_catalog_revision: catalog.revision.clone(),
            setup_values: BTreeMap::new(),
            auth_method: AuthMethodId::new("anthropic-api-key-v1").unwrap(),
            auth_values: auth_values(&[("api_key", "dummy")]),
            client_connect_id: ClientConnectId::new("kimi-connect").unwrap(),
        },
        |_, _| Ok(()),
    );
    assert!(result.is_ok(), "{result:?}");
    let runtime = manager.current();
    let kimi_models = runtime
        .models()
        .values()
        .filter(|model| model.key.provider_id().as_str() == "kimi-for-coding")
        .collect::<Vec<_>>();
    assert_eq!(kimi_models.len(), 4, "runtime={runtime:?}");
    assert!(kimi_models.iter().all(|model| {
        model.model.status == cookie_agent_models::compiler::CompiledModelStatus::Available
    }));
    assert!(kimi_models.iter().all(|model| {
        !model.model.capabilities.reasoning
            || model.model.capabilities.native_replay
                == cookie_agent_models::ReplayCapability::Required
    }));
    let k3 = kimi_models
        .iter()
        .find(|model| model.key.model_id().as_str() == "k3")
        .unwrap();
    assert_eq!(
        k3.model
            .variant_order
            .iter()
            .map(|id| id.as_str())
            .collect::<Vec<_>>(),
        ["off", "low", "high", "max"]
    );
    assert!(!k3.model.variants.keys().any(|id| id.as_str() == "on"));
}

#[tokio::test]
async fn nested_provider_rehydration_accepts_catalog_endpoint_identity() {
    let temporary = TempDir::new().unwrap();
    let catalog = fixture_catalog(
        &temporary,
        "catalog",
        include_bytes!("fixtures/models-dev-kimi-for-coding.json"),
    )
    .await;
    let manager =
        ModelManager::new(BTreeMap::new(), Arc::clone(&catalog), store(&temporary)).unwrap();
    manager
        .connect(
            ProviderConnectRequest {
                provider_id: ProviderId::new("kimi-for-coding").unwrap(),
                expected_catalog_revision: catalog.revision.clone(),
                setup_values: BTreeMap::new(),
                auth_method: AuthMethodId::new("anthropic-api-key-v1").unwrap(),
                auth_values: auth_values(&[("api_key", "dummy")]),
                client_connect_id: ClientConnectId::new("kimi-rehydrate").unwrap(),
            },
            |_, _| Ok(()),
        )
        .unwrap();
    let runtime = manager.current();
    let connection = runtime
        .store()
        .provider(&ProviderId::new("kimi-for-coding").unwrap())
        .unwrap();
    // The stored endpoint identity is the catalog endpoint, which differs
    // from the anthropic family default endpoint; catalog-less rehydration
    // must still accept the connection.
    assert_eq!(
        connection.policy.default_endpoint_identity.as_str(),
        "https://api.kimi.com/coding/v1"
    );
    let manifest_store =
        ModelSnapshotManifestStore::open_directory(temporary.path().join("kimi-manifest")).unwrap();
    let manifest = manifest_store
        .write(runtime.manifest_payload().unwrap())
        .unwrap();
    let index = manifest_store.scan().unwrap();
    for blueprint in &manifest.payload.blueprints {
        let binding = frozen_binding(
            manifest.revision.clone(),
            blueprint,
            blueprint.selection.clone(),
        )
        .unwrap();
        index
            .rehydrate(
                &binding,
                runtime.authored(),
                runtime.store(),
                safe_definition_fingerprint,
            )
            .unwrap();
    }
}

#[tokio::test]
async fn nested_provider_store_rejected_when_catalog_endpoint_drifts() {
    let temporary = TempDir::new().unwrap();
    let catalog = fixture_catalog(
        &temporary,
        "catalog",
        include_bytes!("fixtures/models-dev-kimi-for-coding.json"),
    )
    .await;
    let manager =
        ModelManager::new(BTreeMap::new(), Arc::clone(&catalog), store(&temporary)).unwrap();
    manager
        .connect(
            ProviderConnectRequest {
                provider_id: ProviderId::new("kimi-for-coding").unwrap(),
                expected_catalog_revision: catalog.revision.clone(),
                setup_values: BTreeMap::new(),
                auth_method: AuthMethodId::new("anthropic-api-key-v1").unwrap(),
                auth_values: auth_values(&[("api_key", "dummy")]),
                client_connect_id: ClientConnectId::new("kimi-drift").unwrap(),
            },
            |_, _| Ok(()),
        )
        .unwrap();
    let provider_id = ProviderId::new("kimi-for-coding").unwrap();
    let mut drifted = (*catalog).clone();
    let entry = drifted.providers.get_mut(&provider_id).unwrap();
    entry.record.as_mut().unwrap().api = Some("https://example.invalid/v1".to_owned());
    let manager = ModelManager::new(BTreeMap::new(), Arc::new(drifted), store(&temporary)).unwrap();
    let runtime = manager.current();
    let kimi_models = runtime
        .models()
        .values()
        .filter(|model| model.key.provider_id() == provider_id)
        .collect::<Vec<_>>();
    assert_eq!(kimi_models.len(), 4, "runtime={runtime:?}");
    assert!(kimi_models.iter().all(|model| {
        model.model.status
            == cookie_agent_models::compiler::CompiledModelStatus::CredentialsUnavailable
    }));
}

#[tokio::test]
async fn anthropic_family_catalog_endpoints_connect_and_compile() {
    for (provider, api) in [
        (
            "thinkingmachines",
            "https://tinker.thinkingmachines.dev/services/tinker-prod/anthropic/api/v1",
        ),
        ("freemodel", "https://cc.freemodel.dev/v1"),
        ("minimax", "https://api.minimax.io/anthropic/v1"),
        ("subconscious", "https://api.subconscious.dev/v1"),
        ("minimax-coding-plan", "https://api.minimax.io/anthropic/v1"),
        ("minimax-cn", "https://api.minimaxi.com/anthropic/v1"),
        (
            "minimax-cn-coding-plan",
            "https://api.minimaxi.com/anthropic/v1",
        ),
    ] {
        let temporary = TempDir::new().unwrap();
        let source = fixture_catalog(
            &temporary,
            "catalog",
            include_bytes!("fixtures/models-dev-kimi-for-coding.json"),
        )
        .await;
        let source_id = ProviderId::new("kimi-for-coding").unwrap();
        let provider_id = ProviderId::new(provider).unwrap();
        let mut snapshot = (*source).clone();
        let mut entry = snapshot.providers.remove(&source_id).unwrap();
        entry.id = provider_id.clone();
        let record = entry.record.as_mut().unwrap();
        record.id = provider_id.clone();
        record.name = provider.to_owned();
        record.api = Some(api.to_owned());
        let (model_id, model) = record.models.first_key_value().unwrap();
        record.models = BTreeMap::from([(model_id.clone(), model.clone())]);
        snapshot.providers = BTreeMap::from([(provider_id.clone(), entry)]);
        let catalog = Arc::new(snapshot);
        let manager =
            ModelManager::new(BTreeMap::new(), Arc::clone(&catalog), store(&temporary)).unwrap();
        manager
            .connect(
                ProviderConnectRequest {
                    provider_id: provider_id.clone(),
                    expected_catalog_revision: catalog.revision.clone(),
                    setup_values: BTreeMap::new(),
                    auth_method: AuthMethodId::new("anthropic-api-key-v1").unwrap(),
                    auth_values: auth_values(&[("api_key", "dummy")]),
                    client_connect_id: ClientConnectId::new(format!("{provider}-connect")).unwrap(),
                },
                |_, _| Ok(()),
            )
            .unwrap();
        let runtime = manager.current();
        assert_eq!(
            runtime.providers()[0].effective_auth,
            EffectiveCredentialSource::ProviderStore,
            "{provider}"
        );
        assert!(
            runtime.models().values().all(|model| {
                model.model.status == cookie_agent_models::compiler::CompiledModelStatus::Available
            }),
            "{provider}"
        );
    }
}

#[tokio::test]
async fn nested_anthropic_beta_catalog_metadata_compiles_manager_runtime() {
    let temporary = TempDir::new().unwrap();
    let catalog = fixture_catalog(
        &temporary,
        "catalog",
        include_bytes!("fixtures/models-dev-live-audit-2026-08-05.json"),
    )
    .await;
    let manager = ModelManager::new(BTreeMap::new(), catalog, store(&temporary)).unwrap();
    let runtime = manager.current();
    for provider in [
        "anyapi",
        "snowflake-cortex",
        "github-copilot",
        "auriko",
        "orcarouter",
        "gmicloud",
        "anthropic",
        "databricks",
    ] {
        let provider_id = ProviderId::new(provider).unwrap();
        let state = runtime
            .providers()
            .iter()
            .find(|state| state.id == provider_id)
            .unwrap();
        assert_eq!(state.support_reason, None, "{provider}");
        assert!(
            runtime
                .models()
                .keys()
                .any(|model| model.provider_id() == provider_id),
            "{provider}"
        );
    }
}

#[test]
fn runtime_provider_projection_preserves_catalog_drift_code() {
    let temporary = TempDir::new().unwrap();
    let mut snapshot = (*catalog()).clone();
    let provider_id = ProviderId::new("openai").unwrap();
    let record = snapshot
        .providers
        .get_mut(&provider_id)
        .unwrap()
        .record
        .as_mut()
        .unwrap();
    record.shape = Some("responses".to_owned());
    let manager =
        ModelManager::new(BTreeMap::new(), Arc::new(snapshot), store(&temporary)).unwrap();
    assert_eq!(manager.current().providers()[0].support_reason, None);
    assert!(
        manager
            .connect(
                connect_request("quarantined", "secret", manager.current().catalog()),
                |_, _| Ok(())
            )
            .is_ok()
    );
}

fn empty_catalog() -> Arc<CatalogSnapshot> {
    let now = Timestamp::now();
    Arc::new(CatalogSnapshot {
        revision: revision("empty-catalog"),
        source: CatalogSource::Bootstrap,
        state: CatalogRuntimeState {
            availability: CatalogAvailability::Bootstrap,
            age: CatalogAgeState::Current,
            last_error: None,
        },
        validated_at: now,
        last_checked_at: now,
        etag: None,
        providers: BTreeMap::new(),
        canonical_models: BTreeMap::new(),
        quarantine: Vec::new(),
    })
}

async fn mock_sse_server(body: String) -> (String, tokio::task::JoinHandle<String>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        let mut buffer = [0_u8; 8192];
        loop {
            let read = socket.read(&mut buffer).await.unwrap();
            if read == 0 {
                break;
            }
            request.extend_from_slice(&buffer[..read]);
            if request.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
        }
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        socket.write_all(response.as_bytes()).await.unwrap();
        String::from_utf8_lossy(&request).into_owned()
    });
    (format!("http://{address}/v1"), task)
}

async fn capture_http_request() -> (String, tokio::task::JoinHandle<String>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        let mut buffer = [0_u8; 8192];
        let mut expected = None;
        loop {
            let read = socket.read(&mut buffer).await.unwrap();
            if read == 0 {
                break;
            }
            request.extend_from_slice(&buffer[..read]);
            if expected.is_none()
                && let Some(header_end) =
                    request.windows(4).position(|window| window == b"\r\n\r\n")
            {
                let header_end = header_end + 4;
                let headers = String::from_utf8_lossy(&request[..header_end]);
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .and_then(|value| value.trim().parse::<usize>().ok())
                    })
                    .unwrap_or(0);
                expected = Some(header_end + content_length);
            }
            if expected.is_some_and(|expected| request.len() >= expected) {
                break;
            }
        }
        socket
            .write_all(b"HTTP/1.1 500 Internal Server Error\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
            .await
            .unwrap();
        String::from_utf8_lossy(&request).into_owned()
    });
    (format!("http://{address}"), task)
}

fn connect_request(id: &str, secret: &str, catalog: &CatalogSnapshot) -> ProviderConnectRequest {
    ProviderConnectRequest {
        provider_id: ProviderId::new("openai").unwrap(),
        expected_catalog_revision: catalog.revision.clone(),
        setup_values: BTreeMap::new(),
        auth_method: AuthMethodId::new("bearer-api-key-v1").unwrap(),
        auth_values: ProviderAuthValues::new(BTreeMap::from([(
            AuthFieldName::new("api_key").unwrap(),
            secret.to_owned(),
        )]))
        .unwrap(),
        client_connect_id: ClientConnectId::new(id).unwrap(),
    }
}

#[test]
fn failed_connect_and_disconnect_candidates_leave_store_and_runtime_unchanged() {
    let temporary = TempDir::new().unwrap();
    let catalog = catalog();
    let manager =
        ModelManager::new(BTreeMap::new(), Arc::clone(&catalog), store(&temporary)).unwrap();
    let before = manager.current();
    let store_before = manager.current().store().store_revision().clone();
    let error = manager
        .connect(connect_request("connect-fail", "one", &catalog), |_, _| {
            Err::<(), _>(ModelManagerError::RuntimeCompileFailed)
        })
        .unwrap_err();
    assert!(matches!(error, ModelManagerError::RuntimeCompileFailed));
    assert_eq!(
        manager.current().runtime_revision(),
        before.runtime_revision()
    );
    assert_eq!(manager.current().store().store_revision(), &store_before);

    manager
        .connect(
            connect_request("connect-ok", "one", &catalog),
            |_, _| Ok(()),
        )
        .unwrap();
    let connected = manager.current();
    let connection = connected
        .store()
        .provider(&ProviderId::new("openai").unwrap())
        .unwrap();
    let disconnect = ProviderDisconnectRequest {
        provider_id: ProviderId::new("openai").unwrap(),
        expected_runtime_revision: connected.runtime_revision().clone(),
        expected_provider_state_revision: connected.provider_state_revision(),
        expected_connection_generation: Some(connection.connection_generation),
        client_request_id: ClientRequestId::new("disconnect-fail").unwrap(),
    };
    assert!(
        manager
            .disconnect(disconnect, |_, _| {
                Err::<(), _>(ModelManagerError::RuntimeCompileFailed)
            })
            .is_err()
    );
    assert_eq!(
        manager.current().runtime_revision(),
        connected.runtime_revision()
    );
    assert!(
        manager
            .current()
            .store()
            .provider(&ProviderId::new("openai").unwrap())
            .is_some()
    );
}

#[test]
fn same_id_replays_and_secret_rotation_preserves_safe_model_revision_and_snapshots() {
    let temporary = TempDir::new().unwrap();
    let catalog = catalog();
    let manager =
        ModelManager::new(BTreeMap::new(), Arc::clone(&catalog), store(&temporary)).unwrap();
    let first = manager
        .connect(connect_request("same", "one", &catalog), |_, _| {
            Ok("publish")
        })
        .unwrap();
    assert!(!first.replayed);
    assert_eq!(
        first.effective_auth,
        EffectiveCredentialSource::ProviderStore
    );
    let replay = manager
        .connect(
            connect_request("same", "one", &catalog),
            |_, _| -> Result<(), ModelManagerError> {
                panic!("replay must not compile or publish")
            },
        )
        .unwrap();
    assert!(replay.replayed);
    assert!(replay.publication.is_none());

    let safe_revision = manager.current().model_revision().clone();
    let selection = cookie_agent_identity::ModelSelection {
        model: "openai/gpt-5-mini".parse().unwrap(),
        variant: None,
    };
    let old_runtime = manager.current();
    let old_handle = Arc::clone(old_runtime.resolve(&selection).unwrap().model());
    manager
        .connect(connect_request("rotate", "two", &catalog), |_, _| Ok(()))
        .unwrap();
    assert_eq!(manager.current().model_revision(), &safe_revision);
    assert_eq!(manager.retained_all(&safe_revision).len(), 2);
    let new_handle = Arc::clone(manager.current().resolve(&selection).unwrap().model());
    assert!(!Arc::ptr_eq(&old_handle, &new_handle));
    assert!(Arc::ptr_eq(
        &old_handle,
        old_runtime.resolve(&selection).unwrap().model()
    ));
}

#[test]
fn retained_family_match_rejects_each_persisted_policy_field_drift() {
    let temporary = TempDir::new().unwrap();
    let catalog = catalog();
    let manager =
        ModelManager::new(BTreeMap::new(), Arc::clone(&catalog), store(&temporary)).unwrap();
    manager
        .connect(
            connect_request("retained-policy", "one", &catalog),
            |_, _| Ok(()),
        )
        .unwrap();
    let provider_id = ProviderId::new("openai").unwrap();
    let connection = manager
        .current()
        .store()
        .provider(&provider_id)
        .unwrap()
        .clone();
    assert_eq!(
        retained_family_match(&provider_id, &connection),
        RetainedFamilyMatch::SupportedRemoved
    );

    let rejected = |connection| {
        assert_eq!(
            retained_family_match(&provider_id, &connection),
            RetainedFamilyMatch::RemovedWithoutRetainedFamilyMatch
        );
    };
    let mut drift = connection.clone();
    drift.provider_id = ProviderId::new("anthropic").unwrap();
    rejected(drift);
    let mut drift = connection.clone();
    drift.policy.family_id = SafePolicyString::new("openai-chat").unwrap();
    rejected(drift);
    let mut drift = connection.clone();
    drift.policy.adapter_id = SafePolicyString::new("oven.openai.chat").unwrap();
    assert_eq!(
        drift.policy.source_record_digest,
        connection.policy.source_record_digest
    );
    rejected(drift);
    let mut drift = connection.clone();
    drift.policy.setup_recipe = ProviderSetupRecipeId::new("vertex-setup-v1").unwrap();
    rejected(drift);
    let mut drift = connection.clone();
    drift.policy.compiler_version =
        RecipeCompilerVersion::new("family-registry-compiler-v2").unwrap();
    rejected(drift);
    let mut drift = connection.clone();
    drift.auth_method = AuthMethodId::new("no-auth-v1").unwrap();
    rejected(drift);
    // Endpoint identity drift is intentionally not rejected here: the
    // catalog-less retained-family check cannot resolve the current catalog
    // endpoint, so endpoint enforcement lives in the catalog-aware compile
    // path (see nested_provider_store_rejected_when_catalog_endpoint_drifts).
    let mut drift = connection.clone();
    drift.policy.package_claim = SafePolicyString::new("@ai-sdk/openai-forged").unwrap();
    rejected(drift);
    let mut drift = connection.clone();
    drift.policy.recipe_fingerprint =
        cookie_agent_models::Sha256Digest::new("a".repeat(64)).unwrap();
    assert_eq!(
        drift.policy.source_record_digest,
        connection.policy.source_record_digest
    );
    rejected(drift);
    let mut drift = connection.clone();
    drift.setup_values.insert(
        SetupFieldId::new("region").unwrap(),
        SafeSetupValue::String(BoundedSetupString::new("us-east-1").unwrap()),
    );
    rejected(drift);
    let mut drift = connection;
    drift.setup_fingerprint = cookie_agent_models::Sha256Digest::new("b".repeat(64)).unwrap();
    rejected(drift);
}

#[test]
fn source_record_digest_tracks_exact_catalog_record_independently_of_recipe_identity() {
    let first_temporary = TempDir::new().unwrap();
    let first_catalog = catalog();
    let first = ModelManager::new(
        BTreeMap::new(),
        Arc::clone(&first_catalog),
        store(&first_temporary),
    )
    .unwrap();
    first
        .connect(
            connect_request("source-digest", "secret", &first_catalog),
            |_, _| Ok(()),
        )
        .unwrap();
    let provider_id = ProviderId::new("openai").unwrap();
    let first_connection = first
        .current()
        .store()
        .provider(&provider_id)
        .unwrap()
        .clone();
    let first_source = first_connection.policy.source_record_digest.clone();

    let mut changed_catalog = (*first_catalog).clone();
    changed_catalog
        .providers
        .get_mut(&provider_id)
        .unwrap()
        .record
        .as_mut()
        .unwrap()
        .documentation_url = "https://example.test/changed-provenance".to_owned();
    let second_temporary = TempDir::new().unwrap();
    let second = ModelManager::new(
        BTreeMap::new(),
        Arc::new(changed_catalog),
        store(&second_temporary),
    )
    .unwrap();
    let (second_source, second_recipe) =
        match &second.current().models().values().next().unwrap().source {
            cookie_agent_models::manager::RuntimeProviderSource::Managed {
                source_record_digest,
                recipe_fingerprint,
                ..
            } => (source_record_digest.clone(), recipe_fingerprint.clone()),
            cookie_agent_models::manager::RuntimeProviderSource::Custom { .. } => unreachable!(),
        };
    assert_ne!(first_source, second_source);
    assert_eq!(first_connection.policy.recipe_fingerprint, second_recipe);

    let mut recipe_drift = first_connection;
    recipe_drift.policy.adapter_id = SafePolicyString::new("oven.openai.chat").unwrap();
    assert_eq!(recipe_drift.policy.source_record_digest, first_source);
    assert_eq!(
        retained_family_match(&provider_id, &recipe_drift),
        RetainedFamilyMatch::RemovedWithoutRetainedFamilyMatch
    );
}

#[test]
fn harmless_catalog_refresh_reuses_store_credentials_with_new_source_provenance() {
    let temporary = TempDir::new().unwrap();
    let initial_catalog = catalog();
    let manager = ModelManager::new(
        BTreeMap::new(),
        Arc::clone(&initial_catalog),
        store(&temporary),
    )
    .unwrap();
    manager
        .connect(
            connect_request("refresh-source", "secret", &initial_catalog),
            |_, _| Ok(()),
        )
        .unwrap();
    let old_runtime = manager.current();
    let manifest_store =
        ModelSnapshotManifestStore::open_directory(temporary.path().join("refresh-manifests"))
            .unwrap();
    let old_manifest = manifest_store
        .write(old_runtime.manifest_payload().unwrap())
        .unwrap();
    let old_blueprint = &old_manifest.payload.blueprints[0];
    let old_binding = frozen_binding(
        old_manifest.revision.clone(),
        old_blueprint,
        old_blueprint.selection.clone(),
    )
    .unwrap();
    let (old_source, old_recipe) = match &old_runtime
        .model(&old_blueprint.selection.model)
        .unwrap()
        .source
    {
        cookie_agent_models::manager::RuntimeProviderSource::Managed {
            source_record_digest,
            recipe_fingerprint,
            ..
        } => (source_record_digest.clone(), recipe_fingerprint.clone()),
        cookie_agent_models::manager::RuntimeProviderSource::Custom { .. } => unreachable!(),
    };

    let mut refreshed_catalog = (*initial_catalog).clone();
    refreshed_catalog.revision = revision("harmless-refresh");
    refreshed_catalog
        .providers
        .get_mut(&ProviderId::new("openai").unwrap())
        .unwrap()
        .record
        .as_mut()
        .unwrap()
        .documentation_url = "https://example.test/refreshed-metadata".to_owned();
    let refreshed_catalog = Arc::new(refreshed_catalog);
    manager
        .reload_inputs(BTreeMap::new(), Arc::clone(&refreshed_catalog), |_| Ok(()))
        .unwrap();
    let refreshed_runtime = manager.current();
    assert_eq!(
        refreshed_runtime.providers()[0].effective_auth,
        EffectiveCredentialSource::ProviderStore
    );
    let refreshed_model = refreshed_runtime
        .model(&old_blueprint.selection.model)
        .unwrap();
    let (new_source, new_recipe) = match &refreshed_model.source {
        cookie_agent_models::manager::RuntimeProviderSource::Managed {
            source_record_digest,
            recipe_fingerprint,
            ..
        } => (source_record_digest.clone(), recipe_fingerprint.clone()),
        cookie_agent_models::manager::RuntimeProviderSource::Custom { .. } => unreachable!(),
    };
    assert_ne!(new_source, old_source);
    assert_eq!(new_recipe, old_recipe);

    let new_manifest = manifest_store
        .write(refreshed_runtime.manifest_payload().unwrap())
        .unwrap();
    let new_blueprint = &new_manifest.payload.blueprints[0];
    let new_binding = frozen_binding(
        new_manifest.revision.clone(),
        new_blueprint,
        new_blueprint.selection.clone(),
    )
    .unwrap();
    let index = manifest_store.scan().unwrap();
    for binding in [&old_binding, &new_binding] {
        let rehydrated = index
            .rehydrate(
                binding,
                refreshed_runtime.authored(),
                refreshed_runtime.store(),
                safe_definition_fingerprint,
            )
            .unwrap();
        let prepared = refreshed_runtime
            .resolve_frozen(binding, &rehydrated.blueprint)
            .unwrap()
            .prepare_request(oven_sdk::Request::new(Vec::new()));
        assert!(prepared.history.is_empty());
        assert_eq!(prepared.inference.temperature, None);
    }

    let restarted =
        ModelManager::new(BTreeMap::new(), refreshed_catalog, store(&temporary)).unwrap();
    let reopened_index = manifest_store.scan().unwrap();
    let rehydrated = reopened_index
        .rehydrate(
            &new_binding,
            restarted.current().authored(),
            restarted.current().store(),
            safe_definition_fingerprint,
        )
        .unwrap();
    restarted
        .current()
        .resolve_frozen(&new_binding, &rehydrated.blueprint)
        .unwrap();
}

#[test]
fn removed_provider_without_exact_retained_match_cannot_rotate_credentials() {
    let source = TempDir::new().unwrap();
    let catalog = catalog();
    let source_manager =
        ModelManager::new(BTreeMap::new(), Arc::clone(&catalog), store(&source)).unwrap();
    source_manager
        .connect(
            connect_request("source-policy", "old-secret", &catalog),
            |_, _| Ok(()),
        )
        .unwrap();
    let provider_id = ProviderId::new("openai").unwrap();
    let mut policy = source_manager
        .current()
        .store()
        .provider(&provider_id)
        .unwrap()
        .policy
        .clone();

    let removed_catalog = empty_catalog();
    policy.catalog_revision = removed_catalog.revision.clone();
    policy.adapter_id = SafePolicyString::new("oven.openai.chat").unwrap();
    let target = TempDir::new().unwrap();
    let target_store = store(&target);
    let transaction = target_store.begin_transaction().unwrap();
    let snapshot = transaction.snapshot();
    let mutation = ConnectMutation {
        client_connect_id: ClientConnectId::new("install-drift").unwrap(),
        provider_id: provider_id.clone(),
        expected_catalog_revision: removed_catalog.revision.clone(),
        expectation: snapshot.expectation(),
        setup_values: BTreeMap::new(),
        auth_method: AuthMethodId::new("bearer-api-key-v1").unwrap(),
        auth_values: auth_values(&[("api_key", "old-secret")]),
        policy,
    };
    let ConnectProposal::Proposed(proposal) = transaction
        .propose_connect(&mutation, &removed_catalog.revision)
        .unwrap()
    else {
        panic!("new retained policy must propose");
    };
    transaction.commit(*proposal).unwrap();

    let manager = ModelManager::new(BTreeMap::new(), removed_catalog, target_store).unwrap();
    let current = manager.current();
    let state = &current.providers()[0];
    assert_eq!(
        state.retained_family_match,
        Some(RetainedFamilyMatch::RemovedWithoutRetainedFamilyMatch)
    );
    assert_eq!(
        state.support_reason.as_deref(),
        Some("removed_without_retained_recipe_match")
    );
    assert_eq!(state.effective_auth, EffectiveCredentialSource::Unavailable);
    let request = ProviderConnectRequest {
        provider_id: provider_id.clone(),
        expected_catalog_revision: manager.current().catalog().revision.clone(),
        setup_values: BTreeMap::new(),
        auth_method: AuthMethodId::new("bearer-api-key-v1").unwrap(),
        auth_values: auth_values(&[("api_key", "new-secret")]),
        client_connect_id: ClientConnectId::new("blocked-rotation").unwrap(),
    };
    assert!(matches!(
        manager.connect(request, |_, _| Ok(())),
        Err(ModelManagerError::RemovedWithoutRetainedRecipeMatch)
    ));
    assert_eq!(
        manager
            .current()
            .store()
            .provider(&provider_id)
            .unwrap()
            .credential(&AuthFieldName::new("api_key").unwrap()),
        Some("old-secret")
    );
}

#[tokio::test]
async fn dynamic_handles_execute_anthropic_and_openai_responses_and_build_vertex() {
    let temporary = TempDir::new().unwrap();
    let (anthropic_endpoint, anthropic_request) = mock_sse_server(
        concat!(
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":1}}}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"anthropic-ok\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":1}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"
        )
        .to_owned(),
    )
    .await;
    let (responses_endpoint, responses_request) = mock_sse_server(
        concat!(
            "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\",\"model\":\"test\"}}\n\n",
            "event: response.output_item.added\ndata: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"message\",\"id\":\"msg_1\",\"role\":\"assistant\",\"content\":[]}}\n\n",
            "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"content_index\":0,\"delta\":\"responses-ok\"}\n\n",
            "event: response.output_item.done\ndata: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"message\",\"id\":\"msg_1\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"responses-ok\"}]}}\n\n",
            "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"output\":[{\"type\":\"message\",\"id\":\"msg_1\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"responses-ok\"}]}]}}\n\n"
        )
        .to_owned(),
    )
    .await;
    let model = |display: &str| {
        format!(
            r#"display_name = "{display}"
capabilities = {{ input = ["text"], output = ["text"], context_tokens = 4096, output_tokens = 1024, tool_calling = false, parallel_tool_calls = false, structured_output = false, reasoning = false, temperature = true, top_p = false, seed = false, native_replay = "unsupported", cancellation = "local_only", media = {{}} }}
"#
        )
    };
    let authored = [
        (
            "custom.anthropic",
            format!(
                "source = \"custom\"\nendpoint = \"{anthropic_endpoint}\"\nadaptor = \"anthropic\"\nauth = {{ method = \"anthropic-api-key-v1\", values = {{ api_key = \"anthropic-secret\" }} }}\n\n[models.test]\n{}",
                model("Anthropic")
            ),
        ),
        (
            "custom.responses",
            format!(
                "source = \"custom\"\nendpoint = \"{responses_endpoint}\"\nadaptor = \"openai-responses\"\nauth = {{ method = \"bearer-api-key-v1\", values = {{ api_key = \"responses-secret\" }} }}\n\n[models.test]\n{}",
                model("Responses")
            ),
        ),
        (
            "custom.vertex",
            format!(
                "source = \"custom\"\nendpoint = \"http://127.0.0.1:9/v1\"\nadaptor = \"google-vertex-gemini\"\nsetup = {{ project = \"project-1\", location = \"us-central1\", resource = \"publishers/google\" }}\nauth = {{ method = \"oauth-access-token-v1\", values = {{ access_token = \"vertex-secret\" }} }}\n\n[models.gemini-test]\n{}",
                model("Vertex")
            ),
        ),
        (
            "custom.openai-chat",
            format!(
                "source = \"custom\"\nendpoint = \"http://127.0.0.1:9/v1\"\nadaptor = \"openai-chat\"\nauth = {{ method = \"bearer-api-key-v1\", values = {{ api_key = \"chat-key\" }} }}\n\n[models.test]\n{}",
                model("OpenAI Chat")
            ),
        ),
        (
            "custom.compatible",
            format!(
                "source = \"custom\"\nendpoint = \"http://127.0.0.1:9/v1\"\nadaptor = \"openai-compatible\"\nauth = {{ method = \"api-key-header-v1\", parameters = {{ header_name = \"x-api-key\" }}, values = {{ api_key = \"compatible-key\" }} }}\n\n[models.test]\n{}",
                model("Compatible")
            ),
        ),
        (
            "custom.google",
            format!(
                "source = \"custom\"\nendpoint = \"http://127.0.0.1:9/v1beta\"\nadaptor = \"google-gemini\"\nauth = {{ method = \"google-api-key-header-v1\", values = {{ api_key = \"google-key\" }} }}\n\n[models.gemini-test]\n{}",
                model("Google")
            ),
        ),
        (
            "custom.bedrock",
            format!(
                "source = \"custom\"\nendpoint = \"http://127.0.0.1:9\"\nadaptor = \"aws-bedrock-converse\"\nsetup = {{ region = \"us-east-1\" }}\nauth = {{ method = \"aws-sigv4-credentials-v1\", values = {{ access_key_id = \"access-key\", secret_access_key = \"secret-key\", session_token = \"session-token\" }} }}\n\n[models.test]\n{}",
                model("Bedrock")
            ),
        ),
        (
            "custom.azure-chat",
            format!(
                "source = \"custom\"\nendpoint = \"http://127.0.0.1:9\"\nadaptor = \"azure-openai-chat\"\nsetup = {{ deployment = \"chat-deployment\", api_version = \"2025-01-01\" }}\nauth = {{ method = \"azure-api-key-v1\", values = {{ api_key = \"azure-chat-key\" }} }}\n\n[models.test]\n{}",
                model("Azure Chat")
            ),
        ),
        (
            "custom.azure-responses",
            format!(
                "source = \"custom\"\nendpoint = \"http://127.0.0.1:9\"\nadaptor = \"azure-openai-responses\"\nsetup = {{ deployment = \"responses-deployment\", api_version = \"2025-03-01\" }}\nauth = {{ method = \"azure-api-key-v1\", values = {{ api_key = \"azure-responses-key\" }} }}\n\n[models.test]\n{}",
                model("Azure Responses")
            ),
        ),
        (
            "custom.cohere",
            format!(
                "source = \"custom\"\nendpoint = \"http://127.0.0.1:9/v2\"\nadaptor = \"cohere-v2-chat\"\nauth = {{ method = \"bearer-api-key-v1\", values = {{ api_key = \"cohere-key\" }} }}\n\n[models.test]\n{}",
                model("Cohere")
            ),
        ),
    ]
    .into_iter()
    .map(|(id, value)| {
        (
            ProviderId::new(id).unwrap(),
            toml::from_str::<ProviderDefinition>(&value).unwrap(),
        )
    })
    .collect();
    let manager = ModelManager::new(authored, empty_catalog(), store(&temporary)).unwrap();
    let request = oven_sdk::Request::new(vec![oven_sdk::HistoryTurn::user(
        oven_sdk::UserMessage::new(vec![oven_sdk::InputPart::Text(oven_sdk::TextPart::new(
            "hello",
        ))]),
    )]);
    for key in ["custom.anthropic/test", "custom.responses/test"] {
        manager
            .current()
            .resolve(&cookie_agent_identity::ModelSelection {
                model: key.parse().unwrap(),
                variant: None,
            })
            .unwrap()
            .model()
            .complete(request.clone(), oven_sdk::AbortSignal::default())
            .await
            .unwrap();
    }
    manager
        .current()
        .resolve(&cookie_agent_identity::ModelSelection {
            model: "custom.vertex/gemini-test".parse().unwrap(),
            variant: None,
        })
        .unwrap();
    for key in [
        "custom.openai-chat/test",
        "custom.compatible/test",
        "custom.google/gemini-test",
        "custom.bedrock/test",
        "custom.azure-chat/test",
        "custom.azure-responses/test",
        "custom.cohere/test",
    ] {
        manager
            .current()
            .resolve(&cookie_agent_identity::ModelSelection {
                model: key.parse().unwrap(),
                variant: None,
            })
            .unwrap();
    }
    let anthropic_request = anthropic_request.await.unwrap().to_ascii_lowercase();
    let responses_request = responses_request.await.unwrap().to_ascii_lowercase();
    assert!(anthropic_request.contains("x-api-key: anthropic-secret"));
    assert!(responses_request.contains("authorization: bearer responses-secret"));
    let debug = format!("{:?}", manager.current());
    for secret in ["anthropic-secret", "responses-secret", "vertex-secret"] {
        assert!(!debug.contains(secret));
    }
}

#[tokio::test]
async fn bedrock_toggle_variants_emit_distinct_reasoning_config_payloads() {
    let mut payloads = Vec::new();
    for (label, enabled) in [("enabled", true), ("disabled", false)] {
        let temporary = TempDir::new().unwrap();
        let (endpoint, captured) = capture_http_request().await;
        let definition = format!(
            r#"source = "custom"
endpoint = "{endpoint}"
adaptor = "aws-bedrock-converse"
setup = {{ region = "us-east-1" }}
auth = {{ method = "aws-sigv4-credentials-v1", values = {{ access_key_id = "access-key", secret_access_key = "secret-key" }} }}

[models.test]
display_name = "Bedrock"
capabilities = {{ input = ["text"], output = ["text"], context_tokens = 4096, output_tokens = 1024, tool_calling = false, parallel_tool_calls = false, structured_output = false, reasoning = true, temperature = false, top_p = false, seed = false, native_replay = "unsupported", cancellation = "local_only", media = {{}} }}
variants = {{ toggle = {{ operation = "add", reasoning = {{ type = "toggle", enabled = {enabled} }} }} }}
"#
        );
        let manager = ModelManager::new(
            BTreeMap::from([(
                ProviderId::new(format!("custom.{label}")).unwrap(),
                toml::from_str::<ProviderDefinition>(&definition).unwrap(),
            )]),
            empty_catalog(),
            store(&temporary),
        )
        .unwrap();
        let resolved = manager
            .current()
            .resolve(&cookie_agent_identity::ModelSelection {
                model: format!("custom.{label}/test").parse().unwrap(),
                variant: Some(cookie_agent_identity::VariantId::new("toggle").unwrap()),
            })
            .unwrap();
        let request = oven_sdk::Request::new(vec![oven_sdk::HistoryTurn::user(
            oven_sdk::UserMessage::new(vec![oven_sdk::InputPart::Text(oven_sdk::TextPart::new(
                "hello",
            ))]),
        )]);
        let _ = resolved
            .model()
            .complete(
                resolved.prepare_request(request),
                oven_sdk::AbortSignal::default(),
            )
            .await;
        payloads.push(captured.await.unwrap());
    }
    assert!(payloads[0].contains(r#""reasoningConfig":{"type":"enabled"}"#));
    assert!(payloads[1].contains(r#""reasoningConfig":{"type":"disabled"}"#));
    assert_ne!(payloads[0], payloads[1]);
}

#[test]
fn global_cloud_connections_are_executable_cross_workspace_and_disconnect_cleanly() {
    struct Case<'a> {
        provider: &'a str,
        npm: &'a str,
        environment: &'a [&'a str],
        model: &'a str,
        family: Option<&'a str>,
        setup: &'a [(&'a str, &'a str)],
        method: &'a str,
        credentials: &'a [(&'a str, &'a str)],
    }

    for case in [
        Case {
            provider: "amazon-bedrock",
            npm: "@ai-sdk/amazon-bedrock",
            environment: &[
                "AWS_ACCESS_KEY_ID",
                "AWS_BEARER_TOKEN_BEDROCK",
                "AWS_REGION",
                "AWS_SECRET_ACCESS_KEY",
            ],
            model: "anthropic.claude-3-5-sonnet-20241022-v2:0",
            family: None,
            setup: &[("region", "us-east-1")],
            method: "aws-sigv4-credentials-v1",
            credentials: &[
                ("access_key_id", "bedrock-access"),
                ("secret_access_key", "bedrock-secret"),
                ("session_token", "bedrock-session"),
            ],
        },
        Case {
            provider: "google-vertex",
            npm: "@ai-sdk/google-vertex",
            environment: &[
                "GOOGLE_APPLICATION_CREDENTIALS",
                "GOOGLE_VERTEX_LOCATION",
                "GOOGLE_VERTEX_PROJECT",
            ],
            model: "gemini-2.5-flash",
            family: Some("gemini-flash"),
            setup: &[
                ("location", "us-central1"),
                ("project", "project-1"),
                ("resource", "publishers/google"),
            ],
            method: "oauth-access-token-v1",
            credentials: &[("access_token", "vertex-token")],
        },
        Case {
            provider: "azure",
            npm: "@ai-sdk/azure",
            environment: &["AZURE_API_KEY", "AZURE_RESOURCE_NAME"],
            model: "gpt-5-mini",
            family: None,
            setup: &[
                ("api_version", "2025-03-01"),
                ("deployment", "gpt-5-mini"),
                ("resource_name", "example-resource"),
            ],
            method: "azure-api-key-v1",
            credentials: &[("api_key", "azure-secret")],
        },
    ] {
        let temporary = TempDir::new().unwrap();
        let catalog = cloud_catalog(
            case.provider,
            case.npm,
            case.environment,
            case.model,
            case.family,
        );
        let first =
            ModelManager::new(BTreeMap::new(), Arc::clone(&catalog), store(&temporary)).unwrap();
        let connected = first
            .connect(
                ProviderConnectRequest {
                    provider_id: ProviderId::new(case.provider).unwrap(),
                    expected_catalog_revision: catalog.revision.clone(),
                    setup_values: setup_values(case.setup),
                    auth_method: AuthMethodId::new(case.method).unwrap(),
                    auth_values: auth_values(case.credentials),
                    client_connect_id: ClientConnectId::new("cross-workspace-connect").unwrap(),
                },
                |_, _| Ok(()),
            )
            .unwrap();
        let stored_connection = connected
            .runtime
            .store()
            .provider(&ProviderId::new(case.provider).unwrap())
            .unwrap();
        let rebuilt_endpoint = stored_connection.policy.default_endpoint_identity.as_str();
        assert_eq!(
            connected.effective_auth,
            EffectiveCredentialSource::ProviderStore,
            "{} connection was not effective: {:?}, rebuilt endpoint={rebuilt_endpoint}",
            case.provider,
            connected
                .runtime
                .store()
                .provider(&ProviderId::new(case.provider).unwrap())
        );

        let second =
            ModelManager::new(BTreeMap::new(), Arc::clone(&catalog), store(&temporary)).unwrap();
        let provider_id = ProviderId::new(case.provider).unwrap();
        let model = cookie_agent_identity::ModelSelection {
            model: format!("{}/{}", case.provider, case.model).parse().unwrap(),
            variant: None,
        };
        let second_runtime = second.current();
        assert_eq!(
            second_runtime
                .providers()
                .iter()
                .find(|provider| provider.id == provider_id)
                .unwrap()
                .effective_auth,
            EffectiveCredentialSource::ProviderStore
        );
        assert_eq!(
            second_runtime.model(&model.model).unwrap().model.status,
            cookie_agent_models::compiler::CompiledModelStatus::Available
        );
        second_runtime.resolve(&model).unwrap();
        let debug = format!("{second_runtime:?} {:?}", second_runtime.store());
        for (_, secret) in case.credentials {
            assert!(!debug.contains(secret));
        }
        let connection = second_runtime.store().provider(&provider_id).unwrap();
        let disconnect = ProviderDisconnectRequest {
            provider_id: provider_id.clone(),
            expected_runtime_revision: second_runtime.runtime_revision().clone(),
            expected_provider_state_revision: second_runtime.provider_state_revision(),
            expected_connection_generation: Some(connection.connection_generation),
            client_request_id: ClientRequestId::new("cross-workspace-disconnect").unwrap(),
        };
        let removed = second
            .disconnect(disconnect.clone(), |_, _| Ok(()))
            .unwrap();
        assert!(!removed.replayed);
        assert_eq!(
            removed.effective_auth,
            EffectiveCredentialSource::Unavailable
        );
        assert!(removed.runtime.store().provider(&provider_id).is_none());
        assert_ne!(
            removed.runtime.model(&model.model).unwrap().model.status,
            cookie_agent_models::compiler::CompiledModelStatus::Available
        );
        assert!(removed.runtime.resolve(&model).is_err());

        let reopened = ModelManager::new(BTreeMap::new(), catalog, store(&temporary)).unwrap();
        let replay = reopened.disconnect(disconnect, |_, _| -> Result<(), ModelManagerError> {
            panic!("disconnect replay must not compile or publish")
        });
        assert!(replay.unwrap().replayed);
    }
}

#[test]
fn another_process_generation_is_recompiled_before_publication() {
    let temporary = TempDir::new().unwrap();
    let catalog = catalog();
    let first =
        ModelManager::new(BTreeMap::new(), Arc::clone(&catalog), store(&temporary)).unwrap();
    let second =
        ModelManager::new(BTreeMap::new(), Arc::clone(&catalog), store(&temporary)).unwrap();
    second
        .connect(connect_request("external", "one", &catalog), |_, _| Ok(()))
        .unwrap();
    let observed = first
        .reload_store_if_changed(|candidate| {
            assert!(
                candidate
                    .store()
                    .provider(&ProviderId::new("openai").unwrap())
                    .is_some()
            );
            Ok("provider_store_changed+provider_store_reloaded")
        })
        .unwrap()
        .unwrap();
    assert_eq!(observed.1, "provider_store_changed+provider_store_reloaded");
    assert_eq!(
        first.current().store().generation(),
        second.current().store().generation()
    );
}

#[test]
fn authored_auth_outranks_store_and_survives_disconnect() {
    let temporary = TempDir::new().unwrap();
    let catalog = catalog();
    let provider_id = ProviderId::new("openai").unwrap();
    let authored = BTreeMap::from([(
        provider_id.clone(),
        toml::from_str::<ProviderDefinition>(
            "source = \"models_dev\"\napi_key = \"authored-secret\"\n",
        )
        .unwrap(),
    )]);
    let manager = ModelManager::new(authored, Arc::clone(&catalog), store(&temporary)).unwrap();
    let connected = manager
        .connect(
            connect_request("stored-too", "stored-secret", &catalog),
            |_, _| Ok(()),
        )
        .unwrap();
    assert_eq!(
        connected.effective_auth,
        EffectiveCredentialSource::AuthoredApiKey
    );
    let manifest_store =
        ModelSnapshotManifestStore::open_directory(temporary.path().join("authored-snapshots"))
            .unwrap();
    let manifest = manifest_store
        .write(connected.runtime.manifest_payload().unwrap())
        .unwrap();
    let binding = frozen_binding(
        manifest.revision.clone(),
        &manifest.payload.blueprints[0],
        manifest.payload.blueprints[0].selection.clone(),
    )
    .unwrap();
    assert_eq!(
        manifest_store
            .scan()
            .unwrap()
            .rehydrate(
                &binding,
                &BTreeMap::new(),
                connected.runtime.store(),
                safe_definition_fingerprint,
            )
            .unwrap_err(),
        RehydrationError::SnapshotConfigMismatch
    );
    let connection = connected.runtime.store().provider(&provider_id).unwrap();
    let disconnect = ProviderDisconnectRequest {
        provider_id: provider_id.clone(),
        expected_runtime_revision: connected.runtime.runtime_revision().clone(),
        expected_provider_state_revision: connected.runtime.provider_state_revision(),
        expected_connection_generation: Some(connection.connection_generation),
        client_request_id: ClientRequestId::new("remove-stored").unwrap(),
    };
    let result = manager.disconnect(disconnect, |_, _| Ok(())).unwrap();
    assert_eq!(
        result.effective_auth,
        EffectiveCredentialSource::AuthoredApiKey
    );
    assert!(result.runtime.store().provider(&provider_id).is_none());
}

#[test]
fn absent_disconnect_is_durable_replayable_and_conflicts_on_changed_payload() {
    let temporary = TempDir::new().unwrap();
    let catalog = catalog();
    let manager = ModelManager::new(BTreeMap::new(), catalog, store(&temporary)).unwrap();
    let before = manager.current();
    let request = ProviderDisconnectRequest {
        provider_id: ProviderId::new("groq").unwrap(),
        expected_runtime_revision: before.runtime_revision().clone(),
        expected_provider_state_revision: before.provider_state_revision(),
        expected_connection_generation: None,
        client_request_id: ClientRequestId::new("absent").unwrap(),
    };
    let first = manager.disconnect(request.clone(), |_, _| Ok(())).unwrap();
    assert!(!first.replayed);
    let replay = manager
        .disconnect(request, |_, _| -> Result<(), ModelManagerError> {
            panic!("replay must not publish")
        })
        .unwrap();
    assert!(replay.replayed);
    let conflict = ProviderDisconnectRequest {
        provider_id: ProviderId::new("openai").unwrap(),
        expected_runtime_revision: before.runtime_revision().clone(),
        expected_provider_state_revision: before.provider_state_revision(),
        expected_connection_generation: None,
        client_request_id: ClientRequestId::new("absent").unwrap(),
    };
    assert!(manager.disconnect(conflict, |_, _| Ok(())).is_err());
}

#[test]
fn retained_store_blueprint_rehydrates_without_current_catalog_and_fails_after_removal() {
    let temporary = TempDir::new().unwrap();
    let catalog = catalog();
    let manager =
        ModelManager::new(BTreeMap::new(), Arc::clone(&catalog), store(&temporary)).unwrap();
    manager
        .connect(connect_request("rehydrate", "one", &catalog), |_, _| Ok(()))
        .unwrap();
    let runtime = manager.current();
    let manifest_store =
        ModelSnapshotManifestStore::open_directory(temporary.path().join("model-snapshots"))
            .unwrap();
    let manifest = manifest_store
        .write(runtime.manifest_payload().unwrap())
        .unwrap();
    let index = manifest_store.scan().unwrap();
    let blueprint = manifest.payload.blueprints[0].clone();
    let binding = frozen_binding(
        manifest.revision.clone(),
        &blueprint,
        blueprint.selection.clone(),
    )
    .unwrap();
    index
        .rehydrate(
            &binding,
            &BTreeMap::new(),
            runtime.store(),
            safe_definition_fingerprint,
        )
        .unwrap();

    let removed_catalog = empty_catalog();
    let restarted = ModelManager::new(
        BTreeMap::new(),
        Arc::clone(&removed_catalog),
        store(&temporary),
    )
    .unwrap();
    let removed_runtime = restarted.current();
    assert!(removed_runtime.model(&blueprint.selection.model).is_none());
    assert_eq!(
        removed_runtime.providers()[0].retained_family_match,
        Some(RetainedFamilyMatch::SupportedRemoved)
    );
    assert_eq!(removed_runtime.providers()[0].support_reason, None);
    assert_eq!(
        removed_runtime.providers()[0].effective_auth,
        EffectiveCredentialSource::ProviderStore
    );
    let reconnected = restarted
        .connect(
            connect_request("removed-reconnect", "rotated", &removed_catalog),
            |_, _| Ok(()),
        )
        .unwrap();
    assert_eq!(
        reconnected.effective_auth,
        EffectiveCredentialSource::ProviderStore
    );
    let removed_runtime = reconnected.runtime;
    let rehydrated = index
        .rehydrate(
            &binding,
            removed_runtime.authored(),
            removed_runtime.store(),
            safe_definition_fingerprint,
        )
        .unwrap();
    let resolved = removed_runtime
        .resolve_frozen(&binding, &rehydrated.blueprint)
        .unwrap();
    let prepared = resolved.prepare_request(oven_sdk::Request::new(Vec::new()));
    assert_eq!(prepared.inference.temperature, None);

    let provider_id = ProviderId::new("openai").unwrap();
    let connection = removed_runtime.store().provider(&provider_id).unwrap();
    restarted
        .disconnect(
            ProviderDisconnectRequest {
                provider_id,
                expected_runtime_revision: removed_runtime.runtime_revision().clone(),
                expected_provider_state_revision: removed_runtime.provider_state_revision(),
                expected_connection_generation: Some(connection.connection_generation),
                client_request_id: ClientRequestId::new("remove-rehydrate").unwrap(),
            },
            |_, _| Ok(()),
        )
        .unwrap();
    assert_eq!(
        index
            .rehydrate(
                &binding,
                &BTreeMap::new(),
                restarted.current().store(),
                safe_definition_fingerprint,
            )
            .unwrap_err(),
        RehydrationError::SnapshotCredentialsUnavailable
    );
}

#[test]
fn nested_family_snapshot_persists_effective_npm_and_rehydrates_store_auth() {
    let temporary = TempDir::new().unwrap();
    let mut catalog = (*cloud_catalog(
        "azure",
        "@ai-sdk/azure",
        &["AZURE_API_KEY", "AZURE_RESOURCE_NAME"],
        "claude-model",
        None,
    ))
    .clone();
    let provider = catalog
        .providers
        .values_mut()
        .next()
        .unwrap()
        .record
        .as_mut()
        .unwrap();
    provider
        .models
        .values_mut()
        .next()
        .unwrap()
        .record
        .as_mut()
        .unwrap()
        .provider = Some(cookie_agent_models::catalog::CatalogModelProviderMetadata {
        npm: Some("@ai-sdk/anthropic".to_owned()),
        api: Some("https://${AZURE_RESOURCE_NAME}.services.ai.azure.com/anthropic/v1".to_owned()),
        shape: None,
    });
    let catalog = Arc::new(catalog);
    let manager =
        ModelManager::new(BTreeMap::new(), Arc::clone(&catalog), store(&temporary)).unwrap();
    manager
        .connect(
            ProviderConnectRequest {
                provider_id: ProviderId::new("azure").unwrap(),
                expected_catalog_revision: catalog.revision.clone(),
                setup_values: setup_values(&[("resource_name", "example")]),
                auth_method: AuthMethodId::new("azure-api-key-v1").unwrap(),
                auth_values: auth_values(&[("api_key", "secret")]),
                client_connect_id: ClientConnectId::new("nested-family").unwrap(),
            },
            |_, _| Ok(()),
        )
        .unwrap();
    let runtime = manager.current();
    let manifest_store =
        ModelSnapshotManifestStore::open_directory(temporary.path().join("nested-snapshots"))
            .unwrap();
    let manifest = manifest_store
        .write(runtime.manifest_payload().unwrap())
        .unwrap();
    let blueprint = &manifest.payload.blueprints[0];
    let FrozenProviderSource::Managed { package_claim, .. } = &blueprint.source else {
        panic!("expected managed source")
    };
    assert_eq!(package_claim, "@ai-sdk/anthropic");
    assert_eq!(blueprint.auth_method.as_str(), "anthropic-api-key-v1");
    let binding = frozen_binding(
        manifest.revision.clone(),
        blueprint,
        blueprint.selection.clone(),
    )
    .unwrap();
    drop(runtime);
    drop(manager);
    let reopened_store = store(&temporary);
    let reopened_snapshot = reopened_store.load().unwrap();
    manifest_store
        .scan()
        .unwrap()
        .rehydrate(
            &binding,
            &BTreeMap::new(),
            &reopened_snapshot,
            safe_definition_fingerprint,
        )
        .unwrap();
}

#[test]
fn bedrock_mantle_bearer_connection_constructs_responses_executable() {
    let temporary = TempDir::new().unwrap();
    let mut catalog = (*cloud_catalog(
        "amazon-bedrock",
        "@ai-sdk/amazon-bedrock",
        &[
            "AWS_ACCESS_KEY_ID",
            "AWS_SECRET_ACCESS_KEY",
            "AWS_REGION",
            "AWS_BEARER_TOKEN_BEDROCK",
        ],
        "openai.gpt-5.4",
        None,
    ))
    .clone();
    catalog
        .providers
        .values_mut()
        .next()
        .unwrap()
        .record
        .as_mut()
        .unwrap()
        .models
        .values_mut()
        .next()
        .unwrap()
        .record
        .as_mut()
        .unwrap()
        .provider = Some(cookie_agent_models::catalog::CatalogModelProviderMetadata {
        npm: Some("@ai-sdk/amazon-bedrock/mantle".to_owned()),
        api: Some("https://bedrock-mantle.${AWS_REGION}.api.aws/openai/v1".to_owned()),
        shape: Some("responses".to_owned()),
    });
    let catalog = Arc::new(catalog);
    let manager =
        ModelManager::new(BTreeMap::new(), Arc::clone(&catalog), store(&temporary)).unwrap();
    manager
        .connect(
            ProviderConnectRequest {
                provider_id: ProviderId::new("amazon-bedrock").unwrap(),
                expected_catalog_revision: catalog.revision.clone(),
                setup_values: setup_values(&[("region", "us-east-1")]),
                auth_method: AuthMethodId::new("bearer-api-key-v1").unwrap(),
                auth_values: auth_values(&[("api_key", "bedrock-api-key")]),
                client_connect_id: ClientConnectId::new("bedrock-mantle-bearer").unwrap(),
            },
            |_, _| Ok(()),
        )
        .unwrap();
    let runtime = manager.current();
    let model = runtime.models().values().next().unwrap();
    assert_eq!(model.model.auth.method, "bearer-api-key-v1");
    assert_eq!(model.model.adapter, OvenAdapterFamily::OpenaiResponses);
    runtime
        .resolve(&cookie_agent_identity::ModelSelection {
            model: model.key.clone(),
            variant: None,
        })
        .unwrap();
}

#[test]
fn managed_rehydration_rejects_each_recipe_identity_drift() {
    let temporary = TempDir::new().unwrap();
    let catalog = catalog();
    let manager =
        ModelManager::new(BTreeMap::new(), Arc::clone(&catalog), store(&temporary)).unwrap();
    manager
        .connect(
            connect_request("manifest-drift", "secret", &catalog),
            |_, _| Ok(()),
        )
        .unwrap();
    let runtime = manager.current();
    let original = runtime.manifest_payload().unwrap();

    macro_rules! reject_drift {
        ($label:literal, |$blueprint:ident| $body:block) => {{
            let mut payload = original.clone();
            let $blueprint = &mut payload.blueprints[0];
            $body
            let store = ModelSnapshotManifestStore::open_directory(
                temporary.path().join(concat!("manifest-drift-", $label)),
            )
            .unwrap();
            let manifest = store.write(payload).unwrap();
            let blueprint = &manifest.payload.blueprints[0];
            let binding = frozen_binding(
                manifest.revision.clone(),
                blueprint,
                blueprint.selection.clone(),
            )
            .unwrap();
            assert_eq!(
                store
                    .scan()
                    .unwrap()
                    .rehydrate(
                        &binding,
                        runtime.authored(),
                        runtime.store(),
                        safe_definition_fingerprint,
                    )
                    .unwrap_err(),
                RehydrationError::UnsupportedSnapshotRecipe
            );
        }};
    }

    reject_drift!("package", |blueprint| {
        let FrozenProviderSource::Managed { package_claim, .. } = &mut blueprint.source else {
            unreachable!()
        };
        *package_claim = "@ai-sdk/openai-forged".to_owned();
    });
    reject_drift!("provider-recipe", |blueprint| {
        let recipe = ProviderRecipeId::new("openai.chat.v1").unwrap();
        blueprint.provider_recipe = recipe.clone();
        let FrozenProviderSource::Managed {
            provider_recipe, ..
        } = &mut blueprint.source
        else {
            unreachable!()
        };
        *provider_recipe = recipe;
    });
    reject_drift!("protocol", |blueprint| {
        blueprint.protocol_recipe = ProtocolRecipeId::new("oven.openai.chat").unwrap();
    });
    reject_drift!("compiler", |blueprint| {
        blueprint.compiler_version =
            RecipeCompilerVersion::new("family-registry-compiler-v2").unwrap();
    });
    reject_drift!("recipe-fingerprint", |blueprint| {
        let FrozenProviderSource::Managed {
            source_record_digest,
            recipe_fingerprint,
            ..
        } = &mut blueprint.source
        else {
            unreachable!()
        };
        let original_source = source_record_digest.clone();
        *recipe_fingerprint = cookie_agent_protocol::Sha256Digest::of_bytes(b"forged recipe");
        assert_eq!(*source_record_digest, original_source);
    });
    reject_drift!("auth", |blueprint| {
        blueprint.auth_method = AuthMethodId::new("no-auth-v1").unwrap();
        blueprint.credential_binding.auth_method = AuthMethodId::new("no-auth-v1").unwrap();
        blueprint.credential_binding.fields.clear();
        blueprint.credential_binding.parameters.clear();
        blueprint.credential_binding.owned_headers.clear();
    });
    reject_drift!("setup", |blueprint| {
        let setup = ProviderSetupRecipeId::new("vertex-setup-v1").unwrap();
        blueprint.setup_recipe = setup.clone();
        blueprint.setup_binding.setup_recipe = setup;
    });

    let store = ModelSnapshotManifestStore::open_directory(
        temporary.path().join("manifest-drift-source-record"),
    )
    .unwrap();
    let manifest = store.write(original).unwrap();
    let path = store.path().join(format!(
        "{}.json",
        manifest.revision.as_str().strip_prefix("sha256:").unwrap()
    ));
    let mut document: serde_json::Value =
        serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    document["payload"]["blueprints"][0]["source"]["source_record_digest"] =
        serde_json::Value::String(
            cookie_agent_protocol::Sha256Digest::of_bytes(b"forged source").to_string(),
        );
    fs::write(&path, serde_json::to_vec(&document).unwrap()).unwrap();
    assert!(matches!(
        store.scan(),
        Err(ManifestError::ModelSnapshotDigestMismatch
            | ManifestError::InvalidModelSnapshotManifest)
    ));
}

#[test]
fn authored_base_url_never_falls_through_to_store_on_reload() {
    let temporary = TempDir::new().unwrap();
    let catalog = catalog();
    let provider_id = ProviderId::new("openai").unwrap();
    let authored = BTreeMap::from([(
        provider_id.clone(),
        toml::from_str::<ProviderDefinition>(
            "source = \"models_dev\"\nbase_url = \"https://gateway.example/v1\"\napi_key = \"authored\"\n",
        )
        .unwrap(),
    )]);
    let manager = ModelManager::new(authored, Arc::clone(&catalog), store(&temporary)).unwrap();
    manager
        .connect(
            connect_request("stored-fallback", "stored", &catalog),
            |_, _| Ok(()),
        )
        .unwrap();
    let before = manager.current();
    let mut changed = before.authored().clone();
    let ProviderDefinition::ModelsDev(provider) = changed.get_mut(&provider_id).unwrap() else {
        unreachable!()
    };
    provider.api_key = None;
    provider.auth_override = None;
    assert!(
        manager
            .reload_inputs(changed, Arc::clone(&catalog), |_| Ok(()))
            .is_err()
    );
    assert_eq!(
        manager.current().runtime_revision(),
        before.runtime_revision()
    );
    assert!(manager.current().store().provider(&provider_id).is_some());
}

#[test]
fn manifest_variants_are_self_contained_and_forged_bindings_are_rejected() {
    let temporary = TempDir::new().unwrap();
    let provider_id = ProviderId::new("custom.decimal").unwrap();
    let definition = toml::from_str::<ProviderDefinition>(
        r#"source = "custom"
endpoint = "http://127.0.0.1:9/v1"
adaptor = "openai-compatible"
auth = { method = "no-auth-v1", values = {} }

[models.test]
display_name = "Decimal Model"
capabilities = { input = ["text"], output = ["text"], context_tokens = 4096, output_tokens = 1024, tool_calling = false, parallel_tool_calls = false, structured_output = false, reasoning = false, temperature = true, top_p = true, seed = false, native_replay = "unsupported", cancellation = "local_only", media = {} }
defaults = { temperature = 0.7, top_p = 0.125 }
variants = { precise = { operation = "add", defaults = { temperature = 1.25, top_p = 0.5 } } }
"#,
    )
    .unwrap();
    let manager = ModelManager::new(
        BTreeMap::from([(provider_id, definition)]),
        empty_catalog(),
        store(&temporary),
    )
    .unwrap();
    let runtime = manager.current();
    let payload = runtime.manifest_payload().unwrap();
    let encoded = serde_json::to_value(&payload).unwrap();
    assert_eq!(
        encoded["blueprints"][0]["defaults"]["request"]["temperature"],
        "0.7"
    );
    assert_eq!(
        encoded["blueprints"][0]["defaults"]["request"]["top_p"],
        "0.125"
    );
    assert_eq!(
        encoded["blueprints"][0]["variants"][0]["defaults"]["request"]["temperature"],
        "1.25"
    );
    assert_eq!(
        encoded["blueprints"][0]["variants"][0]["defaults"]["request"]["top_p"],
        "0.5"
    );
    assert!(
        !serde_json::to_string(&payload)
            .unwrap()
            .contains("auth_values")
    );

    let store = ModelSnapshotManifestStore::open_directory(
        temporary.path().join("decimal-model-snapshots"),
    )
    .unwrap();
    let manifest = store.write(payload).unwrap();
    let index = store.scan().unwrap();
    let blueprint = &manifest.payload.blueprints[0];
    assert_eq!(blueprint.variants.len(), 1);
    assert_eq!(blueprint.variants[0].descriptor, blueprint.descriptor);
    assert_ne!(
        blueprint.variants[0].selection_fingerprint,
        blueprint.selection_fingerprint
    );

    let base = frozen_binding(
        manifest.revision.clone(),
        blueprint,
        blueprint.selection.clone(),
    )
    .unwrap();
    let variant = frozen_binding(
        manifest.revision.clone(),
        blueprint,
        cookie_agent_identity::ModelSelection {
            model: blueprint.selection.model.clone(),
            variant: Some(blueprint.variants[0].id.clone()),
        },
    )
    .unwrap();
    for binding in [&base, &variant] {
        index
            .rehydrate(
                binding,
                runtime.authored(),
                runtime.store(),
                safe_definition_fingerprint,
            )
            .unwrap();
    }

    let mut forged_defaults = variant.clone();
    forged_defaults.defaults.request.temperature = Some(NormalizedDecimal::from_f32(1.5).unwrap());
    assert_eq!(
        index
            .rehydrate(
                &forged_defaults,
                runtime.authored(),
                runtime.store(),
                safe_definition_fingerprint,
            )
            .unwrap_err(),
        RehydrationError::SnapshotRehydrationMismatch
    );

    let mut forged_options = variant.clone();
    forged_options.options = cookie_agent_protocol::ProviderOptions::OpenAiCompatible {
        api_path: Some("/forged".to_owned()),
    };
    assert_eq!(
        index
            .rehydrate(
                &forged_options,
                runtime.authored(),
                runtime.store(),
                safe_definition_fingerprint,
            )
            .unwrap_err(),
        RehydrationError::SnapshotRehydrationMismatch
    );

    let mut forged_fingerprint = variant;
    forged_fingerprint.selection_fingerprint =
        cookie_agent_protocol::Sha256Digest::of_bytes(b"forged");
    assert_eq!(
        index
            .rehydrate(
                &forged_fingerprint,
                runtime.authored(),
                runtime.store(),
                safe_definition_fingerprint,
            )
            .unwrap_err(),
        RehydrationError::SnapshotRehydrationMismatch
    );

    let mut corrupt_payload = manifest.payload.clone();
    corrupt_payload.blueprints[0].variants[0]
        .defaults
        .request
        .temperature = Some(NormalizedDecimal::from_f32(1.75).unwrap());
    let canonical = canonical_payload_bytes(&corrupt_payload).unwrap();
    let digest = format!("{:x}", Sha256::digest(canonical));
    let corrupt = ModelSnapshotManifestV1 {
        schema_version: ModelSnapshotManifestSchemaVersion::current(),
        revision: cookie_agent_identity::ModelSnapshotRevision::new(format!("sha256:{digest}"))
            .unwrap(),
        payload: corrupt_payload,
    };
    let path = store.path().join(format!("{digest}.json"));
    fs::write(&path, serde_json::to_vec(&corrupt).unwrap()).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    assert!(matches!(
        store.scan(),
        Err(cookie_agent_models::manifests::ManifestError::InvalidModelSnapshotManifest)
    ));
}
