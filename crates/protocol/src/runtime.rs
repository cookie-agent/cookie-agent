use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::*;

/// A value this build does not know reads as `cache`.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
#[schemars(extend("x-open" = true))]
pub enum CatalogSource {
    Network,
    Bootstrap,
    #[serde(other)]
    Cache,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct CatalogSafeErrorMeta {
    pub code: SafeCode,
    pub message: SafeErrorMessage,
    pub time: jiff::Timestamp,
}

/// Age of the selected validated catalog body; age never makes it unusable.
///
/// A value this build does not know reads as `older_than_thirty_days`.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
#[schemars(extend("x-open" = true))]
pub enum CatalogAge {
    Current,
    OlderThanSevenDays,
    #[serde(other)]
    OlderThanThirtyDays,
}

/// Where the daemon's model catalog stands: its source, its age, the records
/// it quarantined, and its last refresh error.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct CatalogStatus {
    pub source: CatalogSource,
    pub stale: bool,
    pub age: CatalogAge,
    pub provider_quarantine_count: u32,
    pub model_quarantine_count: u32,
    #[serde(deserialize_with = "crate::deserialize_required_option")]
    #[schemars(with = "crate::NullableSchema<CatalogSafeErrorMeta>", required)]
    pub last_error: Option<CatalogSafeErrorMeta>,
}

/// The revision of a [`ModelCatalogView`]: one opaque string, equal for two
/// views exactly when they show the same thing. Clients compare it and send
/// it back, never parse it.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct ViewRevision(String);

impl ViewRevision {
    pub const MAX_LEN: usize = 256;

    pub fn new(value: impl Into<String>) -> Result<Self, WireStringError> {
        let value = value.into();
        if value.is_empty() || value.len() > Self::MAX_LEN || value.chars().any(char::is_control) {
            Err(WireStringError::Invalid(
                "a view revision is 1 to 256 bytes without control characters",
            ))
        } else {
            Ok(Self(value))
        }
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ViewRevision {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for ViewRevision {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

impl JsonSchema for ViewRevision {
    fn inline_schema() -> bool {
        true
    }
    fn schema_name() -> std::borrow::Cow<'static, str> {
        std::borrow::Cow::Borrowed("ViewRevision")
    }
    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": "string",
            "minLength": 1,
            "maxLength": 256,
            "description": "An opaque revision: compared and sent back, never parsed."
        })
    }
}

/// What a client sees of the daemon's providers, models, and agents. It
/// carries only client-facing descriptors: no frozen model bindings, recipe
/// identities, compiler versions, or internal revisions, which change
/// without anything a client shows changing.
#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize)]
pub struct ModelCatalogView {
    /// Changes whenever anything this view shows changes. `provider.connect`
    /// and `provider.disconnect` send back the revision they were filled
    /// from.
    pub revision: ViewRevision,
    pub catalog: CatalogStatus,
    #[schemars(length(max = 4096))]
    pub providers: Vec<ProviderDescriptor>,
    #[schemars(length(max = 4096))]
    pub models: Vec<AvailableModelDescriptor>,
    #[schemars(length(max = 4096))]
    pub agents: Vec<AgentDescriptor>,
}

impl ModelCatalogView {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.catalog.source == CatalogSource::Bootstrap && !self.catalog.stale {
            return Err("bootstrap catalog state must be stale");
        }
        if self.catalog.source == CatalogSource::Network && self.catalog.stale {
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
        if self.agents.len() > 4096
            || self
                .agents
                .windows(2)
                .any(|pair| (&pair[0].preset, &pair[0].id) >= (&pair[1].preset, &pair[1].id))
            || self.agents.iter().any(|agent| agent.validate().is_err())
        {
            return Err(
                "agents must be strictly sorted and unique by preset and ID with at most 4096 entries",
            );
        }
        Ok(())
    }
}

impl<'de> Deserialize<'de> for ModelCatalogView {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Wire {
            revision: ViewRevision,
            catalog: CatalogStatus,
            providers: Vec<ProviderDescriptor>,
            models: Vec<AvailableModelDescriptor>,
            agents: Vec<AgentDescriptor>,
        }
        let wire = Wire::deserialize(deserializer)?;
        let value = Self {
            revision: wire.revision,
            catalog: wire.catalog,
            providers: wire.providers,
            models: wire.models,
            agents: wire.agents,
        };
        value.validate().map_err(serde::de::Error::custom)?;
        Ok(value)
    }
}

/// `runtime.snapshot.get`: the daemon's current catalog view.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct RuntimeSnapshotResult {
    pub snapshot: ModelCatalogView,
}

crate::open_enum! {
    #[derive(
        Clone, Copy, Debug, Deserialize, Eq, Hash, JsonSchema, Ord, PartialEq, PartialOrd, Serialize,
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
        /// The daemon dropped notifications this connection was too slow to
        /// take; the snapshot is current and `previous_revision` is null.
        Resync,
    }
}

/// `runtime.changed`: the daemon published a new catalog view.
#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize)]
pub struct RuntimeChangedNotification {
    /// The revision of the view this one follows; null after a `resync`.
    #[serde(deserialize_with = "crate::deserialize_required_option")]
    #[schemars(with = "crate::NullableSchema<ViewRevision>", required)]
    pub previous_revision: Option<ViewRevision>,
    pub snapshot: ModelCatalogView,
    /// Why, sorted and without repeats.
    #[schemars(length(min = 1))]
    pub reasons: Vec<RuntimeChangeReason>,
}
impl<'de> Deserialize<'de> for RuntimeChangedNotification {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Wire {
            #[serde(deserialize_with = "crate::deserialize_required_option")]
            previous_revision: Option<ViewRevision>,
            snapshot: ModelCatalogView,
            reasons: Vec<RuntimeChangeReason>,
        }
        let mut wire = Wire::deserialize(deserializer)?;
        // Reasons a newer daemon added say nothing this build acts on.
        wire.reasons
            .retain(|reason| *reason != RuntimeChangeReason::Unknown);
        wire.reasons.sort();
        wire.reasons.dedup();
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
pub const PROVIDER_AUTH_BEGIN_METHOD: &str = "provider.auth.begin";
pub const PROVIDER_AUTH_COMPLETE_METHOD: &str = "provider.auth.complete";
pub const PROVIDER_AUTH_CANCEL_METHOD: &str = "provider.auth.cancel";
pub const PROVIDER_AUTH_COMPLETED_METHOD: &str = "provider.auth.completed";

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeSnapshotGetParams {}
