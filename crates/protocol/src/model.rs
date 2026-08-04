use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet},
    fmt,
};

use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::{ModelKey, ModelSelection, ProviderId, ProviderModelId, Sha256Digest, VariantId};

#[derive(
    Clone, Copy, Debug, Deserialize, Eq, Hash, JsonSchema, Ord, PartialEq, PartialOrd, Serialize, TS,
)]
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

pub(crate) struct LanguageModelDescriptorSchema;

impl JsonSchema for LanguageModelDescriptorSchema {
    fn inline_schema() -> bool {
        true
    }

    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("LanguageModelDescriptor")
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type":"object",
            "additionalProperties":false,
            "properties":{
                "identity":{
                    "type":"object","additionalProperties":false,
                    "properties":{
                        "provider_id":{"type":"string","minLength":1},
                        "model_id":{"type":"string","minLength":1}
                    },
                    "required":["provider_id","model_id"]
                },
                "adapter_id":{"type":"string","minLength":1},
                "capabilities":{
                    "type":"object","additionalProperties":false,
                    "properties":{
                        "features":{"type":"array","uniqueItems":true,"items":{"type":"string","enum":["tool_calling","parallel_tools","tool_input_deltas","reasoning","structured_output","temperature","top_p","max_output_tokens","prompt_caching","usage","provider_tools","sources"]}},
                        "limits":{
                            "type":"object","additionalProperties":false,
                            "properties":{
                                "context":{"type":["integer","null"],"minimum":0},
                                "input":{"type":["integer","null"],"minimum":0},
                                "output":{"type":["integer","null"],"minimum":0}
                            },
                            "required":["context","input","output"]
                        },
                        "modalities":{
                            "type":"object","additionalProperties":false,
                            "properties":{
                                "input":{"type":"array","minItems":1,"uniqueItems":true,"items":{"type":"string","minLength":1}},
                                "output":{"type":"array","minItems":1,"uniqueItems":true,"items":{"type":"string","minLength":1}}
                            },
                            "required":["input","output"]
                        },
                        "media":{
                            "type":"object","additionalProperties":false,
                            "properties":{
                                "input":{"type":"object","additionalProperties":{
                                    "type":"object","additionalProperties":false,
                                    "properties":{
                                        "media_types":{"type":"array","minItems":1,"items":{"type":"string","minLength":3}},
                                        "sources":{"type":"array","minItems":1,"uniqueItems":true,"items":{"type":"string","enum":["inline_bytes","inline_text","url","provider_reference"]}}
                                    },
                                    "required":["media_types","sources"]
                                }}
                            },
                            "required":["input"]
                        },
                        "cancellation":{"type":"string","enum":["local_only","remote_best_effort","unsupported"]},
                        "compaction":{"type":"string","enum":["native","unsupported"]},
                        "replay":{
                            "type":"object","additionalProperties":false,
                            "properties":{
                                "policy":{"type":"string","enum":["never","if_valid","always"]},
                                "capability":{"type":"string","enum":["required","optional","unsupported"]},
                                "reasoning":{"type":"boolean"}
                            },
                            "required":["policy","capability","reasoning"]
                        }
                    },
                    "required":["features","limits","modalities","media","cancellation","compaction","replay"]
                },
                "provider_metadata":{"type":"object"}
            },
            "required":["identity","adapter_id","capabilities","provider_metadata"]
        })
    }
}

#[derive(
    Clone, Copy, Debug, Deserialize, Eq, Hash, JsonSchema, Ord, PartialEq, PartialOrd, Serialize, TS,
)]
#[serde(rename_all = "snake_case")]
pub enum Modality {
    Text,
    Image,
    Audio,
    Pdf,
}

#[derive(
    Clone, Copy, Debug, Deserialize, Eq, Hash, JsonSchema, Ord, PartialEq, PartialOrd, Serialize, TS,
)]
#[serde(rename_all = "snake_case")]
pub enum MediaKind {
    Image,
    Audio,
    Pdf,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum ReplayCapability {
    Unsupported,
    Optional,
    Required,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum CompactionCapability {
    Unsupported,
    Optional,
    Required,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum CancellationCapability {
    LocalOnly,
    Provider,
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, TS)]
#[ts(type = "string")]
pub struct MimeType(String);

impl MimeType {
    pub const MAX_BYTES: usize = 255;
    pub fn new(value: impl Into<String>) -> Result<Self, ModelSchemaError> {
        let value = value.into();
        let valid = !value.is_empty()
            && value.len() <= Self::MAX_BYTES
            && !value.chars().any(char::is_control)
            && value
                .split_once('/')
                .is_some_and(|(left, right)| !left.is_empty() && !right.is_empty())
            && !value.bytes().any(|byte| byte.is_ascii_whitespace());
        if !valid {
            return Err(ModelSchemaError::InvalidMimeType);
        }
        Ok(Self(value))
    }
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl fmt::Display for MimeType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}
impl Serialize for MimeType {
    fn serialize<S>(&self, s: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        s.serialize_str(&self.0)
    }
}
impl<'de> Deserialize<'de> for MimeType {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Self::new(String::deserialize(d)?).map_err(serde::de::Error::custom)
    }
}
impl JsonSchema for MimeType {
    fn inline_schema() -> bool {
        true
    }
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("MimeType")
    }
    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({"type":"string","minLength":3,"maxLength":255,"pattern":"^[^/\\s]+/[^/\\s]+$"})
    }
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct MediaCapability {
    #[schemars(length(min = 1))]
    pub mime_types: BTreeSet<MimeType>,
    #[schemars(range(min = 1))]
    pub max_bytes: u64,
    #[schemars(range(min = 1))]
    pub max_count: u32,
}

#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct ModelCapabilities {
    #[schemars(length(min = 1))]
    pub input: BTreeSet<Modality>,
    #[schemars(length(min = 1))]
    pub output: BTreeSet<Modality>,
    #[schemars(range(min = 1))]
    pub context_tokens: u64,
    #[schemars(range(min = 1))]
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

impl ModelCapabilities {
    pub fn validate(&self) -> Result<(), ModelSchemaError> {
        if self.input.is_empty() || self.output.is_empty() {
            return Err(ModelSchemaError::EmptyModalities);
        }
        if self.context_tokens == 0
            || self.output_tokens == 0
            || self.output_tokens > self.context_tokens
        {
            return Err(ModelSchemaError::InvalidTokenLimits);
        }
        if self.parallel_tool_calls && !self.tool_calling {
            return Err(ModelSchemaError::ParallelToolsWithoutTools);
        }
        if self.output.contains(&Modality::Pdf) {
            return Err(ModelSchemaError::InvalidOutputModality);
        }
        for (kind, capability) in &self.media {
            let modality = match kind {
                MediaKind::Image => Modality::Image,
                MediaKind::Audio => Modality::Audio,
                MediaKind::Pdf => Modality::Pdf,
            };
            if !self.input.contains(&modality)
                || capability.mime_types.is_empty()
                || capability.max_bytes == 0
                || capability.max_count == 0
            {
                return Err(ModelSchemaError::InvalidMediaCapability);
            }
        }
        for modality in [Modality::Image, Modality::Audio, Modality::Pdf] {
            let kind = match modality {
                Modality::Image => MediaKind::Image,
                Modality::Audio => MediaKind::Audio,
                Modality::Pdf => MediaKind::Pdf,
                Modality::Text => unreachable!(),
            };
            if self.input.contains(&modality) != self.media.contains_key(&kind) {
                return Err(ModelSchemaError::InvalidMediaCapability);
            }
        }
        Ok(())
    }
}

impl<'de> Deserialize<'de> for ModelCapabilities {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            input: BTreeSet<Modality>,
            output: BTreeSet<Modality>,
            context_tokens: u64,
            output_tokens: u64,
            tool_calling: bool,
            parallel_tool_calls: bool,
            structured_output: bool,
            reasoning: bool,
            temperature: bool,
            top_p: bool,
            seed: bool,
            native_replay: ReplayCapability,
            native_compaction: CompactionCapability,
            cancellation: CancellationCapability,
            media: BTreeMap<MediaKind, MediaCapability>,
        }
        let wire = Wire::deserialize(deserializer)?;
        let value = Self {
            input: wire.input,
            output: wire.output,
            context_tokens: wire.context_tokens,
            output_tokens: wire.output_tokens,
            tool_calling: wire.tool_calling,
            parallel_tool_calls: wire.parallel_tool_calls,
            structured_output: wire.structured_output,
            reasoning: wire.reasoning,
            temperature: wire.temperature,
            top_p: wire.top_p,
            seed: wire.seed,
            native_replay: wire.native_replay,
            native_compaction: wire.native_compaction,
            cancellation: wire.cancellation,
            media: wire.media,
        };
        value.validate().map_err(serde::de::Error::custom)?;
        Ok(value)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, PartialOrd, TS)]
#[ts(type = "number")]
pub struct FiniteF32(f32);

impl FiniteF32 {
    pub fn new(value: f32) -> Result<Self, ModelSchemaError> {
        if !value.is_finite() {
            return Err(ModelSchemaError::NonFiniteNumber);
        }
        Ok(Self(value))
    }
    #[must_use]
    pub const fn get(self) -> f32 {
        self.0
    }
}
impl Serialize for FiniteF32 {
    fn serialize<S>(&self, s: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        s.serialize_f32(self.0)
    }
}
impl<'de> Deserialize<'de> for FiniteF32 {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Self::new(f32::deserialize(d)?).map_err(serde::de::Error::custom)
    }
}
impl JsonSchema for FiniteF32 {
    fn inline_schema() -> bool {
        true
    }
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("FiniteF32")
    }
    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({"type":"number"})
    }
}

struct StopSequenceSchema;
impl JsonSchema for StopSequenceSchema {
    fn inline_schema() -> bool {
        true
    }
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("StopSequence")
    }
    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({"type":"string","minLength":1,"maxLength":256})
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, TS)]
#[ts(type = "string")]
pub struct ModelToolName(String);
impl ModelToolName {
    pub fn new(value: impl Into<String>) -> Result<Self, ModelSchemaError> {
        let value = value.into();
        if value.is_empty() || value.len() > 64 || value.chars().any(char::is_control) {
            return Err(ModelSchemaError::InvalidToolName);
        }
        Ok(Self(value))
    }
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl Serialize for ModelToolName {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}
impl<'de> Deserialize<'de> for ModelToolName {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}
impl JsonSchema for ModelToolName {
    fn inline_schema() -> bool {
        true
    }
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("ModelToolName")
    }
    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({"type":"string","minLength":1,"maxLength":64,"pattern":"^[^\\p{Cc}\\p{Cf}]+$"})
    }
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum ToolChoice {
    Auto,
    None,
    Required,
    Named(ModelToolName),
}

#[derive(Clone, Debug, Default, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct RequestDefaults {
    #[serde(deserialize_with = "crate::deserialize_required_option")]
    #[schemars(with = "TemperatureSchema", required)]
    pub temperature: Option<FiniteF32>,
    #[serde(deserialize_with = "crate::deserialize_required_option")]
    #[schemars(with = "TopPSchema", required)]
    pub top_p: Option<FiniteF32>,
    #[serde(deserialize_with = "crate::deserialize_required_option")]
    #[schemars(with = "PositiveOptionalU64Schema", required)]
    pub max_output_tokens: Option<u64>,
    #[schemars(with = "Vec<StopSequenceSchema>")]
    #[schemars(length(max = 8))]
    pub stop: Vec<String>,
    #[serde(deserialize_with = "crate::deserialize_required_option")]
    #[schemars(with = "crate::NullableSchema<i64>", required)]
    pub seed: Option<i64>,
    #[serde(deserialize_with = "crate::deserialize_required_option")]
    #[schemars(with = "crate::NullableSchema<ToolChoice>", required)]
    pub tool_choice: Option<ToolChoice>,
}

struct PositiveOptionalU64Schema;
struct TemperatureSchema;
struct TopPSchema;
impl JsonSchema for PositiveOptionalU64Schema {
    fn inline_schema() -> bool {
        true
    }
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("PositiveOptionalU64")
    }
    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({"anyOf":[{"type":"integer","minimum":1},{"type":"null"}]})
    }
}
impl JsonSchema for TemperatureSchema {
    fn inline_schema() -> bool {
        true
    }
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("Temperature")
    }
    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({"anyOf":[{"type":"number","minimum":0.0,"maximum":2.0},{"type":"null"}]})
    }
}
impl JsonSchema for TopPSchema {
    fn inline_schema() -> bool {
        true
    }
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("TopP")
    }
    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({"anyOf":[{"type":"number","minimum":0.0,"maximum":1.0},{"type":"null"}]})
    }
}

impl RequestDefaults {
    pub fn validate(&self) -> Result<(), ModelSchemaError> {
        if self
            .temperature
            .is_some_and(|v| !(0.0..=2.0).contains(&v.get()))
        {
            return Err(ModelSchemaError::TemperatureOutOfRange);
        }
        if self.top_p.is_some_and(|v| !(0.0..=1.0).contains(&v.get())) {
            return Err(ModelSchemaError::TopPOutOfRange);
        }
        if self.max_output_tokens == Some(0) {
            return Err(ModelSchemaError::InvalidMaxOutputTokens);
        }
        if self.stop.len() > 8
            || self
                .stop
                .iter()
                .any(|stop| stop.is_empty() || stop.len() > 256)
        {
            return Err(ModelSchemaError::InvalidStopSequence);
        }
        Ok(())
    }
}

impl<'de> Deserialize<'de> for RequestDefaults {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            #[serde(deserialize_with = "crate::deserialize_required_option")]
            temperature: Option<FiniteF32>,
            #[serde(deserialize_with = "crate::deserialize_required_option")]
            top_p: Option<FiniteF32>,
            #[serde(deserialize_with = "crate::deserialize_required_option")]
            max_output_tokens: Option<u64>,
            stop: Vec<String>,
            #[serde(deserialize_with = "crate::deserialize_required_option")]
            seed: Option<i64>,
            #[serde(deserialize_with = "crate::deserialize_required_option")]
            tool_choice: Option<ToolChoice>,
        }
        let wire = Wire::deserialize(deserializer)?;
        let value = Self {
            temperature: wire.temperature,
            top_p: wire.top_p,
            max_output_tokens: wire.max_output_tokens,
            stop: wire.stop,
            seed: wire.seed,
            tool_choice: wire.tool_choice,
        };
        value.validate().map_err(serde::de::Error::custom)?;
        Ok(value)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
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

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum CompiledReasoningBehavior {
    Effort {
        value: ReasoningEffort,
    },
    Toggle {
        enabled: bool,
    },
    BudgetTokens {
        #[schemars(range(min = -1))]
        value: i64,
    },
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct ResolvedRequestDefaults {
    pub request: RequestDefaults,
    #[serde(deserialize_with = "crate::deserialize_required_option")]
    #[schemars(with = "crate::NullableSchema<CompiledReasoningBehavior>", required)]
    pub reasoning: Option<CompiledReasoningBehavior>,
}

impl ResolvedRequestDefaults {
    pub fn validate(&self) -> Result<(), ModelSchemaError> {
        self.request.validate()?;
        if matches!(
            self.reasoning,
            Some(CompiledReasoningBehavior::BudgetTokens { value }) if value < -1
        ) {
            return Err(ModelSchemaError::InvalidReasoningBudget);
        }
        Ok(())
    }
}

impl<'de> Deserialize<'de> for ResolvedRequestDefaults {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            request: RequestDefaults,
            #[serde(deserialize_with = "crate::deserialize_required_option")]
            reasoning: Option<CompiledReasoningBehavior>,
        }
        let wire = Wire::deserialize(deserializer)?;
        let value = Self {
            request: wire.request,
            reasoning: wire.reasoning,
        };
        value.validate().map_err(serde::de::Error::custom)?;
        Ok(value)
    }
}

struct ProviderOptionString512Schema;
struct ProviderOptionString256Schema;
struct ProviderApiPathSchema;
struct ProviderBetaSchema;
impl JsonSchema for ProviderOptionString512Schema {
    fn inline_schema() -> bool {
        true
    }
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("ProviderOptionString512")
    }
    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({"type":"string","minLength":1,"maxLength":512,"pattern":"^[^\\p{Cc}\\p{Cf}]+$"})
    }
}
impl JsonSchema for ProviderOptionString256Schema {
    fn inline_schema() -> bool {
        true
    }
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("ProviderOptionString256")
    }
    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({"type":"string","minLength":1,"maxLength":256,"pattern":"^[^\\p{Cc}\\p{Cf}]+$"})
    }
}
impl JsonSchema for ProviderApiPathSchema {
    fn inline_schema() -> bool {
        true
    }
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("ProviderApiPath")
    }
    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({"type":"string","minLength":1,"maxLength":512,"pattern":"^/[^?#]*$"})
    }
}
impl JsonSchema for ProviderBetaSchema {
    fn inline_schema() -> bool {
        true
    }
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("ProviderBeta")
    }
    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({"type":"array","maxItems":32,"uniqueItems":true,"items":{"type":"string","minLength":1,"maxLength":512,"pattern":"^[^\\p{Cc}\\p{Cf}]+$"}})
    }
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize, TS)]
#[serde(tag = "type", deny_unknown_fields)]
pub enum ProviderOptions {
    #[serde(rename = "anthropic")]
    Anthropic {
        #[serde(deserialize_with = "deserialize_provider_option_256")]
        #[schemars(
            with = "crate::NullableSchema<ProviderOptionString256Schema>",
            required
        )]
        api_version: Option<String>,
        #[serde(deserialize_with = "deserialize_provider_beta")]
        #[schemars(with = "ProviderBetaSchema")]
        beta: Vec<String>,
    },
    #[serde(rename = "openai-chat")]
    OpenAiChat {
        #[serde(deserialize_with = "deserialize_provider_option_512")]
        #[schemars(
            with = "crate::NullableSchema<ProviderOptionString512Schema>",
            required
        )]
        organization: Option<String>,
        #[serde(deserialize_with = "deserialize_provider_option_256")]
        #[schemars(
            with = "crate::NullableSchema<ProviderOptionString256Schema>",
            required
        )]
        project: Option<String>,
    },
    #[serde(rename = "openai-responses")]
    OpenAiResponses {
        #[serde(deserialize_with = "deserialize_provider_option_512")]
        #[schemars(
            with = "crate::NullableSchema<ProviderOptionString512Schema>",
            required
        )]
        organization: Option<String>,
        #[serde(deserialize_with = "deserialize_provider_option_256")]
        #[schemars(
            with = "crate::NullableSchema<ProviderOptionString256Schema>",
            required
        )]
        project: Option<String>,
        #[serde(deserialize_with = "crate::deserialize_required_option")]
        #[schemars(with = "crate::NullableSchema<bool>", required)]
        store: Option<bool>,
    },
    #[serde(rename = "openai-compatible")]
    OpenAiCompatible {
        #[serde(deserialize_with = "deserialize_provider_api_path")]
        #[schemars(with = "crate::NullableSchema<ProviderApiPathSchema>", required)]
        api_path: Option<String>,
    },
    #[serde(rename = "google-gemini")]
    GoogleGemini {
        #[serde(deserialize_with = "deserialize_provider_option_256")]
        #[schemars(
            with = "crate::NullableSchema<ProviderOptionString256Schema>",
            required
        )]
        api_version: Option<String>,
    },
    #[serde(rename = "google-vertex-gemini")]
    GoogleVertexGemini {
        #[serde(deserialize_with = "deserialize_provider_string_256")]
        #[schemars(with = "ProviderOptionString256Schema")]
        project: String,
        #[serde(deserialize_with = "deserialize_provider_string_256")]
        #[schemars(with = "ProviderOptionString256Schema")]
        location: String,
    },
    #[serde(rename = "aws-bedrock-converse")]
    AwsBedrockConverse {
        #[serde(deserialize_with = "deserialize_provider_string_256")]
        #[schemars(with = "ProviderOptionString256Schema")]
        region: String,
    },
    #[serde(rename = "azure-openai-chat")]
    AzureOpenAiChat {
        #[serde(deserialize_with = "deserialize_provider_string_256")]
        #[schemars(with = "ProviderOptionString256Schema")]
        deployment: String,
        #[serde(deserialize_with = "deserialize_provider_string_256")]
        #[schemars(with = "ProviderOptionString256Schema")]
        api_version: String,
    },
    #[serde(rename = "azure-openai-responses")]
    AzureOpenAiResponses {
        #[serde(deserialize_with = "deserialize_provider_string_256")]
        #[schemars(with = "ProviderOptionString256Schema")]
        deployment: String,
        #[serde(deserialize_with = "deserialize_provider_string_256")]
        #[schemars(with = "ProviderOptionString256Schema")]
        api_version: String,
    },
    #[serde(rename = "cohere-v2-chat")]
    CohereV2Chat {
        #[serde(deserialize_with = "deserialize_provider_option_256")]
        #[schemars(
            with = "crate::NullableSchema<ProviderOptionString256Schema>",
            required
        )]
        api_version: Option<String>,
    },
    #[serde(rename = "open-responses")]
    OpenResponses { protocol_mode: OpenResponsesMode },
}

impl ProviderOptions {
    #[must_use]
    pub const fn adapter_id(&self) -> AdaptorId {
        match self {
            Self::Anthropic { .. } => AdaptorId::Anthropic,
            Self::OpenAiChat { .. } => AdaptorId::OpenaiChat,
            Self::OpenAiResponses { .. } => AdaptorId::OpenaiResponses,
            Self::OpenAiCompatible { .. } => AdaptorId::OpenaiCompatible,
            Self::GoogleGemini { .. } => AdaptorId::GoogleGemini,
            Self::GoogleVertexGemini { .. } => AdaptorId::GoogleVertexGemini,
            Self::AwsBedrockConverse { .. } => AdaptorId::AwsBedrockConverse,
            Self::AzureOpenAiChat { .. } => AdaptorId::AzureOpenaiChat,
            Self::AzureOpenAiResponses { .. } => AdaptorId::AzureOpenaiResponses,
            Self::CohereV2Chat { .. } => AdaptorId::CohereV2Chat,
            Self::OpenResponses { .. } => AdaptorId::OpenResponses,
        }
    }
}

fn validate_provider_string<E>(value: &str, maximum: usize) -> Result<(), E>
where
    E: serde::de::Error,
{
    if value.is_empty() || value.len() > maximum || value.chars().any(char::is_control) {
        Err(E::custom("provider option string is invalid"))
    } else {
        Ok(())
    }
}

fn deserialize_provider_string_256<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = String::deserialize(deserializer)?;
    validate_provider_string::<D::Error>(&value, 256)?;
    Ok(value)
}

fn deserialize_provider_option_256<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<String>::deserialize(deserializer)?;
    if let Some(value) = &value {
        validate_provider_string::<D::Error>(value, 256)?;
    }
    Ok(value)
}

fn deserialize_provider_option_512<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<String>::deserialize(deserializer)?;
    if let Some(value) = &value {
        validate_provider_string::<D::Error>(value, 512)?;
    }
    Ok(value)
}

fn deserialize_provider_api_path<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = deserialize_provider_option_512(deserializer)?;
    if value
        .as_ref()
        .is_some_and(|path| !path.starts_with('/') || path.contains('?') || path.contains('#'))
    {
        return Err(serde::de::Error::custom("provider api_path is invalid"));
    }
    Ok(value)
}

fn deserialize_provider_beta<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let values = Vec::<String>::deserialize(deserializer)?;
    if values.len() > 32
        || values
            .iter()
            .any(|value| validate_provider_string::<D::Error>(value, 512).is_err())
        || values.iter().collect::<BTreeSet<_>>().len() != values.len()
    {
        return Err(serde::de::Error::custom(
            "provider beta options are invalid",
        ));
    }
    Ok(values)
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum OpenResponsesMode {
    Standard,
    Compact,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum VariantOrigin {
    ModelsDevEffort,
    ModelsDevToggle,
    ModelsDevBudgetTokens,
    Explicit,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct AvailableVariantDescriptor {
    #[ts(type = "VariantId")]
    pub id: VariantId,
    #[schemars(with = "ModelDisplayNameSchema")]
    pub display_name: String,
    pub origin: VariantOrigin,
    pub behavior_fingerprint: Sha256Digest,
}

#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct AvailableModelDescriptor {
    #[ts(type = "ModelKey")]
    pub key: ModelKey,
    #[schemars(with = "ModelDisplayNameSchema")]
    pub display_name: String,
    pub capabilities: ModelCapabilities,
    #[schemars(length(max = 256))]
    pub variants: Vec<AvailableVariantDescriptor>,
    #[serde(deserialize_with = "crate::deserialize_required_option")]
    #[schemars(with = "crate::NullableSchema<VariantId>", required)]
    #[ts(type = "VariantId | null")]
    pub default_variant: Option<VariantId>,
    pub behavior_fingerprint: Sha256Digest,
}

struct ModelDisplayNameSchema;
impl JsonSchema for ModelDisplayNameSchema {
    fn inline_schema() -> bool {
        true
    }
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("ModelDisplayName")
    }
    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({"type":"string","minLength":1,"maxLength":512,"pattern":"^(?=.*\\S)[^\\p{Cc}\\p{Cf}]+$"})
    }
}

impl AvailableModelDescriptor {
    pub fn validate(&self) -> Result<(), ModelSchemaError> {
        validate_display_name(&self.display_name)?;
        self.capabilities.validate()?;
        if self.variants.len() > 256 {
            return Err(ModelSchemaError::TooManyVariants);
        }
        if self
            .variants
            .windows(2)
            .any(|pair| pair[0].id >= pair[1].id)
        {
            return Err(ModelSchemaError::VariantsNotStrictlySorted);
        }
        for variant in &self.variants {
            validate_display_name(&variant.display_name)?;
        }
        if self
            .default_variant
            .as_ref()
            .is_some_and(|id| !self.variants.iter().any(|variant| &variant.id == id))
        {
            return Err(ModelSchemaError::UnknownDefaultVariant);
        }
        Ok(())
    }
}
impl<'de> Deserialize<'de> for AvailableModelDescriptor {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            key: ModelKey,
            display_name: String,
            capabilities: ModelCapabilities,
            variants: Vec<AvailableVariantDescriptor>,
            #[serde(deserialize_with = "crate::deserialize_required_option")]
            default_variant: Option<VariantId>,
            behavior_fingerprint: Sha256Digest,
        }
        let w = Wire::deserialize(d)?;
        let value = Self {
            key: w.key,
            display_name: w.display_name,
            capabilities: w.capabilities,
            variants: w.variants,
            default_variant: w.default_variant,
            behavior_fingerprint: w.behavior_fingerprint,
        };
        value.validate().map_err(serde::de::Error::custom)?;
        Ok(value)
    }
}

#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct ResolvedModelRef {
    #[serde(deserialize_with = "crate::deserialize_required_model_selection")]
    #[schemars(with = "crate::RequiredModelSelectionSchema")]
    #[ts(type = "ModelSelection")]
    pub selection: ModelSelection,
    #[ts(type = "ProviderId")]
    pub provider_id: ProviderId,
    #[ts(type = "ProviderModelId")]
    pub model_id: ProviderModelId,
    pub adapter_id: AdaptorId,
    pub selection_fingerprint: Sha256Digest,
}

impl ResolvedModelRef {
    pub fn validate(&self) -> Result<(), ModelSchemaError> {
        if self.selection.model.provider_id() != self.provider_id
            || self.selection.model.model_id() != self.model_id
        {
            return Err(ModelSchemaError::ResolvedIdentityMismatch);
        }
        Ok(())
    }
}
impl<'de> Deserialize<'de> for ResolvedModelRef {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            #[serde(deserialize_with = "crate::deserialize_required_model_selection")]
            selection: ModelSelection,
            provider_id: ProviderId,
            model_id: ProviderModelId,
            adapter_id: AdaptorId,
            selection_fingerprint: Sha256Digest,
        }
        let w = Wire::deserialize(d)?;
        let value = Self {
            selection: w.selection,
            provider_id: w.provider_id,
            model_id: w.model_id,
            adapter_id: w.adapter_id,
            selection_fingerprint: w.selection_fingerprint,
        };
        value.validate().map_err(serde::de::Error::custom)?;
        Ok(value)
    }
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct FrozenModelBinding {
    pub resolved: ResolvedModelRef,
    #[schemars(with = "LanguageModelDescriptorSchema")]
    #[ts(type = "LanguageModelDescriptor")]
    pub descriptor: oven_sdk::LanguageModelDescriptor,
    pub defaults: ResolvedRequestDefaults,
    pub provider_options: ProviderOptions,
    pub behavior_fingerprint: Sha256Digest,
}

impl FrozenModelBinding {
    pub fn expected_selection_fingerprint(
        selection: &ModelSelection,
        adapter_id: AdaptorId,
        descriptor: &oven_sdk::LanguageModelDescriptor,
        behavior_fingerprint: &Sha256Digest,
    ) -> Result<Sha256Digest, ModelSchemaError> {
        use sha2::{Digest as _, Sha256};

        let encoded =
            serde_json::to_vec(&(selection, adapter_id, descriptor, behavior_fingerprint))
                .map_err(|_| ModelSchemaError::FingerprintEncoding)?;
        let mut hasher = Sha256::new();
        hasher.update(b"cookie-agent/model-selection/v1");
        hasher.update([0]);
        hasher.update(encoded);
        Sha256Digest::new(format!("{:x}", hasher.finalize()))
            .map_err(|_| ModelSchemaError::FingerprintEncoding)
    }

    pub fn validate(&self) -> Result<(), ModelSchemaError> {
        self.resolved.validate()?;
        if self.descriptor.identity.provider_id.as_str() != self.resolved.provider_id.as_str()
            || self.descriptor.identity.model_id.as_str() != self.resolved.model_id.as_str()
        {
            return Err(ModelSchemaError::DescriptorIdentityMismatch);
        }
        self.defaults.validate()?;
        if self.provider_options.adapter_id() != self.resolved.adapter_id {
            return Err(ModelSchemaError::ProviderOptionsAdapterMismatch);
        }
        if self.resolved.selection_fingerprint
            != Self::expected_selection_fingerprint(
                &self.resolved.selection,
                self.resolved.adapter_id,
                &self.descriptor,
                &self.behavior_fingerprint,
            )?
        {
            return Err(ModelSchemaError::SelectionFingerprintMismatch);
        }
        Ok(())
    }
}
impl<'de> Deserialize<'de> for FrozenModelBinding {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            resolved: ResolvedModelRef,
            descriptor: oven_sdk::LanguageModelDescriptor,
            defaults: ResolvedRequestDefaults,
            provider_options: ProviderOptions,
            behavior_fingerprint: Sha256Digest,
        }
        let w = Wire::deserialize(d)?;
        let value = Self {
            resolved: w.resolved,
            descriptor: w.descriptor,
            defaults: w.defaults,
            provider_options: w.provider_options,
            behavior_fingerprint: w.behavior_fingerprint,
        };
        value.validate().map_err(serde::de::Error::custom)?;
        Ok(value)
    }
}

fn validate_display_name(value: &str) -> Result<(), ModelSchemaError> {
    if value.trim().is_empty() || value.len() > 512 || value.chars().any(char::is_control) {
        Err(ModelSchemaError::InvalidDisplayName)
    } else {
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModelSchemaError {
    InvalidMimeType,
    EmptyModalities,
    InvalidTokenLimits,
    ParallelToolsWithoutTools,
    InvalidOutputModality,
    InvalidMediaCapability,
    NonFiniteNumber,
    TemperatureOutOfRange,
    TopPOutOfRange,
    InvalidMaxOutputTokens,
    InvalidStopSequence,
    InvalidDisplayName,
    TooManyVariants,
    VariantsNotStrictlySorted,
    UnknownDefaultVariant,
    ResolvedIdentityMismatch,
    DescriptorIdentityMismatch,
    ProviderOptionsAdapterMismatch,
    InvalidReasoningBudget,
    InvalidToolName,
    FingerprintEncoding,
    SelectionFingerprintMismatch,
}
impl fmt::Display for ModelSchemaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidMimeType => "invalid MIME type",
            Self::EmptyModalities => "model input and output modalities must be nonempty",
            Self::InvalidTokenLimits => {
                "model token limits must be positive and output must not exceed context"
            }
            Self::ParallelToolsWithoutTools => "parallel tool calls require tool calling",
            Self::InvalidOutputModality => "PDF is not a valid output modality",
            Self::InvalidMediaCapability => {
                "non-text input modalities require exactly one matching positive media capability"
            }
            Self::NonFiniteNumber => "number must be finite",
            Self::TemperatureOutOfRange => "temperature must be in 0.0..=2.0",
            Self::TopPOutOfRange => "top_p must be in 0.0..=1.0",
            Self::InvalidMaxOutputTokens => "max_output_tokens must be positive",
            Self::InvalidStopSequence => {
                "stop must contain at most eight control-free strings of 1..=256 bytes"
            }
            Self::InvalidDisplayName => {
                "display name must be nonblank, control-free, and at most 512 bytes"
            }
            Self::TooManyVariants => "model has more than 256 variants",
            Self::VariantsNotStrictlySorted => "variants must be strictly sorted by ID",
            Self::UnknownDefaultVariant => "default variant is not present",
            Self::ResolvedIdentityMismatch => {
                "resolved provider/model fields do not match selection"
            }
            Self::DescriptorIdentityMismatch => {
                "frozen descriptor key does not match resolved selection"
            }
            Self::ProviderOptionsAdapterMismatch => {
                "provider options do not match the resolved adapter"
            }
            Self::InvalidReasoningBudget => "reasoning token budget must be -1 or nonnegative",
            Self::InvalidToolName => "model tool name must be control-free and 1..=64 bytes",
            Self::FingerprintEncoding => "model selection fingerprint encoding failed",
            Self::SelectionFingerprintMismatch => {
                "selection fingerprint does not match the complete frozen model binding"
            }
        })
    }
}
impl std::error::Error for ModelSchemaError {}
