use std::{collections::BTreeMap, fmt};

use cookie_agent_identity::{
    AuthFieldName, AuthMethodId, CatalogRevision, ProviderId, ProviderModelId,
    ProviderSetupRecipeId, ProviderStateRevision, ProviderStoreRevision, RecipeCompilerVersion,
    SafeCode, SetupFieldId,
};
use jiff::Timestamp;
use serde::{Deserialize, Deserializer, Serialize};
use thiserror::Error;
use uuid::Uuid;
use zeroize::Zeroize;

use crate::{SafeSetupValue, Sha256Digest, secure_store::SecureStoreError};

pub(crate) const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
pub(crate) const MAX_SETUP_FIELDS: usize = 32;
pub(crate) const MAX_AUTH_FIELDS: usize = 32;
pub(crate) const MAX_PROVIDERS: usize = 4096;
pub(crate) const MAX_RECEIPTS: usize = 65_536;
pub(crate) const MAX_POLICY_DEPTH: usize = 8;
pub(crate) const MAX_POLICY_ITEMS: usize = 4096;

macro_rules! bounded_client_id {
    ($name:ident) => {
        #[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Result<Self, ProviderStoreError> {
                let value = value.into();
                if value.is_empty() || value.len() > 256 || value.chars().any(char::is_control) {
                    Err(ProviderStoreError::InvalidRequest)
                } else {
                    Ok(Self(value))
                }
            }

            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                let value = String::deserialize(deserializer)?;
                Self::new(value).map_err(serde::de::Error::custom)
            }
        }
    };
}

bounded_client_id!(ClientConnectId);
bounded_client_id!(ClientRequestId);

macro_rules! positive_counter {
    ($name:ident) => {
        #[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
        #[serde(transparent)]
        pub struct $name(u64);

        impl $name {
            pub fn new(value: u64) -> Result<Self, ProviderStoreError> {
                if (1..=MAX_SAFE_INTEGER).contains(&value) {
                    Ok(Self(value))
                } else {
                    Err(ProviderStoreError::InvalidStore)
                }
            }

            #[must_use]
            pub const fn get(self) -> u64 {
                self.0
            }

            pub(crate) fn checked_next(self) -> Result<Self, ProviderStoreError> {
                self.0
                    .checked_add(1)
                    .filter(|value| *value <= MAX_SAFE_INTEGER)
                    .map(Self)
                    .ok_or(ProviderStoreError::GenerationExhausted)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                Self::new(u64::deserialize(deserializer)?).map_err(serde::de::Error::custom)
            }
        }
    };
}

positive_counter!(ProviderStoreGeneration);
positive_counter!(ProviderConnectionGeneration);

/// Bounded non-secret policy string retained for removed-provider reconstruction.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct SafePolicyString(String);

impl SafePolicyString {
    pub fn new(value: impl Into<String>) -> Result<Self, ProviderStoreError> {
        let value = value.into();
        if value.is_empty()
            || value.len() > 8192
            || value.chars().any(char::is_control)
            || value.contains("${env:")
        {
            Err(ProviderStoreError::InvalidRequest)
        } else {
            Ok(Self(value))
        }
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for SafePolicyString {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

/// Strict JSON-safe metadata for normalized managed model overrides.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum SafePolicyValue {
    Bool(bool),
    Integer(i64),
    Code(SafeCode),
    String(SafePolicyString),
    Digest(Sha256Digest),
    List(Vec<SafePolicyValue>),
    Map(BTreeMap<String, SafePolicyValue>),
}

impl SafePolicyValue {
    pub(crate) fn validate(
        &self,
        depth: usize,
        items: &mut usize,
    ) -> Result<(), ProviderStoreError> {
        if depth > MAX_POLICY_DEPTH {
            return Err(ProviderStoreError::InvalidStore);
        }
        *items = items
            .checked_add(1)
            .ok_or(ProviderStoreError::InvalidStore)?;
        if *items > MAX_POLICY_ITEMS {
            return Err(ProviderStoreError::InvalidStore);
        }
        match self {
            Self::Integer(value)
                if !(-9_007_199_254_740_991..=9_007_199_254_740_991).contains(value) =>
            {
                return Err(ProviderStoreError::InvalidStore);
            }
            Self::List(values) => {
                for value in values {
                    value.validate(depth + 1, items)?;
                }
            }
            Self::Map(values) => {
                for (key, value) in values {
                    if key.is_empty() || key.len() > 128 || key.chars().any(char::is_control) {
                        return Err(ProviderStoreError::InvalidStore);
                    }
                    value.validate(depth + 1, items)?;
                }
            }
            Self::Bool(_)
            | Self::Integer(_)
            | Self::Code(_)
            | Self::String(_)
            | Self::Digest(_) => {}
        }
        Ok(())
    }
}

/// Safe normalized metadata for one sparse managed model override.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoredModelOverrideProjection {
    #[serde(default)]
    pub metadata: BTreeMap<String, SafePolicyValue>,
}

/// Safe recipe, endpoint, source, and model-policy projection retained with a connection.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoredProviderPolicyProjection {
    pub catalog_revision: CatalogRevision,
    pub family_id: SafePolicyString,
    pub setup_recipe: ProviderSetupRecipeId,
    pub adapter_id: SafePolicyString,
    pub compiler_version: RecipeCompilerVersion,
    pub default_endpoint_identity: SafePolicyString,
    pub package_claim: SafePolicyString,
    pub source_record_digest: Sha256Digest,
    pub recipe_fingerprint: Sha256Digest,
    #[serde(default)]
    pub model_overrides: BTreeMap<ProviderModelId, StoredModelOverrideProjection>,
}

impl StoredProviderPolicyProjection {
    pub(crate) fn validate(&self) -> Result<(), ProviderStoreError> {
        if self.model_overrides.len() > MAX_PROVIDERS {
            return Err(ProviderStoreError::InvalidStore);
        }
        let mut items = 0;
        for projection in self.model_overrides.values() {
            for (key, value) in &projection.metadata {
                if key.is_empty() || key.len() > 128 || key.chars().any(char::is_control) {
                    return Err(ProviderStoreError::InvalidStore);
                }
                value.validate(1, &mut items)?;
            }
        }
        Ok(())
    }
}

#[derive(Eq, PartialEq)]
pub(crate) struct SecretValue(String);

impl SecretValue {
    pub(crate) fn new(mut value: String) -> Result<Self, ProviderStoreError> {
        if value.is_empty() || value.len() > 16 * 1024 {
            value.zeroize();
            Err(ProviderStoreError::InvalidRequest)
        } else {
            Ok(Self(value))
        }
    }

    pub(crate) fn expose(&self) -> &str {
        &self.0
    }
}

impl Clone for SecretValue {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl fmt::Debug for SecretValue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SecretValue(<redacted>)")
    }
}

impl Drop for SecretValue {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

/// Secret credential values. Debug is redacted and serialization is intentionally unavailable.
#[derive(Clone, Eq, PartialEq)]
pub struct ProviderAuthValues(pub(crate) BTreeMap<AuthFieldName, SecretValue>);

impl ProviderAuthValues {
    pub fn new(values: BTreeMap<AuthFieldName, String>) -> Result<Self, ProviderStoreError> {
        if values.len() > MAX_AUTH_FIELDS {
            return Err(ProviderStoreError::InvalidRequest);
        }
        values
            .into_iter()
            .map(|(field, value)| Ok((field, SecretValue::new(value)?)))
            .collect::<Result<BTreeMap<_, _>, _>>()
            .map(Self)
    }

    #[must_use]
    pub fn get(&self, field: &AuthFieldName) -> Option<&str> {
        self.0.get(field).map(SecretValue::expose)
    }

    pub fn field_names(&self) -> impl ExactSizeIterator<Item = &AuthFieldName> {
        self.0.keys()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Debug for ProviderAuthValues {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderAuthValues")
            .field("fields", &self.0.keys().collect::<Vec<_>>())
            .finish()
    }
}

/// One active managed provider connection. Secret values are accessible only by field lookup.
#[derive(Clone, Eq, PartialEq)]
pub struct StoredManagedConnection {
    pub provider_id: ProviderId,
    pub setup_values: BTreeMap<SetupFieldId, SafeSetupValue>,
    pub setup_fingerprint: Sha256Digest,
    pub auth_method: AuthMethodId,
    pub(crate) auth_values: ProviderAuthValues,
    pub connection_generation: ProviderConnectionGeneration,
    pub policy: StoredProviderPolicyProjection,
    pub connected_at: Timestamp,
}

impl StoredManagedConnection {
    #[must_use]
    pub fn credential(&self, field: &AuthFieldName) -> Option<&str> {
        self.auth_values.get(field)
    }

    pub fn credential_fields(&self) -> impl ExactSizeIterator<Item = &AuthFieldName> {
        self.auth_values.field_names()
    }

    #[must_use]
    pub fn descriptor(&self) -> DurableConnectionDescriptor {
        DurableConnectionDescriptor {
            provider_id: self.provider_id.clone(),
            setup_values: self.setup_values.clone(),
            setup_fingerprint: self.setup_fingerprint.clone(),
            recipe_fingerprint: self.policy.recipe_fingerprint.clone(),
            auth_method: self.auth_method.clone(),
            credential_fields: self.auth_values.0.keys().cloned().collect(),
            connection_generation: self.connection_generation,
            connected_at: self.connected_at,
        }
    }
}

impl fmt::Debug for StoredManagedConnection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StoredManagedConnection")
            .field("provider_id", &self.provider_id)
            .field("setup_values", &self.setup_values)
            .field("setup_fingerprint", &self.setup_fingerprint)
            .field("auth_method", &self.auth_method)
            .field(
                "credential_fields",
                &self.auth_values.0.keys().collect::<Vec<_>>(),
            )
            .field("connection_generation", &self.connection_generation)
            .field("policy", &self.policy)
            .field("connected_at", &self.connected_at)
            .finish()
    }
}

/// Secret-free durable connection projection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DurableConnectionDescriptor {
    pub provider_id: ProviderId,
    pub setup_values: BTreeMap<SetupFieldId, SafeSetupValue>,
    pub setup_fingerprint: Sha256Digest,
    pub recipe_fingerprint: Sha256Digest,
    pub auth_method: AuthMethodId,
    pub credential_fields: Vec<AuthFieldName>,
    pub connection_generation: ProviderConnectionGeneration,
    pub connected_at: Timestamp,
}

/// Safe receipt allocated in the proposal before candidate compilation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DurableProviderReceipt {
    pub receipt_id: Uuid,
    pub store_revision: ProviderStoreRevision,
    pub provider_state_revision: ProviderStateRevision,
    pub committed_at: Timestamp,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ConnectReceipt {
    pub durable_receipt: DurableProviderReceipt,
    pub durable_connection: DurableConnectionDescriptor,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DisconnectReceipt {
    pub durable_receipt: DurableProviderReceipt,
    pub provider_id: ProviderId,
    pub disconnected: bool,
}

/// Revisions expected by the runtime that requested a provider-store transaction.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderStoreExpectation {
    pub generation: ProviderStoreGeneration,
    pub store_revision: ProviderStoreRevision,
    pub provider_state_revision: ProviderStateRevision,
}

/// Fully normalized connect mutation supplied by the recipe compiler/manager.
#[derive(Clone, Eq, PartialEq)]
pub struct ConnectMutation {
    pub client_connect_id: ClientConnectId,
    pub provider_id: ProviderId,
    pub expected_catalog_revision: CatalogRevision,
    pub expectation: ProviderStoreExpectation,
    pub setup_values: BTreeMap<SetupFieldId, SafeSetupValue>,
    pub auth_method: AuthMethodId,
    pub auth_values: ProviderAuthValues,
    pub policy: StoredProviderPolicyProjection,
}

impl fmt::Debug for ConnectMutation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ConnectMutation")
            .field("client_connect_id", &self.client_connect_id)
            .field("provider_id", &self.provider_id)
            .field("expected_catalog_revision", &self.expected_catalog_revision)
            .field("expectation", &self.expectation)
            .field("setup_values", &self.setup_values)
            .field("auth_method", &self.auth_method)
            .field("auth_values", &self.auth_values)
            .field("policy", &self.policy)
            .finish()
    }
}

/// Complete disconnect mutation. Runtime revision validation is supplied by P6.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DisconnectMutation {
    pub client_request_id: ClientRequestId,
    pub provider_id: ProviderId,
    pub expected_runtime_revision: cookie_agent_identity::RuntimeRevision,
    pub expected_provider_state_revision: ProviderStateRevision,
    pub expected_store_generation: ProviderStoreGeneration,
    pub expected_store_revision: ProviderStoreRevision,
    pub expected_connection_generation: Option<ProviderConnectionGeneration>,
}

/// Safe mutation receipt returned by a proposal, replay, or commit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProviderStoreMutation {
    Connect {
        durable_receipt: DurableProviderReceipt,
        durable_connection: DurableConnectionDescriptor,
    },
    Disconnect {
        durable_receipt: DurableProviderReceipt,
        provider_id: ProviderId,
        disconnected: bool,
    },
}

impl ProviderStoreMutation {
    #[must_use]
    pub fn durable_receipt(&self) -> &DurableProviderReceipt {
        match self {
            Self::Connect {
                durable_receipt, ..
            }
            | Self::Disconnect {
                durable_receipt, ..
            } => durable_receipt,
        }
    }
}

/// Immutable provider-store state used for complete candidate compilation.
#[derive(Clone)]
pub struct ProviderStoreSnapshot {
    pub(crate) generation: ProviderStoreGeneration,
    pub(crate) store_revision: ProviderStoreRevision,
    pub(crate) providers: BTreeMap<ProviderId, StoredManagedConnection>,
    pub(crate) connect_receipts: BTreeMap<ClientConnectId, ConnectReceipt>,
    pub(crate) disconnect_receipts: BTreeMap<ClientRequestId, DisconnectReceipt>,
}

impl ProviderStoreSnapshot {
    #[must_use]
    pub const fn generation(&self) -> ProviderStoreGeneration {
        self.generation
    }

    #[must_use]
    pub fn store_revision(&self) -> &ProviderStoreRevision {
        &self.store_revision
    }

    #[must_use]
    pub fn provider_state_revision(&self) -> ProviderStateRevision {
        ProviderStateRevision::new(self.store_revision.as_str().to_owned())
            .expect("provider store revisions are provider-state revisions")
    }

    #[must_use]
    pub fn expectation(&self) -> ProviderStoreExpectation {
        ProviderStoreExpectation {
            generation: self.generation,
            store_revision: self.store_revision.clone(),
            provider_state_revision: self.provider_state_revision(),
        }
    }

    #[must_use]
    pub fn providers(&self) -> &BTreeMap<ProviderId, StoredManagedConnection> {
        &self.providers
    }

    #[must_use]
    pub fn provider(&self, id: &ProviderId) -> Option<&StoredManagedConnection> {
        self.providers.get(id)
    }

    #[must_use]
    pub fn connect_receipt(&self, id: &ClientConnectId) -> Option<ProviderStoreMutation> {
        self.connect_receipts
            .get(id)
            .map(|receipt| ProviderStoreMutation::Connect {
                durable_receipt: receipt.durable_receipt.clone(),
                durable_connection: receipt.durable_connection.clone(),
            })
    }

    #[must_use]
    pub fn disconnect_receipt(&self, id: &ClientRequestId) -> Option<ProviderStoreMutation> {
        self.disconnect_receipts
            .get(id)
            .map(|receipt| ProviderStoreMutation::Disconnect {
                durable_receipt: receipt.durable_receipt.clone(),
                provider_id: receipt.provider_id.clone(),
                disconnected: receipt.disconnected,
            })
    }
}

impl fmt::Debug for ProviderStoreSnapshot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderStoreSnapshot")
            .field("generation", &self.generation)
            .field("store_revision", &self.store_revision)
            .field("providers", &self.providers.keys().collect::<Vec<_>>())
            .field("connect_receipt_count", &self.connect_receipts.len())
            .field("disconnect_receipt_count", &self.disconnect_receipts.len())
            .finish()
    }
}

/// Provider-store failures contain no request bodies, credential values, or content excerpts.
#[derive(Debug, Error)]
pub enum ProviderStoreError {
    #[error("provider store access failed")]
    Storage(#[source] SecureStoreError),
    #[error("provider store schema is invalid")]
    InvalidStore,
    #[error("unversioned provider store is rejected")]
    UnversionedStore,
    #[error("earlier provider store schema is rejected")]
    LegacyStoreVersion,
    #[error("unsupported provider store schema")]
    UnsupportedStoreVersion,
    #[error("provider store request is invalid")]
    InvalidRequest,
    #[error("catalog revision conflict")]
    CatalogRevisionConflict,
    #[error("runtime revision conflict")]
    RuntimeRevisionConflict,
    #[error("provider store generation conflict")]
    StoreGenerationConflict,
    #[error("provider store revision conflict")]
    StoreRevisionConflict,
    #[error("provider state revision conflict")]
    ProviderStateRevisionConflict,
    #[error("stale provider connection generation")]
    StaleConnectionGeneration,
    #[error("provider request id conflicts with an earlier payload")]
    IdempotencyConflict,
    #[error("provider store generation is exhausted")]
    GenerationExhausted,
    #[error("provider store proposal does not belong to this transaction")]
    ProposalMismatch,
    #[error("provider store encoding failed")]
    Encoding,
}

impl From<SecureStoreError> for ProviderStoreError {
    fn from(error: SecureStoreError) -> Self {
        Self::Storage(error)
    }
}

pub(crate) fn validate_setup_values(
    values: &BTreeMap<SetupFieldId, SafeSetupValue>,
) -> Result<(), ProviderStoreError> {
    if values.len() > MAX_SETUP_FIELDS {
        return Err(ProviderStoreError::InvalidRequest);
    }
    for value in values.values() {
        match value {
            SafeSetupValue::String(value)
                if value.as_str().is_empty()
                    || value.as_str().len() > 2048
                    || value.as_str().contains("${env:") =>
            {
                return Err(ProviderStoreError::InvalidRequest);
            }
            SafeSetupValue::Integer(value)
                if !(-9_007_199_254_740_991..=9_007_199_254_740_991).contains(value) =>
            {
                return Err(ProviderStoreError::InvalidRequest);
            }
            _ => {}
        }
    }
    Ok(())
}
