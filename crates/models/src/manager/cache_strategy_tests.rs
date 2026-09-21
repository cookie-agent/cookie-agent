use super::*;
use crate::adapters::oven::{AdapterConfig, AuthConfig, CommonDefaults, ConcreteModel};
use crate::adapters::{
    BedrockCachePoint, BedrockCacheStrategy, BedrockCacheTtl, BedrockMessageCachePoint,
    GoogleCacheStrategyConfig, OpenAiCacheStrategyConfig,
};
use crate::{ScriptedModel, ScriptedStep};
use oven_sdk::{
    AbortSignal, InputPart, JsonSchema, StreamPart, SystemMessage, SystemPart, TextPart,
    ToolDefinition, ToolMessage, UserMessage,
};

#[test]
fn video_media_capability_maps_to_open_oven_modality() {
    let capabilities = crate::ModelCapabilities {
        input: BTreeSet::from([crate::Modality::Text, crate::Modality::Video]),
        output: BTreeSet::from([crate::Modality::Text]),
        context_tokens: 128_000,
        output_tokens: 8_192,
        tool_calling: false,
        parallel_tool_calls: false,
        structured_output: false,
        reasoning: false,
        temperature: true,
        top_p: false,
        seed: false,
        compaction: crate::CompactionCapability::Unsupported,
        native_replay: crate::ReplayCapability::Unsupported,
        cancellation: crate::CancellationCapability::LocalOnly,
        media: BTreeMap::from([(
            crate::MediaKind::Video,
            crate::MediaCapability {
                mime_types: BTreeSet::from([crate::MimeType::new("video/mp4").unwrap()]),
                max_bytes: 25 * 1024 * 1024,
                max_count: 2,
            },
        )]),
    };
    let oven = oven_capabilities(&capabilities, OvenAdapterFamily::OpenaiResponses).unwrap();

    assert!(oven.modalities.input.contains(&OvenModality::video()));
    assert!(oven.media.input.contains_key(&OvenModality::video()));
}

fn resolved(prompt_caching: bool, steps: usize) -> (ResolvedExecutableModel, ScriptedModel) {
    let capabilities: OvenCapabilities = serde_json::from_value(json!({
        "features": if prompt_caching { vec!["prompt_caching"] } else { Vec::<&str>::new() },
        "limits": {"context": 4096, "input": null, "output": 1024},
        "modalities": {"input": ["text"], "output": ["text"]},
        "media": {"input": {}},
        "cancellation": "local_only",
        "compaction": "unsupported",
        "replay": {"policy": "never", "capability": "unsupported", "reasoning": false}
    }))
    .unwrap();
    let descriptor = LanguageModelDescriptor::new(
        ModelIdentity::new(OvenProviderId::new("test"), ModelId::new("group/model")).unwrap(),
        AdapterId::new("test.scripted"),
        capabilities,
    )
    .unwrap();
    let scripted = ScriptedModel::new(
        descriptor,
        (0..steps).map(|_| {
            ScriptedStep::stream([Ok(StreamPart::StreamStart {
                warnings: Vec::new(),
            })])
        }),
    );
    let resolved = ResolvedExecutableModel {
        selection: ModelSelection {
            model: "test/group/model".parse().unwrap(),
            variant: None,
        },
        model: Arc::new(scripted.clone()),
        adapter: OvenAdapterFamily::Anthropic,
        defaults: crate::ResolvedRequestDefaults::default(),
        provider_options: BTreeMap::new(),
        behavior_fingerprint: Sha256Digest::new("0".repeat(64)).unwrap(),
    };
    (resolved, scripted)
}

fn strategy() -> CacheStrategyConfig {
    CacheStrategyConfig::Anthropic(AnthropicCacheStrategyConfig {
        system: Some(AnthropicCacheTtlConfig::OneHour),
        tools: Some(AnthropicCacheTtlConfig::OneHour),
        rolling: Some(AnthropicCacheTtlConfig::FiveMinutes),
    })
}

fn marker(options: &oven_sdk::ProviderOptions) -> Option<&str> {
    options
        .get("anthropic")?
        .get("cache_control")?
        .get("ttl")?
        .as_str()
}

fn request() -> Request {
    Request::new(vec![
        oven_sdk::HistoryTurn::system(SystemMessage::new(vec![SystemPart::Text(TextPart::new(
            "stable system",
        ))])),
        oven_sdk::HistoryTurn::user(UserMessage::new(vec![InputPart::Text(TextPart::new(
            "eligible user",
        ))])),
        oven_sdk::HistoryTurn::user(UserMessage::new(vec![InputPart::Text(TextPart::new(""))])),
        oven_sdk::HistoryTurn::tool(ToolMessage::new(Vec::new())),
    ])
    .with_tools(vec![
        ToolDefinition::new(
            "first",
            "first tool",
            JsonSchema::new(json!({"type":"object"})).unwrap(),
        ),
        ToolDefinition::new(
            "last",
            "last tool",
            JsonSchema::new(json!({"type":"object"})).unwrap(),
        ),
    ])
}

#[tokio::test]
async fn strategy_places_three_ordered_markers_with_empty_and_tool_fallback() {
    let (resolved, scripted) = resolved(true, 1);
    let strategy = strategy();
    let prepared = resolved.prepare_request_with_cache_strategy(request(), Some(&strategy));
    let _ = scripted
        .stream(prepared, AbortSignal::default())
        .await
        .unwrap();
    let captured = scripted.requests().pop().unwrap();

    let oven_sdk::HistoryTurn::System(system) = &captured.history[0] else {
        panic!("system turn");
    };
    let oven_sdk::HistoryTurn::Tool(rolling) = &captured.history[3] else {
        panic!("rolling tool turn");
    };
    assert_eq!(marker(&system.provider_options), Some("one_hour"));
    assert_eq!(marker(&rolling.provider_options), Some("five_minutes"));
    assert_eq!(marker(&captured.tools[0].provider_options), None);
    assert_eq!(
        marker(&captured.tools[1].provider_options),
        Some("one_hour")
    );
    assert_eq!(
        captured
            .history
            .iter()
            .filter(|turn| marker(match turn {
                oven_sdk::HistoryTurn::System(message) => &message.provider_options,
                oven_sdk::HistoryTurn::User(message) => &message.provider_options,
                oven_sdk::HistoryTurn::Assistant(turn) => &turn.message.provider_options,
                oven_sdk::HistoryTurn::Tool(message) => &message.provider_options,
            })
            .is_some())
            .count()
            + captured
                .tools
                .iter()
                .filter(|tool| marker(&tool.provider_options).is_some())
                .count(),
        3
    );
}

#[test]
fn capability_gate_and_compaction_reanchor_are_stable() {
    let (without_capability, _) = resolved(false, 0);
    let strategy = strategy();
    let gated = without_capability.prepare_request_with_cache_strategy(request(), Some(&strategy));
    assert!(gated.history.iter().all(|turn| {
        marker(match turn {
            oven_sdk::HistoryTurn::System(message) => &message.provider_options,
            oven_sdk::HistoryTurn::User(message) => &message.provider_options,
            oven_sdk::HistoryTurn::Assistant(turn) => &turn.message.provider_options,
            oven_sdk::HistoryTurn::Tool(message) => &message.provider_options,
        })
        .is_none()
    }));

    let (resolved, _) = resolved(true, 0);
    let mut compacted = request();
    compacted
        .history
        .push(oven_sdk::HistoryTurn::system(SystemMessage::new(vec![
            SystemPart::Text(TextPart::new("compacted summary")),
        ])));
    let prepared = resolved.prepare_request_with_cache_strategy(compacted, Some(&strategy));
    let oven_sdk::HistoryTurn::System(first) = &prepared.history[0] else {
        panic!("first system turn");
    };
    let oven_sdk::HistoryTurn::System(last) = prepared.history.last().unwrap() else {
        panic!("summary system turn");
    };
    let oven_sdk::HistoryTurn::Tool(rolling) = &prepared.history[3] else {
        panic!("rolling tool turn");
    };
    assert_eq!(marker(&first.provider_options), Some("one_hour"));
    assert_eq!(marker(&last.provider_options), None);
    assert_eq!(marker(&rolling.provider_options), Some("five_minutes"));
}

#[test]
fn anthropic_rolling_applies_when_other_placements_are_off() {
    let (resolved, _) = resolved(true, 0);
    let strategy = CacheStrategyConfig::Anthropic(AnthropicCacheStrategyConfig {
        system: None,
        tools: None,
        rolling: Some(AnthropicCacheTtlConfig::FiveMinutes),
    });
    let prepared = resolved.prepare_request_with_cache_strategy(request(), Some(&strategy));
    let oven_sdk::HistoryTurn::Tool(rolling) = &prepared.history[3] else {
        panic!("rolling tool turn");
    };
    assert_eq!(marker(&rolling.provider_options), Some("five_minutes"));
    assert_eq!(marker(&prepared.tools[1].provider_options), None);
}

#[test]
fn bedrock_strategy_expands_last_message_placement() {
    let mut request = Request::new(vec![
        oven_sdk::HistoryTurn::system(SystemMessage::new(vec![SystemPart::Text(TextPart::new(
            "stable system",
        ))])),
        oven_sdk::HistoryTurn::user(UserMessage::new(vec![InputPart::Text(TextPart::new(
            "current input",
        ))])),
        oven_sdk::HistoryTurn::tool(ToolMessage::new(Vec::new())),
    ]);
    let strategy = CacheStrategyConfig::Bedrock(BedrockCacheStrategy {
        system: Some(BedrockCachePoint {
            ttl: Some(BedrockCacheTtl::OneHour),
        }),
        tools: Some(BedrockCachePoint {
            ttl: Some(BedrockCacheTtl::OneHour),
        }),
        messages: vec![BedrockMessageCachePoint {
            history_index: usize::MAX,
            cache_point: BedrockCachePoint {
                ttl: Some(BedrockCacheTtl::FiveMinutes),
            },
        }],
    });

    apply_cache_strategy(
        &mut request,
        OvenAdapterFamily::AwsBedrockConverse,
        &strategy,
    );

    assert_eq!(
        request.provider_options["bedrock"]["cache"],
        json!({
            "system": {"ttl": "1h"},
            "tools": null,
            "messages": [{
                "historyIndex": 2,
                "ttl": "5m"
            }]
        })
    );
}

#[test]
fn google_cache_modes_set_clear_or_preserve_cached_content() {
    let explicit = CacheStrategyConfig::Google(GoogleCacheStrategyConfig {
        mode: GoogleCacheMode::Explicit,
        cached_content: Some("cachedContents/example".into()),
    });
    let mut request = Request::new(Vec::new());
    apply_cache_strategy(&mut request, OvenAdapterFamily::GoogleGemini, &explicit);
    assert_eq!(
        request.provider_options["google"]["cached_content"],
        "cachedContents/example"
    );

    let off = CacheStrategyConfig::Google(GoogleCacheStrategyConfig {
        mode: GoogleCacheMode::Off,
        cached_content: None,
    });
    apply_cache_strategy(&mut request, OvenAdapterFamily::GoogleGemini, &off);
    assert!(
        request.provider_options["google"]
            .get("cached_content")
            .is_none()
    );

    request
        .provider_options
        .insert("google_vertex".into(), json!({"topK": 3}));
    let implicit = CacheStrategyConfig::Google(GoogleCacheStrategyConfig {
        mode: GoogleCacheMode::Implicit,
        cached_content: None,
    });
    apply_cache_strategy(
        &mut request,
        OvenAdapterFamily::GoogleVertexGemini,
        &implicit,
    );
    assert_eq!(request.provider_options["google_vertex"]["topK"], 3);
}

#[test]
fn openai_cache_strategy_uses_endpoint_specific_namespace() {
    let strategy = CacheStrategyConfig::OpenAi(OpenAiCacheStrategyConfig {
        prompt_cache_key: Some("session-key".into()),
        prompt_cache_retention: Some(OpenAiPromptCacheRetention::TwentyFourHours),
        mode: Some(OpenAiCacheMode::Explicit),
        ttl: Some(OpenAiPromptCacheTtl::ThirtyMinutes),
        system: true,
        rolling: true,
    });
    for (adapter, namespace, section) in [
        (OvenAdapterFamily::OpenaiChat, "openai", "chat"),
        (OvenAdapterFamily::OpenaiResponses, "openai", "responses"),
        (OvenAdapterFamily::AzureOpenaiChat, "azure_openai", "chat"),
        (
            OvenAdapterFamily::AzureOpenaiResponses,
            "azure_openai",
            "responses",
        ),
    ] {
        let mut request = Request::new(vec![
            oven_sdk::HistoryTurn::system(SystemMessage::new(vec![SystemPart::Text(
                TextPart::new("stable system"),
            )])),
            oven_sdk::HistoryTurn::user(UserMessage::new(vec![InputPart::Text(TextPart::new(
                "eligible latest user",
            ))])),
        ]);
        apply_cache_strategy(&mut request, adapter, &strategy);
        assert_eq!(
            request.provider_options[namespace][section]["prompt_cache_key"],
            "session-key"
        );
        assert_eq!(
            request.provider_options[namespace][section]["prompt_cache_retention"],
            "24h"
        );
        assert_eq!(
            request.provider_options[namespace][section]["prompt_cache_options"],
            json!({"mode":"explicit", "ttl":"30m"})
        );
        let marker = if namespace == "openai" {
            "openai.prompt_cache_breakpoint"
        } else {
            "azure_openai.prompt_cache_breakpoint"
        };
        let oven_sdk::HistoryTurn::System(system) = &request.history[0] else {
            panic!("system turn");
        };
        let oven_sdk::SystemPart::Text(system) = &system.content[0] else {
            panic!("system text");
        };
        let oven_sdk::HistoryTurn::User(rolling) = &request.history[1] else {
            panic!("rolling user turn");
        };
        let oven_sdk::InputPart::Text(rolling) = &rolling.content[0] else {
            panic!("rolling user text");
        };
        assert_eq!(
            system
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get(marker))
                .and_then(Value::as_bool),
            Some(true)
        );
        assert_eq!(
            rolling
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get(marker))
                .and_then(Value::as_bool),
            Some(true)
        );
    }
}

#[test]
fn compatible_openai_strategy_writes_the_official_chat_cache_key() {
    let strategy = CacheStrategyConfig::OpenAi(OpenAiCacheStrategyConfig {
        prompt_cache_key: Some("session-key".into()),
        prompt_cache_retention: None,
        mode: None,
        ttl: None,
        system: false,
        rolling: false,
    });
    let mut request = Request::new(Vec::new());
    apply_cache_strategy(&mut request, OvenAdapterFamily::OpenaiCompatible, &strategy);
    assert_eq!(
        request.provider_options["openai"]["chat"]["prompt_cache_key"],
        "session-key"
    );
}

#[test]
fn removed_managed_provider_cache_is_validated_without_catalog_metadata() {
    let provider_id = ProviderId::new("removed").unwrap();
    let provider = ModelsDevProvider {
        base_url: None,
        setup: BTreeMap::new(),
        api_key: None,
        auth_override: None,
        cache: Some(
            serde_json::from_value(json!({"mode":"implicit"})).expect("provider cache envelope"),
        ),
        headers: BTreeMap::new(),
        model_overrides: BTreeMap::new(),
    };
    let error = validate_unresolved_managed_cache(&provider_id, &provider, None)
        .unwrap_err()
        .to_string();
    assert!(error.contains("provider `removed`"), "{error}");
    assert!(error.contains("implicit"), "{error}");
    assert!(error.contains("auto"), "{error}");
}

fn openai_strategy(system: bool, rolling: bool) -> CacheStrategyConfig {
    CacheStrategyConfig::OpenAi(OpenAiCacheStrategyConfig {
        prompt_cache_key: None,
        prompt_cache_retention: None,
        mode: Some(OpenAiCacheMode::Explicit),
        ttl: Some(OpenAiPromptCacheTtl::ThirtyMinutes),
        system,
        rolling,
    })
}

fn has_openai_breakpoint(metadata: &oven_sdk::PartMetadata) -> bool {
    metadata
        .as_ref()
        .and_then(|metadata| metadata.get("openai.prompt_cache_breakpoint"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

#[test]
fn openai_rolling_uses_latest_user_turn_with_nonempty_text() {
    let mut request = Request::new(vec![
        oven_sdk::HistoryTurn::user(UserMessage::new(vec![InputPart::Text(TextPart::new(
            "earlier eligible text",
        ))])),
        oven_sdk::HistoryTurn::user(UserMessage::new(vec![InputPart::File(
            oven_sdk::FilePart::document(
                "application/pdf",
                oven_sdk::FileSource::Text("latest file".into()),
            ),
        )])),
    ]);

    apply_cache_strategy(
        &mut request,
        OvenAdapterFamily::OpenaiChat,
        &openai_strategy(false, true),
    );

    let oven_sdk::HistoryTurn::User(earlier) = &request.history[0] else {
        panic!("earlier user turn");
    };
    let InputPart::Text(earlier) = &earlier.content[0] else {
        panic!("earlier user text");
    };
    let oven_sdk::HistoryTurn::User(latest) = &request.history[1] else {
        panic!("latest user turn");
    };
    let InputPart::File(latest) = &latest.content[0] else {
        panic!("latest user file");
    };
    assert!(has_openai_breakpoint(&earlier.metadata));
    assert!(!has_openai_breakpoint(&latest.metadata));
}

#[test]
fn openai_system_does_not_fall_forward_from_ineligible_first_turn() {
    let mut request = Request::new(vec![
        oven_sdk::HistoryTurn::system(SystemMessage::new(vec![
            SystemPart::Text(TextPart::new("")),
            SystemPart::Custom(oven_sdk::CustomPart::new(
                "test.system",
                json!({"value":"not text"}),
            )),
        ])),
        oven_sdk::HistoryTurn::system(SystemMessage::new(vec![SystemPart::Text(TextPart::new(
            "later eligible system text",
        ))])),
    ]);

    apply_cache_strategy(
        &mut request,
        OvenAdapterFamily::OpenaiChat,
        &openai_strategy(true, false),
    );

    let oven_sdk::HistoryTurn::System(first) = &request.history[0] else {
        panic!("first system turn");
    };
    let SystemPart::Text(first) = &first.content[0] else {
        panic!("empty system text");
    };
    let oven_sdk::HistoryTurn::System(later) = &request.history[1] else {
        panic!("later system turn");
    };
    let SystemPart::Text(later) = &later.content[0] else {
        panic!("later system text");
    };
    assert!(!has_openai_breakpoint(&first.metadata));
    assert!(!has_openai_breakpoint(&later.metadata));
}

#[test]
fn openai_system_breakpoint_index_is_stable_after_translated_system_emission() {
    let base_history = vec![
        oven_sdk::HistoryTurn::system(SystemMessage::new(vec![
            SystemPart::Text(TextPart::new("stable system one")),
            SystemPart::Text(TextPart::new("stable system two")),
        ])),
        oven_sdk::HistoryTurn::user(UserMessage::new(vec![InputPart::Text(TextPart::new(
            "conversation",
        ))])),
    ];
    let mut without_emission = Request::new(base_history.clone());
    let mut with_emission = Request::new(base_history);
    with_emission
        .history
        .push(oven_sdk::HistoryTurn::user(UserMessage::new(vec![
            InputPart::Text(TextPart::new(
                "[tool-emitted system message; materialized as user history]",
            )),
            InputPart::Text(TextPart::new("emitted system context")),
        ])));

    for request in [&mut without_emission, &mut with_emission] {
        apply_cache_strategy(
            request,
            OvenAdapterFamily::OpenaiChat,
            &openai_strategy(true, false),
        );
    }
    let breakpoint_index = |request: &Request| {
        let oven_sdk::HistoryTurn::System(system) = &request.history[0] else {
            panic!("initial system turn");
        };
        system.content.iter().position(
            |part| matches!(part, SystemPart::Text(text) if has_openai_breakpoint(&text.metadata)),
        )
    };
    assert_eq!(breakpoint_index(&without_emission), Some(1));
    assert_eq!(
        breakpoint_index(&with_emission),
        breakpoint_index(&without_emission)
    );
    let oven_sdk::HistoryTurn::System(system) = &with_emission.history[0] else {
        panic!("initial system turn");
    };
    assert_eq!(system.content.len(), 2);
}

fn real_openai_resolved(adapter: OvenAdapterFamily, endpoint: String) -> ResolvedExecutableModel {
    let capabilities = resolved(false, 0).0.model().capabilities().clone();
    if matches!(
        adapter,
        OvenAdapterFamily::AzureOpenaiChat | OvenAdapterFamily::AzureOpenaiResponses
    ) {
        let provider = oven_sdk::ProviderConfig::new(
            OvenProviderId::new(oven_sdk_azure::AZURE_OPENAI_PROVIDER_ID),
            oven_sdk::ApiEndpoint::parse(endpoint).unwrap(),
            oven_sdk_azure::AzureOpenAiAuth::ApiKey(oven_sdk::SecretString::new("test-key")),
            oven_sdk::HeaderConfig::empty(),
        )
        .unwrap();
        let declaration =
            oven_sdk::ModelDeclaration::new(ModelId::new("gpt-5.6-test"), capabilities).unwrap();
        let model: Arc<dyn LanguageModel> = match adapter {
            OvenAdapterFamily::AzureOpenaiChat => Arc::new(
                oven_sdk_azure::AzureOpenAiChatModel::new(oven_sdk::ModelConfig::new(
                    provider,
                    declaration,
                    oven_sdk_azure::AzureOpenAiChatSettings::default(),
                ))
                .unwrap(),
            ),
            OvenAdapterFamily::AzureOpenaiResponses => Arc::new(
                oven_sdk_azure::AzureOpenAiResponsesModel::new(oven_sdk::ModelConfig::new(
                    provider,
                    declaration,
                    oven_sdk_azure::AzureOpenAiResponsesSettings::default(),
                ))
                .unwrap(),
            ),
            _ => unreachable!("Azure family checked"),
        };
        return ResolvedExecutableModel {
            selection: ModelSelection {
                model: "test/group/model".parse().unwrap(),
                variant: None,
            },
            model,
            adapter,
            defaults: crate::ResolvedRequestDefaults::default(),
            provider_options: BTreeMap::new(),
            behavior_fingerprint: Sha256Digest::new("0".repeat(64)).unwrap(),
        };
    }
    let adapter_config: AdapterConfig = serde_json::from_value(match adapter {
        OvenAdapterFamily::OpenaiChat => json!({
            "adaptor":"openai-chat",
            "settings":{
                "system_message_role":"developer",
                "max_tokens_field":"max_tokens",
                "stream_usage":false,
                "structured_output":"unsupported",
                "reasoning_field":"none",
                "routing_discriminator":null
            },
            "options":{}
        }),
        OvenAdapterFamily::OpenaiResponses => json!({
            "adaptor":"openai-responses",
            "settings":{"routing_discriminator":null,"compaction":"unsupported"},
            "options":{}
        }),
        OvenAdapterFamily::AzureOpenaiChat => json!({
            "adaptor":"azure-chat",
            "settings":{
                "route":{"kind":"v1"},
                "revision":null,
                "system_role":"developer",
                "max_tokens_field":"max_tokens",
                "stream_usage":false,
                "structured_output":"unsupported",
                "reasoning_field":"none",
                "omit_reasoning_sampling":false
            },
            "options":{}
        }),
        OvenAdapterFamily::AzureOpenaiResponses => json!({
            "adaptor":"azure-responses",
            "settings":{
                "route":{"kind":"v1"},
                "revision":null,
                "compaction":{"kind":"unsupported"}
            },
            "options":{}
        }),
        _ => panic!("OpenAI endpoint family"),
    })
    .unwrap();
    let constructed = ConcreteModel {
        provider_id: if matches!(
            adapter,
            OvenAdapterFamily::AzureOpenaiChat | OvenAdapterFamily::AzureOpenaiResponses
        ) {
            "azure.openai".into()
        } else {
            "openai".into()
        },
        model_id: "gpt-5.6-test".into(),
        endpoint,
        auth: if matches!(
            adapter,
            OvenAdapterFamily::AzureOpenaiChat | OvenAdapterFamily::AzureOpenaiResponses
        ) {
            AuthConfig::ApiKey {
                value: "test-key".into(),
            }
        } else {
            AuthConfig::Openai {
                api_key: "test-key".into(),
                organization: None,
                project: None,
            }
        },
        headers: BTreeMap::new(),
        capabilities,
        defaults: CommonDefaults::default(),
        adapter: adapter_config,
    }
    .build()
    .unwrap();
    ResolvedExecutableModel {
        selection: ModelSelection {
            model: "test/group/model".parse().unwrap(),
            variant: None,
        },
        model: constructed.model,
        adapter,
        defaults: crate::ResolvedRequestDefaults::default(),
        provider_options: constructed.provider_options,
        behavior_fingerprint: Sha256Digest::new("0".repeat(64)).unwrap(),
    }
}

async fn wire_capture_server(
    responses: bool,
) -> (String, tokio::task::JoinHandle<serde_json::Value>) {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        let mut buffer = [0_u8; 4096];
        let (body_start, content_length) = loop {
            let read = socket.read(&mut buffer).await.unwrap();
            assert!(read > 0, "request ended before headers");
            request.extend_from_slice(&buffer[..read]);
            let Some(header_end) = request.windows(4).position(|value| value == b"\r\n\r\n") else {
                continue;
            };
            let headers = std::str::from_utf8(&request[..header_end]).unwrap();
            let content_length = headers
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length: ")
                        .map(str::to_owned)
                })
                .unwrap()
                .parse::<usize>()
                .unwrap();
            break (header_end + 4, content_length);
        };
        while request.len() < body_start + content_length {
            let read = socket.read(&mut buffer).await.unwrap();
            assert!(read > 0, "request ended before body");
            request.extend_from_slice(&buffer[..read]);
        }
        let body =
            serde_json::from_slice(&request[body_start..body_start.saturating_add(content_length)])
                .unwrap();
        let stream = if responses {
            concat!(
                "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\",\"model\":\"gpt-5.6-test\"}}\n\n",
                "event: response.output_item.added\ndata: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"message\",\"id\":\"msg_1\",\"role\":\"assistant\",\"content\":[]}}\n\n",
                "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"content_index\":0,\"delta\":\"ok\"}\n\n",
                "event: response.output_item.done\ndata: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"message\",\"id\":\"msg_1\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"ok\"}]}}\n\n",
                "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"model\":\"gpt-5.6-test\",\"status\":\"completed\",\"output\":[{\"type\":\"message\",\"id\":\"msg_1\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"ok\"}]}],\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n"
            )
        } else {
            "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n"
        };
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{stream}",
            stream.len()
        );
        socket.write_all(response.as_bytes()).await.unwrap();
        body
    });
    (format!("http://{address}/v1"), task)
}

fn wire_breakpoint_count(value: &Value) -> usize {
    match value {
        Value::Object(object) => {
            usize::from(object.contains_key("prompt_cache_breakpoint"))
                + object.values().map(wire_breakpoint_count).sum::<usize>()
        }
        Value::Array(values) => values.iter().map(wire_breakpoint_count).sum(),
        _ => 0,
    }
}

#[tokio::test]
async fn openai_cache_controls_reach_all_four_provider_wires() {
    let strategy = CacheStrategyConfig::OpenAi(OpenAiCacheStrategyConfig {
        prompt_cache_key: Some("wire-key".into()),
        prompt_cache_retention: Some(OpenAiPromptCacheRetention::TwentyFourHours),
        mode: Some(OpenAiCacheMode::Explicit),
        ttl: Some(OpenAiPromptCacheTtl::ThirtyMinutes),
        system: true,
        rolling: true,
    });
    for adapter in [
        OvenAdapterFamily::OpenaiChat,
        OvenAdapterFamily::OpenaiResponses,
        OvenAdapterFamily::AzureOpenaiChat,
        OvenAdapterFamily::AzureOpenaiResponses,
    ] {
        let responses = matches!(
            adapter,
            OvenAdapterFamily::OpenaiResponses | OvenAdapterFamily::AzureOpenaiResponses
        );
        let (mut endpoint, captured) = wire_capture_server(responses).await;
        if matches!(
            adapter,
            OvenAdapterFamily::AzureOpenaiChat | OvenAdapterFamily::AzureOpenaiResponses
        ) {
            endpoint.truncate(endpoint.len() - "/v1".len());
        }
        let model = real_openai_resolved(adapter, endpoint);
        let request = Request::new(vec![
            oven_sdk::HistoryTurn::system(SystemMessage::new(vec![SystemPart::Text(
                TextPart::new("stable system"),
            )])),
            oven_sdk::HistoryTurn::user(UserMessage::new(vec![InputPart::Text(TextPart::new(
                "current input",
            ))])),
        ]);
        let request = model.prepare_request_with_cache_strategy(request, Some(&strategy));
        model
            .model()
            .complete(request, AbortSignal::default())
            .await
            .unwrap();
        let body = captured.await.unwrap();
        assert_eq!(body["prompt_cache_key"], "wire-key", "{adapter:?}");
        assert_eq!(body["prompt_cache_retention"], "24h", "{adapter:?}");
        assert_eq!(
            body["prompt_cache_options"],
            json!({"mode":"explicit", "ttl":"30m"}),
            "{adapter:?}"
        );
        assert_eq!(wire_breakpoint_count(&body), 2, "{adapter:?}");
    }
}
