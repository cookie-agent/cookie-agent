use std::{collections::BTreeMap, fs, os::unix::fs::PermissionsExt as _, sync::Arc};

use cookie_agent_config::{
    ApprovalConfig, ConfigSchemaVersion, ContextCompactionConfig, LoadedConfiguration,
    LoadedMcpServer, McpServerConfig, McpServerSource, RuntimeConfig, ServerConfig,
    SessionTitleConfig, ToolOutputConfig,
};
use cookie_agent_engine::{Engine, EngineOptions};
use cookie_agent_models::{
    ModelManager,
    catalog::{
        CatalogAgeState, CatalogAvailability, CatalogLimits, CatalogModalities, CatalogModelEntry,
        CatalogModelRecord, CatalogModelStatus, CatalogProviderEntry, CatalogProviderRecord,
        CatalogRuntimeState, CatalogSnapshot, CatalogSource,
    },
    provider_store::{
        ClientConnectId as StoreClientConnectId, ConnectMutation, ConnectProposal,
        ProviderAuthValues, ProviderStore, SafePolicyString, StoredProviderPolicyProjection,
    },
};
use cookie_agent_protocol::{
    AuthFieldName, AuthMethodId, CatalogRevision, McpApprovalDecision, McpApprovalRespondParams,
    ProviderId, ProviderSetupRecipeId, RUNTIME_CHANGED_METHOD, RecipeCompilerVersion,
    SessionListParams,
};
use jiff::Timestamp;
use serde_json::{Value, json};
use tempfile::TempDir;

use crate::{
    Client, ClientDelivery, InProcessStream, MessageFrame, MessageStream, Server, in_process_pair,
};

struct Harness {
    _directory: TempDir,
    engine: Engine,
    server: Arc<Server>,
}

fn harness() -> Harness {
    harness_with_mcp(BTreeMap::new())
}

fn harness_with_mcp(mcp_servers: BTreeMap<String, LoadedMcpServer>) -> Harness {
    let directory = TempDir::new().expect("temp directory");
    fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700))
        .expect("private temp directory");
    let project = directory.path().join(".cookie-agent");
    fs::create_dir(&project).expect("project runtime directory");
    fs::set_permissions(&project, fs::Permissions::from_mode(0o700)).expect("private project");
    let provider_store = directory.path().join("provider-store");
    fs::create_dir(&provider_store).expect("provider store directory");
    fs::set_permissions(&provider_store, fs::Permissions::from_mode(0o700))
        .expect("private provider store");

    let revision =
        CatalogRevision::new(format!("sha256:{}", "0".repeat(64))).expect("catalog revision");
    let now = Timestamp::now();
    let catalog = Arc::new(CatalogSnapshot {
        revision,
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
    });
    let manager = Arc::new(
        ModelManager::new(
            BTreeMap::new(),
            catalog,
            ProviderStore::open(&provider_store).expect("provider store"),
        )
        .expect("empty model manager"),
    );
    let config = LoadedConfiguration {
        runtime: RuntimeConfig {
            schema_version: ConfigSchemaVersion,
            server: ServerConfig::default(),
            tool_output: ToolOutputConfig::default(),
            approval: ApprovalConfig::default(),
            context_compaction: ContextCompactionConfig::default(),
            session_title: SessionTitleConfig::default(),
            delegation: cookie_agent_config::DelegationConfig::default(),
            providers: BTreeMap::new(),
        },
        agents: BTreeMap::new(),
        mcp_servers,
    };
    let engine = Engine::open(EngineOptions {
        data_dir: directory.path().join("data"),
        cwd: directory.path().to_owned(),
        config,
        model_manager: manager,
        tools: Vec::new(),
    })
    .expect("empty engine");
    let server = Arc::new(Server::new(engine.clone()));
    Harness {
        _directory: directory,
        engine,
        server,
    }
}

#[tokio::test]
async fn mcp_project_approval_is_reachable_through_protocol() {
    let config = |lazy| McpServerConfig {
        command: Some("not-started".into()),
        args: Vec::new(),
        env: BTreeMap::new(),
        cwd: None,
        url: None,
        headers: BTreeMap::new(),
        enabled: true,
        lazy,
        timeout_ms: Some(1_000),
    };
    let harness = harness_with_mcp(BTreeMap::from([
        (
            "approved".into(),
            LoadedMcpServer {
                source: McpServerSource::Workspace,
                config: config(true),
            },
        ),
        (
            "rejected".into(),
            LoadedMcpServer {
                source: McpServerSource::Workspace,
                config: config(true),
            },
        ),
    ]));
    let client = Client::connect_in_process(Arc::clone(&harness.server));
    client.handshake().await.expect("handshake");
    let pending = client
        .list_mcp_approvals()
        .await
        .expect("list MCP approvals")
        .approvals;
    assert_eq!(pending.len(), 2);
    for (server, decision) in [
        ("approved", McpApprovalDecision::Approve),
        ("rejected", McpApprovalDecision::Reject),
    ] {
        assert!(pending.iter().any(|approval| approval.server == server));
        let result = client
            .respond_mcp_approval(McpApprovalRespondParams {
                server: server.into(),
                decision,
            })
            .await
            .expect("respond to MCP approval");
        assert_eq!(result.decision, decision);
    }
    assert!(
        client
            .list_mcp_approvals()
            .await
            .expect("list after responses")
            .approvals
            .is_empty()
    );
    client.shutdown();
    harness.server.shutdown();
    harness.engine.shutdown().await;
}

fn empty_catalog(label: &str) -> Arc<CatalogSnapshot> {
    let now = Timestamp::now();
    Arc::new(CatalogSnapshot {
        revision: CatalogRevision::new(format!("sha256:{label:0<64}")).expect("catalog revision"),
        source: CatalogSource::Network,
        state: CatalogRuntimeState {
            availability: CatalogAvailability::Ready,
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

fn openai_catalog() -> Arc<CatalogSnapshot> {
    let provider_id = ProviderId::new("openai").expect("provider ID");
    let model_id = cookie_agent_protocol::ProviderModelId::new("gpt-5-mini").expect("model ID");
    let environment = vec!["OPENAI_API_KEY".to_owned()];
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
        interleaved: None,
        canonical_provenance: None,
    };
    let record = CatalogProviderRecord {
        id: provider_id.clone(),
        name: "OpenAI".to_owned(),
        environment: environment.clone(),
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
        revision: CatalogRevision::new(format!("sha256:{}", "a".repeat(64)))
            .expect("catalog revision"),
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

fn harness_with_catalog(
    catalog: Arc<CatalogSnapshot>,
    prepare_store: impl FnOnce(&ProviderStore),
) -> Harness {
    let directory = TempDir::new().expect("temp directory");
    fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700))
        .expect("private temp directory");
    let project = directory.path().join(".cookie-agent");
    fs::create_dir(&project).expect("project runtime directory");
    fs::set_permissions(&project, fs::Permissions::from_mode(0o700)).expect("private project");
    let provider_store_path = directory.path().join("provider-store");
    fs::create_dir(&provider_store_path).expect("provider store directory");
    fs::set_permissions(&provider_store_path, fs::Permissions::from_mode(0o700))
        .expect("private provider store");
    let store = ProviderStore::open(&provider_store_path).expect("provider store");
    prepare_store(&store);
    let manager =
        Arc::new(ModelManager::new(BTreeMap::new(), catalog, store).expect("model manager"));
    let config = LoadedConfiguration {
        runtime: RuntimeConfig {
            schema_version: ConfigSchemaVersion,
            server: ServerConfig::default(),
            tool_output: ToolOutputConfig::default(),
            approval: ApprovalConfig::default(),
            context_compaction: ContextCompactionConfig::default(),
            session_title: SessionTitleConfig::default(),
            delegation: cookie_agent_config::DelegationConfig::default(),
            providers: BTreeMap::new(),
        },
        agents: BTreeMap::new(),
        mcp_servers: BTreeMap::new(),
    };
    let engine = Engine::open(EngineOptions {
        data_dir: directory.path().join("data"),
        cwd: directory.path().to_owned(),
        config,
        model_manager: manager,
        tools: Vec::new(),
    })
    .expect("engine");
    let server = Arc::new(Server::new(engine.clone()));
    Harness {
        _directory: directory,
        engine,
        server,
    }
}

async fn request(
    stream: &mut InProcessStream,
    id: i64,
    method: &str,
    params: Value,
) -> (Value, Vec<Value>) {
    stream
        .send(MessageFrame::Value(json!({
            "jsonrpc": "2.0", "id": id, "method": method, "params": params
        })))
        .await
        .expect("send request");
    let mut notifications = Vec::new();
    loop {
        let frame = stream.recv().await.expect("receive").expect("stream open");
        let value = match frame {
            MessageFrame::Value(value) => value,
            MessageFrame::Text(text) => serde_json::from_str(&text).expect("JSON frame"),
        };
        if value.get("id") == Some(&json!(id)) {
            return (value, notifications);
        }
        notifications.push(value);
    }
}

async fn connect(server: Arc<Server>) -> InProcessStream {
    let (mut client, service) = in_process_pair(32);
    tokio::spawn(async move {
        server.serve_stream(service).await.expect("serve stream");
    });
    let (hello, _) = request(
        &mut client,
        1,
        "handshake",
        json!({ "protocol_version": 9 }),
    )
    .await;
    assert_eq!(hello["result"]["protocol_version"], 9);
    client
}

#[tokio::test]
async fn shared_client_handshakes_calls_and_receives_events_in_process() {
    let harness = harness();
    let client = Client::connect_in_process(Arc::clone(&harness.server));
    let mut deliveries = client.subscribe_deliveries().expect("delivery stream");

    let hello = client.handshake().await.expect("handshake");
    assert_eq!(
        hello.protocol_version,
        cookie_agent_protocol::ProtocolVersion::current()
    );
    let sessions = client
        .list_sessions(SessionListParams::default())
        .await
        .expect("session.list");
    assert!(sessions.sessions.is_empty());

    let delivery = tokio::time::timeout(std::time::Duration::from_secs(1), deliveries.recv())
        .await
        .expect("runtime event timeout")
        .expect("runtime event");
    assert!(matches!(delivery, ClientDelivery::RuntimeChanged(changed)
        if changed.previous_revision.is_none()
            && changed.reasons == vec![cookie_agent_protocol::RuntimeChangeReason::Startup]));

    client.shutdown();
    harness.server.shutdown();
    harness.engine.shutdown().await;
}

#[tokio::test]
async fn empty_runtime_snapshot_is_coherent_and_legacy_lists_are_absent() {
    let harness = harness();
    let mut client = connect(Arc::clone(&harness.server)).await;
    let (snapshot, _) = request(&mut client, 2, "runtime.snapshot.get", json!({})).await;
    assert_eq!(
        snapshot["result"]["snapshot"]["snapshot_schema_version"],
        serde_json::json!(cookie_agent_protocol::RuntimeSnapshotSchemaVersion::current())
    );
    assert_eq!(snapshot["result"]["snapshot"]["models"], json!([]));
    let agents = snapshot["result"]["snapshot"]["agents"]
        .as_array()
        .expect("agent descriptors");
    assert_eq!(agents.len(), 3);
    assert!(agents.iter().all(|agent| {
        agent["mode"] == "internal"
            && agent["runnable_as_root"] == false
            && agent["delegation_targets"] == json!([])
    }));
    assert_eq!(snapshot["result"]["snapshot"]["providers"], json!([]));

    for method in [
        "model.list",
        "agent.list",
        "catalog.provider.list",
        "catalog.model.list",
    ] {
        let (response, _) = request(&mut client, 3, method, json!({})).await;
        assert_eq!(response["error"]["code"], -32601, "{method}");
    }
}

#[tokio::test]
async fn empty_session_create_returns_secret_free_no_runnable_failure() {
    let harness = harness();
    let mut client = connect(Arc::clone(&harness.server)).await;
    let (no_runnable, _) = request(
        &mut client,
        2,
        "session.create",
        json!({
            "selection": {
                "agent": "primary",
                "model": { "model": "openai/model", "variant": null }
            }
        }),
    )
    .await;
    assert_eq!(no_runnable["error"]["data"]["code"], "no_runnable_model");
    assert_eq!(
        no_runnable["error"]["data"]["message"],
        "type /connect to continue"
    );

    let secret = "must-not-appear-in-response";
    let (response, _) = request(
        &mut client,
        3,
        "provider.connect",
        json!({
            "provider_id": "missing",
            "expected_catalog_revision": harness.engine.runtime_snapshot().expect("snapshot").snapshot.catalog_revision,
            "setup_values": {},
            "auth_method": "bearer-api-key-v1",
            "auth_values": { "api_key": secret },
            "client_connect_id": "unknown-provider"
        }),
    )
    .await;
    assert!(response.get("error").is_some());
    assert!(
        !serde_json::to_string(&response)
            .expect("serialize response")
            .contains(secret)
    );
}

#[tokio::test]
async fn disconnect_publishes_one_ordered_runtime_changed_notification() {
    let harness = harness();
    let mut client = connect(Arc::clone(&harness.server)).await;
    let snapshot = harness
        .engine
        .runtime_snapshot()
        .expect("snapshot")
        .snapshot;
    let (response, mut notifications) = request(
        &mut client,
        2,
        "provider.disconnect",
        json!({
            "provider_id": "openai",
            "expected_runtime_revision": snapshot.runtime_revision,
            "expected_provider_state_revision": snapshot.provider_state_revision,
            "expected_connection_generation": null,
            "client_request_id": "disconnect-absent"
        }),
    )
    .await;
    assert_eq!(response["result"]["disconnected"], true);
    while !notifications.iter().any(|notification| {
        notification["method"] == RUNTIME_CHANGED_METHOD
            && notification["params"]["reasons"] == json!(["provider_disconnected"])
    }) {
        let frame = tokio::time::timeout(std::time::Duration::from_secs(1), client.recv())
            .await
            .expect("runtime notification timeout")
            .expect("notification receive")
            .expect("stream open");
        notifications.push(match frame {
            MessageFrame::Value(value) => value,
            MessageFrame::Text(text) => serde_json::from_str(&text).expect("JSON notification"),
        });
    }
    let changed = notifications
        .iter()
        .find(|notification| {
            notification["method"] == RUNTIME_CHANGED_METHOD
                && notification["params"]["reasons"] == json!(["provider_disconnected"])
        })
        .expect("runtime.changed notification");
    assert_eq!(
        changed["params"]["reasons"],
        json!(["provider_disconnected"])
    );
    assert_eq!(
        changed["params"]["previous_revision"],
        json!(snapshot.runtime_revision)
    );
    assert_eq!(
        changed["params"]["snapshot"]["runtime_revision"],
        response["result"]["runtime"]["snapshot"]["runtime_revision"]
    );
}

#[tokio::test]
async fn catalog_churn_projects_supported_removed_and_reconnects_via_retained_recipe() {
    let current = openai_catalog();
    let harness = harness_with_catalog(Arc::clone(&current), |_| {});
    let mut client = connect(Arc::clone(&harness.server)).await;
    let (connected, _) = request(
        &mut client,
        2,
        "provider.connect",
        json!({
            "provider_id":"openai",
            "expected_catalog_revision":current.revision,
            "setup_values":{},
            "auth_method":"bearer-api-key-v1",
            "auth_values":{"api_key":"stored-secret"},
            "client_connect_id":"connect-before-removal"
        }),
    )
    .await;
    assert_eq!(
        connected["result"]["effective_auth_source"],
        "provider_store"
    );

    let removed_catalog = empty_catalog("b");
    harness
        .engine
        .refresh_catalog(Arc::clone(&removed_catalog))
        .expect("catalog removal refresh");
    let (snapshot, _) = request(&mut client, 3, "runtime.snapshot.get", json!({})).await;
    let provider = &snapshot["result"]["snapshot"]["providers"][0];
    assert_eq!(provider["id"], "openai");
    assert_eq!(provider["presence"], "removed");
    assert_eq!(provider["support"]["state"], "supported");
    assert_eq!(provider["support"]["reason"], Value::Null);
    assert_eq!(provider["configuration"], "stored");
    assert_eq!(provider["effective_auth_state"], "provider_store");
    assert!(provider["durable_connection"].is_object());

    let (reconnected, _) = request(
        &mut client,
        4,
        "provider.connect",
        json!({
            "provider_id":"openai",
            "expected_catalog_revision":removed_catalog.revision,
            "setup_values":{},
            "auth_method":"bearer-api-key-v1",
            "auth_values":{"api_key":"rotated-secret"},
            "client_connect_id":"reconnect-after-removal"
        }),
    )
    .await;
    assert_eq!(
        reconnected["result"]["effective_auth_source"],
        "provider_store"
    );
    assert_eq!(
        reconnected["result"]["runtime"]["providers"][0]["presence"],
        "removed"
    );
    assert_eq!(
        reconnected["result"]["runtime"]["providers"][0]["support"]["state"],
        "supported"
    );
}

#[tokio::test]
async fn unmatched_retained_policy_is_unsupported_and_reconnect_is_blocked() {
    let catalog = empty_catalog("c");
    let revision = catalog.revision.clone();
    let harness = harness_with_catalog(catalog, |store| {
        let transaction = store.begin_transaction().expect("provider transaction");
        let snapshot = transaction.snapshot();
        let mutation = ConnectMutation {
            client_connect_id: StoreClientConnectId::new("unmatched-retained").expect("connect ID"),
            provider_id: ProviderId::new("openai").expect("provider ID"),
            expected_catalog_revision: revision.clone(),
            expectation: snapshot.expectation(),
            setup_values: BTreeMap::new(),
            auth_method: AuthMethodId::new("bearer-api-key-v1").expect("auth method"),
            auth_values: ProviderAuthValues::new(BTreeMap::from([(
                AuthFieldName::new("api_key").expect("auth field"),
                "stored-secret".to_owned(),
            )]))
            .expect("auth values"),
            policy: StoredProviderPolicyProjection {
                catalog_revision: revision.clone(),
                family_id: SafePolicyString::new("openai").expect("family ID"),
                setup_recipe: ProviderSetupRecipeId::new("no-setup-v1").expect("setup recipe"),
                adapter_id: SafePolicyString::new("openai").expect("adapter ID"),
                compiler_version: RecipeCompilerVersion::new("family-registry-compiler-v1")
                    .expect("compiler version"),
                default_endpoint_identity: SafePolicyString::new("https://api.openai.com/v1")
                    .expect("endpoint"),
                package_claim: SafePolicyString::new("@ai-sdk/openai").expect("package"),
                source_record_digest: cookie_agent_models::Sha256Digest::new("d".repeat(64))
                    .expect("source digest"),
                recipe_fingerprint: cookie_agent_models::Sha256Digest::new("e".repeat(64))
                    .expect("recipe fingerprint"),
                model_overrides: BTreeMap::new(),
            },
        };
        let ConnectProposal::Proposed(proposal) = transaction
            .propose_connect(&mutation, &revision)
            .expect("connect proposal")
        else {
            panic!("unexpected replay")
        };
        transaction.commit(*proposal).expect("connect commit");
    });
    let mut client = connect(Arc::clone(&harness.server)).await;
    let (snapshot, _) = request(&mut client, 2, "runtime.snapshot.get", json!({})).await;
    let provider = &snapshot["result"]["snapshot"]["providers"][0];
    assert_eq!(provider["presence"], "removed");
    assert_eq!(provider["support"]["state"], "unsupported");
    assert_eq!(
        provider["support"]["reason"],
        "removed_without_retained_recipe_match"
    );

    let (blocked, _) = request(
        &mut client,
        3,
        "provider.connect",
        json!({
            "provider_id":"openai",
            "expected_catalog_revision":revision,
            "setup_values":{},
            "auth_method":"bearer-api-key-v1",
            "auth_values":{"api_key":"replacement"},
            "client_connect_id":"blocked-unmatched-reconnect"
        }),
    )
    .await;
    assert_eq!(
        blocked["error"]["data"]["code"],
        "removed_without_retained_recipe_match"
    );
}

#[tokio::test]
async fn catalog_shape_is_compiled_without_provider_quarantine() {
    let mut catalog = (*openai_catalog()).clone();
    let provider = catalog
        .providers
        .get_mut(&ProviderId::new("openai").expect("provider ID"))
        .expect("provider")
        .record
        .as_mut()
        .expect("provider record");
    provider.shape = Some("unexpected".to_owned());
    let catalog = Arc::new(catalog);
    let harness = harness_with_catalog(Arc::clone(&catalog), |_| {});
    let mut client = connect(Arc::clone(&harness.server)).await;
    let (response, _) = request(
        &mut client,
        2,
        "provider.connect",
        json!({
            "provider_id":"openai",
            "expected_catalog_revision":catalog.revision,
            "setup_values":{},
            "auth_method":"bearer-api-key-v1",
            "auth_values":{"api_key":"secret"},
            "client_connect_id":"quarantined-provider"
        }),
    )
    .await;
    assert!(response.get("result").is_some(), "{response}");
}

#[tokio::test]
async fn protocol_eight_and_unknown_params_are_rejected() {
    let harness = harness();
    let (mut client, service) = in_process_pair(8);
    tokio::spawn(async move {
        harness
            .server
            .serve_stream(service)
            .await
            .expect("serve stream");
    });
    let (response, _) = request(
        &mut client,
        1,
        "handshake",
        json!({ "protocol_version": 8 }),
    )
    .await;
    assert_eq!(response["error"]["code"], -32602);
}
