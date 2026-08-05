use std::{borrow::Cow, collections::BTreeMap, fmt};

use jiff::Timestamp;
use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Serialize};
use ts_rs::TS;
use zeroize::Zeroize;

use crate::*;

const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

macro_rules! positive_counter {
    ($name:ident, $description:literal) => {
        #[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, TS)]
        #[serde(transparent)]
        #[ts(type = "number")]
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

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, TS)]
#[ts(type = "string")]
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

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(untagged)]
pub enum SafeSetupValue {
    Bool(bool),
    Integer(i64),
    Code(SafeCode),
    String(BoundedSetupString),
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum SetupFieldType {
    String,
    Code,
    Integer,
    Bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum CredentialFieldType {
    ApiKey,
    AccessToken,
    AccessKeyId,
    SecretAccessKey,
    SessionToken,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
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

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct SetupFieldDescriptor {
    #[ts(type = "SetupFieldId")]
    pub id: SetupFieldId,
    pub display_name: SafeDisplayText,
    pub help: SafeDisplayText,
    pub required: bool,
    #[serde(deserialize_with = "crate::deserialize_required_option")]
    #[schemars(with = "crate::NullableSchema<SafeSetupValue>", required)]
    pub default: Option<SafeSetupValue>,
    pub validation: SetupFieldValidation,
    pub safe_to_project: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct AuthCredentialDescriptor {
    #[ts(type = "AuthFieldName")]
    pub id: AuthFieldName,
    pub display_name: SafeDisplayText,
    pub help: SafeDisplayText,
    pub required: bool,
    pub credential_type: CredentialFieldType,
}

#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct AuthMethodDescriptor {
    #[ts(type = "AuthMethodId")]
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
        #[serde(deny_unknown_fields)]
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

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum ProviderPresence {
    Current,
    Removed,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum ProviderSupportState {
    Supported,
    Unsupported,
    Quarantined,
}

#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
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
        #[serde(deny_unknown_fields)]
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

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct QuarantineDiagnostic {
    pub code: SafeCode,
    pub message: SafeErrorMessage,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum ProviderConfigurationState {
    Unconfigured,
    Authored,
    Stored,
    AuthoredAndStored,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum EffectiveAuthSource {
    AuthoredApiKey,
    AuthoredOverride,
    ProviderStore,
    NoAuth,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum EffectiveAuthState {
    AuthoredApiKey,
    AuthoredOverride,
    ProviderStore,
    NoAuth,
    Unavailable,
}

#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct DurableConnectionDescriptor {
    #[ts(type = "ProviderId")]
    pub provider_id: ProviderId,
    #[ts(type = "Record<string, import(\"./SafeSetupValue.js\").SafeSetupValue>")]
    pub setup_values: BTreeMap<SetupFieldId, SafeSetupValue>,
    pub setup_fingerprint: Sha256Digest,
    pub recipe_fingerprint: Sha256Digest,
    #[ts(type = "AuthMethodId")]
    pub auth_method: AuthMethodId,
    #[schemars(length(max = 32))]
    #[ts(type = "Array<AuthFieldName>")]
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
        #[serde(deny_unknown_fields)]
        struct Wire {
            provider_id: ProviderId,
            setup_values: BTreeMap<SetupFieldId, SafeSetupValue>,
            setup_fingerprint: Sha256Digest,
            recipe_fingerprint: Sha256Digest,
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
            setup_fingerprint: wire.setup_fingerprint,
            recipe_fingerprint: wire.recipe_fingerprint,
            auth_method: wire.auth_method,
            credential_fields: wire.credential_fields,
            connection_generation: wire.connection_generation,
            connected_at: wire.connected_at,
        })
    }
}

#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct ProviderDescriptor {
    #[ts(type = "ProviderId")]
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
}
impl<'de> Deserialize<'de> for ProviderDescriptor {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
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
        }
        let wire = Wire::deserialize(d)?;
        if wire.setup_fields.len() > 32
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderConnectParams {
    pub provider_id: ProviderId,
    pub expected_catalog_revision: CatalogRevision,
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
            expected_catalog_revision: CatalogRevision,
            #[serde(deserialize_with = "deserialize_setup_values")]
            setup_values: BTreeMap<SetupFieldId, SafeSetupValue>,
            auth_method: AuthMethodId,
            auth_values: ProviderCredentialValues,
            client_connect_id: ClientConnectId,
        }
        let w = Wire::deserialize(d)?;
        let value = Self {
            provider_id: w.provider_id,
            expected_catalog_revision: w.expected_catalog_revision,
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

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct DurableProviderReceipt {
    pub receipt_id: DurableProviderReceiptId,
    #[ts(type = "ProviderStoreRevision")]
    pub store_revision: ProviderStoreRevision,
    #[ts(type = "ProviderStateRevision")]
    pub provider_state_revision: ProviderStateRevision,
    pub committed_at: Timestamp,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct ProviderConnectResult {
    pub durable_connection: DurableConnectionDescriptor,
    pub effective_auth_source: EffectiveAuthSource,
    pub runtime: RuntimeSnapshotV1,
    pub replayed: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct ProviderDisconnectParams {
    #[ts(type = "ProviderId")]
    pub provider_id: ProviderId,
    #[ts(type = "RuntimeRevision")]
    pub expected_runtime_revision: RuntimeRevision,
    #[ts(type = "ProviderStateRevision")]
    pub expected_provider_state_revision: ProviderStateRevision,
    #[serde(deserialize_with = "crate::deserialize_required_option")]
    #[schemars(with = "crate::NullableSchema<ProviderConnectionGeneration>", required)]
    pub expected_connection_generation: Option<ProviderConnectionGeneration>,
    pub client_request_id: ClientRequestId,
}

#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct ProviderDisconnectResult {
    pub durable_receipt: DurableProviderReceipt,
    #[ts(type = "ProviderId")]
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
        #[serde(deny_unknown_fields)]
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

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum ProviderConnectErrorCode {
    UnknownProvider,
    UnsupportedProvider,
    QuarantinedProvider,
    RemovedWithoutRetainedRecipeMatch,
    CatalogRevisionConflict,
    MissingSetupField,
    InvalidSetupField,
    MissingCredential,
    InvalidCredential,
    UnsupportedAuthMethod,
    ProviderStoreWriteFailed,
    RuntimeCompileFailed,
    IdempotencyConflict,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct ProviderConnectError {
    pub code: ProviderConnectErrorCode,
    #[ts(type = "ProviderId")]
    pub provider_id: ProviderId,
    pub client_connect_id: ClientConnectId,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum ProviderDisconnectErrorCode {
    InvalidProvider,
    CustomProviderNotStoreBacked,
    RuntimeRevisionConflict,
    ProviderStateRevisionConflict,
    StaleProviderConnectionGeneration,
    ProviderStoreWriteFailed,
    RuntimeCompileFailed,
    IdempotencyConflict,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct ProviderDisconnectError {
    pub code: ProviderDisconnectErrorCode,
    #[ts(type = "ProviderId")]
    pub provider_id: ProviderId,
    pub client_request_id: ClientRequestId,
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
