//! Strict shared identities for agents, providers, models, and variants.

use std::{borrow::Cow, fmt, str::FromStr};

use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use thiserror::Error;
use ts_rs::TS;

/// Identity parsing failure.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum IdentityError {
    #[error("invalid agent id")]
    Agent,
    #[error("invalid provider id")]
    Provider,
    #[error("invalid provider model id")]
    ProviderModel,
    #[error("invalid model key")]
    ModelKey,
    #[error("invalid variant id")]
    Variant,
    #[error("invalid safe code")]
    SafeCode,
    #[error("invalid wildcard pattern")]
    WildcardPattern,
    #[error("invalid strict identifier")]
    StrictIdentifier,
    #[error("invalid revision")]
    Revision,
}

macro_rules! string_identity {
    ($name:ident, $error:ident, $validate:ident, $max:literal, $pattern:literal) => {
        #[derive(Clone, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name(String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Result<Self, IdentityError> {
                let value = value.into();
                if $validate(&value) {
                    Ok(Self(value))
                } else {
                    Err(IdentityError::$error)
                }
            }

            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }

            #[must_use]
            pub fn into_string(self) -> String {
                self.0
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter
                    .debug_tuple(stringify!($name))
                    .field(&self.0)
                    .finish()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(&self.0)
            }
        }

        impl FromStr for $name {
            type Err = IdentityError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Self::new(value)
            }
        }

        impl Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                serializer.serialize_str(&self.0)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                let value = String::deserialize(deserializer)?;
                Self::new(value).map_err(de::Error::custom)
            }
        }

        impl JsonSchema for $name {
            fn inline_schema() -> bool {
                true
            }

            fn schema_name() -> Cow<'static, str> {
                stringify!($name).into()
            }

            fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
                json_schema!({
                    "type": "string",
                    "minLength": 1,
                    "maxLength": $max,
                    "pattern": $pattern
                })
            }
        }
    };
}

string_identity!(
    AgentId,
    Agent,
    valid_agent_id,
    64,
    "^[a-z0-9](?:[a-z0-9]|-(?=[a-z0-9]))*$"
);

/// Stable lowercase machine-readable code shared by configuration and durable protocol values.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, TS)]
#[ts(type = "string")]
pub struct SafeCode(String);

impl SafeCode {
    pub const MAX_BYTES: usize = 128;
    pub const JSON_SCHEMA_PATTERN: &'static str = "^[a-z0-9][a-z0-9._-]*$";

    pub fn new(value: impl Into<String>) -> Result<Self, IdentityError> {
        let value = value.into();
        if (1..=Self::MAX_BYTES).contains(&value.len())
            && value.bytes().enumerate().all(|(index, byte)| {
                byte.is_ascii_lowercase()
                    || byte.is_ascii_digit()
                    || (index > 0 && matches!(byte, b'.' | b'_' | b'-'))
            })
        {
            Ok(Self(value))
        } else {
            Err(IdentityError::SafeCode)
        }
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Current wildcard grammar shared by authored configuration and frozen protocol values.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, TS)]
#[ts(type = "string")]
pub struct WildcardPattern(String);

impl WildcardPattern {
    pub const MAX_BYTES: usize = 4096;
    pub const WORKSPACE_DIR_EXPRESSION: &'static str = "${workspace_dir}";
    pub const JSON_SCHEMA_MAX_UTF8_BYTES_EXTENSION: &'static str = "x-cookie-agent-maxUtf8Bytes";
    pub const JSON_SCHEMA_PATTERN: &'static str =
        "^(?:\\$\\{workspace_dir\\}|[^\\u0000-\\u001f\\u007f-\\u009f\\\\\\[\\]\\{\\}])+$";

    pub fn new(value: impl Into<String>) -> Result<Self, IdentityError> {
        let value = value.into();
        let without_workspace_expression = value.replace(Self::WORKSPACE_DIR_EXPRESSION, "");
        if (1..=Self::MAX_BYTES).contains(&value.len())
            && !value.chars().any(char::is_control)
            && !without_workspace_expression
                .chars()
                .any(|character| matches!(character, '\\' | '[' | ']' | '{' | '}'))
        {
            Ok(Self(value))
        } else {
            Err(IdentityError::WildcardPattern)
        }
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

macro_rules! shared_permission_string_impl {
    ($name:ident, $schema:expr) => {
        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(&self.0)
            }
        }

        impl FromStr for $name {
            type Err = IdentityError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Self::new(value)
            }
        }

        impl Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                serializer.serialize_str(&self.0)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                Self::new(String::deserialize(deserializer)?).map_err(de::Error::custom)
            }
        }

        impl JsonSchema for $name {
            fn inline_schema() -> bool {
                true
            }

            fn schema_name() -> Cow<'static, str> {
                stringify!($name).into()
            }

            fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
                $schema
            }
        }
    };
}

shared_permission_string_impl!(
    SafeCode,
    json_schema!({
        "type": "string",
        "minLength": 1,
        "maxLength": SafeCode::MAX_BYTES,
        "pattern": SafeCode::JSON_SCHEMA_PATTERN,
        "description": "Stable lowercase machine-readable code."
    })
);
shared_permission_string_impl!(
    WildcardPattern,
    json_schema!({
        "type": "string",
        "minLength": 1,
        "pattern": WildcardPattern::JSON_SCHEMA_PATTERN,
        "x-cookie-agent-maxUtf8Bytes": WildcardPattern::MAX_BYTES,
        "description": "Current wildcard grammar: '*' matches any characters, '?' matches one character, and the exact '${workspace_dir}' expression is allowed; controls, globstar, escapes, classes, and every other brace form are forbidden. Runtime deserialization additionally enforces a maximum of 4096 UTF-8 bytes; x-cookie-agent-maxUtf8Bytes records that byte limit because JSON Schema maxLength counts Unicode code points."
    })
);
string_identity!(
    ProviderId,
    Provider,
    valid_provider_id,
    128,
    "^[a-z0-9][a-z0-9._-]{0,127}$"
);
string_identity!(
    ProviderModelId,
    ProviderModel,
    valid_provider_model_id,
    384,
    "^[^/\\s\\u0000-\\u001f\\u007f-\\u009f\\[\\]]+(?:/[^/\\s\\u0000-\\u001f\\u007f-\\u009f\\[\\]]+)*$"
);
string_identity!(
    VariantId,
    Variant,
    valid_variant_id,
    64,
    "^(?!base$)[a-z0-9][a-z0-9._-]{0,63}$"
);

macro_rules! strict_identifier {
    ($($name:ident),+ $(,)?) => {$(
        string_identity!(
            $name,
            StrictIdentifier,
            valid_strict_identifier,
            128,
            "^[a-z0-9][a-z0-9._-]{0,127}$"
        );
    )+};
}

strict_identifier!(
    AuthMethodId,
    AdapterId,
    ProviderRecipeId,
    ProtocolRecipeId,
    ProviderSetupRecipeId,
    AuthRecipeId,
    RecipeCompilerVersion,
    CacheEntryId,
    StoreEntryId,
    ManifestEntryId,
);

macro_rules! field_identifier {
    ($($name:ident),+ $(,)?) => {$(
        string_identity!(
            $name,
            StrictIdentifier,
            valid_field_identifier,
            128,
            "^[a-z][a-z0-9_]{0,127}$"
        );
    )+};
}

field_identifier!(SetupFieldId, AuthParameterId, AuthFieldName);

string_identity!(
    CanonicalModelId,
    StrictIdentifier,
    valid_provider_model_id,
    384,
    "^[^/\\s\\u0000-\\u001f\\u007f-\\u009f\\[\\]]+(?:/[^/\\s\\u0000-\\u001f\\u007f-\\u009f\\[\\]]+)*$"
);

macro_rules! revision_identity {
    ($($name:ident),+ $(,)?) => {$(
        #[derive(Clone, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name(String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Result<Self, IdentityError> {
                let value = value.into();
                if valid_revision(&value) { Ok(Self(value)) } else { Err(IdentityError::Revision) }
            }
            #[must_use]
            pub fn as_str(&self) -> &str { &self.0 }
            #[must_use]
            pub fn into_string(self) -> String { self.0 }
        }
        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.debug_tuple(stringify!($name)).field(&self.0).finish()
            }
        }
        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { f.write_str(&self.0) }
        }
        impl FromStr for $name {
            type Err = IdentityError;
            fn from_str(value: &str) -> Result<Self, Self::Err> { Self::new(value) }
        }
        impl Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where S: Serializer { serializer.serialize_str(&self.0) }
        }
        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where D: Deserializer<'de> {
                Self::new(String::deserialize(deserializer)?).map_err(de::Error::custom)
            }
        }
        impl JsonSchema for $name {
            fn inline_schema() -> bool { true }
            fn schema_name() -> Cow<'static, str> { stringify!($name).into() }
            fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
                json_schema!({
                    "type": "string",
                    "pattern": "^sha256:[0-9a-f]{64}$",
                    "minLength": 71,
                    "maxLength": 71
                })
            }
        }
    )+};
}

revision_identity!(
    CatalogRevision,
    ModelRevision,
    RecipeRegistryRevision,
    ProviderStoreRevision,
    ProviderStateRevision,
    ModelSnapshotRevision,
    RuntimeRevision,
    AgentRevision,
    ManifestRevision,
    CacheRevision,
);

fn valid_agent_id(value: &str) -> bool {
    (1..=64).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        && value
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
        && value
            .as_bytes()
            .last()
            .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        && !value.as_bytes().windows(2).any(|pair| pair == b"--")
}

fn valid_provider_id(value: &str) -> bool {
    (1..=128).contains(&value.len())
        && value
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'_' | b'-')
        })
}

fn valid_provider_model_id(value: &str) -> bool {
    (1..=384).contains(&value.len())
        && !value.contains(['[', ']'])
        && !value.split('/').any(|segment| segment.is_empty())
        && !value
            .chars()
            .any(|character| character.is_control() || character.is_whitespace())
}

fn valid_variant_id(value: &str) -> bool {
    value != "base"
        && (1..=64).contains(&value.len())
        && value
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'_' | b'-')
        })
}

fn valid_strict_identifier(value: &str) -> bool {
    (1..=128).contains(&value.len())
        && value
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'_' | b'-')
        })
}

fn valid_field_identifier(value: &str) -> bool {
    (1..=128).contains(&value.len())
        && value.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

fn valid_revision(value: &str) -> bool {
    value.len() == 71
        && value.starts_with("sha256:")
        && value[7..]
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Direct runnable `provider/model-id` identity.
#[derive(Clone, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ModelKey(String);

impl ModelKey {
    pub fn new(provider: ProviderId, model: ProviderModelId) -> Result<Self, IdentityError> {
        Self::from_parts(&provider, &model)
    }

    fn from_parts(provider: &ProviderId, model: &ProviderModelId) -> Result<Self, IdentityError> {
        let value = format!("{provider}/{model}");
        if value.len() <= 512 {
            Ok(Self(value))
        } else {
            Err(IdentityError::ModelKey)
        }
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    #[must_use]
    pub fn provider_id(&self) -> ProviderId {
        ProviderId::new(self.0.split_once('/').expect("validated model key").0)
            .expect("validated provider id")
    }

    #[must_use]
    pub fn model_id(&self) -> ProviderModelId {
        ProviderModelId::new(self.0.split_once('/').expect("validated model key").1)
            .expect("validated model id")
    }
}

impl FromStr for ModelKey {
    type Err = IdentityError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.len() > 512 {
            return Err(IdentityError::ModelKey);
        }
        let (provider, model) = value.split_once('/').ok_or(IdentityError::ModelKey)?;
        Self::from_parts(&ProviderId::new(provider)?, &ProviderModelId::new(model)?)
    }
}

impl fmt::Debug for ModelKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_tuple("ModelKey").field(&self.0).finish()
    }
}

impl fmt::Display for ModelKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Serialize for ModelKey {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for ModelKey {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(de::Error::custom)
    }
}

impl JsonSchema for ModelKey {
    fn inline_schema() -> bool {
        true
    }

    fn schema_name() -> Cow<'static, str> {
        "ModelKey".into()
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "string",
            "minLength": 3,
            "maxLength": 512,
            "pattern": "^[a-z0-9][a-z0-9._-]{0,127}/[^/\\s\\u0000-\\u001f\\u007f-\\u009f\\[\\]]+(?:/[^/\\s\\u0000-\\u001f\\u007f-\\u009f\\[\\]]+)*$"
        })
    }
}

/// One exact base or named-variant model selection.
#[derive(
    Clone, Debug, Deserialize, Eq, Hash, JsonSchema, Ord, PartialEq, PartialOrd, Serialize,
)]
#[serde(deny_unknown_fields)]
pub struct ModelSelection {
    pub model: ModelKey,
    pub variant: Option<VariantId>,
}

impl fmt::Display for ModelSelection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}[", self.model)?;
        match &self.variant {
            Some(variant) => variant.fmt(formatter)?,
            None => formatter.write_str("base")?,
        }
        formatter.write_str("]")
    }
}

/// Three-state agent fallback authoring after field-presence decoding.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConfiguredVariantRef {
    Base,
    Named(VariantId),
}

/// Three-state provider default authoring after field-presence decoding.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConfiguredModelDefault {
    Base,
    Named(VariantId),
}

macro_rules! variant_reference_serde {
    ($name:ident) => {
        impl Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                match self {
                    Self::Base => serializer.serialize_str("base"),
                    Self::Named(id) => serializer.serialize_str(id.as_str()),
                }
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                let value = String::deserialize(deserializer)?;
                if value == "base" {
                    Ok(Self::Base)
                } else {
                    VariantId::new(value)
                        .map(Self::Named)
                        .map_err(de::Error::custom)
                }
            }
        }
    };
}

variant_reference_serde!(ConfiguredVariantRef);
variant_reference_serde!(ConfiguredModelDefault);

macro_rules! variant_reference_schema {
    ($name:ident) => {
        impl JsonSchema for $name {
            fn inline_schema() -> bool {
                true
            }

            fn schema_name() -> Cow<'static, str> {
                stringify!($name).into()
            }

            fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
                json_schema!({
                    "type": "string",
                    "minLength": 1,
                    "maxLength": 64,
                    "pattern": "^[a-z0-9._-]+$"
                })
            }
        }
    };
}

variant_reference_schema!(ConfiguredVariantRef);
variant_reference_schema!(ConfiguredModelDefault);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identities_are_strict_and_model_keys_split_once() {
        assert!(AgentId::new("worker-one").is_ok());
        assert!(AgentId::new("Worker").is_err());
        assert!(ProviderId::new("openai.compat").is_ok());
        assert!(ProviderModelId::new("model/child").is_ok());
        assert!(VariantId::new("base").is_err());
        let key: ModelKey = "openai/gpt-5.6-sol".parse().unwrap();
        assert_eq!(key.provider_id().as_str(), "openai");
        assert_eq!(key.model_id().as_str(), "gpt-5.6-sol");
        assert_eq!(
            "openai/group/model"
                .parse::<ModelKey>()
                .unwrap()
                .model_id()
                .as_str(),
            "group/model"
        );
    }

    #[test]
    fn base_and_named_default_are_not_conflated() {
        assert_eq!(
            serde_json::from_str::<ConfiguredVariantRef>("\"base\"").unwrap(),
            ConfiguredVariantRef::Base
        );
        assert_eq!(
            serde_json::from_str::<ConfiguredVariantRef>("\"default\"").unwrap(),
            ConfiguredVariantRef::Named(VariantId::new("default").unwrap())
        );
    }

    #[test]
    fn model_selection_format_is_exact_and_serialization_is_unchanged() {
        let base = ModelSelection {
            model: "openai/gpt-5.6-sol".parse().unwrap(),
            variant: None,
        };
        let named_default = ModelSelection {
            model: "openai/gpt-5.6-sol".parse().unwrap(),
            variant: Some(VariantId::new("default").unwrap()),
        };
        let high = ModelSelection {
            model: "anthropic/claude-sonnet-4-6".parse().unwrap(),
            variant: Some(VariantId::new("high").unwrap()),
        };

        assert_eq!(base.to_string(), "openai/gpt-5.6-sol[base]");
        assert_eq!(named_default.to_string(), "openai/gpt-5.6-sol[default]");
        assert_eq!(high.to_string(), "anthropic/claude-sonnet-4-6[high]");
        assert_eq!(
            serde_json::to_value(&base).unwrap(),
            serde_json::json!({"model": "openai/gpt-5.6-sol", "variant": null})
        );
        assert_eq!(
            serde_json::to_value(&named_default).unwrap(),
            serde_json::json!({"model": "openai/gpt-5.6-sol", "variant": "default"})
        );
    }

    #[test]
    fn agent_schema_exposes_runtime_bounds() {
        let schema = serde_json::to_value(schemars::schema_for!(AgentId)).unwrap();
        assert_eq!(schema["maxLength"], 64);
        assert_eq!(schema["pattern"], "^[a-z0-9](?:[a-z0-9]|-(?=[a-z0-9]))*$");
    }

    #[test]
    fn provider_schema_exposes_runtime_bounds() {
        let schema = serde_json::to_value(schemars::schema_for!(ProviderId)).unwrap();
        assert_eq!(schema["maxLength"], 128);
        assert_eq!(schema["pattern"], "^[a-z0-9][a-z0-9._-]{0,127}$");
    }

    #[test]
    fn provider_model_schema_exposes_runtime_bounds() {
        let schema = serde_json::to_value(schemars::schema_for!(ProviderModelId)).unwrap();
        assert_eq!(schema["maxLength"], 384);
        assert!(schema["pattern"].as_str().unwrap().contains("\\s"));
    }

    #[test]
    fn variant_schema_excludes_reserved_base() {
        let schema = serde_json::to_value(schemars::schema_for!(VariantId)).unwrap();
        assert_eq!(schema["maxLength"], 64);
        assert!(schema["pattern"].as_str().unwrap().contains("?!base$"));
    }

    #[test]
    fn model_key_schema_exposes_combined_bound_and_separator() {
        let schema = serde_json::to_value(schemars::schema_for!(ModelKey)).unwrap();
        assert_eq!(schema["maxLength"], 512);
        assert!(schema["pattern"].as_str().unwrap().contains('/'));
    }

    #[test]
    fn configured_variant_schema_includes_base_spelling() {
        let schema = serde_json::to_value(schemars::schema_for!(ConfiguredVariantRef)).unwrap();
        assert_eq!(schema["type"], "string");
        assert!(schema["pattern"].as_str().unwrap().contains("a-z"));
    }

    #[test]
    fn permission_values_enforce_current_shared_grammar() {
        assert!(SafeCode::new("allow-read_1.test").is_ok());
        assert!(SafeCode::new("Allow-read").is_err());
        assert!(SafeCode::new("-allow-read").is_err());
        assert!(SafeCode::new("a".repeat(128)).is_ok());
        assert!(SafeCode::new("a".repeat(129)).is_err());

        for pattern in [
            "*",
            "file?.rs",
            "literal(value)",
            "資料/?",
            "${workspace_dir}/src/*",
            "**",
            "a**b",
            "src/**",
        ] {
            assert!(WildcardPattern::new(pattern).is_ok(), "{pattern:?}");
        }
        for pattern in [
            "",
            r"a\\*",
            "[ab]",
            "{a,b}",
            "${foo}/src/*",
            "${workspace_dir}/src/{x}",
            "a\n",
        ] {
            assert!(WildcardPattern::new(pattern).is_err(), "{pattern:?}");
        }
        assert!(WildcardPattern::new("a".repeat(4096)).is_ok());
        assert!(WildcardPattern::new("a".repeat(4097)).is_err());
        assert!(WildcardPattern::new("界".repeat(1365)).is_ok());
        assert!(WildcardPattern::new("界".repeat(1366)).is_err());
        assert!(WildcardPattern::new("😀".repeat(1024)).is_ok());
        assert!(WildcardPattern::new("😀".repeat(1025)).is_err());
    }

    #[test]
    fn permission_value_schemas_expose_runtime_constraints() {
        let code = serde_json::to_value(schemars::schema_for!(SafeCode)).unwrap();
        assert_eq!(code["maxLength"], SafeCode::MAX_BYTES);
        assert_eq!(code["pattern"], SafeCode::JSON_SCHEMA_PATTERN);

        let wildcard = serde_json::to_value(schemars::schema_for!(WildcardPattern)).unwrap();
        assert!(wildcard.get("maxLength").is_none());
        assert_eq!(
            wildcard[WildcardPattern::JSON_SCHEMA_MAX_UTF8_BYTES_EXTENSION],
            WildcardPattern::MAX_BYTES
        );
        assert_eq!(wildcard["pattern"], WildcardPattern::JSON_SCHEMA_PATTERN);
        let description = wildcard["description"].as_str().unwrap();
        assert!(description.contains("globstar"));
        assert!(description.contains("escapes"));
        assert!(description.contains("UTF-8 bytes"));
        assert!(description.contains("maxLength counts Unicode code points"));
    }

    #[test]
    fn agent_id_accepts_exact_maximum_length() {
        assert!(AgentId::new("a".repeat(64)).is_ok());
        assert!(AgentId::new("a".repeat(65)).is_err());
    }

    #[test]
    fn provider_id_accepts_exact_maximum_length() {
        assert!(ProviderId::new("p".repeat(128)).is_ok());
        assert!(ProviderId::new("p".repeat(129)).is_err());
    }

    #[test]
    fn provider_model_id_rejects_trimmed_and_control_forms() {
        assert!(ProviderModelId::new(" model").is_err());
        assert!(ProviderModelId::new("model\n").is_err());
    }

    #[test]
    fn variant_id_accepts_named_default_but_not_base() {
        assert!(VariantId::new("default").is_ok());
        assert!(VariantId::new("base").is_err());
    }
}
