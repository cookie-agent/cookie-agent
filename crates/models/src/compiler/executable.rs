use std::collections::BTreeMap;

use cookie_agent_identity::AuthFieldName;
use oven_sdk::ModelCapabilities;
use serde_json::{Value, json};
use zeroize::Zeroize as _;

use crate::{
    ProviderOptions, ReasoningBehavior, ReasoningEffort, RequestDefaults,
    adapters::{
        OvenAdapterFamily,
        oven::{AdapterConfig, AuthConfig, CommonDefaults, ConcreteModel, ModelBuildError},
    },
    compiler::CompiledDynamicModel,
};

pub(crate) struct ExecutableCredentialMaterial {
    pub method: String,
    pub values: BTreeMap<AuthFieldName, String>,
}

pub(crate) struct ExecutableBehaviorInput<'a> {
    pub defaults: &'a RequestDefaults,
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
    capabilities: ModelCapabilities,
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
        if model.adapter == OvenAdapterFamily::OpenaiResponses {
            return crate::adapters::no_auth_responses::build(
                executable_provider_id(provider_id, model.adapter, model.custom),
                &executable_model_id(model),
                endpoint,
                headers,
                capabilities,
            );
        }
    }
    let auth = executable_auth(model, credentials, behavior.options)?;
    let adapter = adapter_config(model, behavior.options, behavior.reasoning)?;
    let concrete_capabilities = capabilities.clone();
    let compiled = ConcreteModel {
        provider_id: executable_provider_id(provider_id, model.adapter, model.custom).to_owned(),
        model_id: executable_model_id(model),
        endpoint,
        auth,
        headers,
        capabilities,
        defaults: CommonDefaults {
            max_output_tokens: behavior.defaults.max_output_tokens,
            temperature: behavior
                .defaults
                .temperature
                .map(|value| f64::from(value.get())),
            top_p: behavior.defaults.top_p.map(|value| f64::from(value.get())),
            reasoning_effort: behavior.reasoning.and_then(reasoning_effort),
            include_raw: false,
        },
        adapter,
    }
    .build()?;
    if model.auth.method == "no-auth-v1" && model.adapter == OvenAdapterFamily::OpenaiChat {
        return crate::adapters::reattribute(
            compiled,
            executable_provider_id(provider_id, model.adapter, model.custom),
            &executable_model_id(model),
            &model.adapter_id,
            concrete_capabilities,
        );
    }
    Ok(compiled)
}

fn executable_provider_id(provider_id: &str, family: OvenAdapterFamily, custom: bool) -> &str {
    // Custom Responses providers keep their full authored ID as replay
    // identity so separate gateways never share native replay history with
    // each other or with the managed `openai` family identity.
    if custom && family == OvenAdapterFamily::OpenaiResponses {
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

fn adapter_config(
    model: &CompiledDynamicModel,
    options: &ProviderOptions,
    reasoning: Option<&ReasoningBehavior>,
) -> Result<AdapterConfig, ModelBuildError> {
    let structured = if model.capabilities.structured_output {
        "json_schema"
    } else {
        "unsupported"
    };
    let reasoning_field = if model.capabilities.reasoning {
        "reasoning_content"
    } else {
        "none"
    };
    let value = match model.adapter {
        OvenAdapterFamily::Anthropic => json!({
            "adaptor": "anthropic",
            "settings": {
                "thinking": if model.capabilities.reasoning { "both" } else { "none" },
                "thinking_default_active": false,
                "thinking_disable_allowed": model.capabilities.reasoning,
                "thinking_disable_forbidden_efforts": [],
                "effort": model.capabilities.reasoning,
                "assistant_prefill": false,
                "reject_non_default_sampling": false,
                "native_context_discriminator": Value::Null
            },
            "options": anthropic_options(options, reasoning)
        }),
        OvenAdapterFamily::AnthropicCompatible => json!({
            "adaptor": "anthropic-compatible",
            "settings": {
                "adapter_id": model.adapter_id,
                "thinking": if model.capabilities.reasoning { "both" } else { "none" },
                "thinking_default_active": false,
                "thinking_disable_allowed": model.capabilities.reasoning,
                "thinking_disable_forbidden_efforts": [],
                "effort": model.capabilities.reasoning,
                "assistant_prefill": false,
                "reject_non_default_sampling": !model.capabilities.temperature,
                "native_context_discriminator": Value::Null
            },
            "options": anthropic_options(options, reasoning)
        }),
        OvenAdapterFamily::OpenaiChat if model.auth.method == "no-auth-v1" => json!({
            "adaptor": "openai-compatible",
            "settings": {
                "adapter_id": "cookie.openai-chat.no-auth.v1",
                "system_message_role": "developer",
                "max_tokens_field": if model.capabilities.reasoning { "max_completion_tokens" } else { "max_tokens" },
                "stream_usage": false,
                "structured_output": structured,
                "reasoning_field": reasoning_field,
                "query": {},
                "request_id_headers": ["x-request-id"],
                "strict_sse_content_type": true,
                "routing_discriminator": Value::Null
            },
            "options": {}
        }),
        OvenAdapterFamily::OpenaiChat => json!({
            "adaptor": "openai-chat",
            "settings": {
                "system_message_role": "developer",
                "max_tokens_field": if model.capabilities.reasoning { "max_completion_tokens" } else { "max_tokens" },
                "stream_usage": false,
                "structured_output": structured,
                "reasoning_field": reasoning_field,
                "routing_discriminator": Value::Null
            },
            "options": { "reasoning_effort": reasoning.and_then(reasoning_effort) }
        }),
        OvenAdapterFamily::OpenaiResponses => json!({
            "adaptor": "openai-responses",
            "settings": {
                "routing_discriminator": Value::Null,
                "compaction": if model.capabilities.compaction == crate::CompactionCapability::Native {
                    "openai-responses-compact"
                } else {
                    "unsupported"
                }
            },
            "options": {
                "reasoning_mode": reasoning.and_then(reasoning_effort),
                "parallel_tool_calls": model.capabilities.parallel_tool_calls
            }
        }),
        OvenAdapterFamily::OpenaiCompatible => json!({
            "adaptor": "openai-compatible",
            "settings": {
                "adapter_id": model.adapter_id,
                "system_message_role": "system",
                "max_tokens_field": "max_tokens",
                "stream_usage": false,
                "structured_output": structured,
                "reasoning_field": if model.capabilities.reasoning { model.reasoning_field.as_str() } else { "none" },
                "routing_discriminator": (model.auth.method == "api-key-header-v1")
                    .then(|| format!("header:{}", model.auth.safe_parameters.get("header_name").map_or("api-key", String::as_str)))
            },
            "options": {}
        }),
        OvenAdapterFamily::GoogleGemini => json!({
            "adaptor": "google",
            "settings": {
                "model_resource": format!("models/{}", model.id),
                "thinking": google_thinking(reasoning),
                "strict_functions": model.capabilities.structured_output,
                "mixed_client_and_provider_tools": false,
                "current_turn_signature_sentinel": model.capabilities.native_replay != crate::ReplayCapability::Unsupported
            },
            "options": google_options(reasoning)
        }),
        OvenAdapterFamily::GoogleVertexGemini => json!({
            "adaptor": "vertex",
            "settings": {
                "project": setup(model, "project")?,
                "location": setup(model, "location")?,
                "resource": { "type": "publisher_model", "publisher": if model.family_id == "vertex-anthropic" { "anthropic" } else { "google" }, "model": model.id.as_str() },
                "thinking": vertex_thinking(reasoning),
                "provider_tools": false,
                "mixed_client_and_provider_tools": false,
                "strict_functions": model.capabilities.structured_output,
                "stream_function_call_arguments": false,
                "media": vertex_media()
            },
            "options": vertex_options(reasoning)
        }),
        OvenAdapterFamily::AwsBedrockConverse => json!({
            "adaptor": "bedrock",
            "settings": {
                "region": setup(model, "region")?,
                "reasoning_wire_format": if model.capabilities.reasoning { "bedrock_reasoning_config" } else { "unsupported" },
                "signed_reasoning": false,
                "structured_output": if model.capabilities.structured_output { "json_schema" } else { "unsupported" },
                "max_event_message_bytes": 16 * 1024 * 1024
            },
            "options": bedrock_options(reasoning)
        }),
        OvenAdapterFamily::AzureOpenaiChat => json!({
            "adaptor": "azure-chat",
            "settings": {
                "route": { "kind": "v1" },
                "revision": Value::Null,
                "system_role": "developer",
                "max_tokens_field": if model.capabilities.reasoning { "max_completion_tokens" } else { "max_tokens" },
                "stream_usage": false,
                "structured_output": structured,
                "reasoning_field": reasoning_field,
                "omit_reasoning_sampling": model.capabilities.reasoning
            },
            "options": { "reasoning_effort": reasoning.and_then(reasoning_effort) }
        }),
        OvenAdapterFamily::AzureOpenaiResponses => json!({
            "adaptor": "azure-responses",
            "settings": {
                "route": { "kind": "v1" },
                "revision": if model.capabilities.compaction == crate::CompactionCapability::Native {
                    json!({
                        "model": setup(model, "model")?,
                        "version": setup(model, "version")?,
                        "deployment_type": setup(model, "deployment_type")?
                    })
                } else {
                    Value::Null
                },
                "compaction": if model.capabilities.compaction == crate::CompactionCapability::Native {
                    json!({
                        "kind": "azure-responses-compact",
                        "routing_discriminator": model.adapter_id
                    })
                } else {
                    json!({ "kind": "unsupported" })
                }
            },
            "options": { "reasoning_mode": reasoning.and_then(reasoning_effort) }
        }),
        OvenAdapterFamily::CohereV2Chat => json!({
            "adaptor": "cohere",
            "settings": {
                "strict_tools": model.capabilities.structured_output,
                "safety_mode": Value::Null,
                "thinking": cohere_thinking(reasoning),
                "reasoning_effort": {},
                "top_k": Value::Null,
                "seed": Value::Null,
                "frequency_penalty": Value::Null,
                "presence_penalty": Value::Null,
                "stop_sequences": [],
                "priority": Value::Null
            },
            "options": {}
        }),
    };
    serde_json::from_value(value).map_err(ModelBuildError::ProviderOptions)
}

fn setup<'a>(model: &'a CompiledDynamicModel, name: &str) -> Result<&'a str, ModelBuildError> {
    model
        .setup
        .as_ref()
        .and_then(|setup| setup.values.get(name))
        .map(String::as_str)
        .ok_or_else(|| wrong_auth("dynamic", "complete setup material"))
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

fn executable_model_id(model: &CompiledDynamicModel) -> String {
    if matches!(
        model.adapter,
        OvenAdapterFamily::AzureOpenaiChat | OvenAdapterFamily::AzureOpenaiResponses
    ) {
        model
            .setup
            .as_ref()
            .and_then(|setup| setup.values.get("deployment"))
            .cloned()
            .unwrap_or_else(|| model.id.as_str().to_owned())
    } else {
        model.id.as_str().to_owned()
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

fn anthropic_options(options: &ProviderOptions, reasoning: Option<&ReasoningBehavior>) -> Value {
    let thinking = match reasoning {
        Some(ReasoningBehavior::Toggle { enabled: false }) | None => Value::Null,
        Some(ReasoningBehavior::Toggle { enabled: true }) => {
            json!({ "type": "adaptive", "display": Value::Null })
        }
        Some(ReasoningBehavior::BudgetTokens { value }) if *value > 0 => {
            json!({ "type": "enabled", "budget_tokens": value, "display": Value::Null })
        }
        Some(ReasoningBehavior::BudgetTokens { .. }) => {
            json!({ "type": "adaptive", "display": Value::Null })
        }
        Some(ReasoningBehavior::Effort { .. }) => Value::Null,
    };
    json!({
        "thinking": thinking,
        "effort": reasoning.and_then(reasoning_effort),
        "cache_ttl": Value::Null,
        "user_id": Value::Null,
        "betas": options.beta
    })
}

fn google_thinking(reasoning: Option<&ReasoningBehavior>) -> Value {
    match reasoning {
        Some(ReasoningBehavior::Effort { value }) => json!({
            "type": "level",
            "effort_levels": { reasoning_effort(&ReasoningBehavior::Effort { value: *value }).unwrap_or_default(): reasoning_effort(&ReasoningBehavior::Effort { value: *value }).unwrap_or_default() }
        }),
        Some(_) => json!({ "type": "budget", "effort_budgets": {} }),
        None => json!({ "type": "unsupported" }),
    }
}

fn google_options(reasoning: Option<&ReasoningBehavior>) -> Value {
    let thinking = match reasoning {
        Some(ReasoningBehavior::BudgetTokens { value }) => {
            json!({ "thinking_budget": value, "thinking_level": Value::Null, "include_thoughts": true })
        }
        Some(ReasoningBehavior::Toggle { enabled }) => {
            json!({ "thinking_budget": if *enabled { -1 } else { 0 }, "thinking_level": Value::Null, "include_thoughts": *enabled })
        }
        Some(ReasoningBehavior::Effort { .. }) => {
            json!({ "thinking_budget": Value::Null, "thinking_level": reasoning.and_then(reasoning_effort), "include_thoughts": true })
        }
        None => Value::Null,
    };
    json!({ "thinking": thinking })
}

fn vertex_thinking(reasoning: Option<&ReasoningBehavior>) -> &'static str {
    match reasoning {
        Some(ReasoningBehavior::Effort { .. }) => "level",
        Some(_) => "budget",
        None => "unsupported",
    }
}

fn vertex_options(reasoning: Option<&ReasoningBehavior>) -> Value {
    let thinking = match reasoning {
        Some(ReasoningBehavior::BudgetTokens { value }) => {
            json!({ "thinking_budget": value, "thinking_level": Value::Null, "include_thoughts": true })
        }
        Some(ReasoningBehavior::Toggle { enabled }) => {
            json!({ "thinking_budget": if *enabled { -1 } else { 0 }, "thinking_level": Value::Null, "include_thoughts": *enabled })
        }
        Some(ReasoningBehavior::Effort { .. }) => {
            json!({ "thinking_budget": Value::Null, "thinking_level": reasoning.and_then(reasoning_effort), "include_thoughts": true })
        }
        None => Value::Null,
    };
    json!({ "thinking": thinking })
}

fn vertex_media() -> Value {
    json!({
        "max_images": 20,
        "max_https_images": 20,
        "max_documents": 5,
        "max_audio": 5,
        "max_videos": 5,
        "max_https_videos": 5,
        "max_inline_image_bytes": 20 * 1024 * 1024,
        "max_inline_pdf_bytes": 32 * 1024 * 1024,
        "max_inline_text_bytes": 1024 * 1024,
        "url_schemes": ["https"]
    })
}

fn bedrock_options(reasoning: Option<&ReasoningBehavior>) -> Value {
    match reasoning {
        Some(ReasoningBehavior::Toggle { enabled }) => json!({
            "reasoning_type": if *enabled { "enabled" } else { "disabled" }
        }),
        Some(ReasoningBehavior::BudgetTokens { value }) if *value > 0 => json!({
            "reasoning_type": "enabled",
            "reasoning_budget_tokens": value
        }),
        Some(ReasoningBehavior::Effort { .. }) => json!({
            "max_reasoning_effort": reasoning.and_then(reasoning_effort)
        }),
        _ => json!({}),
    }
}

fn cohere_thinking(reasoning: Option<&ReasoningBehavior>) -> Value {
    match reasoning {
        Some(ReasoningBehavior::Toggle { enabled }) => {
            json!({ "enabled": enabled, "token_budget": Value::Null })
        }
        Some(ReasoningBehavior::BudgetTokens { value }) if *value > 0 => {
            json!({ "enabled": true, "token_budget": value })
        }
        Some(ReasoningBehavior::BudgetTokens { .. }) => {
            json!({ "enabled": true, "token_budget": Value::Null })
        }
        _ => Value::Null,
    }
}

fn wrong_auth(adapter: &'static str, expected: &'static str) -> ModelBuildError {
    ModelBuildError::WrongAuth { adapter, expected }
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
        manager::{ModelManager, safe_definition_fingerprint},
        manifests::{ModelSnapshotManifestStore, frozen_binding},
        provider_store::ProviderStore,
    };

    fn revision(label: &str) -> CatalogRevision {
        CatalogRevision::new(format!("sha256:{:x}", Sha256::digest(label.as_bytes())))
            .expect("catalog revision")
    }

    fn empty_catalog() -> Arc<CatalogSnapshot> {
        let now = Timestamp::now();
        Arc::new(CatalogSnapshot {
            revision: revision("custom-responses-rehydration"),
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
    fn custom_responses_identity_survives_manifest_rehydration() {
        // Custom Responses providers keep their full authored ID as replay
        // identity: prefixed, bare, and even a catalog-colliding bare ID.
        for id in ["custom.gateway", "gateway", "openai"] {
            custom_responses_identity_survives_manifest_rehydration_for(id);
        }
    }

    fn custom_responses_identity_survives_manifest_rehydration_for(id: &str) {
        let temporary = TempDir::new().expect("temporary directory");
        let provider_id = ProviderId::new(id).expect("provider ID");
        let definition = toml::from_str::<ProviderDefinition>(
            r#"source = "custom"
endpoint = "http://127.0.0.1:9/v1"
adaptor = "openai-responses"
auth = { method = "bearer-api-key-v1", values = { api_key = "test-key" } }

[models.test]
display_name = "Test Responses"
capabilities = { input = ["text"], output = ["text"], context_tokens = 32768, output_tokens = 4096, tool_calling = true, parallel_tool_calls = true, structured_output = true, reasoning = true, temperature = false, top_p = false, seed = false, native_replay = "optional", cancellation = "local_only", media = {} }
"#,
        )
        .expect("custom Responses provider");
        let authored = BTreeMap::from([(provider_id.clone(), definition)]);
        let provider_store =
            ProviderStore::open(temporary.path().join("providers")).expect("provider store");
        let manager =
            ModelManager::new(authored, empty_catalog(), provider_store).expect("model manager");
        let runtime = manager.current();

        let manifest_store =
            ModelSnapshotManifestStore::open_directory(temporary.path().join("manifests"))
                .expect("manifest store");
        let manifest = manifest_store
            .write(runtime.manifest_payload().expect("manifest payload"))
            .expect("write manifest");
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
        let rehydrated = manifest_store
            .scan()
            .expect("manifest index")
            .rehydrate(
                &binding,
                runtime.authored(),
                runtime.store(),
                safe_definition_fingerprint,
            )
            .expect("rehydrated blueprint");
        let resolved = runtime
            .resolve_frozen(&binding, &rehydrated.blueprint)
            .expect("rehydrated executable");

        assert_eq!(
            resolved.model().descriptor().identity.provider_id.as_str(),
            provider_id.as_str()
        );
        assert_eq!(
            resolved.model().descriptor().identity.model_id.as_str(),
            "test"
        );
    }
}
