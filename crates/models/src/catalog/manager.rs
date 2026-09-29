use std::{
    io::Read as _,
    path::Path,
    sync::{Arc, Mutex, PoisonError},
};

use cookie_agent_identity::CatalogRevision;
use flate2::read::GzDecoder;
use futures_util::StreamExt as _;
use jiff::Timestamp;
use thiserror::Error;

use crate::secure_store::{DEFAULT_LOCK_BUDGET, SecureDirectory, SecureStoreError};

use super::{
    CATALOG_BODY_FILE, CATALOG_CACHE_SCHEMA_VERSION, CATALOG_LOCK_FILE, CATALOG_MAX_BYTES,
    CATALOG_META_FILE, CatalogAgeState, CatalogAvailability, CatalogCacheMeta, CatalogRuntimeState,
    CatalogSafeErrorMeta, CatalogSnapshot, CatalogSource, CatalogTransport, CatalogTransportError,
    CatalogTransportResponse, MODELS_DEV_BOOTSTRAP, MODELS_DEV_CATALOG_URL, ParsedCatalog,
    parse_cache_meta, parse_catalog,
};

const MAX_META_BYTES: u64 = 128 * 1024;
const MAX_ETAG_BYTES: usize = 1_024;
const MAX_SAFE_MESSAGE_BYTES: usize = 512;
const SEVEN_DAYS_SECONDS: i64 = 7 * 24 * 60 * 60;
const THIRTY_DAYS_SECONDS: i64 = 30 * 24 * 60 * 60;
/// Staging and backup files that the earlier journaled commit could leave
/// behind after a crash. Installing a body deletes any that remain.
const STALE_TRANSACTION_FILES: [&str; 4] = [
    ".models-dev-v2.json.next",
    ".models-dev-v2.meta.json.next",
    ".models-dev-v2.json.backup",
    ".models-dev-v2.meta.json.backup",
];

/// Dynamic network/cache/bootstrap catalog manager.
pub struct CatalogManager<T> {
    transport: Arc<T>,
    cache: Result<SecureDirectory, CatalogError>,
    /// The catalog this manager parsed most recently. A body is identified by
    /// its SHA-256 revision, so while the cache metadata still names this
    /// revision the multi-megabyte body need not be read or parsed again.
    loaded: Mutex<Option<Arc<ParsedCatalog>>>,
}

impl<T: CatalogTransport> CatalogManager<T> {
    /// Uses the fixed per-user catalog cache path.
    pub fn standard(transport: T) -> Self {
        Self {
            transport: Arc::new(transport),
            cache: SecureDirectory::user_data("catalog").map_err(CatalogError::from_store),
            loaded: Mutex::default(),
        }
    }

    /// Uses an explicit secure directory, primarily for deterministic tests.
    #[must_use]
    pub fn new(transport: T, cache: SecureDirectory) -> Self {
        Self {
            transport: Arc::new(transport),
            cache: Ok(cache),
            loaded: Mutex::default(),
        }
    }

    /// Opens an explicit private cache below a trusted anchor.
    pub fn in_directory(
        transport: T,
        anchor: impl AsRef<Path>,
        relative: impl AsRef<Path>,
    ) -> Self {
        Self {
            transport: Arc::new(transport),
            cache: SecureDirectory::open_in(anchor, relative).map_err(CatalogError::from_store),
            loaded: Mutex::default(),
        }
    }

    /// The catalog to start from without touching the network: the validated
    /// cache, else the bundled bootstrap copy. A background refresh then
    /// checks models.dev.
    pub fn load_cached(&self) -> Result<CatalogSnapshot, CatalogError> {
        self.load_cached_at(Timestamp::now())
    }

    /// [`Self::load_cached`] at a supplied time for deterministic tests.
    pub fn load_cached_at(&self, now: Timestamp) -> Result<CatalogSnapshot, CatalogError> {
        match self.load_cache() {
            Ok(cached) => {
                let availability = if cached.meta.stale {
                    CatalogAvailability::Stale
                } else {
                    CatalogAvailability::Ready
                };
                Ok(snapshot_from_parsed(
                    &cached.catalog,
                    CatalogSource::Cache,
                    cached.meta.validated_at,
                    cached.meta.last_checked_at,
                    cached.meta.etag,
                    availability,
                    cached.meta.last_error,
                ))
            }
            Err(_) => {
                let parsed = self.remember(parse_catalog(MODELS_DEV_BOOTSTRAP)?);
                Ok(snapshot_from_parsed(
                    &parsed,
                    CatalogSource::Bootstrap,
                    now,
                    now,
                    None,
                    CatalogAvailability::Bootstrap,
                    None,
                ))
            }
        }
    }

    /// Refreshes using the system clock.
    pub async fn refresh(&self) -> Result<CatalogSnapshot, CatalogError> {
        self.refresh_at(Timestamp::now()).await
    }

    /// Refreshes at a supplied time for deterministic tests.
    pub async fn refresh_at(&self, now: Timestamp) -> Result<CatalogSnapshot, CatalogError> {
        let cache = self.load_cache();
        let etag = cache
            .as_ref()
            .ok()
            .and_then(|cache| cache.meta.etag.clone());
        let fetched = match self.fetch(etag).await {
            Ok(fetched) => fetched,
            Err(error) => return self.select_fallback(cache, error, now),
        };
        let Some(FetchedCatalog { bytes, etag }) = fetched else {
            return self.not_modified(cache, now);
        };
        let parsed = match parse_catalog(&bytes) {
            Ok(parsed) => parsed,
            Err(error) => return self.select_fallback(cache, error, now),
        };
        let parsed = self.remember(parsed);
        let mut meta = metadata_for(
            &parsed,
            CatalogSource::Network,
            false,
            now,
            now,
            etag.clone(),
            None,
        );
        if let Err(error) = self.commit_cache(&bytes, &meta) {
            meta.last_error = Some(error.safe_meta(now));
        }
        Ok(snapshot_from_parsed(
            &parsed,
            CatalogSource::Network,
            now,
            now,
            etag,
            CatalogAvailability::Ready,
            meta.last_error,
        ))
    }

    /// A `304 Not Modified` revalidates the cached body: only its metadata is
    /// rewritten, and the catalog already parsed for it is reused.
    fn not_modified(
        &self,
        cache: Result<ValidatedCache, CatalogError>,
        now: Timestamp,
    ) -> Result<CatalogSnapshot, CatalogError> {
        let cached = match cache {
            Ok(cached) => cached,
            Err(cache_error) => {
                return self.select_fallback(
                    Err(cache_error),
                    CatalogError::new(
                        "not_modified_without_valid_cache",
                        "catalog server returned not modified without a valid cache",
                    ),
                    now,
                );
            }
        };
        let mut meta = cached.meta;
        meta.validated_at = now;
        meta.last_checked_at = now;
        meta.selected_source = CatalogSource::Network;
        meta.stale = false;
        meta.last_error = None;
        let write_error = self.commit_meta(&meta).err();
        Ok(snapshot_from_parsed(
            &cached.catalog,
            CatalogSource::Network,
            meta.validated_at,
            now,
            meta.etag.clone(),
            CatalogAvailability::Ready,
            write_error.map(|error| error.safe_meta(now)),
        ))
    }

    fn select_fallback(
        &self,
        cache: Result<ValidatedCache, CatalogError>,
        network_error: CatalogError,
        now: Timestamp,
    ) -> Result<CatalogSnapshot, CatalogError> {
        match cache {
            Ok(cached) => {
                let safe_error = network_error.safe_meta(now);
                let mut meta = metadata_for(
                    &cached.catalog,
                    CatalogSource::Cache,
                    true,
                    cached.meta.validated_at,
                    now,
                    cached.meta.etag.clone(),
                    Some(safe_error),
                );
                let write_error = self.commit_meta(&meta).err();
                if let Some(error) = write_error {
                    meta.last_error = Some(error.safe_meta(now));
                }
                Ok(snapshot_from_parsed(
                    &cached.catalog,
                    CatalogSource::Cache,
                    meta.validated_at,
                    now,
                    meta.etag,
                    CatalogAvailability::Stale,
                    meta.last_error,
                ))
            }
            Err(cache_error) => {
                let parsed = self.remember(parse_catalog(MODELS_DEV_BOOTSTRAP)?);
                let fallback = CatalogError::new(
                    "catalog_bootstrap_fallback",
                    format!(
                        "network catalog and validated cache were unavailable ({}, {})",
                        network_error.code(),
                        cache_error.code()
                    ),
                );
                let mut meta = metadata_for(
                    &parsed,
                    CatalogSource::Bootstrap,
                    true,
                    now,
                    now,
                    None,
                    Some(fallback.safe_meta(now)),
                );
                let write_error = self.commit_cache(MODELS_DEV_BOOTSTRAP, &meta).err();
                if let Some(error) = write_error {
                    meta.last_error = Some(error.safe_meta(now));
                }
                Ok(snapshot_from_parsed(
                    &parsed,
                    CatalogSource::Bootstrap,
                    now,
                    now,
                    None,
                    CatalogAvailability::Bootstrap,
                    meta.last_error,
                ))
            }
        }
    }

    /// Fetches the catalog body, decoded and within [`CATALOG_MAX_BYTES`];
    /// `None` is a `304 Not Modified` for the supplied ETag.
    async fn fetch(&self, etag: Option<String>) -> Result<Option<FetchedCatalog>, CatalogError> {
        let response = self
            .transport
            .fetch(etag)
            .await
            .map_err(CatalogError::from_transport)?;
        let Some(coding) = validate_response(&response)? else {
            return Ok(None);
        };
        let etag = response.etag.map(validate_etag).transpose()?;
        let capacity = response
            .content_length
            .and_then(|length| usize::try_from(length).ok())
            .unwrap_or(0)
            .min(CATALOG_MAX_BYTES);
        let mut body = response.body;
        let mut bytes = Vec::with_capacity(capacity);
        while let Some(chunk) = body.next().await {
            let chunk = chunk.map_err(CatalogError::from_transport)?;
            if bytes.len().saturating_add(chunk.len()) > CATALOG_MAX_BYTES {
                return Err(body_too_large());
            }
            bytes.extend_from_slice(&chunk);
        }
        let bytes = match coding {
            ContentCoding::Identity => bytes,
            ContentCoding::Gzip => decode_gzip(&bytes)?,
        };
        Ok(Some(FetchedCatalog { bytes, etag }))
    }

    fn load_cache(&self) -> Result<ValidatedCache, CatalogError> {
        let directory = self.cache.as_ref().map_err(Clone::clone)?;
        let lock = directory
            .lock_within(CATALOG_LOCK_FILE, DEFAULT_LOCK_BUDGET)
            .map_err(CatalogError::from_store)?;
        let meta_bytes = lock
            .read(CATALOG_META_FILE, MAX_META_BYTES)
            .map_err(CatalogError::from_store)?
            .ok_or_else(|| {
                CatalogError::new("catalog_cache_missing", "catalog cache metadata is missing")
            })?;
        let meta = parse_cache_meta(&meta_bytes)?;
        validate_meta_error(&meta)?;
        // The metadata names its body by SHA-256 revision and length. When
        // that is the catalog already parsed in memory (a 304 refresh, or any
        // check after startup loaded the cache), reuse it rather than reading
        // and re-parsing the multi-megabyte body. Another process that
        // installed a different body also installed metadata naming it, so a
        // mismatch falls through to the full load.
        if let Some(catalog) = self.loaded_revision(&meta) {
            validate_meta(&meta, catalog.byte_length, &catalog.revision)?;
            return Ok(ValidatedCache { catalog, meta });
        }
        let body = lock
            .read(CATALOG_BODY_FILE, CATALOG_MAX_BYTES as u64)
            .map_err(CatalogError::from_store)?
            .ok_or_else(|| {
                CatalogError::new("catalog_cache_missing", "catalog cache body is missing")
            })?;
        // Parsing computes the body's revision, the one hash of it this load
        // needs. Metadata naming a different body (a write torn between the
        // two files) rejects the pair.
        let parsed = parse_catalog(&body)?;
        validate_meta(&meta, body.len() as u64, &parsed.revision)?;
        Ok(ValidatedCache {
            catalog: self.remember(parsed),
            meta,
        })
    }

    /// The remembered catalog, if it is the body `meta` names.
    fn loaded_revision(&self, meta: &CatalogCacheMeta) -> Option<Arc<ParsedCatalog>> {
        self.loaded
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .filter(|catalog| {
                catalog.revision.as_str() == meta.body_revision
                    && catalog.byte_length == meta.byte_length
            })
            .cloned()
    }

    /// Remembers a freshly parsed catalog for later loads of the same body.
    fn remember(&self, parsed: ParsedCatalog) -> Arc<ParsedCatalog> {
        let parsed = Arc::new(parsed);
        *self.loaded.lock().unwrap_or_else(PoisonError::into_inner) = Some(Arc::clone(&parsed));
        parsed
    }

    /// Rewrites only the metadata for a body that is already installed (a 304,
    /// or serving the cache after a failed refresh), instead of rewriting the
    /// multi-megabyte body. The metadata on disk must still name the same body,
    /// so one atomic replace keeps the pair consistent.
    fn commit_meta(&self, meta: &CatalogCacheMeta) -> Result<(), CatalogError> {
        let meta_bytes = encode_meta(meta)?;
        let directory = self.cache.as_ref().map_err(Clone::clone)?;
        let lock = directory
            .lock_within(CATALOG_LOCK_FILE, DEFAULT_LOCK_BUDGET)
            .map_err(CatalogError::from_store)?;
        let installed = lock
            .read(CATALOG_META_FILE, MAX_META_BYTES)
            .map_err(CatalogError::from_store)?
            .map(|bytes| parse_cache_meta(&bytes))
            .transpose()?;
        if installed.is_none_or(|installed| {
            installed.body_revision != meta.body_revision
                || installed.byte_length != meta.byte_length
        }) {
            return Err(CatalogError::new(
                "catalog_cache_changed",
                "catalog cache body changed before its metadata could be updated",
            ));
        }
        lock.atomic_replace(CATALOG_META_FILE, &meta_bytes)
            .map_err(CatalogError::from_store)
    }

    /// Installs a body, then the metadata naming it, each by one atomic
    /// replace under the cache lock. `meta` was derived from parsing `body`.
    /// A crash between the two replaces leaves metadata naming the previous
    /// body; loading rejects that pair, so startup falls back to the bootstrap
    /// and the next refresh (sent without an ETag) installs a matching pair.
    fn commit_cache(&self, body: &[u8], meta: &CatalogCacheMeta) -> Result<(), CatalogError> {
        let meta_bytes = encode_meta(meta)?;
        let directory = self.cache.as_ref().map_err(Clone::clone)?;
        let lock = directory
            .lock_within(CATALOG_LOCK_FILE, DEFAULT_LOCK_BUDGET)
            .map_err(CatalogError::from_store)?;
        lock.atomic_replace(CATALOG_BODY_FILE, body)
            .map_err(CatalogError::from_store)?;
        lock.atomic_replace(CATALOG_META_FILE, &meta_bytes)
            .map_err(CatalogError::from_store)?;
        for name in STALE_TRANSACTION_FILES {
            // Best effort: the installed pair is already consistent.
            let _ = lock.remove(name);
        }
        Ok(())
    }
}

struct FetchedCatalog {
    bytes: Vec<u8>,
    etag: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ContentCoding {
    Identity,
    Gzip,
}

/// Checks a response's status and headers, returning the body's content
/// coding, or `None` for a `304 Not Modified`.
fn validate_response(
    response: &CatalogTransportResponse,
) -> Result<Option<ContentCoding>, CatalogError> {
    if response.status == 304 {
        return Ok(None);
    }
    if (300..400).contains(&response.status) {
        return Err(CatalogError::new(
            "catalog_redirect_rejected",
            "catalog redirects are forbidden",
        ));
    }
    if response.status != 200 {
        return Err(CatalogError::new(
            "catalog_http_status",
            "catalog server returned an unusable status",
        ));
    }
    let coding = match response.content_encoding.as_deref().map(str::trim) {
        None => ContentCoding::Identity,
        Some(coding) if coding.eq_ignore_ascii_case("identity") => ContentCoding::Identity,
        Some(coding) if coding.eq_ignore_ascii_case("gzip") => ContentCoding::Gzip,
        Some(_) => {
            return Err(CatalogError::new(
                "catalog_encoding_rejected",
                "catalog response uses an unsupported content encoding",
            ));
        }
    };
    let json_content_type = response
        .content_type
        .as_deref()
        .is_some_and(|content_type| {
            content_type.split(';').next().is_some_and(|media_type| {
                media_type.trim().eq_ignore_ascii_case("application/json")
            })
        });
    if !json_content_type {
        return Err(CatalogError::new(
            "catalog_content_type_rejected",
            "catalog response is not JSON",
        ));
    }
    if response
        .content_length
        .is_some_and(|length| length > CATALOG_MAX_BYTES as u64)
    {
        return Err(body_too_large());
    }
    Ok(Some(coding))
}

/// Decodes a gzip body, stopping as soon as the output passes the byte limit.
fn decode_gzip(compressed: &[u8]) -> Result<Vec<u8>, CatalogError> {
    let mut decoded =
        Vec::with_capacity(compressed.len().saturating_mul(10).min(CATALOG_MAX_BYTES));
    GzDecoder::new(compressed)
        .take(CATALOG_MAX_BYTES as u64 + 1)
        .read_to_end(&mut decoded)
        .map_err(|_| {
            CatalogError::new("catalog_gzip_invalid", "catalog response is not valid gzip")
        })?;
    if decoded.len() > CATALOG_MAX_BYTES {
        return Err(body_too_large());
    }
    Ok(decoded)
}

fn body_too_large() -> CatalogError {
    CatalogError::new(
        "catalog_body_too_large",
        "catalog response exceeds the byte limit",
    )
}

struct ValidatedCache {
    catalog: Arc<ParsedCatalog>,
    meta: CatalogCacheMeta,
}

fn metadata_for(
    parsed: &ParsedCatalog,
    source: CatalogSource,
    stale: bool,
    validated_at: Timestamp,
    checked_at: Timestamp,
    etag: Option<String>,
    last_error: Option<CatalogSafeErrorMeta>,
) -> CatalogCacheMeta {
    CatalogCacheMeta {
        schema_version: CATALOG_CACHE_SCHEMA_VERSION,
        url: MODELS_DEV_CATALOG_URL.to_owned(),
        body_revision: parsed.revision.as_str().to_owned(),
        etag,
        byte_length: parsed.byte_length,
        validated_at,
        last_checked_at: checked_at,
        selected_source: source,
        stale,
        last_error,
    }
}

fn encode_meta(meta: &CatalogCacheMeta) -> Result<Vec<u8>, CatalogError> {
    serde_json::to_vec_pretty(meta).map_err(|_| {
        CatalogError::new(
            "cache_metadata_write_failed",
            "catalog cache metadata could not be encoded",
        )
    })
}

/// Checks metadata against its body, given the body's length and its
/// already-computed revision.
fn validate_meta(
    meta: &CatalogCacheMeta,
    body_len: u64,
    body_revision: &CatalogRevision,
) -> Result<(), CatalogError> {
    if meta.schema_version != CATALOG_CACHE_SCHEMA_VERSION
        || meta.url != MODELS_DEV_CATALOG_URL
        || meta.byte_length != body_len
        || meta.body_revision != body_revision.as_str()
        || meta.etag.clone().map(validate_etag).transpose()?.as_deref() != meta.etag.as_deref()
    {
        return Err(CatalogError::new(
            "invalid_catalog_cache_metadata",
            "catalog cache metadata does not match its body",
        ));
    }
    Ok(())
}

fn validate_meta_error(meta: &CatalogCacheMeta) -> Result<(), CatalogError> {
    let error_valid = meta.last_error.as_ref().is_none_or(|error| {
        !error.code.is_empty()
            && error.code.len() <= 128
            && error.code.bytes().all(|byte| {
                byte.is_ascii_lowercase()
                    || byte.is_ascii_digit()
                    || matches!(byte, b'.' | b'_' | b'-')
            })
            && error.safe_message.len() <= MAX_SAFE_MESSAGE_BYTES
            && !error.safe_message.chars().any(char::is_control)
    });
    if error_valid {
        Ok(())
    } else {
        Err(CatalogError::new(
            "invalid_catalog_cache_metadata",
            "catalog cache metadata error state is invalid",
        ))
    }
}

fn snapshot_from_parsed(
    parsed: &ParsedCatalog,
    source: CatalogSource,
    validated_at: Timestamp,
    checked_at: Timestamp,
    etag: Option<String>,
    availability: CatalogAvailability,
    last_error: Option<CatalogSafeErrorMeta>,
) -> CatalogSnapshot {
    CatalogSnapshot {
        revision: parsed.revision.clone(),
        source,
        state: CatalogRuntimeState {
            availability,
            age: age_state(validated_at, checked_at),
            last_error,
        },
        validated_at,
        last_checked_at: checked_at,
        etag,
        providers: parsed.providers.clone(),
        canonical_models: parsed.canonical_models.clone(),
        quarantine: parsed.quarantine.clone(),
    }
}

fn age_state(validated_at: Timestamp, now: Timestamp) -> CatalogAgeState {
    let age = now.as_second().saturating_sub(validated_at.as_second());
    if age >= THIRTY_DAYS_SECONDS {
        CatalogAgeState::OlderThanThirtyDays
    } else if age >= SEVEN_DAYS_SECONDS {
        CatalogAgeState::OlderThanSevenDays
    } else {
        CatalogAgeState::Current
    }
}

fn validate_etag(value: String) -> Result<String, CatalogError> {
    let opaque = value
        .strip_prefix("W/\"")
        .or_else(|| value.strip_prefix('"'))
        .and_then(|value| value.strip_suffix('"'));
    if value.is_empty()
        || value.len() > MAX_ETAG_BYTES
        || value.chars().any(char::is_control)
        || value.contains(['\r', '\n'])
        || http::HeaderValue::from_bytes(value.as_bytes()).is_err()
        || opaque.is_none_or(|opaque| opaque.contains('"'))
    {
        Err(CatalogError::new(
            "invalid_catalog_etag",
            "catalog ETag is invalid",
        ))
    } else {
        Ok(value)
    }
}

/// Stable body-free catalog error.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
#[error("{safe_message}")]
pub struct CatalogError {
    code: String,
    safe_message: String,
}

impl CatalogError {
    pub(crate) fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        let mut safe_message = message.into();
        if safe_message.len() > MAX_SAFE_MESSAGE_BYTES {
            let mut end = MAX_SAFE_MESSAGE_BYTES;
            while !safe_message.is_char_boundary(end) {
                end -= 1;
            }
            safe_message.truncate(end);
        }
        safe_message.retain(|character| !character.is_control());
        Self {
            code: code.into(),
            safe_message,
        }
    }

    #[must_use]
    pub fn code(&self) -> &str {
        &self.code
    }

    #[must_use]
    pub fn safe_message(&self) -> &str {
        &self.safe_message
    }

    #[must_use]
    pub fn safe_meta(&self, occurred_at: Timestamp) -> CatalogSafeErrorMeta {
        CatalogSafeErrorMeta {
            code: self.code.clone(),
            safe_message: self.safe_message.clone(),
            occurred_at,
        }
    }

    fn from_store(error: SecureStoreError) -> Self {
        let code = match error {
            SecureStoreError::HomeUnavailable => "catalog_cache_home_unavailable",
            SecureStoreError::UnsafePath => "catalog_cache_invalid_path",
            SecureStoreError::TooLarge => "catalog_cache_too_large",
            SecureStoreError::LockContention { .. } => "catalog_cache_lock_contention",
            SecureStoreError::Io(_) => "catalog_cache_io_failed",
        };
        Self::new(code, "catalog cache could not be used")
    }

    fn from_transport(error: CatalogTransportError) -> Self {
        let code = match error {
            CatalogTransportError::ClientBuild => "catalog_transport_client_build_failed",
            CatalogTransportError::InvalidEtag => "invalid_catalog_etag",
            CatalogTransportError::InvalidHeaders => "catalog_response_headers_invalid",
            CatalogTransportError::RequestFailed => "catalog_network_failed",
            CatalogTransportError::BodyReadFailed => "catalog_body_read_failed",
            CatalogTransportError::BodyTooLarge => "catalog_body_too_large",
        };
        Self::new(code, error.to_string())
    }
}
