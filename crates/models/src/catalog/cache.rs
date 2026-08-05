use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use super::{CatalogSafeErrorMeta, CatalogSource};

pub const CATALOG_CACHE_SCHEMA_VERSION: u32 = 1;
pub const CATALOG_BODY_FILE: &str = "models-dev-v1.json";
pub const CATALOG_META_FILE: &str = "models-dev-v1.meta.json";
pub const CATALOG_LOCK_FILE: &str = "models-dev-v1.lock";

/// Exact cache metadata schema 1.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogCacheMetaV1 {
    pub schema_version: u32,
    pub url: String,
    pub body_revision: String,
    pub etag: Option<String>,
    pub byte_length: u64,
    pub validated_at: Timestamp,
    pub last_checked_at: Timestamp,
    pub selected_source: CatalogSource,
    pub stale: bool,
    pub provider_quarantine_count: u32,
    pub model_quarantine_count: u32,
    pub quarantine_digest: String,
    pub last_error: Option<CatalogSafeErrorMeta>,
}
