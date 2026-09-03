use std::{borrow::Cow, collections::BTreeMap, fmt};

use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::*;

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, TS)]
#[ts(type = "string")]
pub struct SafeEndpointIdentity(String);
impl SafeEndpointIdentity {
    pub const MAX_BYTES: usize = 2048;
    pub fn new(value: impl Into<String>) -> Result<Self, ManifestSchemaError> {
        let value = value.into();
        if value.is_empty()
            || value.len() > Self::MAX_BYTES
            || value.chars().any(char::is_control)
            || value.contains('@')
            || value.contains('?')
            || value.contains('#')
        {
            Err(ManifestSchemaError::InvalidEndpoint)
        } else {
            Ok(Self(value))
        }
    }
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl Serialize for SafeEndpointIdentity {
    fn serialize<S>(&self, s: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        s.serialize_str(&self.0)
    }
}
impl<'de> Deserialize<'de> for SafeEndpointIdentity {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Self::new(String::deserialize(d)?).map_err(serde::de::Error::custom)
    }
}
impl JsonSchema for SafeEndpointIdentity {
    fn inline_schema() -> bool {
        true
    }
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("SafeEndpointIdentity")
    }
    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({"type":"string","minLength":1,"maxLength":2048})
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, TS)]
#[ts(type = "string")]
pub struct HeaderName(String);
impl HeaderName {
    pub fn new(value: impl Into<String>) -> Result<Self, ManifestSchemaError> {
        let value = value.into().to_ascii_lowercase();
        if value.is_empty()
            || value.len() > 128
            || !value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
        {
            Err(ManifestSchemaError::InvalidHeader)
        } else {
            Ok(Self(value))
        }
    }
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl Serialize for HeaderName {
    fn serialize<S>(&self, s: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        s.serialize_str(&self.0)
    }
}
impl<'de> Deserialize<'de> for HeaderName {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Self::new(String::deserialize(d)?).map_err(serde::de::Error::custom)
    }
}
impl JsonSchema for HeaderName {
    fn inline_schema() -> bool {
        true
    }
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("HeaderName")
    }
    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({"type":"string","minLength":1,"maxLength":128,"pattern":"^[!#$%&'*+.^_`|~0-9A-Za-z-]+$"})
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, TS)]
#[ts(type = "string")]
pub struct SafeStaticHeaderValue(String);
impl SafeStaticHeaderValue {
    pub fn new(value: impl Into<String>) -> Result<Self, ManifestSchemaError> {
        let value = value.into();
        if value.len() > 8192 || value.chars().any(char::is_control) {
            Err(ManifestSchemaError::InvalidHeader)
        } else {
            Ok(Self(value))
        }
    }
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl Serialize for SafeStaticHeaderValue {
    fn serialize<S>(&self, s: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        s.serialize_str(&self.0)
    }
}
impl<'de> Deserialize<'de> for SafeStaticHeaderValue {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Self::new(String::deserialize(d)?).map_err(serde::de::Error::custom)
    }
}
impl JsonSchema for SafeStaticHeaderValue {
    fn inline_schema() -> bool {
        true
    }
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("SafeStaticHeaderValue")
    }
    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({"type":"string","maxLength":8192})
    }
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum FrozenProviderSource {
    Managed {
        #[ts(type = "ProviderRecipeId")]
        provider_recipe: ProviderRecipeId,
        source_record_digest: Sha256Digest,
        recipe_fingerprint: Sha256Digest,
        package_claim: String,
    },
    Custom {
        safe_definition_fingerprint: Sha256Digest,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum FrozenCredentialSource {
    AuthoredApiKey,
    AuthoredOverride,
    ProviderStore,
    NoAuth,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct FrozenSetupBinding {
    #[ts(type = "ProviderSetupRecipeId")]
    pub setup_recipe: ProviderSetupRecipeId,
    #[ts(type = "Record<string, import(\"./SafeSetupValue.js\").SafeSetupValue>")]
    pub values: BTreeMap<SetupFieldId, SafeSetupValue>,
    pub setup_fingerprint: Sha256Digest,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct FrozenCredentialBinding {
    pub source: FrozenCredentialSource,
    #[ts(type = "AuthMethodId")]
    pub auth_method: AuthMethodId,
    #[schemars(length(max = 32))]
    #[ts(type = "Array<AuthFieldName>")]
    pub fields: Vec<AuthFieldName>,
    #[ts(type = "Record<string, string>")]
    pub parameters: BTreeMap<AuthParameterId, FrozenAuthParameterValue>,
    pub owned_headers: Vec<HeaderName>,
}
impl<'de> Deserialize<'de> for FrozenCredentialBinding {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            source: FrozenCredentialSource,
            auth_method: AuthMethodId,
            fields: Vec<AuthFieldName>,
            parameters: BTreeMap<AuthParameterId, FrozenAuthParameterValue>,
            owned_headers: Vec<HeaderName>,
        }
        let w = Wire::deserialize(d)?;
        if w.fields.len() > 32
            || w.fields.windows(2).any(|p| p[0] >= p[1])
            || w.parameters.len() > 32
            || w.owned_headers.len() > 32
            || w.owned_headers.windows(2).any(|p| p[0] >= p[1])
        {
            return Err(serde::de::Error::custom(
                "credential shape must be bounded, sorted, and unique",
            ));
        }
        Ok(Self {
            source: w.source,
            auth_method: w.auth_method,
            fields: w.fields,
            parameters: w.parameters,
            owned_headers: w.owned_headers,
        })
    }
}

pub type FrozenProviderOptions = ProviderOptions;

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, TS)]
#[ts(type = "string")]
pub struct FrozenAuthParameterValue(String);

impl FrozenAuthParameterValue {
    pub fn new(value: impl Into<String>) -> Result<Self, ManifestSchemaError> {
        let value = value.into();
        if value.is_empty()
            || value.len() > 512
            || value.chars().any(char::is_control)
            || value.contains("${env:")
        {
            Err(ManifestSchemaError::InvalidAuthParameter)
        } else {
            Ok(Self(value))
        }
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Serialize for FrozenAuthParameterValue {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for FrozenAuthParameterValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

impl JsonSchema for FrozenAuthParameterValue {
    fn inline_schema() -> bool {
        true
    }
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("FrozenAuthParameterValue")
    }
    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({"type":"string","minLength":1,"maxLength":512})
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, TS)]
#[ts(type = "string")]
pub struct NormalizedDecimal(String);

impl NormalizedDecimal {
    pub fn from_f32(value: f32) -> Result<Self, ManifestSchemaError> {
        if !value.is_finite() {
            return Err(ManifestSchemaError::InvalidDecimal);
        }
        Ok(Self(value.to_string()))
    }

    pub fn parse(value: impl Into<String>) -> Result<Self, ManifestSchemaError> {
        let value = value.into();
        let parsed = value
            .parse::<f32>()
            .map_err(|_| ManifestSchemaError::InvalidDecimal)?;
        if !parsed.is_finite() || parsed.to_string() != value {
            return Err(ManifestSchemaError::InvalidDecimal);
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    #[must_use]
    pub fn get(&self) -> f32 {
        self.0
            .parse()
            .expect("normalized decimal was validated at construction")
    }
}

impl Serialize for NormalizedDecimal {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for NormalizedDecimal {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Self::parse(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

impl JsonSchema for NormalizedDecimal {
    fn inline_schema() -> bool {
        true
    }
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("NormalizedDecimal")
    }
    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({"type":"string","minLength":1,"maxLength":32})
    }
}

#[derive(Clone, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct FrozenRequestDefaults {
    pub temperature: Option<NormalizedDecimal>,
    pub top_p: Option<NormalizedDecimal>,
    pub max_output_tokens: Option<u64>,
    pub stop: Vec<String>,
    pub seed: Option<i64>,
    pub tool_choice: Option<ToolChoice>,
}

impl FrozenRequestDefaults {
    pub fn validate(&self) -> Result<(), ManifestSchemaError> {
        if self
            .temperature
            .as_ref()
            .is_some_and(|value| !(0.0..=2.0).contains(&value.get()))
            || self
                .top_p
                .as_ref()
                .is_some_and(|value| !(0.0..=1.0).contains(&value.get()))
            || self.max_output_tokens == Some(0)
            || self.stop.len() > 8
            || self
                .stop
                .iter()
                .any(|value| value.is_empty() || value.len() > 256)
        {
            Err(ManifestSchemaError::InvalidDefaults)
        } else {
            Ok(())
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct FrozenResolvedRequestDefaults {
    pub request: FrozenRequestDefaults,
    pub reasoning: Option<CompiledReasoningBehavior>,
}

impl FrozenResolvedRequestDefaults {
    pub fn validate(&self) -> Result<(), ManifestSchemaError> {
        self.request.validate()?;
        if matches!(
            self.reasoning,
            Some(CompiledReasoningBehavior::BudgetTokens { value }) if value < -1
        ) {
            Err(ManifestSchemaError::InvalidDefaults)
        } else {
            Ok(())
        }
    }
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct FrozenVariantBlueprint {
    #[ts(type = "VariantId")]
    pub id: VariantId,
    #[schemars(with = "crate::LanguageModelDescriptorSchema")]
    #[ts(type = "LanguageModelDescriptor")]
    pub descriptor: oven_sdk::LanguageModelDescriptor,
    pub defaults: FrozenResolvedRequestDefaults,
    pub options: FrozenProviderOptions,
    #[serde(default)]
    pub static_headers: BTreeMap<HeaderName, SafeStaticHeaderValue>,
    pub behavior_fingerprint: Sha256Digest,
    pub selection_fingerprint: Sha256Digest,
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct CompiledSafeModelBlueprint {
    pub blueprint_fingerprint: Sha256Digest,
    #[ts(type = "ModelSelection")]
    pub selection: ModelSelection,
    pub source: FrozenProviderSource,
    pub config_override_fingerprint: Sha256Digest,
    pub setup_binding: FrozenSetupBinding,
    pub credential_binding: FrozenCredentialBinding,
    pub endpoint_identity: SafeEndpointIdentity,
    #[ts(type = "ProviderRecipeId")]
    pub provider_recipe: ProviderRecipeId,
    #[ts(type = "ProtocolRecipeId")]
    pub protocol_recipe: ProtocolRecipeId,
    #[ts(type = "ProviderSetupRecipeId")]
    pub setup_recipe: ProviderSetupRecipeId,
    #[ts(type = "AuthMethodId")]
    pub auth_method: AuthMethodId,
    #[ts(type = "RecipeCompilerVersion")]
    pub compiler_version: RecipeCompilerVersion,
    #[schemars(with = "crate::LanguageModelDescriptorSchema")]
    #[ts(type = "LanguageModelDescriptor")]
    pub descriptor: oven_sdk::LanguageModelDescriptor,
    pub defaults: FrozenResolvedRequestDefaults,
    pub options: FrozenProviderOptions,
    pub static_headers: BTreeMap<HeaderName, SafeStaticHeaderValue>,
    #[schemars(length(max = 256))]
    pub variants: Vec<FrozenVariantBlueprint>,
    pub behavior_fingerprint: Sha256Digest,
    pub selection_fingerprint: Sha256Digest,
}
impl<'de> Deserialize<'de> for CompiledSafeModelBlueprint {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            blueprint_fingerprint: Sha256Digest,
            selection: ModelSelection,
            source: FrozenProviderSource,
            config_override_fingerprint: Sha256Digest,
            setup_binding: FrozenSetupBinding,
            credential_binding: FrozenCredentialBinding,
            endpoint_identity: SafeEndpointIdentity,
            provider_recipe: ProviderRecipeId,
            protocol_recipe: ProtocolRecipeId,
            setup_recipe: ProviderSetupRecipeId,
            auth_method: AuthMethodId,
            compiler_version: RecipeCompilerVersion,
            descriptor: oven_sdk::LanguageModelDescriptor,
            defaults: FrozenResolvedRequestDefaults,
            options: FrozenProviderOptions,
            static_headers: BTreeMap<HeaderName, SafeStaticHeaderValue>,
            variants: Vec<FrozenVariantBlueprint>,
            behavior_fingerprint: Sha256Digest,
            selection_fingerprint: Sha256Digest,
        }
        let w = Wire::deserialize(d)?;
        if w.selection.variant.is_some()
            || w.defaults.validate().is_err()
            || w.variants.len() > 256
            || w.variants.windows(2).any(|p| p[0].id >= p[1].id)
            || w.variants.iter().any(|variant| {
                variant.defaults.validate().is_err()
                    || variant.descriptor.identity != w.descriptor.identity
            })
        {
            return Err(serde::de::Error::custom(
                "model blueprint behavior is invalid",
            ));
        }
        Ok(Self {
            blueprint_fingerprint: w.blueprint_fingerprint,
            selection: w.selection,
            source: w.source,
            config_override_fingerprint: w.config_override_fingerprint,
            setup_binding: w.setup_binding,
            credential_binding: w.credential_binding,
            endpoint_identity: w.endpoint_identity,
            provider_recipe: w.provider_recipe,
            protocol_recipe: w.protocol_recipe,
            setup_recipe: w.setup_recipe,
            auth_method: w.auth_method,
            compiler_version: w.compiler_version,
            descriptor: w.descriptor,
            defaults: w.defaults,
            options: w.options,
            static_headers: w.static_headers,
            variants: w.variants,
            behavior_fingerprint: w.behavior_fingerprint,
            selection_fingerprint: w.selection_fingerprint,
        })
    }
}

#[derive(Clone, Debug, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct ModelSnapshotPayloadV1 {
    #[ts(type = "CatalogRevision")]
    pub catalog_revision: CatalogRevision,
    #[ts(type = "RecipeRegistryRevision")]
    pub recipe_registry_revision: RecipeRegistryRevision,
    #[ts(type = "ProviderStateRevision")]
    pub provider_state_revision: ProviderStateRevision,
    #[ts(type = "ModelRevision")]
    pub model_revision: ModelRevision,
    #[schemars(length(max = 4096))]
    pub blueprints: Vec<CompiledSafeModelBlueprint>,
}
impl<'de> Deserialize<'de> for ModelSnapshotPayloadV1 {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            catalog_revision: CatalogRevision,
            recipe_registry_revision: RecipeRegistryRevision,
            provider_state_revision: ProviderStateRevision,
            model_revision: ModelRevision,
            blueprints: Vec<CompiledSafeModelBlueprint>,
        }
        let w = Wire::deserialize(d)?;
        if w.blueprints.len() > 4096
            || w.blueprints
                .windows(2)
                .any(|p| p[0].selection.model >= p[1].selection.model)
        {
            return Err(serde::de::Error::custom(
                "blueprints must be strictly sorted by model key",
            ));
        }
        Ok(Self {
            catalog_revision: w.catalog_revision,
            recipe_registry_revision: w.recipe_registry_revision,
            provider_state_revision: w.provider_state_revision,
            model_revision: w.model_revision,
            blueprints: w.blueprints,
        })
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq, TS)]
#[ts(type = "1")]
pub struct ModelSnapshotManifestSchemaVersion(());
impl ModelSnapshotManifestSchemaVersion {
    #[must_use]
    pub const fn current() -> Self {
        Self(())
    }
}
impl Serialize for ModelSnapshotManifestSchemaVersion {
    fn serialize<S>(&self, s: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        s.serialize_u32(1)
    }
}
impl<'de> Deserialize<'de> for ModelSnapshotManifestSchemaVersion {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = u32::deserialize(d)?;
        if value == 1 {
            Ok(Self::current())
        } else {
            Err(serde::de::Error::custom(
                "unsupported model-snapshot manifest schema; expected 1",
            ))
        }
    }
}
impl JsonSchema for ModelSnapshotManifestSchemaVersion {
    fn inline_schema() -> bool {
        true
    }
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("ModelSnapshotManifestSchemaVersion")
    }
    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({"type":"integer","const":1})
    }
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct ModelSnapshotManifestV1 {
    pub schema_version: ModelSnapshotManifestSchemaVersion,
    #[ts(type = "ModelSnapshotRevision")]
    pub revision: ModelSnapshotRevision,
    pub payload: ModelSnapshotPayloadV1,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ManifestSchemaError {
    InvalidEndpoint,
    InvalidHeader,
    InvalidAuthParameter,
    InvalidDecimal,
    InvalidDefaults,
}
impl fmt::Display for ManifestSchemaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidEndpoint => "invalid safe endpoint identity",
            Self::InvalidHeader => "invalid safe static header",
            Self::InvalidAuthParameter => "invalid frozen auth parameter",
            Self::InvalidDecimal => "invalid normalized decimal",
            Self::InvalidDefaults => "invalid frozen request defaults",
        })
    }
}
impl std::error::Error for ManifestSchemaError {}
