//! Fixed models.dev acquisition, cache schema 2, bootstrap, and structural validation.

mod bootstrap;
mod cache;
mod manager;
mod parser;
mod records;
mod transport;

pub use bootstrap::{
    MODELS_DEV_BOOTSTRAP, MODELS_DEV_BOOTSTRAP_BYTES, MODELS_DEV_BOOTSTRAP_COMMIT,
    MODELS_DEV_BOOTSTRAP_SHA256, MODELS_DEV_BOOTSTRAP_SOURCE,
};
pub use cache::{
    CATALOG_BODY_FILE, CATALOG_CACHE_SCHEMA_VERSION, CATALOG_LOCK_FILE, CATALOG_META_FILE,
    CatalogCacheMeta,
};
pub use manager::{CatalogError, CatalogManager};
pub use records::{
    CanonicalModelProvenance, CanonicalModelRecord, CatalogAgeState, CatalogAvailability,
    CatalogInterleaved, CatalogLimits, CatalogModalities, CatalogModelCost, CatalogModelCostRates,
    CatalogModelCostTier, CatalogModelEntry, CatalogModelProviderMetadata, CatalogModelRecord,
    CatalogModelStatus, CatalogProviderEntry, CatalogProviderRecord, CatalogQuarantineEntry,
    CatalogQuarantineReason, CatalogReasoningOption, CatalogRuntimeState, CatalogSafeErrorMeta,
    CatalogSnapshot, CatalogSource, PicoUsdPerMillion,
};
pub use transport::{
    CatalogBodyStream, CatalogRequest, CatalogTransport, CatalogTransportError,
    CatalogTransportFuture, CatalogTransportResponse, HttpCatalogTransport, MODELS_DEV_USER_AGENT,
};

pub const MODELS_DEV_CATALOG_URL: &str = "https://models.dev/catalog.json";
pub const CATALOG_MAX_BYTES: usize = 16 * 1024 * 1024;

pub(crate) use bootstrap::validated_bootstrap;
pub(crate) use parser::{ParsedCatalog, parse_cache_meta, parse_catalog};
