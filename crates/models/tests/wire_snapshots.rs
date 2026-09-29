#![cfg(unix)]

//! Golden wire requests for every adapter family.
//!
//! Each configured model, variant, and prompt-cache strategy below is compiled
//! through [`ModelManager`] and sent to a local capture server. The request
//! line, headers, and body, together with the prepared request's provider
//! options, are compared with `fixtures/adapter_wire_snapshots.json`, which
//! also records each model variant's adapter family and Oven descriptor.
//! Regenerate the golden with
//! `UPDATE_WIRE_SNAPSHOTS=1 cargo test -p cookie_agent_models --test wire_snapshots`.

use std::{collections::BTreeMap, fs, os::unix::fs::PermissionsExt as _, path::PathBuf, sync::Arc};

use cookie_agent_identity::{CatalogRevision, ModelSelection, ProviderId, ProviderModelId};
use cookie_agent_models::{
    ModelManager, ProviderDefinition,
    adapters::{
        AnthropicCacheStrategyConfig, AnthropicCacheTtlConfig, BedrockCachePoint,
        BedrockCacheStrategy, BedrockCacheTtl, BedrockMessageCachePoint, CacheStrategyConfig,
        GoogleCacheMode, GoogleCacheStrategyConfig, OpenAiCacheMode, OpenAiCacheStrategyConfig,
        OpenAiPromptCacheRetention, OpenAiPromptCacheTtl, OvenAdapterFamily,
    },
    catalog::{
        CatalogAgeState, CatalogAvailability, CatalogInterleaved, CatalogLimits, CatalogModalities,
        CatalogModelEntry, CatalogModelRecord, CatalogModelStatus, CatalogProviderEntry,
        CatalogProviderRecord, CatalogReasoningOption, CatalogRuntimeState, CatalogSnapshot,
        CatalogSource,
    },
    provider_store::ProviderStore,
};
use futures_util::StreamExt as _;
use jiff::Timestamp;
use oven_sdk::{
    AbortSignal, HeaderContext, HistoryTurn, InputPart, JsonSchema, Request, SystemMessage,
    SystemPart, TextPart, ToolDefinition, UserMessage,
};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use tempfile::TempDir;
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    sync::mpsc,
};

const OUTPUT_TOKENS: u64 = 32_000;

/// Accepts connections forever and forwards each complete HTTP request.
async fn capture_server() -> (String, mpsc::UnboundedReceiver<String>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (sender, receiver) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let mut request = Vec::new();
            let mut buffer = [0_u8; 8192];
            let mut expected = None;
            while let Ok(read) = socket.read(&mut buffer).await {
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..read]);
                if expected.is_none()
                    && let Some(header_end) =
                        request.windows(4).position(|window| window == b"\r\n\r\n")
                {
                    let headers = String::from_utf8_lossy(&request[..header_end]);
                    let length = headers
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().ok())
                                .flatten()
                        })
                        .unwrap_or(0);
                    expected = Some(header_end + 4 + length);
                }
                if expected.is_some_and(|expected| request.len() >= expected) {
                    break;
                }
            }
            // Forward before responding, so a completed client call always
            // finds its request already queued.
            if sender
                .send(String::from_utf8_lossy(&request).into_owned())
                .is_err()
            {
                return;
            }
            let _ = socket
                .write_all(
                    b"HTTP/1.1 400 Bad Request\r\ncontent-type: application/json\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}",
                )
                .await;
        }
    });
    (format!("http://{address}"), receiver)
}

fn capabilities(reasoning: bool, structured: bool, temperature: bool, replay: &str) -> String {
    let replay = if replay.is_empty() {
        String::new()
    } else {
        format!(", native_replay = \"{replay}\"")
    };
    format!(
        "capabilities = {{ input = [\"text\"], output = [\"text\"], context_tokens = 200000, output_tokens = {OUTPUT_TOKENS}, tool_calling = true, parallel_tool_calls = true, structured_output = {structured}, reasoning = {reasoning}, temperature = {temperature}, top_p = true, seed = false, media = {{}}{replay} }}"
    )
}

struct ModelSpec {
    id: &'static str,
    wire: &'static str,
    reasoning: bool,
    structured: bool,
    temperature: bool,
    replay: &'static str,
    variants: &'static [(&'static str, &'static str)],
    options: &'static str,
}

const EFFORT_HIGH: (&str, &str) = (
    "effort-high",
    r#"reasoning = { type = "effort", value = "high" }"#,
);
const EFFORT_MINIMAL: (&str, &str) = (
    "effort-minimal",
    r#"reasoning = { type = "effort", value = "minimal" }"#,
);
const EFFORT_MAX: (&str, &str) = (
    "effort-max",
    r#"reasoning = { type = "effort", value = "max" }"#,
);
const TOGGLE_ON: (&str, &str) = (
    "toggle-on",
    r#"reasoning = { type = "toggle", enabled = true }"#,
);
const TOGGLE_OFF: (&str, &str) = (
    "toggle-off",
    r#"reasoning = { type = "toggle", enabled = false }"#,
);
const BUDGET: (&str, &str) = (
    "budget",
    r#"reasoning = { type = "budget_tokens", value = 4096 }"#,
);
const BUDGET_ZERO: (&str, &str) = (
    "budget-zero",
    r#"reasoning = { type = "budget_tokens", value = 0 }"#,
);

const ALL_REASONING: &[(&str, &str)] = &[
    EFFORT_HIGH,
    EFFORT_MAX,
    TOGGLE_ON,
    TOGGLE_OFF,
    BUDGET,
    BUDGET_ZERO,
];
const EFFORT_ONLY: &[(&str, &str)] = &[EFFORT_HIGH, EFFORT_MINIMAL];
const COHERE_REASONING: &[(&str, &str)] = &[TOGGLE_ON, TOGGLE_OFF, BUDGET, BUDGET_ZERO];

const fn model(
    id: &'static str,
    wire: &'static str,
    reasoning: bool,
    variants: &'static [(&'static str, &'static str)],
) -> ModelSpec {
    ModelSpec {
        id,
        wire,
        reasoning,
        structured: true,
        temperature: true,
        replay: "",
        variants,
        options: "",
    }
}

struct ProviderSpec {
    id: &'static str,
    adaptor: &'static str,
    path: &'static str,
    setup: &'static str,
    auth: &'static str,
    headers: &'static str,
    models: Vec<ModelSpec>,
}

fn custom_definition(base: &str, provider: &ProviderSpec) -> ProviderDefinition {
    let mut text = format!(
        "source = \"custom\"\nendpoint = \"{base}{path}\"\nadaptor = \"{adaptor}\"\n",
        path = provider.path,
        adaptor = provider.adaptor,
    );
    if !provider.setup.is_empty() {
        text.push_str(&format!("setup = {}\n", provider.setup));
    }
    text.push_str(&format!("auth = {}\n", provider.auth));
    if !provider.headers.is_empty() {
        text.push_str(&format!("headers = {}\n", provider.headers));
    }
    for model in &provider.models {
        text.push_str(&format!(
            "\n[models.{id}]\ndisplay_name = \"{id}\"\nmodel_id = \"{wire}\"\n{capabilities}\n",
            id = model.id,
            wire = model.wire,
            capabilities = capabilities(
                model.reasoning,
                model.structured,
                model.temperature,
                model.replay
            ),
        ));
        if !model.options.is_empty() {
            text.push_str(&format!("adaptor_options = {}\n", model.options));
        }
        for (variant, body) in model.variants {
            text.push_str(&format!(
                "\n[models.{id}.variants.{variant}]\n{body}\n",
                id = model.id
            ));
        }
    }
    toml::from_str(&text).unwrap_or_else(|error| panic!("{}: {error}\n{text}", provider.id))
}

fn custom_providers() -> Vec<ProviderSpec> {
    vec![
        ProviderSpec {
            id: "custom.anthropic",
            adaptor: "anthropic",
            path: "/v1",
            setup: "",
            auth: r#"{ method = "anthropic-api-key-v1", values = { api_key = "anthropic-key" } }"#,
            headers: r#"{ x-session-id = "${session_id}", x-static = "fixed" }"#,
            models: vec![
                ModelSpec {
                    variants: &[
                        EFFORT_HIGH,
                        EFFORT_MAX,
                        TOGGLE_ON,
                        TOGGLE_OFF,
                        BUDGET,
                        BUDGET_ZERO,
                        (
                            "beta",
                            r#"adaptor_options = { beta = ["files-api-2025-04-14"] }
reasoning = { type = "effort", value = "low" }"#,
                        ),
                    ],
                    ..model("sonnet", "claude-sonnet-4-6", true, &[])
                },
                model("legacy", "claude-3-7-sonnet-20250219", true, ALL_REASONING),
                model("plain", "claude-haiku-3", false, &[]),
            ],
        },
        ProviderSpec {
            id: "custom.anthropic-compatible-key",
            adaptor: "anthropic-compatible",
            path: "/v1",
            setup: "",
            auth: r#"{ method = "anthropic-api-key-v1", values = { api_key = "compatible-key" } }"#,
            headers: "",
            models: vec![
                ModelSpec {
                    temperature: false,
                    ..model("thinker", "kimi-k3", true, ALL_REASONING)
                },
                model("plain", "glm-4.7", false, &[]),
            ],
        },
        ProviderSpec {
            id: "custom.anthropic-compatible-bearer",
            adaptor: "anthropic-compatible",
            path: "/v1",
            setup: "",
            auth: r#"{ method = "bearer-api-key-v1", values = { api_key = "compatible-bearer" } }"#,
            headers: "",
            models: vec![model("thinker", "claude-opus-4-5", true, &[EFFORT_HIGH])],
        },
        ProviderSpec {
            id: "custom.anthropic-compatible-open",
            adaptor: "anthropic-compatible",
            path: "/v1",
            setup: "",
            auth: r#"{ method = "no-auth-v1", values = {} }"#,
            headers: "",
            models: vec![model("plain", "local-model", false, &[])],
        },
        ProviderSpec {
            id: "custom.openai-chat",
            adaptor: "openai-chat",
            path: "/v1",
            setup: "",
            auth: r#"{ method = "bearer-api-key-v1", values = { api_key = "openai-key" } }"#,
            headers: "",
            models: vec![
                ModelSpec {
                    options: r#"{ organization = "org-1", project = "proj-1" }"#,
                    ..model("reasoner", "gpt-5.6", true, EFFORT_ONLY)
                },
                ModelSpec {
                    structured: false,
                    ..model("plain", "gpt-4.1", false, &[])
                },
            ],
        },
        ProviderSpec {
            id: "custom.openai-chat-open",
            adaptor: "openai-chat",
            path: "/v1",
            setup: "",
            auth: r#"{ method = "no-auth-v1", values = {} }"#,
            headers: "",
            models: vec![ModelSpec {
                options: r#"{ organization = "org-2", project = "proj-2" }"#,
                ..model("reasoner", "local-reasoner", true, &[EFFORT_HIGH])
            }],
        },
        ProviderSpec {
            id: "custom.openai-responses",
            adaptor: "openai-responses",
            path: "/v1",
            setup: "",
            auth: r#"{ method = "bearer-api-key-v1", values = { api_key = "openai-key" } }"#,
            headers: "",
            models: vec![
                ModelSpec {
                    options: r#"{ organization = "org-1", project = "proj-1" }"#,
                    ..model("reasoner", "gpt-5.6", true, EFFORT_ONLY)
                },
                ModelSpec {
                    structured: false,
                    ..model("plain", "gpt-4.1", false, &[])
                },
            ],
        },
        ProviderSpec {
            id: "custom.openai-responses-open",
            adaptor: "openai-responses",
            path: "/v1",
            setup: "",
            auth: r#"{ method = "no-auth-v1", values = {} }"#,
            headers: r#"{ x-route = "${env:COOKIE_WIRE_SNAPSHOT_UNSET:-default-route}" }"#,
            models: vec![model("reasoner", "local-reasoner", true, &[EFFORT_HIGH])],
        },
        ProviderSpec {
            id: "custom.openai-compatible",
            adaptor: "openai-compatible",
            path: "/v1",
            setup: "",
            auth: r#"{ method = "bearer-api-key-v1", values = { api_key = "compatible-key" } }"#,
            headers: "",
            models: vec![
                ModelSpec {
                    variants: &[
                        EFFORT_HIGH,
                        (
                            "responses",
                            r#"adaptor_options = { request_endpoint = "responses" }
reasoning = { type = "effort", value = "high" }"#,
                        ),
                    ],
                    ..model("reasoner", "qwen3", true, &[])
                },
                ModelSpec {
                    structured: false,
                    ..model("plain", "llama", false, &[])
                },
            ],
        },
        ProviderSpec {
            id: "custom.openai-compatible-header",
            adaptor: "openai-compatible",
            path: "/v1",
            setup: "",
            auth: r#"{ method = "api-key-header-v1", parameters = { header_name = "x-api-key" }, values = { api_key = "header-key" } }"#,
            headers: r#"{ x-session-id = "${session_id}" }"#,
            models: vec![ModelSpec {
                variants: &[
                    EFFORT_HIGH,
                    (
                        "responses",
                        r#"adaptor_options = { request_endpoint = "responses" }"#,
                    ),
                ],
                ..model("reasoner", "qwen3", true, &[])
            }],
        },
        ProviderSpec {
            id: "custom.openai-compatible-open",
            adaptor: "openai-compatible",
            path: "/v1",
            setup: "",
            auth: r#"{ method = "no-auth-v1", values = {} }"#,
            headers: "",
            models: vec![model("plain", "local-model", false, &[])],
        },
        ProviderSpec {
            id: "custom.google",
            adaptor: "google-gemini",
            path: "/v1beta",
            setup: "",
            auth: r#"{ method = "google-api-key-header-v1", values = { api_key = "google-key" } }"#,
            headers: "",
            models: vec![
                model("thinker", "gemini-3-pro", true, ALL_REASONING),
                ModelSpec {
                    structured: false,
                    replay: "unsupported",
                    ..model("plain", "gemini-2.0-flash", false, &[])
                },
            ],
        },
        ProviderSpec {
            id: "custom.vertex",
            adaptor: "google-vertex-gemini",
            path: "/v1",
            setup: r#"{ project = "project-1", location = "us-central1", resource = "publishers/google" }"#,
            auth: r#"{ method = "oauth-access-token-v1", values = { access_token = "vertex-token" } }"#,
            headers: "",
            models: vec![
                model("thinker", "gemini-3-pro", true, ALL_REASONING),
                ModelSpec {
                    structured: false,
                    ..model("plain", "gemini-2.0-flash", false, &[])
                },
            ],
        },
        ProviderSpec {
            id: "custom.bedrock",
            adaptor: "aws-bedrock-converse",
            path: "",
            setup: r#"{ region = "us-east-1" }"#,
            auth: r#"{ method = "aws-sigv4-credentials-v1", values = { access_key_id = "access-key", secret_access_key = "secret-key", session_token = "session-token" } }"#,
            headers: "",
            models: vec![
                ModelSpec {
                    replay: "required",
                    ..model(
                        "claude",
                        "us.anthropic.claude-sonnet-4-6",
                        true,
                        ALL_REASONING,
                    )
                },
                model(
                    "claude-legacy",
                    "anthropic.claude-3-7-sonnet-20250219-v1:0",
                    true,
                    &[EFFORT_HIGH, TOGGLE_ON, BUDGET],
                ),
                model("nova", "amazon.nova-pro-v1:0", true, ALL_REASONING),
                ModelSpec {
                    structured: false,
                    ..model("plain", "meta.llama3-70b-instruct-v1:0", false, &[])
                },
            ],
        },
        ProviderSpec {
            id: "custom.bedrock-static",
            adaptor: "aws-bedrock-converse",
            path: "",
            setup: r#"{ region = "eu-west-1" }"#,
            auth: r#"{ method = "aws-sigv4-credentials-v1", values = { access_key_id = "access-key", secret_access_key = "secret-key" } }"#,
            headers: "",
            models: vec![model("nova", "amazon.nova-lite-v1:0", false, &[])],
        },
        ProviderSpec {
            id: "custom.azure-chat",
            adaptor: "azure-openai-chat",
            path: "",
            setup: r#"{ deployment = "deployment", api_version = "2025-03-01", model = "gpt-5.6", version = "2026-01-01", deployment_type = "GlobalStandard" }"#,
            auth: r#"{ method = "azure-api-key-v1", values = { api_key = "azure-key" } }"#,
            headers: "",
            models: vec![
                model("reasoner", "gpt-5.6", true, EFFORT_ONLY),
                ModelSpec {
                    structured: false,
                    ..model("plain", "gpt-4.1", false, &[])
                },
            ],
        },
        ProviderSpec {
            id: "custom.azure-chat-bare",
            adaptor: "azure-openai-chat",
            path: "",
            setup: r#"{ deployment = "deployment", api_version = "2025-03-01" }"#,
            auth: r#"{ method = "azure-api-key-v1", values = { api_key = "azure-key" } }"#,
            headers: "",
            models: vec![model("plain", "gpt-4.1", false, &[])],
        },
        ProviderSpec {
            id: "custom.azure-responses",
            adaptor: "azure-openai-responses",
            path: "",
            setup: r#"{ deployment = "deployment", api_version = "2025-03-01", model = "gpt-5.6", version = "2026-01-01", deployment_type = "GlobalStandard" }"#,
            auth: r#"{ method = "azure-api-key-v1", values = { api_key = "azure-key" } }"#,
            headers: "",
            models: vec![
                model("reasoner", "gpt-5.6", true, EFFORT_ONLY),
                ModelSpec {
                    structured: false,
                    ..model("plain", "gpt-4.1", false, &[])
                },
            ],
        },
        ProviderSpec {
            id: "custom.cohere",
            adaptor: "cohere-v2-chat",
            path: "/v2",
            setup: "",
            auth: r#"{ method = "bearer-api-key-v1", values = { api_key = "cohere-key" } }"#,
            headers: "",
            models: vec![
                model(
                    "thinker",
                    "command-a-reasoning-08-2025",
                    true,
                    COHERE_REASONING,
                ),
                ModelSpec {
                    structured: false,
                    ..model("plain", "command-r", false, &[])
                },
            ],
        },
    ]
}

struct ManagedSpec {
    provider: &'static str,
    npm: &'static str,
    model: &'static str,
    output: u64,
    options: Vec<CatalogReasoningOption>,
    interleaved: Option<CatalogInterleaved>,
    authored: &'static str,
}

fn effort(values: &[&str]) -> CatalogReasoningOption {
    CatalogReasoningOption::Effort {
        values: values
            .iter()
            .map(|value| Some((*value).to_owned()))
            .collect(),
    }
}

fn managed_providers() -> Vec<ManagedSpec> {
    let managed = |provider, npm, model, options| ManagedSpec {
        provider,
        npm,
        model,
        output: OUTPUT_TOKENS,
        options,
        interleaved: None,
        authored: "",
    };
    vec![
        managed(
            "anthropic",
            "@ai-sdk/anthropic",
            "claude-opus-4-5",
            vec![
                effort(&["low", "high"]),
                CatalogReasoningOption::BudgetTokens {
                    min: Some(1024),
                    max: None,
                },
            ],
        ),
        managed(
            "deepseek",
            "@ai-sdk/openai-compatible",
            "deepseek-v4-pro",
            vec![CatalogReasoningOption::Toggle, effort(&["low", "max"])],
        ),
        managed(
            "alibaba",
            "@ai-sdk/openai-compatible",
            "qwen3.5-plus",
            vec![CatalogReasoningOption::Toggle],
        ),
        managed(
            "kimi-code-plan-cn",
            "@ai-sdk/openai-compatible",
            "k3",
            vec![CatalogReasoningOption::Toggle, effort(&["high"])],
        ),
        ManagedSpec {
            interleaved: Some(CatalogInterleaved::Reasoning),
            ..managed(
                "moonshotai",
                "@ai-sdk/openai-compatible",
                "kimi-k3",
                vec![effort(&["high"])],
            )
        },
        managed(
            "kimi-for-coding",
            "@ai-sdk/anthropic",
            "k3",
            vec![CatalogReasoningOption::Toggle, effort(&["high"])],
        ),
        ManagedSpec {
            authored: r#"[models."gpt-5.6"]
adaptor_options = { request_endpoint = "responses" }
compaction = "openai-responses-compact"
"#,
            ..managed(
                "openai",
                "@ai-sdk/openai",
                "gpt-5.6",
                vec![effort(&["low", "high"])],
            )
        },
    ]
}

fn catalog_provider(base: &str, spec: &ManagedSpec) -> CatalogProviderEntry {
    let provider_id = ProviderId::new(spec.provider).unwrap();
    let model_id = ProviderModelId::new(spec.model).unwrap();
    let record = CatalogModelRecord {
        id: model_id.clone(),
        name: spec.model.to_owned(),
        description: "test".to_owned(),
        family: None,
        attachment: false,
        reasoning: true,
        tool_call: true,
        structured_output: Some(true),
        temperature: Some(false),
        open_weights: false,
        status: CatalogModelStatus::Stable,
        release_date: "2026-01-01".to_owned(),
        last_updated: "2026-01-01".to_owned(),
        modalities: CatalogModalities {
            input: vec!["text".to_owned()],
            output: vec!["text".to_owned()],
        },
        limits: CatalogLimits {
            context: 200_000,
            input: None,
            output: spec.output,
        },
        shape: None,
        provider: None,
        reasoning_options: spec.options.clone(),
        cost: None,
        interleaved: spec.interleaved,
        canonical_provenance: None,
    };
    CatalogProviderEntry {
        id: provider_id.clone(),
        record: Some(CatalogProviderRecord {
            id: provider_id,
            name: spec.provider.to_owned(),
            environment: vec!["TEST_API_KEY".to_owned()],
            npm: spec.npm.to_owned(),
            api: Some(format!("{base}/v1")),
            shape: None,
            documentation_url: "https://example.test/docs".to_owned(),
            models: BTreeMap::from([(
                model_id.clone(),
                CatalogModelEntry {
                    id: model_id,
                    record: Some(record),
                    quarantine: None,
                },
            )]),
        }),
        quarantine: None,
    }
}

fn catalog(base: &str, specs: &[ManagedSpec]) -> Arc<CatalogSnapshot> {
    let now = Timestamp::now();
    Arc::new(CatalogSnapshot {
        revision: CatalogRevision::new(format!(
            "sha256:{:x}",
            Sha256::digest(b"wire-snapshot-catalog")
        ))
        .unwrap(),
        source: CatalogSource::Network,
        state: CatalogRuntimeState {
            availability: CatalogAvailability::Ready,
            age: CatalogAgeState::Current,
            last_error: None,
        },
        validated_at: now,
        last_checked_at: now,
        etag: None,
        providers: specs
            .iter()
            .map(|spec| {
                (
                    ProviderId::new(spec.provider).unwrap(),
                    catalog_provider(base, spec),
                )
            })
            .collect(),
        canonical_models: BTreeMap::new(),
        quarantine: Vec::new(),
    })
}

fn managed_definition(base: &str, spec: &ManagedSpec) -> ProviderDefinition {
    let base_url = if spec.npm == "@ai-sdk/openai" {
        format!("base_url = \"{base}/v1\"\n")
    } else {
        String::new()
    };
    let text = format!(
        "source = \"models_dev\"\napi_key = \"managed-key\"\n{base_url}{}",
        spec.authored
    );
    toml::from_str(&text).unwrap_or_else(|error| panic!("{}: {error}\n{text}", spec.provider))
}

fn cache_strategies(family: OvenAdapterFamily) -> Vec<(&'static str, CacheStrategyConfig)> {
    match family {
        OvenAdapterFamily::Anthropic | OvenAdapterFamily::AnthropicCompatible => vec![(
            "anthropic-cache",
            CacheStrategyConfig::Anthropic(AnthropicCacheStrategyConfig {
                system: Some(AnthropicCacheTtlConfig::OneHour),
                tools: Some(AnthropicCacheTtlConfig::OneHour),
                rolling: Some(AnthropicCacheTtlConfig::FiveMinutes),
            }),
        )],
        OvenAdapterFamily::AwsBedrockConverse => vec![(
            "bedrock-cache",
            CacheStrategyConfig::Bedrock(BedrockCacheStrategy {
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
            }),
        )],
        OvenAdapterFamily::GoogleGemini | OvenAdapterFamily::GoogleVertexGemini => vec![
            (
                "google-cache-explicit",
                CacheStrategyConfig::Google(GoogleCacheStrategyConfig {
                    mode: GoogleCacheMode::Explicit,
                    cached_content: Some("cachedContents/cookie".into()),
                }),
            ),
            (
                "google-cache-off",
                CacheStrategyConfig::Google(GoogleCacheStrategyConfig {
                    mode: GoogleCacheMode::Off,
                    cached_content: None,
                }),
            ),
        ],
        OvenAdapterFamily::OpenaiChat
        | OvenAdapterFamily::OpenaiResponses
        | OvenAdapterFamily::OpenaiCompatible
        | OvenAdapterFamily::AzureOpenaiChat
        | OvenAdapterFamily::AzureOpenaiResponses => vec![
            (
                "openai-cache",
                CacheStrategyConfig::OpenAi(OpenAiCacheStrategyConfig {
                    prompt_cache_key: Some("cookie-cache-key".into()),
                    prompt_cache_retention: Some(OpenAiPromptCacheRetention::InMemory),
                    mode: Some(OpenAiCacheMode::Explicit),
                    ttl: Some(OpenAiPromptCacheTtl::ThirtyMinutes),
                    system: true,
                    rolling: true,
                }),
            ),
            (
                "openai-cache-key-only",
                CacheStrategyConfig::OpenAi(OpenAiCacheStrategyConfig {
                    prompt_cache_key: Some("cookie-cache-key".into()),
                    prompt_cache_retention: Some(OpenAiPromptCacheRetention::TwentyFourHours),
                    mode: None,
                    ttl: None,
                    system: false,
                    rolling: false,
                }),
            ),
        ],
        OvenAdapterFamily::CohereV2Chat => Vec::new(),
    }
}

fn request() -> Request {
    let mut request = Request::new(vec![
        HistoryTurn::system(SystemMessage::new(vec![SystemPart::Text(TextPart::new(
            "stable system",
        ))])),
        HistoryTurn::user(UserMessage::new(vec![InputPart::Text(TextPart::new(
            "hello",
        ))])),
    ])
    .with_tools(vec![ToolDefinition::new(
        "inspect",
        "Inspect one path.",
        JsonSchema::new(json!({
            "type": "object",
            "properties": { "path": { "type": "string" } },
            "required": ["path"]
        }))
        .unwrap(),
    )])
    .with_header_context(HeaderContext::new("root-session").with_parent_session_id("parent"));
    request.inference.max_output_tokens = Some(OUTPUT_TOKENS);
    request
}

fn redact(family: OvenAdapterFamily, name: &str, value: &str) -> String {
    let volatile = name == "host"
        || family == OvenAdapterFamily::AwsBedrockConverse
            && matches!(name, "authorization" | "x-amz-date");
    if volatile {
        "<volatile>".into()
    } else {
        value.to_owned()
    }
}

fn wire_snapshot(family: OvenAdapterFamily, raw: &str) -> Value {
    let (head, body) = raw.split_once("\r\n\r\n").unwrap_or((raw, ""));
    let mut lines = head.lines();
    let request_line = lines.next().unwrap_or_default().to_owned();
    let headers = lines
        .map(|line| {
            let (name, value) = line.split_once(':').unwrap();
            let name = name.trim().to_ascii_lowercase();
            let value = redact(family, &name, value.trim());
            format!("{name}: {value}")
        })
        .collect::<Vec<_>>();
    let body = serde_json::from_str::<Value>(body).unwrap_or_else(|_| Value::String(body.into()));
    json!({ "request_line": request_line, "headers": headers, "body": body })
}

async fn record_runtime(
    manager: &ModelManager,
    receiver: &mut mpsc::UnboundedReceiver<String>,
    snapshots: &mut BTreeMap<String, Value>,
) {
    let runtime = manager.current();
    for (key, compiled) in runtime.models() {
        let variants =
            std::iter::once(None).chain(compiled.model.variant_order.iter().cloned().map(Some));
        for variant in variants {
            let selection = ModelSelection {
                model: key.clone(),
                variant: variant.clone(),
            };
            let resolved = runtime
                .resolve(&selection)
                .unwrap_or_else(|error| panic!("{key} {variant:?}: {error:?}"));
            let family = resolved.adapter_family();
            let variant_name = format!(
                "{key}#{}",
                variant
                    .as_ref()
                    .map_or("default", |variant| variant.as_str())
            );
            snapshots.insert(
                variant_name.clone(),
                json!({
                    "adapter_family": family,
                    "descriptor": resolved.model().descriptor(),
                }),
            );
            let strategies = std::iter::once(("no-cache", None)).chain(
                cache_strategies(family)
                    .into_iter()
                    .map(|(label, strategy)| (label, Some(strategy))),
            );
            for (label, strategy) in strategies {
                let prepared = match &strategy {
                    Some(strategy) => {
                        resolved.prepare_request_with_cache_strategy(request(), Some(strategy))
                    }
                    None => resolved.prepare_request(request()),
                };
                let provider_options = serde_json::to_value(&prepared.provider_options).unwrap();
                let outcome = match resolved
                    .model()
                    .stream(prepared, AbortSignal::default())
                    .await
                {
                    Ok(mut stream) => {
                        let mut error = None;
                        while let Some(part) = stream.stream.next().await {
                            if let Err(part) = part {
                                error = Some(part);
                            }
                        }
                        error.map(|error| error.to_string())
                    }
                    Err(error) => Some(error.to_string()),
                };
                let wire = match receiver.try_recv() {
                    Ok(raw) => wire_snapshot(family, &raw),
                    Err(_) => json!({ "not_sent": outcome }),
                };
                // Drain retries so the next case reads its own request.
                while receiver.try_recv().is_ok() {}
                snapshots.insert(
                    format!("{variant_name}+{label}"),
                    json!({ "provider_options": provider_options, "wire": wire }),
                );
            }
        }
    }
}

fn store(temporary: &TempDir) -> ProviderStore {
    fs::set_permissions(temporary.path(), fs::Permissions::from_mode(0o700)).unwrap();
    ProviderStore::open(temporary.path().join("providers")).unwrap()
}

#[tokio::test]
async fn adapter_wire_requests_match_golden_snapshots() {
    let (base, mut receiver) = capture_server().await;
    let mut snapshots = BTreeMap::new();

    let temporary = TempDir::new().unwrap();
    let custom = ModelManager::new(
        custom_providers()
            .iter()
            .map(|provider| {
                (
                    ProviderId::new(provider.id).unwrap(),
                    custom_definition(&base, provider),
                )
            })
            .collect(),
        catalog(&base, &[]),
        store(&temporary),
    )
    .unwrap();
    record_runtime(&custom, &mut receiver, &mut snapshots).await;

    let managed_specs = managed_providers();
    let temporary = TempDir::new().unwrap();
    let managed = ModelManager::new(
        managed_specs
            .iter()
            .map(|spec| {
                (
                    ProviderId::new(spec.provider).unwrap(),
                    managed_definition(&base, spec),
                )
            })
            .collect(),
        catalog(&base, &managed_specs),
        store(&temporary),
    )
    .unwrap();
    record_runtime(&managed, &mut receiver, &mut snapshots).await;

    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/adapter_wire_snapshots.json");
    let actual = serde_json::to_string_pretty(&snapshots).unwrap() + "\n";
    if std::env::var_os("UPDATE_WIRE_SNAPSHOTS").is_some() {
        fs::write(&path, &actual).unwrap();
        return;
    }
    let expected: BTreeMap<String, Value> =
        serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    let mismatched = snapshots
        .keys()
        .chain(expected.keys())
        .filter(|name| snapshots.get(*name) != expected.get(*name))
        .collect::<std::collections::BTreeSet<_>>();
    assert!(
        mismatched.is_empty(),
        "wire snapshots differ for {} case(s): {mismatched:#?}\nfirst actual: {}",
        mismatched.len(),
        mismatched
            .first()
            .and_then(|name| snapshots.get(*name))
            .map(|value| serde_json::to_string_pretty(value).unwrap())
            .unwrap_or_default()
    );
}
