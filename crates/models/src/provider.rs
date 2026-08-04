//! Strict schema-6 provider declarations and variant compiler.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
};

use cookie_agent_identity::{
    ConfiguredModelDefault, ModelKey, ProviderId, ProviderModelId, VariantId,
};
use http::{HeaderName as HttpHeaderName, HeaderValue};
use oven_sdk::{
    CancellationCapability as OvenCancellation, Capability, CompactionCapability as OvenCompaction,
    MediaCapabilities as OvenMediaCapabilities, MediaInputSupport, MediaSourceSupport, Modalities,
    Modality as OvenModality, ModelCapabilities as OvenCapabilities, ModelLimits,
    ReplayCapability as OvenReplay, ReplayDeclaration, ReplayPolicy,
};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use serde_json::Value;
use thiserror::Error;
use url::Url;
use zeroize::Zeroize as _;

use crate::{
    Catalog, CatalogReasoningOption, ModelEntry, ModelSet, ModelSetError, ModelVariant,
    Sha256Digest, VariantOrigin,
    schema::{
        AdapterConfig, AnthropicSettingsConfig, AnthropicThinkingSupportConfig,
        AuthConfig as ConcreteAuth, AzureChatOptionsConfig, AzureChatSettingsConfig,
        AzureResponsesOptionsConfig, AzureResponsesSettingsConfig, AzureRouteConfig,
        BedrockOptionsConfig, BedrockReasoningFormatConfig, BedrockSettingsConfig,
        BedrockStructuredOutputConfig, CohereOptionsConfig, CohereSettingsConfig,
        CohereThinkingConfig, CommonDefaults, CompatibleOptionsConfig, CompatibleSettingsConfig,
        ConcreteModel, GoogleOptionsConfig, GoogleSettingsConfig, GoogleThinkingSettingsConfig,
        MaxTokensFieldConfig, OpenAiChatOptionsConfig, OpenAiChatSettingsConfig,
        OpenAiResponsesCompactionConfig, OpenAiResponsesOptionsConfig,
        OpenAiResponsesSettingsConfig, OpenResponsesOptionsConfig, OpenResponsesSettingsConfig,
        OpenResponsesTransportConfig, ReasoningFieldConfig, StructuredOutputConfig,
        SystemRoleConfig, VertexMediaConfig, VertexOptionsConfig, VertexResourceConfig,
        VertexSettingsConfig, VertexThinkingModeConfig,
    },
};

const MAX_STRING: usize = 512;

/// Reviewed user-facing adaptor IDs.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum AdaptorId {
    Anthropic,
    OpenaiChat,
    OpenaiResponses,
    OpenaiCompatible,
    GoogleGemini,
    GoogleVertexGemini,
    AwsBedrockConverse,
    AzureOpenaiChat,
    AzureOpenaiResponses,
    CohereV2Chat,
    OpenResponses,
}

impl AdaptorId {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic",
            Self::OpenaiChat => "openai-chat",
            Self::OpenaiResponses => "openai-responses",
            Self::OpenaiCompatible => "openai-compatible",
            Self::GoogleGemini => "google-gemini",
            Self::GoogleVertexGemini => "google-vertex-gemini",
            Self::AwsBedrockConverse => "aws-bedrock-converse",
            Self::AzureOpenaiChat => "azure-openai-chat",
            Self::AzureOpenaiResponses => "azure-openai-responses",
            Self::CohereV2Chat => "cohere-v2-chat",
            Self::OpenResponses => "open-responses",
        }
    }
}

/// Secret-bearing string with redacted formatting.
#[derive(Clone, Deserialize, Eq, PartialEq)]
#[serde(transparent)]
pub struct SecretString(String);

impl SecretString {
    pub(crate) fn expose(&self) -> &str {
        &self.0
    }
    fn validate(&self) -> Result<(), ModelBuildError> {
        if self.0.is_empty() {
            Err(ModelBuildError::EmptySecret)
        } else {
            Ok(())
        }
    }
}

impl fmt::Debug for SecretString {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SecretString(<redacted>)")
    }
}

impl Drop for SecretString {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

/// Lowercase validated HTTP header name.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct HeaderName(String);

impl HeaderName {
    pub fn new(value: impl Into<String>) -> Result<Self, ModelBuildError> {
        let value = value.into().to_ascii_lowercase();
        HttpHeaderName::from_bytes(value.as_bytes()).map_err(|_| ModelBuildError::HeaderName)?;
        Ok(Self(value))
    }
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Serialize for HeaderName {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}
impl<'de> Deserialize<'de> for HeaderName {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(de::Error::custom)
    }
}

fn deserialize_headers<'de, D>(
    deserializer: D,
) -> Result<BTreeMap<HeaderName, SecretString>, D::Error>
where
    D: Deserializer<'de>,
{
    struct HeadersVisitor;
    impl<'de> de::Visitor<'de> for HeadersVisitor {
        type Value = BTreeMap<HeaderName, SecretString>;
        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("a map of unique case-insensitive HTTP headers")
        }
        fn visit_map<A>(self, mut access: A) -> Result<Self::Value, A::Error>
        where
            A: de::MapAccess<'de>,
        {
            let mut headers = BTreeMap::new();
            while let Some((name, value)) = access.next_entry::<HeaderName, SecretString>()? {
                if headers.insert(name, value).is_some() {
                    return Err(de::Error::custom("duplicate case-insensitive header name"));
                }
            }
            Ok(headers)
        }
    }
    deserializer.deserialize_map(HeadersVisitor)
}

/// Strict adaptor-declared semantic auth field name.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct AuthFieldName(String);

impl AuthFieldName {
    fn validate(value: &str) -> bool {
        !value.is_empty()
            && value.len() <= 128
            && value
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
    }
}

impl<'de> Deserialize<'de> for AuthFieldName {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        if Self::validate(&value) {
            Ok(Self(value))
        } else {
            Err(de::Error::custom("invalid auth field name"))
        }
    }
}

/// Exact authentication authoring forms.
#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum AuthDefinition {
    None,
    CredentialStore,
    Bearer {
        token: SecretString,
    },
    ApiKey {
        key: SecretString,
        header: Option<HeaderName>,
    },
    Basic {
        username: SecretString,
        password: SecretString,
    },
    AwsSdk,
    GoogleAdc,
    Fields {
        values: BTreeMap<AuthFieldName, SecretString>,
    },
}

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "source", rename_all = "snake_case")]
pub enum ProviderDefinition {
    ModelsDev(ModelsDevProvider),
    Explicit(ExplicitProvider),
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelsDevProvider {
    pub catalog_revision: String,
    pub endpoint: Option<String>,
    pub adaptor: Option<AdaptorId>,
    pub auth: AuthDefinition,
    #[serde(default, deserialize_with = "deserialize_headers")]
    pub headers: BTreeMap<HeaderName, SecretString>,
    pub models: BTreeMap<ProviderModelId, ModelsDevModelConfig>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExplicitProvider {
    pub endpoint: String,
    pub adaptor: AdaptorId,
    pub auth: AuthDefinition,
    #[serde(default, deserialize_with = "deserialize_headers")]
    pub headers: BTreeMap<HeaderName, SecretString>,
    pub models: BTreeMap<ProviderModelId, ExplicitModelConfig>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelsDevModelConfig {
    #[serde(default = "yes")]
    pub enabled: bool,
    pub display_name: Option<String>,
    #[serde(default)]
    pub defaults: RequestDefaults,
    #[serde(default)]
    pub options: ProviderOptions,
    #[serde(default)]
    pub variants: BTreeMap<VariantId, VariantDirective>,
    pub default_variant: Option<ConfiguredModelDefault>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExplicitModelConfig {
    #[serde(default = "yes")]
    pub enabled: bool,
    pub display_name: String,
    pub capabilities: ModelCapabilities,
    #[serde(default)]
    pub defaults: RequestDefaults,
    #[serde(default)]
    pub options: ProviderOptions,
    #[serde(default)]
    pub variants: BTreeMap<VariantId, VariantDirective>,
    pub default_variant: Option<ConfiguredModelDefault>,
}

const fn yes() -> bool {
    true
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Modality {
    Text,
    Image,
    Audio,
    Pdf,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum MediaKind {
    Image,
    Audio,
    Pdf,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct MimeType(String);

impl<'de> Deserialize<'de> for MimeType {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        let valid = value.len() <= 255
            && value
                .split_once('/')
                .is_some_and(|(kind, subtype)| !kind.is_empty() && !subtype.is_empty())
            && value.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'.' | b'+' | b'-')
            });
        if valid {
            Ok(Self(value))
        } else {
            Err(de::Error::custom("invalid MIME type"))
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MediaCapability {
    pub mime_types: BTreeSet<MimeType>,
    pub max_bytes: u64,
    pub max_count: u32,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplayCapability {
    Unsupported,
    Optional,
    Required,
}
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactionCapability {
    Unsupported,
    Optional,
    Required,
}
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CancellationCapability {
    LocalOnly,
    Provider,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ModelCapabilities {
    pub input: BTreeSet<Modality>,
    pub output: BTreeSet<Modality>,
    pub context_tokens: u64,
    pub output_tokens: u64,
    pub tool_calling: bool,
    pub parallel_tool_calls: bool,
    pub structured_output: bool,
    pub reasoning: bool,
    pub temperature: bool,
    pub top_p: bool,
    pub seed: bool,
    pub native_replay: ReplayCapability,
    pub native_compaction: CompactionCapability,
    pub cancellation: CancellationCapability,
    pub media: BTreeMap<MediaKind, MediaCapability>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FiniteF32(f32);
impl FiniteF32 {
    #[must_use]
    pub const fn get(self) -> f32 {
        self.0
    }
}
impl Serialize for FiniteF32 {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_f32(self.0)
    }
}
impl<'de> Deserialize<'de> for FiniteF32 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = f32::deserialize(deserializer)?;
        if value.is_finite() {
            Ok(Self(value))
        } else {
            Err(de::Error::custom("number must be finite"))
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolChoice {
    Auto,
    None,
    Required,
    Named(String),
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct RequestDefaults {
    pub temperature: Option<FiniteF32>,
    pub top_p: Option<FiniteF32>,
    pub max_output_tokens: Option<u64>,
    pub stop: Vec<String>,
    #[serde(skip)]
    stop_present: bool,
    pub seed: Option<i64>,
    pub tool_choice: Option<ToolChoice>,
}

impl<'de> Deserialize<'de> for RequestDefaults {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            temperature: Option<FiniteF32>,
            top_p: Option<FiniteF32>,
            max_output_tokens: Option<u64>,
            stop: Option<Vec<String>>,
            seed: Option<i64>,
            tool_choice: Option<ToolChoice>,
        }
        let wire = Wire::deserialize(deserializer)?;
        Ok(Self {
            temperature: wire.temperature,
            top_p: wire.top_p,
            max_output_tokens: wire.max_output_tokens,
            stop_present: wire.stop.is_some(),
            stop: wire.stop.unwrap_or_default(),
            seed: wire.seed,
            tool_choice: wire.tool_choice,
        })
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningEffort {
    None,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
    Default,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ReasoningBehavior {
    Effort { value: ReasoningEffort },
    Toggle { enabled: bool },
    BudgetTokens { value: i64 },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum CompiledReasoningBehavior {
    Effort { value: ReasoningEffort },
    Toggle { enabled: bool },
    BudgetTokens { value: i64 },
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ResolvedRequestDefaults {
    pub request: RequestDefaults,
    pub reasoning: Option<CompiledReasoningBehavior>,
}

impl ResolvedRequestDefaults {
    #[must_use]
    pub fn apply(
        &self,
        options: &ProviderOptions,
        mut request: oven_sdk::Request,
    ) -> oven_sdk::Request {
        request.inference.max_output_tokens = request
            .inference
            .max_output_tokens
            .or(self.request.max_output_tokens);
        request.inference.temperature = request
            .inference
            .temperature
            .or(self.request.temperature.map(|value| f64::from(value.get())));
        request.inference.top_p = request
            .inference
            .top_p
            .or(self.request.top_p.map(|value| f64::from(value.get())));
        if let Some(choice) = &self.request.tool_choice {
            request.tool_choice = match choice {
                ToolChoice::Auto => oven_sdk::ToolChoice::Auto,
                ToolChoice::None => oven_sdk::ToolChoice::None,
                ToolChoice::Required => oven_sdk::ToolChoice::Required,
                ToolChoice::Named(name) => oven_sdk::ToolChoice::Tool(name.clone()),
            };
        }
        if let Some(CompiledReasoningBehavior::Effort { value }) = self.reasoning {
            request.inference.reasoning_effort = Some(reasoning_effort_name(value).to_owned());
        }
        let mut namespaces = options.to_oven_namespaces();
        match options.compiled_adaptor {
            Some(AdaptorId::OpenaiCompatible)
                if self.request.seed.is_some() || !self.request.stop.is_empty() =>
            {
                let value = namespaces
                    .entry("openai_compatible".into())
                    .or_insert_with(|| serde_json::json!({"extra_body":{}}));
                let extra = value
                    .get_mut("extra_body")
                    .and_then(Value::as_object_mut)
                    .expect("compiled compatible options");
                if let Some(seed) = self.request.seed {
                    extra.insert("seed".into(), Value::from(seed));
                }
                if !self.request.stop.is_empty() {
                    extra.insert("stop".into(), serde_json::json!(self.request.stop));
                }
            }
            Some(AdaptorId::GoogleGemini | AdaptorId::GoogleVertexGemini)
                if self.request.seed.is_some() || !self.request.stop.is_empty() =>
            {
                let namespace = if options.compiled_adaptor == Some(AdaptorId::GoogleGemini) {
                    "google"
                } else {
                    "google_vertex"
                };
                let value = namespaces
                    .entry(namespace.into())
                    .or_insert_with(|| serde_json::json!({}));
                let object = value.as_object_mut().expect("compiled Google options");
                if let Some(seed) = self.request.seed {
                    object.insert("seed".into(), Value::from(seed));
                }
                if !self.request.stop.is_empty() {
                    object.insert("stopSequences".into(), serde_json::json!(self.request.stop));
                }
            }
            _ => {}
        }
        if let Some(reasoning) = &options.compiled_reasoning
            && matches!(
                reasoning,
                CompiledProviderReasoning::CohereToggle { .. }
                    | CompiledProviderReasoning::CohereBudget { .. }
            )
        {
            request.inference.reasoning_effort = Some(cohere_reasoning_label(reasoning));
        }
        for (namespace, value) in namespaces {
            request.provider_options.entry(namespace).or_insert(value);
        }
        request
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct ProviderOptions {
    pub api_version: Option<String>,
    pub beta: Vec<String>,
    #[serde(skip)]
    beta_present: bool,
    pub organization: Option<String>,
    pub project: Option<String>,
    pub store: Option<bool>,
    pub api_path: Option<String>,
    pub location: Option<String>,
    pub region: Option<String>,
    pub deployment: Option<String>,
    pub protocol_mode: Option<OpenResponsesMode>,
    #[serde(skip_deserializing, default, skip_serializing_if = "Option::is_none")]
    pub compiled_reasoning: Option<CompiledProviderReasoning>,
    #[serde(skip_deserializing, default, skip_serializing_if = "Option::is_none")]
    pub compiled_adaptor: Option<AdaptorId>,
}

impl<'de> Deserialize<'de> for ProviderOptions {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            api_version: Option<String>,
            beta: Option<Vec<String>>,
            organization: Option<String>,
            project: Option<String>,
            store: Option<bool>,
            api_path: Option<String>,
            location: Option<String>,
            region: Option<String>,
            deployment: Option<String>,
            protocol_mode: Option<OpenResponsesMode>,
        }
        let wire = Wire::deserialize(deserializer)?;
        Ok(Self {
            api_version: wire.api_version,
            beta_present: wire.beta.is_some(),
            beta: wire.beta.unwrap_or_default(),
            organization: wire.organization,
            project: wire.project,
            store: wire.store,
            api_path: wire.api_path,
            location: wire.location,
            region: wire.region,
            deployment: wire.deployment,
            protocol_mode: wire.protocol_mode,
            compiled_reasoning: None,
            compiled_adaptor: None,
        })
    }
}

impl ProviderOptions {
    fn to_oven_namespaces(&self) -> BTreeMap<String, Value> {
        let mut options = BTreeMap::new();
        if self.compiled_adaptor == Some(AdaptorId::Anthropic) && !self.beta.is_empty() {
            options.insert("anthropic".into(), serde_json::json!({"betas":self.beta}));
        }
        if let Some(reasoning) = &self.compiled_reasoning {
            match reasoning {
                CompiledProviderReasoning::AnthropicToggle { enabled } => {
                    options
                        .entry("anthropic".into())
                        .or_insert_with(|| serde_json::json!({}))
                        .as_object_mut()
                        .expect("compiled Anthropic options")
                        .insert(
                            "thinking".into(),
                            if *enabled {
                                serde_json::json!({"type":"adaptive"})
                            } else {
                                serde_json::json!({"type":"disabled"})
                            },
                        );
                }
                CompiledProviderReasoning::AnthropicBudget { value } => {
                    options
                        .entry("anthropic".into())
                        .or_insert_with(|| serde_json::json!({}))
                        .as_object_mut()
                        .expect("compiled Anthropic options")
                        .insert(
                            "thinking".into(),
                            serde_json::json!({"type":"enabled","budget_tokens":value}),
                        );
                }
                CompiledProviderReasoning::GoogleToggle { enabled } => {
                    let namespace = if self.compiled_adaptor == Some(AdaptorId::GoogleVertexGemini)
                    {
                        "google_vertex"
                    } else {
                        "google"
                    };
                    options.insert(namespace.into(), serde_json::json!({"thinkingConfig":{"thinkingBudget":if *enabled {-1} else {0}}}));
                }
                CompiledProviderReasoning::GoogleBudget { value } => {
                    let namespace = if self.compiled_adaptor == Some(AdaptorId::GoogleVertexGemini)
                    {
                        "google_vertex"
                    } else {
                        "google"
                    };
                    options.insert(
                        namespace.into(),
                        serde_json::json!({"thinkingConfig":{"thinkingBudget":value}}),
                    );
                }
                CompiledProviderReasoning::CohereToggle { .. }
                | CompiledProviderReasoning::CohereBudget { .. } => {}
            }
        }
        options
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum CompiledProviderReasoning {
    AnthropicToggle { enabled: bool },
    AnthropicBudget { value: i64 },
    GoogleToggle { enabled: bool },
    GoogleBudget { value: i64 },
    CohereToggle { enabled: bool },
    CohereBudget { value: i64 },
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OpenResponsesMode {
    Standard,
    Compact,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "operation", rename_all = "lowercase", deny_unknown_fields)]
pub enum VariantDirective {
    Add {
        display_name: Option<String>,
        #[serde(default)]
        defaults: RequestDefaults,
        #[serde(default)]
        options: ProviderOptions,
        reasoning: Option<ReasoningBehavior>,
    },
    Replace {
        display_name: Option<String>,
        #[serde(default)]
        defaults: RequestDefaults,
        #[serde(default)]
        options: ProviderOptions,
        reasoning: Option<ReasoningBehavior>,
    },
    Disable,
}

/// Builds every enabled provider/model/variant before returning one immutable set.
pub fn build_model_set(
    providers: &BTreeMap<ProviderId, ProviderDefinition>,
    catalog: &Catalog,
    credentials: Option<&crate::CredentialSnapshot>,
) -> Result<ModelSet, ModelBuildError> {
    if providers.is_empty() {
        return Err(ModelBuildError::EmptyProviders);
    }
    let mut entries = Vec::new();
    let mut safe_providers = Vec::new();
    for (provider_id, definition) in providers {
        let built = build_provider(provider_id, definition, catalog, credentials)?;
        safe_providers.push((
            provider_id.clone(),
            built
                .iter()
                .map(|(_, entry)| {
                    (
                        entry.key().clone(),
                        entry.display_name().to_owned(),
                        entry.behavior_fingerprint().clone(),
                        entry
                            .variants()
                            .iter()
                            .map(|(id, variant)| (id.clone(), variant.behavior_fingerprint.clone()))
                            .collect::<Vec<_>>(),
                        entry.default_variant().cloned(),
                        entry.is_available(),
                    )
                })
                .collect::<Vec<_>>(),
        ));
        entries.extend(built);
    }
    let fingerprint = Sha256Digest::hash("cookie-agent/model-set/v2", &safe_providers)?;
    ModelSet::new(entries, fingerprint).map_err(ModelBuildError::Set)
}

fn build_provider(
    provider_id: &ProviderId,
    definition: &ProviderDefinition,
    catalog: &Catalog,
    credentials: Option<&crate::CredentialSnapshot>,
) -> Result<Vec<(ModelKey, ModelEntry)>, ModelBuildError> {
    match definition {
        ProviderDefinition::ModelsDev(provider) => {
            build_models_dev(provider_id, provider, catalog, credentials)
        }
        ProviderDefinition::Explicit(provider) => build_explicit(provider_id, provider),
    }
}

fn build_models_dev(
    provider_id: &ProviderId,
    provider: &ModelsDevProvider,
    catalog: &Catalog,
    credentials: Option<&crate::CredentialSnapshot>,
) -> Result<Vec<(ModelKey, ModelEntry)>, ModelBuildError> {
    if provider.catalog_revision != format!("sha256:{}", crate::MODELS_DEV_ARTIFACT_SHA256) {
        return Err(ModelBuildError::CatalogRevision);
    }
    if provider.models.is_empty() {
        return Err(ModelBuildError::EmptyModels(provider_id.clone()));
    }
    let stored = credentials.and_then(|snapshot| snapshot.connections().get(provider_id));
    if stored.is_some_and(|connection| connection.catalog_revision != provider.catalog_revision) {
        return Err(ModelBuildError::CatalogRevision);
    }
    let source_provider = catalog.providers().get(provider_id.as_str());
    if let (Some(connection), Some(source_provider)) = (stored, source_provider)
        && (connection.credentials.len() != source_provider.credential_fields.len()
            || source_provider
                .credential_fields
                .iter()
                .any(|field| !connection.credentials.contains_key(field)))
    {
        return Err(ModelBuildError::AuthShape);
    }
    let unresolved = matches!(provider.auth, AuthDefinition::CredentialStore) && stored.is_none();
    let mut entries = Vec::new();
    for (model_id, config) in &provider.models {
        if !config.enabled {
            continue;
        }
        let source = catalog
            .model(provider_id.as_str(), model_id.as_str())
            .ok_or_else(|| {
                ModelBuildError::UnknownCatalogModel(
                    ModelKey::new(provider_id.clone(), model_id.clone()).expect("validated key"),
                )
            })?;
        let recipe = catalog
            .recipe(source)
            .map_err(ModelBuildError::UnsupportedCatalogModel)?;
        let adaptor = provider
            .adaptor
            .unwrap_or_else(|| adaptor_for_recipe(recipe));
        if adaptor != adaptor_for_recipe(recipe) {
            return Err(ModelBuildError::AdaptorOverride);
        }
        if provider.endpoint.is_some()
            && !matches!(
                recipe,
                crate::CatalogRecipe::OpenRouterChat | crate::CatalogRecipe::OpenAiCompatibleChat
            )
        {
            return Err(ModelBuildError::EndpointOverride);
        }
        let endpoint = provider
            .endpoint
            .clone()
            .unwrap_or_else(|| endpoint_for_recipe(recipe, catalog.effective_api(source)));
        validate_endpoint(&endpoint, adaptor)?;
        let auth = if let Some(connection) = stored {
            auth_from_stored(&connection.credentials, adaptor)?
        } else {
            provider.auth.clone()
        };
        validate_headers(&provider.headers, &auth, adaptor)?;
        let capabilities = capabilities_from_catalog(source, adaptor);
        validate_models_dev_options(&config.options, recipe)?;
        let recipe_defaults = recipe_defaults(source);
        let defaults = overlay_defaults(&recipe_defaults, &config.defaults);
        let options = overlay_options(&recipe_options(recipe), &config.options);
        let key = ModelKey::new(provider_id.clone(), model_id.clone())
            .map_err(|_| ModelBuildError::Identity)?;
        let generated = generated_variants(&source.reasoning_options, adaptor)?;
        let display = config
            .display_name
            .clone()
            .unwrap_or_else(|| source.name.clone());
        let entry = compile_entry(EntryInput {
            key: key.clone(),
            display_name: display,
            endpoint,
            adaptor,
            auth: &auth,
            fingerprint_auth: &provider.auth,
            source_revision: Some(&provider.catalog_revision),
            credential_fields: source_provider
                .map(|provider| provider.credential_fields.as_slice()),
            headers: &provider.headers,
            capabilities,
            defaults: &defaults,
            options: &options,
            generated,
            directives: &config.variants,
            configured_default: config.default_variant.as_ref(),
            source_default: None,
            available: !unresolved,
        })?;
        entries.push((key, entry));
    }
    Ok(entries)
}

fn build_explicit(
    provider_id: &ProviderId,
    provider: &ExplicitProvider,
) -> Result<Vec<(ModelKey, ModelEntry)>, ModelBuildError> {
    if provider.models.is_empty() {
        return Err(ModelBuildError::EmptyModels(provider_id.clone()));
    }
    if matches!(provider.auth, AuthDefinition::CredentialStore) {
        return Err(ModelBuildError::CredentialStoreExplicit);
    }
    validate_endpoint(&provider.endpoint, provider.adaptor)?;
    validate_headers(&provider.headers, &provider.auth, provider.adaptor)?;
    let mut entries = Vec::new();
    for (model_id, config) in &provider.models {
        if !config.enabled {
            continue;
        }
        validate_capabilities(&config.capabilities)?;
        let key = ModelKey::new(provider_id.clone(), model_id.clone())
            .map_err(|_| ModelBuildError::Identity)?;
        let entry = compile_entry(EntryInput {
            key: key.clone(),
            display_name: bounded(&config.display_name, "display_name")?.to_owned(),
            endpoint: provider.endpoint.clone(),
            adaptor: provider.adaptor,
            auth: &provider.auth,
            fingerprint_auth: &provider.auth,
            source_revision: None,
            credential_fields: None,
            headers: &provider.headers,
            capabilities: config.capabilities.clone(),
            defaults: &config.defaults,
            options: &config.options,
            generated: BTreeMap::new(),
            directives: &config.variants,
            configured_default: config.default_variant.as_ref(),
            source_default: None,
            available: true,
        })?;
        entries.push((key, entry));
    }
    Ok(entries)
}

struct EntryInput<'a> {
    key: ModelKey,
    display_name: String,
    endpoint: String,
    adaptor: AdaptorId,
    auth: &'a AuthDefinition,
    fingerprint_auth: &'a AuthDefinition,
    source_revision: Option<&'a str>,
    credential_fields: Option<&'a [String]>,
    headers: &'a BTreeMap<HeaderName, SecretString>,
    capabilities: ModelCapabilities,
    defaults: &'a RequestDefaults,
    options: &'a ProviderOptions,
    generated: BTreeMap<VariantId, GeneratedVariant>,
    directives: &'a BTreeMap<VariantId, VariantDirective>,
    configured_default: Option<&'a ConfiguredModelDefault>,
    source_default: Option<VariantId>,
    available: bool,
}

fn compile_entry(mut input: EntryInput<'_>) -> Result<ModelEntry, ModelBuildError> {
    bounded(&input.display_name, "display_name")?;
    validate_capabilities(&input.capabilities)?;
    validate_defaults(input.defaults, &input.capabilities)?;
    validate_options(input.options, input.adaptor)?;
    validate_auth(input.auth, input.adaptor)?;
    validate_adaptor_defaults(input.defaults, &input.capabilities, input.adaptor)?;
    let mut variants = std::mem::take(&mut input.generated);
    for (id, directive) in input.directives {
        match directive {
            VariantDirective::Add {
                display_name,
                defaults,
                options,
                reasoning,
            } => {
                if variants.contains_key(id) {
                    return Err(ModelBuildError::VariantAlreadyExists(id.clone()));
                }
                variants.insert(
                    id.clone(),
                    GeneratedVariant::explicit(id, display_name, defaults, options, reasoning),
                );
            }
            VariantDirective::Replace {
                display_name,
                defaults,
                options,
                reasoning,
            } => {
                if !variants.contains_key(id) {
                    return Err(ModelBuildError::VariantMissing(id.clone()));
                }
                variants.insert(
                    id.clone(),
                    GeneratedVariant::explicit(id, display_name, defaults, options, reasoning),
                );
            }
            VariantDirective::Disable => {
                if variants.remove(id).is_none() {
                    return Err(ModelBuildError::VariantMissing(id.clone()));
                }
            }
        }
    }
    let default_variant = match input.configured_default {
        None => input.source_default.clone(),
        Some(ConfiguredModelDefault::Base) => None,
        Some(ConfiguredModelDefault::Named(id)) => {
            if !variants.contains_key(id) {
                return Err(ModelBuildError::DefaultVariant(id.clone()));
            }
            Some(id.clone())
        }
    };
    if let Some(source) = &default_variant
        && !variants.contains_key(source)
    {
        return Err(ModelBuildError::DefaultVariant(source.clone()));
    }

    let profile = reasoning_profile(&variants, input.adaptor)?;
    let base_defaults = ResolvedRequestDefaults {
        request: input.defaults.clone(),
        reasoning: None,
    };
    let mut base_options = input.options.clone();
    base_options.compiled_adaptor = Some(input.adaptor);
    let base_fingerprint = behavior_fingerprint(&input, &base_defaults, &base_options, None)?;
    let concrete = concrete_model(
        &input,
        profile,
        &variants,
        &base_defaults.request,
        &base_options,
    )?
    .build()
    .map_err(ModelBuildError::Concrete)?;
    let descriptor = concrete.model.descriptor();
    let mut compiled_variants = BTreeMap::new();
    let mut variant_models = BTreeMap::new();
    let mut variant_descriptors = BTreeMap::new();
    for (id, variant) in &variants {
        bounded(&variant.display_name, "variant display_name")?;
        let defaults = overlay_defaults(input.defaults, &variant.defaults);
        validate_defaults(&defaults, &input.capabilities)?;
        validate_adaptor_defaults(&defaults, &input.capabilities, input.adaptor)?;
        let mut options = overlay_options(input.options, &variant.options);
        options.compiled_adaptor = Some(input.adaptor);
        validate_options(&options, input.adaptor)?;
        let reasoning = variant
            .reasoning
            .clone()
            .map(|reasoning| compile_reasoning(reasoning, input.adaptor, &mut options))
            .transpose()?;
        if reasoning.is_some() && !input.capabilities.reasoning {
            return Err(ModelBuildError::ReasoningCapability);
        }
        let resolved = ResolvedRequestDefaults {
            request: defaults,
            reasoning,
        };
        let fingerprint = behavior_fingerprint(&input, &resolved, &options, Some(id))?;
        let variant_concrete =
            concrete_model(&input, profile, &variants, &resolved.request, &options)?
                .build()
                .map_err(ModelBuildError::Concrete)?;
        variant_descriptors.insert(id.clone(), variant_concrete.model.descriptor());
        variant_models.insert(
            id.clone(),
            input.available.then_some(variant_concrete.model),
        );
        compiled_variants.insert(
            id.clone(),
            ModelVariant {
                id: id.clone(),
                display_name: variant.display_name.clone(),
                origin: variant.origin,
                defaults: resolved,
                provider_options: options,
                behavior_fingerprint: fingerprint,
            },
        );
    }
    Ok(ModelEntry::new(
        input.key,
        input.display_name,
        input.adaptor,
        input.available.then_some(concrete.model),
        descriptor,
        input.capabilities,
        base_defaults,
        base_options,
        compiled_variants,
        variant_models,
        variant_descriptors,
        default_variant,
        base_fingerprint,
        input.available,
    ))
}

fn concrete_model(
    input: &EntryInput<'_>,
    profile: ReasoningProfile,
    variants: &BTreeMap<VariantId, GeneratedVariant>,
    defaults: &RequestDefaults,
    options: &ProviderOptions,
) -> Result<ConcreteModel, ModelBuildError> {
    let auth = concrete_auth(input.auth, input.adaptor, options)?;
    let capabilities = to_oven_capabilities(&input.capabilities);
    let mut headers: BTreeMap<String, String> = input
        .headers
        .iter()
        .map(|(name, value)| (name.as_str().to_owned(), value.expose().to_owned()))
        .collect();
    if input.adaptor == AdaptorId::OpenaiCompatible
        && let AuthDefinition::ApiKey { key, header } = input.auth
    {
        headers.insert(
            header
                .as_ref()
                .map_or("x-api-key", HeaderName::as_str)
                .to_owned(),
            key.expose().to_owned(),
        );
    }
    let defaults = CommonDefaults {
        max_output_tokens: defaults.max_output_tokens,
        temperature: defaults.temperature.map(|value| f64::from(value.get())),
        top_p: defaults.top_p.map(|value| f64::from(value.get())),
        reasoning_effort: None,
        include_raw: false,
    };
    let adapter = concrete_adapter(
        input.adaptor,
        options,
        &input.key,
        &input.capabilities,
        profile,
        variants,
    )?;
    Ok(ConcreteModel {
        provider_id: concrete_provider_id(input.adaptor, &input.key),
        model_id: if matches!(
            input.adaptor,
            AdaptorId::AzureOpenaiChat | AdaptorId::AzureOpenaiResponses
        ) {
            options
                .deployment
                .clone()
                .unwrap_or_else(|| input.key.model_id().into_string())
        } else {
            input.key.model_id().into_string()
        },
        endpoint: effective_endpoint(&input.endpoint, input.adaptor, options)?,
        auth,
        headers,
        capabilities,
        defaults,
        adapter,
    })
}

fn concrete_adapter(
    adaptor: AdaptorId,
    options: &ProviderOptions,
    key: &ModelKey,
    capabilities: &ModelCapabilities,
    profile: ReasoningProfile,
    variants: &BTreeMap<VariantId, GeneratedVariant>,
) -> Result<AdapterConfig, ModelBuildError> {
    Ok(match adaptor {
        AdaptorId::Anthropic => AdapterConfig::Anthropic {
            settings: AnthropicSettingsConfig {
                timeouts: Default::default(),
                thinking: if capabilities.reasoning {
                    AnthropicThinkingSupportConfig::Both
                } else {
                    AnthropicThinkingSupportConfig::None
                },
                thinking_default_active: false,
                thinking_disable_allowed: capabilities.reasoning,
                thinking_disable_forbidden_efforts: BTreeSet::new(),
                effort: capabilities.reasoning,
                assistant_prefill: false,
                reject_non_default_sampling: false,
                native_context_discriminator: None,
            },
            options: Default::default(),
        },
        AdaptorId::OpenaiChat => AdapterConfig::OpenaiChat {
            settings: OpenAiChatSettingsConfig {
                system_message_role: SystemRoleConfig::Developer,
                max_tokens_field: MaxTokensFieldConfig::MaxCompletionTokens,
                stream_usage: false,
                structured_output: structured(capabilities),
                reasoning_field: ReasoningFieldConfig::None,
                routing_discriminator: None,
                timeouts: Default::default(),
            },
            options: OpenAiChatOptionsConfig::default(),
        },
        AdaptorId::OpenaiResponses => AdapterConfig::OpenaiResponses {
            settings: OpenAiResponsesSettingsConfig {
                routing_discriminator: None,
                compaction: OpenAiResponsesCompactionConfig::Unsupported,
                timeouts: Default::default(),
            },
            options: OpenAiResponsesOptionsConfig::default(),
        },
        AdaptorId::OpenaiCompatible => AdapterConfig::OpenaiCompatible {
            settings: CompatibleSettingsConfig {
                adapter_id: "cookie.openai-compatible.chat.v1".into(),
                system_message_role: SystemRoleConfig::System,
                max_tokens_field: MaxTokensFieldConfig::MaxTokens,
                stream_usage: false,
                structured_output: structured(capabilities),
                reasoning_field: if capabilities.reasoning {
                    ReasoningFieldConfig::ReasoningContent
                } else {
                    ReasoningFieldConfig::None
                },
                query: BTreeMap::new(),
                request_id_headers: vec!["x-request-id".into()],
                strict_sse_content_type: false,
                routing_discriminator: None,
                timeouts: Default::default(),
            },
            options: CompatibleOptionsConfig::default(),
        },
        AdaptorId::GoogleGemini => AdapterConfig::Google {
            settings: GoogleSettingsConfig {
                model_resource: format!("models/{}", key.model_id()),
                timeouts: Default::default(),
                thinking: match profile {
                    ReasoningProfile::Effort => GoogleThinkingSettingsConfig::Level {
                        effort_levels: [
                            ReasoningEffort::None,
                            ReasoningEffort::Minimal,
                            ReasoningEffort::Low,
                            ReasoningEffort::Medium,
                            ReasoningEffort::High,
                            ReasoningEffort::Xhigh,
                            ReasoningEffort::Max,
                            ReasoningEffort::Default,
                        ]
                        .into_iter()
                        .map(|effort| {
                            (
                                reasoning_effort_name(effort).to_owned(),
                                reasoning_effort_name(effort).to_owned(),
                            )
                        })
                        .collect(),
                    },
                    ReasoningProfile::ToggleOrBudget => GoogleThinkingSettingsConfig::Budget {
                        effort_budgets: BTreeMap::new(),
                    },
                    ReasoningProfile::None => GoogleThinkingSettingsConfig::Unsupported,
                },
                strict_functions: false,
                mixed_client_and_provider_tools: false,
                current_turn_signature_sentinel: false,
            },
            options: GoogleOptionsConfig::default(),
        },
        AdaptorId::GoogleVertexGemini => AdapterConfig::Vertex {
            settings: VertexSettingsConfig {
                project: required_option(&options.project, "options.project")?.to_owned(),
                location: required_option(&options.location, "options.location")?.to_owned(),
                resource: VertexResourceConfig::PublisherModel {
                    publisher: "google".into(),
                    model: key.model_id().into_string(),
                },
                thinking: match profile {
                    ReasoningProfile::Effort => VertexThinkingModeConfig::Level,
                    ReasoningProfile::ToggleOrBudget => VertexThinkingModeConfig::Budget,
                    ReasoningProfile::None => VertexThinkingModeConfig::Unsupported,
                },
                provider_tools: false,
                mixed_client_and_provider_tools: false,
                strict_functions: false,
                stream_function_call_arguments: false,
                media: VertexMediaConfig {
                    max_images: media_count(capabilities, MediaKind::Image),
                    max_https_images: 0,
                    max_documents: media_count(capabilities, MediaKind::Pdf),
                    max_audio: media_count(capabilities, MediaKind::Audio),
                    max_videos: 0,
                    max_https_videos: 0,
                    max_inline_image_bytes: media_bytes(capabilities, MediaKind::Image),
                    max_inline_pdf_bytes: media_bytes(capabilities, MediaKind::Pdf),
                    max_inline_text_bytes: 1,
                    url_schemes: vec!["https".into()],
                },
                timeouts: Default::default(),
            },
            options: VertexOptionsConfig::default(),
        },
        AdaptorId::AwsBedrockConverse => AdapterConfig::Bedrock {
            settings: BedrockSettingsConfig {
                region: required_option(&options.region, "options.region")?.to_owned(),
                reasoning_wire_format: if capabilities.reasoning {
                    BedrockReasoningFormatConfig::BedrockReasoningConfig
                } else {
                    BedrockReasoningFormatConfig::Unsupported
                },
                signed_reasoning: false,
                structured_output: if capabilities.structured_output {
                    BedrockStructuredOutputConfig::JsonSchema
                } else {
                    BedrockStructuredOutputConfig::Unsupported
                },
                max_event_message_bytes: 1024 * 1024,
                timeouts: Default::default(),
            },
            options: BedrockOptionsConfig::default(),
        },
        AdaptorId::AzureOpenaiChat => AdapterConfig::AzureChat {
            settings: AzureChatSettingsConfig {
                route: AzureRouteConfig::Dated {
                    version: required_option(&options.api_version, "options.api_version")?
                        .to_owned(),
                },
                revision: None,
                timeouts: Default::default(),
                system_role: SystemRoleConfig::System,
                max_tokens_field: MaxTokensFieldConfig::MaxTokens,
                stream_usage: false,
                structured_output: structured(capabilities),
                reasoning_field: ReasoningFieldConfig::None,
                omit_reasoning_sampling: false,
            },
            options: AzureChatOptionsConfig::default(),
        },
        AdaptorId::AzureOpenaiResponses => AdapterConfig::AzureResponses {
            settings: AzureResponsesSettingsConfig {
                route: AzureRouteConfig::Dated {
                    version: required_option(&options.api_version, "options.api_version")?
                        .to_owned(),
                },
                revision: None,
                compaction: Default::default(),
                timeouts: Default::default(),
            },
            options: AzureResponsesOptionsConfig::default(),
        },
        AdaptorId::CohereV2Chat => AdapterConfig::Cohere {
            settings: CohereSettingsConfig {
                timeouts: Default::default(),
                strict_tools: false,
                safety_mode: None,
                thinking: None,
                reasoning_effort: variants
                    .values()
                    .filter_map(|variant| match variant.reasoning.as_ref()? {
                        ReasoningBehavior::Toggle { enabled } => Some((
                            cohere_reasoning_label(&CompiledProviderReasoning::CohereToggle {
                                enabled: *enabled,
                            }),
                            CohereThinkingConfig {
                                enabled: *enabled,
                                token_budget: None,
                            },
                        )),
                        ReasoningBehavior::BudgetTokens { value } if *value >= 0 => Some((
                            cohere_reasoning_label(&CompiledProviderReasoning::CohereBudget {
                                value: *value,
                            }),
                            CohereThinkingConfig {
                                enabled: true,
                                token_budget: Some(*value as u64),
                            },
                        )),
                        _ => None,
                    })
                    .collect(),
                top_k: None,
                seed: None,
                frequency_penalty: None,
                presence_penalty: None,
                stop_sequences: Vec::new(),
                priority: None,
            },
            options: CohereOptionsConfig::default(),
        },
        AdaptorId::OpenResponses => AdapterConfig::OpenResponses {
            settings: OpenResponsesSettingsConfig {
                transport: OpenResponsesTransportConfig::Generic {
                    profile: match options.protocol_mode.unwrap_or(OpenResponsesMode::Standard) {
                        OpenResponsesMode::Standard => "standard",
                        OpenResponsesMode::Compact => "compact",
                    }
                    .into(),
                },
                timeouts: Default::default(),
                strict_json_schema: capabilities.structured_output,
                strict_tools: false,
                parallel_tool_calls: capabilities.parallel_tool_calls,
                store: false,
                include: Vec::new(),
                reasoning_summary: None,
            },
            options: OpenResponsesOptionsConfig::default(),
        },
    })
}

fn structured(capabilities: &ModelCapabilities) -> StructuredOutputConfig {
    if capabilities.structured_output {
        StructuredOutputConfig::JsonSchema
    } else {
        StructuredOutputConfig::Unsupported
    }
}

fn concrete_provider_id(adaptor: AdaptorId, key: &ModelKey) -> String {
    match adaptor {
        AdaptorId::Anthropic => "anthropic".into(),
        AdaptorId::OpenaiChat | AdaptorId::OpenaiResponses => "openai".into(),
        AdaptorId::GoogleGemini => "google".into(),
        AdaptorId::GoogleVertexGemini => "google.vertex".into(),
        AdaptorId::AwsBedrockConverse => "amazon.bedrock".into(),
        AdaptorId::AzureOpenaiChat | AdaptorId::AzureOpenaiResponses => "azure.openai".into(),
        AdaptorId::CohereV2Chat => "cohere".into(),
        AdaptorId::OpenaiCompatible | AdaptorId::OpenResponses => key.provider_id().into_string(),
    }
}

fn concrete_auth(
    auth: &AuthDefinition,
    adaptor: AdaptorId,
    options: &ProviderOptions,
) -> Result<ConcreteAuth, ModelBuildError> {
    Ok(match (adaptor, auth) {
        (
            AdaptorId::Anthropic
            | AdaptorId::GoogleGemini
            | AdaptorId::AzureOpenaiChat
            | AdaptorId::AzureOpenaiResponses,
            AuthDefinition::ApiKey { key, .. },
        ) => ConcreteAuth::ApiKey {
            value: key.expose().to_owned(),
        },
        (AdaptorId::OpenaiChat | AdaptorId::OpenaiResponses, AuthDefinition::Bearer { token }) => {
            ConcreteAuth::Openai {
                api_key: token.expose().to_owned(),
                organization: options.organization.clone(),
                project: options.project.clone(),
            }
        }
        (
            AdaptorId::OpenaiChat | AdaptorId::OpenaiResponses,
            AuthDefinition::ApiKey { key, .. },
        ) => ConcreteAuth::Openai {
            api_key: key.expose().to_owned(),
            organization: options.organization.clone(),
            project: options.project.clone(),
        },
        (AdaptorId::OpenaiChat | AdaptorId::OpenaiResponses, AuthDefinition::CredentialStore) => {
            ConcreteAuth::Openai {
                api_key: "unresolved-credential".into(),
                organization: options.organization.clone(),
                project: options.project.clone(),
            }
        }
        (AdaptorId::OpenaiCompatible, AuthDefinition::None | AuthDefinition::ApiKey { .. }) => {
            ConcreteAuth::None
        }
        (
            AdaptorId::OpenaiCompatible | AdaptorId::CohereV2Chat | AdaptorId::OpenResponses,
            AuthDefinition::Bearer { token },
        ) => ConcreteAuth::Bearer {
            token: token.expose().to_owned(),
        },
        (
            AdaptorId::Anthropic
            | AdaptorId::GoogleGemini
            | AdaptorId::AzureOpenaiChat
            | AdaptorId::AzureOpenaiResponses,
            AuthDefinition::CredentialStore,
        ) => ConcreteAuth::ApiKey {
            value: "unresolved-credential".into(),
        },
        (_, AuthDefinition::CredentialStore) => ConcreteAuth::Bearer {
            token: "unresolved-credential".into(),
        },
        (AdaptorId::GoogleVertexGemini, AuthDefinition::Fields { values }) => {
            ConcreteAuth::AccessToken {
                token: field(values, "access_token")?.expose().to_owned(),
            }
        }
        (AdaptorId::AwsBedrockConverse, AuthDefinition::Fields { values }) => {
            ConcreteAuth::AwsStatic {
                access_key_id: field(values, "access_key_id")?.expose().to_owned(),
                secret_access_key: field(values, "secret_access_key")?.expose().to_owned(),
                session_token: values
                    .iter()
                    .find(|(name, _)| name.0 == "session_token")
                    .map(|(_, value)| value.expose().to_owned()),
            }
        }
        _ => return Err(ModelBuildError::AuthShape),
    })
}

fn auth_from_stored(
    values: &BTreeMap<String, String>,
    adaptor: AdaptorId,
) -> Result<AuthDefinition, ModelBuildError> {
    if values.len() == 1 {
        let secret = SecretString(values.values().next().expect("length checked").clone());
        return Ok(
            if matches!(
                adaptor,
                AdaptorId::Anthropic
                    | AdaptorId::GoogleGemini
                    | AdaptorId::AzureOpenaiChat
                    | AdaptorId::AzureOpenaiResponses
            ) {
                AuthDefinition::ApiKey {
                    key: secret,
                    header: None,
                }
            } else {
                AuthDefinition::Bearer { token: secret }
            },
        );
    }
    Ok(AuthDefinition::Fields {
        values: values
            .iter()
            .map(|(name, value)| (AuthFieldName(name.clone()), SecretString(value.clone())))
            .collect(),
    })
}

fn field<'a>(
    values: &'a BTreeMap<AuthFieldName, SecretString>,
    name: &str,
) -> Result<&'a SecretString, ModelBuildError> {
    values
        .iter()
        .find(|(field, _)| field.0 == name)
        .map(|(_, value)| value)
        .ok_or(ModelBuildError::AuthShape)
}

fn generated_variants(
    options: &[CatalogReasoningOption],
    adaptor: AdaptorId,
) -> Result<BTreeMap<VariantId, GeneratedVariant>, ModelBuildError> {
    let mut generated: BTreeMap<VariantId, GeneratedVariant> = BTreeMap::new();
    for option in options {
        let (origin, variants) = match option {
            CatalogReasoningOption::Effort { values } => (
                VariantOrigin::ModelsDevEffort,
                values
                    .iter()
                    .map(|value| {
                        let (id, behavior) = match value {
                            Some(value) => (
                                reasoning_effort_name(*value),
                                ReasoningBehavior::Effort { value: *value },
                            ),
                            None => ("off", ReasoningBehavior::Toggle { enabled: false }),
                        };
                        (id.to_owned(), behavior)
                    })
                    .collect::<Vec<_>>(),
            ),
            CatalogReasoningOption::Toggle => (
                VariantOrigin::ModelsDevToggle,
                vec![
                    ("off".into(), ReasoningBehavior::Toggle { enabled: false }),
                    ("on".into(), ReasoningBehavior::Toggle { enabled: true }),
                ],
            ),
            CatalogReasoningOption::BudgetTokens { min, max } => {
                let mut values = Vec::new();
                if let Some(min) = min {
                    values.push((
                        if *min == -1 {
                            "budget-auto"
                        } else {
                            "budget-min"
                        }
                        .into(),
                        ReasoningBehavior::BudgetTokens { value: *min },
                    ));
                }
                if let Some(max) = max {
                    values.push((
                        "budget-max".into(),
                        ReasoningBehavior::BudgetTokens { value: *max },
                    ));
                }
                (VariantOrigin::ModelsDevBudgetTokens, values)
            }
        };
        for (id, reasoning) in variants {
            let id = VariantId::new(id).map_err(|_| ModelBuildError::Identity)?;
            let candidate = GeneratedVariant {
                display_name: display_from_id(&id),
                defaults: RequestDefaults::default(),
                options: ProviderOptions::default(),
                reasoning: Some(reasoning),
                origin,
            };
            if let Some(existing) = generated.get(&id) {
                let existing_patch = normalized_reasoning_patch(
                    existing.reasoning.clone().expect("generated reasoning"),
                    adaptor,
                )?;
                let candidate_patch = normalized_reasoning_patch(
                    candidate.reasoning.clone().expect("generated reasoning"),
                    adaptor,
                )?;
                if existing_patch != candidate_patch {
                    return Err(ModelBuildError::VariantCollision(id));
                }
                if origin_priority(origin) == origin_priority(existing.origin) {
                    return Err(ModelBuildError::VariantCollision(id));
                }
                if origin_priority(origin) > origin_priority(existing.origin) {
                    continue;
                }
            }
            generated.insert(id, candidate);
        }
    }
    Ok(generated)
}

#[derive(Clone)]
struct GeneratedVariant {
    display_name: String,
    defaults: RequestDefaults,
    options: ProviderOptions,
    reasoning: Option<ReasoningBehavior>,
    origin: VariantOrigin,
}
impl GeneratedVariant {
    fn explicit(
        id: &VariantId,
        display_name: &Option<String>,
        defaults: &RequestDefaults,
        options: &ProviderOptions,
        reasoning: &Option<ReasoningBehavior>,
    ) -> Self {
        Self {
            display_name: display_name.clone().unwrap_or_else(|| display_from_id(id)),
            defaults: defaults.clone(),
            options: options.clone(),
            reasoning: reasoning.clone(),
            origin: VariantOrigin::Explicit,
        }
    }
}

#[derive(Clone, Copy)]
enum ReasoningProfile {
    None,
    Effort,
    ToggleOrBudget,
}

fn reasoning_profile(
    variants: &BTreeMap<VariantId, GeneratedVariant>,
    adaptor: AdaptorId,
) -> Result<ReasoningProfile, ModelBuildError> {
    let effort = variants
        .values()
        .any(|variant| matches!(variant.reasoning, Some(ReasoningBehavior::Effort { .. })));
    let other = variants.values().any(|variant| {
        matches!(
            variant.reasoning,
            Some(ReasoningBehavior::Toggle { .. } | ReasoningBehavior::BudgetTokens { .. })
        )
    });
    if effort
        && other
        && matches!(
            adaptor,
            AdaptorId::GoogleGemini | AdaptorId::GoogleVertexGemini
        )
    {
        return Err(ModelBuildError::ReasoningEncoding);
    }
    Ok(if effort {
        ReasoningProfile::Effort
    } else if other {
        ReasoningProfile::ToggleOrBudget
    } else {
        ReasoningProfile::None
    })
}

fn origin_priority(origin: VariantOrigin) -> u8 {
    match origin {
        VariantOrigin::ModelsDevEffort => 0,
        VariantOrigin::ModelsDevToggle => 1,
        VariantOrigin::ModelsDevBudgetTokens => 2,
        VariantOrigin::Explicit => 3,
    }
}
fn display_from_id(id: &VariantId) -> String {
    id.as_str()
        .split(['-', '_', '.'])
        .map(|part| {
            let mut chars = part.chars();
            chars.next().map_or_else(String::new, |first| {
                first.to_uppercase().chain(chars).collect()
            })
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn compile_reasoning(
    value: ReasoningBehavior,
    adaptor: AdaptorId,
    options: &mut ProviderOptions,
) -> Result<CompiledReasoningBehavior, ModelBuildError> {
    Ok(match value {
        ReasoningBehavior::Effort { value }
            if matches!(
                adaptor,
                AdaptorId::Anthropic
                    | AdaptorId::OpenaiChat
                    | AdaptorId::OpenaiResponses
                    | AdaptorId::OpenaiCompatible
                    | AdaptorId::AwsBedrockConverse
                    | AdaptorId::AzureOpenaiChat
                    | AdaptorId::AzureOpenaiResponses
            ) =>
        {
            CompiledReasoningBehavior::Effort { value }
        }
        ReasoningBehavior::Effort { value }
            if matches!(
                adaptor,
                AdaptorId::GoogleGemini | AdaptorId::GoogleVertexGemini
            ) =>
        {
            CompiledReasoningBehavior::Effort { value }
        }
        ReasoningBehavior::Effort { .. } => return Err(ModelBuildError::ReasoningEncoding),
        ReasoningBehavior::Toggle { enabled } => {
            options.compiled_reasoning = Some(match adaptor {
                AdaptorId::Anthropic => CompiledProviderReasoning::AnthropicToggle { enabled },
                AdaptorId::GoogleGemini | AdaptorId::GoogleVertexGemini => {
                    CompiledProviderReasoning::GoogleToggle { enabled }
                }
                AdaptorId::CohereV2Chat => CompiledProviderReasoning::CohereToggle { enabled },
                _ => return Err(ModelBuildError::ReasoningEncoding),
            });
            CompiledReasoningBehavior::Toggle { enabled }
        }
        ReasoningBehavior::BudgetTokens { value }
            if value >= -1
                && (value != -1
                    || matches!(
                        adaptor,
                        AdaptorId::GoogleGemini | AdaptorId::GoogleVertexGemini
                    )) =>
        {
            options.compiled_reasoning = Some(match adaptor {
                AdaptorId::Anthropic => CompiledProviderReasoning::AnthropicBudget { value },
                AdaptorId::GoogleGemini | AdaptorId::GoogleVertexGemini => {
                    CompiledProviderReasoning::GoogleBudget { value }
                }
                AdaptorId::CohereV2Chat => CompiledProviderReasoning::CohereBudget { value },
                _ => return Err(ModelBuildError::ReasoningEncoding),
            });
            CompiledReasoningBehavior::BudgetTokens { value }
        }
        ReasoningBehavior::BudgetTokens { .. } => return Err(ModelBuildError::ReasoningBudget),
    })
}

fn normalized_reasoning_patch(
    reasoning: ReasoningBehavior,
    adaptor: AdaptorId,
) -> Result<Value, ModelBuildError> {
    let mut options = ProviderOptions {
        compiled_adaptor: Some(adaptor),
        ..ProviderOptions::default()
    };
    let resolved = compile_reasoning(reasoning, adaptor, &mut options)?;
    serde_json::to_value((resolved, options.to_oven_namespaces())).map_err(ModelBuildError::Json)
}

fn overlay_defaults(base: &RequestDefaults, overlay: &RequestDefaults) -> RequestDefaults {
    RequestDefaults {
        temperature: overlay.temperature.or(base.temperature),
        top_p: overlay.top_p.or(base.top_p),
        max_output_tokens: overlay.max_output_tokens.or(base.max_output_tokens),
        stop: if overlay.stop_present {
            overlay.stop.clone()
        } else {
            base.stop.clone()
        },
        stop_present: overlay.stop_present || base.stop_present,
        seed: overlay.seed.or(base.seed),
        tool_choice: overlay
            .tool_choice
            .clone()
            .or_else(|| base.tool_choice.clone()),
    }
}
fn overlay_options(base: &ProviderOptions, overlay: &ProviderOptions) -> ProviderOptions {
    ProviderOptions {
        api_version: overlay
            .api_version
            .clone()
            .or_else(|| base.api_version.clone()),
        beta: if overlay.beta_present {
            overlay.beta.clone()
        } else {
            base.beta.clone()
        },
        beta_present: overlay.beta_present || base.beta_present,
        organization: overlay
            .organization
            .clone()
            .or_else(|| base.organization.clone()),
        project: overlay.project.clone().or_else(|| base.project.clone()),
        store: overlay.store.or(base.store),
        api_path: overlay.api_path.clone().or_else(|| base.api_path.clone()),
        location: overlay.location.clone().or_else(|| base.location.clone()),
        region: overlay.region.clone().or_else(|| base.region.clone()),
        deployment: overlay
            .deployment
            .clone()
            .or_else(|| base.deployment.clone()),
        protocol_mode: overlay.protocol_mode.or(base.protocol_mode),
        compiled_reasoning: None,
        compiled_adaptor: None,
    }
}

fn behavior_fingerprint(
    input: &EntryInput<'_>,
    defaults: &ResolvedRequestDefaults,
    options: &ProviderOptions,
    variant: Option<&VariantId>,
) -> Result<Sha256Digest, ModelBuildError> {
    #[derive(Serialize)]
    struct Safe<'a> {
        key: &'a ModelKey,
        endpoint: &'a str,
        adaptor: AdaptorId,
        auth: Value,
        source_revision: Option<&'a str>,
        credential_fields: Option<&'a [String]>,
        header_names: Vec<&'a str>,
        capabilities: &'a ModelCapabilities,
        defaults: &'a ResolvedRequestDefaults,
        options: &'a ProviderOptions,
        variant: Option<&'a VariantId>,
    }
    let safe = Safe {
        key: &input.key,
        endpoint: &input.endpoint,
        adaptor: input.adaptor,
        auth: safe_auth(input.fingerprint_auth),
        source_revision: input.source_revision,
        credential_fields: input.credential_fields,
        header_names: input.headers.keys().map(HeaderName::as_str).collect(),
        capabilities: &input.capabilities,
        defaults,
        options,
        variant,
    };
    Sha256Digest::hash("cookie-agent/model-behavior/v2", &safe).map_err(ModelBuildError::Json)
}

fn safe_auth(auth: &AuthDefinition) -> Value {
    match auth {
        AuthDefinition::None => serde_json::json!({"type":"none"}),
        AuthDefinition::CredentialStore => serde_json::json!({"type":"credential_store"}),
        AuthDefinition::Bearer { .. } => serde_json::json!({"type":"bearer"}),
        AuthDefinition::ApiKey { header, .. } => {
            serde_json::json!({"type":"api_key","header":header.as_ref().map(HeaderName::as_str)})
        }
        AuthDefinition::Basic { .. } => serde_json::json!({"type":"basic"}),
        AuthDefinition::AwsSdk => serde_json::json!({"type":"aws_sdk"}),
        AuthDefinition::GoogleAdc => serde_json::json!({"type":"google_adc"}),
        AuthDefinition::Fields { values } => {
            serde_json::json!({"type":"fields","fields":values.keys().map(|field| field.0.as_str()).collect::<Vec<_>>()})
        }
    }
}

fn validate_endpoint(value: &str, adaptor: AdaptorId) -> Result<(), ModelBuildError> {
    let url = Url::parse(value).map_err(|_| ModelBuildError::Endpoint)?;
    if !url.username().is_empty() || url.password().is_some() || url.fragment().is_some() {
        return Err(ModelBuildError::Endpoint);
    }
    if url.query_pairs().any(|(name, _)| {
        matches!(
            name.to_ascii_lowercase().as_str(),
            "key" | "api_key" | "token" | "access_token" | "password"
        )
    }) {
        return Err(ModelBuildError::Endpoint);
    }
    let loopback = url
        .host_str()
        .is_some_and(|host| matches!(host, "localhost" | "127.0.0.1" | "::1"));
    if url.scheme() != "https"
        && !(url.scheme() == "http" && loopback && adaptor == AdaptorId::OpenaiCompatible)
    {
        return Err(ModelBuildError::Endpoint);
    }
    Ok(())
}

fn effective_endpoint(
    endpoint: &str,
    adaptor: AdaptorId,
    options: &ProviderOptions,
) -> Result<String, ModelBuildError> {
    if let Some(path) = options.api_path.as_deref() {
        if adaptor != AdaptorId::OpenaiCompatible {
            return Err(ModelBuildError::Options);
        }
        let base_path = path
            .strip_suffix("/chat/completions")
            .ok_or(ModelBuildError::Options)?;
        let mut url = Url::parse(endpoint).map_err(|_| ModelBuildError::Endpoint)?;
        url.set_path(if base_path.is_empty() { "/" } else { base_path });
        url.set_query(None);
        return Ok(url.to_string().trim_end_matches('/').to_owned());
    }
    if adaptor == AdaptorId::GoogleGemini
        && let Some(version) = options.api_version.as_deref()
    {
        let mut url = Url::parse(endpoint).map_err(|_| ModelBuildError::Endpoint)?;
        let mut segments = url
            .path_segments()
            .ok_or(ModelBuildError::Endpoint)?
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let last = segments.last_mut().ok_or(ModelBuildError::Endpoint)?;
        *last = version.to_owned();
        url.set_path(&format!("/{}", segments.join("/")));
        return Ok(url.to_string().trim_end_matches('/').to_owned());
    }
    Ok(endpoint.to_owned())
}

fn validate_headers(
    headers: &BTreeMap<HeaderName, SecretString>,
    auth: &AuthDefinition,
    adaptor: AdaptorId,
) -> Result<(), ModelBuildError> {
    for (name, value) in headers {
        value.validate()?;
        HeaderValue::from_str(value.expose()).map_err(|_| ModelBuildError::HeaderValue)?;
        if matches!(
            name.as_str(),
            "host" | "content-length" | "transfer-encoding" | "connection"
        ) {
            return Err(ModelBuildError::ControlledHeader);
        }
        if matches!(auth, AuthDefinition::Bearer { .. }) && name.as_str() == "authorization" {
            return Err(ModelBuildError::ControlledHeader);
        }
        if matches!(auth, AuthDefinition::CredentialStore)
            && match adaptor {
                AdaptorId::Anthropic => name.as_str() == "x-api-key",
                AdaptorId::GoogleGemini => false,
                AdaptorId::AzureOpenaiChat | AdaptorId::AzureOpenaiResponses => {
                    name.as_str() == "api-key"
                }
                _ => name.as_str() == "authorization",
            }
        {
            return Err(ModelBuildError::ControlledHeader);
        }
        if let AuthDefinition::ApiKey { header, .. } = auth {
            let owned = header.as_ref().map(HeaderName::as_str).or(match adaptor {
                AdaptorId::Anthropic => Some("x-api-key"),
                AdaptorId::OpenaiChat | AdaptorId::OpenaiResponses => Some("authorization"),
                AdaptorId::OpenaiCompatible => Some("x-api-key"),
                AdaptorId::AzureOpenaiChat | AdaptorId::AzureOpenaiResponses => Some("api-key"),
                AdaptorId::GoogleGemini
                | AdaptorId::GoogleVertexGemini
                | AdaptorId::AwsBedrockConverse
                | AdaptorId::CohereV2Chat
                | AdaptorId::OpenResponses => None,
            });
            if owned == Some(name.as_str()) {
                return Err(ModelBuildError::ControlledHeader);
            }
        }
    }
    Ok(())
}

fn validate_auth(auth: &AuthDefinition, adaptor: AdaptorId) -> Result<(), ModelBuildError> {
    match auth {
        AuthDefinition::Bearer { token } => token.validate()?,
        AuthDefinition::ApiKey { key, .. } => {
            key.validate()?;
            if adaptor != AdaptorId::GoogleGemini {
                HeaderValue::from_str(key.expose()).map_err(|_| ModelBuildError::HeaderValue)?;
            }
        }
        AuthDefinition::Basic { username, password } => {
            username.validate()?;
            password.validate()?;
        }
        AuthDefinition::Fields { values } => {
            if values.is_empty() {
                return Err(ModelBuildError::AuthShape);
            }
            for value in values.values() {
                value.validate()?;
            }
        }
        AuthDefinition::None
        | AuthDefinition::CredentialStore
        | AuthDefinition::AwsSdk
        | AuthDefinition::GoogleAdc => {}
    }
    let supported = match adaptor {
        AdaptorId::Anthropic => {
            api_key_header_is(auth, "x-api-key") || matches!(auth, AuthDefinition::CredentialStore)
        }
        AdaptorId::GoogleGemini => matches!(
            auth,
            AuthDefinition::ApiKey { header: None, .. } | AuthDefinition::CredentialStore
        ),
        AdaptorId::AzureOpenaiChat | AdaptorId::AzureOpenaiResponses => {
            api_key_header_is(auth, "api-key") || matches!(auth, AuthDefinition::CredentialStore)
        }
        AdaptorId::OpenaiChat | AdaptorId::OpenaiResponses => matches!(
            auth,
            AuthDefinition::Bearer { .. }
                | AuthDefinition::ApiKey { header: None, .. }
                | AuthDefinition::CredentialStore
        ),
        AdaptorId::OpenaiCompatible => matches!(
            auth,
            AuthDefinition::None
                | AuthDefinition::Bearer { .. }
                | AuthDefinition::ApiKey { .. }
                | AuthDefinition::CredentialStore
        ),
        AdaptorId::GoogleVertexGemini => {
            matches!(auth, AuthDefinition::Fields { values } if exact_fields(values, &["access_token"]))
        }
        AdaptorId::AwsBedrockConverse => {
            matches!(auth, AuthDefinition::Fields { values } if exact_fields(values, &["access_key_id", "secret_access_key"]) || exact_fields(values, &["access_key_id", "secret_access_key", "session_token"]))
        }
        AdaptorId::CohereV2Chat | AdaptorId::OpenResponses => matches!(
            auth,
            AuthDefinition::Bearer { .. } | AuthDefinition::CredentialStore
        ),
    };
    if supported {
        Ok(())
    } else {
        Err(ModelBuildError::AuthShape)
    }
}

fn exact_fields(values: &BTreeMap<AuthFieldName, SecretString>, names: &[&str]) -> bool {
    values.len() == names.len()
        && names
            .iter()
            .all(|name| values.keys().any(|field| field.0 == *name))
}

fn api_key_header_is(auth: &AuthDefinition, expected: &str) -> bool {
    matches!(
        auth,
        AuthDefinition::ApiKey { header, .. }
            if header.as_ref().is_none_or(|header| header.as_str() == expected)
    )
}

fn validate_capabilities(capabilities: &ModelCapabilities) -> Result<(), ModelBuildError> {
    if capabilities.input.is_empty()
        || capabilities.output.is_empty()
        || capabilities.context_tokens == 0
        || capabilities.output_tokens == 0
        || capabilities.output_tokens > capabilities.context_tokens
        || capabilities.output.contains(&Modality::Pdf)
    {
        return Err(ModelBuildError::Capabilities);
    }
    if capabilities.parallel_tool_calls && !capabilities.tool_calling {
        return Err(ModelBuildError::Capabilities);
    }
    for kind in [MediaKind::Image, MediaKind::Audio, MediaKind::Pdf] {
        let declared = capabilities.input.contains(&match kind {
            MediaKind::Image => Modality::Image,
            MediaKind::Audio => Modality::Audio,
            MediaKind::Pdf => Modality::Pdf,
        });
        match (declared, capabilities.media.get(&kind)) {
            (true, Some(media))
                if !media.mime_types.is_empty() && media.max_bytes > 0 && media.max_count > 0 => {}
            (false, None) => {}
            _ => return Err(ModelBuildError::Capabilities),
        }
    }
    Ok(())
}

fn validate_defaults(
    defaults: &RequestDefaults,
    capabilities: &ModelCapabilities,
) -> Result<(), ModelBuildError> {
    if defaults
        .temperature
        .is_some_and(|value| !(0.0..=2.0).contains(&value.get()))
        || defaults
            .top_p
            .is_some_and(|value| !(0.0..=1.0).contains(&value.get()))
    {
        return Err(ModelBuildError::Defaults);
    }
    if defaults.temperature.is_some() && !capabilities.temperature
        || defaults.top_p.is_some() && !capabilities.top_p
        || defaults.seed.is_some() && !capabilities.seed
    {
        return Err(ModelBuildError::Defaults);
    }
    if defaults
        .max_output_tokens
        .is_some_and(|value| value == 0 || value > capabilities.output_tokens)
    {
        return Err(ModelBuildError::Defaults);
    }
    if defaults.stop.len() > 8
        || defaults
            .stop
            .iter()
            .any(|value| value.is_empty() || value.len() > 256)
    {
        return Err(ModelBuildError::Defaults);
    }
    if defaults.tool_choice.is_some() && !capabilities.tool_calling {
        return Err(ModelBuildError::Defaults);
    }
    if let Some(ToolChoice::Named(name)) = &defaults.tool_choice
        && (name.is_empty() || name.len() > 64 || name.chars().any(char::is_control))
    {
        return Err(ModelBuildError::Defaults);
    }
    Ok(())
}

fn validate_adaptor_defaults(
    defaults: &RequestDefaults,
    capabilities: &ModelCapabilities,
    adaptor: AdaptorId,
) -> Result<(), ModelBuildError> {
    if capabilities.seed
        && !matches!(
            adaptor,
            AdaptorId::OpenaiCompatible | AdaptorId::GoogleGemini | AdaptorId::GoogleVertexGemini
        )
    {
        return Err(ModelBuildError::CapabilityEncoding);
    }
    if (defaults.seed.is_some() || !defaults.stop.is_empty())
        && !matches!(
            adaptor,
            AdaptorId::OpenaiCompatible | AdaptorId::GoogleGemini | AdaptorId::GoogleVertexGemini
        )
    {
        return Err(ModelBuildError::DefaultEncoding);
    }
    if matches!(
        adaptor,
        AdaptorId::GoogleGemini | AdaptorId::GoogleVertexGemini
    ) && defaults
        .seed
        .is_some_and(|seed| i32::try_from(seed).is_err())
    {
        return Err(ModelBuildError::DefaultEncoding);
    }
    Ok(())
}

fn validate_options(options: &ProviderOptions, adaptor: AdaptorId) -> Result<(), ModelBuildError> {
    for value in [
        &options.api_version,
        &options.organization,
        &options.project,
        &options.api_path,
        &options.location,
        &options.region,
        &options.deployment,
    ]
    .into_iter()
    .flatten()
    {
        bounded(value, "options")?;
    }
    if options.beta.len() > 32
        || options.beta.iter().collect::<BTreeSet<_>>().len() != options.beta.len()
        || options
            .beta
            .iter()
            .any(|value| bounded(value, "options.beta").is_err())
    {
        return Err(ModelBuildError::Options);
    }
    if options
        .api_path
        .as_ref()
        .is_some_and(|path| !path.starts_with('/') || path.contains('?') || path.contains('#'))
    {
        return Err(ModelBuildError::Options);
    }
    let allowed = match adaptor {
        AdaptorId::Anthropic => {
            options
                .api_version
                .as_deref()
                .is_none_or(|version| version == "2023-06-01")
                && options.organization.is_none()
                && options.project.is_none()
                && options.store.is_none()
                && options.api_path.is_none()
                && options.location.is_none()
                && options.region.is_none()
                && options.deployment.is_none()
                && options.protocol_mode.is_none()
        }
        AdaptorId::OpenaiChat => {
            options.api_version.is_none()
                && options.beta.is_empty()
                && options.store.is_none()
                && options.api_path.is_none()
                && options.location.is_none()
                && options.region.is_none()
                && options.deployment.is_none()
                && options.protocol_mode.is_none()
        }
        AdaptorId::OpenaiResponses => {
            options.api_version.is_none()
                && options.beta.is_empty()
                && options.store != Some(true)
                && options.api_path.is_none()
                && options.location.is_none()
                && options.region.is_none()
                && options.deployment.is_none()
                && options.protocol_mode.is_none()
        }
        AdaptorId::OpenaiCompatible => {
            options.api_version.is_none()
                && options.beta.is_empty()
                && options.organization.is_none()
                && options.project.is_none()
                && options.store.is_none()
                && options.location.is_none()
                && options.region.is_none()
                && options.deployment.is_none()
                && options.protocol_mode.is_none()
                && options
                    .api_path
                    .as_ref()
                    .is_none_or(|path| path.ends_with("/chat/completions"))
        }
        AdaptorId::GoogleGemini => {
            options.beta.is_empty()
                && options.organization.is_none()
                && options.project.is_none()
                && options.store.is_none()
                && options.api_path.is_none()
                && options.location.is_none()
                && options.region.is_none()
                && options.deployment.is_none()
                && options.protocol_mode.is_none()
        }
        AdaptorId::CohereV2Chat => {
            options
                .api_version
                .as_deref()
                .is_none_or(|version| version == "v2")
                && options.beta.is_empty()
                && options.organization.is_none()
                && options.project.is_none()
                && options.store.is_none()
                && options.api_path.is_none()
                && options.location.is_none()
                && options.region.is_none()
                && options.deployment.is_none()
                && options.protocol_mode.is_none()
        }
        AdaptorId::GoogleVertexGemini => {
            options.api_version.is_none()
                && options.beta.is_empty()
                && options.organization.is_none()
                && options.store.is_none()
                && options.api_path.is_none()
                && options.region.is_none()
                && options.deployment.is_none()
                && options.protocol_mode.is_none()
                && options.project.is_some()
                && options.location.is_some()
        }
        AdaptorId::AwsBedrockConverse => {
            options.api_version.is_none()
                && options.beta.is_empty()
                && options.organization.is_none()
                && options.project.is_none()
                && options.store.is_none()
                && options.api_path.is_none()
                && options.location.is_none()
                && options.deployment.is_none()
                && options.protocol_mode.is_none()
                && options.region.is_some()
        }
        AdaptorId::AzureOpenaiChat | AdaptorId::AzureOpenaiResponses => {
            options.beta.is_empty()
                && options.organization.is_none()
                && options.project.is_none()
                && options.store.is_none()
                && options.api_path.is_none()
                && options.location.is_none()
                && options.region.is_none()
                && options.protocol_mode.is_none()
                && options.deployment.is_some()
                && options.api_version.is_some()
        }
        AdaptorId::OpenResponses => {
            options.api_version.is_none()
                && options.beta.is_empty()
                && options.organization.is_none()
                && options.project.is_none()
                && options.store.is_none()
                && options.api_path.is_none()
                && options.location.is_none()
                && options.region.is_none()
                && options.deployment.is_none()
        }
    };
    if allowed {
        Ok(())
    } else {
        Err(ModelBuildError::Options)
    }
}

fn validate_models_dev_options(
    options: &ProviderOptions,
    recipe: crate::CatalogRecipe,
) -> Result<(), ModelBuildError> {
    let allowed = match recipe {
        crate::CatalogRecipe::Anthropic => {
            options.organization.is_none()
                && options.project.is_none()
                && options.store.is_none()
                && options.api_path.is_none()
                && options.location.is_none()
                && options.region.is_none()
                && options.deployment.is_none()
                && options.protocol_mode.is_none()
        }
        crate::CatalogRecipe::OpenAiChat => {
            options.api_version.is_none()
                && options.beta.is_empty()
                && options.store.is_none()
                && options.api_path.is_none()
                && options.location.is_none()
                && options.region.is_none()
                && options.deployment.is_none()
                && options.protocol_mode.is_none()
        }
        crate::CatalogRecipe::OpenAiResponses => {
            options.api_version.is_none()
                && options.beta.is_empty()
                && options.store != Some(true)
                && options.api_path.is_none()
                && options.location.is_none()
                && options.region.is_none()
                && options.deployment.is_none()
                && options.protocol_mode.is_none()
        }
        crate::CatalogRecipe::OpenRouterChat | crate::CatalogRecipe::OpenAiCompatibleChat => {
            options.api_version.is_none()
                && options.beta.is_empty()
                && options.organization.is_none()
                && options.project.is_none()
                && options.store.is_none()
                && options.location.is_none()
                && options.region.is_none()
                && options.deployment.is_none()
                && options.protocol_mode.is_none()
        }
        crate::CatalogRecipe::Google => {
            options.beta.is_empty()
                && options.organization.is_none()
                && options.project.is_none()
                && options.store.is_none()
                && options.api_path.is_none()
                && options.location.is_none()
                && options.region.is_none()
                && options.deployment.is_none()
                && options.protocol_mode.is_none()
        }
        crate::CatalogRecipe::Cohere => {
            options
                .api_version
                .as_deref()
                .is_none_or(|value| value == "v2")
                && options.beta.is_empty()
                && options.organization.is_none()
                && options.project.is_none()
                && options.store.is_none()
                && options.api_path.is_none()
                && options.location.is_none()
                && options.region.is_none()
                && options.deployment.is_none()
                && options.protocol_mode.is_none()
        }
    };
    if allowed {
        Ok(())
    } else {
        Err(ModelBuildError::Options)
    }
}

fn recipe_defaults(model: &crate::CatalogModel) -> RequestDefaults {
    RequestDefaults {
        max_output_tokens: Some(model.limits.output.min(16_384)),
        ..RequestDefaults::default()
    }
}

fn recipe_options(recipe: crate::CatalogRecipe) -> ProviderOptions {
    match recipe {
        crate::CatalogRecipe::Anthropic => ProviderOptions {
            api_version: Some("2023-06-01".into()),
            ..ProviderOptions::default()
        },
        crate::CatalogRecipe::OpenAiResponses => ProviderOptions {
            store: Some(false),
            ..ProviderOptions::default()
        },
        crate::CatalogRecipe::Google => ProviderOptions {
            api_version: Some("v1beta".into()),
            ..ProviderOptions::default()
        },
        crate::CatalogRecipe::Cohere => ProviderOptions {
            api_version: Some("v2".into()),
            ..ProviderOptions::default()
        },
        crate::CatalogRecipe::OpenAiChat
        | crate::CatalogRecipe::OpenRouterChat
        | crate::CatalogRecipe::OpenAiCompatibleChat => ProviderOptions::default(),
    }
}

fn reviewed_media_baseline(kind: MediaKind) -> (BTreeSet<MimeType>, u64, u32) {
    let values: &[&str] = match kind {
        MediaKind::Image => &["image/jpeg", "image/png", "image/gif", "image/webp"],
        MediaKind::Audio => &["audio/mpeg", "audio/wav", "audio/ogg"],
        MediaKind::Pdf => &["application/pdf"],
    };
    let mime_types = values
        .iter()
        .map(|value| MimeType((*value).to_owned()))
        .collect();
    match kind {
        MediaKind::Image => (mime_types, 20 * 1024 * 1024, 20),
        MediaKind::Audio => (mime_types, 25 * 1024 * 1024, 5),
        MediaKind::Pdf => (mime_types, 32 * 1024 * 1024, 5),
    }
}

fn media_count(capabilities: &ModelCapabilities, kind: MediaKind) -> usize {
    capabilities
        .media
        .get(&kind)
        .map_or(0, |media| media.max_count as usize)
}

fn media_bytes(capabilities: &ModelCapabilities, kind: MediaKind) -> usize {
    capabilities
        .media
        .get(&kind)
        .map_or(1, |media| media.max_bytes as usize)
}

fn cohere_reasoning_label(reasoning: &CompiledProviderReasoning) -> String {
    match reasoning {
        CompiledProviderReasoning::CohereToggle { enabled } => {
            format!("cookie-toggle-{}", if *enabled { "on" } else { "off" })
        }
        CompiledProviderReasoning::CohereBudget { value } => {
            format!("cookie-budget-{value}")
        }
        _ => unreachable!("Cohere label requested for non-Cohere reasoning"),
    }
}

fn capabilities_from_catalog(model: &crate::CatalogModel, adaptor: AdaptorId) -> ModelCapabilities {
    let input = model
        .modalities
        .input
        .iter()
        .filter_map(|value| match value.as_str() {
            "text" => Some(Modality::Text),
            "image" if model.capabilities.attachment => Some(Modality::Image),
            "audio" if model.capabilities.attachment => Some(Modality::Audio),
            "pdf" if model.capabilities.attachment => Some(Modality::Pdf),
            _ => None,
        })
        .collect::<BTreeSet<_>>();
    let mut media = BTreeMap::new();
    for kind in [MediaKind::Image, MediaKind::Audio, MediaKind::Pdf] {
        let modality = match kind {
            MediaKind::Image => Modality::Image,
            MediaKind::Audio => Modality::Audio,
            MediaKind::Pdf => Modality::Pdf,
        };
        if input.contains(&modality) {
            let (mime_types, max_bytes, max_count) = reviewed_media_baseline(kind);
            media.insert(
                kind,
                MediaCapability {
                    mime_types,
                    max_bytes,
                    max_count,
                },
            );
        }
    }
    ModelCapabilities {
        input,
        output: model
            .modalities
            .output
            .iter()
            .filter_map(|value| match value.as_str() {
                "text" => Some(Modality::Text),
                "image" => Some(Modality::Image),
                "audio" => Some(Modality::Audio),
                _ => None,
            })
            .collect(),
        context_tokens: model.limits.context,
        output_tokens: model.limits.output,
        tool_calling: model.capabilities.tool_call,
        parallel_tool_calls: false,
        structured_output: model.capabilities.structured_output,
        reasoning: model.capabilities.reasoning,
        temperature: model.capabilities.temperature,
        top_p: false,
        seed: false,
        native_replay: if model.capabilities.reasoning && adaptor == AdaptorId::OpenaiResponses {
            ReplayCapability::Required
        } else {
            ReplayCapability::Unsupported
        },
        native_compaction: CompactionCapability::Unsupported,
        cancellation: CancellationCapability::LocalOnly,
        media,
    }
}

fn to_oven_capabilities(value: &ModelCapabilities) -> OvenCapabilities {
    let mut features = Capability::MAX_OUTPUT_TOKENS;
    if value.tool_calling {
        features |= Capability::TOOL_CALLING;
    }
    if value.parallel_tool_calls {
        features |= Capability::PARALLEL_TOOLS;
    }
    if value.structured_output {
        features |= Capability::STRUCTURED_OUTPUT;
    }
    if value.reasoning {
        features |= Capability::REASONING;
    }
    if value.temperature {
        features |= Capability::TEMPERATURE;
    }
    if value.top_p {
        features |= Capability::TOP_P;
    }
    OvenCapabilities {
        features,
        limits: ModelLimits::new(Some(value.context_tokens), None, Some(value.output_tokens)),
        modalities: Modalities::new(
            value.input.iter().map(to_oven_modality),
            value.output.iter().map(to_oven_modality),
        ),
        media: OvenMediaCapabilities {
            input: value
                .media
                .iter()
                .map(|(kind, media)| {
                    (
                        to_oven_modality(&match kind {
                            MediaKind::Image => Modality::Image,
                            MediaKind::Audio => Modality::Audio,
                            MediaKind::Pdf => Modality::Pdf,
                        }),
                        MediaInputSupport::new(
                            media.mime_types.iter().map(|mime| mime.0.clone()),
                            MediaSourceSupport::INLINE_BYTES,
                        )
                        .expect("validated media capabilities"),
                    )
                })
                .collect(),
        },
        cancellation: match value.cancellation {
            CancellationCapability::LocalOnly => OvenCancellation::LocalOnly,
            CancellationCapability::Provider => OvenCancellation::RemoteBestEffort,
        },
        compaction: match value.native_compaction {
            CompactionCapability::Unsupported => OvenCompaction::Unsupported,
            CompactionCapability::Optional | CompactionCapability::Required => {
                OvenCompaction::Native
            }
        },
        replay: ReplayDeclaration {
            policy: if value.native_replay == ReplayCapability::Unsupported {
                ReplayPolicy::Never
            } else {
                ReplayPolicy::IfValid
            },
            capability: match value.native_replay {
                ReplayCapability::Unsupported => OvenReplay::Unsupported,
                ReplayCapability::Optional => OvenReplay::Optional,
                ReplayCapability::Required => OvenReplay::Required,
            },
            reasoning: value.reasoning && value.native_replay != ReplayCapability::Unsupported,
        },
    }
}
fn to_oven_modality(value: &Modality) -> OvenModality {
    match value {
        Modality::Text => OvenModality::text(),
        Modality::Image => OvenModality::image(),
        Modality::Audio => OvenModality::audio(),
        Modality::Pdf => OvenModality::pdf(),
    }
}

fn adaptor_for_recipe(recipe: crate::CatalogRecipe) -> AdaptorId {
    match recipe {
        crate::CatalogRecipe::Anthropic => AdaptorId::Anthropic,
        crate::CatalogRecipe::OpenAiResponses => AdaptorId::OpenaiResponses,
        crate::CatalogRecipe::OpenAiChat => AdaptorId::OpenaiChat,
        crate::CatalogRecipe::Google => AdaptorId::GoogleGemini,
        crate::CatalogRecipe::Cohere => AdaptorId::CohereV2Chat,
        crate::CatalogRecipe::OpenRouterChat | crate::CatalogRecipe::OpenAiCompatibleChat => {
            AdaptorId::OpenaiCompatible
        }
    }
}
fn endpoint_for_recipe(recipe: crate::CatalogRecipe, source: Option<String>) -> String {
    let endpoint = source.unwrap_or_else(|| {
        match recipe {
            crate::CatalogRecipe::Anthropic => "https://api.anthropic.com/v1",
            crate::CatalogRecipe::OpenAiResponses | crate::CatalogRecipe::OpenAiChat => {
                "https://api.openai.com/v1"
            }
            crate::CatalogRecipe::Google => "https://generativelanguage.googleapis.com/v1beta",
            crate::CatalogRecipe::Cohere => "https://api.cohere.com/v2/chat",
            crate::CatalogRecipe::OpenRouterChat => "https://openrouter.ai/api/v1",
            crate::CatalogRecipe::OpenAiCompatibleChat => "https://example.invalid/v1",
        }
        .into()
    });
    let required_suffix = match recipe {
        crate::CatalogRecipe::Anthropic
        | crate::CatalogRecipe::OpenAiResponses
        | crate::CatalogRecipe::OpenAiChat => Some("/v1"),
        crate::CatalogRecipe::Google => Some("/v1beta"),
        crate::CatalogRecipe::Cohere => Some("/v2/chat"),
        crate::CatalogRecipe::OpenRouterChat | crate::CatalogRecipe::OpenAiCompatibleChat => None,
    };
    required_suffix.map_or(endpoint.clone(), |suffix| {
        if endpoint.trim_end_matches('/').ends_with(suffix) {
            endpoint.trim_end_matches('/').to_owned()
        } else {
            format!("{}{suffix}", endpoint.trim_end_matches('/'))
        }
    })
}
fn reasoning_effort_name(value: ReasoningEffort) -> &'static str {
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
}
fn bounded<'a>(value: &'a str, _field: &str) -> Result<&'a str, ModelBuildError> {
    if value.is_empty() || value.len() > MAX_STRING || value.chars().any(char::is_control) {
        Err(ModelBuildError::StringBound)
    } else {
        Ok(value)
    }
}
fn required_option<'a>(
    value: &'a Option<String>,
    _field: &str,
) -> Result<&'a str, ModelBuildError> {
    value.as_deref().ok_or(ModelBuildError::Options)
}

#[derive(Debug, Error)]
pub enum ModelBuildError {
    #[error("providers must be nonempty")]
    EmptyProviders,
    #[error("provider `{0}` must include a nonempty models map")]
    EmptyModels(ProviderId),
    #[error("models.dev catalog revision does not match the pinned artifact")]
    CatalogRevision,
    #[error("unknown included models.dev model `{0}`")]
    UnknownCatalogModel(ModelKey),
    #[error("included models.dev model has no reviewed recipe: {0}")]
    UnsupportedCatalogModel(crate::UnsupportedReason),
    #[error("models.dev adaptor override is not a reviewed alternative")]
    AdaptorOverride,
    #[error("models.dev endpoint override is not permitted by the reviewed recipe")]
    EndpointOverride,
    #[error("credential_store is models.dev-only")]
    CredentialStoreExplicit,
    #[error("provider endpoint is invalid or unsupported")]
    Endpoint,
    #[error("provider auth is not supported by the selected adaptor")]
    AuthShape,
    #[error("secret values must be nonempty")]
    EmptySecret,
    #[error("invalid HTTP header name")]
    HeaderName,
    #[error("invalid HTTP header value")]
    HeaderValue,
    #[error("provider header is transport- or auth-controlled")]
    ControlledHeader,
    #[error("model capabilities are inconsistent")]
    Capabilities,
    #[error("selected adaptor cannot implement declared capabilities")]
    CapabilityEncoding,
    #[error("request defaults contradict capabilities or bounds")]
    Defaults,
    #[error("selected adaptor cannot encode request defaults exactly")]
    DefaultEncoding,
    #[error("provider options do not match the selected adaptor")]
    Options,
    #[error("reasoning requires model reasoning capability")]
    ReasoningCapability,
    #[error("reasoning token budget must be -1 or nonnegative")]
    ReasoningBudget,
    #[error("selected adaptor cannot encode reasoning behavior exactly")]
    ReasoningEncoding,
    #[error("variant `{0}` already exists")]
    VariantAlreadyExists(VariantId),
    #[error("variant `{0}` does not exist")]
    VariantMissing(VariantId),
    #[error("variant generation collision for `{0}`")]
    VariantCollision(VariantId),
    #[error("default variant `{0}` is not enabled")]
    DefaultVariant(VariantId),
    #[error("identity is invalid")]
    Identity,
    #[error("bounded string is invalid")]
    StringBound,
    #[error("Oven adaptor construction failed: {0}")]
    Concrete(#[source] crate::schema::ModelBuildError),
    #[error("model set construction failed: {0}")]
    Set(#[source] ModelSetError),
    #[error("model fingerprint canonicalization failed")]
    Json(#[source] serde_json::Error),
}

impl From<serde_json::Error> for ModelBuildError {
    fn from(value: serde_json::Error) -> Self {
        Self::Json(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(value: Value) -> ProviderOptions {
        serde_json::from_value(value).unwrap()
    }

    fn text_capabilities() -> ModelCapabilities {
        ModelCapabilities {
            input: BTreeSet::from([Modality::Text]),
            output: BTreeSet::from([Modality::Text]),
            context_tokens: 8192,
            output_tokens: 4096,
            tool_calling: false,
            parallel_tool_calls: false,
            structured_output: false,
            reasoning: true,
            temperature: true,
            top_p: true,
            seed: false,
            native_replay: ReplayCapability::Unsupported,
            native_compaction: CompactionCapability::Unsupported,
            cancellation: CancellationCapability::LocalOnly,
            media: BTreeMap::new(),
        }
    }

    fn fields(names: &[&str]) -> AuthDefinition {
        AuthDefinition::Fields {
            values: names
                .iter()
                .map(|name| (AuthFieldName((*name).into()), SecretString("secret".into())))
                .collect(),
        }
    }

    #[test]
    fn explicit_empty_stop_replaces_inherited_stop() {
        let base =
            serde_json::from_value::<RequestDefaults>(serde_json::json!({"stop":["x"]})).unwrap();
        let overlay =
            serde_json::from_value::<RequestDefaults>(serde_json::json!({"stop":[]})).unwrap();
        assert!(overlay_defaults(&base, &overlay).stop.is_empty());
    }

    #[test]
    fn omitted_stop_inherits() {
        let base =
            serde_json::from_value::<RequestDefaults>(serde_json::json!({"stop":["x"]})).unwrap();
        assert_eq!(
            overlay_defaults(&base, &RequestDefaults::default()).stop,
            ["x"]
        );
    }

    #[test]
    fn explicit_empty_beta_replaces_inherited_beta() {
        let base = options(serde_json::json!({"beta":["one"]}));
        let overlay = options(serde_json::json!({"beta":[]}));
        assert!(overlay_options(&base, &overlay).beta.is_empty());
    }

    #[test]
    fn omitted_beta_inherits() {
        let base = options(serde_json::json!({"beta":["one"]}));
        assert_eq!(
            overlay_options(&base, &ProviderOptions::default()).beta,
            ["one"]
        );
    }

    #[test]
    fn anthropic_beta_compiles_into_anthropic_namespace() {
        let mut value = options(serde_json::json!({"beta":["files-api"]}));
        value.compiled_adaptor = Some(AdaptorId::Anthropic);
        assert_eq!(
            value.to_oven_namespaces()["anthropic"]["betas"][0],
            "files-api"
        );
    }

    #[test]
    fn anthropic_fixed_api_version_is_honest() {
        let value = options(serde_json::json!({"api_version":"2023-06-01"}));
        assert!(validate_options(&value, AdaptorId::Anthropic).is_ok());
    }

    #[test]
    fn anthropic_unknown_api_version_is_rejected() {
        let value = options(serde_json::json!({"api_version":"tomorrow"}));
        assert!(validate_options(&value, AdaptorId::Anthropic).is_err());
    }

    #[test]
    fn responses_store_false_matches_oven_behavior() {
        let value = options(serde_json::json!({"store":false}));
        assert!(validate_options(&value, AdaptorId::OpenaiResponses).is_ok());
    }

    #[test]
    fn responses_store_true_is_rejected() {
        let value = options(serde_json::json!({"store":true}));
        assert!(validate_options(&value, AdaptorId::OpenaiResponses).is_err());
    }

    #[test]
    fn compatible_api_path_changes_effective_endpoint() {
        let value = options(serde_json::json!({"api_path":"/custom/chat/completions"}));
        assert_eq!(
            effective_endpoint(
                "https://example.test/v1",
                AdaptorId::OpenaiCompatible,
                &value
            )
            .unwrap(),
            "https://example.test/custom"
        );
    }

    #[test]
    fn compatible_unencodable_api_path_is_rejected() {
        let value = options(serde_json::json!({"api_path":"/custom/generate"}));
        assert!(validate_options(&value, AdaptorId::OpenaiCompatible).is_err());
    }

    #[test]
    fn gemini_api_version_changes_effective_endpoint() {
        let value = options(serde_json::json!({"api_version":"v1"}));
        assert_eq!(
            effective_endpoint(
                "https://example.test/v1beta",
                AdaptorId::GoogleGemini,
                &value
            )
            .unwrap(),
            "https://example.test/v1"
        );
    }

    #[test]
    fn cohere_v2_api_version_is_accepted() {
        let value = options(serde_json::json!({"api_version":"v2"}));
        assert!(validate_options(&value, AdaptorId::CohereV2Chat).is_ok());
    }

    #[test]
    fn cohere_unknown_api_version_is_rejected() {
        let value = options(serde_json::json!({"api_version":"v1"}));
        assert!(validate_options(&value, AdaptorId::CohereV2Chat).is_err());
    }

    #[test]
    fn vertex_reasoning_uses_vertex_namespace_and_camel_case() {
        let value = ProviderOptions {
            compiled_adaptor: Some(AdaptorId::GoogleVertexGemini),
            compiled_reasoning: Some(CompiledProviderReasoning::GoogleBudget { value: 512 }),
            ..ProviderOptions::default()
        };
        let namespace = value.to_oven_namespaces().remove("google_vertex").unwrap();
        assert_eq!(namespace["thinkingConfig"]["thinkingBudget"], 512);
    }

    #[test]
    fn anthropic_auto_budget_is_rejected() {
        let mut value = ProviderOptions::default();
        assert!(
            compile_reasoning(
                ReasoningBehavior::BudgetTokens { value: -1 },
                AdaptorId::Anthropic,
                &mut value
            )
            .is_err()
        );
    }

    #[test]
    fn cohere_auto_budget_is_rejected() {
        let mut value = ProviderOptions::default();
        assert!(
            compile_reasoning(
                ReasoningBehavior::BudgetTokens { value: -1 },
                AdaptorId::CohereV2Chat,
                &mut value
            )
            .is_err()
        );
    }

    #[test]
    fn google_auto_budget_is_encodable() {
        let mut value = ProviderOptions::default();
        assert!(
            compile_reasoning(
                ReasoningBehavior::BudgetTokens { value: -1 },
                AdaptorId::GoogleGemini,
                &mut value
            )
            .is_ok()
        );
    }

    #[test]
    fn vertex_fields_require_exact_access_token_name() {
        assert!(validate_auth(&fields(&["access_token"]), AdaptorId::GoogleVertexGemini).is_ok());
        assert!(validate_auth(&fields(&["token"]), AdaptorId::GoogleVertexGemini).is_err());
    }

    #[test]
    fn vertex_fields_reject_extras() {
        assert!(
            validate_auth(
                &fields(&["access_token", "extra"]),
                AdaptorId::GoogleVertexGemini
            )
            .is_err()
        );
    }

    #[test]
    fn bedrock_fields_require_exact_static_shape() {
        assert!(
            validate_auth(
                &fields(&["access_key_id", "secret_access_key"]),
                AdaptorId::AwsBedrockConverse
            )
            .is_ok()
        );
        assert!(validate_auth(&fields(&["access_key_id"]), AdaptorId::AwsBedrockConverse).is_err());
    }

    #[test]
    fn bedrock_fields_allow_only_optional_session_token() {
        assert!(
            validate_auth(
                &fields(&["access_key_id", "secret_access_key", "session_token"]),
                AdaptorId::AwsBedrockConverse
            )
            .is_ok()
        );
        assert!(
            validate_auth(
                &fields(&["access_key_id", "secret_access_key", "extra"]),
                AdaptorId::AwsBedrockConverse
            )
            .is_err()
        );
    }

    #[test]
    fn unsupported_sdk_and_basic_auth_are_rejected() {
        assert!(validate_auth(&AuthDefinition::GoogleAdc, AdaptorId::GoogleVertexGemini).is_err());
        assert!(validate_auth(&AuthDefinition::AwsSdk, AdaptorId::AwsBedrockConverse).is_err());
        assert!(
            validate_auth(
                &AuthDefinition::Basic {
                    username: SecretString("u".into()),
                    password: SecretString("p".into())
                },
                AdaptorId::OpenaiCompatible
            )
            .is_err()
        );
    }

    #[test]
    fn compatible_api_key_header_is_compiled_into_static_headers() {
        let key: ModelKey = "test/model".parse().unwrap();
        let auth = AuthDefinition::ApiKey {
            key: SecretString("value".into()),
            header: Some(HeaderName::new("x-token").unwrap()),
        };
        let capabilities = text_capabilities();
        let defaults = RequestDefaults::default();
        let provider_options = ProviderOptions::default();
        let headers = BTreeMap::new();
        let directives = BTreeMap::new();
        let input = EntryInput {
            key,
            display_name: "Test".into(),
            endpoint: "https://example.test/v1".into(),
            adaptor: AdaptorId::OpenaiCompatible,
            auth: &auth,
            fingerprint_auth: &auth,
            source_revision: None,
            credential_fields: None,
            headers: &headers,
            capabilities,
            defaults: &defaults,
            options: &provider_options,
            generated: BTreeMap::new(),
            directives: &directives,
            configured_default: None,
            source_default: None,
            available: true,
        };
        let concrete = concrete_model(
            &input,
            ReasoningProfile::None,
            &BTreeMap::new(),
            &defaults,
            &provider_options,
        )
        .unwrap();
        assert_eq!(concrete.headers["x-token"], "value");
    }

    #[test]
    fn media_capabilities_compile_into_oven_descriptor() {
        let mut capabilities = text_capabilities();
        capabilities.input.insert(Modality::Image);
        capabilities.media.insert(
            MediaKind::Image,
            MediaCapability {
                mime_types: BTreeSet::from([MimeType("image/png".into())]),
                max_bytes: 1024,
                max_count: 2,
            },
        );
        let oven = to_oven_capabilities(&capabilities);
        assert_eq!(
            oven.media.input[&OvenModality::image()].media_types,
            ["image/png"]
        );
    }

    #[test]
    fn pdf_output_is_rejected_by_contract() {
        let mut capabilities = text_capabilities();
        capabilities.output.insert(Modality::Pdf);
        assert!(validate_capabilities(&capabilities).is_err());
    }

    #[test]
    fn reviewed_recipe_default_is_bounded_by_source_output() {
        let catalog = Catalog::embedded().unwrap();
        let model = catalog.model("openai", "gpt-5.6-sol").unwrap();
        assert_eq!(
            recipe_defaults(model).max_output_tokens,
            Some(model.limits.output.min(16_384))
        );
    }

    #[test]
    fn models_dev_rejects_unreviewed_response_store_true() {
        let value = options(serde_json::json!({"store":true}));
        assert!(
            validate_models_dev_options(&value, crate::CatalogRecipe::OpenAiResponses).is_err()
        );
    }
}
