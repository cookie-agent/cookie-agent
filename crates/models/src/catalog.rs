//! Offline models.dev catalog parsing, safe projection, and reviewed recipes.

use std::{collections::BTreeMap, fmt, sync::Arc};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest as _, Sha256};
use thiserror::Error;

use crate::ReasoningEffort;

/// Exact upstream commit used by the embedded artifact.
pub const MODELS_DEV_COMMIT: &str = "c3057690bbb8bd41cafdefadcd2a7b958e2a4642";
/// SHA-256 of the exact embedded upstream `snapshotPayload` bytes.
pub const MODELS_DEV_ARTIFACT_SHA256: &str =
    "d65af0b058204954f6b08af537fa13e91f251c618d69d8c20a2d5915731d482a";
/// Exact byte size of the embedded artifact.
pub const MODELS_DEV_ARTIFACT_BYTES: usize = 3_567_054;
/// Stable source identity for safe catalog projections.
pub const MODELS_DEV_SOURCE: &str =
    "https://github.com/anomalyco/models.dev@c3057690bbb8bd41cafdefadcd2a7b958e2a4642";
/// Upstream commit timestamp normalized to UTC.
pub const MODELS_DEV_FETCHED_AT: &str = "2026-08-01T17:34:27Z";
/// Exact generated artifact, embedded without build-time or runtime I/O.
pub const MODELS_DEV_JSON: &[u8] = include_bytes!("../catalog/models-dev.json");

const MAX_CATALOG_BYTES: usize = 8 * 1024 * 1024;
const MAX_PROVIDERS: usize = 1_000;
const MAX_MODELS: usize = 100_000;
const MAX_IDENTIFIER_BYTES: usize = 1_024;
const MAX_TEXT_BYTES: usize = 16 * 1024;

/// Parsed catalog with every upstream JSON record retained verbatim internally.
#[derive(Clone)]
pub struct Catalog {
    raw: Arc<Value>,
    providers: Arc<BTreeMap<String, CatalogProvider>>,
    models: Arc<Vec<CatalogModel>>,
    by_model: Arc<BTreeMap<(String, String), usize>>,
    revision: String,
}

impl Catalog {
    /// Parses and validates the checked-in exact catalog artifact.
    pub fn embedded() -> Result<Self, CatalogError> {
        let digest = format!("{:x}", Sha256::digest(MODELS_DEV_JSON));
        if MODELS_DEV_JSON.len() != MODELS_DEV_ARTIFACT_BYTES
            || digest != MODELS_DEV_ARTIFACT_SHA256
        {
            return Err(CatalogError::ArtifactMismatch);
        }
        Self::parse(MODELS_DEV_JSON)
    }

    /// Parses a complete generated models.dev catalog under strict resource limits.
    pub fn parse(bytes: &[u8]) -> Result<Self, CatalogError> {
        if bytes.len() > MAX_CATALOG_BYTES {
            return Err(CatalogError::TooLarge);
        }
        let raw: Value = serde_json::from_slice(bytes).map_err(CatalogError::Json)?;
        let root = object(&raw, "catalog")?;
        exact_keys(root, &["providers", "models"], "catalog")?;
        let raw_providers = object(required(root, "providers")?, "providers")?;
        let raw_models = object(required(root, "models")?, "models")?;
        if raw_providers.len() > MAX_PROVIDERS {
            return Err(CatalogError::Limit("too many providers"));
        }
        if raw_models.len() > MAX_MODELS {
            return Err(CatalogError::Limit("too many model metadata records"));
        }

        let top_level_models = raw_models
            .iter()
            .map(|(id, value)| {
                validate_identifier(id, "model metadata id")?;
                let record = object(value, "model metadata")?;
                if string(required(record, "id")?, "model metadata id")? != id {
                    return Err(CatalogError::IdentityMismatch);
                }
                validate_optional_date(record.get("release_date"), "release_date")?;
                validate_optional_date(record.get("last_updated"), "last_updated")?;
                Ok((id.clone(), value.clone()))
            })
            .collect::<Result<BTreeMap<_, _>, CatalogError>>()?;

        let mut providers = BTreeMap::new();
        let mut models = Vec::new();
        for (provider_id, raw_provider) in raw_providers {
            validate_identifier(provider_id, "provider id")?;
            let provider = parse_provider(provider_id, raw_provider)?;
            let provider_models = object(
                required(object(raw_provider, "provider")?, "models")?,
                "provider models",
            )?;
            if models.len().saturating_add(provider_models.len()) > MAX_MODELS {
                return Err(CatalogError::Limit("too many provider model records"));
            }
            for (model_id, raw_model) in provider_models {
                models.push(parse_model(
                    &provider,
                    model_id,
                    raw_model,
                    top_level_models
                        .contains_key(&format!("{provider_id}/{model_id}"))
                        .then(|| format!("{provider_id}/{model_id}")),
                )?);
            }
            providers.insert(provider_id.clone(), provider);
        }
        models.sort_by(|left, right| {
            (&left.provider_id, &left.model_id).cmp(&(&right.provider_id, &right.model_id))
        });
        let by_model = models
            .iter()
            .enumerate()
            .map(|(index, model)| ((model.provider_id.clone(), model.model_id.clone()), index))
            .collect();

        let revision = format!("sha256:{:x}", Sha256::digest(bytes));
        Ok(Self {
            raw: Arc::new(raw),
            providers: Arc::new(providers),
            models: Arc::new(models),
            by_model: Arc::new(by_model),
            revision,
        })
    }

    /// Returns the SHA-256 revision of the exact parsed snapshot bytes.
    #[must_use]
    pub fn revision(&self) -> &str {
        &self.revision
    }

    /// Returns the immutable source identity accompanying protocol projections.
    #[must_use]
    pub fn snapshot(&self) -> CatalogSnapshot {
        CatalogSnapshot {
            revision: self.revision.clone(),
            source: MODELS_DEV_SOURCE.to_owned(),
            fetched_at: MODELS_DEV_FETCHED_AT.to_owned(),
        }
    }

    /// Returns all safe provider projections in stable wire order.
    #[must_use]
    pub fn providers(&self) -> &BTreeMap<String, CatalogProvider> {
        &self.providers
    }

    /// Returns all safe model projections in stable provider/model order.
    #[must_use]
    pub fn models(&self) -> &[CatalogModel] {
        &self.models
    }

    /// Returns one exact provider/model wire record.
    #[must_use]
    pub fn model(&self, provider_id: &str, model_id: &str) -> Option<&CatalogModel> {
        self.by_model
            .get(&(provider_id.to_owned(), model_id.to_owned()))
            .map(|index| &self.models[*index])
    }

    /// Returns whether an upstream provider is known independently of support.
    #[must_use]
    pub fn is_known_provider(&self, provider_id: &str) -> bool {
        self.providers.contains_key(provider_id)
    }

    /// Returns whether at least one model has a reviewed safe recipe.
    #[must_use]
    pub fn is_supported_provider(&self, provider_id: &str) -> bool {
        self.models
            .iter()
            .filter(|model| model.provider_id == provider_id)
            .any(|model| self.recipe(model).is_ok())
    }

    /// Returns the reviewed recipe for one exact record, or an explicit reason.
    pub fn recipe(&self, model: &CatalogModel) -> Result<CatalogRecipe, UnsupportedReason> {
        let provider = self
            .providers
            .get(&model.provider_id)
            .ok_or(UnsupportedReason::UnknownProvider)?;
        if model.status == CatalogModelStatus::Deprecated {
            return Err(UnsupportedReason::DeprecatedModel);
        }
        if !model.modalities.input.iter().any(|value| value == "text")
            || model.modalities.output != ["text"]
            || model.limits.context == 0
            || model.limits.output == 0
            || model.limits.output > model.limits.context
            || model
                .limits
                .input
                .is_some_and(|input| input > model.limits.context)
        {
            return Err(UnsupportedReason::NotTextGeneration);
        }
        if explicitly_unsupported(&model.provider_id, &provider.npm) {
            return Err(UnsupportedReason::ExplicitlyUnsupported);
        }
        let raw = self.raw_provider_model(&model.provider_id, &model.model_id)?;
        let override_provider = raw.get("provider").and_then(Value::as_object);
        if override_provider.is_some_and(has_injected_body_or_headers) {
            return Err(UnsupportedReason::ProviderInjectionRequired);
        }
        let effective_npm = override_provider
            .and_then(|value| value.get("npm"))
            .and_then(Value::as_str)
            .unwrap_or(&provider.npm);
        let effective_api = override_provider
            .and_then(|value| value.get("api"))
            .and_then(Value::as_str)
            .or(provider.api.as_deref());

        match (model.provider_id.as_str(), effective_npm) {
            ("anthropic", "@ai-sdk/anthropic") => Ok(CatalogRecipe::Anthropic),
            ("openai", "@ai-sdk/openai") => openai_recipe(&model.model_id),
            ("google", "@ai-sdk/google") => Ok(CatalogRecipe::Google),
            ("cohere", "@ai-sdk/cohere") => Ok(CatalogRecipe::Cohere),
            ("openrouter", "@openrouter/ai-sdk-provider") => {
                https_single_credential(provider, effective_api)?;
                Ok(CatalogRecipe::OpenRouterChat)
            }
            (_, "@ai-sdk/openai-compatible") => {
                https_single_credential(provider, effective_api)?;
                Ok(CatalogRecipe::OpenAiCompatibleChat)
            }
            _ => Err(UnsupportedReason::UnreviewedPackage),
        }
    }

    pub(crate) fn effective_api(&self, model: &CatalogModel) -> Option<String> {
        self.raw_provider_model(&model.provider_id, &model.model_id)
            .ok()
            .and_then(|raw| raw.get("provider").and_then(Value::as_object))
            .and_then(|provider| provider.get("api").and_then(Value::as_str))
            .or_else(|| {
                self.providers
                    .get(&model.provider_id)
                    .and_then(|provider| provider.api.as_deref())
            })
            .map(ToOwned::to_owned)
    }

    fn raw_provider_model(
        &self,
        provider_id: &str,
        model_id: &str,
    ) -> Result<&Map<String, Value>, UnsupportedReason> {
        self.raw
            .get("providers")
            .and_then(|value| value.get(provider_id))
            .and_then(|value| value.get("models"))
            .and_then(|value| value.get(model_id))
            .and_then(Value::as_object)
            .ok_or(UnsupportedReason::UnknownModel)
    }
}

impl fmt::Debug for Catalog {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Catalog")
            .field("revision", &self.revision)
            .field("provider_count", &self.providers.len())
            .field("model_count", &self.models.len())
            .finish()
    }
}

/// Safe provider projection with known-vs-supported kept distinct.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogProvider {
    pub id: String,
    pub name: String,
    pub credential_fields: Vec<String>,
    pub npm: String,
    pub api: Option<String>,
    pub documentation_url: String,
}

/// Safe immutable source identity for provider/model list projections.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogSnapshot {
    pub revision: String,
    pub source: String,
    pub fetched_at: String,
}

/// Safe provider-specific model projection.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogModel {
    pub provider_id: String,
    pub model_id: String,
    pub canonical_model_id: Option<String>,
    pub name: String,
    pub family: Option<String>,
    pub capabilities: CatalogModelCapabilities,
    pub reasoning_options: Vec<CatalogReasoningOption>,
    pub limits: CatalogModelLimits,
    pub modalities: CatalogModelModalities,
    pub status: CatalogModelStatus,
    pub release_date: String,
    pub last_updated: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum CatalogReasoningOption {
    Effort {
        values: Vec<Option<ReasoningEffort>>,
    },
    Toggle,
    BudgetTokens {
        min: Option<i64>,
        max: Option<i64>,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogModelCapabilities {
    pub attachment: bool,
    pub reasoning: bool,
    pub tool_call: bool,
    pub structured_output: bool,
    pub temperature: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogModelLimits {
    pub context: u64,
    pub input: Option<u64>,
    pub output: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogModelModalities {
    pub input: Vec<String>,
    pub output: Vec<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CatalogModelStatus {
    Stable,
    Alpha,
    Beta,
    Deprecated,
}

/// Hand-reviewed construction recipes. Absence is explicit unsupported state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CatalogRecipe {
    Anthropic,
    OpenAiResponses,
    OpenAiChat,
    Google,
    Cohere,
    OpenRouterChat,
    OpenAiCompatibleChat,
}

/// Why a known catalog record cannot be generated safely.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum UnsupportedReason {
    #[error("unknown catalog provider")]
    UnknownProvider,
    #[error("unknown catalog model")]
    UnknownModel,
    #[error("provider is explicitly unsupported")]
    ExplicitlyUnsupported,
    #[error("model is deprecated")]
    DeprecatedModel,
    #[error("model is not bounded text generation")]
    NotTextGeneration,
    #[error("provider package has not been reviewed")]
    UnreviewedPackage,
    #[error("provider endpoint is not reviewed HTTPS")]
    InsecureEndpoint,
    #[error("provider does not have exactly one credential field")]
    CredentialShape,
    #[error("provider body or header injection would be required")]
    ProviderInjectionRequired,
    #[error("OpenAI model has no exact reviewed API-family mapping")]
    UnmappedOpenAiModel,
}

#[derive(Debug, Error)]
pub enum CatalogError {
    #[error("embedded models.dev artifact does not match pinned provenance")]
    ArtifactMismatch,
    #[error("catalog exceeds the input byte limit")]
    TooLarge,
    #[error("catalog JSON is invalid")]
    Json(#[source] serde_json::Error),
    #[error("catalog field is missing: {0}")]
    Missing(&'static str),
    #[error("catalog field has an invalid type at {0}")]
    Type(&'static str),
    #[error("catalog has unknown or missing fields at {0}")]
    Shape(&'static str),
    #[error("catalog identity does not match its exact map key")]
    IdentityMismatch,
    #[error("catalog input limit exceeded: {0}")]
    Limit(&'static str),
    #[error("catalog identifier is invalid: {0}")]
    Identifier(&'static str),
    #[error("catalog date is invalid at {0}")]
    Date(&'static str),
    #[error("catalog limit is invalid at {0}")]
    TokenLimit(&'static str),
}

fn parse_provider(id: &str, value: &Value) -> Result<CatalogProvider, CatalogError> {
    let object = object(value, "provider")?;
    exact_keys(
        object,
        &["id", "env", "npm", "api", "name", "doc", "models"],
        "provider",
    )?;
    if string(required(object, "id")?, "provider id")? != id {
        return Err(CatalogError::IdentityMismatch);
    }
    let credential_fields = strings(required(object, "env")?, "provider env")?;
    if credential_fields.is_empty() || credential_fields.len() > 32 {
        return Err(CatalogError::Limit("provider credential fields"));
    }
    for field in &credential_fields {
        validate_identifier(field, "credential field")?;
    }
    Ok(CatalogProvider {
        id: id.to_owned(),
        name: bounded_string(required(object, "name")?, "provider name")?,
        credential_fields,
        npm: bounded_string(required(object, "npm")?, "provider npm")?,
        api: object
            .get("api")
            .map(|value| bounded_string(value, "provider api"))
            .transpose()?,
        documentation_url: bounded_string(required(object, "doc")?, "provider doc")?,
    })
}

fn parse_model(
    provider: &CatalogProvider,
    id: &str,
    value: &Value,
    canonical_model_id: Option<String>,
) -> Result<CatalogModel, CatalogError> {
    validate_identifier(id, "provider model id")?;
    let record = object(value, "provider model")?;
    if string(required(record, "id")?, "provider model id")? != id {
        return Err(CatalogError::IdentityMismatch);
    }
    let limits = object(required(record, "limit")?, "model limit")?;
    let context = token(required(limits, "context")?, "limit.context")?;
    let output = token(required(limits, "output")?, "limit.output")?;
    let input = limits
        .get("input")
        .map(|value| token(value, "limit.input"))
        .transpose()?;
    let modalities = object(required(record, "modalities")?, "model modalities")?;
    let input_modalities = strings(required(modalities, "input")?, "modalities.input")?;
    let output_modalities = strings(required(modalities, "output")?, "modalities.output")?;
    for modality in input_modalities.iter().chain(&output_modalities) {
        if !matches!(
            modality.as_str(),
            "text" | "audio" | "image" | "video" | "pdf"
        ) {
            return Err(CatalogError::Identifier("modality"));
        }
    }
    let release_date = required_date(record, "release_date")?;
    let last_updated = required_date(record, "last_updated")?;
    Ok(CatalogModel {
        provider_id: provider.id.clone(),
        model_id: id.to_owned(),
        canonical_model_id,
        name: bounded_string(required(record, "name")?, "model name")?,
        family: record
            .get("family")
            .map(|value| bounded_string(value, "model family"))
            .transpose()?,
        capabilities: CatalogModelCapabilities {
            attachment: boolean(required(record, "attachment")?, "attachment")?,
            reasoning: boolean(required(record, "reasoning")?, "reasoning")?,
            tool_call: boolean(required(record, "tool_call")?, "tool_call")?,
            structured_output: optional_bool(record.get("structured_output"), "structured_output")?,
            temperature: optional_bool(record.get("temperature"), "temperature")?,
        },
        reasoning_options: parse_reasoning_options(record.get("reasoning_options"))?,
        limits: CatalogModelLimits {
            context,
            input,
            output,
        },
        modalities: CatalogModelModalities {
            input: input_modalities,
            output: output_modalities,
        },
        status: match record.get("status").and_then(Value::as_str) {
            None => CatalogModelStatus::Stable,
            Some("alpha") => CatalogModelStatus::Alpha,
            Some("beta") => CatalogModelStatus::Beta,
            Some("deprecated") => CatalogModelStatus::Deprecated,
            Some(_) => return Err(CatalogError::Identifier("model status")),
        },
        release_date,
        last_updated,
    })
}

fn parse_reasoning_options(
    value: Option<&Value>,
) -> Result<Vec<CatalogReasoningOption>, CatalogError> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let values = value
        .as_array()
        .ok_or(CatalogError::Type("reasoning_options"))?;
    if values.len() > 32 {
        return Err(CatalogError::Limit("reasoning_options"));
    }
    values
        .iter()
        .map(|value| {
            let object = object(value, "reasoning option")?;
            match string(required(object, "type")?, "reasoning option type")? {
                "effort" => {
                    exact_keys(object, &["type", "values"], "reasoning effort")?;
                    let raw = required(object, "values")?
                        .as_array()
                        .ok_or(CatalogError::Type("reasoning effort values"))?;
                    let mut seen = BTreeMap::new();
                    let mut parsed = Vec::new();
                    for value in raw {
                        let effort = if value.is_null() {
                            None
                        } else {
                            Some(
                                match value
                                    .as_str()
                                    .ok_or(CatalogError::Type("reasoning effort"))?
                                {
                                    "none" => ReasoningEffort::None,
                                    "minimal" => ReasoningEffort::Minimal,
                                    "low" => ReasoningEffort::Low,
                                    "medium" => ReasoningEffort::Medium,
                                    "high" => ReasoningEffort::High,
                                    "xhigh" => ReasoningEffort::Xhigh,
                                    "max" => ReasoningEffort::Max,
                                    "default" => ReasoningEffort::Default,
                                    _ => return Err(CatalogError::Identifier("reasoning effort")),
                                },
                            )
                        };
                        let key = format!("{effort:?}");
                        if seen.insert(key, ()).is_some() {
                            return Err(CatalogError::Shape("duplicate reasoning effort"));
                        }
                        parsed.push(effort);
                    }
                    Ok(CatalogReasoningOption::Effort { values: parsed })
                }
                "toggle" => {
                    exact_keys(object, &["type"], "reasoning toggle")?;
                    Ok(CatalogReasoningOption::Toggle)
                }
                "budget_tokens" => {
                    if object
                        .keys()
                        .any(|key| !matches!(key.as_str(), "type" | "min" | "max"))
                    {
                        return Err(CatalogError::Shape("reasoning budget_tokens"));
                    }
                    let min = object
                        .get("min")
                        .map(|value| {
                            value
                                .as_i64()
                                .ok_or(CatalogError::Type("reasoning budget min"))
                        })
                        .transpose()?;
                    let max = object
                        .get("max")
                        .map(|value| {
                            value
                                .as_i64()
                                .ok_or(CatalogError::Type("reasoning budget max"))
                        })
                        .transpose()?;
                    if min.is_some_and(|value| value < -1)
                        || max.is_some_and(|value| value < 0)
                        || matches!((min, max), (Some(min), Some(max)) if min >= 0 && min > max)
                    {
                        return Err(CatalogError::TokenLimit("reasoning budget"));
                    }
                    Ok(CatalogReasoningOption::BudgetTokens { min, max })
                }
                _ => Err(CatalogError::Identifier("reasoning option type")),
            }
        })
        .collect()
}

fn openai_recipe(model_id: &str) -> Result<CatalogRecipe, UnsupportedReason> {
    const RESPONSES: &[&str] = &[
        "gpt-5",
        "gpt-5-mini",
        "gpt-5-nano",
        "gpt-5.1",
        "gpt-5.1-chat-latest",
        "gpt-5.1-codex",
        "gpt-5.1-codex-mini",
        "gpt-5.2",
        "gpt-5.2-chat-latest",
        "gpt-5.2-codex",
        "gpt-5.3",
        "gpt-5.3-chat-latest",
        "gpt-5.3-codex",
        "gpt-5.4",
        "gpt-5.4-mini",
        "gpt-5.4-nano",
        "gpt-5.5",
        "gpt-5.5-instant",
        "gpt-5.6-luna",
        "gpt-5.6-sol",
        "gpt-5.6-terra",
        "o1",
        "o1-mini",
        "o3",
        "o3-mini",
        "o3-pro",
        "o4-mini",
    ];
    const CHAT: &[&str] = &[
        "gpt-3.5-turbo",
        "gpt-4",
        "gpt-4-turbo",
        "gpt-4.1",
        "gpt-4.1-mini",
        "gpt-4.1-nano",
        "gpt-4o",
        "gpt-4o-mini",
        "chatgpt-4o-latest",
    ];
    if RESPONSES.contains(&model_id) {
        Ok(CatalogRecipe::OpenAiResponses)
    } else if CHAT.contains(&model_id) {
        Ok(CatalogRecipe::OpenAiChat)
    } else {
        Err(UnsupportedReason::UnmappedOpenAiModel)
    }
}

fn explicitly_unsupported(provider_id: &str, npm: &str) -> bool {
    matches!(
        provider_id,
        "azure"
            | "azure-openai"
            | "amazon-bedrock"
            | "bedrock"
            | "google-vertex"
            | "vertex"
            | "open-responses"
            | "minimax"
            | "anthropic-aws"
    ) || matches!(
        npm,
        "@ai-sdk/azure"
            | "@ai-sdk/amazon-bedrock"
            | "@ai-sdk/google-vertex"
            | "@ai-sdk/open-responses"
            | "@ai-sdk/minimax"
    )
}

fn https_single_credential(
    provider: &CatalogProvider,
    api: Option<&str>,
) -> Result<(), UnsupportedReason> {
    if provider.credential_fields.len() != 1 {
        return Err(UnsupportedReason::CredentialShape);
    }
    if !api.is_some_and(|value| value.starts_with("https://")) {
        return Err(UnsupportedReason::InsecureEndpoint);
    }
    Ok(())
}

fn has_injected_body_or_headers(value: &Map<String, Value>) -> bool {
    value.contains_key("body") || value.contains_key("headers")
}

fn required<'a>(
    object: &'a Map<String, Value>,
    key: &'static str,
) -> Result<&'a Value, CatalogError> {
    object.get(key).ok_or(CatalogError::Missing(key))
}

fn object<'a>(
    value: &'a Value,
    path: &'static str,
) -> Result<&'a Map<String, Value>, CatalogError> {
    value.as_object().ok_or(CatalogError::Type(path))
}

fn string<'a>(value: &'a Value, path: &'static str) -> Result<&'a str, CatalogError> {
    value.as_str().ok_or(CatalogError::Type(path))
}

fn bounded_string(value: &Value, path: &'static str) -> Result<String, CatalogError> {
    let value = string(value, path)?.trim();
    if value.is_empty() || value.len() > MAX_TEXT_BYTES || value.chars().any(char::is_control) {
        return Err(CatalogError::Identifier(path));
    }
    Ok(value.to_owned())
}

fn validate_identifier(value: &str, path: &'static str) -> Result<(), CatalogError> {
    if value.is_empty() || value.len() > MAX_IDENTIFIER_BYTES || value.chars().any(char::is_control)
    {
        Err(CatalogError::Identifier(path))
    } else {
        Ok(())
    }
}

fn exact_keys(
    object: &Map<String, Value>,
    allowed: &[&str],
    path: &'static str,
) -> Result<(), CatalogError> {
    if object.keys().any(|key| !allowed.contains(&key.as_str()))
        || allowed
            .iter()
            .filter(|key| !matches!(**key, "api"))
            .any(|key| !object.contains_key(*key))
    {
        Err(CatalogError::Shape(path))
    } else {
        Ok(())
    }
}

fn strings(value: &Value, path: &'static str) -> Result<Vec<String>, CatalogError> {
    let values = value.as_array().ok_or(CatalogError::Type(path))?;
    if values.len() > 64 {
        return Err(CatalogError::Limit(path));
    }
    values
        .iter()
        .map(|value| bounded_string(value, path))
        .collect()
}

fn boolean(value: &Value, path: &'static str) -> Result<bool, CatalogError> {
    value.as_bool().ok_or(CatalogError::Type(path))
}

fn optional_bool(value: Option<&Value>, path: &'static str) -> Result<bool, CatalogError> {
    value.map_or(Ok(false), |value| boolean(value, path))
}

fn token(value: &Value, path: &'static str) -> Result<u64, CatalogError> {
    value.as_u64().ok_or(CatalogError::TokenLimit(path))
}

fn required_date(object: &Map<String, Value>, field: &'static str) -> Result<String, CatalogError> {
    let value = string(required(object, field)?, field)?;
    validate_date(value, field)?;
    Ok(value.to_owned())
}

fn validate_optional_date(value: Option<&Value>, field: &'static str) -> Result<(), CatalogError> {
    if let Some(value) = value {
        validate_date(string(value, field)?, field)?;
    }
    Ok(())
}

fn validate_date(value: &str, field: &'static str) -> Result<(), CatalogError> {
    let parts = value.split('-').collect::<Vec<_>>();
    if !matches!(parts.len(), 2 | 3)
        || parts[0].len() != 4
        || parts[1].len() != 2
        || parts.get(2).is_some_and(|day| day.len() != 2)
    {
        return Err(CatalogError::Date(field));
    }
    let year = parts[0]
        .parse::<u32>()
        .map_err(|_| CatalogError::Date(field))?;
    let month = parts[1]
        .parse::<u32>()
        .map_err(|_| CatalogError::Date(field))?;
    if !(1..=12).contains(&month) {
        return Err(CatalogError::Date(field));
    }
    if let Some(day) = parts.get(2) {
        let day = day.parse::<u32>().map_err(|_| CatalogError::Date(field))?;
        let leap =
            year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400));
        let maximum = [
            31,
            if leap { 29 } else { 28 },
            31,
            30,
            31,
            30,
            31,
            31,
            30,
            31,
            30,
            31,
        ][(month - 1) as usize];
        if day == 0 || day > maximum {
            return Err(CatalogError::Date(field));
        }
    }
    Ok(())
}
