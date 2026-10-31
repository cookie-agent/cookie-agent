//! The daemon's record of models that rejected deferred tools (deferred
//! tools in cookie, section 9).
//!
//! `deferred-tools.json` in the daemon's data folder holds one entry per
//! model, keyed `provider/model`: a request that sent it deferred tools was
//! refused, and the same request without them succeeded. A marked model is
//! sent every tool up front. An entry expires after seven days and is
//! dropped when the model's configuration changes. The file is a cache: one
//! that cannot be read counts as empty and is written again on the next
//! change.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Mutex,
};

use cookie_agent_models::DeferredToolsSupport;
use cookie_agent_protocol::{DeferredToolsMark, FrozenModelBinding};
use oven_sdk::{ModelError, ModelErrorKind};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::native_compaction::{
    ENTRY_LIFETIME, provider_config_digest, vendor_detail, write_atomic,
};

/// The record's file name in the daemon's data folder.
pub(crate) const RECORD_FILE: &str = "deferred-tools.json";

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct Entry {
    since: jiff::Timestamp,
    reason: String,
    config_digest: String,
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct RecordFile {
    #[serde(default)]
    models: BTreeMap<String, Entry>,
}

/// [`RecordFile`] as it is written, borrowing the entries.
#[derive(Serialize)]
struct RecordFileRef<'a> {
    models: &'a BTreeMap<String, Entry>,
}

/// The record, held in memory and written whole on every change.
#[derive(Debug, Default)]
pub(crate) struct DeferredToolsRecord {
    path: Option<PathBuf>,
    entries: Mutex<BTreeMap<String, Entry>>,
}

impl DeferredToolsRecord {
    /// The record of the daemon whose data folder is `data_dir`, as its file
    /// holds it; empty when the file is missing or unreadable.
    pub(crate) fn open(data_dir: &Path) -> Self {
        let path = data_dir.join(RECORD_FILE);
        let entries = std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<RecordFile>(&bytes).ok())
            .map(|file| file.models)
            .unwrap_or_default();
        Self {
            path: Some(path),
            entries: Mutex::new(entries),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, Entry>> {
        self.entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Whether `binding`'s model is marked, now.
    pub(crate) fn rejects(&self, binding: &FrozenModelBinding) -> bool {
        self.rejects_at(
            binding.selection.model.as_str(),
            &model_config_digest(binding),
            jiff::Timestamp::now(),
        )
    }

    /// Whether `model` is marked at `now`, dropping its entry, and writing
    /// the file, when the entry expired or `config_digest` differs from the
    /// one it was marked under.
    pub(crate) fn rejects_at(
        &self,
        model: &str,
        config_digest: &str,
        now: jiff::Timestamp,
    ) -> bool {
        let mut entries = self.lock();
        let Some(entry) = entries.get(model) else {
            return false;
        };
        if now.duration_since(entry.since) < ENTRY_LIFETIME && entry.config_digest == config_digest
        {
            return true;
        }
        entries.remove(model);
        self.write(&entries);
        false
    }

    /// Marks `binding`'s model for `reason`, now.
    pub(crate) fn mark(&self, binding: &FrozenModelBinding, reason: String) {
        self.mark_at(
            binding.selection.model.as_str(),
            &model_config_digest(binding),
            reason,
            jiff::Timestamp::now(),
        );
    }

    /// Marks `model`, replacing any entry it had, and writes the file.
    pub(crate) fn mark_at(
        &self,
        model: &str,
        config_digest: &str,
        reason: String,
        now: jiff::Timestamp,
    ) {
        let mut entries = self.lock();
        entries.insert(
            model.to_owned(),
            Entry {
                since: now,
                reason,
                config_digest: config_digest.to_owned(),
            },
        );
        self.write(&entries);
    }

    /// The entries that have not expired, for the configuration status.
    pub(crate) fn marks(&self) -> Vec<DeferredToolsMark> {
        self.marks_at(jiff::Timestamp::now())
    }

    pub(crate) fn marks_at(&self, now: jiff::Timestamp) -> Vec<DeferredToolsMark> {
        self.lock()
            .iter()
            .filter(|(_, entry)| now.duration_since(entry.since) < ENTRY_LIFETIME)
            .filter_map(|(model, entry)| {
                let (provider_id, model_id) = model.split_once('/')?;
                Some(DeferredToolsMark {
                    provider_id: provider_id.to_owned(),
                    model_id: model_id.to_owned(),
                    reason: entry.reason.clone(),
                    age_seconds: u64::try_from(now.duration_since(entry.since).as_secs())
                        .unwrap_or(0),
                })
            })
            .collect()
    }

    /// Writes `models`, which the caller holds locked: the writes of two
    /// changes never interleave, and the last change is the last written.
    fn write(&self, models: &BTreeMap<String, Entry>) {
        let Some(path) = &self.path else {
            return;
        };
        if let Err(error) = write_atomic(path, &RecordFileRef { models }) {
            eprintln!("cookie-agent: could not write {}: {error}", path.display());
        }
    }
}

/// A digest of what defines `binding`'s model: its provider's
/// configuration, the model, and the deferral support its descriptor
/// claims. An entry marked under another digest is stale.
pub(crate) fn model_config_digest(binding: &FrozenModelBinding) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"cookie-agent/deferred-tools-model/v1\0");
    hasher.update(provider_config_digest(binding).as_bytes());
    hasher.update(b"\0");
    hasher.update(binding.selection.model.as_str().as_bytes());
    hasher.update(b"\0");
    hasher.update(
        DeferredToolsSupport::of_descriptor(&binding.descriptor)
            .as_str()
            .as_bytes(),
    );
    format!("sha256:{:x}", hasher.finalize())
}

/// Why a request that sent `support`'s model deferred tools is tried once
/// more without them, or `None`: any `400` or `422` from a model deferral is
/// only attempted on, and on a verified model only a refusal oven names as
/// unsupported.
pub(crate) fn retry_reason(error: &ModelError, support: DeferredToolsSupport) -> Option<String> {
    let status = error.diagnostics.http_status;
    let retried = match support {
        DeferredToolsSupport::Attempted => matches!(status, Some(400 | 422)),
        DeferredToolsSupport::Native => error.kind == ModelErrorKind::Unsupported,
        DeferredToolsSupport::Unsupported => false,
    };
    retried.then(|| {
        format!(
            "a request with deferred tools was refused (HTTP {}{})",
            status.map_or_else(|| "error".to_owned(), |status| status.to_string()),
            vendor_detail(error)
        )
    })
}

#[cfg(test)]
mod tests;
