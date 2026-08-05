use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::catalog::{
    CatalogClaim as RawCatalogClaim, CatalogModelProviderClaims, CatalogModelRecord,
    CatalogProviderRecord,
};

/// Presence-sensitive catalog string claim.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ClaimPresence {
    PresentExact(&'static str),
    PresentOneOf(&'static [&'static str]),
    Absent,
}

impl ClaimPresence {
    #[must_use]
    pub fn matches(self, actual: Option<&str>) -> bool {
        match (self, actual) {
            (Self::PresentExact(expected), Some(actual)) => actual == expected,
            (Self::PresentOneOf(expected), Some(actual)) => expected.contains(&actual),
            (Self::Absent, None) => true,
            _ => false,
        }
    }

    #[must_use]
    pub fn matches_catalog(self, actual: &RawCatalogClaim<&str>) -> bool {
        match actual {
            RawCatalogClaim::Absent => self.matches(None),
            RawCatalogClaim::Present(value) => self.matches(Some(value)),
        }
    }
}

/// Exact top-level provider claims retained by one recipe entry.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct CatalogClaim {
    pub npm: ClaimPresence,
    pub api: ClaimPresence,
    pub environment: &'static [&'static str],
    pub shape: ClaimPresence,
}

/// Presence-complete provider claim input, including fields omitted by the
/// structurally parsed catalog projection.
#[derive(Clone, Debug)]
pub struct CatalogProviderClaimInput<'a> {
    pub id: &'a str,
    pub npm: RawCatalogClaim<&'a str>,
    pub api: RawCatalogClaim<&'a str>,
    pub environment: RawCatalogClaim<&'a [String]>,
    pub shape: RawCatalogClaim<&'a str>,
}

impl<'a> CatalogProviderClaimInput<'a> {
    #[must_use]
    pub fn from_record(record: &'a CatalogProviderRecord) -> Self {
        Self {
            id: record.id.as_str(),
            npm: string_claim(&record.claims.npm),
            api: string_claim(&record.claims.api),
            environment: match &record.claims.environment {
                RawCatalogClaim::Absent => RawCatalogClaim::Absent,
                RawCatalogClaim::Present(value) => RawCatalogClaim::Present(value.as_slice()),
            },
            shape: string_claim(&record.claims.shape),
        }
    }
}

fn string_claim(value: &RawCatalogClaim<String>) -> RawCatalogClaim<&str> {
    match value {
        RawCatalogClaim::Absent => RawCatalogClaim::Absent,
        RawCatalogClaim::Present(value) => RawCatalogClaim::Present(value.as_str()),
    }
}

/// Presence-complete model claim input.
#[derive(Clone, Debug)]
pub struct CatalogModelClaimInput<'a> {
    pub table_key: &'a str,
    pub record: &'a CatalogModelRecord,
}

impl<'a> CatalogModelClaimInput<'a> {
    #[must_use]
    pub const fn from_record(table_key: &'a str, record: &'a CatalogModelRecord) -> Self {
        Self { table_key, record }
    }
}

/// Exact Registry-1 quarantine and unsupported reason vocabulary.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RecipeQuarantineReason {
    CatalogProviderNpmDrift,
    CatalogProviderApiDrift,
    CatalogProviderEnvDrift,
    CatalogProviderShapeDrift,
    CatalogModelProviderNpmDrift,
    CatalogModelProviderApiDrift,
    CatalogModelProviderShapeDrift,
    CatalogModelShapeDrift,
    UnreviewedOpenaiModelFamily,
    AmbiguousOpenaiModelFamily,
    UnsupportedModelCapabilities,
    UnsupportedProtocolFeature,
    UnsupportedVertexModelFamily,
    NoReviewedProviderRecipe,
    RemovedWithoutRetainedRecipeMatch,
}

impl RecipeQuarantineReason {
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::CatalogProviderNpmDrift => "catalog_provider_npm_drift",
            Self::CatalogProviderApiDrift => "catalog_provider_api_drift",
            Self::CatalogProviderEnvDrift => "catalog_provider_env_drift",
            Self::CatalogProviderShapeDrift => "catalog_provider_shape_drift",
            Self::CatalogModelProviderNpmDrift => "catalog_model_provider_npm_drift",
            Self::CatalogModelProviderApiDrift => "catalog_model_provider_api_drift",
            Self::CatalogModelProviderShapeDrift => "catalog_model_provider_shape_drift",
            Self::CatalogModelShapeDrift => "catalog_model_shape_drift",
            Self::UnreviewedOpenaiModelFamily => "unreviewed_openai_model_family",
            Self::AmbiguousOpenaiModelFamily => "ambiguous_openai_model_family",
            Self::UnsupportedModelCapabilities => "unsupported_model_capabilities",
            Self::UnsupportedProtocolFeature => "unsupported_protocol_feature",
            Self::UnsupportedVertexModelFamily => "unsupported_vertex_model_family",
            Self::NoReviewedProviderRecipe => "no_reviewed_provider_recipe",
            Self::RemovedWithoutRetainedRecipeMatch => "removed_without_retained_recipe_match",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProviderRecipeMatch<'a> {
    Supported(&'a crate::recipes::ProviderRecipe),
    Quarantined(RecipeQuarantineReason),
    Unsupported(RecipeQuarantineReason),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ModelRecipeMatch<'a> {
    Supported(&'a crate::recipes::ProviderRecipe),
    Quarantined(RecipeQuarantineReason),
    Omitted,
}

pub(crate) fn provider_claim_drift(
    expected_id: &str,
    claim: CatalogClaim,
    input: &CatalogProviderClaimInput<'_>,
) -> Option<RecipeQuarantineReason> {
    if input.id != expected_id || !claim.npm.matches_catalog(&input.npm) {
        return Some(RecipeQuarantineReason::CatalogProviderNpmDrift);
    }
    if !claim.api.matches_catalog(&input.api) {
        return Some(RecipeQuarantineReason::CatalogProviderApiDrift);
    }
    let RawCatalogClaim::Present(environment) = &input.environment else {
        return Some(RecipeQuarantineReason::CatalogProviderEnvDrift);
    };
    let actual = environment
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    let expected = claim.environment.iter().copied().collect::<BTreeSet<_>>();
    if actual.len() != environment.len() || actual != expected {
        return Some(RecipeQuarantineReason::CatalogProviderEnvDrift);
    }
    if !claim.shape.matches_catalog(&input.shape) {
        return Some(RecipeQuarantineReason::CatalogProviderShapeDrift);
    }
    None
}

pub(crate) fn absent_model_provider_drift(
    provider: Option<&CatalogModelProviderClaims>,
) -> Option<RecipeQuarantineReason> {
    let provider = provider?;
    if provider.npm.is_some() {
        Some(RecipeQuarantineReason::CatalogModelProviderNpmDrift)
    } else if provider.api.is_some() {
        Some(RecipeQuarantineReason::CatalogModelProviderApiDrift)
    } else {
        Some(RecipeQuarantineReason::CatalogModelProviderShapeDrift)
    }
}

pub(crate) fn exact_model_provider_drift(
    actual: &CatalogModelProviderClaims,
    npm: ClaimPresence,
    api: ClaimPresence,
    shape: ClaimPresence,
) -> Option<RecipeQuarantineReason> {
    if !npm.matches(actual.npm.as_deref()) {
        Some(RecipeQuarantineReason::CatalogModelProviderNpmDrift)
    } else if !api.matches(actual.api.as_deref()) {
        Some(RecipeQuarantineReason::CatalogModelProviderApiDrift)
    } else if !shape.matches(actual.shape.as_deref()) {
        Some(RecipeQuarantineReason::CatalogModelProviderShapeDrift)
    } else {
        None
    }
}
