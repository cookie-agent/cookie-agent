use std::sync::OnceLock;

use cookie_agent_identity::RecipeRegistryRevision;
use serde::Serialize;
use sha2::{Digest as _, Sha256};

use crate::{
    catalog::{CatalogModelProviderClaims, CatalogModelStatus},
    recipes::{
        CatalogClaim, CatalogModelClaimInput, CatalogProviderClaimInput, ClaimPresence,
        EndpointPolicy, ModelRecipeMatch, ProviderRecipeMatch, RecipeQuarantineReason,
        SetupFieldRecipe, SetupFieldType, SetupRecipe,
        claims::{absent_model_provider_drift, exact_model_provider_drift, provider_claim_drift},
    },
};

pub const COMPILER_VERSION: &str = "registry1-compiler-v1";

const EMPTY_SETUP: SetupRecipe = SetupRecipe {
    id: "no-setup-v1",
    fields: &[],
};
const VERTEX_FIELDS: &[SetupFieldRecipe] = &[
    SetupFieldRecipe {
        id: "location",
        value_type: SetupFieldType::String,
        required: true,
        default: None,
        environment_alias: Some("GOOGLE_VERTEX_LOCATION"),
    },
    SetupFieldRecipe {
        id: "project",
        value_type: SetupFieldType::String,
        required: true,
        default: None,
        environment_alias: Some("GOOGLE_VERTEX_PROJECT"),
    },
    SetupFieldRecipe {
        id: "resource",
        value_type: SetupFieldType::String,
        required: false,
        default: Some("publishers/google"),
        environment_alias: None,
    },
];
const VERTEX_SETUP: SetupRecipe = SetupRecipe {
    id: "google-vertex-setup-v1",
    fields: VERTEX_FIELDS,
};
const BEDROCK_FIELDS: &[SetupFieldRecipe] = &[SetupFieldRecipe {
    id: "region",
    value_type: SetupFieldType::String,
    required: true,
    default: None,
    environment_alias: Some("AWS_REGION"),
}];
const BEDROCK_SETUP: SetupRecipe = SetupRecipe {
    id: "amazon-bedrock-setup-v1",
    fields: BEDROCK_FIELDS,
};
const AZURE_FIELDS: &[SetupFieldRecipe] = &[
    SetupFieldRecipe {
        id: "api_version",
        value_type: SetupFieldType::String,
        required: true,
        default: None,
        environment_alias: None,
    },
    SetupFieldRecipe {
        id: "deployment",
        value_type: SetupFieldType::String,
        required: true,
        default: None,
        environment_alias: None,
    },
    SetupFieldRecipe {
        id: "resource_name",
        value_type: SetupFieldType::String,
        required: true,
        default: None,
        environment_alias: Some("AZURE_RESOURCE_NAME"),
    },
];
const AZURE_SETUP: SetupRecipe = SetupRecipe {
    id: "azure-openai-setup-v1",
    fields: AZURE_FIELDS,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct ProviderRecipe {
    pub id: &'static str,
    pub provider_id: &'static str,
    pub protocol_recipe: &'static str,
    pub adapter_id: &'static str,
    pub claim: CatalogClaim,
    pub endpoint: EndpointPolicy,
    pub setup: &'static SetupRecipe,
    pub default_auth_method: &'static str,
    pub allowed_auth_methods: &'static [&'static str],
    pub protocol_owned_headers: &'static [&'static str],
}

impl ProviderRecipe {
    /// Environment aliases that may be imported for one semantic credential.
    /// An empty slice means the catalog claim is intentionally not consumed.
    #[must_use]
    pub fn credential_environment_aliases(self, credential: &str) -> &'static [&'static str] {
        match (self.provider_id, credential) {
            ("anthropic", "api_key") => &["ANTHROPIC_API_KEY"],
            ("openai", "api_key") => &["OPENAI_API_KEY"],
            ("openrouter", "api_key") => &["OPENROUTER_API_KEY"],
            ("google", "api_key") => &[
                "GOOGLE_API_KEY",
                "GOOGLE_GENERATIVE_AI_API_KEY",
                "GEMINI_API_KEY",
            ],
            ("cohere", "api_key") => &["COHERE_API_KEY"],
            ("groq", "api_key") => &["GROQ_API_KEY"],
            ("togetherai", "api_key") => &["TOGETHER_API_KEY"],
            ("deepinfra", "api_key") => &["DEEPINFRA_API_KEY"],
            ("fireworks-ai", "api_key") => &["FIREWORKS_API_KEY"],
            ("amazon-bedrock", "access_key_id") => &["AWS_ACCESS_KEY_ID"],
            ("amazon-bedrock", "secret_access_key") => &["AWS_SECRET_ACCESS_KEY"],
            ("azure", "api_key") => &["AZURE_API_KEY"],
            _ => &[],
        }
    }
}

const OPENAI_ENV: &[&str] = &["OPENAI_API_KEY"];
macro_rules! recipe {
    ($id:expr, $provider_id:expr, $protocol:expr, $adapter:expr, $npm:expr, $api:expr, $env:expr, $default:expr, $auth:expr, $allowed:expr, $headers:expr $(,)?) => {
        ProviderRecipe {
            id: $id,
            provider_id: $provider_id,
            protocol_recipe: $protocol,
            adapter_id: $adapter,
            claim: CatalogClaim {
                npm: ClaimPresence::PresentExact($npm),
                api: $api,
                environment: $env,
                shape: ClaimPresence::Absent,
            },
            endpoint: EndpointPolicy::DefaultWithAuthoredHttpsOverride { default: $default },
            setup: &EMPTY_SETUP,
            default_auth_method: $auth,
            allowed_auth_methods: $allowed,
            protocol_owned_headers: $headers,
        }
    };
}
macro_rules! cloud_recipe {
    ($id:expr, $provider_id:expr, $protocol:expr, $adapter:expr, $npm:expr, $env:expr, $endpoint:expr, $setup:expr, $auth:expr, $allowed:expr $(,)?) => {
        ProviderRecipe {
            id: $id,
            provider_id: $provider_id,
            protocol_recipe: $protocol,
            adapter_id: $adapter,
            claim: CatalogClaim {
                npm: ClaimPresence::PresentExact($npm),
                api: ClaimPresence::Absent,
                environment: $env,
                shape: ClaimPresence::Absent,
            },
            endpoint: $endpoint,
            setup: $setup,
            default_auth_method: $auth,
            allowed_auth_methods: $allowed,
            protocol_owned_headers: &[],
        }
    };
}
const RECIPES: &[ProviderRecipe] = &[
    recipe!(
        "anthropic.messages.v1",
        "anthropic",
        "oven.anthropic.messages",
        "anthropic",
        "@ai-sdk/anthropic",
        ClaimPresence::Absent,
        &["ANTHROPIC_API_KEY"],
        "https://api.anthropic.com/v1",
        "anthropic-api-key-v1",
        &["anthropic-api-key-v1"],
        &["anthropic-version", "anthropic-beta"],
    ),
    recipe!(
        "openai.responses.v1",
        "openai",
        "oven.openai.responses",
        "openai-responses",
        "@ai-sdk/openai",
        ClaimPresence::Absent,
        OPENAI_ENV,
        "https://api.openai.com/v1",
        "bearer-api-key-v1",
        &["bearer-api-key-v1"],
        &[],
    ),
    recipe!(
        "openai.chat.v1",
        "openai",
        "oven.openai.chat",
        "openai-chat",
        "@ai-sdk/openai",
        ClaimPresence::Absent,
        OPENAI_ENV,
        "https://api.openai.com/v1",
        "bearer-api-key-v1",
        &["bearer-api-key-v1"],
        &[],
    ),
    recipe!(
        "openrouter.chat.v1",
        "openrouter",
        "oven.openai-compatible.chat",
        "openai-compatible",
        "@openrouter/ai-sdk-provider",
        ClaimPresence::PresentExact("https://openrouter.ai/api/v1"),
        &["OPENROUTER_API_KEY"],
        "https://openrouter.ai/api/v1",
        "bearer-api-key-v1",
        &["bearer-api-key-v1"],
        &[],
    ),
    recipe!(
        "google.gemini.v1",
        "google",
        "oven.google.gemini.generate-content",
        "google-gemini",
        "@ai-sdk/google",
        ClaimPresence::Absent,
        &[
            "GEMINI_API_KEY",
            "GOOGLE_API_KEY",
            "GOOGLE_GENERATIVE_AI_API_KEY",
        ],
        "https://generativelanguage.googleapis.com/v1beta",
        "google-api-key-header-v1",
        &["google-api-key-header-v1"],
        &[],
    ),
    recipe!(
        "cohere.chat.v2",
        "cohere",
        "oven.cohere.chat-v2",
        "cohere-v2-chat",
        "@ai-sdk/cohere",
        ClaimPresence::Absent,
        &["COHERE_API_KEY"],
        "https://api.cohere.com/v2",
        "bearer-api-key-v1",
        &["bearer-api-key-v1"],
        &[],
    ),
    recipe!(
        "compatible.groq.v1",
        "groq",
        "oven.openai-compatible.chat",
        "openai-compatible",
        "@ai-sdk/groq",
        ClaimPresence::Absent,
        &["GROQ_API_KEY"],
        "https://api.groq.com/openai/v1",
        "bearer-api-key-v1",
        &["bearer-api-key-v1"],
        &[],
    ),
    recipe!(
        "compatible.togetherai.v1",
        "togetherai",
        "oven.openai-compatible.chat",
        "openai-compatible",
        "@ai-sdk/togetherai",
        ClaimPresence::Absent,
        &["TOGETHER_API_KEY"],
        "https://api.together.xyz/v1",
        "bearer-api-key-v1",
        &["bearer-api-key-v1"],
        &[],
    ),
    recipe!(
        "compatible.deepinfra.v1",
        "deepinfra",
        "oven.openai-compatible.chat",
        "openai-compatible",
        "@ai-sdk/deepinfra",
        ClaimPresence::Absent,
        &["DEEPINFRA_API_KEY"],
        "https://api.deepinfra.com/v1/openai",
        "bearer-api-key-v1",
        &["bearer-api-key-v1"],
        &[],
    ),
    recipe!(
        "compatible.fireworks.v1",
        "fireworks-ai",
        "oven.openai-compatible.chat",
        "openai-compatible",
        "@ai-sdk/openai-compatible",
        ClaimPresence::PresentExact("https://api.fireworks.ai/inference/v1/"),
        &["FIREWORKS_API_KEY"],
        "https://api.fireworks.ai/inference/v1",
        "bearer-api-key-v1",
        &["bearer-api-key-v1"],
        &[],
    ),
    cloud_recipe!(
        "google.vertex.gemini.v1",
        "google-vertex",
        "oven.google.vertex.generate-content",
        "google-vertex-gemini",
        "@ai-sdk/google-vertex",
        &[
            "GOOGLE_APPLICATION_CREDENTIALS",
            "GOOGLE_VERTEX_LOCATION",
            "GOOGLE_VERTEX_PROJECT",
        ],
        EndpointPolicy::VertexPublisher,
        &VERTEX_SETUP,
        "oauth-access-token-v1",
        &["oauth-access-token-v1"],
    ),
    cloud_recipe!(
        "amazon.bedrock.converse.v1",
        "amazon-bedrock",
        "oven.bedrock.converse",
        "aws-bedrock-converse",
        "@ai-sdk/amazon-bedrock",
        &[
            "AWS_ACCESS_KEY_ID",
            "AWS_BEARER_TOKEN_BEDROCK",
            "AWS_REGION",
            "AWS_SECRET_ACCESS_KEY",
        ],
        EndpointPolicy::BedrockRegional,
        &BEDROCK_SETUP,
        "aws-sigv4-credentials-v1",
        &["aws-sigv4-credentials-v1"],
    ),
    cloud_recipe!(
        "azure.openai.v1",
        "azure",
        "oven.azure.openai.chat",
        "azure-openai-chat",
        "@ai-sdk/azure",
        &["AZURE_API_KEY", "AZURE_RESOURCE_NAME"],
        EndpointPolicy::AzureOpenai,
        &AZURE_SETUP,
        "azure-api-key-v1",
        &["azure-api-key-v1"],
    ),
];

#[derive(Clone, Copy, Debug, Default)]
pub struct RecipeRegistry;

#[must_use]
pub const fn registry1() -> RecipeRegistry {
    RecipeRegistry
}

impl RecipeRegistry {
    #[must_use]
    pub const fn schema_version(self) -> u32 {
        1
    }

    #[must_use]
    pub fn revision(self) -> RecipeRegistryRevision {
        static REVISION: OnceLock<RecipeRegistryRevision> = OnceLock::new();
        REVISION
            .get_or_init(|| {
                let auth_methods = crate::recipes::auth_methods().collect::<Vec<_>>();
                let bytes = serde_json::to_vec(&(RECIPES, auth_methods))
                    .expect("static registry serializes");
                let mut digest = Sha256::new();
                digest.update(b"cookie-agent/recipe-registry/v1\0");
                digest.update(bytes);
                RecipeRegistryRevision::new(format!("sha256:{:x}", digest.finalize()))
                    .expect("registry digest is valid")
            })
            .clone()
    }

    pub fn recipes(self) -> impl ExactSizeIterator<Item = &'static ProviderRecipe> {
        RECIPES.iter()
    }

    #[must_use]
    pub fn provider_recipes(self, provider_id: &str) -> Vec<&'static ProviderRecipe> {
        RECIPES
            .iter()
            .filter(|recipe| recipe.provider_id == provider_id)
            .collect()
    }

    #[must_use]
    pub fn recipe(self, id: &str) -> Option<&'static ProviderRecipe> {
        RECIPES.iter().find(|recipe| recipe.id == id)
    }

    #[must_use]
    pub fn match_provider<'a>(
        self,
        input: &CatalogProviderClaimInput<'_>,
    ) -> ProviderRecipeMatch<'a> {
        let recipes = self.provider_recipes(input.id);
        let Some(first) = recipes.first().copied() else {
            return ProviderRecipeMatch::Unsupported(
                RecipeQuarantineReason::NoReviewedProviderRecipe,
            );
        };
        match provider_claim_drift(first.provider_id, first.claim, input) {
            Some(reason) => ProviderRecipeMatch::Quarantined(reason),
            None => ProviderRecipeMatch::Supported(first),
        }
    }

    #[must_use]
    pub fn match_model<'a>(
        self,
        provider_id: &str,
        input: &CatalogModelClaimInput<'_>,
    ) -> ModelRecipeMatch<'a> {
        if input.record.shape.is_some() {
            return ModelRecipeMatch::Quarantined(RecipeQuarantineReason::CatalogModelShapeDrift);
        }
        let record = input.record;
        if input.table_key != record.id.as_str() {
            return ModelRecipeMatch::Quarantined(
                RecipeQuarantineReason::UnsupportedModelCapabilities,
            );
        }
        if record.status == CatalogModelStatus::Deprecated
            || !record.modalities.output.iter().any(|value| value == "text")
        {
            return ModelRecipeMatch::Omitted;
        }
        match provider_id {
            "openai" => match route_openai_model(record.id.as_str()) {
                Ok("responses") => self.supported("openai.responses.v1"),
                Ok("chat") => self.supported("openai.chat.v1"),
                Err(reason) => ModelRecipeMatch::Quarantined(reason),
                Ok(_) => unreachable!(),
            },
            "azure" => self.match_azure(input),
            "google" => {
                if let Some(reason) = absent_model_provider_drift(record.provider.as_ref()) {
                    ModelRecipeMatch::Quarantined(reason)
                } else if record.id.as_str().starts_with("gemini-") {
                    self.supported("google.gemini.v1")
                } else {
                    ModelRecipeMatch::Quarantined(
                        RecipeQuarantineReason::UnsupportedModelCapabilities,
                    )
                }
            }
            "google-vertex" => self.match_vertex(input),
            "amazon-bedrock" => self.match_bedrock(input),
            "cohere" => self.match_cohere(input),
            _ => {
                if let Some(reason) = absent_model_provider_drift(record.provider.as_ref()) {
                    ModelRecipeMatch::Quarantined(reason)
                } else if let Some(recipe) = self.provider_recipes(provider_id).first() {
                    ModelRecipeMatch::Supported(recipe)
                } else {
                    ModelRecipeMatch::Quarantined(RecipeQuarantineReason::NoReviewedProviderRecipe)
                }
            }
        }
    }

    fn supported<'a>(self, id: &str) -> ModelRecipeMatch<'a> {
        ModelRecipeMatch::Supported(self.recipe(id).expect("static recipe id"))
    }

    fn match_cohere<'a>(self, input: &CatalogModelClaimInput<'_>) -> ModelRecipeMatch<'a> {
        let record = input.record;
        if record.id.as_str() == "north-mini-code-1-0" {
            let Some(provider) = record.provider.as_ref() else {
                return ModelRecipeMatch::Quarantined(
                    RecipeQuarantineReason::CatalogModelProviderNpmDrift,
                );
            };
            if let Some(reason) = exact_model_provider_drift(
                provider,
                ClaimPresence::PresentExact("@ai-sdk/openai-compatible"),
                ClaimPresence::PresentExact("https://api.cohere.ai/compatibility/v1"),
                ClaimPresence::Absent,
            ) {
                return ModelRecipeMatch::Quarantined(reason);
            }
            return self.supported("cohere.chat.v2");
        }
        if let Some(reason) = absent_model_provider_drift(record.provider.as_ref()) {
            ModelRecipeMatch::Quarantined(reason)
        } else {
            self.supported("cohere.chat.v2")
        }
    }

    fn match_vertex<'a>(self, input: &CatalogModelClaimInput<'_>) -> ModelRecipeMatch<'a> {
        let record = input.record;
        if let Some(provider) = record.provider.as_ref() {
            return match known_vertex_override(provider) {
                Ok(()) => ModelRecipeMatch::Quarantined(
                    RecipeQuarantineReason::UnsupportedProtocolFeature,
                ),
                Err(reason) => ModelRecipeMatch::Quarantined(reason),
            };
        }
        let id = record.id.as_str();
        let id_valid = id.starts_with("gemini-")
            && !id.starts_with("gemini-embedding-")
            && id != "gemini-embedding-001"
            && !id.contains('/')
            && id.bytes().all(|byte| {
                byte.is_ascii_lowercase()
                    || byte.is_ascii_digit()
                    || matches!(byte, b'.' | b'_' | b'-')
            });
        let family_valid = matches!(
            record.family.as_deref(),
            Some("gemini-flash" | "gemini-flash-lite" | "gemini-pro")
        );
        let shape_valid = record.modalities.input.iter().any(|value| value == "text")
            && record.modalities.output.iter().any(|value| value == "text")
            && record.limits.context > 0
            && record.limits.output > 0;
        if id_valid && family_valid && shape_valid {
            self.supported("google.vertex.gemini.v1")
        } else {
            ModelRecipeMatch::Quarantined(RecipeQuarantineReason::UnsupportedVertexModelFamily)
        }
    }

    fn match_bedrock<'a>(self, input: &CatalogModelClaimInput<'_>) -> ModelRecipeMatch<'a> {
        let Some(provider) = input.record.provider.as_ref() else {
            return self.supported("amazon.bedrock.converse.v1");
        };
        match exact_model_provider_drift(
            provider,
            ClaimPresence::PresentExact("@ai-sdk/amazon-bedrock/mantle"),
            ClaimPresence::PresentOneOf(&[
                "https://bedrock-mantle.${AWS_REGION}.api.aws/openai/v1",
                "https://bedrock-mantle.${AWS_REGION}.api.aws/v1",
            ]),
            ClaimPresence::PresentExact("responses"),
        ) {
            None => {
                ModelRecipeMatch::Quarantined(RecipeQuarantineReason::UnsupportedProtocolFeature)
            }
            Some(reason) => ModelRecipeMatch::Quarantined(reason),
        }
    }

    fn match_azure<'a>(self, input: &CatalogModelClaimInput<'_>) -> ModelRecipeMatch<'a> {
        let record = input.record;
        if let Some(provider) = record.provider.as_ref() {
            return match known_azure_override(provider) {
                Ok(()) => ModelRecipeMatch::Quarantined(
                    RecipeQuarantineReason::UnsupportedProtocolFeature,
                ),
                Err(reason) => ModelRecipeMatch::Quarantined(reason),
            };
        }
        match route_openai_model(record.id.as_str()) {
            Ok("responses" | "chat") => self.supported("azure.openai.v1"),
            Err(reason) => ModelRecipeMatch::Quarantined(reason),
            Ok(_) => unreachable!(),
        }
    }
}

fn known_vertex_override(
    provider: &CatalogModelProviderClaims,
) -> Result<(), RecipeQuarantineReason> {
    if provider.npm.as_deref() == Some("@ai-sdk/google-vertex/anthropic") {
        return exact_model_provider_drift(
            provider,
            ClaimPresence::PresentExact("@ai-sdk/google-vertex/anthropic"),
            ClaimPresence::Absent,
            ClaimPresence::Absent,
        )
        .map_or(Ok(()), Err);
    }
    exact_model_provider_drift(
        provider,
        ClaimPresence::PresentExact("@ai-sdk/openai-compatible"),
        ClaimPresence::PresentExact("https://${GOOGLE_VERTEX_ENDPOINT}/v1/projects/${GOOGLE_VERTEX_PROJECT}/locations/${GOOGLE_VERTEX_LOCATION}/endpoints/openapi"),
        ClaimPresence::Absent,
    )
    .map_or(Ok(()), Err)
}

fn known_azure_override(
    provider: &CatalogModelProviderClaims,
) -> Result<(), RecipeQuarantineReason> {
    if provider.npm.as_deref() == Some("@ai-sdk/anthropic") {
        return exact_model_provider_drift(
            provider,
            ClaimPresence::PresentExact("@ai-sdk/anthropic"),
            ClaimPresence::PresentExact(
                "https://${AZURE_RESOURCE_NAME}.services.ai.azure.com/anthropic/v1",
            ),
            ClaimPresence::Absent,
        )
        .map_or(Ok(()), Err);
    }
    exact_model_provider_drift(
        provider,
        ClaimPresence::PresentExact("@ai-sdk/openai-compatible"),
        ClaimPresence::PresentExact("https://${AZURE_RESOURCE_NAME}.services.ai.azure.com/models"),
        ClaimPresence::PresentExact("completions"),
    )
    .map_or(Ok(()), Err)
}

pub fn route_openai_model(id: &str) -> Result<&'static str, RecipeQuarantineReason> {
    const RESPONSES: &[&str] = &["gpt-5", "o1", "o3", "o4"];
    const CHAT: &[&str] = &["gpt-4.1", "gpt-4o", "gpt-4-turbo", "gpt-3.5-turbo"];
    let responses = RESPONSES
        .iter()
        .filter(|root| {
            id == **root
                || id
                    .strip_prefix(**root)
                    .is_some_and(|tail| tail.starts_with('-'))
        })
        .count();
    let chat = CHAT
        .iter()
        .filter(|root| {
            id == **root
                || id
                    .strip_prefix(**root)
                    .is_some_and(|tail| tail.starts_with('-'))
        })
        .count();
    match (responses, chat) {
        (1, 0) => Ok("responses"),
        (0, 1) => Ok("chat"),
        (0, 0) => Err(RecipeQuarantineReason::UnreviewedOpenaiModelFamily),
        _ => Err(RecipeQuarantineReason::AmbiguousOpenaiModelFamily),
    }
}
