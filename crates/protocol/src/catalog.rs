use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use ts_rs::TS;

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum CatalogUnsupportedReason {
    NoKnownProtocolFamily,
    UnsupportedModelShape,
    UnsupportedModelCapabilities,
    InvalidCatalogProviderRecord,
    RemovedWithoutRetainedRecipeMatch,
}
