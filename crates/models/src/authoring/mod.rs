//! Strict schema-10 provider authoring data transfer objects.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    net::IpAddr,
};

use cookie_agent_identity::{
    AdapterId, AuthFieldName, AuthMethodId, AuthParameterId, ConfiguredModelDefault,
    ProviderModelId, SafeCode, SetupFieldId, VariantId,
};
use http::HeaderName as HttpHeaderName;
use serde::{Deserialize, Deserializer, Serialize, de};
use url::Url;
use zeroize::Zeroize;

pub use crate::model_types::{
    CancellationCapability, CompactionCapability, FiniteF32, MediaCapability, MediaKind, MimeType,
    Modality, ModelCapabilities, NativeCompactionConfig, ProviderOptions as CustomProviderOptions,
    ReasoningBehavior, ReplayCapability, RequestDefaults, ToolChoice, VariantDirective,
};

const MAX_ENDPOINT_BYTES: usize = 2048;
const MAX_SETUP_STRING_BYTES: usize = 8192;
const MAX_HEADER_ENTRIES: usize = 64;
const MAX_HEADER_NAME_BYTES: usize = 128;
const MAX_HEADER_VALUE_BYTES: usize = 8192;
const MAX_HEADER_AGGREGATE_BYTES: usize = 65_536;

/// Secret-bearing string. Formatting and serialization are deliberately unavailable.
#[derive(Clone, Eq, PartialEq)]
pub struct SecretString(String);

impl SecretString {
    pub fn new(value: impl Into<String>) -> Result<Self, AuthoringError> {
        let value = value.into();
        if value.is_empty() {
            Err(AuthoringError::EmptySecret)
        } else {
            Ok(Self(value))
        }
    }

    pub(crate) fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SecretString {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SecretString(<redacted>)")
    }
}

impl<'de> Deserialize<'de> for SecretString {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(de::Error::custom)
    }
}

impl Drop for SecretString {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

/// Absolute, query-free endpoint URL accepted by schema 10.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct EndpointUrl(String);

impl EndpointUrl {
    pub fn new(value: impl Into<String>) -> Result<Self, AuthoringError> {
        let value = value.into();
        if value.is_empty() || value.len() > MAX_ENDPOINT_BYTES {
            return Err(AuthoringError::Endpoint);
        }
        let parsed = Url::parse(&value).map_err(|_| AuthoringError::Endpoint)?;
        if !parsed.has_host()
            || !parsed.username().is_empty()
            || parsed.password().is_some()
            || parsed.query().is_some()
            || parsed.fragment().is_some()
        {
            return Err(AuthoringError::Endpoint);
        }
        let secure = parsed.scheme() == "https";
        let loopback_http = parsed.scheme() == "http"
            && parsed.host_str().is_some_and(|host| {
                host == "localhost" || host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
            });
        if !secure && !loopback_http {
            return Err(AuthoringError::Endpoint);
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for EndpointUrl {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(de::Error::custom)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct BoundedSetupString(String);

impl BoundedSetupString {
    pub fn new(value: impl Into<String>) -> Result<Self, AuthoringError> {
        let value = value.into();
        if value.len() <= MAX_SETUP_STRING_BYTES && !value.chars().any(char::is_control) {
            Ok(Self(value))
        } else {
            Err(AuthoringError::SetupValue)
        }
    }
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for BoundedSetupString {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(de::Error::custom)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(untagged)]
pub enum SafeSetupValue {
    String(BoundedSetupString),
    Code(SafeCode),
    Integer(i64),
    Bool(bool),
}

pub type ConfigSetupValue = SafeSetupValue;

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct SafeAuthParameterValue(String);

impl SafeAuthParameterValue {
    pub fn new(value: impl Into<String>) -> Result<Self, AuthoringError> {
        let value = value.into();
        if !value.is_empty()
            && value.len() <= 512
            && !value.chars().any(char::is_control)
            && !value.contains("${env:")
        {
            Ok(Self(value))
        } else {
            Err(AuthoringError::AuthParameter)
        }
    }
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for SafeAuthParameterValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(de::Error::custom)
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct HeaderName(String);

impl HeaderName {
    pub fn new(value: impl Into<String>) -> Result<Self, AuthoringError> {
        let value = value.into();
        if value.is_empty() || value.len() > MAX_HEADER_NAME_BYTES || !value.is_ascii() {
            return Err(AuthoringError::HeaderName);
        }
        let canonical = value.to_ascii_lowercase();
        HttpHeaderName::from_bytes(canonical.as_bytes()).map_err(|_| AuthoringError::HeaderName)?;
        Ok(Self(canonical))
    }
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
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

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct SafeStaticHeaderValue(String);

impl SafeStaticHeaderValue {
    pub fn new(value: impl Into<String>) -> Result<Self, AuthoringError> {
        let value = value.into();
        if value.len() <= MAX_HEADER_VALUE_BYTES
            && !value.contains("${env:")
            && !value
                .chars()
                .any(|c| c.is_control() || ('\u{7f}'..='\u{9f}').contains(&c))
        {
            Ok(Self(value))
        } else {
            Err(AuthoringError::HeaderValue)
        }
    }
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for SafeStaticHeaderValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(de::Error::custom)
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthOverride {
    pub method: AuthMethodId,
    pub values: BTreeMap<AuthFieldName, SecretString>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthDefinition {
    pub method: AuthMethodId,
    #[serde(default)]
    pub parameters: BTreeMap<AuthParameterId, SafeAuthParameterValue>,
    pub values: BTreeMap<AuthFieldName, SecretString>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PartialRequestDefaults {
    pub temperature: Option<FiniteF32>,
    pub top_p: Option<FiniteF32>,
    pub max_output_tokens: Option<u64>,
    pub stop: Option<Vec<String>>,
    pub seed: Option<i64>,
    pub tool_choice: Option<ToolChoice>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedModelOverride {
    pub enabled: Option<bool>,
    pub display_name: Option<String>,
    #[serde(default)]
    pub defaults: PartialRequestDefaults,
    #[serde(default)]
    pub variants: BTreeMap<VariantId, VariantDirective>,
    pub default_variant: Option<ConfiguredModelDefault>,
    pub shape: Option<ManagedModelShape>,
    #[serde(default)]
    pub compaction: NativeCompactionConfig,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ManagedModelShape {
    Chat,
    Responses,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelsDevProvider {
    pub base_url: Option<EndpointUrl>,
    #[serde(default)]
    pub setup: BTreeMap<SetupFieldId, ConfigSetupValue>,
    pub api_key: Option<SecretString>,
    pub auth_override: Option<AuthOverride>,
    pub shape: Option<ManagedModelShape>,
    #[serde(default)]
    pub model_overrides: BTreeMap<ProviderModelId, ManagedModelOverride>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CustomModelDefinition {
    #[serde(default = "yes")]
    pub enabled: bool,
    pub display_name: String,
    pub capabilities: ModelCapabilities,
    #[serde(default)]
    pub defaults: RequestDefaults,
    #[serde(default)]
    pub options: CustomProviderOptions,
    #[serde(default)]
    pub variants: BTreeMap<VariantId, VariantDirective>,
    pub default_variant: Option<ConfiguredModelDefault>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CustomProvider {
    pub endpoint: EndpointUrl,
    pub adaptor: AdapterId,
    #[serde(default)]
    pub setup: BTreeMap<SetupFieldId, ConfigSetupValue>,
    pub auth: AuthDefinition,
    #[serde(default, deserialize_with = "deserialize_headers")]
    pub headers: BTreeMap<HeaderName, SafeStaticHeaderValue>,
    pub models: BTreeMap<ProviderModelId, CustomModelDefinition>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "source", rename_all = "snake_case")]
pub enum ProviderDefinition {
    ModelsDev(ModelsDevProvider),
    Custom(CustomProvider),
}

impl ProviderDefinition {
    pub fn validate_for(
        &self,
        provider_id: &cookie_agent_identity::ProviderId,
    ) -> Result<(), AuthoringError> {
        match self {
            Self::ModelsDev(provider) => {
                if provider_id.as_str().starts_with("custom.") {
                    return Err(AuthoringError::ProviderNamespace);
                }
                if provider.api_key.is_some() && provider.auth_override.is_some() {
                    return Err(AuthoringError::AuthConflict);
                }
                if provider.base_url.is_some()
                    && provider.api_key.is_none()
                    && provider.auth_override.is_none()
                {
                    return Err(AuthoringError::BaseUrlWithoutAuth);
                }
                validate_display_overrides(provider)?;
            }
            Self::Custom(provider) => {
                if !provider_id.as_str().starts_with("custom.") {
                    return Err(AuthoringError::ProviderNamespace);
                }
                validate_custom(provider)?;
            }
        }
        Ok(())
    }
}

fn validate_display_overrides(provider: &ModelsDevProvider) -> Result<(), AuthoringError> {
    for value in provider
        .model_overrides
        .values()
        .filter_map(|model| model.display_name.as_deref())
    {
        validate_display_name(value)?;
    }
    Ok(())
}

fn validate_custom(provider: &CustomProvider) -> Result<(), AuthoringError> {
    if provider.models.is_empty() {
        return Err(AuthoringError::EmptyModels);
    }
    validate_auth_shape(&provider.auth, provider.adaptor.as_str())?;
    validate_header_ownership(&provider.headers, &provider.auth)?;
    for model in provider.models.values() {
        validate_display_name(&model.display_name)?;
        validate_capabilities(&model.capabilities)?;
        validate_defaults(&model.defaults, &model.capabilities)?;
        validate_custom_options(&model.options, provider.adaptor.as_str())?;
        if let Some(ConfiguredModelDefault::Named(id)) = &model.default_variant
            && !model.variants.contains_key(id)
        {
            return Err(AuthoringError::DefaultVariant);
        }
    }
    Ok(())
}

fn validate_display_name(value: &str) -> Result<(), AuthoringError> {
    if value.is_empty()
        || value.len() > 512
        || value.trim().is_empty()
        || value.chars().any(char::is_control)
    {
        Err(AuthoringError::DisplayName)
    } else {
        Ok(())
    }
}

fn validate_capabilities(value: &ModelCapabilities) -> Result<(), AuthoringError> {
    if value.input.is_empty()
        || value.output.is_empty()
        || value.context_tokens == 0
        || value.output_tokens == 0
        || value.output_tokens > value.context_tokens
        || value.parallel_tool_calls && !value.tool_calling
        || value.compaction != CompactionCapability::Unsupported
    {
        return Err(AuthoringError::Capabilities);
    }
    for kind in [MediaKind::Image, MediaKind::Audio, MediaKind::Pdf] {
        let modality = match kind {
            MediaKind::Image => Modality::Image,
            MediaKind::Audio => Modality::Audio,
            MediaKind::Pdf => Modality::Pdf,
        };
        let declared = value.input.contains(&modality);
        let media = value.media.get(&kind);
        if value.output.contains(&modality)
            || declared != media.is_some()
            || media
                .is_some_and(|m| m.mime_types.is_empty() || m.max_bytes == 0 || m.max_count == 0)
        {
            return Err(AuthoringError::Capabilities);
        }
    }
    Ok(())
}

fn validate_custom_options(
    options: &CustomProviderOptions,
    adaptor: &str,
) -> Result<(), AuthoringError> {
    let has_openai =
        options.organization.is_some() || options.project.is_some() || options.store.is_some();
    let has_anthropic = !options.beta.is_empty();
    let has_compatible = options.api_path.is_some();
    let has_setup_leak = options.api_version.is_some()
        || options.location.is_some()
        || options.region.is_some()
        || options.deployment.is_some();
    let valid = !has_setup_leak
        && match adaptor {
            "anthropic" | "anthropic-compatible" => !has_openai && !has_compatible,
            "openai-chat" | "openai-responses" => !has_anthropic && !has_compatible,
            "openai-compatible" => !has_anthropic && !has_openai,
            "google-gemini"
            | "google-vertex-gemini"
            | "aws-bedrock-converse"
            | "azure-openai-chat"
            | "azure-openai-responses"
            | "cohere-v2-chat" => !has_anthropic && !has_openai && !has_compatible,
            _ => false,
        };
    if valid {
        Ok(())
    } else {
        Err(AuthoringError::Options)
    }
}

fn validate_defaults(
    value: &RequestDefaults,
    capabilities: &ModelCapabilities,
) -> Result<(), AuthoringError> {
    if value
        .max_output_tokens
        .is_some_and(|n| n == 0 || n > capabilities.output_tokens)
        || value.temperature.is_some() && !capabilities.temperature
        || value.top_p.is_some() && !capabilities.top_p
        || value.seed.is_some() && !capabilities.seed
        || value.tool_choice.is_some() && !capabilities.tool_calling
    {
        Err(AuthoringError::Defaults)
    } else {
        Ok(())
    }
}

fn validate_auth_shape(auth: &AuthDefinition, adaptor: &str) -> Result<(), AuthoringError> {
    let method = auth.method.as_str();
    let allowed: BTreeSet<&str> = match adaptor {
        "openai-compatible" => ["bearer-api-key-v1", "api-key-header-v1", "no-auth-v1"]
            .into_iter()
            .collect(),
        "openai-chat" | "openai-responses" => {
            ["bearer-api-key-v1", "no-auth-v1"].into_iter().collect()
        }
        "anthropic" | "anthropic-compatible" => ["anthropic-api-key-v1"].into_iter().collect(),
        "google-gemini" => ["google-api-key-header-v1"].into_iter().collect(),
        "google-vertex-gemini" => ["oauth-access-token-v1"].into_iter().collect(),
        "aws-bedrock-converse" => ["aws-sigv4-credentials-v1"].into_iter().collect(),
        "azure-openai-chat" | "azure-openai-responses" => {
            ["azure-api-key-v1"].into_iter().collect()
        }
        "cohere-v2-chat" => ["bearer-api-key-v1"].into_iter().collect(),
        _ => return Err(AuthoringError::Adaptor),
    };
    if !allowed.contains(method) {
        return Err(AuthoringError::AuthMethod);
    }
    let fields: BTreeSet<&str> = auth.values.keys().map(AuthFieldName::as_str).collect();
    let exact = match method {
        "no-auth-v1" => fields.is_empty() && auth.parameters.is_empty(),
        "bearer-api-key-v1"
        | "anthropic-api-key-v1"
        | "google-api-key-header-v1"
        | "azure-api-key-v1" => fields == BTreeSet::from(["api_key"]) && auth.parameters.is_empty(),
        "oauth-access-token-v1" => {
            fields == BTreeSet::from(["access_token"]) && auth.parameters.is_empty()
        }
        "api-key-header-v1" => {
            fields == BTreeSet::from(["api_key"])
                && auth.parameters.len() == 1
                && auth
                    .parameters
                    .get(&AuthParameterId::new("header_name").expect("static id"))
                    .is_some_and(|v| matches!(v.as_str(), "x-api-key" | "api-key"))
        }
        "aws-sigv4-credentials-v1" => {
            (fields == BTreeSet::from(["access_key_id", "secret_access_key"])
                || fields
                    == BTreeSet::from(["access_key_id", "secret_access_key", "session_token"]))
                && auth.parameters.is_empty()
        }
        _ => false,
    };
    if exact {
        Ok(())
    } else {
        Err(AuthoringError::AuthShape)
    }
}

fn validate_header_ownership(
    headers: &BTreeMap<HeaderName, SafeStaticHeaderValue>,
    auth: &AuthDefinition,
) -> Result<(), AuthoringError> {
    const FORBIDDEN: &[&str] = &[
        "authorization",
        "host",
        "content-length",
        "transfer-encoding",
        "connection",
        "proxy-authorization",
        "cookie",
        "set-cookie",
        "accept",
        "content-type",
        "user-agent",
        "x-api-key",
        "x-goog-api-key",
        "api-key",
        "anthropic-version",
    ];
    if headers
        .keys()
        .any(|name| FORBIDDEN.contains(&name.as_str()))
    {
        return Err(AuthoringError::HeaderOwned);
    }
    if auth.method.as_str() == "api-key-header-v1"
        && let Some(name) = auth
            .parameters
            .get(&AuthParameterId::new("header_name").expect("static id"))
        && headers
            .keys()
            .any(|header| header.as_str() == name.as_str())
    {
        return Err(AuthoringError::HeaderOwned);
    }
    Ok(())
}

fn deserialize_headers<'de, D>(
    deserializer: D,
) -> Result<BTreeMap<HeaderName, SafeStaticHeaderValue>, D::Error>
where
    D: Deserializer<'de>,
{
    struct Visitor;
    impl<'de> de::Visitor<'de> for Visitor {
        type Value = BTreeMap<HeaderName, SafeStaticHeaderValue>;
        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("a bounded static header map")
        }
        fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
        where
            A: de::MapAccess<'de>,
        {
            let mut result = BTreeMap::new();
            let mut aggregate = 0usize;
            while let Some((name, value)) = map.next_entry::<HeaderName, SafeStaticHeaderValue>()? {
                aggregate = aggregate.saturating_add(name.as_str().len() + value.as_str().len());
                if result.len() == MAX_HEADER_ENTRIES || aggregate > MAX_HEADER_AGGREGATE_BYTES {
                    return Err(de::Error::custom("static header limits exceeded"));
                }
                if result.insert(name, value).is_some() {
                    return Err(de::Error::custom("duplicate case-insensitive header name"));
                }
            }
            Ok(result)
        }
    }
    deserializer.deserialize_map(Visitor)
}

const fn yes() -> bool {
    true
}

#[derive(Clone, Copy, Debug, thiserror::Error, Eq, PartialEq)]
pub enum AuthoringError {
    #[error("secret values must be nonempty")]
    EmptySecret,
    #[error("invalid endpoint URL")]
    Endpoint,
    #[error("invalid setup value")]
    SetupValue,
    #[error("invalid safe auth parameter")]
    AuthParameter,
    #[error("invalid static header name")]
    HeaderName,
    #[error("invalid static header value")]
    HeaderValue,
    #[error("provider ID does not match source namespace")]
    ProviderNamespace,
    #[error("api_key and auth_override are mutually exclusive")]
    AuthConflict,
    #[error("authored base_url requires same-definition auth")]
    BaseUrlWithoutAuth,
    #[error("authored base_url is forbidden by this provider recipe")]
    BaseUrlForbidden,
    #[error("api_key is ambiguous for this provider recipe")]
    AmbiguousApiKey,
    #[error("invalid managed setup fields")]
    SetupShape,
    #[error("custom models must be nonempty")]
    EmptyModels,
    #[error("invalid display name")]
    DisplayName,
    #[error("invalid model capabilities")]
    Capabilities,
    #[error("invalid request defaults")]
    Defaults,
    #[error("invalid adaptor-specific model options")]
    Options,
    #[error("default variant does not name an authored variant")]
    DefaultVariant,
    #[error("unsupported adaptor")]
    Adaptor,
    #[error("unsupported auth method")]
    AuthMethod,
    #[error("invalid auth parameter or credential fields")]
    AuthShape,
    #[error("static header is transport, protocol, or auth owned")]
    HeaderOwned,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn anthropic_compatible_provider(method: &str) -> CustomProvider {
        CustomProvider {
            endpoint: EndpointUrl::new("https://api.example.invalid/v1").expect("valid endpoint"),
            adaptor: AdapterId::new("anthropic-compatible").expect("valid adaptor ID"),
            setup: BTreeMap::new(),
            auth: AuthDefinition {
                method: AuthMethodId::new(method).expect("valid auth method ID"),
                parameters: BTreeMap::new(),
                values: BTreeMap::from([(
                    AuthFieldName::new("api_key").expect("valid auth field name"),
                    SecretString::new("secret").expect("nonempty secret"),
                )]),
            },
            headers: BTreeMap::new(),
            models: BTreeMap::new(),
        }
    }

    #[test]
    fn anthropic_compatible_accepts_only_anthropic_api_key_auth() {
        let supported = anthropic_compatible_provider("anthropic-api-key-v1");
        assert_eq!(
            validate_auth_shape(&supported.auth, supported.adaptor.as_str()),
            Ok(())
        );
        let unsupported = anthropic_compatible_provider("bearer-api-key-v1");
        assert_eq!(
            validate_auth_shape(&unsupported.auth, unsupported.adaptor.as_str()),
            Err(AuthoringError::AuthMethod)
        );
    }
}
