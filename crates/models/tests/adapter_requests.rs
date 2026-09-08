#![cfg(unix)]

use std::{collections::BTreeMap, fs, os::unix::fs::PermissionsExt as _, sync::Arc};

use cookie_agent_identity::{
    CatalogRevision, ModelKey, ModelSelection, ProviderId, ProviderModelId, VariantId,
};
use cookie_agent_models::{
    HeaderName, ModelManager, ProviderDefinition, SafeStaticHeaderValue, VariantDefinition,
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
    let (endpoint, captured) = server_requests(response_body, 1).await;
    (
        endpoint,
        tokio::spawn(async move { captured.await.unwrap().pop().unwrap() }),
    )
}

async fn server_requests(
    response_body: &'static str,
    count: usize,
) -> (String, tokio::task::JoinHandle<Vec<String>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let mut requests = Vec::new();
        for _ in 0..count {
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
            requests.push(String::from_utf8(request).unwrap());
        }
        requests
    });
    (format!("http://{address}/v1"), task)
}

#[tokio::test]
async fn compatible_responses_standard_replay_survives_route_override_deletion_and_template_changes()
 {
    const RESPONSE: &str = "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"output\":[{\"type\":\"message\",\"id\":\"message-1\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"ok\"}]}]}}\n\n";
    for route_template in ["route-one-secret", "${parent_session_id}"] {
        let (endpoint, captured) = server_requests(RESPONSE, 5).await;
        let mut provider = definition(&endpoint, "openai-compatible", false);
        let ProviderDefinition::Custom(custom) = &mut provider else {
            unreachable!()
        };
        custom.auth = toml::from_str("method = \"api-key-header-v1\"\nparameters = { header_name = \"x-api-key\" }\nvalues = { api_key = \"header-secret\" }").unwrap();
        custom.headers.insert(
            HeaderName::new("x-route").unwrap(),
            SafeStaticHeaderValue::new(route_template).unwrap(),
        );
        let model = custom.models.values_mut().next().unwrap();
        model.capabilities.native_replay = None;
        model.options.request_endpoint = Some(cookie_agent_models::RequestEndpoint::Responses);
        model.variants = toml::from_str(
            r#"
[overridden]
headers = { X-Route = "route-two-secret" }
[deleted]
headers = { X-Route = "" }
"#,
        )
        .unwrap();
        let temporary = TempDir::new().unwrap();
        let manager = ModelManager::new(
            BTreeMap::from([(ProviderId::new("gateway").unwrap(), provider)]),
            empty_catalog(),
            store(&temporary),
        )
        .unwrap();
        let runtime = manager.current();
        let base = ModelSelection {
            model: "gateway/test".parse().unwrap(),
            variant: None,
        };
        let request = |history, route: &str| {
            Request::new(history).with_header_context(
                oven_sdk::HeaderContext::new("session").with_parent_session_id(route),
            )
        };
        let resolved = runtime.resolve(&base).unwrap();
        let first = resolved
            .model()
            .complete(
                resolved.prepare_request(request(vec![], "route-one-secret")),
                AbortSignal::default(),
            )
            .await
            .unwrap()
            .turn;
        let scope = first.finish.native_replay.as_ref().unwrap().scope();
        for secret in ["route-one-secret", "header-secret"] {
            assert!(!format!("{scope:?}").contains(secret));
        }
        for (variant, route) in [
            (None, "route-one-secret"),
            (Some("overridden"), "route-one-secret"),
            (Some("deleted"), "route-one-secret"),
            (None, "route-two-secret"),
        ] {
            let selected = runtime
                .resolve(&ModelSelection {
                    variant: variant.map(|value| VariantId::new(value).unwrap()),
                    ..base.clone()
                })
                .unwrap();
            let mut response = selected
                .model()
                .stream(
                    selected.prepare_request(request(
                        vec![HistoryTurn::assistant(first.clone())],
                        route,
                    )),
                    AbortSignal::default(),
                )
                .await
                .unwrap();
            assert!(matches!(
                response.request.replay.decisions[0].disposition,
                oven_sdk::ReplayDisposition::Replayed
            ));
            let diagnostic = format!("{:?}", response.request.replay);
            for secret in ["route-one-secret", "route-two-secret", "header-secret"] {
                assert!(!diagnostic.contains(secret));
            }
            while let Some(part) = response.stream.next().await {
                part.unwrap();
            }
        }
        let requests = captured.await.unwrap();
        for (index, wire) in requests.iter().enumerate() {
            assert!(wire.starts_with("POST /v1/responses HTTP/1.1"));
            assert!(wire.contains("x-api-key: header-secret\r\n"));
            match index {
                2 => assert!(wire.contains("x-route: route-two-secret\r\n")),
                3 => assert!(!wire.contains("x-route:")),
                4 if route_template == "${parent_session_id}" => {
                    assert!(wire.contains("x-route: route-two-secret\r\n"))
                }
                _ => assert!(wire.contains("x-route: route-one-secret\r\n")),
            }
        }
    }
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
capabilities = {{ input = {input}, output = ["text"], context_tokens = 4096, output_tokens = 1024, tool_calling = {tool_calling}, parallel_tool_calls = false, structured_output = false, reasoning = false, temperature = true, top_p = true, seed = false, native_replay = "unsupported", media = {media} }}
"#
    ))
    .unwrap()
}

#[tokio::test]
async fn aliases_and_variant_wire_ids_control_only_encrypted_reasoning_eligibility() {
    const RESPONSE: &str = "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"output\":[{\"type\":\"message\",\"id\":\"msg\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"portable\"}]},{\"type\":\"reasoning\",\"id\":\"rs\",\"summary\":[],\"encrypted_content\":\"opaque-reasoning\"}]}}\n\n";
    let (endpoint, captured) = server_requests(RESPONSE, 3).await;
    let define = || {
        toml::from_str::<ProviderDefinition>(&format!(r#"
source = "custom"
endpoint = "{endpoint}"
adaptor = "openai-compatible"
auth = {{ method = "no-auth-v1", values = {{}} }}
[models.coding]
display_name = "Local Coding Alias"
model_id = "backend-v1"
adaptor_options = {{ request_endpoint = "responses" }}
capabilities = {{ input = ["text"], output = ["text"], context_tokens = 8192, output_tokens = 2048, reasoning = true, temperature = false, top_p = false, seed = false, media = {{}} }}
variants = {{ fast = {{ model_id = "backend-v2" }} }}
"#)).unwrap()
    };
    let temporary = TempDir::new().unwrap();
    let manager = ModelManager::new(
        BTreeMap::from([
            (ProviderId::new("gateway-one").unwrap(), define()),
            (ProviderId::new("gateway-two").unwrap(), define()),
        ]),
        empty_catalog(),
        store(&temporary),
    )
    .unwrap();
    let runtime = manager.current();
    let resolve = |model: &str, variant: Option<&str>| {
        runtime
            .resolve(&ModelSelection {
                model: model.parse().unwrap(),
                variant: variant.map(|id| VariantId::new(id).unwrap()),
            })
            .unwrap()
    };
    let source = resolve("gateway-one/coding", None);
    let first = source
        .model()
        .complete(
            source.prepare_request(Request::new(vec![])),
            AbortSignal::default(),
        )
        .await
        .unwrap()
        .turn;
    assert_eq!(
        first
            .finish
            .native_replay
            .as_ref()
            .unwrap()
            .source_wire_model_id()
            .unwrap()
            .as_str(),
        "backend-v1"
    );
    for (local, variant) in [
        ("gateway-two/coding", None),
        ("gateway-one/coding", Some("fast")),
    ] {
        let target = resolve(local, variant);
        let mut response = target
            .model()
            .stream(
                target.prepare_request(Request::new(vec![HistoryTurn::assistant(first.clone())])),
                AbortSignal::default(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.request.replay.decisions[0].disposition,
            oven_sdk::ReplayDisposition::Replayed
        );
        while let Some(part) = response.stream.next().await {
            part.unwrap();
        }
    }
    let requests = captured.await.unwrap();
    let body = |index: usize| {
        serde_json::from_str::<serde_json::Value>(requests[index].split_once("\r\n\r\n").unwrap().1)
            .unwrap()
    };
    assert_eq!(body(0)["model"], "backend-v1");
    assert_eq!(body(1)["model"], "backend-v1");
    assert_eq!(body(1)["input"].as_array().unwrap().len(), 2);
    assert_eq!(body(1)["input"][1]["encrypted_content"], "opaque-reasoning");
    assert_eq!(body(2)["model"], "backend-v2");
    assert_eq!(body(2)["input"].as_array().unwrap().len(), 1);
    assert_eq!(body(2)["input"][0]["content"][0]["text"], "portable");
}

#[tokio::test]
async fn responses_integrity_markers_do_not_make_ordinary_tools_require_native_reasoning() {
    const PLAIN: &str = "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"output\":[{\"type\":\"function_call\",\"id\":\"item\",\"call_id\":\"call\",\"name\":\"inspect\",\"arguments\":\"{}\"}]}}\n\n";
    const ENCRYPTED: &str = "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"output\":[{\"type\":\"reasoning\",\"id\":\"rs\",\"summary\":[],\"encrypted_content\":\"opaque-state\"},{\"type\":\"function_call\",\"id\":\"item\",\"call_id\":\"call\",\"name\":\"inspect\",\"arguments\":\"{}\"}]}}\n\n";
    for encrypted in [false, true] {
        let expected_requests = if encrypted { 3 } else { 13 };
        let (endpoint, captured) =
            server_requests(if encrypted { ENCRYPTED } else { PLAIN }, expected_requests).await;
        let make = |adaptor: &str, enabled: bool| {
            let azure = adaptor == "azure-openai-responses";
            let endpoint = if azure {
                endpoint.trim_end_matches("/v1")
            } else {
                endpoint.as_str()
            };
            let config = format!(
                r#"
source = "custom"
endpoint = "{endpoint}"
adaptor = "{adaptor}"
setup = {setup}
auth = {{ method = "{auth}", values = {{ api_key = "test-key" }} }}
[models.test]
display_name = "Test"
model_id = "wire-model"
capabilities = {{ input = ["text"], output = ["text"], context_tokens = 8192, output_tokens = 2048, tool_calling = true, parallel_tool_calls = false, structured_output = false, reasoning = {enabled}, temperature = false, top_p = false, seed = false, native_replay = "{replay}", media = {{}} }}
"#,
                setup = if azure {
                    "{ deployment = \"deployment\", api_version = \"2025-03-01\" }"
                } else {
                    "{}"
                },
                auth = if azure {
                    "azure-api-key-v1"
                } else {
                    "bearer-api-key-v1"
                },
                replay = if enabled { "optional" } else { "unsupported" }
            );
            toml::from_str::<ProviderDefinition>(&config).unwrap()
        };
        let temporary = TempDir::new().unwrap();
        let source = ModelManager::new(
            BTreeMap::from([(
                ProviderId::new("source").unwrap(),
                make("azure-openai-responses", true),
            )]),
            empty_catalog(),
            store(&temporary),
        )
        .unwrap();
        let source = source
            .current()
            .resolve(&ModelSelection {
                model: "source/test".parse().unwrap(),
                variant: None,
            })
            .unwrap();
        let first = source
            .model()
            .complete(Request::new(vec![]), AbortSignal::default())
            .await
            .unwrap()
            .turn;
        assert!(first.message.content.iter().any(|part| matches!(part, AssistantPart::Custom(part) if part.kind == oven_sdk::replay::AZURE_RESPONSES_FINGERPRINT_KIND)));
        assert_eq!(first.message.content.iter().any(|part| matches!(part, AssistantPart::Custom(part) if part.kind == oven_sdk::replay::AZURE_RESPONSES_CONTINUATION)), encrypted);
        for adaptor in ["openai-responses", "azure-openai-responses"] {
            for enabled in [false, true] {
                let target_dir = TempDir::new().unwrap();
                let manager = ModelManager::new(
                    BTreeMap::from([(ProviderId::new("target").unwrap(), make(adaptor, enabled))]),
                    empty_catalog(),
                    store(&target_dir),
                )
                .unwrap();
                let target = manager
                    .current()
                    .resolve(&ModelSelection {
                        model: "target/test".parse().unwrap(),
                        variant: None,
                    })
                    .unwrap();
                for damage in ["intact", "missing", "corrupt"] {
                    let mut turn = first.clone();
                    if damage == "missing" {
                        turn.finish.native_replay = None;
                    }
                    if damage == "corrupt" {
                        let artifact = turn.finish.native_replay.take().unwrap();
                        let mut payload = artifact.payload().clone();
                        payload["fingerprint"] = "corrupted-integrity".into();
                        turn.finish.native_replay = Some(
                            oven_sdk::NativeReplayArtifact::new(
                                artifact.adapter_id().clone(),
                                artifact.scope().clone(),
                                payload,
                            )
                            .unwrap()
                            .with_source_wire_model_id(
                                artifact.source_wire_model_id().unwrap().clone(),
                            )
                            .unwrap(),
                        );
                    }
                    let request = Request::new(vec![
                        HistoryTurn::assistant(turn),
                        HistoryTurn::tool(ToolMessage::new(vec![ToolResultPart::new(
                            "call",
                            ToolContent::Text("result".into()),
                        )])),
                    ]);
                    let result = target
                        .model()
                        .complete(request, AbortSignal::default())
                        .await;
                    if encrypted && (!enabled || damage != "intact") {
                        assert_eq!(
                            result.unwrap_err().kind,
                            oven_sdk::ModelErrorKind::Replay,
                            "{adaptor}/{enabled}/{damage}"
                        );
                    } else {
                        let result = result.unwrap();
                        if enabled && damage == "intact" {
                            assert_eq!(
                                result.request.replay.decisions[0].disposition,
                                oven_sdk::ReplayDisposition::Replayed
                            );
                        }
                        if enabled && damage == "corrupt" {
                            assert!(matches!(
                                result.request.replay.decisions[0].disposition,
                                oven_sdk::ReplayDisposition::DiscardedInvalidPayload { .. }
                            ));
                        }
                    }
                }
            }
        }
        let requests = captured.await.unwrap();
        assert_eq!(requests.len(), expected_requests);
        for request in &requests[1..] {
            let body: serde_json::Value =
                serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap();
            assert!(body["input"].as_array().unwrap().iter().any(|item| item["type"] == "function_call_output" && item["call_id"] == "call"));
            assert_eq!(body.to_string().contains("opaque-state"), encrypted);
        }
    }
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
capabilities = {{ input = ["text", "video"], output = ["text"], context_tokens = 4096, output_tokens = 1024, tool_calling = true, parallel_tool_calls = false, structured_output = false, reasoning = false, temperature = true, top_p = true, seed = false, native_replay = "unsupported", media = {{ video = {{ mime_types = ["{video_mime_type}"], max_bytes = 26214400, max_count = 2 }} }} }}
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
    assert!(
        !request
            .to_ascii_lowercase()
            .contains("\r\nuser-agent: opencode/")
    );
    assert!(!request.to_ascii_lowercase().contains("authorization:"));
    assert!(!request.contains("no-auth"));
}

#[tokio::test]
async fn configured_auth_and_session_headers_win_on_the_wire() {
    let response = "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n";
    let (endpoint, captured) = server(response).await;
    let definition = definition(&endpoint, "openai-chat", false);
    let ProviderDefinition::Custom(mut provider) = definition else {
        unreachable!();
    };
    provider.auth =
        toml::from_str("method = \"bearer-api-key-v1\"\nvalues = { api_key = \"typed-secret\" }")
            .unwrap();
    provider.headers.insert(
        HeaderName::new("Authorization").unwrap(),
        SafeStaticHeaderValue::new("Bearer configured").unwrap(),
    );
    provider
        .models
        .get_mut(&ProviderModelId::new("test").unwrap())
        .unwrap()
        .variants
        .insert(
            VariantId::new("parent-route").unwrap(),
            VariantDefinition {
                headers: Some(BTreeMap::from([(
                    HeaderName::new("x-session-id").unwrap(),
                    SafeStaticHeaderValue::new("${parent_session_id}").unwrap(),
                )])),
                ..VariantDefinition::default()
            },
        );
    let global_headers = [
        ("user-agent", "cookie-agent/test"),
        ("x-session-id", "${session_id}"),
        ("x-session-parent-id", "${parent_session_id}"),
    ]
    .into_iter()
    .map(|(name, value)| {
        (
            HeaderName::new(name).unwrap(),
            SafeStaticHeaderValue::new(value).unwrap(),
        )
    })
    .collect();
    let temporary = TempDir::new().unwrap();
    let provider_id = ProviderId::new("custom.headers").unwrap();
    let manager = ModelManager::new_with_headers(
        BTreeMap::from([(provider_id.clone(), ProviderDefinition::Custom(provider))]),
        global_headers,
        empty_catalog(),
        store(&temporary),
    )
    .unwrap();
    let key = ModelKey::new(provider_id, ProviderModelId::new("test").unwrap()).unwrap();
    let runtime = manager.current();
    let manifest = runtime.manifest_payload().unwrap();
    let blueprint = manifest
        .blueprints
        .iter()
        .find(|blueprint| blueprint.selection.model == key)
        .unwrap();
    assert_eq!(
        blueprint.static_headers[&cookie_agent_protocol::HeaderName::new("x-session-id").unwrap()]
            .as_str(),
        "${session_id}"
    );
    assert_eq!(
        blueprint.variants[0].static_headers
            [&cookie_agent_protocol::HeaderName::new("x-session-id").unwrap()]
            .as_str(),
        "${parent_session_id}"
    );
    assert!(
        !serde_json::to_string(&blueprint)
            .unwrap()
            .contains("root-session")
    );
    let resolved = runtime
        .resolve(&ModelSelection {
            model: key,
            variant: None,
        })
        .unwrap();
    let request = Request::new(vec![HistoryTurn::user(UserMessage::new(vec![
        InputPart::Text(TextPart::new("hello")),
    ]))])
    .with_header_context(oven_sdk::HeaderContext::new("root-session"));
    let mut stream = resolved
        .model()
        .stream(resolved.prepare_request(request), AbortSignal::default())
        .await
        .unwrap();
    while let Some(part) = stream.stream.next().await {
        part.unwrap();
    }
    let request = captured.await.unwrap().to_ascii_lowercase();
    assert!(request.contains("\r\nauthorization: bearer configured\r\n"));
    assert!(!request.contains("typed-secret"));
    assert!(request.contains("\r\nuser-agent: cookie-agent/test\r\n"));
    assert!(request.contains("\r\nx-session-id: root-session\r\n"));
    assert!(!request.contains("x-session-parent-id:"));
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
        "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"output\":[{\"type\":\"message\",\"id\":\"out\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"ok\"}]}],\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n",
    )
    .await;
    assert!(request.starts_with("POST /v1/responses HTTP/1.1"));
    assert!(!request.to_ascii_lowercase().contains("authorization:"));
    assert!(!request.contains("no-auth"));
}

#[tokio::test]
async fn compatible_variant_endpoint_selects_responses_tools_effort_and_native_capture() {
    let response = "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"output\":[{\"type\":\"function_call\",\"id\":\"item-1\",\"call_id\":\"call-1\",\"name\":\"inspect\",\"arguments\":\"{}\"}]}}\n\n";
    let (endpoint, captured) = server(response).await;
    let temporary = TempDir::new().unwrap();
    let id = ProviderId::new("gateway").unwrap();
    let mut provider = definition(&endpoint, "openai-compatible", false);
    let ProviderDefinition::Custom(custom) = &mut provider else {
        unreachable!()
    };
    let model = custom.models.values_mut().next().unwrap();
    model.capabilities.reasoning = true;
    model.capabilities.native_replay = None;
    model.variants.insert(
        VariantId::new("high").unwrap(),
        toml::from_str(
            r#"
adaptor_options = { request_endpoint = "responses" }
reasoning = { type = "effort", value = "high" }
"#,
        )
        .unwrap(),
    );
    let manager = ModelManager::new(
        BTreeMap::from([(id.clone(), provider)]),
        empty_catalog(),
        store(&temporary),
    )
    .unwrap();
    let runtime = manager.current();
    let selection = ModelSelection {
        model: ModelKey::new(id, ProviderModelId::new("test").unwrap()).unwrap(),
        variant: Some(VariantId::new("high").unwrap()),
    };
    let resolved = runtime.resolve(&selection).unwrap();
    assert_eq!(
        resolved.adapter_family(),
        cookie_agent_models::adapters::OvenAdapterFamily::OpenaiResponses
    );
    let payload = runtime.manifest_payload().unwrap();
    let blueprint = &payload.blueprints[0];
    assert!(blueprint.descriptor.adapter_id.as_str().contains(".chat."));
    assert!(
        blueprint.variants[0]
            .descriptor
            .adapter_id
            .as_str()
            .contains(".responses.")
    );
    let _: cookie_agent_models::manifests::ModelSnapshotPayloadV1 =
        serde_json::from_value(serde_json::to_value(&payload).unwrap()).unwrap();
    let completed = resolved
        .model()
        .complete(
            resolved.prepare_request(Request::new(vec![])),
            AbortSignal::default(),
        )
        .await
        .unwrap();
    assert!(completed.turn.finish.native_replay.is_some());
    assert!(
        completed
            .turn
            .message
            .content
            .iter()
            .any(|part| matches!(part, AssistantPart::ToolCall(call) if call.name == "inspect"))
    );
    let request = captured.await.unwrap();
    assert!(request.starts_with("POST /v1/responses HTTP/1.1"));
    let body: serde_json::Value =
        serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap();
    assert_eq!(body["reasoning"]["effort"], "high");
    assert_eq!(body["store"], false);
    assert!(body.get("messages").is_none());
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
