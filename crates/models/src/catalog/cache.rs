use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use super::{CatalogSafeErrorMeta, CatalogSource};

pub const CATALOG_CACHE_SCHEMA_VERSION: u32 = 2;
pub const CATALOG_BODY_FILE: &str = "models-dev-v2.json";
pub const CATALOG_META_FILE: &str = "models-dev-v2.meta.json";
pub const CATALOG_LOCK_FILE: &str = "models-dev-v2.lock";

/// Exact cache metadata for the current schema.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogCacheMeta {
    pub schema_version: u32,
    pub url: String,
    pub body_revision: String,
    pub etag: Option<String>,
    pub byte_length: u64,
    pub validated_at: Timestamp,
    pub last_checked_at: Timestamp,
    pub selected_source: CatalogSource,
    pub stale: bool,
    pub last_error: Option<CatalogSafeErrorMeta>,
}
