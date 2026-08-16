use std::collections::BTreeMap;

use cookie_agent_identity::{CanonicalModelId, CatalogRevision, ProviderId, ProviderModelId};
use jiff::Timestamp;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

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
    // Catalog prices are runtime metadata; frozen manifest inputs prohibit JSON floats.
    #[serde(default, skip_serializing)]
    pub cost: Option<CatalogModelCost>,
    pub interleaved: Option<CatalogInterleaved>,
    pub canonical_provenance: Option<CanonicalModelProvenance>,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct PicoUsdPerMillion(u128);

impl PicoUsdPerMillion {
    const SCALE: i32 = 12;

    #[must_use]
    pub const fn new(value: u128) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn value(self) -> u128 {
        self.0
    }

    fn decimal_string(self) -> String {
        let whole = self.0 / 1_000_000_000_000;
        let fraction = self.0 % 1_000_000_000_000;
        if fraction == 0 {
            whole.to_string()
        } else {
            let fraction = format!("{fraction:012}");
            format!("{whole}.{}", fraction.trim_end_matches('0'))
        }
    }

    pub fn from_decimal_str(value: &str) -> Option<Self> {
        let value = value.trim();
        if value.is_empty() || value.starts_with('-') {
            return None;
        }
        let value = value.strip_prefix('+').unwrap_or(value);
        let (coefficient, exponent) =
            value
                .split_once(['e', 'E'])
                .map_or((value, 0), |(coefficient, exponent)| {
                    exponent
                        .parse::<i32>()
                        .map(|exponent| (coefficient, exponent))
                        .unwrap_or(("", 0))
                });
        if coefficient.is_empty() {
            return None;
        }
        let (whole, fraction) = coefficient
            .split_once('.')
            .map_or((coefficient, ""), |parts| parts);
        if (whole.is_empty() && fraction.is_empty())
            || !whole.bytes().all(|byte| byte.is_ascii_digit())
            || !fraction.bytes().all(|byte| byte.is_ascii_digit())
        {
            return None;
        }
        let digits = format!("{whole}{fraction}");
        let mut significant = digits.trim_start_matches('0');
        if significant.is_empty() {
            return Some(Self(0));
        }
        let fraction_digits = i32::try_from(fraction.len()).ok()?;
        let mut shift = Self::SCALE
            .checked_add(exponent)?
            .checked_sub(fraction_digits)?;
        if shift < 0 {
            let excess = usize::try_from(shift.unsigned_abs()).ok()?;
            let retained = significant.len().checked_sub(excess)?;
            if retained == 0 || !significant[retained..].bytes().all(|byte| byte == b'0') {
                return None;
            }
            significant = &significant[..retained];
            shift = 0;
        }
        let coefficient = significant.parse::<u128>().ok()?;
        let scaled = coefficient.checked_mul(checked_power_of_ten(u32::try_from(shift).ok()?)?)?;
        (scaled > 0).then_some(Self(scaled))
    }
}

fn checked_power_of_ten(exponent: u32) -> Option<u128> {
    (0..exponent).try_fold(1_u128, |value, _| value.checked_mul(10))
}

impl<'de> Deserialize<'de> for PicoUsdPerMillion {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::from_decimal_str(&value)
            .ok_or_else(|| serde::de::Error::custom("invalid pico-USD pricing rate"))
    }
}

impl Serialize for PicoUsdPerMillion {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.decimal_string())
    }
}

#[cfg(test)]
mod pricing_tests {
    use super::PicoUsdPerMillion;

    #[test]
    fn exact_decimal_rates_accept_supported_forms_and_reject_loss() {
        assert_eq!(
            PicoUsdPerMillion::from_decimal_str("0.125")
                .unwrap()
                .value(),
            125_000_000_000
        );
        assert_eq!(
            PicoUsdPerMillion::from_decimal_str("1.250000000000")
                .unwrap()
                .value(),
            1_250_000_000_000
        );
        assert!(PicoUsdPerMillion::from_decimal_str("0.0000000000015").is_none());
        assert!(
            PicoUsdPerMillion::from_decimal_str("340282366920938463463374607431768211455")
                .is_none()
        );
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogModelCost {
    pub input: PicoUsdPerMillion,
    pub output: PicoUsdPerMillion,
    pub reasoning: Option<PicoUsdPerMillion>,
    pub cache_read: Option<PicoUsdPerMillion>,
    pub cache_write: Option<PicoUsdPerMillion>,
    #[serde(default)]
    pub context_over_200k: Option<CatalogModelCostRates>,
    #[serde(default)]
    pub tiers: Vec<CatalogModelCostTier>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogModelCostRates {
    pub input: PicoUsdPerMillion,
    pub output: PicoUsdPerMillion,
    pub reasoning: Option<PicoUsdPerMillion>,
    pub cache_read: Option<PicoUsdPerMillion>,
    pub cache_write: Option<PicoUsdPerMillion>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogModelCostTier {
    pub context_tokens: u64,
    pub rates: CatalogModelCostRates,
}

impl CatalogModelCost {
    #[must_use]
    pub fn rates_for_input(&self, input_tokens: u64) -> CatalogModelCostRates {
        let mut selected = CatalogModelCostRates {
            input: self.input,
            output: self.output,
            reasoning: self.reasoning,
            cache_read: self.cache_read,
            cache_write: self.cache_write,
        };
        let mut selected_threshold = 0;
        if input_tokens >= 200_000
            && let Some(rates) = self.context_over_200k
        {
            selected = rates;
            selected_threshold = 200_000;
        }
        for tier in &self.tiers {
            if input_tokens < tier.context_tokens {
                break;
            }
            if tier.context_tokens >= selected_threshold {
                selected = tier.rates;
                selected_threshold = tier.context_tokens;
            }
        }
        selected
    }
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
