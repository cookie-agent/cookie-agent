use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use cookie_agent_identity::AuthFieldName;
use oven_sdk::{
    AdapterId, CompactionCapability as OvenCompaction, LanguageModel, ModelCapabilities,
    ModelConfig, ModelDeclaration, ModelError, ModelId,
};
use oven_sdk_anthropic::{
    AnthropicCompatibleModel, AnthropicCompatibleSettings, AnthropicModel,
    AnthropicProtocolSettings, AnthropicRequestOptions, AnthropicSettings, AnthropicThinking,
    AnthropicThinkingSupport,
};
use oven_sdk_azure::{
    AzureApiRoute, AzureMaxTokensField, AzureOpenAiChatModel, AzureOpenAiChatOptions,
    AzureOpenAiChatSettings, AzureOpenAiCompletionsConfig, AzureOpenAiResponsesCompaction,
    AzureOpenAiResponsesModel, AzureOpenAiResponsesOptions, AzureOpenAiResponsesSettings,
    AzureOpenAiRevision, AzureReasoningField, AzureStructuredOutputSupport, AzureSystemMessageRole,
};
use oven_sdk_bedrock::{
    BedrockConverseSettings, BedrockEventStreamLimits, BedrockModel, BedrockReasoningWireFormat,
    BedrockRequestOptions, BedrockStructuredOutput,
};
use oven_sdk_cohere::{CohereModel, CohereRequestOptions, CohereSettings, CohereThinking};
use oven_sdk_google::{
    GoogleGenerateContentSettings, GoogleModel, GoogleRequestOptions, GoogleThinkingConfig,
    GoogleThinkingSettings, GoogleToolSettings,
};
use oven_sdk_google_vertex::{
    GoogleVertexMediaSettings, GoogleVertexModel, GoogleVertexRequestOptions, GoogleVertexResource,
    GoogleVertexSettings, GoogleVertexThinkingConfig, GoogleVertexThinkingMode,
    GoogleVertexToolSettings, google_vertex_native_context_scope,
};
use oven_sdk_openai::{
    CompatibleChatOptions, MaxTokensField, OpenAiChatModel, OpenAiChatOptions, OpenAiChatSettings,
    OpenAiCompatibleChatModel, OpenAiCompatibleChatSettings, OpenAiResponsesCompaction,
    OpenAiResponsesModel, OpenAiResponsesOptions, OpenAiResponsesSettings, ReasoningField,
    StructuredOutputSupport, SystemMessageRole,
};
use serde::Serialize;
use serde_json::{Map, Value, json};
use zeroize::Zeroize as _;

use crate::{
    ProviderOptions, ReasoningBehavior, ReasoningEffort,
    adapters::{
        OvenAdapterFamily,
        oven::{
            AuthConfig, CommonProvider, ModelBuildError, TimeoutsConfig,
            combined_routing_discriminator, header_routing_discriminator, namespace, wrong_auth,
        },
    },
    compiler::CompiledDynamicModel,
};

pub(crate) struct ExecutableCredentialMaterial {
    pub method: String,
    pub values: BTreeMap<AuthFieldName, String>,
}

pub(crate) struct ExecutableBehaviorInput<'a> {
    pub options: &'a ProviderOptions,
    pub reasoning: Option<&'a ReasoningBehavior>,
}

impl Drop for ExecutableCredentialMaterial {
    fn drop(&mut self) {
        for value in self.values.values_mut() {
            value.zeroize();
        }
    }
}

pub(crate) fn compile_executable(
    provider_id: &str,
    model: &CompiledDynamicModel,
    mut capabilities: ModelCapabilities,
    mut headers: BTreeMap<String, String>,
    credentials: &ExecutableCredentialMaterial,
    behavior: ExecutableBehaviorInput<'_>,
) -> Result<crate::ConstructedAdapter, ModelBuildError> {
    let endpoint = executable_endpoint(model)?;
    if model.auth.method == "no-auth-v1" {
        if let Some(organization) = &behavior.options.organization {
            headers.insert("openai-organization".into(), organization.clone());
        }
        if let Some(project) = &behavior.options.project {
            headers.insert("openai-project".into(), project.clone());
        }
    }
    let auth = executable_auth(model, credentials, behavior.options)?;
    let provider = CommonProvider::new(
        executable_provider_id(provider_id, model.adapter, model.custom),
        &endpoint,
        &headers,
    )?;
    capabilities.compaction = if native_compaction(model) {
        OvenCompaction::Native
    } else {
        OvenCompaction::Unsupported
    };
    let declaration = ModelDeclaration::new(
        ModelId::new(model.wire_model_id.as_str().to_owned()),
        capabilities,
    )?;
    let header_discriminator =
        (!headers.is_empty()).then(|| header_routing_discriminator(&headers));
    let (model, provider_options) = construct(
        model,
        &behavior,
        &provider,
        declaration,
        &auth,
        header_discriminator.as_deref(),
    )?;
    Ok(crate::ConstructedAdapter {
        model,
        provider_options,
    })
}

pub(crate) fn executable_provider_id(
    provider_id: &str,
    family: OvenAdapterFamily,
    custom: bool,
) -> &str {
    // Custom Responses providers keep their full authored ID as replay
    // identity so separate gateways never share native replay history with
    // each other or with the managed `openai` family identity.
    if custom
        && matches!(
            family,
            OvenAdapterFamily::OpenaiChat | OvenAdapterFamily::OpenaiResponses
        )
        || family == OvenAdapterFamily::OpenaiResponses && provider_id != "openai"
    {
        return provider_id;
    }
    match family {
        OvenAdapterFamily::Anthropic => "anthropic",
        OvenAdapterFamily::AnthropicCompatible => provider_id,
        OvenAdapterFamily::OpenaiChat | OvenAdapterFamily::OpenaiResponses => "openai",
        OvenAdapterFamily::GoogleGemini => "google",
        OvenAdapterFamily::GoogleVertexGemini => "google-vertex",
        OvenAdapterFamily::AwsBedrockConverse => "amazon.bedrock",
        OvenAdapterFamily::AzureOpenaiChat | OvenAdapterFamily::AzureOpenaiResponses => {
            "azure.openai"
        }
        OvenAdapterFamily::CohereV2Chat => "cohere",
        OvenAdapterFamily::OpenaiCompatible => provider_id,
    }
}

fn credential<'a>(
    material: &'a ExecutableCredentialMaterial,
    name: &str,
) -> Result<&'a str, ModelBuildError> {
    material
        .values
        .iter()
        .find(|(field, _)| field.as_str() == name)
        .map(|(_, value)| value.as_str())
        .ok_or_else(|| wrong_auth("dynamic", "complete credential material"))
}

fn executable_auth(
    model: &CompiledDynamicModel,
    material: &ExecutableCredentialMaterial,
    options: &ProviderOptions,
) -> Result<AuthConfig, ModelBuildError> {
    if material.method != model.auth.method {
        return Err(wrong_auth("dynamic", "compiled auth method"));
    }
    Ok(match material.method.as_str() {
        "no-auth-v1" => AuthConfig::None,
        "bearer-api-key-v1" => {
            let value = credential(material, "api_key")?.to_owned();
            match model.adapter {
                OvenAdapterFamily::OpenaiResponses
                    if model.adapter_id.starts_with("oven.openai-compatible.") =>
                {
                    AuthConfig::Bearer { token: value }
                }
                OvenAdapterFamily::OpenaiChat | OvenAdapterFamily::OpenaiResponses => {
                    AuthConfig::Openai {
                        api_key: value,
                        organization: options.organization.clone(),
                        project: options.project.clone(),
                    }
                }
                _ => AuthConfig::Bearer { token: value },
            }
        }
        "api-key-header-v1" => AuthConfig::HeaderApiKey {
            name: model
                .auth
                .safe_parameters
                .get("header_name")
                .cloned()
                .ok_or_else(|| wrong_auth("dynamic", "header_name parameter"))?,
            value: credential(material, "api_key")?.to_owned(),
        },
        "anthropic-api-key-v1" | "google-api-key-header-v1" | "azure-api-key-v1" => {
            AuthConfig::ApiKey {
                value: credential(material, "api_key")?.to_owned(),
            }
        }
        "oauth-access-token-v1" => AuthConfig::AccessToken {
            token: credential(material, "access_token")?.to_owned(),
        },
        "aws-sigv4-credentials-v1" => AuthConfig::AwsStatic {
            access_key_id: credential(material, "access_key_id")?.to_owned(),
            secret_access_key: credential(material, "secret_access_key")?.to_owned(),
            session_token: material
                .values
                .iter()
                .find(|(field, _)| field.as_str() == "session_token")
                .map(|(_, value)| value.clone()),
        },
        _ => return Err(wrong_auth("dynamic", "Registry-1 auth method")),
    })
}

/// OpenAI-protocol Responses served by a compatible gateway or without
/// credentials: a caller-named adapter identity and no native compaction.
fn compatible_responses(model: &CompiledDynamicModel) -> bool {
    model.adapter == OvenAdapterFamily::OpenaiResponses
        && (model.adapter_id.starts_with("oven.openai-compatible.")
            || model.auth.method == "no-auth-v1")
}

fn native_compaction(model: &CompiledDynamicModel) -> bool {
    model.capabilities.compaction == crate::CompactionCapability::Native
        && match model.adapter {
            OvenAdapterFamily::OpenaiResponses => !compatible_responses(model),
            OvenAdapterFamily::AzureOpenaiResponses => true,
            _ => false,
        }
}

/// Routing label for a caller-selected API-key header, so replay scopes of
/// gateways keyed by different headers never collide.
fn api_key_header_route(model: &CompiledDynamicModel) -> Option<String> {
    (model.auth.method == "api-key-header-v1").then(|| {
        format!(
            "header:{}",
            model
                .auth
                .safe_parameters
                .get("header_name")
                .map_or("api-key", String::as_str)
        )
    })
}

type Constructed = (Arc<dyn LanguageModel>, BTreeMap<String, Value>);

/// Builds the family's Oven model and its per-model request options.
fn construct(
    model: &CompiledDynamicModel,
    behavior: &ExecutableBehaviorInput<'_>,
    provider: &CommonProvider,
    declaration: ModelDeclaration,
    auth: &AuthConfig,
    header_discriminator: Option<&str>,
) -> Result<Constructed, ModelBuildError> {
    let reasoning = behavior.reasoning;
    let capabilities = &model.capabilities;
    let timeouts = TimeoutsConfig::default();
    let structured = if capabilities.structured_output {
        StructuredOutputSupport::JsonSchema
    } else {
        StructuredOutputSupport::Unsupported
    };
    Ok(match model.adapter {
        OvenAdapterFamily::Anthropic => (
            Arc::new(AnthropicModel::new(ModelConfig::new(
                provider.with_auth(auth.anthropic()?),
                declaration,
                AnthropicSettings {
                    client: anthropic_client(timeouts)?,
                    timeouts: timeouts.anthropic(),
                    protocol: anthropic_protocol(model, false),
                    native_context_discriminator: None,
                },
            ))?),
            namespace("anthropic", anthropic_options(model, behavior))?,
        ),
        OvenAdapterFamily::AnthropicCompatible => (
            Arc::new(AnthropicCompatibleModel::new(ModelConfig::new(
                provider.with_auth(auth.anthropic_compatible()?),
                declaration,
                AnthropicCompatibleSettings {
                    adapter_id: AdapterId::new(model.adapter_id.clone()),
                    client: anthropic_client(timeouts)?,
                    timeouts: timeouts.anthropic(),
                    protocol: anthropic_protocol(model, !capabilities.temperature),
                    native_context_discriminator: None,
                },
            ))?),
            namespace("anthropic", anthropic_options(model, behavior))?,
        ),
        OvenAdapterFamily::OpenaiChat => {
            let settings = OpenAiChatSettings {
                system_message_role: SystemMessageRole::Developer,
                max_tokens_field: if capabilities.reasoning {
                    MaxTokensField::MaxCompletionTokens
                } else {
                    MaxTokensField::MaxTokens
                },
                stream_usage: false,
                structured_output: structured,
                reasoning_field: if capabilities.reasoning {
                    ReasoningField::ReasoningContent
                } else {
                    ReasoningField::None
                },
                routing_discriminator: combined_routing_discriminator(None, header_discriminator),
                client: timeouts.shared_client(),
                timeouts: timeouts.openai(),
            };
            let language_model: Arc<dyn LanguageModel> = if matches!(auth, AuthConfig::None) {
                Arc::new(OpenAiChatModel::new_no_auth(ModelConfig::new(
                    provider.with_auth(()),
                    declaration,
                    settings,
                ))?)
            } else {
                Arc::new(OpenAiChatModel::new(ModelConfig::new(
                    provider.with_auth(auth.openai()?),
                    declaration,
                    settings,
                ))?)
            };
            let options = OpenAiChatOptions {
                reasoning_effort: reasoning.and_then(reasoning_effort),
                ..OpenAiChatOptions::default()
            };
            (
                language_model,
                namespace("openai", json!({ "chat": options }))?,
            )
        }
        OvenAdapterFamily::OpenaiResponses if compatible_responses(model) => (
            Arc::new(OpenAiResponsesModel::new_compatible(
                ModelConfig::new(
                    provider.with_auth(auth.openai_compatible()?),
                    declaration,
                    OpenAiResponsesSettings {
                        routing_discriminator: combined_routing_discriminator(
                            api_key_header_route(model).as_deref(),
                            header_discriminator,
                        ),
                        compaction: OpenAiResponsesCompaction::Unsupported,
                        client: timeouts.shared_client(),
                        timeouts: timeouts.openai(),
                    },
                ),
                AdapterId::new(model.adapter_id.clone()),
            )?),
            namespace(
                "openai",
                json!({ "responses": compatible_responses_options(model) }),
            )?,
        ),
        OvenAdapterFamily::OpenaiResponses => (
            Arc::new(OpenAiResponsesModel::new(ModelConfig::new(
                provider.with_auth(auth.openai()?),
                declaration,
                OpenAiResponsesSettings {
                    routing_discriminator: combined_routing_discriminator(
                        None,
                        header_discriminator,
                    ),
                    compaction: if native_compaction(model) {
                        OpenAiResponsesCompaction::V1
                    } else {
                        OpenAiResponsesCompaction::Unsupported
                    },
                    client: timeouts.shared_client(),
                    timeouts: timeouts.openai(),
                },
            ))?),
            namespace(
                "openai",
                json!({ "responses": openai_responses_options(model) }),
            )?,
        ),
        OvenAdapterFamily::OpenaiCompatible => (
            Arc::new(OpenAiCompatibleChatModel::new(ModelConfig::new(
                provider.with_auth(auth.openai_compatible()?),
                declaration,
                OpenAiCompatibleChatSettings {
                    adapter_id: AdapterId::new(model.adapter_id.clone()),
                    system_message_role: SystemMessageRole::System,
                    max_tokens_field: MaxTokensField::MaxTokens,
                    stream_usage: false,
                    structured_output: structured,
                    reasoning_field: match (capabilities.reasoning, model.reasoning_field.as_str())
                    {
                        (false, _) | (true, "none") => ReasoningField::None,
                        (true, "reasoning") => ReasoningField::Reasoning,
                        (true, _) => ReasoningField::ReasoningContent,
                    },
                    query: Vec::new(),
                    request_id_headers: vec!["x-request-id".into()],
                    strict_sse_content_type: false,
                    routing_discriminator: combined_routing_discriminator(
                        api_key_header_route(model).as_deref(),
                        header_discriminator,
                    ),
                    client: timeouts.shared_client(),
                    timeouts: timeouts.openai(),
                },
            ))?),
            namespace(
                "openai_compatible",
                CompatibleChatOptions {
                    extra_body: compatible_thinking_body(model.thinking_toggle, reasoning),
                },
            )?,
        ),
        OvenAdapterFamily::GoogleGemini => (
            Arc::new(GoogleModel::new(ModelConfig::new(
                provider.with_auth(auth.google()?),
                declaration,
                GoogleGenerateContentSettings {
                    model_resource: format!("models/{}", model.wire_model_id.as_str()),
                    timeouts: timeouts.google(),
                    thinking: google_thinking(reasoning),
                    tools: GoogleToolSettings {
                        strict_functions: capabilities.structured_output,
                        mixed_client_and_provider_tools: false,
                        current_turn_signature_sentinel: capabilities.native_replay
                            != crate::ReplayCapability::Unsupported,
                    },
                },
            ))?),
            namespace(
                "google",
                GoogleRequestOptions {
                    thinking_config: google_thinking_config(reasoning).map(
                        |(thinking_budget, thinking_level, include_thoughts)| {
                            GoogleThinkingConfig {
                                thinking_budget,
                                thinking_level,
                                include_thoughts,
                            }
                        },
                    ),
                    ..GoogleRequestOptions::default()
                },
            )?,
        ),
        OvenAdapterFamily::GoogleVertexGemini => {
            let project = setup(model, "project")?;
            let location = setup(model, "location")?;
            let provider = provider.with_auth(auth.vertex()?);
            let resource = GoogleVertexResource::PublisherModel {
                publisher: "google".into(),
                model: model.wire_model_id.as_str().to_owned(),
            };
            let native_context_scope = google_vertex_native_context_scope(
                provider.id.clone(),
                declaration.id.clone(),
                &provider.api,
                project,
                location,
                &resource,
            )?;
            let settings = GoogleVertexSettings {
                project: project.to_owned(),
                location: location.to_owned(),
                resource,
                thinking: match reasoning {
                    Some(ReasoningBehavior::Effort { .. }) => GoogleVertexThinkingMode::Level,
                    Some(_) => GoogleVertexThinkingMode::Budget,
                    None => GoogleVertexThinkingMode::Unsupported,
                },
                tools: GoogleVertexToolSettings {
                    provider_tools: false,
                    mixed_client_and_provider_tools: false,
                    strict_functions: capabilities.structured_output,
                },
                stream_function_call_arguments: false,
                media: GoogleVertexMediaSettings {
                    max_images: 20,
                    max_https_images: 20,
                    max_documents: 5,
                    max_audio: 5,
                    max_videos: 5,
                    max_https_videos: 5,
                    max_inline_image_bytes: 7 * 1024 * 1024,
                    max_inline_pdf_bytes: 32 * 1024 * 1024,
                    max_inline_text_bytes: 1024 * 1024,
                    url_schemes: vec!["https".into()],
                },
                native_context_scope,
                client: timeouts.shared_client(),
                timeouts: timeouts.vertex(),
            };
            (
                Arc::new(GoogleVertexModel::new(ModelConfig::new(
                    provider,
                    declaration,
                    settings,
                ))?),
                namespace(
                    "google_vertex",
                    GoogleVertexRequestOptions {
                        thinking_config: google_thinking_config(reasoning).map(
                            |(thinking_budget, thinking_level, include_thoughts)| {
                                GoogleVertexThinkingConfig {
                                    thinking_budget,
                                    thinking_level,
                                    include_thoughts,
                                }
                            },
                        ),
                        ..GoogleVertexRequestOptions::default()
                    },
                )?,
            )
        }
        OvenAdapterFamily::AwsBedrockConverse => {
            let claude = bedrock_anthropic_thinking(model.adapter, model.wire_model_id.as_str());
            (
                Arc::new(BedrockModel::new(ModelConfig::new(
                    provider.with_auth(auth.bedrock()?),
                    declaration,
                    BedrockConverseSettings {
                        region: setup(model, "region")?.to_owned(),
                        reasoning_wire_format: match (capabilities.reasoning, claude) {
                            (false, _) => BedrockReasoningWireFormat::Unsupported,
                            (true, true) => BedrockReasoningWireFormat::AnthropicThinking,
                            (true, false) => BedrockReasoningWireFormat::BedrockReasoningConfig,
                        },
                        signed_reasoning: capabilities.reasoning
                            && claude
                            && capabilities.native_replay == crate::ReplayCapability::Required,
                        structured_output: if capabilities.structured_output {
                            BedrockStructuredOutput::JsonSchema
                        } else {
                            BedrockStructuredOutput::Unsupported
                        },
                        event_stream: BedrockEventStreamLimits::new(16 * 1024 * 1024),
                        timeouts: timeouts.bedrock(),
                        client: timeouts.shared_client(),
                    },
                ))?),
                namespace(
                    "bedrock",
                    if claude {
                        bedrock_anthropic_options(model, reasoning)
                    } else {
                        bedrock_options(reasoning)
                    },
                )?,
            )
        }
        OvenAdapterFamily::AzureOpenaiChat => (
            Arc::new(AzureOpenAiChatModel::new(ModelConfig::new(
                provider.with_auth(auth.azure()?),
                declaration,
                AzureOpenAiChatSettings {
                    route: AzureApiRoute::V1,
                    revision: azure_revision(model),
                    timeouts: timeouts.azure(),
                    completions: AzureOpenAiCompletionsConfig {
                        system_role: AzureSystemMessageRole::Developer,
                        max_tokens_field: if capabilities.reasoning {
                            AzureMaxTokensField::MaxCompletionTokens
                        } else {
                            AzureMaxTokensField::MaxTokens
                        },
                        stream_usage: false,
                        structured_output: if capabilities.structured_output {
                            AzureStructuredOutputSupport::JsonSchema
                        } else {
                            AzureStructuredOutputSupport::Unsupported
                        },
                        reasoning_field: if capabilities.reasoning {
                            AzureReasoningField::ReasoningContent
                        } else {
                            AzureReasoningField::None
                        },
                        omit_reasoning_sampling: capabilities.reasoning,
                    },
                },
            ))?),
            namespace(
                "azure_openai",
                json!({ "chat": azure_chat_options(reasoning) }),
            )?,
        ),
        OvenAdapterFamily::AzureOpenaiResponses => (
            Arc::new(AzureOpenAiResponsesModel::new(ModelConfig::new(
                provider.with_auth(auth.azure()?),
                declaration,
                AzureOpenAiResponsesSettings {
                    route: AzureApiRoute::V1,
                    revision: azure_revision(model),
                    timeouts: timeouts.azure(),
                    compaction: if native_compaction(model) {
                        AzureOpenAiResponsesCompaction::V1 {
                            routing_discriminator: model.adapter_id.clone(),
                        }
                    } else {
                        AzureOpenAiResponsesCompaction::Unsupported
                    },
                },
            ))?),
            namespace(
                "azure_openai",
                json!({ "responses": AzureOpenAiResponsesOptions::default() }),
            )?,
        ),
        OvenAdapterFamily::CohereV2Chat => (
            Arc::new(CohereModel::new(ModelConfig::new(
                provider.with_auth(auth.cohere()?),
                declaration,
                CohereSettings {
                    timeouts: timeouts.cohere(),
                    strict_tools: capabilities.structured_output,
                    safety_mode: None,
                    thinking: cohere_thinking(reasoning),
                    reasoning_effort: BTreeMap::new(),
                    top_k: None,
                    seed: None,
                    frequency_penalty: None,
                    presence_penalty: None,
                    stop_sequences: Vec::new(),
                    priority: None,
                },
            ))?),
            namespace("cohere", CohereRequestOptions::default())?,
        ),
    })
}

fn setup<'a>(model: &'a CompiledDynamicModel, name: &str) -> Result<&'a str, ModelBuildError> {
    model
        .setup
        .as_ref()
        .and_then(|setup| setup.values.get(name))
        .map(String::as_str)
        .ok_or_else(|| wrong_auth("dynamic", "complete setup material"))
}

fn azure_revision(model: &CompiledDynamicModel) -> Option<AzureOpenAiRevision> {
    Some(AzureOpenAiRevision {
        model: setup(model, "model").ok()?.to_owned(),
        version: setup(model, "version").ok()?.to_owned(),
        deployment_type: setup(model, "deployment_type").ok()?.to_owned(),
    })
}

fn executable_endpoint(model: &CompiledDynamicModel) -> Result<String, ModelBuildError> {
    let endpoint = model
        .endpoint
        .clone()
        .ok_or_else(|| wrong_auth("dynamic", "compiled endpoint"))?;
    if model.adapter == OvenAdapterFamily::GoogleVertexGemini {
        let marker = "/v1/projects/";
        Ok(endpoint.find(marker).map_or(endpoint.clone(), |index| {
            format!("{}/v1", &endpoint[..index])
        }))
    } else if model.adapter == OvenAdapterFamily::CohereV2Chat && !endpoint.ends_with("/v2/chat") {
        Ok(format!("{}/chat", endpoint.trim_end_matches('/')))
    } else {
        Ok(endpoint)
    }
}

fn reasoning_effort(reasoning: &ReasoningBehavior) -> Option<String> {
    match reasoning {
        ReasoningBehavior::Effort { value } => Some(
            match value {
                ReasoningEffort::None => "none",
                ReasoningEffort::Minimal => "minimal",
                ReasoningEffort::Low => "low",
                ReasoningEffort::Medium => "medium",
                ReasoningEffort::High => "high",
                ReasoningEffort::Xhigh => "xhigh",
                ReasoningEffort::Max => "max",
                ReasoningEffort::Default => "default",
            }
            .to_owned(),
        ),
        ReasoningBehavior::Toggle { .. } | ReasoningBehavior::BudgetTokens { .. } => None,
    }
}

fn anthropic_client(timeouts: TimeoutsConfig) -> Result<reqwest::Client, ModelBuildError> {
    timeouts
        .shared_client()
        .ok_or_else(|| ModelError::transport("could not construct Anthropic HTTP client").into())
}

fn anthropic_protocol(
    model: &CompiledDynamicModel,
    reject_non_default_sampling: bool,
) -> AnthropicProtocolSettings {
    let reasoning = model.capabilities.reasoning;
    AnthropicProtocolSettings {
        thinking: if reasoning {
            AnthropicThinkingSupport::Both
        } else {
            AnthropicThinkingSupport::None
        },
        thinking_default_active: false,
        thinking_disable_allowed: reasoning,
        thinking_disable_forbidden_efforts: BTreeSet::new(),
        effort: reasoning,
        assistant_prefill: false,
        reject_non_default_sampling,
    }
}

/// Anthropic Messages reasoning controls.
///
/// Thinking is requested with `display: "summarized"`: Claude Opus 4.7 and
/// later default to `"omitted"`, which streams empty reasoning, and earlier
/// models already default to summaries. Effort and toggle-on variants think
/// adaptively, because Claude Opus 4.6+ runs without thinking when `thinking`
/// is omitted; models before Claude 4.6 accept only manual budgets, so they
/// get an explicit budget instead. Toggle-off sends `{"type": "disabled"}`,
/// because Claude Sonnet 5 and Opus 5 think by default.
fn anthropic_options(
    model: &CompiledDynamicModel,
    behavior: &ExecutableBehaviorInput<'_>,
) -> AnthropicRequestOptions {
    let reasoning = behavior.reasoning;
    let enabled = |budget_tokens| AnthropicThinking::Enabled {
        budget_tokens,
        display: Some("summarized".into()),
    };
    let thinking = match reasoning {
        None => None,
        Some(ReasoningBehavior::Toggle { enabled: false }) => Some(AnthropicThinking::Disabled),
        Some(ReasoningBehavior::BudgetTokens { value }) if *value > 0 => {
            Some(enabled(value.unsigned_abs()))
        }
        Some(_) if claude_extended_thinking_only(model.wire_model_id.as_str()) => {
            Some(enabled(extended_thinking_budget(model)))
        }
        Some(_) => Some(AnthropicThinking::Adaptive {
            display: Some("summarized".into()),
        }),
    };
    AnthropicRequestOptions {
        thinking,
        effort: reasoning.and_then(reasoning_effort),
        betas: behavior.options.beta.clone(),
        ..AnthropicRequestOptions::default()
    }
}

/// Claude on Bedrock Converse takes Anthropic `thinking` and
/// `output_config.effort` through `additionalModelRequestFields`
/// (docs.aws.amazon.com/bedrock/latest/userguide/claude-messages-adaptive-thinking.html);
/// Bedrock's own `reasoningConfig` belongs to other model families.
pub(crate) fn bedrock_anthropic_thinking(adapter: OvenAdapterFamily, wire_model_id: &str) -> bool {
    adapter == OvenAdapterFamily::AwsBedrockConverse
        && wire_model_id.to_ascii_lowercase().contains("claude")
}

/// Bedrock Converse encoding of [`anthropic_options`]: adaptive thinking with
/// summarized display for effort and toggle-on variants (manual budgets on
/// models before Claude 4.6) and `disabled` for toggle-off. The effort level
/// travels as the request's normalized reasoning effort, which Oven encodes as
/// `output_config.effort`. Oven rejects a display on manual budgets.
fn bedrock_anthropic_options(
    model: &CompiledDynamicModel,
    reasoning: Option<&ReasoningBehavior>,
) -> BedrockRequestOptions {
    let enabled = |budget_tokens| BedrockRequestOptions {
        reasoning_type: Some("enabled".into()),
        reasoning_budget_tokens: Some(budget_tokens),
        ..BedrockRequestOptions::default()
    };
    match reasoning {
        None => BedrockRequestOptions::default(),
        Some(ReasoningBehavior::Toggle { enabled: false }) => BedrockRequestOptions {
            reasoning_type: Some("disabled".into()),
            ..BedrockRequestOptions::default()
        },
        Some(ReasoningBehavior::BudgetTokens { value }) if *value > 0 => {
            enabled(value.unsigned_abs())
        }
        Some(_) if claude_extended_thinking_only(model.wire_model_id.as_str()) => {
            enabled(extended_thinking_budget(model))
        }
        Some(_) => BedrockRequestOptions {
            reasoning_type: Some("adaptive".into()),
            reasoning_display: Some("summarized".into()),
            ..BedrockRequestOptions::default()
        },
    }
}

/// Claude models released before Claude 4.6 reject adaptive thinking and
/// accept only `{"type": "enabled", "budget_tokens": N}`.
fn claude_extended_thinking_only(wire_model_id: &str) -> bool {
    let id = wire_model_id.to_ascii_lowercase();
    id.contains("claude-3")
        || [
            "opus-4-5",
            "opus-4.5",
            "sonnet-4-5",
            "sonnet-4.5",
            "haiku-4-5",
            "haiku-4.5",
            "opus-4-1",
            "opus-4.1",
            "opus-4-2",
            "sonnet-4-2",
        ]
        .iter()
        .any(|marker| id.contains(marker))
}

/// Manual thinking budget for effort and toggle variants on models that
/// accept only budgets: half the output room left after the minimum visible
/// output, capped at 16,000 tokens and floored at the 1,024 minimum.
fn extended_thinking_budget(model: &CompiledDynamicModel) -> u64 {
    let room = model
        .capabilities
        .output_tokens
        .saturating_sub(super::variants::MIN_VISIBLE_OUTPUT_TOKENS);
    (room / 2).clamp(1024, 16_000)
}

/// Documented OpenAI-compatible Chat request fields that turn thinking on or
/// off for one managed models.dev provider. Providers without a confirmed
/// switch get no toggle variants.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CompatibleThinkingToggle {
    /// `thinking: {"type": "enabled" | "disabled"}` (DeepSeek, Z.ai/Zhipu GLM,
    /// Volcengine Ark).
    ThinkingType,
    /// `enable_thinking: true | false` (Alibaba Cloud Model Studio).
    EnableThinking,
    /// `reasoning_effort: "none"` turns thinking off (Kimi Code); generated
    /// `off` variants become that effort level and there is no `on`.
    ReasoningEffortNone,
}

pub(crate) fn compatible_thinking_toggle(provider_id: &str) -> Option<CompatibleThinkingToggle> {
    match provider_id {
        // api-docs.deepseek.com/api/create-chat-completion; docs.z.ai and
        // docs.bigmodel.cn guides/capabilities/thinking; volcengine.com/docs/82379/1494384.
        "deepseek"
        | "zai"
        | "zai-coding-plan"
        | "zhipuai"
        | "zhipuai-coding-plan"
        | "volcengine" => Some(CompatibleThinkingToggle::ThinkingType),
        // alibabacloud.com/help/en/model-studio/qwen-api-via-openai-chat-completions.
        "alibaba" | "alibaba-cn" => Some(CompatibleThinkingToggle::EnableThinking),
        // kimi.com/code/docs/en/kimi-code/models: `none` maps to disabled thinking.
        "kimi-for-coding" | "kimi-code-plan-cn" | "kimi-code-plan-global" => {
            Some(CompatibleThinkingToggle::ReasoningEffortNone)
        }
        _ => None,
    }
}

fn compatible_thinking_body(
    toggle: Option<CompatibleThinkingToggle>,
    reasoning: Option<&ReasoningBehavior>,
) -> Map<String, Value> {
    let Some(ReasoningBehavior::Toggle { enabled }) = reasoning else {
        return Map::new();
    };
    let field = match toggle {
        Some(CompatibleThinkingToggle::ThinkingType) => (
            "thinking",
            json!({ "type": if *enabled { "enabled" } else { "disabled" } }),
        ),
        Some(CompatibleThinkingToggle::EnableThinking) => ("enable_thinking", json!(enabled)),
        Some(CompatibleThinkingToggle::ReasoningEffortNone) | None => return Map::new(),
    };
    Map::from_iter([(field.0.to_owned(), field.1)])
}

/// Responses options for compatible gateways: parallel tool calls follow the
/// capability.
fn compatible_responses_options(model: &CompiledDynamicModel) -> OpenAiResponsesOptions {
    OpenAiResponsesOptions {
        parallel_tool_calls: Some(model.capabilities.parallel_tool_calls),
        ..OpenAiResponsesOptions::default()
    }
}

/// Official Responses options: as for compatible gateways, and reasoning
/// models request automatic summaries.
fn openai_responses_options(model: &CompiledDynamicModel) -> OpenAiResponsesOptions {
    OpenAiResponsesOptions {
        reasoning_summary: model.capabilities.reasoning.then(|| "auto".into()),
        ..compatible_responses_options(model)
    }
}

fn azure_chat_options(reasoning: Option<&ReasoningBehavior>) -> AzureOpenAiChatOptions {
    AzureOpenAiChatOptions {
        reasoning_effort: reasoning.and_then(reasoning_effort),
        ..AzureOpenAiChatOptions::default()
    }
}

fn google_thinking(reasoning: Option<&ReasoningBehavior>) -> GoogleThinkingSettings {
    match reasoning {
        Some(effort @ ReasoningBehavior::Effort { .. }) => {
            let level = reasoning_effort(effort).unwrap_or_default();
            GoogleThinkingSettings::Level {
                effort_levels: BTreeMap::from([(level.clone(), level)]),
            }
        }
        Some(_) => GoogleThinkingSettings::Budget {
            effort_budgets: BTreeMap::new(),
        },
        None => GoogleThinkingSettings::Unsupported,
    }
}

/// Gemini `thinkingConfig` as `(thinking_budget, thinking_level,
/// include_thoughts)`, shared by the Gemini API and Vertex encodings.
fn google_thinking_config(
    reasoning: Option<&ReasoningBehavior>,
) -> Option<(Option<i64>, Option<String>, Option<bool>)> {
    match reasoning? {
        ReasoningBehavior::BudgetTokens { value } => Some((Some(*value), None, Some(true))),
        ReasoningBehavior::Toggle { enabled } => {
            Some((Some(if *enabled { -1 } else { 0 }), None, Some(*enabled)))
        }
        effort @ ReasoningBehavior::Effort { .. } => {
            Some((None, reasoning_effort(effort), Some(true)))
        }
    }
}

fn bedrock_options(reasoning: Option<&ReasoningBehavior>) -> BedrockRequestOptions {
    match reasoning {
        Some(ReasoningBehavior::Toggle { enabled }) => BedrockRequestOptions {
            reasoning_type: Some(if *enabled { "enabled" } else { "disabled" }.into()),
            ..BedrockRequestOptions::default()
        },
        Some(ReasoningBehavior::BudgetTokens { value }) if *value > 0 => BedrockRequestOptions {
            reasoning_type: Some("enabled".into()),
            reasoning_budget_tokens: Some(value.unsigned_abs()),
            ..BedrockRequestOptions::default()
        },
        // Effort reaches Bedrock as the request's normalized reasoning effort;
        // Oven rejects a second copy in the Bedrock options.
        _ => BedrockRequestOptions::default(),
    }
}

fn cohere_thinking(reasoning: Option<&ReasoningBehavior>) -> Option<CohereThinking> {
    match reasoning? {
        ReasoningBehavior::Toggle { enabled } => Some(CohereThinking {
            enabled: *enabled,
            token_budget: None,
        }),
        ReasoningBehavior::BudgetTokens { value } => Some(CohereThinking {
            enabled: true,
            token_budget: (*value > 0).then(|| value.unsigned_abs()),
        }),
        ReasoningBehavior::Effort { .. } => None,
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, sync::Arc};

    use cookie_agent_identity::{CatalogRevision, ProviderId};
    use jiff::Timestamp;
    use sha2::{Digest as _, Sha256};
    use tempfile::TempDir;

    use crate::{
        ProviderDefinition,
        catalog::{
            CatalogAgeState, CatalogAvailability, CatalogRuntimeState, CatalogSnapshot,
            CatalogSource,
        },
        manager::ModelManager,
        manifests::{build_manifest, frozen_binding},
        provider_store::ProviderStore,
    };

    fn revision(label: &str) -> CatalogRevision {
        CatalogRevision::new(format!("sha256:{:x}", Sha256::digest(label.as_bytes())))
            .expect("catalog revision")
    }

    fn empty_catalog() -> Arc<CatalogSnapshot> {
        let now = Timestamp::now();
        Arc::new(CatalogSnapshot {
            revision: revision("custom-responses-runtime"),
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

    #[test]
    fn custom_responses_identity_survives_manifest_and_resolution() {
        // Custom Responses providers keep their full authored ID as replay
        // identity: prefixed, bare, and even a catalog-colliding bare ID.
        for id in ["custom.gateway", "gateway", "openai"] {
            custom_responses_identity_survives_manifest_and_resolution_for(id);
        }
    }

    fn custom_responses_identity_survives_manifest_and_resolution_for(id: &str) {
        let temporary = TempDir::new().expect("temporary directory");
        let provider_id = ProviderId::new(id).expect("provider ID");
        let definition = toml::from_str::<ProviderDefinition>(
            r#"source = "custom"
endpoint = "http://127.0.0.1:9/v1"
adaptor = "openai-responses"
auth = { method = "bearer-api-key-v1", values = { api_key = "test-key" } }

[models.test]
display_name = "Test Responses"
capabilities = { input = ["text"], output = ["text"], context_tokens = 32768, output_tokens = 4096, tool_calling = true, parallel_tool_calls = true, structured_output = true, reasoning = true, temperature = false, top_p = false, seed = false, native_replay = "optional", media = {} }
"#,
        )
        .expect("custom Responses provider");
        let authored = BTreeMap::from([(provider_id.clone(), definition)]);
        let provider_store =
            ProviderStore::open(temporary.path().join("providers")).expect("provider store");
        let manager =
            ModelManager::new(authored, empty_catalog(), provider_store).expect("model manager");
        let runtime = manager.current();

        let manifest = build_manifest(runtime.manifest_payload().expect("manifest payload"))
            .expect("build manifest");
        let blueprint = manifest
            .payload
            .blueprints
            .first()
            .expect("model blueprint");
        let binding = frozen_binding(
            manifest.revision.clone(),
            blueprint,
            blueprint.selection.clone(),
        )
        .expect("frozen binding");
        assert_eq!(
            binding.descriptor.identity.provider_id.as_str(),
            provider_id.as_str()
        );
        let resolved = runtime
            .resolve(&binding.selection)
            .expect("live executable");

        assert_eq!(
            resolved.model().descriptor().identity.provider_id.as_str(),
            provider_id.as_str()
        );
        assert_eq!(
            resolved.model().descriptor().identity.model_id.as_str(),
            "test"
        );
    }

    fn openai_responses_options(reasoning: bool) -> oven_sdk_openai::OpenAiResponsesOptions {
        let temporary = TempDir::new().expect("temporary directory");
        let provider_id = ProviderId::new("openai").expect("provider ID");
        let definition = toml::from_str::<ProviderDefinition>(&format!(
            r#"source = "custom"
endpoint = "http://127.0.0.1:9/v1"
adaptor = "openai-responses"
auth = {{ method = "bearer-api-key-v1", values = {{ api_key = "test-key" }} }}

[models.test]
display_name = "Test Responses"
capabilities = {{ input = ["text"], output = ["text"], context_tokens = 32768, output_tokens = 4096, tool_calling = true, parallel_tool_calls = true, structured_output = true, reasoning = {reasoning}, temperature = false, top_p = false, seed = false, native_replay = "{replay}", media = {{}} }}
"#,
            replay = if reasoning { "optional" } else { "unsupported" },
        ))
        .expect("custom Responses provider");
        let authored = BTreeMap::from([(provider_id, definition)]);
        let provider_store =
            ProviderStore::open(temporary.path().join("providers")).expect("provider store");
        let manager =
            ModelManager::new(authored, empty_catalog(), provider_store).expect("model manager");
        let runtime = manager.current();
        let compiled = &runtime
            .models()
            .values()
            .next()
            .expect("compiled model")
            .model;
        assert!(!super::compatible_responses(compiled));
        super::openai_responses_options(compiled)
    }

    #[test]
    fn openai_responses_requests_automatic_reasoning_summaries() {
        assert_eq!(
            openai_responses_options(true).reasoning_summary.as_deref(),
            Some("auto")
        );
    }

    #[test]
    fn openai_responses_omits_reasoning_summaries_without_reasoning() {
        assert_eq!(openai_responses_options(false).reasoning_summary, None);
    }
}
