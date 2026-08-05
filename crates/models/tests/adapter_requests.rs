#![cfg(unix)]

use std::{collections::BTreeMap, fs, os::unix::fs::PermissionsExt as _, sync::Arc};

use cookie_agent_identity::{
    CatalogRevision, ModelKey, ModelSelection, ProviderId, ProviderModelId,
};
use cookie_agent_models::{
    ModelManager, ProviderDefinition,
    catalog::{
        CatalogAgeState, CatalogAvailability, CatalogRuntimeState, CatalogSnapshot, CatalogSource,
    },
    provider_store::ProviderStore,
};
use futures_util::StreamExt as _;
use jiff::Timestamp;
use oven_sdk::{AbortSignal, HistoryTurn, InputPart, Request, TextPart, UserMessage};
use sha2::{Digest as _, Sha256};
use tempfile::TempDir;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

fn empty_catalog() -> Arc<CatalogSnapshot> {
    let now = Timestamp::now();
    Arc::new(CatalogSnapshot {
        revision: CatalogRevision::new(format!(
            "sha256:{:x}",
            Sha256::digest(b"adapter-request-catalog")
        ))
        .unwrap(),
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

fn store(temporary: &TempDir) -> ProviderStore {
    fs::set_permissions(temporary.path(), fs::Permissions::from_mode(0o700)).unwrap();
    ProviderStore::open(temporary.path().join("providers")).unwrap()
}

async fn server(response_body: &'static str) -> (String, tokio::task::JoinHandle<String>) {
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
            response_body.len(),
            response_body
        );
        socket.write_all(response.as_bytes()).await.unwrap();
        String::from_utf8(request).unwrap()
    });
    (format!("http://{address}/v1"), task)
}

fn definition(endpoint: &str, adaptor: &str) -> ProviderDefinition {
    toml::from_str(&format!(
        r#"source = "custom"
endpoint = "{endpoint}"
adaptor = "{adaptor}"
auth = {{ method = "no-auth-v1", values = {{}} }}

[models.test]
display_name = "No Auth"
capabilities = {{ input = ["text"], output = ["text"], context_tokens = 4096, output_tokens = 1024, tool_calling = false, parallel_tool_calls = false, structured_output = false, reasoning = false, temperature = true, top_p = true, seed = false, native_replay = "unsupported", native_compaction = "unsupported", cancellation = "local_only", media = {{}} }}
"#
    ))
    .unwrap()
}

async fn dispatch(adaptor: &str, response: &'static str) -> String {
    let (endpoint, captured) = server(response).await;
    let temporary = TempDir::new().unwrap();
    let provider_id = ProviderId::new("custom.no-auth").unwrap();
    let manager = ModelManager::new(
        BTreeMap::from([(provider_id.clone(), definition(&endpoint, adaptor))]),
        empty_catalog(),
        store(&temporary),
    )
    .unwrap();
    let key = ModelKey::new(provider_id, ProviderModelId::new("test").unwrap()).unwrap();
    let resolved = manager
        .current()
        .resolve(&ModelSelection {
            model: key,
            variant: None,
        })
        .unwrap();
    let request = Request::new(vec![HistoryTurn::user(UserMessage::new(vec![
        InputPart::Text(TextPart::new("hello")),
    ]))]);
    let mut stream = resolved
        .model()
        .stream(resolved.prepare_request(request), AbortSignal::default())
        .await
        .unwrap();
    while let Some(part) = stream.stream.next().await {
        part.unwrap();
    }
    captured.await.unwrap()
}

#[tokio::test]
async fn custom_openai_chat_no_auth_emits_no_credential_material() {
    let request = dispatch(
        "openai-chat",
        "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
    )
    .await;
    assert!(request.starts_with("POST /v1/chat/completions? HTTP/1.1"));
    assert!(!request.to_ascii_lowercase().contains("authorization:"));
    assert!(!request.contains("no-auth"));
}

#[tokio::test]
async fn custom_openai_compatible_no_auth_emits_no_credential_material() {
    let request = dispatch(
        "openai-compatible",
        "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
    )
    .await;
    assert!(request.starts_with("POST /v1/chat/completions? HTTP/1.1"));
    assert!(!request.to_ascii_lowercase().contains("authorization:"));
}

#[tokio::test]
async fn custom_openai_responses_no_auth_uses_responses_wire_without_auth() {
    let request = dispatch(
        "openai-responses",
        "data: {\"type\":\"response.output_text.delta\",\"item_id\":\"out\",\"delta\":\"ok\"}\n\ndata: {\"type\":\"response.output_text.done\",\"item_id\":\"out\"}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n",
    )
    .await;
    assert!(request.starts_with("POST /v1/responses HTTP/1.1"));
    assert!(!request.to_ascii_lowercase().contains("authorization:"));
    assert!(!request.contains("no-auth"));
}
