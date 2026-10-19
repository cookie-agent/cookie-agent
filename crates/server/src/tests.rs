use std::{collections::BTreeMap, fs, sync::Arc};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt as _;

use cookie_agent_config::{
    ApprovalConfig, ContextCompactionConfig, EngineConfig, LoadedConfiguration, LoadedMcpServer,
    McpServerSource, ServerConfig, SessionTitleConfig, ToolOutputConfig,
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
    AuthFieldName, AuthMethodId, CatalogRevision, McpAuthBeginParams, McpAuthCancelParams,
    McpAuthCompleteParams, McpConfigSource, McpServerAddParams, McpServerDefinition,
    McpServerEditParams, McpServerNameParams, ProviderId, ProviderSetupRecipeId,
    RUNTIME_CHANGED_METHOD, RecipeCompilerVersion, SessionListParams,
};
use jiff::Timestamp;
use serde_json::{Value, json};
use tempfile::TempDir;

use cookie_agent_protocol::{
    Authentication, CLIENT_PROTOCOL_VERSION, ClientDelivery, InProcessStream, MessageFrame,
    MessageStream, in_process_pair,
};

use crate::Server;

pub(crate) struct Harness {
    _directory: TempDir,
    pub(crate) engine: Engine,
    pub(crate) server: Arc<Server>,
}

pub(crate) fn private_tempdir() -> TempDir {
    let directory = TempDir::new().expect("temp directory");
    #[cfg(unix)]
    fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700))
        .expect("private temp directory");
    #[cfg(windows)]
    {
        fs::remove_dir(directory.path()).expect("remove ordinary temp directory");
        cookie_agent_models::secure_store::SecureDirectory::open(directory.path())
            .expect("private temp directory");
    }
    directory
}

pub(crate) fn create_private_test_dir(path: &std::path::Path) {
    #[cfg(unix)]
    {
        fs::create_dir(path).expect("private test directory");
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .expect("private test directory");
    }
    #[cfg(windows)]
    cookie_agent_models::secure_store::SecureDirectory::open(path).expect("private test directory");
}

#[tokio::test]
async fn plugin_events_are_filtered_by_connection_session_subscription() {
    let harness = harness();
    let session_a = cookie_agent_protocol::SessionId::new_v7();
    let session_b = cookie_agent_protocol::SessionId::new_v7();
    let (context_a, mut notifications_a) = cookie_agent_protocol::test_server_context();
    let (context_b, mut notifications_b) = cookie_agent_protocol::test_server_context();
    context_a.register_session_subscription(session_a);
    context_b.register_session_subscription(session_b);
    harness.server.start_engine_event_notifications(context_a);
    harness.server.start_engine_event_notifications(context_b);

    harness
        .engine
        .publish_engine_event_for_test(cookie_agent_engine::EngineEvent::PluginEvent {
            session_id: session_a,
            plugin: "fixture".into(),
            name: "session_a".into(),
            payload: serde_json::json!({}),
        });
    let notification =
        tokio::time::timeout(std::time::Duration::from_secs(1), notifications_a.recv())
            .await
            .expect("session A notification")
            .expect("session A queue");
    let notification: Value = serde_json::from_str(&notification).expect("JSON notification");
    assert_eq!(notification["method"], "events.plugin");
    assert_eq!(notification["params"]["session_id"], session_a.to_string());
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(50), notifications_b.recv())
            .await
            .is_err(),
        "session A event leaked to session B connection"
    );
    harness.server.shutdown();
    harness.engine.shutdown().await;
}

pub(crate) fn harness() -> Harness {
    harness_with_mcp(BTreeMap::new())
}

fn harness_with_mcp(mcp_servers: BTreeMap<String, LoadedMcpServer>) -> Harness {
    let directory = private_tempdir();
    let project = directory.path().join(".cookie-agent");
    create_private_test_dir(&project);
    let provider_store = directory.path().join("provider-store");
    create_private_test_dir(&provider_store);

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
        providers: Arc::new(BTreeMap::new()),
        canonical_models: Arc::new(BTreeMap::new()),
        quarantine: Arc::new(Vec::new()),
    });
    let manager = Arc::new(
        ModelManager::new(
            BTreeMap::new(),
            catalog,
            ProviderStore::open(&provider_store).expect("provider store"),
        )
        .expect("empty model manager"),
    );
    let user_mcp_servers = mcp_servers
        .iter()
        .filter(|(_, server)| server.source == McpServerSource::UserFile)
        .map(|(name, server)| (name.clone(), server.config.clone()))
        .collect();
    let config = LoadedConfiguration {
        runtime: EngineConfig {
            server: ServerConfig::default(),
            tool_output: ToolOutputConfig::default(),
            agent_md: cookie_agent_config::AgentMdConfig::default(),
            loop_warning: cookie_agent_config::LoopWarningConfig::default(),
            approval: ApprovalConfig::default(),
            model_retry: cookie_agent_config::ModelRetryConfig::default(),
            context_compaction: ContextCompactionConfig::default(),
            session_title: SessionTitleConfig::default(),
            delegation: cookie_agent_config::DelegationConfig::default(),
            messaging: cookie_agent_config::MessagingConfig::default(),
            producers: cookie_agent_config::ProducersConfig::default(),
            memory: cookie_agent_config::MemoryConfig::default(),
            pricing: cookie_agent_config::PricingConfig::default(),
            headers: BTreeMap::new(),
            providers: BTreeMap::new(),
            subagent_output: Default::default(),
        },
        agents: BTreeMap::new(),
        agent_presets: BTreeMap::new(),
        mcp_servers,
        user_mcp_servers,
        plugins: Default::default(),
        runtimes: Default::default(),
        config_paths: cookie_agent_config::ConfigLayerPaths::default(),
        revision: cookie_agent_protocol::ConfigRevision::new(format!("sha256:{}", "0".repeat(64)))
            .unwrap(),
        skills: cookie_agent_config::SkillRegistry::default(),
    };
    let engine = Engine::open(EngineOptions {
        data_dir: directory.path().join("data"),
        cwd: directory.path().to_owned(),
        config,
        model_manager: manager,
        runtimes: cookie_agent_runtime_api::RuntimeOptions::new(directory.path()),
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
async fn mcp_management_is_reachable_through_protocol() {
    let harness = harness();
    let client = Arc::clone(&harness.server).connect_in_process();
    client.handshake().await.expect("handshake");
    let runtime_definition = McpServerDefinition {
        command: Some("runtime-server".into()),
        args: vec!["--stdio".into()],
        env: BTreeMap::new(),
        cwd: None,
        url: None,
        headers: BTreeMap::new(),
        oauth: None,
        enabled: true,
        timeout_ms: Some(1000),
    };
    client
        .add_mcp_server(McpServerAddParams {
            name: "runtime".into(),
            definition: runtime_definition.clone(),
        })
        .await
        .expect("add runtime MCP server");
    let runtime = client
        .list_mcp_servers()
        .await
        .expect("list MCP servers")
        .servers
        .into_iter()
        .find(|server| server.name == "runtime")
        .expect("runtime server");
    assert_eq!(runtime.source, McpConfigSource::Runtime);
    let auth_error = client
        .begin_mcp_auth(McpAuthBeginParams {
            server: "runtime".into(),
            redirect_uri: "http://127.0.0.1:9/callback".into(),
        })
        .await
        .expect_err("stdio server cannot begin OAuth");
    assert!(auth_error.error_code().is_some(), "{auth_error}");
    assert!(!auth_error.to_string().is_empty());
    let complete_error = client
        .complete_mcp_auth(McpAuthCompleteParams {
            server: "runtime".into(),
            callback_url: "http://127.0.0.1:9/callback?code=c&state=s".into(),
        })
        .await
        .expect_err("no OAuth flow to complete");
    assert!(complete_error.error_code().is_some(), "{complete_error}");
    assert!(!complete_error.to_string().is_empty());
    let cancel_error = client
        .cancel_mcp_auth(McpAuthCancelParams {
            server: "runtime".into(),
        })
        .await
        .expect_err("no OAuth flow to cancel");
    assert!(cancel_error.error_code().is_some(), "{cancel_error}");
    assert!(!cancel_error.to_string().is_empty());
    let mut edited = runtime_definition;
    edited.args.push("--edited".into());
    client
        .edit_mcp_server(McpServerEditParams {
            name: "runtime".into(),
            definition: edited,
        })
        .await
        .expect("edit runtime MCP server");
    client
        .remove_mcp_server(McpServerNameParams {
            name: "runtime".into(),
        })
        .await
        .expect("remove runtime MCP server");
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
        providers: Arc::new(BTreeMap::new()),
        canonical_models: Arc::new(BTreeMap::new()),
        quarantine: Arc::new(Vec::new()),
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
        cost: None,
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
        providers: Arc::new(BTreeMap::from([(
            provider_id.clone(),
            CatalogProviderEntry {
                id: provider_id,
                record: Some(record),
                quarantine: None,
            },
        )])),
        canonical_models: Arc::new(BTreeMap::new()),
        quarantine: Arc::new(Vec::new()),
    })
}

fn harness_with_catalog(
    catalog: Arc<CatalogSnapshot>,
    prepare_store: impl FnOnce(&ProviderStore),
) -> Harness {
    harness_with_catalog_and_mcp(catalog, prepare_store, BTreeMap::new())
}

fn harness_with_catalog_and_mcp(
    catalog: Arc<CatalogSnapshot>,
    prepare_store: impl FnOnce(&ProviderStore),
    mcp_servers: BTreeMap<String, LoadedMcpServer>,
) -> Harness {
    let directory = private_tempdir();
    let project = directory.path().join(".cookie-agent");
    create_private_test_dir(&project);
    let provider_store_path = directory.path().join("provider-store");
    create_private_test_dir(&provider_store_path);
    let store = ProviderStore::open(&provider_store_path).expect("provider store");
    prepare_store(&store);
    let manager =
        Arc::new(ModelManager::new(BTreeMap::new(), catalog, store).expect("model manager"));
    let config = LoadedConfiguration {
        runtime: EngineConfig {
            server: ServerConfig::default(),
            tool_output: ToolOutputConfig::default(),
            agent_md: cookie_agent_config::AgentMdConfig::default(),
            loop_warning: cookie_agent_config::LoopWarningConfig::default(),
            approval: ApprovalConfig::default(),
            model_retry: cookie_agent_config::ModelRetryConfig::default(),
            context_compaction: ContextCompactionConfig::default(),
            session_title: SessionTitleConfig::default(),
            delegation: cookie_agent_config::DelegationConfig::default(),
            messaging: cookie_agent_config::MessagingConfig::default(),
            producers: cookie_agent_config::ProducersConfig::default(),
            memory: cookie_agent_config::MemoryConfig::default(),
            pricing: cookie_agent_config::PricingConfig::default(),
            headers: BTreeMap::new(),
            providers: BTreeMap::new(),
            subagent_output: Default::default(),
        },
        agents: BTreeMap::new(),
        agent_presets: BTreeMap::new(),
        user_mcp_servers: mcp_servers
            .iter()
            .map(|(name, server)| (name.clone(), server.config.clone()))
            .collect(),
        mcp_servers,
        plugins: Default::default(),
        runtimes: Default::default(),
        config_paths: cookie_agent_config::ConfigLayerPaths::default(),
        revision: cookie_agent_protocol::ConfigRevision::new(format!("sha256:{}", "0".repeat(64)))
            .unwrap(),
        skills: cookie_agent_config::SkillRegistry::default(),
    };
    let engine = Engine::open(EngineOptions {
        data_dir: directory.path().join("data"),
        cwd: directory.path().to_owned(),
        config,
        model_manager: manager,
        runtimes: cookie_agent_runtime_api::RuntimeOptions::new(directory.path()),
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
            MessageFrame::Bytes(bytes) => serde_json::from_slice(&bytes).expect("JSON frame"),
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
        server
            .serve_stream(service, Authentication::Authenticated)
            .await
            .expect("serve stream");
    });
    let (hello, _) = request(
        &mut client,
        1,
        "handshake",
        json!({
            "protocol_version": CLIENT_PROTOCOL_VERSION,
            "client": {
                "kind": "test",
                "instance_id": "0192f3a4-5b6c-7d8e-9f00-112233445566",
            },
        }),
    )
    .await;
    assert_eq!(
        hello["result"]["protocol_version"],
        CLIENT_PROTOCOL_VERSION.to_string()
    );
    assert_eq!(hello["result"]["engine_version"], env!("CARGO_PKG_VERSION"));
    // The daemon stamps this connection's events with an origin derived
    // from the handshake identity.
    assert_eq!(
        hello["result"]["origin"],
        "client:test-0192f3a45b6c7d8e9f00112233445566"
    );
    client
}

#[tokio::test]
async fn shared_client_handshakes_calls_and_receives_events_in_process() {
    let harness = harness();
    let client = Arc::clone(&harness.server).connect_in_process();
    let mut deliveries = client.subscribe_deliveries().expect("delivery stream");

    let hello = client.handshake().await.expect("handshake");
    assert_eq!(hello.protocol_version, CLIENT_PROTOCOL_VERSION);
    assert_eq!(client.daemon_version(), Some(CLIENT_PROTOCOL_VERSION));
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
    let view = &snapshot["result"]["snapshot"];
    assert!(view["revision"].is_string());
    assert!(view.get("snapshot_schema_version").is_none());
    assert!(view["catalog"]["source"].is_string());
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
async fn cross_session_usage_rollups_are_not_served() {
    let harness = harness();
    let mut client = connect(Arc::clone(&harness.server)).await;
    // Usage is only ever read for one session or one session tree.
    for (method, params) in [
        ("agent.usage", json!({"agent_id": "default"})),
        ("usage.global", json!({})),
    ] {
        let (response, _) = request(&mut client, 3, method, params).await;
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
    assert_eq!(no_runnable["error"]["code"], -32000);
    assert_eq!(no_runnable["error"]["data"]["code"], "no_runnable_model");
    assert_eq!(
        no_runnable["error"]["data"]["hint"],
        cookie_agent_protocol::errors::NO_RUNNABLE_MODEL
            .hint
            .unwrap()
    );

    let secret = "must-not-appear-in-response";
    let (response, _) = request(
        &mut client,
        3,
        "provider.connect",
        json!({
            "provider_id": "missing",
            "expected_revision": harness.engine.runtime_snapshot().expect("view").snapshot.revision,
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
            "expected_revision": snapshot.revision,
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
            MessageFrame::Bytes(bytes) => {
                serde_json::from_slice(&bytes).expect("JSON notification")
            }
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
        json!(snapshot.revision)
    );
    assert_eq!(
        changed["params"]["snapshot"]["revision"],
        response["result"]["runtime"]["snapshot"]["revision"]
    );
}

#[tokio::test]
async fn goal_and_producer_inspection_are_reachable_through_rpc() {
    let mut catalog = (*openai_catalog()).clone();
    Arc::make_mut(&mut catalog.providers)
        .values_mut()
        .next()
        .expect("OpenAI provider")
        .record
        .as_mut()
        .expect("OpenAI provider record")
        .api = Some("http://127.0.0.1:9/v1".into());
    let catalog = Arc::new(catalog);
    let harness = harness_with_catalog(Arc::clone(&catalog), |_| {});
    let mut client = connect(Arc::clone(&harness.server)).await;
    let (connected, _) = request(
        &mut client,
        2,
        "provider.connect",
        json!({
            "provider_id":"openai",
            "expected_revision": harness.engine.runtime_snapshot().expect("view").snapshot.revision,
            "setup_values":{},
            "auth_method":"bearer-api-key-v1",
            "auth_values":{"api_key":"test-secret"},
            "client_connect_id":"goal-rpc-test"
        }),
    )
    .await;
    assert!(connected.get("result").is_some(), "{connected}");

    let (created, _) = request(
        &mut client,
        3,
        "session.create",
        json!({
            "selection": {
                "agent": "default",
                "model": { "model": "openai/gpt-5-mini", "variant": null }
            }
        }),
    )
    .await;
    let session_id = created["result"]["session"]["session_id"]
        .as_str()
        .expect("created session ID");

    let (activated, _) = request(
        &mut client,
        4,
        cookie_agent_protocol::SESSION_GOAL_SET_METHOD,
        json!({"session_id":session_id,"objective":"Verify the goal RPC surface"}),
    )
    .await;
    let goal_id = activated["result"]["goal"]["goal_id"]
        .as_str()
        .expect("activated goal ID");
    let revision = activated["result"]["goal"]["revision"]
        .as_u64()
        .expect("activated goal revision");

    let (paused, _) = request(
        &mut client,
        5,
        cookie_agent_protocol::SESSION_GOAL_LIFECYCLE_METHOD,
        json!({
            "session_id":session_id,
            "goal_id":goal_id,
            "expected_revision":revision,
            "action":"pause"
        }),
    )
    .await;
    assert_eq!(paused["result"]["goal"]["status"], "paused");

    let (goal, _) = request(
        &mut client,
        6,
        cookie_agent_protocol::SESSION_GOAL_GET_METHOD,
        json!({"session_id":session_id}),
    )
    .await;
    assert_eq!(goal["result"]["goal"], paused["result"]["goal"]);

    let (producers, _) = request(
        &mut client,
        7,
        cookie_agent_protocol::SESSION_PRODUCERS_METHOD,
        json!({"session_id":session_id}),
    )
    .await;
    assert_eq!(producers["result"]["producers"], json!([]));
    assert_eq!(producers["result"]["sources"], json!([]));

    harness.server.shutdown();
    harness.engine.shutdown().await;
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
            "expected_revision": harness.engine.runtime_snapshot().expect("view").snapshot.revision,
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
            "expected_revision": harness.engine.runtime_snapshot().expect("view").snapshot.revision,
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
            "expected_revision": harness.engine.runtime_snapshot().expect("view").snapshot.revision,
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
    let provider = Arc::make_mut(&mut catalog.providers)
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
            "expected_revision": harness.engine.runtime_snapshot().expect("view").snapshot.revision,
            "setup_values":{},
            "auth_method":"bearer-api-key-v1",
            "auth_values":{"api_key":"secret"},
            "client_connect_id":"quarantined-provider"
        }),
    )
    .await;
    assert!(response.get("result").is_some(), "{response}");
}

/// A client of another major, of a build from before the freeze, or of a
/// build from before semver versions is refused with `unsupported_protocol`
/// naming both versions; another minor or patch of the same major connects.
#[tokio::test]
async fn version_skew_is_refused_with_both_versions_and_another_minor_connects() {
    let harness = harness();
    let (mut client, service) = in_process_pair(8);
    let server = Arc::clone(&harness.server);
    tokio::spawn(async move {
        server
            .serve_stream(service, Authentication::Authenticated)
            .await
            .expect("serve stream");
    });
    let current = CLIENT_PROTOCOL_VERSION;
    let client_identity = json!({
        "kind": "test",
        "instance_id": "0192f3a4-5b6c-7d8e-9f00-112233445566",
    });
    for (id, version, named) in [
        (1, json!("0.52.0"), "0.52.0".to_owned()),
        (
            2,
            json!(format!("{}.{}.0", current.major() + 1, current.minor())),
            format!("{}.{}.0", current.major() + 1, current.minor()),
        ),
        (3, json!(50), "50".to_owned()),
    ] {
        let (response, _) = request(
            &mut client,
            id,
            "handshake",
            json!({ "protocol_version": version, "client": client_identity }),
        )
        .await;
        assert_eq!(response["error"]["code"], -32002, "{response}");
        let data = &response["error"]["data"];
        assert_eq!(data["code"], "unsupported_protocol", "{response}");
        assert_eq!(data["supported_version"], current.to_string());
        assert_eq!(data["peer_version"], named);
    }
    // The rest of a hello is a command: an unknown member is refused.
    let (response, _) = request(
        &mut client,
        4,
        "handshake",
        json!({
            "protocol_version": current,
            "client": client_identity,
            "extra": true,
        }),
    )
    .await;
    assert_eq!(
        response["error"]["data"]["code"], "unknown_field",
        "{response}"
    );
    let (response, _) = request(
        &mut client,
        5,
        "handshake",
        json!({
            "protocol_version": format!("{}.{}.{}", current.major(), current.minor() + 3, current.patch() + 7),
            "client": client_identity,
            "features": ["from_the_future"],
        }),
    )
    .await;
    assert_eq!(
        response["result"]["protocol_version"],
        current.to_string(),
        "{response}"
    );
    harness.server.shutdown();
    harness.engine.shutdown().await;
}

#[tokio::test]
async fn runtime_attachments_are_reachable_through_rpc() {
    let mut catalog = (*openai_catalog()).clone();
    Arc::make_mut(&mut catalog.providers)
        .values_mut()
        .next()
        .expect("OpenAI provider")
        .record
        .as_mut()
        .expect("OpenAI provider record")
        .api = Some("http://127.0.0.1:9/v1".into());
    let catalog = Arc::new(catalog);
    let harness = harness_with_catalog(Arc::clone(&catalog), |_| {});
    let mut client = connect(Arc::clone(&harness.server)).await;
    let (connected, _) = request(
        &mut client,
        2,
        "provider.connect",
        json!({
            "provider_id":"openai",
            "expected_revision": harness.engine.runtime_snapshot().expect("view").snapshot.revision,
            "setup_values":{},
            "auth_method":"bearer-api-key-v1",
            "auth_values":{"api_key":"test-secret"},
            "client_connect_id":"runtime-rpc-test"
        }),
    )
    .await;
    assert!(connected.get("result").is_some(), "{connected}");
    let selection = json!({
        "agent": "default",
        "model": { "model": "openai/gpt-5-mini", "variant": null }
    });

    let (listed, _) = request(&mut client, 3, "runtimes.list", json!({})).await;
    let runtimes = listed["result"]["runtimes"].as_array().expect("runtimes");
    assert_eq!(
        runtimes
            .iter()
            .map(|runtime| (
                runtime["runtime_id"].as_str().unwrap(),
                runtime["lifetime"].as_str().unwrap(),
                runtime["capabilities"][0]["name"].as_str().unwrap(),
                runtime["capabilities"][0]["takes_workspace"]
                    .as_bool()
                    .unwrap()
            ))
            .collect::<Vec<_>>(),
        [
            ("global", "builtin", "global", false),
            ("local", "builtin", "coding", true)
        ]
    );
    assert!(
        listed["result"]["instances"]
            .as_array()
            .expect("instances")
            .iter()
            .all(|instance| instance["status"] == "ready")
    );

    // The default: `global`, then `local`.
    let (created, _) = request(
        &mut client,
        4,
        "session.create",
        json!({ "selection": selection.clone() }),
    )
    .await;
    let names = |runtimes: &serde_json::Value| {
        runtimes
            .as_array()
            .expect("runtimes")
            .iter()
            .map(|runtime| runtime["name"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>()
    };
    let defaults = names(&created["result"]["runtimes"]);
    assert_eq!(defaults.len(), 2);
    assert_eq!(defaults[0], "global");
    assert!(defaults[1].starts_with("local"));

    // No runtimes at all.
    let (bare, _) = request(
        &mut client,
        5,
        "session.create",
        json!({ "selection": selection.clone(), "runtimes": [], "global": false }),
    )
    .await;
    assert!(names(&bare["result"]["runtimes"]).is_empty(), "{bare}");
    let session_id = bare["result"]["session"]["session_id"].clone();

    let (attached, _) = request(
        &mut client,
        6,
        "session.runtime.attach",
        json!({ "session_id": session_id, "runtime_id": "global" }),
    )
    .await;
    assert_eq!(
        attached["result"]["attachment"]["name"], "global",
        "{attached}"
    );
    assert_eq!(names(&attached["result"]["runtimes"]), ["global"]);
    for (id, params, detail) in [
        (
            7,
            json!({ "session_id": session_id, "runtime_id": "labpc" }),
            "unknown runtime `labpc`",
        ),
        (
            8,
            json!({ "session_id": session_id, "runtime_id": "local", "capability": "browse" }),
            "runtime `local` offers no capability `browse`",
        ),
    ] {
        let (rejected, _) = request(&mut client, id, "session.runtime.attach", params).await;
        assert!(
            rejected["error"]["data"]["detail"]
                .as_str()
                .is_some_and(|message| message.contains(detail)),
            "{rejected}"
        );
    }
    let (detached, _) = request(
        &mut client,
        9,
        "session.runtime.detach",
        json!({ "session_id": session_id, "name": "global" }),
    )
    .await;
    assert!(
        names(&detached["result"]["runtimes"]).is_empty(),
        "{detached}"
    );
    let (fetched, _) = request(
        &mut client,
        10,
        "session.get",
        json!({ "session_id": session_id }),
    )
    .await;
    assert!(
        names(&fetched["result"]["runtimes"]).is_empty(),
        "{fetched}"
    );

    // Retrying a ready instance leaves it as it is.
    let (reconnected, _) = request(
        &mut client,
        11,
        "runtimes.reconnect",
        json!({ "runtime_id": "local" }),
    )
    .await;
    assert_eq!(
        reconnected["result"]["instance"]["status"], "ready",
        "{reconnected}"
    );
    assert_eq!(reconnected["result"]["instance"]["runtime_id"], "local");
    let (rejected, _) = request(
        &mut client,
        12,
        "runtimes.reconnect",
        json!({ "runtime_id": "labpc", "workspace": "~/api" }),
    )
    .await;
    assert!(
        rejected["error"]["data"]["detail"]
            .as_str()
            .is_some_and(|message| message.contains("unknown runtime `labpc`")),
        "{rejected}"
    );
}

/// Reads frames until a `method` notification that `accept` takes.
async fn notification_where(
    stream: &mut InProcessStream,
    method: &str,
    accept: impl Fn(&Value) -> bool,
) -> Value {
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        loop {
            let frame = stream.recv().await.expect("receive").expect("stream open");
            let value = match frame {
                MessageFrame::Value(value) => value,
                MessageFrame::Text(text) => serde_json::from_str(&text).expect("JSON frame"),
                MessageFrame::Bytes(bytes) => serde_json::from_slice(&bytes).expect("JSON frame"),
            };
            if value["method"] == method && accept(&value["params"]) {
                return value["params"].clone();
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("no matching {method} notification"))
}

#[tokio::test]
async fn mcp_connection_failures_reach_clients_viewing_affected_sessions() {
    let mut catalog = (*openai_catalog()).clone();
    Arc::make_mut(&mut catalog.providers)
        .values_mut()
        .next()
        .expect("OpenAI provider")
        .record
        .as_mut()
        .expect("OpenAI provider record")
        .api = Some("http://127.0.0.1:9/v1".into());
    let catalog = Arc::new(catalog);
    let missing = private_tempdir();
    let harness = harness_with_catalog_and_mcp(
        Arc::clone(&catalog),
        |_| {},
        BTreeMap::from([(
            "broken".into(),
            LoadedMcpServer {
                source: McpServerSource::UserFile,
                config: cookie_agent_config::McpServerConfig {
                    command: Some(missing.path().join("no-such-server").display().to_string()),
                    args: Vec::new(),
                    env: BTreeMap::new(),
                    cwd: None,
                    url: None,
                    headers: BTreeMap::new(),
                    oauth: Default::default(),
                    enabled: true,
                    timeout_ms: Some(5_000),
                },
            },
        )]),
    );
    let mut client = connect(Arc::clone(&harness.server)).await;
    let (connected, _) = request(
        &mut client,
        2,
        "provider.connect",
        json!({
            "provider_id":"openai",
            "expected_revision": harness.engine.runtime_snapshot().expect("view").snapshot.revision,
            "setup_values":{},
            "auth_method":"bearer-api-key-v1",
            "auth_values":{"api_key":"test-secret"},
            "client_connect_id":"mcp-failure-test"
        }),
    )
    .await;
    assert!(connected.get("result").is_some(), "{connected}");
    let selection = json!({
        "agent": "default",
        "model": { "model": "openai/gpt-5-mini", "variant": null }
    });
    let (created, _) = request(
        &mut client,
        3,
        "session.create",
        json!({ "selection": selection.clone() }),
    )
    .await;
    let session_id = created["result"]["session"]["session_id"].clone();
    let (bare, _) = request(
        &mut client,
        4,
        "session.create",
        json!({ "selection": selection, "runtimes": [], "global": false }),
    )
    .await;
    let bare_id = bare["result"]["session"]["session_id"].clone();
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        while !harness.engine.mcp_statuses().iter().any(|status| {
            status.server == "broken" && status.state == cookie_agent_engine::McpServerState::Failed
        }) {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the retry at creation fails");

    // Subscribing tells the client about the failure it missed.
    let (subscribed, _) = request(
        &mut client,
        5,
        "events.subscribe",
        json!({ "session_id": session_id }),
    )
    .await;
    assert!(subscribed.get("result").is_some(), "{subscribed}");
    let replayed = notification_where(&mut client, "mcp.changed", |params| {
        params.get("failures").is_some()
    })
    .await;
    assert_eq!(replayed["failures"][0]["server"], "broken");
    assert_eq!(replayed["failures"][0]["runtime_id"], "global");
    assert_eq!(replayed["failures"][0]["session_ids"], json!([session_id]));
    assert!(
        replayed["failures"][0]["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("no-such-server")),
        "{replayed}"
    );

    // A session without the global runtime is never named.
    let (subscribed, _) = request(
        &mut client,
        6,
        "events.subscribe",
        json!({ "session_id": bare_id }),
    )
    .await;
    assert!(subscribed.get("result").is_some(), "{subscribed}");

    // Opening the session retries the server; the new failure names it.
    let (resumed, _) = request(
        &mut client,
        7,
        "session.resume",
        json!({ "session_id": session_id }),
    )
    .await;
    assert!(resumed.get("result").is_some(), "{resumed}");
    let reported = notification_where(&mut client, "mcp.changed", |params| {
        params.get("failures").is_some()
    })
    .await;
    assert_eq!(reported["failures"][0]["session_ids"], json!([session_id]));
    harness.server.shutdown();
    harness.engine.shutdown().await;
}

/// A 1×1 PNG.
const PIXEL_PNG: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x04, 0x00, 0x00, 0x00, 0xb5, 0x1c, 0x0c,
    0x02, 0x00, 0x00, 0x00, 0x0b, 0x49, 0x44, 0x41, 0x54, 0x78, 0xda, 0x63, 0x64, 0xf8, 0x0f, 0x00,
    0x01, 0x05, 0x01, 0x01, 0x27, 0x18, 0xe3, 0x66, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44,
    0xae, 0x42, 0x60, 0x82,
];

fn upload_temporaries(directory: &std::path::Path) -> usize {
    let mut found = 0;
    for entry in fs::read_dir(directory).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.is_dir() {
            found += upload_temporaries(&path);
        } else if entry.file_name().to_string_lossy().starts_with(".upload-") {
            found += 1;
        }
    }
    found
}

#[tokio::test]
async fn media_uploads_and_reads_travel_as_base64_and_end_with_their_connection() {
    use base64::Engine as _;
    use sha2::Digest as _;
    let base64 = base64::engine::general_purpose::STANDARD;
    let mut catalog = (*openai_catalog()).clone();
    Arc::make_mut(&mut catalog.providers)
        .values_mut()
        .next()
        .expect("OpenAI provider")
        .record
        .as_mut()
        .expect("OpenAI provider record")
        .api = Some("http://127.0.0.1:9/v1".into());
    let harness = harness_with_catalog(Arc::new(catalog), |_| {});
    let mut client = connect(Arc::clone(&harness.server)).await;
    let (connected, _) = request(
        &mut client,
        2,
        "provider.connect",
        json!({
            "provider_id":"openai",
            "expected_revision": harness.engine.runtime_snapshot().expect("view").snapshot.revision,
            "setup_values":{},
            "auth_method":"bearer-api-key-v1",
            "auth_values":{"api_key":"test-secret"},
            "client_connect_id":"upload-rpc-test"
        }),
    )
    .await;
    assert!(connected.get("result").is_some(), "{connected}");
    let (created, _) = request(
        &mut client,
        3,
        "session.create",
        json!({"selection": {"agent": "default", "model": {"model": "openai/gpt-5-mini", "variant": null}}}),
    )
    .await;
    let session_id = created["result"]["session"]["session_id"].clone();

    let (begun, _) = request(
        &mut client,
        4,
        "artifact.upload.begin",
        json!({"session_id": session_id, "byte_length": PIXEL_PNG.len(), "filename": "pixel.png"}),
    )
    .await;
    assert_eq!(begun["result"]["chunk_bytes"], 1024 * 1024, "{begun}");
    let upload_id = begun["result"]["upload_id"].clone();
    let (bad, _) = request(
        &mut client,
        5,
        "artifact.upload.chunk",
        json!({"upload_id": upload_id, "offset": 0, "data": "not base64!"}),
    )
    .await;
    assert_eq!(bad["error"]["code"], -32602, "{bad}");
    let (gap, _) = request(
        &mut client,
        6,
        "artifact.upload.chunk",
        json!({"upload_id": upload_id, "offset": 3, "data": base64.encode(PIXEL_PNG)}),
    )
    .await;
    assert_eq!(
        gap["error"]["data"]["code"], "upload_offset_mismatch",
        "{gap}"
    );
    assert_eq!(gap["error"]["data"]["expected_offset"], 0, "{gap}");
    let (chunk, _) = request(
        &mut client,
        7,
        "artifact.upload.chunk",
        json!({"upload_id": upload_id, "offset": 0, "data": base64.encode(PIXEL_PNG)}),
    )
    .await;
    assert_eq!(chunk["result"], json!({}), "{chunk}");
    let sha256 = format!("{:x}", sha2::Sha256::digest(PIXEL_PNG));
    let (finished, _) = request(
        &mut client,
        8,
        "artifact.upload.finish",
        json!({"upload_id": upload_id, "sha256": sha256}),
    )
    .await;
    assert_eq!(
        finished["result"],
        json!({
            "artifact": {"sha256": sha256, "byte_length": PIXEL_PNG.len(),
                         "media_type": "image/png", "filename": "pixel.png"},
            "image": {"width": 1, "height": 1}
        }),
        "{finished}"
    );
    let (read, _) = request(
        &mut client,
        9,
        "artifact.read",
        json!({"session_id": session_id, "sha256": sha256, "offset": 8, "length": 8}),
    )
    .await;
    assert_eq!(
        read["result"],
        json!({"data": base64.encode(&PIXEL_PNG[8..16]), "total_bytes": PIXEL_PNG.len(),
               "media_type": "image/png"}),
        "{read}"
    );
    let (missing, _) = request(
        &mut client,
        10,
        "artifact.read",
        json!({"session_id": session_id, "sha256": "0".repeat(64), "offset": 0, "length": 8}),
    )
    .await;
    assert_eq!(
        missing["error"]["data"]["code"], "artifact_not_found",
        "{missing}"
    );
    let (unknown, _) = request(
        &mut client,
        11,
        "artifact.upload.finish",
        json!({"upload_id": upload_id, "sha256": sha256}),
    )
    .await;
    assert_eq!(
        unknown["error"]["data"]["code"], "upload_not_found",
        "{unknown}"
    );
    let (large, _) = request(
        &mut client,
        12,
        "artifact.upload.begin",
        json!({"session_id": session_id, "byte_length": 25 * 1024 * 1024 + 1}),
    )
    .await;
    assert_eq!(large["error"]["data"]["code"], "media_too_large", "{large}");
    assert_eq!(
        large["error"]["data"]["max_bytes"],
        25 * 1024 * 1024,
        "{large}"
    );

    // An upload left in progress ends with its connection.
    let (begun, _) = request(
        &mut client,
        13,
        "artifact.upload.begin",
        json!({"session_id": session_id, "byte_length": 100}),
    )
    .await;
    assert!(begun.get("result").is_some(), "{begun}");
    assert_eq!(upload_temporaries(harness._directory.path()), 1);
    drop(client);
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while upload_temporaries(harness._directory.path()) != 0 {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the closed connection's upload is dropped");
    harness.server.shutdown();
    harness.engine.shutdown().await;
}

#[tokio::test]
async fn machines_are_listed_changed_and_announced_through_rpc() {
    let harness = harness_with_mcp(BTreeMap::new());
    let mut client = connect(Arc::clone(&harness.server)).await;
    let own = cookie_agent_protocol::runtime_protocol::RuntimeMachine::this_machine()
        .expect("this machine has a machine ID");

    // `global` and `local` share the daemon's machine.
    let (listed, _) = request(&mut client, 2, "machines.list", json!({})).await;
    let machines = listed["result"]["machines"].as_array().expect("machines");
    assert_eq!(machines.len(), 1, "{listed}");
    let daemon = machines[0]["name"].as_str().unwrap().to_owned();
    assert_eq!(machines[0]["ids"], json!([own.id]));
    assert_eq!(
        machines[0]["connected"]
            .as_array()
            .unwrap()
            .iter()
            .map(|runtime| (
                runtime["runtime_id"].as_str().unwrap(),
                runtime["assigned_by"].as_str().unwrap()
            ))
            .collect::<Vec<_>>(),
        [("global", "machine_id"), ("local", "machine_id")]
    );
    let (runtimes, _) = request(&mut client, 3, "runtimes.list", json!({})).await;
    assert!(
        runtimes["result"]["runtimes"]
            .as_array()
            .unwrap()
            .iter()
            .all(|runtime| runtime["machine"] == daemon.as_str()),
        "{runtimes}"
    );

    // Pinning `local` elsewhere moves it at once and tells every client.
    let (assigned, early) = request(
        &mut client,
        4,
        "machines.assign",
        json!({ "runtime": "local", "machine": "sandbox" }),
    )
    .await;
    let names = |result: &Value| {
        result["result"]["machines"]
            .as_array()
            .unwrap()
            .iter()
            .map(|machine| machine["name"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>()
    };
    let mut expected = vec![daemon.clone(), "sandbox".to_owned()];
    expected.sort();
    assert_eq!(names(&assigned), expected, "{assigned}");
    if !early
        .iter()
        .any(|notification| notification["method"] == "machines.changed")
    {
        notification_where(&mut client, "machines.changed", |_| true).await;
    }
    assert_eq!(
        harness.engine.runtime_machine("local").unwrap().as_str(),
        "sandbox"
    );

    // Refusals are registry errors that change nothing.
    let (taken, _) = request(
        &mut client,
        5,
        "machines.rename",
        json!({ "machine": "sandbox", "name": daemon }),
    )
    .await;
    assert_eq!(
        taken["error"]["data"]["code"], "machine_name_taken",
        "{taken}"
    );
    let (in_use, _) = request(
        &mut client,
        6,
        "machines.forget",
        json!({ "machine": "sandbox" }),
    )
    .await;
    assert_eq!(
        in_use["error"]["data"]["code"], "machine_in_use",
        "{in_use}"
    );
    let (missing, _) = request(
        &mut client,
        7,
        "machines.merge",
        json!({ "from": "nope", "into": "sandbox" }),
    )
    .await;
    assert_eq!(missing["error"]["data"]["code"], "machine_not_found");
    let (invalid, _) = request(
        &mut client,
        8,
        "machines.rename",
        json!({ "machine": "sandbox", "name": "Not Valid" }),
    )
    .await;
    assert_eq!(invalid["error"]["code"], -32602, "{invalid}");

    // Merging the sandbox back leaves one machine that pins `local`.
    let (merged, _) = request(
        &mut client,
        9,
        "machines.merge",
        json!({ "from": "sandbox", "into": daemon }),
    )
    .await;
    assert_eq!(names(&merged), std::slice::from_ref(&daemon));
    assert_eq!(merged["result"]["machines"][0]["pinned"], json!(["local"]));
    let (unassigned, _) = request(
        &mut client,
        10,
        "machines.unassign",
        json!({ "runtime": "local" }),
    )
    .await;
    assert_eq!(unassigned["result"]["machines"][0]["pinned"], json!([]));
    let (again, _) = request(
        &mut client,
        11,
        "machines.unassign",
        json!({ "runtime": "local" }),
    )
    .await;
    assert_eq!(again["error"]["data"]["code"], "runtime_not_pinned");
    harness.engine.shutdown().await;
}

/// `memory.*` act on the store for the user: whole-file writes with
/// revisions, refusals as registry errors, filtered listings that report
/// invalid files, ignored entries, and agent files of no current agent, and
/// `memory.changed` after each change.
#[tokio::test]
async fn memory_is_listed_read_written_and_deleted_through_rpc() {
    let harness = harness_with_mcp(BTreeMap::new());
    let memory = harness._directory.path().join("data/memory");
    let mut client = connect(Arc::clone(&harness.server)).await;
    let code = |reply: &Value| {
        reply["error"]["data"]["code"]
            .as_str()
            .unwrap_or("")
            .to_owned()
    };

    // The first start made the four default categories.
    let (listed, _) = request(&mut client, 2, "memory.list", json!({})).await;
    let urls = |reply: &Value| {
        reply["result"]["memories"]
            .as_array()
            .unwrap()
            .iter()
            .map(|memory| memory["url"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>()
    };
    assert_eq!(
        urls(&listed),
        [
            "user://feedback",
            "user://preferences",
            "user://profile",
            "user://references"
        ],
        "{listed}"
    );

    // Creating a project memory, then reading it back whole.
    let text = "---\ndescription: Build conventions\nkeywords: [cargo]\n---\nRun the gates.\n";
    let (written, early) = request(
        &mut client,
        3,
        "memory.write",
        json!({ "url": "project://web/build", "text": text }),
    )
    .await;
    let revision = written["result"]["revision"].as_str().unwrap().to_owned();
    assert_eq!(revision.len(), 16, "{written}");
    if !early
        .iter()
        .any(|notification| notification["method"] == "memory.changed")
    {
        notification_where(&mut client, "memory.changed", |_| true).await;
    }
    let (read, _) = request(
        &mut client,
        4,
        "memory.read",
        json!({ "url": "project://web/build" }),
    )
    .await;
    assert_eq!(read["result"]["text"], text);
    assert_eq!(read["result"]["revision"], revision.as_str());
    assert!(read["result"].get("invalid").is_none(), "{read}");

    // Without a revision an existing memory is not replaced; with a stale
    // one it is not either, and both errors carry the current revision.
    let (exists, _) = request(
        &mut client,
        5,
        "memory.write",
        json!({ "url": "project://web/build", "text": text }),
    )
    .await;
    assert_eq!(code(&exists), "memory_exists");
    assert_eq!(exists["error"]["data"]["revision"], revision.as_str());
    let (stale, _) = request(
        &mut client,
        6,
        "memory.write",
        json!({ "url": "project://web/build", "text": text, "revision": "0000000000000000" }),
    )
    .await;
    assert_eq!(code(&stale), "memory_revision_mismatch");
    assert_eq!(stale["error"]["data"]["revision"], revision.as_str());

    // Invalid text, a bad URL, a bad revision, and an agent the registry
    // does not have are refused, changing nothing.
    let (invalid, _) = request(
        &mut client,
        7,
        "memory.write",
        json!({ "url": "project://web/build", "text": "no frontmatter\n", "revision": revision }),
    )
    .await;
    assert_eq!(code(&invalid), "memory_invalid", "{invalid}");
    let (bad_url, _) = request(
        &mut client,
        8,
        "memory.read",
        json!({ "url": "project://Web/build" }),
    )
    .await;
    assert_eq!(code(&bad_url), "memory_url_invalid", "{bad_url}");
    let (bad_revision, _) = request(
        &mut client,
        9,
        "memory.delete",
        json!({ "url": "project://web/build", "revision": "nope" }),
    )
    .await;
    assert_eq!(bad_revision["error"]["code"], -32602, "{bad_revision}");
    assert_eq!(bad_revision["error"]["data"]["field"], "revision");
    let (refused, _) = request(
        &mut client,
        10,
        "memory.write",
        json!({ "url": "agent://ghost", "text": "---\ndescription: Lessons\n---\n" }),
    )
    .await;
    assert_eq!(code(&refused), "memory_refused", "{refused}");
    assert_eq!(
        std::fs::read_to_string(memory.join("projects/web/build.md")).unwrap(),
        text
    );

    // The user adds a category, which a model may not.
    let (category, _) = request(
        &mut client,
        11,
        "memory.write",
        json!({ "url": "user://hardware", "text": "---\ndescription: The user's hardware\n---\n" }),
    )
    .await;
    assert!(category["result"]["revision"].is_string(), "{category}");

    // Files written by hand: an agent file of no current agent, an invalid
    // machine memory, and a stray file.
    std::fs::write(
        memory.join("agents/ghost.md"),
        "---\ndescription: Old lessons\n---\n",
    )
    .unwrap();
    std::fs::create_dir_all(memory.join("machines/labpc")).unwrap();
    std::fs::write(memory.join("machines/labpc/gpu.md"), "no frontmatter\n").unwrap();
    std::fs::write(memory.join("projects/web/notes.txt"), "stray").unwrap();
    let (agents, _) = request(&mut client, 12, "memory.list", json!({ "layer": "agent" })).await;
    let ghost = &agents["result"]["memories"][0];
    assert_eq!(ghost["url"], "agent://ghost", "{agents}");
    assert_eq!(ghost["orphaned"], true);
    assert_eq!(ghost["description"], "Old lessons");
    let (machine, _) = request(
        &mut client,
        13,
        "memory.list",
        json!({ "machine": "labpc" }),
    )
    .await;
    let gpu = &machine["result"]["memories"][0];
    assert_eq!(gpu["url"], "machine://labpc/gpu", "{machine}");
    assert!(gpu["invalid"].is_string(), "{machine}");
    assert!(gpu.get("description").is_none());
    let (project, _) = request(&mut client, 14, "memory.list", json!({ "project": "web" })).await;
    assert_eq!(urls(&project), ["project://web/build"]);
    assert_eq!(project["result"]["memories"][0]["group"], "web");
    assert_eq!(project["result"]["memories"][0]["size"], text.len());
    assert_eq!(
        project["result"]["ignored"],
        json!([{ "path": "projects/web/notes.txt", "reason": "not a `.md` file" }])
    );
    let (two, _) = request(
        &mut client,
        15,
        "memory.list",
        json!({ "agent": "default", "project": "web" }),
    )
    .await;
    assert_eq!(two["error"]["code"], -32602, "{two}");

    // The user deletes an agent file and a category, which a model may not.
    let (deleted, _) = request(
        &mut client,
        16,
        "memory.delete",
        json!({ "url": "agent://ghost", "revision": ghost["revision"] }),
    )
    .await;
    assert_eq!(deleted["result"], json!({}), "{deleted}");
    let (deleted, _) = request(
        &mut client,
        17,
        "memory.delete",
        json!({ "url": "user://hardware", "revision": category["result"]["revision"] }),
    )
    .await;
    assert_eq!(deleted["result"], json!({}), "{deleted}");
    let (gone, _) = request(
        &mut client,
        18,
        "memory.read",
        json!({ "url": "agent://ghost" }),
    )
    .await;
    assert_eq!(code(&gone), "memory_not_found");
    harness.engine.shutdown().await;
}
