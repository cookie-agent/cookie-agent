use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use ts_rs::TS;

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum CatalogQuarantineReason {
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
    InvalidCatalogProviderRecord,
    NoReviewedProviderRecipe,
    RemovedWithoutRetainedRecipeMatch,
}
