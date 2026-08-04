use std::{collections::BTreeMap, fs, os::unix::fs::PermissionsExt as _, path::Path, sync::Arc};

use axum::http::{HeaderMap, StatusCode, header};
use cookie_agent_config::load_from_roots;
use cookie_agent_engine::{Engine, EngineOptions};
use cookie_agent_models::{Catalog, CredentialStore, MODELS_DEV_ARTIFACT_SHA256, ModelSetManager};
use cookie_agent_protocol::{
    AgentId, ApprovalRespondParams, CatalogModelListResult, CatalogRevision, ModelSelection,
    ProviderConnectParams, RunSelection, RunStartParams, SessionCreateParams, SessionId,
};
use futures_util::{SinkExt as _, StreamExt as _};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;

use crate::{
    InProcessStream, MessageFrame, MessageStream, Server, TokenError,
    auth_token::{TOKEN_ENCODED_BYTES, load_or_create_token},
    catalog::into_manager_connect_request,
    in_process_pair,
    service::authorized,
};

struct Harness {
    _directory: TempDir,
    engine: Engine,
    server: Arc<Server>,
    selection: RunSelection,
    external_selection: Option<RunSelection>,
}

fn write_agent(root: &Path, model: &str) {
    fs::create_dir_all(root.join("agents")).expect("create agents");
    fs::write(
        root.join("agents/primary.md"),
        format!(
            "---\nschema: 1\ndescription: Primary test agent\nmode: primary\nenabled: true\nmodel_fallback: [{{ model: \"{model}\" }}]\ntools: []\npermissions: []\n---\nTest system prompt.\n"
        ),
    )
    .expect("write agent");
}

fn harness(credential_store: bool) -> Harness {
    let directory = TempDir::new().expect("tempdir");
    fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700))
        .expect("private tempdir");
    let root = directory.path().join(".cookie-agent");
    fs::create_dir(&root).expect("create root");
    let (config, model) = if credential_store {
        (
            format!(
                "schema_version = 6\n[providers.openai]\nsource = \"models_dev\"\ncatalog_revision = \"sha256:{MODELS_DEV_ARTIFACT_SHA256}\"\nauth = {{ type = \"credential_store\" }}\n[providers.openai.models.\"gpt-5.6-sol\"]\n"
            ),
            "openai/gpt-5.6-sol",
        )
    } else {
        (
            "schema_version = 6\n[providers.test]\nsource = \"explicit\"\nendpoint = \"http://127.0.0.1:9/v1\"\nadaptor = \"openai-compatible\"\nauth = { type = \"none\" }\n[providers.test.models.model]\ndisplay_name = \"Model\"\ndefault_variant = \"fast\"\n[providers.test.models.model.capabilities]\ninput = [\"text\"]\noutput = [\"text\"]\ncontext_tokens = 8192\noutput_tokens = 2048\ntool_calling = true\nparallel_tool_calls = false\nstructured_output = false\nreasoning = false\ntemperature = true\ntop_p = true\nseed = true\nnative_replay = \"unsupported\"\nnative_compaction = \"unsupported\"\ncancellation = \"local_only\"\nmedia = {}\n[providers.test.models.model.variants.fast]\noperation = \"add\"\ndefaults = { temperature = 0.1 }\n[providers.test.models.external]\ndisplay_name = \"External\"\n[providers.test.models.external.capabilities]\ninput = [\"text\"]\noutput = [\"text\"]\ncontext_tokens = 8192\noutput_tokens = 2048\ntool_calling = true\nparallel_tool_calls = false\nstructured_output = false\nreasoning = false\ntemperature = true\ntop_p = true\nseed = true\nnative_replay = \"unsupported\"\nnative_compaction = \"unsupported\"\ncancellation = \"local_only\"\nmedia = {}\n".into(),
            "test/model",
        )
    };
    fs::write(root.join("config.toml"), config).expect("write config");
    write_agent(&root, model);
    let loaded = load_from_roots(None, Some(&root)).expect("load config");
    let catalog = Arc::new(Catalog::embedded().expect("catalog"));
    let model_manager = Arc::new(
        ModelSetManager::new(
            loaded.runtime.providers.clone(),
            Arc::clone(&catalog),
            CredentialStore::new(directory.path().join("credentials")),
        )
        .expect("model manager"),
    );
    let model = model.parse().expect("model key");
    let variant = model_manager
        .current()
        .model_set()
        .get(&model)
        .expect("model")
        .default_variant()
        .cloned();
    let selection = RunSelection {
        agent: AgentId::new("primary").expect("agent"),
        model: ModelSelection { model, variant },
    };
    let external_selection = (!credential_store).then(|| RunSelection {
        agent: AgentId::new("primary").expect("agent"),
        model: ModelSelection {
            model: "test/external".parse().expect("external model key"),
            variant: None,
        },
    });
    let loaded = Arc::new(loaded);
    let engine = Engine::open(EngineOptions {
        data_dir: directory.path().join("data"),
        cwd: directory.path().to_owned(),
        config: (*loaded).clone(),
        model_manager: Arc::clone(&model_manager),
        tools: Vec::new(),
    })
    .expect("engine");
    let server = Arc::new(Server {
        token_path: directory.path().join("daemon/token-v1"),
        ..Server::new(engine.clone(), model_manager, catalog, loaded)
    });
    Harness {
        _directory: directory,
        engine,
        server,
        selection,
        external_selection,
    }
}

async fn request(stream: &mut InProcessStream, id: i64, method: &str, params: Value) -> Value {
    stream
        .send(MessageFrame::Value(json!({
            "jsonrpc": "2.0", "id": id, "method": method, "params": params
        })))
        .await
        .expect("send");
    loop {
        let frame = stream.recv().await.expect("recv").expect("open");
        let value = match frame {
            MessageFrame::Value(value) => value,
            MessageFrame::Text(text) => serde_json::from_str(&text).expect("JSON"),
        };
        if value.get("id") == Some(&json!(id)) {
            return value;
        }
    }
}

async fn handshake(stream: &mut InProcessStream) {
    let response = request(stream, 1, "handshake", json!({ "protocol_version": 7 })).await;
    assert_eq!(response["result"]["protocol_version"], 7);
}

#[tokio::test]
async fn v7_handshake_is_required_and_v6_is_rejected() {
    let harness = harness(false);
    let (mut client, server_stream) = in_process_pair(8);
    let task = tokio::spawn(harness.server.clone().serve_stream(server_stream));
    let blocked = request(&mut client, 1, "model.list", json!({})).await;
    assert_eq!(blocked["error"]["code"], -32001);
    let old = request(
        &mut client,
        2,
        "handshake",
        json!({ "protocol_version": 6 }),
    )
    .await;
    assert_eq!(old["error"]["code"], -32602);
    handshake(&mut client).await;
    drop(client);
    task.await.expect("join").expect("serve");
    harness.engine.shutdown().await;
}

#[tokio::test]
async fn model_agent_catalog_and_session_projections_are_v7_and_exact() {
    let harness = harness(false);
    let (mut client, server_stream) = in_process_pair(16);
    let task = tokio::spawn(harness.server.clone().serve_stream(server_stream));
    handshake(&mut client).await;

    let models = request(&mut client, 2, "model.list", json!({})).await["result"].clone();
    assert_eq!(models["models"].as_array().expect("models").len(), 2);
    let authored = models["models"]
        .as_array()
        .expect("models")
        .iter()
        .find(|model| model["key"] == "test/model")
        .expect("authored model");
    assert_eq!(authored["default_variant"], "fast");
    assert_eq!(authored["variants"][0]["id"], "fast");
    assert!(
        models["models"]
            .as_array()
            .expect("models")
            .iter()
            .any(|model| model["key"] == "test/external")
    );
    assert!(models["revision"].as_str().unwrap().starts_with("sha256:"));

    let agents = request(&mut client, 3, "agent.list", json!({})).await["result"].clone();
    assert_eq!(agents["model_revision"], models["revision"]);
    assert_eq!(agents["agents"][0]["id"], "primary");
    assert_eq!(
        agents["agents"][0]["resolved_fallback"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        agents["agents"][0]["resolved_fallback"][0]["model"],
        "test/model"
    );
    assert_eq!(
        agents["agents"][0]["resolved_fallback"][0]["variant"],
        "fast"
    );

    let catalog =
        request(&mut client, 4, "catalog.provider.list", json!({})).await["result"].clone();
    assert_eq!(
        catalog["snapshot"]["revision"],
        CatalogRevision::current().to_string()
    );
    let ids = catalog["providers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|provider| provider["id"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert!(ids.is_empty());
    let catalog_models = request(
        &mut client,
        5,
        "catalog.model.list",
        json!({ "provider_id": "anthropic" }),
    )
    .await["result"]
        .clone();
    let catalog_models: CatalogModelListResult =
        serde_json::from_value(catalog_models).expect("valid catalog model projection");
    assert!(!catalog_models.models.is_empty());
    assert!(catalog_models.models.iter().all(|model| {
        model.provider_id.as_str() == "anthropic"
            && model.limits.context > 0
            && model.limits.output > 0
    }));

    let created = request(
        &mut client,
        6,
        "session.create",
        serde_json::to_value(SessionCreateParams {
            selection: harness
                .external_selection
                .clone()
                .expect("external selection"),
        })
        .unwrap(),
    )
    .await["result"]["session"]
        .clone();
    assert_eq!(created["creation_selection"]["agent"], "primary");
    assert_eq!(
        created["creation_selection"]["model"]["model"],
        "test/external"
    );
    assert_eq!(created["title_updated_seq"], 0);
    assert_eq!(created["last_event_seq"], 1);

    let session_id: SessionId =
        serde_json::from_value(created["session_id"].clone()).expect("session id");
    let started = request(
        &mut client,
        7,
        "run.start",
        serde_json::to_value(RunStartParams {
            session_id,
            client_run_id: cookie_agent_protocol::ClientRunId::new("external-root-run")
                .expect("client run id"),
            selection: harness
                .external_selection
                .clone()
                .expect("external selection"),
            input: "run external configured model".into(),
        })
        .expect("run params"),
    )
    .await;
    assert!(started["result"]["run_id"].is_string());
    let run_id: cookie_agent_protocol::RunId =
        serde_json::from_value(started["result"]["run_id"].clone()).expect("run id");
    let _ = harness.engine.cancel_run(run_id).await;

    drop(client);
    task.await.expect("join").expect("serve");
    harness.engine.shutdown().await;
}

#[tokio::test]
async fn title_sequence_and_stored_event_projection_are_monotonic() {
    let harness = harness(false);
    let session = harness
        .engine
        .create_session(harness.selection.clone())
        .expect("session");
    let (mut client, server_stream) = in_process_pair(16);
    let task = tokio::spawn(harness.server.clone().serve_stream(server_stream));
    handshake(&mut client).await;

    let renamed = request(
        &mut client,
        2,
        "session.rename",
        json!({
            "session_id": session.session_id,
            "client_rename_id": "rename-1",
            "change": { "type": "set", "title": "New title" }
        }),
    )
    .await["result"]["session"]
        .clone();
    assert_eq!(renamed["title"], "New title");
    assert_eq!(renamed["title_updated_seq"], renamed["last_event_seq"]);

    let events = request(
        &mut client,
        3,
        "events.subscribe",
        json!({ "session_id": session.session_id, "cursor": null }),
    )
    .await["result"]["events"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(events[0]["event_schema_version"], 7);
    assert_eq!(events[0]["seq"], 1);
    assert_eq!(
        events.last().unwrap()["payload"]["type"],
        "session_title_committed"
    );

    drop(client);
    task.await.expect("join").expect("serve");
    harness.engine.shutdown().await;
}

#[tokio::test]
async fn approval_and_tool_routes_keep_exact_v7_fields() {
    let harness = harness(false);
    let session = harness
        .engine
        .create_session(harness.selection.clone())
        .expect("session");
    let (mut client, server_stream) = in_process_pair(16);
    let task = tokio::spawn(harness.server.clone().serve_stream(server_stream));
    handshake(&mut client).await;

    let approval = request(
        &mut client,
        2,
        "approval.respond",
        json!({
            "session_id": session.session_id,
            "approval_id": cookie_agent_protocol::ApprovalId::new_v7(),
            "request_revision": 1,
            "operation_fingerprint": { "digest": "00".repeat(32) },
            "client_response_id": "response-1",
            "decision": "approve_once",
            "feedback": null
        }),
    )
    .await;
    assert_eq!(approval["error"]["data"]["code"], "approval_not_found");

    let old_tool_stdin = request(
        &mut client,
        3,
        "run.tool_stdin",
        json!({
            "run_id": cookie_agent_protocol::RunId::new_v7(),
            "tool_call_id": cookie_agent_protocol::ToolCallId::new_v7(),
            "input": "legacy"
        }),
    )
    .await;
    assert_eq!(old_tool_stdin["error"]["code"], -32602);

    drop(client);
    task.await.expect("join").expect("serve");
    harness.engine.shutdown().await;
}

#[tokio::test]
async fn provider_connect_uses_only_declared_credential_store_providers() {
    let harness = harness(true);
    let (mut client, server_stream) = in_process_pair(16);
    let task = tokio::spawn(harness.server.clone().serve_stream(server_stream));
    handshake(&mut client).await;
    let unavailable = request(&mut client, 2, "model.list", json!({})).await;
    assert!(
        unavailable["result"]["models"]
            .as_array()
            .expect("models")
            .is_empty()
    );
    let params = json!({
        "client_connect_id": "connect-1",
        "provider_id": "openai",
        "catalog_revision": CatalogRevision::current(),
        "credentials": { "values": { "OPENAI_API_KEY": "sentinel-secret" } }
    });
    let first = request(&mut client, 3, "provider.connect", params.clone()).await;
    let replay = request(&mut client, 4, "provider.connect", params).await;
    assert_eq!(first["result"], replay["result"]);
    assert_eq!(first["result"]["connection"]["provider_id"], "openai");
    assert!(!first.to_string().contains("sentinel-secret"));
    let agents = request(&mut client, 5, "agent.list", json!({})).await;
    assert_eq!(
        agents["result"]["model_revision"],
        first["result"]["model_revision"]
    );
    assert_eq!(agents["result"]["agents"][0]["runnable_as_root"], true);
    let created = request(
        &mut client,
        6,
        "session.create",
        serde_json::to_value(SessionCreateParams {
            selection: harness.selection.clone(),
        })
        .unwrap(),
    )
    .await;
    assert_eq!(
        created["result"]["session"]["creation_selection"]["agent"],
        "primary"
    );

    let catalog = request(&mut client, 7, "catalog.provider.list", json!({})).await;
    assert_eq!(catalog["result"]["providers"].as_array().unwrap().len(), 1);
    assert_eq!(catalog["result"]["providers"][0]["id"], "openai");

    let missing = request(
        &mut client,
        8,
        "provider.connect",
        json!({
            "client_connect_id": "missing",
            "provider_id": "openai",
            "catalog_revision": CatalogRevision::current(),
            "credentials": { "values": {} }
        }),
    )
    .await;
    assert_eq!(missing["error"]["data"]["code"], "missing_credential");
    assert_eq!(
        missing["error"]["data"]["missing_credential_fields"],
        json!(["OPENAI_API_KEY"])
    );

    let unknown = request(
        &mut client,
        9,
        "provider.connect",
        json!({
            "client_connect_id": "unknown",
            "provider_id": "unknown",
            "catalog_revision": CatalogRevision::current(),
            "credentials": { "values": { "OPENAI_API_KEY": "secret" } }
        }),
    )
    .await;
    assert_eq!(unknown["error"]["data"]["code"], "unknown_provider");

    drop(client);
    task.await.expect("join").expect("serve");
    harness.engine.shutdown().await;
}

#[tokio::test]
async fn configured_explicit_provider_connect_is_unsupported_not_unknown() {
    let harness = harness(false);
    let (mut client, server_stream) = in_process_pair(8);
    let task = tokio::spawn(harness.server.clone().serve_stream(server_stream));
    handshake(&mut client).await;
    let response = request(
        &mut client,
        2,
        "provider.connect",
        json!({
            "client_connect_id": "explicit",
            "provider_id": "test",
            "catalog_revision": CatalogRevision::current(),
            "credentials": { "values": {} }
        }),
    )
    .await;
    assert_eq!(response["error"]["data"]["code"], "unsupported_provider");
    assert_eq!(
        response["error"]["data"]["missing_credential_fields"],
        json!([])
    );
    drop(client);
    task.await.expect("join").expect("serve");
    harness.engine.shutdown().await;
}

#[test]
fn auth_is_strict_and_debug_redacts_frames() {
    let mut headers = HeaderMap::new();
    headers.insert(header::AUTHORIZATION, "Bearer correct".parse().unwrap());
    assert!(authorized(&headers, "correct"));
    assert!(!authorized(&headers, "incorrect"));
    headers.insert(header::ORIGIN, "https://example.test".parse().unwrap());
    assert!(headers.contains_key(header::ORIGIN));

    let sentinel = "SENTINEL_SECRET";
    let frame = MessageFrame::Value(json!({ "credentials": sentinel }));
    assert_eq!(format!("{frame:?}"), "MessageFrame::Value(<redacted>)");
    assert!(!format!("{frame:?}").contains(sentinel));
}

#[tokio::test]
async fn authenticated_websocket_transport_serves_v7_and_rejects_origin() {
    let harness = harness(false);
    let token = load_or_create_token(&harness.server.token_path).expect("token");
    let running = harness.server.clone().serve(0).await.expect("serve");
    let url = format!("ws://{}/ws", running.address());

    let mut request = url.clone().into_client_request().unwrap();
    request
        .headers_mut()
        .insert("authorization", format!("Bearer {token}").parse().unwrap());
    let (mut socket, _) = tokio_tungstenite::connect_async(request)
        .await
        .expect("authenticated websocket");
    socket
        .send(tokio_tungstenite::tungstenite::Message::Text(
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "handshake",
                "params": { "protocol_version": 7 }
            })
            .to_string()
            .into(),
        ))
        .await
        .unwrap();
    let response = socket.next().await.unwrap().unwrap().into_text().unwrap();
    let response: Value = serde_json::from_str(&response).unwrap();
    assert_eq!(response["result"]["protocol_version"], 7);
    drop(socket);

    let mut origin = url.into_client_request().unwrap();
    origin
        .headers_mut()
        .insert("authorization", format!("Bearer {token}").parse().unwrap());
    origin
        .headers_mut()
        .insert("origin", "https://example.test".parse().unwrap());
    let error = tokio_tungstenite::connect_async(origin)
        .await
        .expect_err("origin must be rejected");
    let tokio_tungstenite::tungstenite::Error::Http(response) = error else {
        panic!("expected HTTP rejection")
    };
    assert_eq!(response.status(), StatusCode::FORBIDDEN);

    harness.server.shutdown();
    running.wait().await;
    harness.engine.shutdown().await;
}

#[cfg(unix)]
#[test]
fn token_file_is_private_and_links_are_rejected() {
    let directory = TempDir::new().expect("tempdir");
    fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let path = directory.path().join("daemon/token-v1");
    let token = load_or_create_token(&path).expect("create token");
    assert_eq!(token.len(), TOKEN_ENCODED_BYTES);
    let metadata = fs::metadata(&path).unwrap();
    assert_eq!(metadata.permissions().mode() & 0o777, 0o600);

    fs::remove_file(&path).unwrap();
    let target = directory.path().join("target");
    fs::write(&target, "not-a-token").unwrap();
    std::os::unix::fs::symlink(&target, &path).unwrap();
    assert!(matches!(
        load_or_create_token(&path),
        Err(TokenError::UnsafePath)
    ));
}

#[test]
fn old_profile_alias_and_approval_shapes_are_invalid() {
    assert!(
        serde_json::from_value::<SessionCreateParams>(json!({
            "cwd": "/tmp", "profile": "primary"
        }))
        .is_err()
    );
    assert!(
        serde_json::from_value::<ApprovalRespondParams>(json!({
            "session_id": SessionId::new_v7(),
            "approval_id": cookie_agent_protocol::ApprovalId::new_v7(),
            "decision": "once"
        }))
        .is_err()
    );
}

#[test]
fn manager_request_moves_and_redacts_credentials() {
    let request: ProviderConnectParams = serde_json::from_value(json!({
        "client_connect_id": "move",
        "provider_id": "openai",
        "catalog_revision": CatalogRevision::current(),
        "credentials": { "values": { "OPENAI_API_KEY": "sentinel" } }
    }))
    .unwrap();
    let manager = into_manager_connect_request(request);
    assert_eq!(
        manager.credentials,
        BTreeMap::from([("OPENAI_API_KEY".into(), "sentinel".into())])
    );
    assert!(!format!("{manager:?}").contains("sentinel"));
}
