use std::collections::BTreeMap;

use cookie_agent_identity::{CanonicalModelId, CatalogRevision, ProviderId, ProviderModelId};
use jiff::Timestamp;
use serde::{Deserialize, Serialize};

/// Exact selected source for one dynamic catalog snapshot.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CatalogSource {
    Network,
    Cache,
    Bootstrap,
}

/// Durable catalog availability projected to runtime/TUI state.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CatalogAvailability {
    Ready,
    Stale,
    Bootstrap,
}

/// Non-expiring age warning for a selected validated body.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CatalogAgeState {
    Current,
    OlderThanSevenDays,
    OlderThanThirtyDays,
}

/// Complete durable catalog state; age never makes a cache unusable.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogRuntimeState {
    pub availability: CatalogAvailability,
    pub age: CatalogAgeState,
    pub last_error: Option<CatalogSafeErrorMeta>,
}

/// Bounded, body-free error metadata safe for persistence and display.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogSafeErrorMeta {
    pub code: String,
    pub safe_message: String,
    pub occurred_at: Timestamp,
}

/// Structural quarantine reason for independently recoverable catalog records.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CatalogQuarantineReason {
    InvalidCatalogProviderRecord,
    InvalidCatalogModelRecord,
    InvalidCanonicalModelRecord,
    DuplicateProviderId,
    AmbiguousProviderId,
    DuplicateProviderModelId,
    AmbiguousProviderModelId,
    DuplicateCanonicalModelId,
    AmbiguousCanonicalModelId,
    ProviderIdentityMismatch,
    ProviderModelIdentityMismatch,
    CanonicalModelIdentityMismatch,
}

impl CatalogQuarantineReason {
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::InvalidCatalogProviderRecord => "invalid_catalog_provider_record",
            Self::InvalidCatalogModelRecord => "invalid_catalog_model_record",
            Self::InvalidCanonicalModelRecord => "invalid_canonical_model_record",
            Self::DuplicateProviderId => "duplicate_provider_id",
            Self::AmbiguousProviderId => "ambiguous_provider_id",
            Self::DuplicateProviderModelId => "duplicate_provider_model_id",
            Self::AmbiguousProviderModelId => "ambiguous_provider_model_id",
            Self::DuplicateCanonicalModelId => "duplicate_canonical_model_id",
            Self::AmbiguousCanonicalModelId => "ambiguous_canonical_model_id",
            Self::ProviderIdentityMismatch => "provider_identity_mismatch",
            Self::ProviderModelIdentityMismatch => "provider_model_identity_mismatch",
            Self::CanonicalModelIdentityMismatch => "canonical_model_identity_mismatch",
        }
    }
}

/// One sorted safe quarantine diagnostic.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogQuarantineEntry {
    pub provider_id: Option<String>,
    pub model_id: Option<String>,
    pub canonical_model_id: Option<String>,
    pub reason: CatalogQuarantineReason,
}

/// A provider row that remains visible even when its local record quarantines.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogProviderEntry {
    pub id: ProviderId,
    pub record: Option<CatalogProviderRecord>,
    pub quarantine: Option<CatalogQuarantineReason>,
}

/// Strict provider-scoped catalog metadata.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogProviderRecord {
    pub id: ProviderId,
    pub name: String,
    pub environment: Vec<String>,
    pub npm: String,
    pub api: Option<String>,
    pub shape: Option<String>,
    pub documentation_url: String,
    pub models: BTreeMap<ProviderModelId, CatalogModelEntry>,
}

/// A provider-model row with executable metadata isolated from canonical metadata.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogModelEntry {
    pub id: ProviderModelId,
    pub record: Option<CatalogModelRecord>,
    pub quarantine: Option<CatalogQuarantineReason>,
}

/// Provider-scoped model metadata compiled by the family registry.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogModelRecord {
    pub id: ProviderModelId,
    pub name: String,
    pub description: String,
    pub family: Option<String>,
    pub attachment: bool,
    pub reasoning: bool,
    pub tool_call: bool,
    pub structured_output: Option<bool>,
    pub temperature: Option<bool>,
    pub open_weights: bool,
    pub status: CatalogModelStatus,
    pub release_date: String,
    pub last_updated: String,
    pub modalities: CatalogModalities,
    pub limits: CatalogLimits,
    pub shape: Option<String>,
    pub provider: Option<CatalogModelProviderMetadata>,
    pub reasoning_options: Vec<CatalogReasoningOption>,
    pub interleaved: Option<CatalogInterleaved>,
    pub canonical_provenance: Option<CanonicalModelProvenance>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CatalogInterleaved {
    Default,
    ReasoningContent,
    Reasoning,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CatalogModelStatus {
    Stable,
    Alpha,
    Beta,
    Deprecated,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogModalities {
    pub input: Vec<String>,
    pub output: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogLimits {
    pub context: u64,
    pub input: Option<u64>,
    pub output: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogModelProviderMetadata {
    pub npm: Option<String>,
    pub api: Option<String>,
    pub shape: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum CatalogReasoningOption {
    Effort { values: Vec<Option<String>> },
    Toggle,
    BudgetTokens { min: Option<i64>, max: Option<i64> },
}

/// Safe metadata-only canonical record. It cannot select runtime behavior.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CanonicalModelRecord {
    pub id: CanonicalModelId,
    pub name: String,
    pub description: String,
    pub family: Option<String>,
    pub release_date: String,
    pub last_updated: String,
    pub metadata_digest: String,
}

/// Exact-key, metadata-only provenance link.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CanonicalModelProvenance {
    pub id: CanonicalModelId,
    pub metadata_digest: String,
}

/// Immutable dynamic catalog snapshot.
#[derive(Clone, Debug)]
pub struct CatalogSnapshot {
    pub revision: CatalogRevision,
    pub source: CatalogSource,
    pub state: CatalogRuntimeState,
    pub validated_at: Timestamp,
    pub last_checked_at: Timestamp,
    pub etag: Option<String>,
    pub providers: BTreeMap<ProviderId, CatalogProviderEntry>,
    pub canonical_models: BTreeMap<CanonicalModelId, CanonicalModelRecord>,
    pub quarantine: Vec<CatalogQuarantineEntry>,
}

impl CatalogSnapshot {
    #[must_use]
    pub fn provider(&self, id: &ProviderId) -> Option<&CatalogProviderEntry> {
        self.providers.get(id)
    }

    #[must_use]
    pub fn model(
        &self,
        provider: &ProviderId,
        model: &ProviderModelId,
    ) -> Option<&CatalogModelEntry> {
        self.providers
            .get(provider)?
            .record
            .as_ref()?
            .models
            .get(model)
    }
}
