#![cfg(unix)]

use std::{collections::BTreeMap, fs, os::unix::fs::PermissionsExt as _, sync::Arc};

use cookie_agent_identity::{
    CatalogRevision, ModelKey, ModelSelection, ProviderId, ProviderModelId,
};
use cookie_agent_models::{
    ModelManager, ProviderDefinition,
    adapters::{AnthropicCacheStrategyConfig, AnthropicCacheTtlConfig, CacheStrategyConfig},
    catalog::{
        CatalogAgeState, CatalogAvailability, CatalogRuntimeState, CatalogSnapshot, CatalogSource,
    },
    provider_store::ProviderStore,
};
use futures_util::StreamExt as _;
use jiff::Timestamp;
use oven_sdk::{
    AbortSignal, HistoryTurn, InputPart, JsonSchema, Request, SystemMessage, SystemPart, TextPart,
    ToolDefinition, UserMessage,
};
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
        let mut expected_length = None;
        loop {
            let read = socket.read(&mut buffer).await.unwrap();
            if read == 0 {
                break;
            }
            request.extend_from_slice(&buffer[..read]);
            if expected_length.is_none()
                && let Some(header_end) =
                    request.windows(4).position(|window| window == b"\r\n\r\n")
            {
                let headers = std::str::from_utf8(&request[..header_end]).unwrap();
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().unwrap())
                    })
                    .unwrap_or_default();
                expected_length = Some(header_end + 4 + content_length);
            }
            if expected_length.is_some_and(|length| request.len() >= length) {
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
    let tool_calling = adaptor != "openai-responses";
    toml::from_str(&format!(
        r#"source = "custom"
endpoint = "{endpoint}"
adaptor = "{adaptor}"
auth = {{ method = "no-auth-v1", values = {{}} }}

[models.test]
display_name = "No Auth"
capabilities = {{ input = ["text"], output = ["text"], context_tokens = 4096, output_tokens = 1024, tool_calling = {tool_calling}, parallel_tool_calls = false, structured_output = false, reasoning = false, temperature = true, top_p = true, seed = false, native_replay = "unsupported", cancellation = "local_only", media = {{}} }}
"#
    ))
    .unwrap()
}

async fn dispatch_request(
    adaptor: &str,
    response: &'static str,
    request: Request,
    cache_strategy: bool,
) -> String {
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
    let request = if cache_strategy {
        let strategy = CacheStrategyConfig::Anthropic(AnthropicCacheStrategyConfig {
            system: Some(AnthropicCacheTtlConfig::OneHour),
            tools: Some(AnthropicCacheTtlConfig::OneHour),
            rolling: Some(AnthropicCacheTtlConfig::FiveMinutes),
        });
        resolved.prepare_request_with_cache_strategy(request, Some(&strategy))
    } else {
        resolved.prepare_request(request)
    };
    let mut stream = resolved
        .model()
        .stream(request, AbortSignal::default())
        .await
        .unwrap();
    while let Some(part) = stream.stream.next().await {
        part.unwrap();
    }
    captured.await.unwrap()
}

async fn dispatch(adaptor: &str, response: &'static str) -> String {
    dispatch_request(
        adaptor,
        response,
        Request::new(vec![HistoryTurn::user(UserMessage::new(vec![
            InputPart::Text(TextPart::new("hello")),
        ]))]),
        true,
    )
    .await
}

fn http_body(request: &str) -> serde_json::Value {
    serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap()
}

#[tokio::test]
async fn custom_openai_chat_no_auth_emits_no_credential_material() {
    let request = dispatch(
        "openai-chat",
        "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
    )
    .await;
    assert!(request.starts_with("POST /v1/chat/completions? HTTP/1.1"));
    assert!(request.to_ascii_lowercase().contains(
        "\r\nuser-agent: opencode/1.18.2 ai-sdk/provider-utils/4.0.27 runtime/bun/1.3.14\r\n"
    ));
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

#[tokio::test]
async fn anthropic_cache_strategy_lowers_to_system_tools_and_messages() {
    let response =
        "event: message_start\ndata: {\"message\":{}}\n\nevent: message_stop\ndata: {}\n\n";
    let request = Request::new(vec![
        HistoryTurn::system(SystemMessage::new(vec![SystemPart::Text(TextPart::new(
            "stable system",
        ))])),
        HistoryTurn::user(UserMessage::new(vec![InputPart::Text(TextPart::new(
            "rolling user",
        ))])),
    ])
    .with_tools(vec![
        ToolDefinition::new(
            "first",
            "first tool",
            JsonSchema::new(serde_json::json!({"type":"object"})).unwrap(),
        ),
        ToolDefinition::new(
            "last",
            "last tool",
            JsonSchema::new(serde_json::json!({"type":"object"})).unwrap(),
        ),
    ]);
    let captured = dispatch_request("anthropic-compatible", response, request.clone(), true).await;
    let body = http_body(&captured);
    assert_eq!(body["system"][0]["cache_control"]["ttl"], "1h");
    assert_eq!(body["tools"][1]["cache_control"]["ttl"], "1h");
    assert_eq!(
        body["messages"][0]["content"][0]["cache_control"]["ttl"],
        "5m"
    );

    let uncached = dispatch_request("anthropic-compatible", response, request, false).await;
    assert!(!uncached.contains("cache_control"));
}
