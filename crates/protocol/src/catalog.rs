use std::{borrow::Cow, fmt};

use jiff::Timestamp;
use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::{
    CatalogIdentifier, CatalogRevision, CatalogText, CredentialFieldName, ReasoningEffort,
};

pub const PINNED_CATALOG_SOURCE: &str =
    "https://github.com/anomalyco/models.dev@c3057690bbb8bd41cafdefadcd2a7b958e2a4642";
pub const PINNED_CATALOG_FETCHED_AT: &str = "2026-08-01T17:34:27Z";

#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct CatalogSnapshot {
    pub revision: CatalogRevision,
    #[schemars(with = "CatalogSourceSchema")]
    pub source: CatalogText,
    #[schemars(with = "CatalogFetchedAtSchema")]
    pub fetched_at: Timestamp,
}
struct CatalogSourceSchema;
struct CatalogFetchedAtSchema;
impl JsonSchema for CatalogSourceSchema {
    fn inline_schema() -> bool {
        true
    }
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("CatalogSource")
    }
    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        json_schema!({"type":"string","const":PINNED_CATALOG_SOURCE})
    }
}
impl JsonSchema for CatalogFetchedAtSchema {
    fn inline_schema() -> bool {
        true
    }
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("CatalogFetchedAt")
    }
    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        json_schema!({"type":"string","format":"date-time","const":PINNED_CATALOG_FETCHED_AT})
    }
}
impl CatalogSnapshot {
    pub fn validate(&self) -> Result<(), CatalogSchemaError> {
        if self.source.as_str() != PINNED_CATALOG_SOURCE
            || self.fetched_at.to_string() != PINNED_CATALOG_FETCHED_AT
        {
            return Err(CatalogSchemaError::SnapshotIdentity);
        }
        Ok(())
    }
}
impl<'de> Deserialize<'de> for CatalogSnapshot {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            revision: CatalogRevision,
            source: CatalogText,
            fetched_at: Timestamp,
        }
        let wire = Wire::deserialize(deserializer)?;
        let value = Self {
            revision: wire.revision,
            source: wire.source,
            fetched_at: wire.fetched_at,
        };
        value.validate().map_err(serde::de::Error::custom)?;
        Ok(value)
    }
}

#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct CatalogProvider {
    pub id: CatalogIdentifier,
    pub name: CatalogText,
    #[schemars(length(min = 1, max = 32))]
    pub credential_fields: Vec<CredentialFieldName>,
    pub npm: CatalogText,
    #[serde(deserialize_with = "crate::deserialize_required_option")]
    #[schemars(with = "crate::NullableSchema<CatalogText>", required)]
    pub api: Option<CatalogText>,
    pub documentation_url: CatalogText,
}

impl CatalogProvider {
    pub fn validate(&self) -> Result<(), CatalogSchemaError> {
        if self.credential_fields.is_empty()
            || self.credential_fields.len() > 32
            || self
                .credential_fields
                .windows(2)
                .any(|pair| pair[0] >= pair[1])
        {
            return Err(CatalogSchemaError::CredentialFields);
        }
        Ok(())
    }
}

impl<'de> Deserialize<'de> for CatalogProvider {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            id: CatalogIdentifier,
            name: CatalogText,
            credential_fields: Vec<CredentialFieldName>,
            npm: CatalogText,
            #[serde(deserialize_with = "crate::deserialize_required_option")]
            api: Option<CatalogText>,
            documentation_url: CatalogText,
        }
        let wire = Wire::deserialize(deserializer)?;
        let value = Self {
            id: wire.id,
            name: wire.name,
            credential_fields: wire.credential_fields,
            npm: wire.npm,
            api: wire.api,
            documentation_url: wire.documentation_url,
        };
        value.validate().map_err(serde::de::Error::custom)?;
        Ok(value)
    }
}

#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum CatalogReasoningOption {
    Effort {
        #[schemars(length(min = 1, max = 9))]
        values: Vec<Option<ReasoningEffort>>,
    },
    Toggle,
    BudgetTokens {
        #[serde(deserialize_with = "crate::deserialize_required_option")]
        #[schemars(with = "crate::NullableSchema<i64>", required)]
        min: Option<i64>,
        #[serde(deserialize_with = "crate::deserialize_required_option")]
        #[schemars(with = "crate::NullableSchema<i64>", required)]
        max: Option<i64>,
    },
}
impl<'de> Deserialize<'de> for CatalogReasoningOption {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
        enum Wire {
            Effort {
                values: Vec<Option<ReasoningEffort>>,
            },
            Toggle,
            BudgetTokens {
                #[serde(deserialize_with = "crate::deserialize_required_option")]
                min: Option<i64>,
                #[serde(deserialize_with = "crate::deserialize_required_option")]
                max: Option<i64>,
            },
        }
        match Wire::deserialize(deserializer)? {
            Wire::Effort { values } => {
                if values.is_empty()
                    || values.len() > 9
                    || values.iter().enumerate().any(|(index, value)| {
                        values[..index].iter().any(|previous| previous == value)
                    })
                {
                    return Err(serde::de::Error::custom(
                        "catalog effort values must be unique and contain 1..=9 entries",
                    ));
                }
                Ok(Self::Effort { values })
            }
            Wire::Toggle => Ok(Self::Toggle),
            Wire::BudgetTokens { min, max } => {
                if min.is_some_and(|value| value < -1)
                    || max.is_some_and(|value| value < 0)
                    || matches!((min, max), (Some(min), Some(max)) if min >= 0 && min > max)
                {
                    return Err(serde::de::Error::custom(
                        "catalog reasoning budget bounds are invalid",
                    ));
                }
                Ok(Self::BudgetTokens { min, max })
            }
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct CatalogModelCapabilities {
    pub attachment: bool,
    pub reasoning: bool,
    pub tool_call: bool,
    pub structured_output: bool,
    pub temperature: bool,
}

#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct CatalogModelLimits {
    #[schemars(range(min = 1))]
    pub context: u64,
    #[serde(deserialize_with = "crate::deserialize_required_option")]
    #[schemars(with = "PositiveOptionalU64Schema", required)]
    pub input: Option<u64>,
    #[schemars(range(min = 1))]
    pub output: u64,
}

impl CatalogModelLimits {
    pub fn validate(&self) -> Result<(), CatalogSchemaError> {
        if self.context == 0
            || self.output == 0
            || self.output > self.context
            || self
                .input
                .is_some_and(|input| input == 0 || input > self.context)
        {
            return Err(CatalogSchemaError::TokenLimits);
        }
        Ok(())
    }
}

impl<'de> Deserialize<'de> for CatalogModelLimits {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            context: u64,
            #[serde(deserialize_with = "crate::deserialize_required_option")]
            input: Option<u64>,
            output: u64,
        }
        let wire = Wire::deserialize(deserializer)?;
        let value = Self {
            context: wire.context,
            input: wire.input,
            output: wire.output,
        };
        value.validate().map_err(serde::de::Error::custom)?;
        Ok(value)
    }
}

struct PositiveOptionalU64Schema;
impl JsonSchema for PositiveOptionalU64Schema {
    fn inline_schema() -> bool {
        true
    }
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("PositiveOptionalU64")
    }
    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        json_schema!({"anyOf":[{"type":"integer","minimum":1},{"type":"null"}]})
    }
}

#[derive(Clone, Debug, Eq, Hash, JsonSchema, Ord, PartialEq, PartialOrd, Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum CatalogModality {
    Text,
    Audio,
    Image,
    Video,
    Pdf,
}

impl<'de> Deserialize<'de> for CatalogModality {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        match String::deserialize(deserializer)?.as_str() {
            "text" => Ok(Self::Text),
            "audio" => Ok(Self::Audio),
            "image" => Ok(Self::Image),
            "video" => Ok(Self::Video),
            "pdf" => Ok(Self::Pdf),
            _ => Err(serde::de::Error::custom("unknown catalog modality")),
        }
    }
}

#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct CatalogModelModalities {
    #[schemars(length(min = 1, max = 8))]
    pub input: Vec<CatalogModality>,
    #[schemars(length(min = 1, max = 8))]
    pub output: Vec<CatalogModality>,
}
impl<'de> Deserialize<'de> for CatalogModelModalities {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            input: Vec<CatalogModality>,
            output: Vec<CatalogModality>,
        }
        let wire = Wire::deserialize(deserializer)?;
        if wire.input.is_empty()
            || wire.output.is_empty()
            || wire.input.len() > 8
            || wire.output.len() > 8
        {
            return Err(serde::de::Error::custom(
                "catalog modalities must be nonempty and bounded",
            ));
        }
        Ok(Self {
            input: wire.input,
            output: wire.output,
        })
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum CatalogModelStatus {
    Stable,
    Alpha,
    Beta,
    Deprecated,
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, TS)]
#[ts(type = "string")]
pub struct CatalogDate(String);

impl CatalogDate {
    pub fn new(value: impl Into<String>) -> Result<Self, CatalogSchemaError> {
        let value = value.into();
        let parts = value.split('-').collect::<Vec<_>>();
        if !matches!(parts.len(), 2 | 3)
            || parts[0].len() != 4
            || parts[1].len() != 2
            || parts.get(2).is_some_and(|day| day.len() != 2)
        {
            return Err(CatalogSchemaError::Date);
        }
        let year = parts[0]
            .parse::<u32>()
            .map_err(|_| CatalogSchemaError::Date)?;
        let month = parts[1]
            .parse::<u32>()
            .map_err(|_| CatalogSchemaError::Date)?;
        if !(1..=12).contains(&month) {
            return Err(CatalogSchemaError::Date);
        }
        if let Some(day) = parts.get(2) {
            let day = day.parse::<u32>().map_err(|_| CatalogSchemaError::Date)?;
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
                return Err(CatalogSchemaError::Date);
            }
        }
        Ok(Self(value))
    }
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl Serialize for CatalogDate {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}
impl<'de> Deserialize<'de> for CatalogDate {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}
impl JsonSchema for CatalogDate {
    fn inline_schema() -> bool {
        true
    }
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("CatalogDate")
    }
    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        json_schema!({"type":"string","minLength":7,"maxLength":10,"pattern":"^[0-9]{4}-[0-9]{2}(?:-[0-9]{2})?$"})
    }
}

#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct CatalogModel {
    pub provider_id: CatalogIdentifier,
    pub model_id: CatalogIdentifier,
    #[serde(deserialize_with = "crate::deserialize_required_option")]
    #[schemars(with = "crate::NullableSchema<CatalogText>", required)]
    pub canonical_model_id: Option<CatalogText>,
    pub name: CatalogText,
    #[serde(deserialize_with = "crate::deserialize_required_option")]
    #[schemars(with = "crate::NullableSchema<CatalogText>", required)]
    pub family: Option<CatalogText>,
    pub capabilities: CatalogModelCapabilities,
    #[schemars(length(max = 3))]
    pub reasoning_options: Vec<CatalogReasoningOption>,
    pub limits: CatalogModelLimits,
    pub modalities: CatalogModelModalities,
    pub status: CatalogModelStatus,
    pub release_date: CatalogDate,
    pub last_updated: CatalogDate,
}
impl<'de> Deserialize<'de> for CatalogModel {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            provider_id: CatalogIdentifier,
            model_id: CatalogIdentifier,
            #[serde(deserialize_with = "crate::deserialize_required_option")]
            canonical_model_id: Option<CatalogText>,
            name: CatalogText,
            #[serde(deserialize_with = "crate::deserialize_required_option")]
            family: Option<CatalogText>,
            capabilities: CatalogModelCapabilities,
            reasoning_options: Vec<CatalogReasoningOption>,
            limits: CatalogModelLimits,
            modalities: CatalogModelModalities,
            status: CatalogModelStatus,
            release_date: CatalogDate,
            last_updated: CatalogDate,
        }
        let wire = Wire::deserialize(deserializer)?;
        if wire.reasoning_options.len() > 3 {
            return Err(serde::de::Error::custom(
                "catalog has at most three reasoning option forms",
            ));
        }
        Ok(Self {
            provider_id: wire.provider_id,
            model_id: wire.model_id,
            canonical_model_id: wire.canonical_model_id,
            name: wire.name,
            family: wire.family,
            capabilities: wire.capabilities,
            reasoning_options: wire.reasoning_options,
            limits: wire.limits,
            modalities: wire.modalities,
            status: wire.status,
            release_date: wire.release_date,
            last_updated: wire.last_updated,
        })
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct CatalogProviderListParams {}

#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct CatalogProviderListResult {
    pub snapshot: CatalogSnapshot,
    #[schemars(length(max = 1000))]
    pub providers: Vec<CatalogProvider>,
}

impl<'de> Deserialize<'de> for CatalogProviderListResult {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            snapshot: CatalogSnapshot,
            providers: Vec<CatalogProvider>,
        }
        let wire = Wire::deserialize(deserializer)?;
        if wire.providers.len() > 1_000
            || wire
                .providers
                .windows(2)
                .any(|pair| pair[0].id >= pair[1].id)
        {
            return Err(serde::de::Error::custom(
                "catalog providers must be strictly sorted and unique",
            ));
        }
        Ok(Self {
            snapshot: wire.snapshot,
            providers: wire.providers,
        })
    }
}

#[derive(Clone, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct CatalogModelListParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional = nullable)]
    pub provider_id: Option<CatalogIdentifier>,
}

#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct CatalogModelListResult {
    pub snapshot: CatalogSnapshot,
    #[schemars(length(max = 100000))]
    pub models: Vec<CatalogModel>,
}

impl<'de> Deserialize<'de> for CatalogModelListResult {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            snapshot: CatalogSnapshot,
            models: Vec<CatalogModel>,
        }
        let wire = Wire::deserialize(deserializer)?;
        if wire.models.len() > 100_000
            || wire.models.windows(2).any(|pair| {
                (&pair[0].provider_id, &pair[0].model_id)
                    >= (&pair[1].provider_id, &pair[1].model_id)
            })
        {
            return Err(serde::de::Error::custom(
                "catalog models must be strictly sorted and unique",
            ));
        }
        Ok(Self {
            snapshot: wire.snapshot,
            models: wire.models,
        })
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum CatalogErrorCode {
    CatalogUnavailable,
    CatalogSnapshotInvalid,
    CatalogRevisionNotFound,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct CatalogError {
    pub code: CatalogErrorCode,
    #[serde(deserialize_with = "crate::deserialize_required_option")]
    #[schemars(with = "crate::NullableSchema<CatalogRevision>", required)]
    pub revision: Option<CatalogRevision>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CatalogSchemaError {
    CredentialFields,
    TokenLimits,
    Date,
    SnapshotIdentity,
}

impl fmt::Display for CatalogSchemaError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::CredentialFields => {
                "credential fields must be a sorted unique list of 1..=32 strict names"
            }
            Self::TokenLimits => "catalog token limits are invalid",
            Self::Date => "catalog date must be a valid YYYY-MM or YYYY-MM-DD value",
            Self::SnapshotIdentity => "catalog source identity does not match the pinned snapshot",
        })
    }
}

impl std::error::Error for CatalogSchemaError {}
