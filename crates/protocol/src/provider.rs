use std::{borrow::Cow, collections::BTreeMap, fmt};

use jiff::Timestamp;
use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Serialize};
use zeroize::Zeroize;

use crate::*;

const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

macro_rules! positive_counter {
    ($name:ident, $description:literal) => {
        #[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
        #[serde(transparent)]
        pub struct $name(u64);

        impl $name {
            pub fn new(value: u64) -> Result<Self, ProviderSchemaError> {
                if (1..=MAX_SAFE_INTEGER).contains(&value) {
                    Ok(Self(value))
                } else {
                    Err(ProviderSchemaError::InvalidCounter)
                }
            }

            #[must_use]
            pub const fn get(self) -> u64 { self.0 }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where D: serde::Deserializer<'de> {
                Self::new(u64::deserialize(deserializer)?).map_err(serde::de::Error::custom)
            }
        }

        impl JsonSchema for $name {
            fn inline_schema() -> bool { true }
            fn schema_name() -> Cow<'static, str> { Cow::Borrowed(stringify!($name)) }
            fn json_schema(_: &mut SchemaGenerator) -> Schema {
                json_schema!({"type":"integer","minimum":1,"maximum":MAX_SAFE_INTEGER,"description":$description})
            }
        }
    };
}

positive_counter!(
    ProviderStoreGeneration,
    "Monotonic provider-store generation."
);
positive_counter!(
    ProviderConnectionGeneration,
    "Monotonic provider connection generation."
);

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct BoundedSetupString(String);

impl BoundedSetupString {
    pub const MAX_BYTES: usize = 2048;
    pub fn new(value: impl Into<String>) -> Result<Self, ProviderSchemaError> {
        let value = value.into();
        if value.is_empty() || value.len() > Self::MAX_BYTES || value.chars().any(char::is_control)
        {
            Err(ProviderSchemaError::InvalidSetupString)
        } else {
            Ok(Self(value))
        }
    }
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl Serialize for BoundedSetupString {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}
impl<'de> Deserialize<'de> for BoundedSetupString {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}
impl JsonSchema for BoundedSetupString {
    fn inline_schema() -> bool {
        true
    }
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("BoundedSetupString")
    }
    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({"type":"string","minLength":1,"maxLength":2048,"pattern":"^[^\\p{Cc}\\p{Cf}]+$"})
    }
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(untagged)]
pub enum SafeSetupValue {
    Bool(bool),
    Integer(i64),
    Code(SafeCode),
    String(BoundedSetupString),
}

/// A value this build does not know reads as `string`: a string, which the
/// daemon validates.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
#[schemars(extend("x-open" = true))]
pub enum SetupFieldType {
    Code,
    Integer,
    Bool,
    #[serde(other)]
    String,
}

/// A value this build does not know reads as `api_key`: a secret like an API
/// key.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
#[schemars(extend("x-open" = true))]
pub enum CredentialFieldType {
    AccessToken,
    AccessKeyId,
    SecretAccessKey,
    SessionToken,
    #[serde(other)]
    ApiKey,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct SetupFieldValidation {
    pub value_type: SetupFieldType,
    #[serde(deserialize_with = "crate::deserialize_required_option")]
    #[schemars(with = "crate::NullableSchema<u32>", required)]
    pub min_length: Option<u32>,
    #[serde(deserialize_with = "crate::deserialize_required_option")]
    #[schemars(with = "crate::NullableSchema<u32>", required)]
    pub max_length: Option<u32>,
    #[serde(deserialize_with = "crate::deserialize_required_option")]
    #[schemars(with = "crate::NullableSchema<i64>", required)]
    pub minimum: Option<i64>,
    #[serde(deserialize_with = "crate::deserialize_required_option")]
    #[schemars(with = "crate::NullableSchema<i64>", required)]
    pub maximum: Option<i64>,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct SetupFieldDescriptor {
    pub id: SetupFieldId,
    pub display_name: SafeDisplayText,
    pub help: SafeDisplayText,
    pub required: bool,
    #[serde(deserialize_with = "crate::deserialize_required_option")]
    #[schemars(with = "crate::NullableSchema<SafeSetupValue>", required)]
    pub default: Option<SafeSetupValue>,
    pub validation: SetupFieldValidation,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct AuthCredentialDescriptor {
    pub id: AuthFieldName,
    pub display_name: SafeDisplayText,
    pub help: SafeDisplayText,
    pub required: bool,
    pub credential_type: CredentialFieldType,
}

#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize)]
pub struct AuthMethodDescriptor {
    pub id: AuthMethodId,
    pub display_name: SafeDisplayText,
    #[schemars(length(min = 1, max = 32))]
    pub credentials: Vec<AuthCredentialDescriptor>,
}
impl<'de> Deserialize<'de> for AuthMethodDescriptor {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Wire {
            id: AuthMethodId,
            display_name: SafeDisplayText,
            credentials: Vec<AuthCredentialDescriptor>,
        }
        let wire = Wire::deserialize(deserializer)?;
        if wire.credentials.is_empty()
            || wire.credentials.len() > 32
            || wire
                .credentials
                .windows(2)
                .any(|pair| pair[0].id >= pair[1].id)
        {
            return Err(serde::de::Error::custom(
                "auth credentials must be a sorted unique list of 1..=32 fields",
            ));
        }
        Ok(Self {
            id: wire.id,
            display_name: wire.display_name,
            credentials: wire.credentials,
        })
    }
}

/// A value this build does not know reads as `current`.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
#[schemars(extend("x-open" = true))]
pub enum ProviderPresence {
    Removed,
    #[serde(other)]
    Current,
}

/// A value this build does not know reads as `unsupported`: a provider that
/// cannot be connected.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
#[schemars(extend("x-open" = true))]
pub enum ProviderSupportState {
    Supported,
    Quarantined,
    #[serde(other)]
    Unsupported,
}

#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize)]
pub struct ProviderSupport {
    pub state: ProviderSupportState,
    #[serde(deserialize_with = "crate::deserialize_required_option")]
    #[schemars(with = "crate::NullableSchema<SafeCode>", required)]
    pub reason: Option<SafeCode>,
}
impl<'de> Deserialize<'de> for ProviderSupport {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Wire {
            state: ProviderSupportState,
            #[serde(deserialize_with = "crate::deserialize_required_option")]
            reason: Option<SafeCode>,
        }
        let wire = Wire::deserialize(d)?;
        if (wire.state == ProviderSupportState::Supported) != wire.reason.is_none() {
            return Err(serde::de::Error::custom(
                "supported providers have no reason; unsupported and quarantined providers require one",
            ));
        }
        Ok(Self {
            state: wire.state,
            reason: wire.reason,
        })
    }
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct QuarantineDiagnostic {
    pub code: SafeCode,
    pub message: SafeErrorMessage,
}

/// A value this build does not know reads as `unconfigured`.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
#[schemars(extend("x-open" = true))]
pub enum ProviderConfigurationState {
    Authored,
    Stored,
    AuthoredAndStored,
    #[serde(other)]
    Unconfigured,
}

/// A value this build does not know reads as `no_auth`.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
#[schemars(extend("x-open" = true))]
pub enum EffectiveAuthSource {
    AuthoredApiKey,
    AuthoredOverride,
    ProviderStore,
    #[serde(other)]
    NoAuth,
}

/// A value this build does not know reads as `unavailable`.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
#[schemars(extend("x-open" = true))]
pub enum EffectiveAuthState {
    AuthoredApiKey,
    AuthoredOverride,
    ProviderStore,
    NoAuth,
    #[serde(other)]
    Unavailable,
}

#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize)]
pub struct DurableConnectionDescriptor {
    pub provider_id: ProviderId,
    pub setup_values: BTreeMap<SetupFieldId, SafeSetupValue>,
    pub auth_method: AuthMethodId,
    #[schemars(length(max = 32))]
    pub credential_fields: Vec<AuthFieldName>,
    pub connection_generation: ProviderConnectionGeneration,
    pub connected_at: Timestamp,
}
impl<'de> Deserialize<'de> for DurableConnectionDescriptor {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Wire {
            provider_id: ProviderId,
            setup_values: BTreeMap<SetupFieldId, SafeSetupValue>,
            auth_method: AuthMethodId,
            credential_fields: Vec<AuthFieldName>,
            connection_generation: ProviderConnectionGeneration,
            connected_at: Timestamp,
        }
        let wire = Wire::deserialize(d)?;
        if wire.setup_values.len() > 32
            || wire.credential_fields.len() > 32
            || wire
                .credential_fields
                .windows(2)
                .any(|pair| pair[0] >= pair[1])
        {
            return Err(serde::de::Error::custom(
                "durable connection setup and credential metadata are not bounded and sorted",
            ));
        }
        Ok(Self {
            provider_id: wire.provider_id,
            setup_values: wire.setup_values,
            auth_method: wire.auth_method,
            credential_fields: wire.credential_fields,
            connection_generation: wire.connection_generation,
            connected_at: wire.connected_at,
        })
    }
}

/// Why a provider's catalog model is not selectable.
///
/// A value this build does not know reads as `unsupported`.
#[derive(
    Clone, Copy, Debug, Deserialize, Eq, Hash, JsonSchema, Ord, PartialEq, PartialOrd, Serialize,
)]
#[serde(rename_all = "snake_case")]
#[schemars(extend("x-open" = true))]
pub enum ModelUnavailableKind {
    /// The catalog parser quarantined the model record.
    Quarantined,
    /// Provider setup values (region, resource, deployment) are missing.
    NeedsSetup,
    /// No usable credentials are configured or stored for the provider.
    NeedsCredentials,
    /// The record parsed, but cookie cannot drive the model.
    #[serde(other)]
    Unsupported,
}

/// Per-provider model availability counts over every catalog model row that
/// is not deprecated, disabled, or non-text.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct ProviderModelCounts {
    pub available: u32,
    pub quarantined: u32,
    pub unsupported: u32,
    pub needs_setup: u32,
    pub needs_credentials: u32,
}

impl ProviderModelCounts {
    #[must_use]
    pub const fn unavailable(&self) -> u32 {
        self.quarantined
            .saturating_add(self.unsupported)
            .saturating_add(self.needs_setup)
            .saturating_add(self.needs_credentials)
    }

    #[must_use]
    pub const fn count(&self, kind: ModelUnavailableKind) -> u32 {
        match kind {
            ModelUnavailableKind::Quarantined => self.quarantined,
            ModelUnavailableKind::Unsupported => self.unsupported,
            ModelUnavailableKind::NeedsSetup => self.needs_setup,
            ModelUnavailableKind::NeedsCredentials => self.needs_credentials,
        }
    }
}

/// One catalog model that is listed but not selectable, with its reason.
#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize)]
pub struct UnavailableModelDescriptor {
    pub id: ProviderModelId,
    pub display_name: SafeDisplayText,
    pub kind: ModelUnavailableKind,
    /// Quarantine code or compiler reason; null for setup and credential gaps.
    #[serde(deserialize_with = "crate::deserialize_required_option")]
    #[schemars(with = "crate::NullableSchema<SafeErrorMessage>", required)]
    pub reason: Option<SafeErrorMessage>,
}
impl<'de> Deserialize<'de> for UnavailableModelDescriptor {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Wire {
            id: ProviderModelId,
            display_name: SafeDisplayText,
            kind: ModelUnavailableKind,
            #[serde(deserialize_with = "crate::deserialize_required_option")]
            reason: Option<SafeErrorMessage>,
        }
        let wire = Wire::deserialize(d)?;
        Ok(Self {
            id: wire.id,
            display_name: wire.display_name,
            kind: wire.kind,
            reason: wire.reason,
        })
    }
}

#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize)]
pub struct ProviderDescriptor {
    pub id: ProviderId,
    pub display_name: SafeDisplayText,
    pub presence: ProviderPresence,
    pub support: ProviderSupport,
    pub setup_fields: Vec<SetupFieldDescriptor>,
    pub auth_methods: Vec<AuthMethodDescriptor>,
    pub configuration: ProviderConfigurationState,
    pub effective_auth_state: EffectiveAuthState,
    #[serde(deserialize_with = "crate::deserialize_required_option")]
    #[schemars(with = "crate::NullableSchema<DurableConnectionDescriptor>", required)]
    pub durable_connection: Option<DurableConnectionDescriptor>,
    #[serde(deserialize_with = "crate::deserialize_required_option")]
    #[schemars(with = "crate::NullableSchema<QuarantineDiagnostic>", required)]
    pub quarantine: Option<QuarantineDiagnostic>,
    /// The catalog's provider documentation URL, when it has one.
    #[serde(deserialize_with = "crate::deserialize_required_option")]
    #[schemars(with = "crate::NullableSchema<SafeDisplayText>", required)]
    pub documentation_url: Option<SafeDisplayText>,
    /// Environment variable names the catalog associates with the provider,
    /// in catalog order. Display-only: cookie never reads them implicitly.
    #[schemars(length(max = 32))]
    pub environment: Vec<CredentialFieldName>,
    pub model_counts: ProviderModelCounts,
    /// Unavailable models with reasons, strictly sorted by ID. Listed only
    /// for configured (authored or stored) providers; unconfigured providers
    /// report counts alone.
    #[schemars(length(max = 4096))]
    pub unavailable_models: Vec<UnavailableModelDescriptor>,
    /// Interactive sign-ins the provider offers instead of typed credentials,
    /// connected with `provider.auth.begin` and `provider.auth.complete`
    /// (client protocol 1.49.0). Empty when it offers none, and from older
    /// daemons.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 4))]
    pub sign_in_methods: Vec<SignInMethodDescriptor>,
    /// The account a sign-in connected (client protocol 1.49.0).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sign_in_account: Option<SignInAccountDescriptor>,
}

/// An interactive sign-in. The browser must return to
/// `http://127.0.0.1:<any port><redirect_path>`, which the client catches with
/// a loopback listener, or the user pastes back.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct SignInMethodDescriptor {
    pub id: AuthMethodId,
    /// The sign-in button's label, as the provider's branding requires.
    pub display_name: SafeDisplayText,
    pub redirect_path: SafeDisplayText,
}

/// The account a sign-in connected.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct SignInAccountDescriptor {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<SafeDisplayText>,
    /// The sign-in can no longer refresh; the user must sign in again.
    #[serde(default)]
    pub sign_in_required: bool,
}
impl<'de> Deserialize<'de> for ProviderDescriptor {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Wire {
            id: ProviderId,
            display_name: SafeDisplayText,
            presence: ProviderPresence,
            support: ProviderSupport,
            setup_fields: Vec<SetupFieldDescriptor>,
            auth_methods: Vec<AuthMethodDescriptor>,
            configuration: ProviderConfigurationState,
            effective_auth_state: EffectiveAuthState,
            #[serde(deserialize_with = "crate::deserialize_required_option")]
            durable_connection: Option<DurableConnectionDescriptor>,
            #[serde(deserialize_with = "crate::deserialize_required_option")]
            quarantine: Option<QuarantineDiagnostic>,
            #[serde(deserialize_with = "crate::deserialize_required_option")]
            documentation_url: Option<SafeDisplayText>,
            environment: Vec<CredentialFieldName>,
            model_counts: ProviderModelCounts,
            unavailable_models: Vec<UnavailableModelDescriptor>,
            #[serde(default)]
            sign_in_methods: Vec<SignInMethodDescriptor>,
            #[serde(default)]
            sign_in_account: Option<SignInAccountDescriptor>,
        }
        let wire = Wire::deserialize(d)?;
        if wire.setup_fields.len() > 32
            || wire.environment.len() > 32
            || wire.unavailable_models.len() > 4096
            || wire
                .unavailable_models
                .windows(2)
                .any(|pair| pair[0].id >= pair[1].id)
            || wire
                .setup_fields
                .windows(2)
                .any(|pair| pair[0].id >= pair[1].id)
            || wire.auth_methods.len() > 16
            || wire
                .auth_methods
                .windows(2)
                .any(|pair| pair[0].id >= pair[1].id)
            || (wire.support.state == ProviderSupportState::Quarantined)
                != wire.quarantine.is_some()
            || wire
                .durable_connection
                .as_ref()
                .is_some_and(|connection| connection.provider_id != wire.id)
        {
            return Err(serde::de::Error::custom(
                "provider descriptor metadata is inconsistent or not strictly sorted",
            ));
        }
        Ok(Self {
            id: wire.id,
            display_name: wire.display_name,
            presence: wire.presence,
            support: wire.support,
            setup_fields: wire.setup_fields,
            auth_methods: wire.auth_methods,
            configuration: wire.configuration,
            effective_auth_state: wire.effective_auth_state,
            durable_connection: wire.durable_connection,
            quarantine: wire.quarantine,
            documentation_url: wire.documentation_url,
            environment: wire.environment,
            model_counts: wire.model_counts,
            unavailable_models: wire.unavailable_models,
            sign_in_methods: wire.sign_in_methods,
            sign_in_account: wire.sign_in_account,
        })
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct ProviderCredentialValues(BTreeMap<String, String>);

impl ProviderCredentialValues {
    #[must_use]
    pub fn get(&self, field: &AuthFieldName) -> Option<&str> {
        self.0.get(field.as_str()).map(String::as_str)
    }
    pub fn field_names(&self) -> impl Iterator<Item = &str> {
        self.0.keys().map(String::as_str)
    }
}
impl JsonSchema for ProviderCredentialValues {
    fn inline_schema() -> bool {
        true
    }
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("ProviderCredentialValues")
    }
    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        let names = generator.subschema_for::<AuthFieldName>();
        json_schema!({
            "type": "object",
            "description": "Credential fields of the auth method, by field name.",
            "maxProperties": 32,
            "propertyNames": names,
            "additionalProperties": {"type": "string", "minLength": 1, "maxLength": 16384}
        })
    }
}
impl fmt::Debug for ProviderCredentialValues {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ProviderCredentialValues(<redacted>)")
    }
}
impl Drop for ProviderCredentialValues {
    fn drop(&mut self) {
        for value in self.0.values_mut() {
            value.zeroize();
        }
    }
}
impl<'de> Deserialize<'de> for ProviderCredentialValues {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = BTreeMap<String, String>;
            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a unique credential field map")
            }
            fn visit_map<A>(self, mut access: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::MapAccess<'de>,
            {
                let mut raw = BTreeMap::new();
                while let Some((key, value)) = access.next_entry::<String, String>()? {
                    if raw.len() >= 32 {
                        return Err(serde::de::Error::custom("auth values exceed 32 fields"));
                    }
                    if raw.insert(key, value).is_some() {
                        return Err(serde::de::Error::custom("duplicate auth credential field"));
                    }
                }
                Ok(raw)
            }
        }
        let raw = deserializer.deserialize_map(Visitor)?;
        if raw.len() > 32
            || raw.iter().any(|(key, value)| {
                AuthFieldName::new(key.clone()).is_err()
                    || value.is_empty()
                    || value.len() > 16 * 1024
            })
        {
            return Err(serde::de::Error::custom(
                "auth values must contain at most 32 strict bounded nonempty credential fields",
            ));
        }
        Ok(Self(raw))
    }
}

#[derive(Clone, Debug, Eq, JsonSchema, PartialEq)]
#[schemars(deny_unknown_fields)]
pub struct ProviderConnectParams {
    pub provider_id: ProviderId,
    /// The revision of the catalog view the form was filled from. A daemon
    /// whose view has changed since refuses with `catalog_revision_conflict`.
    pub expected_revision: ViewRevision,
    pub setup_values: BTreeMap<SetupFieldId, SafeSetupValue>,
    pub auth_method: AuthMethodId,
    pub auth_values: ProviderCredentialValues,
    pub client_connect_id: ClientConnectId,
}

impl ProviderConnectParams {
    pub fn validate(&self) -> Result<(), ProviderSchemaError> {
        if self.setup_values.len() > 32 {
            Err(ProviderSchemaError::TooManySetupFields)
        } else {
            Ok(())
        }
    }
}
impl<'de> Deserialize<'de> for ProviderConnectParams {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            provider_id: ProviderId,
            expected_revision: ViewRevision,
            #[serde(deserialize_with = "deserialize_setup_values")]
            setup_values: BTreeMap<SetupFieldId, SafeSetupValue>,
            auth_method: AuthMethodId,
            auth_values: ProviderCredentialValues,
            client_connect_id: ClientConnectId,
        }
        let w = Wire::deserialize(d)?;
        let value = Self {
            provider_id: w.provider_id,
            expected_revision: w.expected_revision,
            setup_values: w.setup_values,
            auth_method: w.auth_method,
            auth_values: w.auth_values,
            client_connect_id: w.client_connect_id,
        };
        value.validate().map_err(serde::de::Error::custom)?;
        Ok(value)
    }
}

fn deserialize_setup_values<'de, D>(
    d: D,
) -> Result<BTreeMap<SetupFieldId, SafeSetupValue>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct Visitor;
    impl<'de> serde::de::Visitor<'de> for Visitor {
        type Value = BTreeMap<SetupFieldId, SafeSetupValue>;
        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("a unique bounded setup field map")
        }
        fn visit_map<A>(self, mut access: A) -> Result<Self::Value, A::Error>
        where
            A: serde::de::MapAccess<'de>,
        {
            let mut values = BTreeMap::new();
            while let Some((key, value)) = access.next_entry::<SetupFieldId, SafeSetupValue>()? {
                if values.len() >= 32 {
                    return Err(serde::de::Error::custom("setup values exceed 32 fields"));
                }
                if values.insert(key, value).is_some() {
                    return Err(serde::de::Error::custom("duplicate setup field"));
                }
            }
            Ok(values)
        }
    }
    d.deserialize_map(Visitor)
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct DurableProviderReceipt {
    pub receipt_id: DurableProviderReceiptId,
    pub committed_at: Timestamp,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct ProviderConnectResult {
    pub durable_connection: DurableConnectionDescriptor,
    pub effective_auth_source: EffectiveAuthSource,
    pub runtime: ModelCatalogView,
    pub replayed: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderDisconnectParams {
    pub provider_id: ProviderId,
    /// The revision of the catalog view the provider was read from.
    pub expected_revision: ViewRevision,
    #[serde(deserialize_with = "crate::deserialize_required_option")]
    #[schemars(with = "crate::NullableSchema<ProviderConnectionGeneration>", required)]
    pub expected_connection_generation: Option<ProviderConnectionGeneration>,
    pub client_request_id: ClientRequestId,
}

#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize)]
pub struct ProviderDisconnectResult {
    pub durable_receipt: DurableProviderReceipt,
    pub provider_id: ProviderId,
    pub disconnected: bool,
    pub effective_auth_state: EffectiveAuthState,
    pub runtime: RuntimeSnapshotResult,
    pub replayed: bool,
}
impl<'de> Deserialize<'de> for ProviderDisconnectResult {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Wire {
            durable_receipt: DurableProviderReceipt,
            provider_id: ProviderId,
            disconnected: bool,
            effective_auth_state: EffectiveAuthState,
            runtime: RuntimeSnapshotResult,
            replayed: bool,
        }
        let w = Wire::deserialize(d)?;
        if !w.disconnected {
            return Err(serde::de::Error::custom(
                "disconnected must be true on success",
            ));
        }
        Ok(Self {
            durable_receipt: w.durable_receipt,
            provider_id: w.provider_id,
            disconnected: w.disconnected,
            effective_auth_state: w.effective_auth_state,
            runtime: w.runtime,
            replayed: w.replayed,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProviderSchemaError {
    InvalidCounter,
    InvalidSetupString,
    TooManySetupFields,
}
impl fmt::Display for ProviderSchemaError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidCounter => "counter must be positive and I-JSON safe",
            Self::InvalidSetupString => "setup string must be control-free and 1..=2048 bytes",
            Self::TooManySetupFields => "setup values exceed 32 fields",
        })
    }
}
impl std::error::Error for ProviderSchemaError {}

/// `provider.auth.begin` (client protocol 1.49.0): starts a provider's
/// interactive sign-in. The daemon holds the PKCE verifier and does the code
/// exchange; tokens never cross the connection.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderAuthBeginParams {
    pub provider_id: ProviderId,
    /// One of the provider's `sign_in_methods`.
    pub method: AuthMethodId,
    /// Where the browser returns: `http://127.0.0.1:<port><redirect_path>`,
    /// caught by the client or pasted back to `provider.auth.complete`. Null
    /// asks the daemon to catch it on its own machine and announce the
    /// outcome with `provider.auth.completed`, for a client whose browser
    /// runs beside the daemon.
    #[serde(deserialize_with = "crate::deserialize_required_option")]
    #[schemars(with = "crate::NullableSchema<String>", required)]
    pub redirect_uri: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct ProviderAuthBeginResult {
    pub provider_id: ProviderId,
    pub authorization_url: String,
    /// The redirect the sign-in uses: the client's, or the daemon's own.
    pub redirect_uri: String,
}

/// The address the browser landed on, carried back to the daemon. Only the
/// single-use authorization code crosses the connection.
#[derive(Clone, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderAuthCompleteParams {
    pub provider_id: ProviderId,
    pub callback_url: String,
}

impl std::fmt::Debug for ProviderAuthCompleteParams {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProviderAuthCompleteParams")
            .field("provider_id", &self.provider_id)
            .field("callback_url", &"<redacted>")
            .finish()
    }
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct ProviderAuthCompleteResult {
    pub provider_id: ProviderId,
    pub account: SignInAccountDescriptor,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderAuthCancelParams {
    pub provider_id: ProviderId,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct ProviderAuthCancelResult {
    pub provider_id: ProviderId,
}

/// `provider.auth.completed`: a sign-in whose redirect the daemon caught
/// itself has finished, successfully or not.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct ProviderAuthCompletedNotification {
    pub provider_id: ProviderId,
    /// The signed-in account when it succeeded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<SignInAccountDescriptor>,
    /// Why it failed, as `code: message`, when it did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<SafeDisplayText>,
}
