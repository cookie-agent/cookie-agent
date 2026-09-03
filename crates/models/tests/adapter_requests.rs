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
    AbortSignal, AssistantMessage, AssistantPart, CompletedTurn, ContentValue, FilePart,
    FileSource, Finish, FinishReason, HistoryTurn, InputPart, JsonSchema, Request, SystemMessage,
    SystemPart, TextPart, ToolCallPart, ToolContent, ToolDefinition, ToolMessage, ToolResultPart,
    UserMessage,
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

fn definition(endpoint: &str, adaptor: &str, image_capable: bool) -> ProviderDefinition {
    let tool_calling = adaptor != "openai-responses";
    let (input, media) = if image_capable {
        (
            r#"["text", "image"]"#,
            r#"{ image = { mime_types = ["image/png"], max_bytes = 20971520, max_count = 1 } }"#,
        )
    } else {
        (r#"["text"]"#, "{}")
    };
    toml::from_str(&format!(
        r#"source = "custom"
endpoint = "{endpoint}"
adaptor = "{adaptor}"
auth = {{ method = "no-auth-v1", values = {{}} }}

[models.test]
display_name = "No Auth"
capabilities = {{ input = {input}, output = ["text"], context_tokens = 4096, output_tokens = 1024, tool_calling = {tool_calling}, parallel_tool_calls = false, structured_output = false, reasoning = false, temperature = true, top_p = true, seed = false, native_replay = "unsupported", cancellation = "local_only", media = {media} }}
"#
    ))
    .unwrap()
}

fn video_definition(endpoint: &str, adaptor: &str, video_mime_type: &str) -> ProviderDefinition {
    let (setup, auth) = match adaptor {
        "google-gemini" => (
            "",
            r#"auth = { method = "google-api-key-header-v1", values = { api_key = "google-key" } }"#,
        ),
        "google-vertex-gemini" => (
            r#"setup = { project = "project-1", location = "us-central1", resource = "publishers/google" }"#,
            r#"auth = { method = "oauth-access-token-v1", values = { access_token = "vertex-token" } }"#,
        ),
        "openai-compatible" | "anthropic-compatible" => {
            ("", r#"auth = { method = "no-auth-v1", values = {} }"#)
        }
        "anthropic" => (
            "",
            r#"auth = { method = "anthropic-api-key-v1", values = { api_key = "test-key" } }"#,
        ),
        _ => panic!("unsupported video fixture adaptor"),
    };
    toml::from_str(&format!(
        r#"source = "custom"
endpoint = "{endpoint}"
adaptor = "{adaptor}"
{setup}
{auth}

[models.test]
display_name = "Video"
capabilities = {{ input = ["text", "video"], output = ["text"], context_tokens = 4096, output_tokens = 1024, tool_calling = true, parallel_tool_calls = false, structured_output = false, reasoning = false, temperature = true, top_p = true, seed = false, native_replay = "unsupported", cancellation = "local_only", media = {{ video = {{ mime_types = ["{video_mime_type}"], max_bytes = 26214400, max_count = 2 }} }} }}
"#
    ))
    .unwrap()
}

#[test]
fn authored_anthropic_model_rejects_video_capability() {
    let temporary = TempDir::new().unwrap();
    let provider_id = ProviderId::new("custom.anthropic-video").unwrap();
    let result = ModelManager::new(
        BTreeMap::from([(
            provider_id,
            video_definition("http://127.0.0.1:9/v1", "anthropic", "video/mp4"),
        )]),
        empty_catalog(),
        store(&temporary),
    );

    let error = match result {
        Ok(_) => panic!("true Anthropic adapter accepted authored video capability"),
        Err(error) => error,
    };
    let error = format!("{error:?}");
    assert!(error.contains("Anthropic declaration exceeds the protocol modality ceiling"));
}

async fn dispatch_request(
    adaptor: &str,
    response: &'static str,
    request: Request,
    cache_strategy: bool,
) -> String {
    dispatch_request_with_media(adaptor, response, request, cache_strategy, false).await
}

async fn dispatch_request_with_media(
    adaptor: &str,
    response: &'static str,
    request: Request,
    cache_strategy: bool,
    image_capable: bool,
) -> String {
    let (endpoint, captured) = server(response).await;
    let temporary = TempDir::new().unwrap();
    let provider_id = ProviderId::new("custom.no-auth").unwrap();
    let manager = ModelManager::new(
        BTreeMap::from([(
            provider_id.clone(),
            definition(&endpoint, adaptor, image_capable),
        )]),
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

async fn dispatch_video_request(
    adaptor: &str,
    response: &'static str,
    request: Request,
    declared_video_mime_type: &str,
) -> String {
    let (mut endpoint, captured) = server(response).await;
    if adaptor == "google-gemini" {
        endpoint = endpoint.trim_end_matches("/v1").to_owned() + "/v1beta";
    }
    let temporary = TempDir::new().unwrap();
    let provider_id = ProviderId::new("custom.video").unwrap();
    let manager = ModelManager::new(
        BTreeMap::from([(
            provider_id.clone(),
            video_definition(&endpoint, adaptor, declared_video_mime_type),
        )]),
        empty_catalog(),
        store(&temporary),
    )
    .unwrap_or_else(|error| panic!("{adaptor} video manager: {error:?}"));
    let key = ModelKey::new(provider_id, ProviderModelId::new("test").unwrap()).unwrap();
    let resolved = manager
        .current()
        .resolve(&ModelSelection {
            model: key,
            variant: None,
        })
        .unwrap();
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

async fn dispatch_video(adaptor: &str, response: &'static str, video_mime_type: &str) -> String {
    dispatch_video_request(
        adaptor,
        response,
        Request::new(vec![HistoryTurn::user(UserMessage::new(vec![
            InputPart::File(FilePart::video(
                video_mime_type,
                FileSource::Bytes(b"video".to_vec().into()),
            )),
        ]))]),
        video_mime_type,
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

#[tokio::test]
async fn anthropic_cache_breakpoint_survives_image_bearing_tool_result() {
    let response =
        "event: message_start\ndata: {\"message\":{}}\n\nevent: message_stop\ndata: {}\n\n";
    let assistant = CompletedTurn::new(
        AssistantMessage::new(vec![AssistantPart::ToolCall(ToolCallPart::new(
            "call",
            "read",
            serde_json::json!({}),
        ))]),
        Finish::new(Default::default(), FinishReason::ToolCalls),
    );
    let result = ToolResultPart::new(
        "call",
        ToolContent::Mixed(vec![
            ContentValue::Text("Attached image/png".into()),
            ContentValue::File(FilePart::image(
                "image/png",
                FileSource::Bytes(b"png".to_vec().into()),
            )),
        ]),
    );
    let request = Request::new(vec![
        HistoryTurn::assistant(assistant),
        HistoryTurn::tool(ToolMessage::new(vec![result])),
    ]);
    let captured =
        dispatch_request_with_media("anthropic-compatible", response, request, true, true).await;
    let body = http_body(&captured);
    let tool_result = &body["messages"][1]["content"][0];
    assert_eq!(tool_result["content"][1]["type"], "image");
    assert_eq!(
        tool_result["content"][1]["source"]["media_type"],
        "image/png"
    );
    assert_eq!(tool_result["content"][1]["source"]["data"], "cG5n");
    assert_eq!(
        body["messages"][1]["content"]
            .as_array()
            .unwrap()
            .last()
            .unwrap()["cache_control"]["ttl"],
        "5m"
    );
}

#[tokio::test]
async fn user_turn_video_encodes_for_every_declared_delivery_family() {
    let openai = http_body(
        &dispatch_video(
            "openai-compatible",
            "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "video/mp4",
        )
        .await,
    );
    assert_eq!(
        openai["messages"][0]["content"][0],
        serde_json::json!({
            "type":"video_url",
            "video_url":{"url":"data:video/mp4;base64,dmlkZW8="}
        })
    );

    let anthropic = http_body(
        &dispatch_video(
            "anthropic-compatible",
            "event: message_start\ndata: {\"message\":{}}\n\nevent: message_delta\ndata: {\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{}}\n\nevent: message_stop\ndata: {}\n\n",
            "video/mov",
        )
        .await,
    );
    assert_eq!(anthropic["messages"][0]["content"][0]["type"], "video");
    assert_eq!(
        anthropic["messages"][0]["content"][0]["source"],
        serde_json::json!({
            "type":"base64",
            "media_type":"video/mov",
            "data":"dmlkZW8="
        })
    );

    let google_response = "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"ok\"}]},\"finishReason\":\"STOP\"}]}\n\n";
    for (adaptor, mime_type) in [
        ("google-gemini", "video/webm"),
        ("google-vertex-gemini", "video/mpegs"),
    ] {
        let body = http_body(&dispatch_video(adaptor, google_response, mime_type).await);
        assert_eq!(
            body["contents"][0]["parts"][0],
            serde_json::json!({
                "inlineData":{"mimeType":mime_type,"data":"dmlkZW8="}
            }),
            "{adaptor}"
        );
    }
}

#[tokio::test]
async fn translated_system_emission_stays_a_user_turn_on_each_delivery_family_wire() {
    const MARKER: &str = "[tool-emitted system message; materialized as user history]";
    let request = || {
        Request::new(vec![
            HistoryTurn::system(SystemMessage::new(vec![SystemPart::Text(TextPart::new(
                "stable system prefix",
            ))])),
            HistoryTurn::user(UserMessage::new(vec![
                InputPart::Text(TextPart::new(MARKER)),
                InputPart::Text(TextPart::new("emitted system context")),
            ])),
        ])
    };

    let openai = http_body(
        &dispatch_video_request(
            "openai-compatible",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            request(),
            "video/mp4",
        )
        .await,
    );
    assert_eq!(openai["messages"][0]["role"], "system");
    assert_eq!(openai["messages"][1]["role"], "user");
    assert_eq!(
        openai["messages"][1]["content"],
        format!("{MARKER}emitted system context")
    );

    let anthropic = http_body(
        &dispatch_video_request(
            "anthropic-compatible",
            "event: message_start\ndata: {\"message\":{}}\n\nevent: message_stop\ndata: {}\n\n",
            request(),
            "video/mp4",
        )
        .await,
    );
    assert_eq!(anthropic["system"][0]["text"], "stable system prefix");
    assert_eq!(anthropic["messages"][0]["role"], "user");
    assert_eq!(anthropic["messages"][0]["content"][0]["text"], MARKER);

    let google_response = "data: {\"candidates\":[{\"finishReason\":\"STOP\"}]}\n\n";
    for adaptor in ["google-gemini", "google-vertex-gemini"] {
        let body = http_body(
            &dispatch_video_request(adaptor, google_response, request(), "video/mp4").await,
        );
        assert_eq!(
            body["systemInstruction"]["parts"][0]["text"], "stable system prefix",
            "{adaptor}"
        );
        assert_eq!(body["contents"][0]["role"], "user", "{adaptor}");
        assert_eq!(body["contents"][0]["parts"][0]["text"], MARKER, "{adaptor}");
    }
}
