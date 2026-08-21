use std::{path::Path, sync::Arc};

use cookie_agent_identity::CatalogRevision;
use futures_util::StreamExt as _;
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use thiserror::Error;

use crate::secure_store::{SecureDirectory, SecureDirectoryLock, SecureStoreError};

use super::{
    CATALOG_BODY_FILE, CATALOG_CACHE_SCHEMA_VERSION, CATALOG_LOCK_FILE, CATALOG_MAX_BYTES,
    CATALOG_META_FILE, CatalogAgeState, CatalogAvailability, CatalogCacheMeta, CatalogRequest,
    CatalogRuntimeState, CatalogSafeErrorMeta, CatalogSnapshot, CatalogSource, CatalogTransport,
    CatalogTransportError, CatalogTransportResponse, MODELS_DEV_CATALOG_URL, parse_cache_meta,
    parse_catalog, validated_bootstrap,
};

const MAX_META_BYTES: u64 = 128 * 1024;
const MAX_JOURNAL_BYTES: u64 = 16 * 1024;
const MAX_ETAG_BYTES: usize = 1_024;
const MAX_SAFE_MESSAGE_BYTES: usize = 512;
const SEVEN_DAYS_SECONDS: i64 = 7 * 24 * 60 * 60;
const THIRTY_DAYS_SECONDS: i64 = 30 * 24 * 60 * 60;
const BODY_NEXT_FILE: &str = ".models-dev-v2.json.next";
const META_NEXT_FILE: &str = ".models-dev-v2.meta.json.next";
const BODY_BACKUP_FILE: &str = ".models-dev-v2.json.backup";
const META_BACKUP_FILE: &str = ".models-dev-v2.meta.json.backup";

/// Dynamic network/cache/bootstrap catalog manager.
pub struct CatalogManager<T> {
    transport: Arc<T>,
    cache: Result<SecureDirectory, CatalogError>,
    #[cfg(all(test, unix))]
    commit_failure: std::sync::Mutex<Option<CacheCommitPhase>>,
}

impl<T: CatalogTransport> CatalogManager<T> {
    /// Uses the fixed per-user catalog cache path.
    pub fn standard(transport: T) -> Self {
        Self {
            transport: Arc::new(transport),
            cache: SecureDirectory::user_data("catalog").map_err(CatalogError::from_store),
            #[cfg(all(test, unix))]
            commit_failure: std::sync::Mutex::new(None),
        }
    }

    /// Uses an explicit secure directory, primarily for deterministic tests.
    #[must_use]
    pub fn new(transport: T, cache: SecureDirectory) -> Self {
        Self {
            transport: Arc::new(transport),
            cache: Ok(cache),
            #[cfg(all(test, unix))]
            commit_failure: std::sync::Mutex::new(None),
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
            #[cfg(all(test, unix))]
            commit_failure: std::sync::Mutex::new(None),
        }
    }

    /// Refreshes using the system clock.
    pub async fn refresh(&self) -> Result<CatalogSnapshot, CatalogError> {
        self.refresh_at(Timestamp::now()).await
    }

    /// Refreshes at a supplied time for deterministic tests.
    pub async fn refresh_at(&self, now: Timestamp) -> Result<CatalogSnapshot, CatalogError> {
        let cache = self.load_cache(now);
        let etag = cache
            .as_ref()
            .ok()
            .and_then(|cache| cache.meta.etag.clone());
        let network = match self.transport.fetch(CatalogRequest::fixed(etag)).await {
            Ok(response) => self.validate_response(response),
            Err(error) => Err(CatalogError::from_transport(error)),
        };
        let network = match network {
            Ok(result) => Self::finish_stream(result).await,
            Err(error) => Err(error),
        };

        match network {
            Ok(NetworkResult::NotModified) => {
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
                let write_error = self.commit_cache(&cached.parsed.body, &meta).err();
                Ok(snapshot_from_parsed(
                    cached.parsed,
                    CatalogSource::Network,
                    meta.validated_at,
                    now,
                    meta.etag.clone(),
                    CatalogAvailability::Ready,
                    write_error.map(|error| error.safe_meta(now)),
                ))
            }
            Ok(NetworkResult::Body { bytes, etag }) => {
                let parsed = match parse_catalog(&bytes) {
                    Ok(parsed) => parsed,
                    Err(error) => return self.select_fallback(cache, error, now),
                };
                let mut meta = metadata_for(
                    &parsed,
                    CatalogSource::Network,
                    false,
                    now,
                    now,
                    etag.clone(),
                    None,
                );
                let write_error = self.commit_cache(&bytes, &meta).err();
                if let Some(error) = &write_error {
                    meta.last_error = Some(error.safe_meta(now));
                }
                Ok(snapshot_from_parsed(
                    parsed,
                    CatalogSource::Network,
                    now,
                    now,
                    etag,
                    CatalogAvailability::Ready,
                    meta.last_error,
                ))
            }
            Ok(NetworkResult::Streaming { .. }) => Err(CatalogError::new(
                "catalog_internal_stream_state",
                "catalog response stream was not finalized",
            )),
            Err(network_error) => self.select_fallback(cache, network_error, now),
        }
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
                    &cached.parsed,
                    CatalogSource::Cache,
                    true,
                    cached.meta.validated_at,
                    now,
                    cached.meta.etag.clone(),
                    Some(safe_error),
                );
                let write_error = self.commit_cache(&cached.parsed.body, &meta).err();
                if let Some(error) = write_error {
                    meta.last_error = Some(error.safe_meta(now));
                }
                Ok(snapshot_from_parsed(
                    cached.parsed,
                    CatalogSource::Cache,
                    meta.validated_at,
                    now,
                    meta.etag,
                    CatalogAvailability::Stale,
                    meta.last_error,
                ))
            }
            Err(cache_error) => {
                let bootstrap = validated_bootstrap()?;
                let parsed = parse_catalog(bootstrap)?;
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
                let write_error = self.commit_cache(bootstrap, &meta).err();
                if let Some(error) = write_error {
                    meta.last_error = Some(error.safe_meta(now));
                }
                Ok(snapshot_from_parsed(
                    parsed,
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

    fn validate_response(
        &self,
        response: CatalogTransportResponse,
    ) -> Result<NetworkResult, CatalogError> {
        if (300..400).contains(&response.status) && response.status != 304 {
            return Err(CatalogError::new(
                "catalog_redirect_rejected",
                "catalog redirects are forbidden",
            ));
        }
        if response.status == 304 {
            return Ok(NetworkResult::NotModified);
        }
        if response.status != 200 {
            return Err(CatalogError::new(
                "catalog_http_status",
                "catalog server returned an unusable status",
            ));
        }
        if response
            .content_encoding
            .as_deref()
            .is_some_and(|encoding| !encoding.trim().eq_ignore_ascii_case("identity"))
        {
            return Err(CatalogError::new(
                "catalog_compression_rejected",
                "compressed catalog responses are forbidden",
            ));
        }
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
            return Err(CatalogError::new(
                "catalog_body_too_large",
                "catalog response exceeds the byte limit",
            ));
        }
        let etag = response.etag.map(validate_etag).transpose()?;
        Ok(NetworkResult::Streaming {
            body: response.body,
            capacity: response
                .content_length
                .and_then(|length| usize::try_from(length).ok())
                .unwrap_or(0),
            etag,
        })
    }

    fn load_cache(&self, _now: Timestamp) -> Result<ValidatedCache, CatalogError> {
        let directory = self.cache.as_ref().map_err(Clone::clone)?;
        let lock = directory
            .lock(CATALOG_LOCK_FILE)
            .map_err(CatalogError::from_store)?;
        recover_cache(&lock)?;
        let body = lock
            .read(CATALOG_BODY_FILE, CATALOG_MAX_BYTES as u64)
            .map_err(CatalogError::from_store)?
            .ok_or_else(|| {
                CatalogError::new("catalog_cache_missing", "catalog cache body is missing")
            })?;
        let meta_bytes = lock
            .read(CATALOG_META_FILE, MAX_META_BYTES)
            .map_err(CatalogError::from_store)?
            .ok_or_else(|| {
                CatalogError::new("catalog_cache_missing", "catalog cache metadata is missing")
            })?;
        let meta = parse_cache_meta(&meta_bytes)?;
        validate_meta(&meta, &body)?;
        let parsed = parse_catalog(&body)?;
        if parsed.revision.as_str() != meta.body_revision {
            return Err(CatalogError::new(
                "catalog_cache_revision_mismatch",
                "catalog cache revision does not match its body",
            ));
        }
        validate_parsed_meta(&meta, &parsed)?;
        Ok(ValidatedCache { parsed, meta })
    }

    fn commit_cache(&self, body: &[u8], meta: &CatalogCacheMeta) -> Result<(), CatalogError> {
        let directory = self.cache.as_ref().map_err(Clone::clone)?;
        let lock = directory
            .lock(CATALOG_LOCK_FILE)
            .map_err(CatalogError::from_store)?;
        recover_cache(&lock)?;
        let meta_bytes = serde_json::to_vec_pretty(meta).map_err(|_| {
            CatalogError::new(
                "cache_metadata_write_failed",
                "catalog cache metadata could not be encoded",
            )
        })?;
        validate_meta(meta, body)?;
        self.commit_checkpoint(CacheCommitPhase::BeforeNextBody)?;
        lock.atomic_replace(BODY_NEXT_FILE, body)
            .map_err(CatalogError::from_store)?;
        self.commit_checkpoint(CacheCommitPhase::AfterNextBody)?;
        lock.atomic_replace(META_NEXT_FILE, &meta_bytes)
            .map_err(CatalogError::from_store)?;
        self.commit_checkpoint(CacheCommitPhase::AfterNextMetadata)?;
        validate_staged_pair(&lock, body, &meta_bytes)?;

        let previous = read_fixed_pair(&lock)?;
        let prepared = CacheJournalRecord::prepared(
            previous.is_some(),
            revision(body).as_str().to_owned(),
            digest_bytes(&meta_bytes),
        );
        append_journal_record(&lock, &prepared)?;
        self.commit_checkpoint(CacheCommitPhase::AfterPrepared)?;

        if let Some(previous) = &previous {
            lock.atomic_replace(BODY_BACKUP_FILE, &previous.body)
                .map_err(CatalogError::from_store)?;
            self.commit_checkpoint(CacheCommitPhase::AfterBodyBackup)?;
            lock.atomic_replace(META_BACKUP_FILE, &previous.meta)
                .map_err(CatalogError::from_store)?;
        }
        self.commit_checkpoint(CacheCommitPhase::AfterMetadataBackup)?;
        append_journal_record(
            &lock,
            &CacheJournalRecord::phase(CacheJournalPhase::Installing),
        )?;
        self.commit_checkpoint(CacheCommitPhase::AfterInstalling)?;

        lock.atomic_replace(CATALOG_BODY_FILE, body)
            .map_err(CatalogError::from_store)?;
        self.commit_checkpoint(CacheCommitPhase::AfterBodyInstall)?;
        lock.atomic_replace(CATALOG_META_FILE, &meta_bytes)
            .map_err(CatalogError::from_store)?;
        self.commit_checkpoint(CacheCommitPhase::AfterMetadataInstall)?;
        validate_fixed_pair(&lock, &prepared)?;
        self.commit_checkpoint(CacheCommitPhase::AfterValidation)?;

        append_journal_record(
            &lock,
            &CacheJournalRecord::phase(CacheJournalPhase::Committed),
        )?;
        self.commit_checkpoint(CacheCommitPhase::AfterCommitted)?;
        cleanup_transaction_files(&lock)?;
        self.commit_checkpoint(CacheCommitPhase::AfterCleanup)?;
        lock.clear_journal().map_err(CatalogError::from_store)?;
        Ok(())
    }

    #[cfg(all(test, unix))]
    fn fail_commit_at(&self, phase: CacheCommitPhase) {
        *self.commit_failure.lock().expect("commit failure lock") = Some(phase);
    }

    #[cfg(all(test, unix))]
    fn commit_checkpoint(&self, phase: CacheCommitPhase) -> Result<(), CatalogError> {
        let mut failure = self.commit_failure.lock().expect("commit failure lock");
        if failure.as_ref() == Some(&phase) {
            *failure = None;
            Err(CatalogError::new(
                "cache_commit_injected_failure",
                "catalog cache commit failure was injected",
            ))
        } else {
            Ok(())
        }
    }

    #[cfg(not(all(test, unix)))]
    fn commit_checkpoint(&self, _phase: CacheCommitPhase) -> Result<(), CatalogError> {
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CacheCommitPhase {
    BeforeNextBody,
    AfterNextBody,
    AfterNextMetadata,
    AfterPrepared,
    AfterBodyBackup,
    AfterMetadataBackup,
    AfterInstalling,
    AfterBodyInstall,
    AfterMetadataInstall,
    AfterValidation,
    AfterCommitted,
    AfterCleanup,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum CacheJournalPhase {
    Prepared,
    Installing,
    Committed,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct CacheJournalRecord {
    schema_version: u32,
    phase: CacheJournalPhase,
    had_previous: Option<bool>,
    body_revision: Option<String>,
    metadata_digest: Option<String>,
}

impl CacheJournalRecord {
    fn prepared(had_previous: bool, body_revision: String, metadata_digest: String) -> Self {
        Self {
            schema_version: CATALOG_CACHE_SCHEMA_VERSION,
            phase: CacheJournalPhase::Prepared,
            had_previous: Some(had_previous),
            body_revision: Some(body_revision),
            metadata_digest: Some(metadata_digest),
        }
    }

    const fn phase(phase: CacheJournalPhase) -> Self {
        Self {
            schema_version: CATALOG_CACHE_SCHEMA_VERSION,
            phase,
            had_previous: None,
            body_revision: None,
            metadata_digest: None,
        }
    }

    fn valid(&self) -> bool {
        if self.schema_version != CATALOG_CACHE_SCHEMA_VERSION {
            return false;
        }
        match self.phase {
            CacheJournalPhase::Prepared => {
                self.had_previous.is_some()
                    && self
                        .body_revision
                        .as_deref()
                        .is_some_and(valid_sha256_revision)
                    && self
                        .metadata_digest
                        .as_deref()
                        .is_some_and(valid_sha256_revision)
            }
            CacheJournalPhase::Installing | CacheJournalPhase::Committed => {
                self.had_previous.is_none()
                    && self.body_revision.is_none()
                    && self.metadata_digest.is_none()
            }
        }
    }
}

fn append_journal_record(
    lock: &SecureDirectoryLock<'_>,
    record: &CacheJournalRecord,
) -> Result<(), CatalogError> {
    let mut bytes = serde_json::to_vec(record).map_err(|_| {
        CatalogError::new(
            "cache_journal_write_failed",
            "catalog cache transaction journal could not be encoded",
        )
    })?;
    bytes.push(b'\n');
    lock.append_journal(&bytes, MAX_JOURNAL_BYTES)
        .map_err(CatalogError::from_store)
}

fn recover_cache(lock: &SecureDirectoryLock<'_>) -> Result<(), CatalogError> {
    let bytes = lock
        .read_journal(MAX_JOURNAL_BYTES)
        .map_err(CatalogError::from_store)?;
    let records = valid_journal_prefix(&bytes);
    let Some(prepared) = records.first() else {
        lock.clear_journal().map_err(CatalogError::from_store)?;
        cleanup_transaction_files(lock)?;
        return Ok(());
    };
    let committed = records
        .last()
        .is_some_and(|record| record.phase == CacheJournalPhase::Committed);
    let installing = records
        .iter()
        .any(|record| record.phase == CacheJournalPhase::Installing);
    if committed && fixed_pair_matches(lock, prepared)? {
        lock.clear_journal().map_err(CatalogError::from_store)?;
        cleanup_transaction_files(lock)?;
        return Ok(());
    }
    if installing {
        rollback_transaction(lock, prepared.had_previous == Some(true))?;
    }
    lock.clear_journal().map_err(CatalogError::from_store)?;
    cleanup_transaction_files(lock)?;
    Ok(())
}

fn valid_journal_prefix(bytes: &[u8]) -> Vec<CacheJournalRecord> {
    let mut records = Vec::new();
    for line in bytes.split_inclusive(|byte| *byte == b'\n') {
        let Some(payload) = line.strip_suffix(b"\n") else {
            break;
        };
        let Ok(record) = serde_json::from_slice::<CacheJournalRecord>(payload) else {
            break;
        };
        if !record.valid() {
            break;
        }
        let expected = match records.len() {
            0 => CacheJournalPhase::Prepared,
            1 => CacheJournalPhase::Installing,
            2 => CacheJournalPhase::Committed,
            _ => break,
        };
        if record.phase != expected {
            break;
        }
        records.push(record);
    }
    records
}

fn rollback_transaction(
    lock: &SecureDirectoryLock<'_>,
    had_previous: bool,
) -> Result<(), CatalogError> {
    if had_previous {
        let body = lock
            .read(BODY_BACKUP_FILE, CATALOG_MAX_BYTES as u64)
            .map_err(CatalogError::from_store)?
            .ok_or_else(|| {
                CatalogError::new(
                    "cache_recovery_failed",
                    "catalog cache body backup is missing",
                )
            })?;
        let meta = lock
            .read(META_BACKUP_FILE, MAX_META_BYTES)
            .map_err(CatalogError::from_store)?
            .ok_or_else(|| {
                CatalogError::new(
                    "cache_recovery_failed",
                    "catalog cache metadata backup is missing",
                )
            })?;
        validate_pair_bytes(&body, &meta)?;
        lock.atomic_replace(CATALOG_BODY_FILE, &body)
            .map_err(CatalogError::from_store)?;
        lock.atomic_replace(CATALOG_META_FILE, &meta)
            .map_err(CatalogError::from_store)?;
    } else {
        lock.remove(CATALOG_BODY_FILE)
            .map_err(CatalogError::from_store)?;
        lock.remove(CATALOG_META_FILE)
            .map_err(CatalogError::from_store)?;
    }
    Ok(())
}

fn cleanup_transaction_files(lock: &SecureDirectoryLock<'_>) -> Result<(), CatalogError> {
    for name in [
        BODY_NEXT_FILE,
        META_NEXT_FILE,
        BODY_BACKUP_FILE,
        META_BACKUP_FILE,
    ] {
        lock.remove(name).map_err(CatalogError::from_store)?;
    }
    Ok(())
}

struct CachePair {
    body: Vec<u8>,
    meta: Vec<u8>,
}

fn read_fixed_pair(lock: &SecureDirectoryLock<'_>) -> Result<Option<CachePair>, CatalogError> {
    let body = lock
        .read(CATALOG_BODY_FILE, CATALOG_MAX_BYTES as u64)
        .map_err(CatalogError::from_store)?;
    let meta = lock
        .read(CATALOG_META_FILE, MAX_META_BYTES)
        .map_err(CatalogError::from_store)?;
    match (body, meta) {
        (Some(body), Some(meta)) if validate_pair_bytes(&body, &meta).is_ok() => {
            Ok(Some(CachePair { body, meta }))
        }
        (None, None) => Ok(None),
        _ => {
            lock.remove(CATALOG_BODY_FILE)
                .map_err(CatalogError::from_store)?;
            lock.remove(CATALOG_META_FILE)
                .map_err(CatalogError::from_store)?;
            Ok(None)
        }
    }
}

fn validate_staged_pair(
    lock: &SecureDirectoryLock<'_>,
    expected_body: &[u8],
    expected_meta: &[u8],
) -> Result<(), CatalogError> {
    let body = lock
        .read(BODY_NEXT_FILE, CATALOG_MAX_BYTES as u64)
        .map_err(CatalogError::from_store)?
        .ok_or_else(|| {
            CatalogError::new(
                "cache_transaction_validation_failed",
                "catalog cache staged body is missing",
            )
        })?;
    let meta = lock
        .read(META_NEXT_FILE, MAX_META_BYTES)
        .map_err(CatalogError::from_store)?
        .ok_or_else(|| {
            CatalogError::new(
                "cache_transaction_validation_failed",
                "catalog cache staged metadata is missing",
            )
        })?;
    if body != expected_body || meta != expected_meta {
        return Err(CatalogError::new(
            "cache_transaction_validation_failed",
            "catalog cache staged pair changed before commit",
        ));
    }
    validate_pair_bytes(&body, &meta)
}

fn validate_fixed_pair(
    lock: &SecureDirectoryLock<'_>,
    prepared: &CacheJournalRecord,
) -> Result<(), CatalogError> {
    if fixed_pair_matches(lock, prepared)? {
        Ok(())
    } else {
        Err(CatalogError::new(
            "cache_transaction_validation_failed",
            "catalog cache fixed pair changed before commit",
        ))
    }
}

fn fixed_pair_matches(
    lock: &SecureDirectoryLock<'_>,
    prepared: &CacheJournalRecord,
) -> Result<bool, CatalogError> {
    let Some(body) = lock
        .read(CATALOG_BODY_FILE, CATALOG_MAX_BYTES as u64)
        .map_err(CatalogError::from_store)?
    else {
        return Ok(false);
    };
    let Some(meta) = lock
        .read(CATALOG_META_FILE, MAX_META_BYTES)
        .map_err(CatalogError::from_store)?
    else {
        return Ok(false);
    };
    Ok(validate_pair_bytes(&body, &meta).is_ok()
        && prepared.body_revision.as_deref() == Some(revision(&body).as_str())
        && prepared.metadata_digest.as_deref() == Some(digest_bytes(&meta).as_str()))
}

fn validate_pair_bytes(body: &[u8], meta_bytes: &[u8]) -> Result<(), CatalogError> {
    let meta = parse_cache_meta(meta_bytes)?;
    validate_meta(&meta, body)?;
    let parsed = parse_catalog(body)?;
    if parsed.revision.as_str() != meta.body_revision {
        return Err(CatalogError::new(
            "catalog_cache_revision_mismatch",
            "catalog cache revision does not match its body",
        ));
    }
    validate_parsed_meta(&meta, &parsed)
}

fn digest_bytes(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

fn valid_sha256_revision(value: &str) -> bool {
    value.len() == 71
        && value.starts_with("sha256:")
        && value[7..].bytes().all(|byte| byte.is_ascii_hexdigit())
}

enum NetworkResult {
    NotModified,
    Body {
        bytes: Vec<u8>,
        etag: Option<String>,
    },
    Streaming {
        body: super::CatalogBodyStream,
        capacity: usize,
        etag: Option<String>,
    },
}

impl<T: CatalogTransport> CatalogManager<T> {
    async fn finish_stream(result: NetworkResult) -> Result<NetworkResult, CatalogError> {
        let NetworkResult::Streaming {
            mut body,
            capacity,
            etag,
        } = result
        else {
            return Ok(result);
        };
        let mut bytes = Vec::with_capacity(capacity.min(CATALOG_MAX_BYTES));
        while let Some(chunk) = body.next().await {
            let chunk = chunk.map_err(CatalogError::from_transport)?;
            if bytes.len().saturating_add(chunk.len()) > CATALOG_MAX_BYTES {
                return Err(CatalogError::new(
                    "catalog_body_too_large",
                    "catalog response exceeds the byte limit",
                ));
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(NetworkResult::Body { bytes, etag })
    }
}

struct ValidatedCache {
    parsed: super::ParsedCatalog,
    meta: CatalogCacheMeta,
}

fn metadata_for(
    parsed: &super::ParsedCatalog,
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
        byte_length: parsed.body.len() as u64,
        validated_at,
        last_checked_at: checked_at,
        selected_source: source,
        stale,
        last_error,
    }
}

fn validate_meta(meta: &CatalogCacheMeta, body: &[u8]) -> Result<(), CatalogError> {
    if meta.schema_version != CATALOG_CACHE_SCHEMA_VERSION
        || meta.url != MODELS_DEV_CATALOG_URL
        || meta.byte_length != body.len() as u64
        || meta.body_revision != revision(body).as_str()
        || meta.etag.clone().map(validate_etag).transpose()?.as_deref() != meta.etag.as_deref()
    {
        return Err(CatalogError::new(
            "invalid_catalog_cache_metadata",
            "catalog cache metadata does not match its body",
        ));
    }
    Ok(())
}

fn validate_parsed_meta(
    meta: &CatalogCacheMeta,
    parsed: &super::ParsedCatalog,
) -> Result<(), CatalogError> {
    let _ = parsed;
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
    if !error_valid {
        Err(CatalogError::new(
            "invalid_catalog_cache_metadata",
            "catalog cache metadata quarantine state is invalid",
        ))
    } else {
        Ok(())
    }
}

fn revision(body: &[u8]) -> CatalogRevision {
    CatalogRevision::new(format!("sha256:{:x}", Sha256::digest(body)))
        .expect("SHA-256 revision is valid")
}

fn snapshot_from_parsed(
    parsed: super::ParsedCatalog,
    source: CatalogSource,
    validated_at: Timestamp,
    checked_at: Timestamp,
    etag: Option<String>,
    availability: CatalogAvailability,
    last_error: Option<CatalogSafeErrorMeta>,
) -> CatalogSnapshot {
    CatalogSnapshot {
        revision: parsed.revision,
        source,
        state: CatalogRuntimeState {
            availability,
            age: age_state(validated_at, checked_at),
            last_error,
        },
        validated_at,
        last_checked_at: checked_at,
        etag,
        providers: parsed.providers,
        canonical_models: parsed.canonical_models,
        quarantine: parsed.quarantine,
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
            SecureStoreError::UnsafePath => "catalog_cache_unsafe_path",
            SecureStoreError::TooLarge => "catalog_cache_too_large",
            SecureStoreError::Io(_) => "catalog_cache_io_failed",
        };
        Self::new(code, "catalog cache could not be used safely")
    }

    fn from_transport(error: CatalogTransportError) -> Self {
        let code = match error {
            CatalogTransportError::ClientBuild => "catalog_transport_client_build_failed",
            CatalogTransportError::InvalidRequest => "catalog_transport_invalid_request",
            CatalogTransportError::InvalidEtag => "invalid_catalog_etag",
            CatalogTransportError::InvalidHeaders => "catalog_response_headers_invalid",
            CatalogTransportError::RequestFailed => "catalog_network_failed",
            CatalogTransportError::BodyReadFailed => "catalog_body_read_failed",
            CatalogTransportError::BodyTooLarge => "catalog_body_too_large",
        };
        Self::new(code, error.to_string())
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::{collections::VecDeque, fs, os::unix::fs::PermissionsExt as _, sync::Mutex};

    use super::*;
    use crate::catalog::{CatalogRequest, CatalogTransportFuture, CatalogTransportResponse};

    struct TestTransport {
        responses: Mutex<VecDeque<Result<CatalogTransportResponse, CatalogTransportError>>>,
    }

    impl CatalogTransport for TestTransport {
        fn fetch(&self, _request: CatalogRequest) -> CatalogTransportFuture<'_> {
            let response = self
                .responses
                .lock()
                .expect("transport responses")
                .pop_front()
                .expect("scripted response");
            Box::pin(async move { response })
        }
    }

    fn body(name: &str) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "providers": {
                "test": {
                    "id": "test",
                    "env": ["TEST_API_KEY"],
                    "npm": "test-npm",
                    "name": "Test",
                    "doc": "https://example.invalid",
                    "models": {
                        "model": {
                            "id": "model",
                            "name": name,
                            "description": "test",
                            "attachment": false,
                            "reasoning": false,
                            "tool_call": false,
                            "open_weights": false,
                            "release_date": "2026-08-01",
                            "last_updated": "2026-08-05",
                            "modalities": {"input": ["text"], "output": ["text"]},
                            "limit": {"context": 4096, "output": 1024}
                        }
                    }
                }
            },
            "models": {
                "model": {
                    "id": "model",
                    "name": name,
                    "description": "test",
                    "attachment": false,
                    "reasoning": false,
                    "tool_call": false,
                    "open_weights": false,
                    "release_date": "2026-08-01",
                    "last_updated": "2026-08-05",
                    "modalities": {"input": ["text"], "output": ["text"]},
                    "limit": {"context": 4096, "output": 1024}
                }
            }
        }))
        .expect("catalog fixture")
    }

    fn time() -> Timestamp {
        "2026-08-05T00:00:00Z".parse().expect("timestamp")
    }

    #[tokio::test]
    async fn every_fixed_pair_commit_failure_recovers_one_complete_transaction() {
        for phase in [
            CacheCommitPhase::BeforeNextBody,
            CacheCommitPhase::AfterNextBody,
            CacheCommitPhase::AfterNextMetadata,
            CacheCommitPhase::AfterPrepared,
            CacheCommitPhase::AfterBodyBackup,
            CacheCommitPhase::AfterMetadataBackup,
            CacheCommitPhase::AfterInstalling,
            CacheCommitPhase::AfterBodyInstall,
            CacheCommitPhase::AfterMetadataInstall,
            CacheCommitPhase::AfterValidation,
            CacheCommitPhase::AfterCommitted,
            CacheCommitPhase::AfterCleanup,
        ] {
            let temporary = tempfile::tempdir().expect("temporary directory");
            fs::set_permissions(temporary.path(), fs::Permissions::from_mode(0o700))
                .expect("private temporary directory");
            let directory = SecureDirectory::open_in(temporary.path(), "catalog")
                .expect("secure catalog directory");
            let first_body = body("first");
            let second_body = body("second");
            let transport = TestTransport {
                responses: Mutex::new(VecDeque::from([
                    Ok(CatalogTransportResponse::from_bytes(200, first_body)),
                    Ok(CatalogTransportResponse::from_bytes(200, second_body)),
                    Err(CatalogTransportError::RequestFailed),
                ])),
            };
            let manager = CatalogManager::new(transport, directory);
            let first = manager.refresh_at(time()).await.expect("first transaction");
            manager.fail_commit_at(phase);
            let second = manager.refresh_at(time()).await.expect("network selection");
            assert_eq!(
                second
                    .state
                    .last_error
                    .as_ref()
                    .map(|error| error.code.as_str()),
                Some("cache_commit_injected_failure"),
                "phase {phase:?}"
            );
            let recovered = manager.refresh_at(time()).await.expect("cache recovery");
            assert_eq!(recovered.source, CatalogSource::Cache, "phase {phase:?}");
            let expected = if matches!(
                phase,
                CacheCommitPhase::AfterCommitted | CacheCommitPhase::AfterCleanup
            ) {
                &second.revision
            } else {
                &first.revision
            };
            assert_eq!(&recovered.revision, expected, "phase {phase:?}");
            let entries = fs::read_dir(temporary.path().join("catalog"))
                .expect("cache directory")
                .map(|entry| {
                    entry
                        .expect("cache entry")
                        .file_name()
                        .into_string()
                        .expect("UTF-8 cache name")
                })
                .collect::<std::collections::BTreeSet<_>>();
            assert_eq!(
                entries,
                [CATALOG_BODY_FILE, CATALOG_META_FILE, CATALOG_LOCK_FILE]
                    .into_iter()
                    .map(str::to_owned)
                    .collect(),
                "phase {phase:?}"
            );
        }
    }
}
