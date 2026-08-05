use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    time::Duration,
};

use http::{HeaderMap, HeaderName, HeaderValue};
use oven_sdk::{
    AdapterId, ApiEndpoint, HeaderConfig, HeaderOverrides, HeaderProvider, LanguageModel,
    ModelCapabilities, ModelConfig, ModelDeclaration, ModelError, ModelId, ProviderConfig,
    ProviderId, ResourceId, SecretString,
};
use oven_sdk_anthropic::{
    AnthropicAuth, AnthropicCacheControl, AnthropicCacheTtl, AnthropicModel,
    AnthropicProtocolSettings, AnthropicRequestOptions, AnthropicSettings, AnthropicThinking,
    AnthropicThinkingSupport, AnthropicTimeouts,
};
use oven_sdk_azure::{
    AzureApiRoute, AzureApiVersion, AzureMaxTokensField, AzureOpenAiAuth, AzureOpenAiChatModel,
    AzureOpenAiChatOptions, AzureOpenAiChatSettings, AzureOpenAiCompletionsConfig,
    AzureOpenAiResponsesCompaction, AzureOpenAiResponsesModel, AzureOpenAiResponsesOptions,
    AzureOpenAiResponsesSettings, AzureOpenAiRevision, AzureOpenAiTimeouts, AzureReasoningField,
    AzureStructuredOutputSupport, AzureSystemMessageRole,
};
use oven_sdk_bedrock::{
    AwsCredentials, BedrockAuth, BedrockConverseSettings, BedrockEventStreamLimits,
    BedrockGuardrailConfig, BedrockModel, BedrockReasoningWireFormat, BedrockRequestOptions,
    BedrockS3LocationOptions, BedrockStructuredOutput, BedrockTimeouts,
};
use oven_sdk_cohere::{
    CohereAuth, CohereDocument, CohereModel, CohereRequestOptions, CohereSettings, CohereThinking,
    CohereTimeouts,
};
use oven_sdk_google::{
    GoogleApiKeyAuth, GoogleGenerateContentSettings, GoogleModel, GoogleProviderTool,
    GoogleRequestOptions, GoogleSafetySetting, GoogleThinkingConfig, GoogleThinkingSettings,
    GoogleTimeouts, GoogleToolSettings,
};
use oven_sdk_google_vertex::{
    GoogleVertexMediaSettings, GoogleVertexModel, GoogleVertexProviderTool,
    GoogleVertexRequestOptions, GoogleVertexResource, GoogleVertexSafetySetting,
    GoogleVertexSettings, GoogleVertexThinkingConfig, GoogleVertexThinkingMode,
    GoogleVertexTimeouts, GoogleVertexToolSettings, VertexAuth, google_vertex_native_context_scope,
};
use oven_sdk_openai::{
    CompatibleChatOptions, MaxTokensField, OpenAiAuth, OpenAiChatModel, OpenAiChatOptions,
    OpenAiChatSettings, OpenAiCompatibleAuth, OpenAiCompatibleChatModel,
    OpenAiCompatibleChatSettings, OpenAiResponsesCompaction, OpenAiResponsesModel,
    OpenAiResponsesOptions, OpenAiResponsesSettings, OpenAiTimeouts, ReasoningField,
    StructuredOutputSupport, SystemMessageRole,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use thiserror::Error;
use zeroize::Zeroize as _;

use crate::ConstructedAdapter;

/// Internal fully concrete declaration consumed by one reviewed Oven constructor.
#[derive(Clone, Serialize)]
pub struct ConcreteModel {
    /// Explicit serving-provider identity.
    pub provider_id: String,
    /// Exact model, deployment, or resource identifier.
    pub model_id: String,
    /// Exact base or full API endpoint required by the selected adapter.
    pub endpoint: String,
    /// Explicit resolved authentication. Oven never reads the environment.
    pub auth: AuthConfig,
    /// Static caller headers. Values may be interpolated by the config crate only.
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// Complete explicit Oven capabilities, limits, modalities, media, cancellation,
    /// native compaction, and replay.
    pub capabilities: ModelCapabilities,
    /// Common and provider request defaults.
    #[serde(default)]
    pub defaults: CommonDefaults,
    /// Concrete adapter and all structural/request settings.
    #[serde(flatten)]
    pub adapter: AdapterConfig,
}

impl std::fmt::Debug for ConcreteModel {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ConcreteModel")
            .field("provider_id", &self.provider_id)
            .field("model_id", &self.model_id)
            .field("endpoint", &"<redacted>")
            .field("auth", &"<redacted>")
            .field("header_names", &self.headers.keys().collect::<Vec<_>>())
            .field("capabilities", &self.capabilities)
            .field("defaults", &self.defaults)
            .field("adapter", &self.adapter)
            .finish()
    }
}

impl ConcreteModel {
    /// Constructs one exact concrete Oven model and immutable request defaults.
    pub fn build(&self) -> Result<ConstructedAdapter, ModelBuildError> {
        let provider = self.provider()?;
        let declaration = ModelDeclaration::new(
            ModelId::new(self.model_id.clone()),
            self.capabilities.clone(),
        )?;
        let (model, provider_options): (Arc<dyn LanguageModel>, BTreeMap<String, Value>) =
            match &self.adapter {
                AdapterConfig::Anthropic { settings, options } => (
                    Arc::new(AnthropicModel::new(ModelConfig::new(
                        provider.with_auth(self.anthropic_auth()?),
                        declaration,
                        settings.to_oven()?,
                    ))?),
                    namespace("anthropic", options.to_oven())?,
                ),
                AdapterConfig::OpenaiChat { settings, options } => (
                    Arc::new(OpenAiChatModel::new(ModelConfig::new(
                        provider.with_auth(self.openai_auth()?),
                        declaration,
                        settings.to_openai_chat(),
                    ))?),
                    namespace("openai", json!({ "chat": options }))?,
                ),
                AdapterConfig::OpenaiResponses { settings, options } => (
                    Arc::new(OpenAiResponsesModel::new(ModelConfig::new(
                        provider.with_auth(self.openai_auth()?),
                        declaration,
                        settings.to_openai_responses(),
                    ))?),
                    namespace("openai", json!({ "responses": options }))?,
                ),
                AdapterConfig::OpenaiCompatible { settings, options } => (
                    Arc::new(OpenAiCompatibleChatModel::new(ModelConfig::new(
                        provider.with_auth(self.compatible_auth()?),
                        declaration,
                        settings.to_oven(),
                    ))?),
                    namespace(
                        "openai_compatible",
                        CompatibleChatOptions {
                            extra_body: options.extra_body.clone(),
                        },
                    )?,
                ),
                AdapterConfig::Google { settings, options } => (
                    Arc::new(GoogleModel::new(ModelConfig::new(
                        provider.with_auth(self.google_auth()?),
                        declaration,
                        settings.to_oven(),
                    ))?),
                    namespace("google", options.to_oven())?,
                ),
                AdapterConfig::Vertex { settings, options } => {
                    let provider = provider.with_auth(self.vertex_auth()?);
                    let settings = settings.to_oven(&provider, &declaration)?;
                    (
                        Arc::new(GoogleVertexModel::new(ModelConfig::new(
                            provider,
                            declaration,
                            settings,
                        ))?),
                        namespace("google_vertex", options.to_oven())?,
                    )
                }
                AdapterConfig::Bedrock { settings, options } => (
                    Arc::new(BedrockModel::new(ModelConfig::new(
                        provider.with_auth(self.bedrock_auth()?),
                        declaration,
                        settings.to_oven(),
                    ))?),
                    namespace("bedrock", options.to_oven())?,
                ),
                AdapterConfig::AzureChat { settings, options } => (
                    Arc::new(AzureOpenAiChatModel::new(ModelConfig::new(
                        provider.with_auth(self.azure_auth()?),
                        declaration,
                        settings.to_chat()?,
                    ))?),
                    namespace("azure_openai", json!({ "chat": options }))?,
                ),
                AdapterConfig::AzureResponses { settings, options } => (
                    Arc::new(AzureOpenAiResponsesModel::new(ModelConfig::new(
                        provider.with_auth(self.azure_auth()?),
                        declaration,
                        settings.to_responses()?,
                    ))?),
                    namespace("azure_openai", json!({ "responses": options }))?,
                ),
                AdapterConfig::Cohere { settings, options } => (
                    Arc::new(CohereModel::new(ModelConfig::new(
                        provider.with_auth(self.cohere_auth()?),
                        declaration,
                        settings.to_oven(),
                    ))?),
                    namespace("cohere", options.to_oven())?,
                ),
            };
        Ok(ConstructedAdapter {
            model,
            provider_options,
        })
    }

    fn provider(&self) -> Result<CommonProvider, ModelBuildError> {
        Ok(CommonProvider {
            id: ProviderId::new(self.provider_id.clone()),
            api: ApiEndpoint::parse(&self.endpoint)?,
            headers: header_config(&self.headers)?,
        })
    }

    fn anthropic_auth(&self) -> Result<AnthropicAuth, ModelBuildError> {
        match &self.auth {
            AuthConfig::None => Ok(AnthropicAuth::None),
            AuthConfig::ApiKey { value } => Ok(AnthropicAuth::ApiKey(secret(value))),
            _ => Err(wrong_auth("anthropic", "none or api_key")),
        }
    }

    fn openai_auth(&self) -> Result<OpenAiAuth, ModelBuildError> {
        match &self.auth {
            AuthConfig::Openai {
                api_key,
                organization,
                project,
            } => Ok(OpenAiAuth {
                api_key: secret(api_key),
                organization: organization.clone(),
                project: project.clone(),
            }),
            _ => Err(wrong_auth("OpenAI", "openai")),
        }
    }

    fn compatible_auth(&self) -> Result<OpenAiCompatibleAuth, ModelBuildError> {
        match &self.auth {
            AuthConfig::None => Ok(OpenAiCompatibleAuth::none()),
            AuthConfig::Bearer { token } => Ok(OpenAiCompatibleAuth::bearer(secret(token))),
            AuthConfig::HeaderApiKey { name, value } => Ok(OpenAiCompatibleAuth::headers(
                Arc::new(SecretHeaderProvider {
                    name: name.clone(),
                    value: secret(value),
                }),
            )),
            _ => Err(wrong_auth(
                "OpenAI-compatible",
                "none, bearer, or header_api_key",
            )),
        }
    }

    fn google_auth(&self) -> Result<GoogleApiKeyAuth, ModelBuildError> {
        match &self.auth {
            AuthConfig::ApiKey { value } => Ok(GoogleApiKeyAuth::new(value.clone())),
            _ => Err(wrong_auth("Google", "api_key")),
        }
    }

    fn vertex_auth(&self) -> Result<VertexAuth, ModelBuildError> {
        match &self.auth {
            AuthConfig::AccessToken { token } => Ok(VertexAuth::AccessToken(secret(token))),
            _ => Err(wrong_auth("Vertex", "access_token")),
        }
    }

    fn bedrock_auth(&self) -> Result<BedrockAuth, ModelBuildError> {
        match &self.auth {
            AuthConfig::AwsStatic {
                access_key_id,
                secret_access_key,
                session_token,
            } => Ok(BedrockAuth::Static(AwsCredentials {
                access_key_id: access_key_id.clone(),
                secret_access_key: secret_access_key.clone(),
                session_token: session_token.clone(),
            })),
            _ => Err(wrong_auth("Bedrock", "aws_static")),
        }
    }

    fn azure_auth(&self) -> Result<AzureOpenAiAuth, ModelBuildError> {
        match &self.auth {
            AuthConfig::ApiKey { value } => Ok(AzureOpenAiAuth::ApiKey(secret(value))),
            _ => Err(wrong_auth("Azure OpenAI", "api_key")),
        }
    }

    fn cohere_auth(&self) -> Result<CohereAuth, ModelBuildError> {
        match &self.auth {
            AuthConfig::Bearer { token } => Ok(CohereAuth::bearer(secret(token))),
            _ => Err(wrong_auth("Cohere", "bearer")),
        }
    }
}

#[derive(Clone)]
struct CommonProvider {
    id: ProviderId,
    api: ApiEndpoint,
    headers: HeaderConfig,
}

impl CommonProvider {
    fn with_auth<A>(&self, auth: A) -> ProviderConfig<A> {
        ProviderConfig::new(
            self.id.clone(),
            self.api.clone(),
            auth,
            self.headers.clone(),
        )
        .expect("common provider identity was validated")
    }
}

/// Resolved authentication forms supported by static TOML configuration.
#[derive(Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum AuthConfig {
    /// No adapter-injected authentication.
    None,
    /// API-key authentication.
    ApiKey { value: String },
    /// Bearer authentication.
    Bearer { token: String },
    /// Caller-selected reviewed API-key header authentication.
    HeaderApiKey { name: String, value: String },
    /// Official OpenAI bearer authentication and account headers.
    Openai {
        api_key: String,
        organization: Option<String>,
        project: Option<String>,
    },
    /// Static OAuth access token.
    AccessToken { token: String },
    /// Static AWS credentials.
    AwsStatic {
        access_key_id: String,
        secret_access_key: String,
        session_token: Option<String>,
    },
}

impl Drop for AuthConfig {
    fn drop(&mut self) {
        match self {
            Self::None => {}
            Self::ApiKey { value } => value.zeroize(),
            Self::Bearer { token } | Self::AccessToken { token } => token.zeroize(),
            Self::HeaderApiKey { value, .. } => value.zeroize(),
            Self::Openai { api_key, .. } => api_key.zeroize(),
            Self::AwsStatic {
                access_key_id,
                secret_access_key,
                session_token,
            } => {
                access_key_id.zeroize();
                secret_access_key.zeroize();
                if let Some(session_token) = session_token {
                    session_token.zeroize();
                }
            }
        }
    }
}

#[derive(Clone)]
struct SecretHeaderProvider {
    name: String,
    value: SecretString,
}

impl HeaderProvider for SecretHeaderProvider {
    fn headers(&self) -> Result<HeaderOverrides, ModelError> {
        let name = HeaderName::from_bytes(self.name.as_bytes())
            .map_err(|_| ModelError::invalid_request("invalid API-key header name"))?;
        let value = HeaderValue::from_str(self.value.expose_secret())
            .map_err(|_| ModelError::invalid_request("invalid API-key header value"))?;
        Ok(HeaderOverrides::new(HeaderMap::from_iter([(name, value)])))
    }
}

/// Common normalized request defaults.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CommonDefaults {
    pub max_output_tokens: Option<u64>,
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub reasoning_effort: Option<String>,
    #[serde(default)]
    pub include_raw: bool,
}

/// All retained concrete Oven adapters. MiniMax and Claude Platform on AWS are intentionally absent.
///
/// Rust follows Oven's `adapter` terminology; the user-facing TOML discriminator
/// deliberately uses the British spelling `adaptor`.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "adaptor", rename_all = "kebab-case", deny_unknown_fields)]
pub enum AdapterConfig {
    Anthropic {
        settings: AnthropicSettingsConfig,
        #[serde(default)]
        options: AnthropicOptionsConfig,
    },
    OpenaiChat {
        settings: OpenAiChatSettingsConfig,
        #[serde(default)]
        options: OpenAiChatOptionsConfig,
    },
    OpenaiResponses {
        #[serde(default)]
        settings: OpenAiResponsesSettingsConfig,
        #[serde(default)]
        options: OpenAiResponsesOptionsConfig,
    },
    OpenaiCompatible {
        settings: CompatibleSettingsConfig,
        #[serde(default)]
        options: CompatibleOptionsConfig,
    },
    Google {
        settings: GoogleSettingsConfig,
        #[serde(default)]
        options: GoogleOptionsConfig,
    },
    Vertex {
        settings: VertexSettingsConfig,
        #[serde(default)]
        options: VertexOptionsConfig,
    },
    Bedrock {
        settings: BedrockSettingsConfig,
        #[serde(default)]
        options: BedrockOptionsConfig,
    },
    AzureChat {
        settings: AzureChatSettingsConfig,
        #[serde(default)]
        options: AzureChatOptionsConfig,
    },
    AzureResponses {
        settings: AzureResponsesSettingsConfig,
        #[serde(default)]
        options: AzureResponsesOptionsConfig,
    },
    Cohere {
        settings: CohereSettingsConfig,
        #[serde(default)]
        options: CohereOptionsConfig,
    },
}

/// Concrete model construction error with redacted formatting.
#[derive(Error)]
pub enum ModelBuildError {
    #[error("invalid model configuration: {0}")]
    Oven(#[source] Box<ModelError>),
    #[error("invalid HTTP header name `{0}`")]
    HeaderName(String),
    #[error("invalid HTTP header value for `{0}`")]
    HeaderValue(String),
    #[error("{adapter} adapter requires auth.type = {expected}")]
    WrongAuth {
        adapter: &'static str,
        expected: &'static str,
    },
    #[error("could not encode provider request defaults")]
    ProviderOptions(#[source] serde_json::Error),
}

impl From<ModelError> for ModelBuildError {
    fn from(error: ModelError) -> Self {
        Self::Oven(Box::new(error))
    }
}

impl std::fmt::Debug for ModelBuildError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_tuple("ModelBuildError")
            .field(&self.to_string())
            .finish()
    }
}

fn wrong_auth(adapter: &'static str, expected: &'static str) -> ModelBuildError {
    ModelBuildError::WrongAuth { adapter, expected }
}

fn secret(value: &str) -> SecretString {
    SecretString::new(value.to_owned())
}

fn header_config(values: &BTreeMap<String, String>) -> Result<HeaderConfig, ModelBuildError> {
    let mut headers = HeaderMap::new();
    for (name, value) in values {
        let parsed_name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| ModelBuildError::HeaderName(name.clone()))?;
        let parsed_value =
            HeaderValue::from_str(value).map_err(|_| ModelBuildError::HeaderValue(name.clone()))?;
        headers.insert(parsed_name, parsed_value);
    }
    Ok(HeaderConfig {
        static_headers: HeaderOverrides::new(headers),
        dynamic_headers: None,
    })
}

fn namespace(
    name: &str,
    value: impl Serialize,
) -> Result<BTreeMap<String, Value>, ModelBuildError> {
    let value = serde_json::to_value(value).map_err(ModelBuildError::ProviderOptions)?;
    Ok([(name.to_owned(), value)].into_iter().collect())
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TimeoutsConfig {
    #[serde(default = "default_connect")]
    pub connect_seconds: u64,
    #[serde(default = "default_headers")]
    pub headers_seconds: u64,
    #[serde(default = "default_credentials")]
    pub credentials_seconds: u64,
    #[serde(default = "default_stream_idle")]
    pub stream_idle_seconds: u64,
}

impl Default for TimeoutsConfig {
    fn default() -> Self {
        Self {
            connect_seconds: default_connect(),
            headers_seconds: default_headers(),
            credentials_seconds: default_credentials(),
            stream_idle_seconds: default_stream_idle(),
        }
    }
}

const fn default_connect() -> u64 {
    10
}
const fn default_headers() -> u64 {
    30
}
const fn default_credentials() -> u64 {
    30
}
const fn default_stream_idle() -> u64 {
    60
}

impl TimeoutsConfig {
    fn anthropic(self) -> AnthropicTimeouts {
        AnthropicTimeouts {
            headers: Duration::from_secs(self.headers_seconds),
            credentials: Duration::from_secs(self.credentials_seconds),
            stream_idle: Duration::from_secs(self.stream_idle_seconds),
        }
    }
    fn openai(self) -> OpenAiTimeouts {
        OpenAiTimeouts {
            connect: Duration::from_secs(self.connect_seconds),
            headers: Duration::from_secs(self.headers_seconds),
            stream_idle: Duration::from_secs(self.stream_idle_seconds),
        }
    }
    fn google(self) -> GoogleTimeouts {
        GoogleTimeouts {
            connect: Duration::from_secs(self.connect_seconds),
            headers: Duration::from_secs(self.headers_seconds),
            stream_idle: Duration::from_secs(self.stream_idle_seconds),
        }
    }
    fn vertex(self) -> GoogleVertexTimeouts {
        GoogleVertexTimeouts {
            connect: Duration::from_secs(self.connect_seconds),
            headers: Duration::from_secs(self.headers_seconds),
            credentials: Duration::from_secs(self.credentials_seconds),
            stream_idle: Duration::from_secs(self.stream_idle_seconds),
        }
    }
    fn bedrock(self) -> BedrockTimeouts {
        BedrockTimeouts {
            connect: Duration::from_secs(self.connect_seconds),
            headers: Duration::from_secs(self.headers_seconds),
            credentials: Duration::from_secs(self.credentials_seconds),
            stream_idle: Duration::from_secs(self.stream_idle_seconds),
        }
    }
    fn azure(self) -> AzureOpenAiTimeouts {
        AzureOpenAiTimeouts {
            connect: Duration::from_secs(self.connect_seconds),
            headers: Duration::from_secs(self.headers_seconds),
            credentials: Duration::from_secs(self.credentials_seconds),
            stream_idle: Duration::from_secs(self.stream_idle_seconds),
        }
    }
    fn cohere(self) -> CohereTimeouts {
        CohereTimeouts {
            connect: Duration::from_secs(self.connect_seconds),
            headers: Duration::from_secs(self.headers_seconds),
            stream_idle: Duration::from_secs(self.stream_idle_seconds),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AnthropicThinkingSupportConfig {
    None,
    Extended,
    Adaptive,
    Both,
}

impl From<AnthropicThinkingSupportConfig> for AnthropicThinkingSupport {
    fn from(value: AnthropicThinkingSupportConfig) -> Self {
        match value {
            AnthropicThinkingSupportConfig::None => Self::None,
            AnthropicThinkingSupportConfig::Extended => Self::Extended,
            AnthropicThinkingSupportConfig::Adaptive => Self::Adaptive,
            AnthropicThinkingSupportConfig::Both => Self::Both,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AnthropicSettingsConfig {
    #[serde(default)]
    pub timeouts: TimeoutsConfig,
    pub thinking: AnthropicThinkingSupportConfig,
    pub thinking_default_active: bool,
    pub thinking_disable_allowed: bool,
    #[serde(default)]
    pub thinking_disable_forbidden_efforts: BTreeSet<String>,
    pub effort: bool,
    pub assistant_prefill: bool,
    pub reject_non_default_sampling: bool,
    pub native_context_discriminator: Option<String>,
}

impl AnthropicSettingsConfig {
    fn to_oven(&self) -> Result<AnthropicSettings, ModelBuildError> {
        Ok(AnthropicSettings {
            client: reqwest_oven::Client::builder()
                .connect_timeout(Duration::from_secs(self.timeouts.connect_seconds))
                .build()
                .map_err(|_| ModelError::transport("could not construct Anthropic HTTP client"))?,
            timeouts: self.timeouts.anthropic(),
            protocol: AnthropicProtocolSettings {
                thinking: self.thinking.into(),
                thinking_default_active: self.thinking_default_active,
                thinking_disable_allowed: self.thinking_disable_allowed,
                thinking_disable_forbidden_efforts: self.thinking_disable_forbidden_efforts.clone(),
                effort: self.effort,
                assistant_prefill: self.assistant_prefill,
                reject_non_default_sampling: self.reject_non_default_sampling,
            },
            native_context_discriminator: self
                .native_context_discriminator
                .as_ref()
                .map(|value| ResourceId::new(value.clone()).map_err(ModelBuildError::from))
                .transpose()?,
        })
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum AnthropicThinkingConfig {
    Disabled,
    Enabled {
        budget_tokens: u64,
        display: Option<String>,
    },
    Adaptive {
        display: Option<String>,
    },
}

impl From<AnthropicThinkingConfig> for AnthropicThinking {
    fn from(value: AnthropicThinkingConfig) -> Self {
        match value {
            AnthropicThinkingConfig::Disabled => Self::Disabled,
            AnthropicThinkingConfig::Enabled {
                budget_tokens,
                display,
            } => Self::Enabled {
                budget_tokens,
                display,
            },
            AnthropicThinkingConfig::Adaptive { display } => Self::Adaptive { display },
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AnthropicCacheTtlConfig {
    FiveMinutes,
    OneHour,
}

impl From<AnthropicCacheTtlConfig> for AnthropicCacheTtl {
    fn from(value: AnthropicCacheTtlConfig) -> Self {
        match value {
            AnthropicCacheTtlConfig::FiveMinutes => Self::FiveMinutes,
            AnthropicCacheTtlConfig::OneHour => Self::OneHour,
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AnthropicOptionsConfig {
    pub thinking: Option<AnthropicThinkingConfig>,
    pub effort: Option<String>,
    pub cache_ttl: Option<AnthropicCacheTtlConfig>,
    pub user_id: Option<String>,
    #[serde(default)]
    pub betas: Vec<String>,
}

impl AnthropicOptionsConfig {
    fn to_oven(&self) -> AnthropicRequestOptions {
        AnthropicRequestOptions {
            thinking: self.thinking.clone().map(Into::into),
            effort: self.effort.clone(),
            cache_control: self
                .cache_ttl
                .map(|ttl| AnthropicCacheControl { ttl: ttl.into() }),
            user_id: self.user_id.clone(),
            betas: self.betas.clone(),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SystemRoleConfig {
    System,
    Developer,
    Omit,
}

impl From<SystemRoleConfig> for SystemMessageRole {
    fn from(value: SystemRoleConfig) -> Self {
        match value {
            SystemRoleConfig::System => Self::System,
            SystemRoleConfig::Developer => Self::Developer,
            SystemRoleConfig::Omit => Self::Omit,
        }
    }
}

impl From<SystemRoleConfig> for AzureSystemMessageRole {
    fn from(value: SystemRoleConfig) -> Self {
        match value {
            SystemRoleConfig::System => Self::System,
            SystemRoleConfig::Developer => Self::Developer,
            SystemRoleConfig::Omit => Self::Omit,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MaxTokensFieldConfig {
    MaxTokens,
    MaxCompletionTokens,
    Omit,
}

impl From<MaxTokensFieldConfig> for MaxTokensField {
    fn from(value: MaxTokensFieldConfig) -> Self {
        match value {
            MaxTokensFieldConfig::MaxTokens => Self::MaxTokens,
            MaxTokensFieldConfig::MaxCompletionTokens => Self::MaxCompletionTokens,
            MaxTokensFieldConfig::Omit => Self::Omit,
        }
    }
}

impl From<MaxTokensFieldConfig> for AzureMaxTokensField {
    fn from(value: MaxTokensFieldConfig) -> Self {
        match value {
            MaxTokensFieldConfig::MaxTokens => Self::MaxTokens,
            MaxTokensFieldConfig::MaxCompletionTokens => Self::MaxCompletionTokens,
            MaxTokensFieldConfig::Omit => Self::Omit,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StructuredOutputConfig {
    Unsupported,
    JsonObject,
    JsonSchema,
}

impl From<StructuredOutputConfig> for StructuredOutputSupport {
    fn from(value: StructuredOutputConfig) -> Self {
        match value {
            StructuredOutputConfig::Unsupported => Self::Unsupported,
            StructuredOutputConfig::JsonObject => Self::JsonObject,
            StructuredOutputConfig::JsonSchema => Self::JsonSchema,
        }
    }
}

impl From<StructuredOutputConfig> for AzureStructuredOutputSupport {
    fn from(value: StructuredOutputConfig) -> Self {
        match value {
            StructuredOutputConfig::Unsupported => Self::Unsupported,
            StructuredOutputConfig::JsonObject => Self::JsonObject,
            StructuredOutputConfig::JsonSchema => Self::JsonSchema,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningFieldConfig {
    None,
    ReasoningContent,
    Reasoning,
}

impl From<ReasoningFieldConfig> for ReasoningField {
    fn from(value: ReasoningFieldConfig) -> Self {
        match value {
            ReasoningFieldConfig::None => Self::None,
            ReasoningFieldConfig::ReasoningContent => Self::ReasoningContent,
            ReasoningFieldConfig::Reasoning => Self::Reasoning,
        }
    }
}

impl From<ReasoningFieldConfig> for AzureReasoningField {
    fn from(value: ReasoningFieldConfig) -> Self {
        match value {
            ReasoningFieldConfig::None => Self::None,
            ReasoningFieldConfig::ReasoningContent => Self::ReasoningContent,
            ReasoningFieldConfig::Reasoning => Self::Reasoning,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OpenAiChatSettingsConfig {
    pub system_message_role: SystemRoleConfig,
    pub max_tokens_field: MaxTokensFieldConfig,
    pub stream_usage: bool,
    pub structured_output: StructuredOutputConfig,
    pub reasoning_field: ReasoningFieldConfig,
    pub routing_discriminator: Option<String>,
    #[serde(default)]
    pub timeouts: TimeoutsConfig,
}

impl OpenAiChatSettingsConfig {
    fn to_openai_chat(&self) -> OpenAiChatSettings {
        OpenAiChatSettings {
            system_message_role: self.system_message_role.into(),
            max_tokens_field: self.max_tokens_field.into(),
            stream_usage: self.stream_usage,
            structured_output: self.structured_output.into(),
            reasoning_field: self.reasoning_field.into(),
            routing_discriminator: self.routing_discriminator.clone(),
            client: None,
            timeouts: self.timeouts.openai(),
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OpenAiResponsesSettingsConfig {
    pub routing_discriminator: Option<String>,
    #[serde(default)]
    pub compaction: OpenAiResponsesCompactionConfig,
    #[serde(default)]
    pub timeouts: TimeoutsConfig,
}

impl OpenAiResponsesSettingsConfig {
    fn to_openai_responses(&self) -> OpenAiResponsesSettings {
        OpenAiResponsesSettings {
            routing_discriminator: self.routing_discriminator.clone(),
            compaction: self.compaction.into(),
            client: None,
            timeouts: self.timeouts.openai(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OpenAiResponsesCompactionConfig {
    #[default]
    Unsupported,
    V1,
}

impl From<OpenAiResponsesCompactionConfig> for OpenAiResponsesCompaction {
    fn from(value: OpenAiResponsesCompactionConfig) -> Self {
        match value {
            OpenAiResponsesCompactionConfig::Unsupported => Self::Unsupported,
            OpenAiResponsesCompactionConfig::V1 => Self::V1,
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OpenAiChatOptionsConfig {
    pub user: Option<String>,
    pub reasoning_effort: Option<String>,
    pub service_tier: Option<String>,
    pub verbosity: Option<String>,
    pub parallel_tool_calls: Option<bool>,
}

impl From<&OpenAiChatOptionsConfig> for OpenAiChatOptions {
    fn from(value: &OpenAiChatOptionsConfig) -> Self {
        Self {
            user: value.user.clone(),
            reasoning_effort: value.reasoning_effort.clone(),
            service_tier: value.service_tier.clone(),
            verbosity: value.verbosity.clone(),
            parallel_tool_calls: value.parallel_tool_calls,
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OpenAiResponsesOptionsConfig {
    #[serde(default)]
    pub include: Vec<String>,
    pub user: Option<String>,
    pub service_tier: Option<String>,
    pub verbosity: Option<String>,
    pub reasoning_summary: Option<String>,
    pub reasoning_mode: Option<String>,
    pub truncation: Option<String>,
    pub parallel_tool_calls: Option<bool>,
}

impl From<&OpenAiResponsesOptionsConfig> for OpenAiResponsesOptions {
    fn from(value: &OpenAiResponsesOptionsConfig) -> Self {
        Self {
            include: value.include.clone(),
            user: value.user.clone(),
            service_tier: value.service_tier.clone(),
            verbosity: value.verbosity.clone(),
            reasoning_summary: value.reasoning_summary.clone(),
            reasoning_mode: value.reasoning_mode.clone(),
            truncation: value.truncation.clone(),
            parallel_tool_calls: value.parallel_tool_calls,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CompatibleSettingsConfig {
    pub adapter_id: String,
    pub system_message_role: SystemRoleConfig,
    pub max_tokens_field: MaxTokensFieldConfig,
    pub stream_usage: bool,
    pub structured_output: StructuredOutputConfig,
    pub reasoning_field: ReasoningFieldConfig,
    #[serde(default)]
    pub query: BTreeMap<String, String>,
    #[serde(default = "default_request_id_headers")]
    pub request_id_headers: Vec<String>,
    #[serde(default)]
    pub strict_sse_content_type: bool,
    pub routing_discriminator: Option<String>,
    #[serde(default)]
    pub timeouts: TimeoutsConfig,
}

fn default_request_id_headers() -> Vec<String> {
    vec!["x-request-id".into()]
}

impl CompatibleSettingsConfig {
    fn to_oven(&self) -> OpenAiCompatibleChatSettings {
        OpenAiCompatibleChatSettings {
            adapter_id: AdapterId::new(self.adapter_id.clone()),
            system_message_role: self.system_message_role.into(),
            max_tokens_field: self.max_tokens_field.into(),
            stream_usage: self.stream_usage,
            structured_output: self.structured_output.into(),
            reasoning_field: self.reasoning_field.into(),
            query: self
                .query
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect(),
            request_id_headers: self.request_id_headers.clone(),
            strict_sse_content_type: self.strict_sse_content_type,
            routing_discriminator: self.routing_discriminator.clone(),
            client: None,
            timeouts: self.timeouts.openai(),
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CompatibleOptionsConfig {
    #[serde(default)]
    pub extra_body: Map<String, Value>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum GoogleThinkingSettingsConfig {
    Unsupported,
    Budget {
        effort_budgets: BTreeMap<String, i64>,
    },
    Level {
        effort_levels: BTreeMap<String, String>,
    },
}

impl GoogleThinkingSettingsConfig {
    fn to_oven(&self) -> GoogleThinkingSettings {
        match self {
            Self::Unsupported => GoogleThinkingSettings::Unsupported,
            Self::Budget { effort_budgets } => GoogleThinkingSettings::Budget {
                effort_budgets: effort_budgets.clone(),
            },
            Self::Level { effort_levels } => GoogleThinkingSettings::Level {
                effort_levels: effort_levels.clone(),
            },
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GoogleSettingsConfig {
    pub model_resource: String,
    #[serde(default)]
    pub timeouts: TimeoutsConfig,
    pub thinking: GoogleThinkingSettingsConfig,
    pub strict_functions: bool,
    pub mixed_client_and_provider_tools: bool,
    pub current_turn_signature_sentinel: bool,
}

impl GoogleSettingsConfig {
    fn to_oven(&self) -> GoogleGenerateContentSettings {
        GoogleGenerateContentSettings {
            model_resource: self.model_resource.clone(),
            timeouts: self.timeouts.google(),
            thinking: self.thinking.to_oven(),
            tools: GoogleToolSettings {
                strict_functions: self.strict_functions,
                mixed_client_and_provider_tools: self.mixed_client_and_provider_tools,
                current_turn_signature_sentinel: self.current_turn_signature_sentinel,
            },
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GoogleThinkingOptionsConfig {
    pub thinking_budget: Option<i64>,
    pub thinking_level: Option<String>,
    pub include_thoughts: Option<bool>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SafetySettingConfig {
    pub category: String,
    pub threshold: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum GoogleProviderToolConfig {
    GoogleSearch,
    UrlContext,
    CodeExecution,
    FileSearch { stores: Vec<String> },
    GoogleMaps,
}

impl GoogleProviderToolConfig {
    fn to_oven(&self) -> GoogleProviderTool {
        match self {
            Self::GoogleSearch => GoogleProviderTool::GoogleSearch,
            Self::UrlContext => GoogleProviderTool::UrlContext,
            Self::CodeExecution => GoogleProviderTool::CodeExecution,
            Self::FileSearch { stores } => GoogleProviderTool::FileSearch {
                stores: stores.clone(),
            },
            Self::GoogleMaps => GoogleProviderTool::GoogleMaps,
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GoogleOptionsConfig {
    pub top_k: Option<u32>,
    #[serde(default)]
    pub stop_sequences: Vec<String>,
    pub seed: Option<i32>,
    pub presence_penalty: Option<f64>,
    pub frequency_penalty: Option<f64>,
    pub thinking: Option<GoogleThinkingOptionsConfig>,
    #[serde(default)]
    pub provider_tools: Vec<GoogleProviderToolConfig>,
    pub service_tier: Option<String>,
    pub cached_content: Option<String>,
    #[serde(default)]
    pub safety_settings: Vec<SafetySettingConfig>,
}

impl GoogleOptionsConfig {
    fn to_oven(&self) -> GoogleRequestOptions {
        GoogleRequestOptions {
            top_k: self.top_k,
            stop_sequences: self.stop_sequences.clone(),
            seed: self.seed,
            presence_penalty: self.presence_penalty,
            frequency_penalty: self.frequency_penalty,
            thinking_config: self.thinking.as_ref().map(|thinking| GoogleThinkingConfig {
                thinking_budget: thinking.thinking_budget,
                thinking_level: thinking.thinking_level.clone(),
                include_thoughts: thinking.include_thoughts,
            }),
            provider_tools: self
                .provider_tools
                .iter()
                .map(GoogleProviderToolConfig::to_oven)
                .collect(),
            service_tier: self.service_tier.clone(),
            cached_content: self.cached_content.clone(),
            safety_settings: self
                .safety_settings
                .iter()
                .map(|setting| GoogleSafetySetting {
                    category: setting.category.clone(),
                    threshold: setting.threshold.clone(),
                })
                .collect(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum VertexResourceConfig {
    PublisherModel { publisher: String, model: String },
    Endpoint { endpoint: String },
}

impl VertexResourceConfig {
    fn to_oven(&self) -> GoogleVertexResource {
        match self {
            Self::PublisherModel { publisher, model } => GoogleVertexResource::PublisherModel {
                publisher: publisher.clone(),
                model: model.clone(),
            },
            Self::Endpoint { endpoint } => GoogleVertexResource::Endpoint {
                endpoint: endpoint.clone(),
            },
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VertexThinkingModeConfig {
    Unsupported,
    Budget,
    Level,
}

impl From<VertexThinkingModeConfig> for GoogleVertexThinkingMode {
    fn from(value: VertexThinkingModeConfig) -> Self {
        match value {
            VertexThinkingModeConfig::Unsupported => Self::Unsupported,
            VertexThinkingModeConfig::Budget => Self::Budget,
            VertexThinkingModeConfig::Level => Self::Level,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct VertexMediaConfig {
    pub max_images: usize,
    pub max_https_images: usize,
    pub max_documents: usize,
    pub max_audio: usize,
    pub max_videos: usize,
    pub max_https_videos: usize,
    pub max_inline_image_bytes: usize,
    pub max_inline_pdf_bytes: usize,
    pub max_inline_text_bytes: usize,
    pub url_schemes: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct VertexSettingsConfig {
    pub project: String,
    pub location: String,
    pub resource: VertexResourceConfig,
    pub thinking: VertexThinkingModeConfig,
    pub provider_tools: bool,
    pub mixed_client_and_provider_tools: bool,
    pub strict_functions: bool,
    pub stream_function_call_arguments: bool,
    pub media: VertexMediaConfig,
    #[serde(default)]
    pub timeouts: TimeoutsConfig,
}

impl VertexSettingsConfig {
    fn to_oven(
        &self,
        provider: &ProviderConfig<VertexAuth>,
        declaration: &ModelDeclaration,
    ) -> Result<GoogleVertexSettings, ModelBuildError> {
        let resource = self.resource.to_oven();
        let native_context_scope = google_vertex_native_context_scope(
            provider.id.clone(),
            declaration.id.clone(),
            &provider.api,
            &self.project,
            &self.location,
            &resource,
        )?;
        Ok(GoogleVertexSettings {
            project: self.project.clone(),
            location: self.location.clone(),
            resource,
            thinking: self.thinking.into(),
            tools: GoogleVertexToolSettings {
                provider_tools: self.provider_tools,
                mixed_client_and_provider_tools: self.mixed_client_and_provider_tools,
                strict_functions: self.strict_functions,
            },
            stream_function_call_arguments: self.stream_function_call_arguments,
            media: GoogleVertexMediaSettings {
                max_images: self.media.max_images,
                max_https_images: self.media.max_https_images,
                max_documents: self.media.max_documents,
                max_audio: self.media.max_audio,
                max_videos: self.media.max_videos,
                max_https_videos: self.media.max_https_videos,
                max_inline_image_bytes: self.media.max_inline_image_bytes,
                max_inline_pdf_bytes: self.media.max_inline_pdf_bytes,
                max_inline_text_bytes: self.media.max_inline_text_bytes,
                url_schemes: self.media.url_schemes.clone(),
            },
            native_context_scope,
            client: None,
            timeouts: self.timeouts.vertex(),
        })
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct VertexThinkingOptionsConfig {
    pub thinking_budget: Option<i64>,
    pub thinking_level: Option<String>,
    pub include_thoughts: Option<bool>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct VertexSafetySettingConfig {
    pub category: String,
    pub threshold: String,
    pub method: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum VertexProviderToolConfig {
    GoogleSearch,
    UrlContext,
    CodeExecution,
    VertexRagStore {
        rag_corpus: String,
        top_k: Option<u32>,
    },
    GoogleMaps,
}

impl VertexProviderToolConfig {
    fn to_oven(&self) -> GoogleVertexProviderTool {
        match self {
            Self::GoogleSearch => GoogleVertexProviderTool::GoogleSearch,
            Self::UrlContext => GoogleVertexProviderTool::UrlContext,
            Self::CodeExecution => GoogleVertexProviderTool::CodeExecution,
            Self::VertexRagStore { rag_corpus, top_k } => {
                GoogleVertexProviderTool::VertexRagStore {
                    rag_corpus: rag_corpus.clone(),
                    top_k: *top_k,
                }
            }
            Self::GoogleMaps => GoogleVertexProviderTool::GoogleMaps,
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct VertexOptionsConfig {
    pub top_k: Option<f64>,
    #[serde(default)]
    pub stop_sequences: Vec<String>,
    pub seed: Option<i32>,
    pub presence_penalty: Option<f64>,
    pub frequency_penalty: Option<f64>,
    pub thinking: Option<VertexThinkingOptionsConfig>,
    #[serde(default)]
    pub provider_tools: Vec<VertexProviderToolConfig>,
    pub cached_content: Option<String>,
    #[serde(default)]
    pub safety_settings: Vec<VertexSafetySettingConfig>,
    pub shared_request_type: Option<String>,
    pub request_type: Option<String>,
}

impl VertexOptionsConfig {
    fn to_oven(&self) -> GoogleVertexRequestOptions {
        GoogleVertexRequestOptions {
            top_k: self.top_k,
            stop_sequences: self.stop_sequences.clone(),
            seed: self.seed,
            presence_penalty: self.presence_penalty,
            frequency_penalty: self.frequency_penalty,
            thinking_config: self
                .thinking
                .as_ref()
                .map(|thinking| GoogleVertexThinkingConfig {
                    thinking_budget: thinking.thinking_budget,
                    thinking_level: thinking.thinking_level.clone(),
                    include_thoughts: thinking.include_thoughts,
                }),
            provider_tools: self
                .provider_tools
                .iter()
                .map(VertexProviderToolConfig::to_oven)
                .collect(),
            cached_content: self.cached_content.clone(),
            safety_settings: self
                .safety_settings
                .iter()
                .map(|setting| GoogleVertexSafetySetting {
                    category: setting.category.clone(),
                    threshold: setting.threshold.clone(),
                    method: setting.method.clone(),
                })
                .collect(),
            shared_request_type: self.shared_request_type.clone(),
            request_type: self.request_type.clone(),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BedrockReasoningFormatConfig {
    Unsupported,
    AnthropicThinking,
    OpenaiReasoningEffort,
    BedrockReasoningConfig,
}

impl From<BedrockReasoningFormatConfig> for BedrockReasoningWireFormat {
    fn from(value: BedrockReasoningFormatConfig) -> Self {
        match value {
            BedrockReasoningFormatConfig::Unsupported => Self::Unsupported,
            BedrockReasoningFormatConfig::AnthropicThinking => Self::AnthropicThinking,
            BedrockReasoningFormatConfig::OpenaiReasoningEffort => Self::OpenAiReasoningEffort,
            BedrockReasoningFormatConfig::BedrockReasoningConfig => Self::BedrockReasoningConfig,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BedrockStructuredOutputConfig {
    Unsupported,
    JsonSchema,
}

impl From<BedrockStructuredOutputConfig> for BedrockStructuredOutput {
    fn from(value: BedrockStructuredOutputConfig) -> Self {
        match value {
            BedrockStructuredOutputConfig::Unsupported => Self::Unsupported,
            BedrockStructuredOutputConfig::JsonSchema => Self::JsonSchema,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BedrockSettingsConfig {
    pub region: String,
    pub reasoning_wire_format: BedrockReasoningFormatConfig,
    pub signed_reasoning: bool,
    pub structured_output: BedrockStructuredOutputConfig,
    pub max_event_message_bytes: usize,
    #[serde(default)]
    pub timeouts: TimeoutsConfig,
}

impl BedrockSettingsConfig {
    fn to_oven(&self) -> BedrockConverseSettings {
        BedrockConverseSettings {
            region: self.region.clone(),
            reasoning_wire_format: self.reasoning_wire_format.into(),
            signed_reasoning: self.signed_reasoning,
            structured_output: self.structured_output.into(),
            event_stream: BedrockEventStreamLimits::new(self.max_event_message_bytes),
            timeouts: self.timeouts.bedrock(),
            client: None,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BedrockGuardrailConfigInput {
    pub guardrail_identifier: String,
    pub guardrail_version: String,
    pub trace: Option<String>,
    pub stream_processing_mode: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BedrockOptionsConfig {
    pub additional_model_request_fields: Option<Value>,
    #[serde(default)]
    pub additional_model_response_field_paths: Vec<String>,
    pub service_tier: Option<String>,
    pub performance_latency: Option<String>,
    #[serde(default)]
    pub request_metadata: BTreeMap<String, String>,
    pub guardrail: Option<BedrockGuardrailConfigInput>,
    pub s3_bucket_owner: Option<String>,
    #[serde(default)]
    pub stop_sequences: Vec<String>,
    pub reasoning_type: Option<String>,
    pub reasoning_budget_tokens: Option<u64>,
    pub reasoning_display: Option<String>,
    pub max_reasoning_effort: Option<String>,
}

impl BedrockOptionsConfig {
    fn to_oven(&self) -> BedrockRequestOptions {
        BedrockRequestOptions {
            additional_model_request_fields: self.additional_model_request_fields.clone(),
            additional_model_response_field_paths: self
                .additional_model_response_field_paths
                .clone(),
            service_tier: self.service_tier.clone(),
            performance_latency: self.performance_latency.clone(),
            request_metadata: self.request_metadata.clone(),
            guardrail: self
                .guardrail
                .as_ref()
                .map(|guardrail| BedrockGuardrailConfig {
                    guardrail_identifier: guardrail.guardrail_identifier.clone(),
                    guardrail_version: guardrail.guardrail_version.clone(),
                    trace: guardrail.trace.clone(),
                    stream_processing_mode: guardrail.stream_processing_mode.clone(),
                }),
            s3: self
                .s3_bucket_owner
                .as_ref()
                .map(|owner| BedrockS3LocationOptions {
                    bucket_owner: Some(owner.clone()),
                }),
            stop_sequences: self.stop_sequences.clone(),
            reasoning_type: self.reasoning_type.clone(),
            reasoning_budget_tokens: self.reasoning_budget_tokens,
            reasoning_display: self.reasoning_display.clone(),
            max_reasoning_effort: self.max_reasoning_effort.clone(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum AzureRouteConfig {
    V1,
    V1Preview,
    Dated { version: String },
}

impl AzureRouteConfig {
    fn to_oven(&self) -> Result<AzureApiRoute, ModelBuildError> {
        Ok(match self {
            Self::V1 => AzureApiRoute::V1,
            Self::V1Preview => AzureApiRoute::V1Preview,
            Self::Dated { version } => AzureApiRoute::Dated(AzureApiVersion::new(version.clone())?),
        })
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AzureRevisionConfig {
    pub model: String,
    pub version: String,
    pub deployment_type: String,
}

impl AzureRevisionConfig {
    fn to_oven(&self) -> AzureOpenAiRevision {
        AzureOpenAiRevision {
            model: self.model.clone(),
            version: self.version.clone(),
            deployment_type: self.deployment_type.clone(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AzureChatSettingsConfig {
    pub route: AzureRouteConfig,
    pub revision: Option<AzureRevisionConfig>,
    #[serde(default)]
    pub timeouts: TimeoutsConfig,
    pub system_role: SystemRoleConfig,
    pub max_tokens_field: MaxTokensFieldConfig,
    pub stream_usage: bool,
    pub structured_output: StructuredOutputConfig,
    pub reasoning_field: ReasoningFieldConfig,
    pub omit_reasoning_sampling: bool,
}

impl AzureChatSettingsConfig {
    fn to_chat(&self) -> Result<AzureOpenAiChatSettings, ModelBuildError> {
        Ok(AzureOpenAiChatSettings {
            route: self.route.to_oven()?,
            revision: self.revision.as_ref().map(AzureRevisionConfig::to_oven),
            timeouts: self.timeouts.azure(),
            completions: AzureOpenAiCompletionsConfig {
                system_role: self.system_role.into(),
                max_tokens_field: self.max_tokens_field.into(),
                stream_usage: self.stream_usage,
                structured_output: self.structured_output.into(),
                reasoning_field: self.reasoning_field.into(),
                omit_reasoning_sampling: self.omit_reasoning_sampling,
            },
        })
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AzureResponsesSettingsConfig {
    pub route: AzureRouteConfig,
    pub revision: Option<AzureRevisionConfig>,
    #[serde(default)]
    pub compaction: AzureResponsesCompactionConfig,
    #[serde(default)]
    pub timeouts: TimeoutsConfig,
}

impl AzureResponsesSettingsConfig {
    fn to_responses(&self) -> Result<AzureOpenAiResponsesSettings, ModelBuildError> {
        Ok(AzureOpenAiResponsesSettings {
            route: self.route.to_oven()?,
            revision: self.revision.as_ref().map(AzureRevisionConfig::to_oven),
            timeouts: self.timeouts.azure(),
            compaction: self.compaction.to_oven(),
        })
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum AzureResponsesCompactionConfig {
    #[default]
    Unsupported,
    V1 {
        routing_discriminator: String,
    },
}

impl AzureResponsesCompactionConfig {
    fn to_oven(&self) -> AzureOpenAiResponsesCompaction {
        match self {
            Self::Unsupported => AzureOpenAiResponsesCompaction::Unsupported,
            Self::V1 {
                routing_discriminator,
            } => AzureOpenAiResponsesCompaction::V1 {
                routing_discriminator: routing_discriminator.clone(),
            },
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AzureChatOptionsConfig {
    pub user: Option<String>,
    pub reasoning_effort: Option<String>,
    pub service_tier: Option<String>,
    pub verbosity: Option<String>,
    pub parallel_tool_calls: Option<bool>,
}

impl From<&AzureChatOptionsConfig> for AzureOpenAiChatOptions {
    fn from(value: &AzureChatOptionsConfig) -> Self {
        Self {
            user: value.user.clone(),
            reasoning_effort: value.reasoning_effort.clone(),
            service_tier: value.service_tier.clone(),
            verbosity: value.verbosity.clone(),
            parallel_tool_calls: value.parallel_tool_calls,
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AzureResponsesOptionsConfig {
    #[serde(default)]
    pub include: Vec<String>,
    pub user: Option<String>,
    pub service_tier: Option<String>,
    pub verbosity: Option<String>,
    pub reasoning_summary: Option<String>,
    pub reasoning_mode: Option<String>,
    pub truncation: Option<String>,
    pub parallel_tool_calls: Option<bool>,
}

impl From<&AzureResponsesOptionsConfig> for AzureOpenAiResponsesOptions {
    fn from(value: &AzureResponsesOptionsConfig) -> Self {
        Self {
            include: value.include.clone(),
            user: value.user.clone(),
            service_tier: value.service_tier.clone(),
            verbosity: value.verbosity.clone(),
            reasoning_summary: value.reasoning_summary.clone(),
            reasoning_mode: value.reasoning_mode.clone(),
            truncation: value.truncation.clone(),
            parallel_tool_calls: value.parallel_tool_calls,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CohereThinkingConfig {
    pub enabled: bool,
    pub token_budget: Option<u64>,
}

impl CohereThinkingConfig {
    fn to_oven(&self) -> CohereThinking {
        CohereThinking {
            enabled: self.enabled,
            token_budget: self.token_budget,
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CohereSettingsConfig {
    #[serde(default)]
    pub timeouts: TimeoutsConfig,
    pub strict_tools: bool,
    pub safety_mode: Option<String>,
    pub thinking: Option<CohereThinkingConfig>,
    #[serde(default)]
    pub reasoning_effort: BTreeMap<String, CohereThinkingConfig>,
    pub top_k: Option<u32>,
    pub seed: Option<u64>,
    pub frequency_penalty: Option<f64>,
    pub presence_penalty: Option<f64>,
    #[serde(default)]
    pub stop_sequences: Vec<String>,
    pub priority: Option<i64>,
}

impl CohereSettingsConfig {
    fn to_oven(&self) -> CohereSettings {
        CohereSettings {
            timeouts: self.timeouts.cohere(),
            strict_tools: self.strict_tools,
            safety_mode: self.safety_mode.clone(),
            thinking: self.thinking.as_ref().map(CohereThinkingConfig::to_oven),
            reasoning_effort: self
                .reasoning_effort
                .iter()
                .map(|(effort, thinking)| (effort.clone(), thinking.to_oven()))
                .collect(),
            top_k: self.top_k,
            seed: self.seed,
            frequency_penalty: self.frequency_penalty,
            presence_penalty: self.presence_penalty,
            stop_sequences: self.stop_sequences.clone(),
            priority: self.priority,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CohereDocumentConfig {
    pub id: Option<String>,
    pub data: Map<String, Value>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CohereOptionsConfig {
    #[serde(default)]
    pub documents: Vec<CohereDocumentConfig>,
    pub citation_mode: Option<String>,
    pub image_detail: Option<String>,
}

impl CohereOptionsConfig {
    fn to_oven(&self) -> CohereRequestOptions {
        CohereRequestOptions {
            documents: self
                .documents
                .iter()
                .map(|document| CohereDocument {
                    id: document.id.clone(),
                    data: document.data.clone(),
                })
                .collect(),
            citation_mode: self.citation_mode.clone(),
            image_detail: self.image_detail.clone(),
        }
    }
}
