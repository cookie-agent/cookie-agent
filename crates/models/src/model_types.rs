//! Current dynamic model authoring and frozen-request value types.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Deserializer, Serialize, de};
use sha2::{Digest as _, Sha256};

/// Validated lowercase SHA-256 digest used by internal compiled state.
#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct Sha256Digest(String);

impl Sha256Digest {
    pub fn new(value: impl Into<String>) -> Result<Self, ModelTypeError> {
        let value = value.into();
        if value.len() == 64
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            Ok(Self(value))
        } else {
            Err(ModelTypeError::Fingerprint)
        }
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub(crate) fn hash(domain: &str, value: &impl Serialize) -> Result<Self, serde_json::Error> {
        let mut hasher = Sha256::new();
        hasher.update(domain.as_bytes());
        hasher.update([0]);
        hasher.update(serde_json::to_vec(value)?);
        Ok(Self(format!("{:x}", hasher.finalize())))
    }
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

impl MimeType {
    pub fn new(value: impl Into<String>) -> Result<Self, ModelTypeError> {
        let value = value.into();
        let valid = !value.is_empty()
            && value.len() <= 255
            && value
                .split_once('/')
                .is_some_and(|(kind, subtype)| !kind.is_empty() && !subtype.is_empty())
            && !value.chars().any(char::is_control)
            && !value.bytes().any(|byte| byte.is_ascii_whitespace());
        valid.then_some(Self(value)).ok_or(ModelTypeError::MimeType)
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for MimeType {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(de::Error::custom)
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
    pub cancellation: CancellationCapability,
    pub media: BTreeMap<MediaKind, MediaCapability>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FiniteF32(f32);

impl FiniteF32 {
    pub fn new(value: f32) -> Result<Self, ModelTypeError> {
        value
            .is_finite()
            .then_some(Self(value))
            .ok_or(ModelTypeError::Finite)
    }

    #[must_use]
    pub const fn get(self) -> f32 {
        self.0
    }
}

impl Serialize for FiniteF32 {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_f32(self.0)
    }
}

impl<'de> Deserialize<'de> for FiniteF32 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(f32::deserialize(deserializer)?).map_err(de::Error::custom)
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

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RequestDefaults {
    pub temperature: Option<FiniteF32>,
    pub top_p: Option<FiniteF32>,
    pub max_output_tokens: Option<u64>,
    #[serde(default)]
    pub stop: Vec<String>,
    pub seed: Option<i64>,
    pub tool_choice: Option<ToolChoice>,
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

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ResolvedRequestDefaults {
    pub request: RequestDefaults,
    pub reasoning: Option<ReasoningBehavior>,
}

impl ResolvedRequestDefaults {
    #[must_use]
    pub fn apply(
        &self,
        _options: &ProviderOptions,
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
        request.inference.reasoning_effort =
            request
                .inference
                .reasoning_effort
                .clone()
                .or_else(|| match &self.reasoning {
                    Some(ReasoningBehavior::Effort { value }) => Some(
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
                    Some(
                        ReasoningBehavior::Toggle { .. } | ReasoningBehavior::BudgetTokens { .. },
                    )
                    | None => None,
                });
        request
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderOptions {
    pub api_version: Option<String>,
    #[serde(default)]
    pub beta: Vec<String>,
    pub organization: Option<String>,
    pub project: Option<String>,
    pub store: Option<bool>,
    pub api_path: Option<String>,
    pub location: Option<String>,
    pub region: Option<String>,
    pub deployment: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
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

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ModelTypeError {
    #[error("invalid MIME type")]
    MimeType,
    #[error("number must be finite")]
    Finite,
    #[error("fingerprint must be lowercase SHA-256 hexadecimal")]
    Fingerprint,
}
