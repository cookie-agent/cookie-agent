use std::sync::OnceLock;

use cookie_agent_identity::RecipeRegistryRevision;
use serde::Serialize;
use sha2::{Digest as _, Sha256};

use crate::{
    adapters::OvenAdapterFamily,
    authoring::ManagedModelShape,
    catalog::{CatalogModelRecord, CatalogProviderRecord},
};

pub const COMPILER_VERSION: &str = "family-registry-compiler-v1";

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum FamilyKind {
    OpenAiCompatibleChat,
    Anthropic,
    OpenAi,
    Google,
    Vertex,
    VertexAnthropic,
    Bedrock,
    Azure,
    Cohere,
}

impl FamilyKind {
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::OpenAiCompatibleChat => "openai-compatible-chat",
            Self::Anthropic => "anthropic",
            Self::OpenAi => "openai",
            Self::Google => "google",
            Self::Vertex => "vertex",
            Self::VertexAnthropic => "vertex-anthropic",
            Self::Bedrock => "bedrock",
            Self::Azure => "azure",
            Self::Cohere => "cohere",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct FamilyRecipe {
    pub npm: &'static str,
    pub family: FamilyKind,
    pub default_endpoint: Option<&'static str>,
    pub default_auth_method: &'static str,
    pub allowed_auth_methods: &'static [&'static str],
}

const RECIPES: &[FamilyRecipe] = &[
    compatible("@ai-sdk/openai-compatible", None),
    compatible("@ai-sdk/groq", Some("https://api.groq.com/openai/v1")),
    compatible("@ai-sdk/mistral", Some("https://api.mistral.ai/v1")),
    compatible("@ai-sdk/xai", Some("https://api.x.ai/v1")),
    compatible("@ai-sdk/cerebras", Some("https://api.cerebras.ai/v1")),
    compatible("@ai-sdk/togetherai", Some("https://api.together.xyz/v1")),
    compatible(
        "@ai-sdk/deepinfra",
        Some("https://api.deepinfra.com/v1/openai"),
    ),
    compatible("@ai-sdk/perplexity", Some("https://api.perplexity.ai")),
    compatible(
        "venice-ai-sdk-provider",
        Some("https://api.venice.ai/api/v1"),
    ),
    compatible(
        "@openrouter/ai-sdk-provider",
        Some("https://openrouter.ai/api/v1"),
    ),
    compatible("@qvac/ai-sdk-provider", None),
    family(
        "@ai-sdk/anthropic",
        FamilyKind::Anthropic,
        Some("https://api.anthropic.com/v1"),
        "anthropic-api-key-v1",
        &["anthropic-api-key-v1", "bearer-api-key-v1"],
    ),
    family(
        "@ai-sdk/openai",
        FamilyKind::OpenAi,
        Some("https://api.openai.com/v1"),
        "bearer-api-key-v1",
        &["bearer-api-key-v1"],
    ),
    family(
        "@ai-sdk/google",
        FamilyKind::Google,
        Some("https://generativelanguage.googleapis.com/v1beta"),
        "google-api-key-header-v1",
        &["google-api-key-header-v1"],
    ),
    family(
        "@ai-sdk/google-vertex",
        FamilyKind::Vertex,
        None,
        "oauth-access-token-v1",
        &["oauth-access-token-v1"],
    ),
    family(
        "@ai-sdk/google-vertex/anthropic",
        FamilyKind::VertexAnthropic,
        None,
        "oauth-access-token-v1",
        &["oauth-access-token-v1"],
    ),
    family(
        "@ai-sdk/amazon-bedrock",
        FamilyKind::Bedrock,
        Some("https://bedrock-runtime.${AWS_REGION}.amazonaws.com"),
        "aws-sigv4-credentials-v1",
        &["aws-sigv4-credentials-v1", "bearer-api-key-v1"],
    ),
    family(
        "@ai-sdk/amazon-bedrock/mantle",
        FamilyKind::Bedrock,
        None,
        "bearer-api-key-v1",
        &["bearer-api-key-v1"],
    ),
    family(
        "@ai-sdk/azure",
        FamilyKind::Azure,
        Some("https://${AZURE_RESOURCE_NAME}.openai.azure.com"),
        "azure-api-key-v1",
        &["azure-api-key-v1", "bearer-api-key-v1"],
    ),
    family(
        "@ai-sdk/cohere",
        FamilyKind::Cohere,
        Some("https://api.cohere.com/v2/chat"),
        "bearer-api-key-v1",
        &["bearer-api-key-v1"],
    ),
];

const fn compatible(npm: &'static str, endpoint: Option<&'static str>) -> FamilyRecipe {
    family(
        npm,
        FamilyKind::OpenAiCompatibleChat,
        endpoint,
        "bearer-api-key-v1",
        &["bearer-api-key-v1"],
    )
}

const fn family(
    npm: &'static str,
    kind: FamilyKind,
    endpoint: Option<&'static str>,
    default_auth_method: &'static str,
    allowed_auth_methods: &'static [&'static str],
) -> FamilyRecipe {
    FamilyRecipe {
        npm,
        family: kind,
        default_endpoint: endpoint,
        default_auth_method,
        allowed_auth_methods,
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct FamilyRecipeRegistry;

#[must_use]
pub const fn family_registry() -> FamilyRecipeRegistry {
    FamilyRecipeRegistry
}

impl FamilyRecipeRegistry {
    pub fn recipes(self) -> impl ExactSizeIterator<Item = &'static FamilyRecipe> {
        RECIPES.iter()
    }

    #[must_use]
    pub fn by_npm(self, npm: &str) -> Option<&'static FamilyRecipe> {
        RECIPES.iter().find(|recipe| recipe.npm == npm)
    }

    #[must_use]
    pub fn classify(self, provider: &CatalogProviderRecord) -> Option<&'static FamilyRecipe> {
        self.by_npm(&provider.npm)
    }

    #[must_use]
    pub fn revision(self) -> RecipeRegistryRevision {
        static REVISION: OnceLock<RecipeRegistryRevision> = OnceLock::new();
        REVISION
            .get_or_init(|| {
                let bytes = serde_json::to_vec(&(
                    RECIPES,
                    crate::recipes::auth_methods().collect::<Vec<_>>(),
                ))
                .expect("static family registry serializes");
                let mut digest = Sha256::new();
                digest.update(b"cookie-agent/family-recipe-registry/v1\0");
                digest.update(bytes);
                RecipeRegistryRevision::new(format!("sha256:{:x}", digest.finalize()))
                    .expect("registry digest is valid")
            })
            .clone()
    }
}

/// Maps one provider-level credential method onto the wire auth expected by an
/// effective model family. Only equivalent credential semantics are mapped:
/// single API keys may change header/bearer encoding, and an explicit access
/// token may become bearer auth. AWS static credentials never cross families.
#[must_use]
pub fn compatible_auth_method(
    source_method: &str,
    effective: &FamilyRecipe,
) -> Option<&'static str> {
    if let Some(method) = effective
        .allowed_auth_methods
        .iter()
        .copied()
        .find(|method| *method == source_method)
    {
        return Some(method);
    }
    let source_is_api_key = matches!(
        source_method,
        "anthropic-api-key-v1"
            | "azure-api-key-v1"
            | "google-api-key-header-v1"
            | "bearer-api-key-v1"
    );
    let source_is_access_token = source_method == "oauth-access-token-v1";
    if !source_is_api_key && !source_is_access_token {
        return None;
    }
    let preferred = match effective.family {
        FamilyKind::Anthropic => {
            // `@ai-sdk/anthropic` sends configured API keys as `x-api-key`, and
            // Microsoft Foundry accepts `x-api-key` on its Anthropic endpoint.
            if source_method == "bearer-api-key-v1" || source_is_access_token {
                "bearer-api-key-v1"
            } else {
                "anthropic-api-key-v1"
            }
        }
        FamilyKind::OpenAi | FamilyKind::OpenAiCompatibleChat | FamilyKind::Cohere => {
            "bearer-api-key-v1"
        }
        FamilyKind::Google => "google-api-key-header-v1",
        FamilyKind::Azure => {
            if source_method == "bearer-api-key-v1" || source_is_access_token {
                "bearer-api-key-v1"
            } else {
                "azure-api-key-v1"
            }
        }
        FamilyKind::Vertex | FamilyKind::VertexAnthropic => return None,
        FamilyKind::Bedrock => return None,
    };
    effective
        .allowed_auth_methods
        .contains(&preferred)
        .then_some(preferred)
}

#[must_use]
pub fn compatible_credential_field(
    source_method: &str,
    target_field: &str,
) -> Option<&'static str> {
    match target_field {
        "api_key" if source_method == "oauth-access-token-v1" => Some("access_token"),
        "api_key" => Some("api_key"),
        "access_token" if source_method == "bearer-api-key-v1" => Some("api_key"),
        "access_token" => Some("access_token"),
        "access_key_id" if source_method == "aws-sigv4-credentials-v1" => Some("access_key_id"),
        "secret_access_key" if source_method == "aws-sigv4-credentials-v1" => {
            Some("secret_access_key")
        }
        "session_token" if source_method == "aws-sigv4-credentials-v1" => Some("session_token"),
        _ => None,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ResolvedShape {
    Chat,
    Responses,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedFamilyModel {
    pub recipe: &'static FamilyRecipe,
    pub npm: String,
    pub endpoint_template: Option<String>,
    pub shape: ResolvedShape,
    pub adapter: OvenAdapterFamily,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum FamilyResolutionError {
    #[error("no_known_protocol_family")]
    UnknownFamily,
    #[error("unsupported_model_shape")]
    UnsupportedShape,
}

pub fn resolve_model(
    provider: &CatalogProviderRecord,
    model: &CatalogModelRecord,
    provider_shape: Option<ManagedModelShape>,
    model_shape: Option<ManagedModelShape>,
) -> Result<ResolvedFamilyModel, FamilyResolutionError> {
    let override_provider = model.provider.as_ref();
    let npm = override_provider
        .and_then(|value| value.npm.as_deref())
        .unwrap_or(&provider.npm);
    let recipe = family_registry()
        .by_npm(npm)
        .ok_or(FamilyResolutionError::UnknownFamily)?;
    let endpoint_template = override_provider
        .and_then(|value| value.api.clone())
        .or_else(|| provider.api.clone())
        .or_else(|| recipe.default_endpoint.map(str::to_owned));
    let catalog_shape = override_provider
        .and_then(|value| value.shape.as_deref())
        .or(model.shape.as_deref())
        .or(provider.shape.as_deref());
    let catalog_shape = catalog_shape
        .map(|shape| match shape {
            "responses" => Ok(ResolvedShape::Responses),
            "completions" | "chat" => Ok(ResolvedShape::Chat),
            _ => Err(FamilyResolutionError::UnsupportedShape),
        })
        .transpose()?;
    let shape = model_shape
        .or(provider_shape)
        .map(|shape| match shape {
            ManagedModelShape::Chat => ResolvedShape::Chat,
            ManagedModelShape::Responses => ResolvedShape::Responses,
        })
        .or(catalog_shape)
        .unwrap_or_else(|| {
            if recipe.family == FamilyKind::OpenAi {
                ResolvedShape::Responses
            } else {
                ResolvedShape::Chat
            }
        });
    let adapter = adapter(recipe.family, shape)?;
    Ok(ResolvedFamilyModel {
        recipe,
        npm: npm.to_owned(),
        endpoint_template,
        shape,
        adapter,
    })
}

fn adapter(
    kind: FamilyKind,
    shape: ResolvedShape,
) -> Result<OvenAdapterFamily, FamilyResolutionError> {
    Ok(match kind {
        FamilyKind::OpenAiCompatibleChat => match shape {
            ResolvedShape::Chat => OvenAdapterFamily::OpenaiCompatible,
            ResolvedShape::Responses => OvenAdapterFamily::OpenaiResponses,
        },
        FamilyKind::Anthropic => OvenAdapterFamily::AnthropicCompatible,
        FamilyKind::OpenAi => match shape {
            ResolvedShape::Chat => OvenAdapterFamily::OpenaiChat,
            ResolvedShape::Responses => OvenAdapterFamily::OpenaiResponses,
        },
        FamilyKind::Google => OvenAdapterFamily::GoogleGemini,
        FamilyKind::Vertex | FamilyKind::VertexAnthropic => OvenAdapterFamily::GoogleVertexGemini,
        FamilyKind::Bedrock => match shape {
            ResolvedShape::Chat => OvenAdapterFamily::AwsBedrockConverse,
            ResolvedShape::Responses => OvenAdapterFamily::OpenaiResponses,
        },
        FamilyKind::Azure => match shape {
            ResolvedShape::Chat => OvenAdapterFamily::AzureOpenaiChat,
            ResolvedShape::Responses => OvenAdapterFamily::AzureOpenaiResponses,
        },
        FamilyKind::Cohere => OvenAdapterFamily::CohereV2Chat,
    })
}

#[must_use]
pub fn environment_aliases<'a>(
    provider: &'a CatalogProviderRecord,
    credential: &str,
) -> Vec<&'a str> {
    match credential {
        "api_key" | "access_token" => provider
            .environment
            .first()
            .map(String::as_str)
            .into_iter()
            .collect(),
        "access_key_id" => provider
            .environment
            .iter()
            .find(|value| value.as_str() == "AWS_ACCESS_KEY_ID")
            .map(String::as_str)
            .into_iter()
            .collect(),
        "secret_access_key" => provider
            .environment
            .iter()
            .find(|value| value.as_str() == "AWS_SECRET_ACCESS_KEY")
            .map(String::as_str)
            .into_iter()
            .collect(),
        "session_token" => provider
            .environment
            .iter()
            .find(|value| value.as_str() == "AWS_SESSION_TOKEN")
            .map(String::as_str)
            .into_iter()
            .collect(),
        _ => Vec::new(),
    }
}

#[must_use]
pub fn placeholders(template: &str) -> Vec<String> {
    let mut result = Vec::new();
    let mut rest = template;
    while let Some(start) = rest.find("${") {
        rest = &rest[start + 2..];
        let Some(end) = rest.find('}') else { break };
        let name = &rest[..end];
        if !name.is_empty()
            && name
                .bytes()
                .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
            && !result.iter().any(|value| value == name)
        {
            result.push(name.to_owned());
        }
        rest = &rest[end + 1..];
    }
    result.sort();
    result
}

pub fn substitute_placeholders(
    template: &str,
    values: &std::collections::BTreeMap<String, String>,
) -> Option<String> {
    let mut endpoint = template.to_owned();
    for name in placeholders(template) {
        let field = setup_field_name(&name);
        let value = values.get(&name).or_else(|| values.get(&field))?;
        endpoint = endpoint.replace(&format!("${{{name}}}"), value);
    }
    Some(endpoint)
}

#[must_use]
pub fn setup_field_name(name: &str) -> String {
    match name {
        "AWS_REGION" => "region".to_owned(),
        "AZURE_RESOURCE_NAME" => "resource_name".to_owned(),
        "GOOGLE_VERTEX_PROJECT" => "project".to_owned(),
        "GOOGLE_VERTEX_LOCATION" => "location".to_owned(),
        "GOOGLE_VERTEX_ENDPOINT" => "endpoint".to_owned(),
        _ => name.to_ascii_lowercase(),
    }
}
