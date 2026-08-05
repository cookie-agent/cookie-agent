use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::*;

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum CatalogSource {
    Network,
    Cache,
    Bootstrap,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct CatalogSafeErrorMeta {
    pub code: SafeCode,
    pub message: SafeErrorMessage,
    pub time: jiff::Timestamp,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct CatalogRuntimeState {
    pub stale: bool,
    pub provider_quarantine_count: u32,
    pub model_quarantine_count: u32,
    pub quarantine_digest: Sha256Digest,
    #[serde(deserialize_with = "crate::deserialize_required_option")]
    #[schemars(with = "crate::NullableSchema<CatalogSafeErrorMeta>", required)]
    pub last_error: Option<CatalogSafeErrorMeta>,
}

#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct RuntimeSnapshotV1 {
    pub snapshot_schema_version: RuntimeSnapshotSchemaVersion,
    #[ts(type = "RecipeRegistryRevision")]
    pub recipe_registry_revision: RecipeRegistryRevision,
    #[ts(type = "CatalogRevision")]
    pub catalog_revision: CatalogRevision,
    pub catalog_source: CatalogSource,
    pub catalog_state: CatalogRuntimeState,
    #[ts(type = "ProviderStateRevision")]
    pub provider_state_revision: ProviderStateRevision,
    pub provider_store_generation: ProviderStoreGeneration,
    #[ts(type = "ModelRevision")]
    pub model_revision: ModelRevision,
    #[ts(type = "AgentRevision")]
    pub agent_revision: AgentRevision,
    #[ts(type = "RuntimeRevision")]
    pub runtime_revision: RuntimeRevision,
    #[schemars(length(max = 4096))]
    pub providers: Vec<ProviderDescriptor>,
    #[schemars(length(max = 4096))]
    pub models: Vec<AvailableModelDescriptor>,
    #[schemars(length(max = 4096))]
    pub agents: Vec<AgentDescriptor>,
}

impl RuntimeSnapshotV1 {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.catalog_source == CatalogSource::Bootstrap && !self.catalog_state.stale {
            return Err("bootstrap catalog state must be stale");
        }
        if self.catalog_source == CatalogSource::Network && self.catalog_state.stale {
            return Err("network catalog state cannot be stale");
        }
        if self.providers.len() > 4096
            || self
                .providers
                .windows(2)
                .any(|pair| pair[0].id >= pair[1].id)
        {
            return Err("providers must be strictly sorted and unique with at most 4096 entries");
        }
        if self.models.len() > 4096
            || self
                .models
                .windows(2)
                .any(|pair| pair[0].key >= pair[1].key)
        {
            return Err("models must be strictly sorted and unique with at most 4096 entries");
        }
        if self.agents.len() > 4096 || self.agents.windows(2).any(|pair| pair[0].id >= pair[1].id) {
            return Err("agents must be strictly sorted and unique with at most 4096 entries");
        }
        Ok(())
    }
}

impl<'de> Deserialize<'de> for RuntimeSnapshotV1 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            snapshot_schema_version: RuntimeSnapshotSchemaVersion,
            recipe_registry_revision: RecipeRegistryRevision,
            catalog_revision: CatalogRevision,
            catalog_source: CatalogSource,
            catalog_state: CatalogRuntimeState,
            provider_state_revision: ProviderStateRevision,
            provider_store_generation: ProviderStoreGeneration,
            model_revision: ModelRevision,
            agent_revision: AgentRevision,
            runtime_revision: RuntimeRevision,
            providers: Vec<ProviderDescriptor>,
            models: Vec<AvailableModelDescriptor>,
            agents: Vec<AgentDescriptor>,
        }
        let wire = Wire::deserialize(deserializer)?;
        let value = Self {
            snapshot_schema_version: wire.snapshot_schema_version,
            recipe_registry_revision: wire.recipe_registry_revision,
            catalog_revision: wire.catalog_revision,
            catalog_source: wire.catalog_source,
            catalog_state: wire.catalog_state,
            provider_state_revision: wire.provider_state_revision,
            provider_store_generation: wire.provider_store_generation,
            model_revision: wire.model_revision,
            agent_revision: wire.agent_revision,
            runtime_revision: wire.runtime_revision,
            providers: wire.providers,
            models: wire.models,
            agents: wire.agents,
        };
        value.validate().map_err(serde::de::Error::custom)?;
        Ok(value)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct RuntimeSnapshotResult {
    pub snapshot: RuntimeSnapshotV1,
}

#[derive(
    Clone, Copy, Debug, Deserialize, Eq, Hash, JsonSchema, Ord, PartialEq, PartialOrd, Serialize, TS,
)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeChangeReason {
    Startup,
    CatalogRefreshed,
    CatalogFallback,
    ConfigReloaded,
    ProviderConnected,
    ProviderDisconnected,
    ProviderStoreChanged,
    ProviderStoreReloaded,
    AgentReloaded,
}

#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct RuntimeChangedNotification {
    #[serde(deserialize_with = "crate::deserialize_required_option")]
    #[schemars(with = "crate::NullableSchema<RuntimeRevision>", required)]
    #[ts(type = "RuntimeRevision | null")]
    pub previous_revision: Option<RuntimeRevision>,
    pub snapshot: RuntimeSnapshotV1,
    #[schemars(length(min = 1, max = 9))]
    pub reasons: Vec<RuntimeChangeReason>,
}
impl<'de> Deserialize<'de> for RuntimeChangedNotification {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            #[serde(deserialize_with = "crate::deserialize_required_option")]
            previous_revision: Option<RuntimeRevision>,
            snapshot: RuntimeSnapshotV1,
            reasons: Vec<RuntimeChangeReason>,
        }
        let wire = Wire::deserialize(deserializer)?;
        if wire.reasons.is_empty()
            || wire.reasons.len() > 9
            || wire.reasons.windows(2).any(|pair| pair[0] >= pair[1])
        {
            return Err(serde::de::Error::custom(
                "runtime change reasons must be a nonempty strictly sorted unique set",
            ));
        }
        Ok(Self {
            previous_revision: wire.previous_revision,
            snapshot: wire.snapshot,
            reasons: wire.reasons,
        })
    }
}

pub const RUNTIME_SNAPSHOT_GET_METHOD: &str = "runtime.snapshot.get";
pub const RUNTIME_CHANGED_METHOD: &str = "runtime.changed";
pub const PROVIDER_CONNECT_METHOD: &str = "provider.connect";
pub const PROVIDER_DISCONNECT_METHOD: &str = "provider.disconnect";

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct RuntimeSnapshotGetParams {}
