//! The daemon's record of providers that native compaction does not work
//! with (`docs/site/specs/native-compaction.md` section 5).
//!
//! `native-compaction.json` in the daemon's data folder holds one entry per
//! provider ID: `unsupported` when the compact call answered definitively
//! that the provider has none, `rejects_windows` when the provider refused
//! a request carrying a compacted window. Either state stops native
//! compaction for the provider; `rejects_windows` also makes requests use
//! the summary instead of stored windows. An entry expires after seven days
//! and is dropped when the provider's configuration changes. The file is a
//! cache: one that cannot be read counts as empty and is written again on
//! the next change.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Mutex,
};

use cookie_agent_protocol::{
    FrozenModelBinding, FrozenProviderSource, NativeCompactionMark, NativeCompactionState,
};
use oven_sdk::{ModelError, provider_support::error_names_responses_native_context};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

/// The record's file name in the daemon's data folder.
pub(crate) const RECORD_FILE: &str = "native-compaction.json";

/// How long an entry stands before native compaction is tried again.
pub(crate) const ENTRY_LIFETIME: jiff::SignedDuration = jiff::SignedDuration::from_hours(7 * 24);

/// Why a provider is in the record.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum MarkState {
    Unsupported,
    RejectsWindows,
}

impl MarkState {
    fn wire(self) -> NativeCompactionState {
        match self {
            Self::Unsupported => NativeCompactionState::Unsupported,
            Self::RejectsWindows => NativeCompactionState::RejectsWindows,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct Entry {
    state: MarkState,
    since: jiff::Timestamp,
    reason: String,
    config_digest: String,
}

impl Entry {
    fn expired(&self, now: jiff::Timestamp) -> bool {
        now.duration_since(self.since) >= ENTRY_LIFETIME
    }
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct RecordFile {
    #[serde(default)]
    providers: BTreeMap<String, Entry>,
}

/// [`RecordFile`] as it is written, borrowing the entries.
#[derive(Serialize)]
struct RecordFileRef<'a> {
    providers: &'a BTreeMap<String, Entry>,
}

/// The record, held in memory and written whole on every change.
#[derive(Debug, Default)]
pub(crate) struct NativeCompactionRecord {
    path: Option<PathBuf>,
    entries: Mutex<BTreeMap<String, Entry>>,
}

impl NativeCompactionRecord {
    /// The record of the daemon whose data folder is `data_dir`, as its file
    /// holds it; empty when the file is missing or unreadable.
    pub(crate) fn open(data_dir: &Path) -> Self {
        let path = data_dir.join(RECORD_FILE);
        let entries = std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<RecordFile>(&bytes).ok())
            .map(|file| file.providers)
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

    /// The state the record holds for `binding`'s provider, now. An
    /// unmarked provider, the usual case, is answered without the digest
    /// of its configuration.
    pub(crate) fn state(&self, binding: &FrozenModelBinding) -> Option<MarkState> {
        if !self
            .lock()
            .contains_key(binding.selection.model.provider_id().as_str())
        {
            return None;
        }
        self.state_at(
            binding.selection.model.provider_id().as_str(),
            &provider_config_digest(binding),
            jiff::Timestamp::now(),
        )
    }

    /// Whether native compaction may run for `binding`'s provider.
    pub(crate) fn allows_compaction(&self, binding: &FrozenModelBinding) -> bool {
        self.state(binding).is_none()
    }

    /// Whether requests on `binding`'s provider may carry stored windows.
    pub(crate) fn allows_windows(&self, binding: &FrozenModelBinding) -> bool {
        self.state(binding) != Some(MarkState::RejectsWindows)
    }

    /// The state for `provider` at `now`, dropping its entry, and writing
    /// the file, when the entry expired or `config_digest` differs from the
    /// one it was marked under.
    pub(crate) fn state_at(
        &self,
        provider: &str,
        config_digest: &str,
        now: jiff::Timestamp,
    ) -> Option<MarkState> {
        let mut entries = self.lock();
        let entry = entries.get(provider)?;
        if !entry.expired(now) && entry.config_digest == config_digest {
            return Some(entry.state);
        }
        entries.remove(provider);
        self.write(&entries);
        None
    }

    /// Marks `binding`'s provider with `state` for `reason`, now.
    pub(crate) fn mark(&self, binding: &FrozenModelBinding, state: MarkState, reason: String) {
        self.mark_at(
            binding.selection.model.provider_id().as_str(),
            &provider_config_digest(binding),
            state,
            reason,
            jiff::Timestamp::now(),
        );
    }

    /// Marks `provider`, replacing any entry it had, and writes the file.
    pub(crate) fn mark_at(
        &self,
        provider: &str,
        config_digest: &str,
        state: MarkState,
        reason: String,
        now: jiff::Timestamp,
    ) {
        let mut entries = self.lock();
        entries.insert(
            provider.to_owned(),
            Entry {
                state,
                since: now,
                reason,
                config_digest: config_digest.to_owned(),
            },
        );
        self.write(&entries);
    }

    /// The entries that have not expired, for the configuration status.
    pub(crate) fn marks(&self) -> Vec<NativeCompactionMark> {
        self.marks_at(jiff::Timestamp::now())
    }

    pub(crate) fn marks_at(&self, now: jiff::Timestamp) -> Vec<NativeCompactionMark> {
        self.lock()
            .iter()
            .filter(|(_, entry)| !entry.expired(now))
            .map(|(provider, entry)| NativeCompactionMark {
                provider_id: provider.clone(),
                state: entry.state.wire(),
                reason: entry.reason.clone(),
                age_seconds: u64::try_from(now.duration_since(entry.since).as_secs()).unwrap_or(0),
            })
            .collect()
    }

    /// Writes `providers`, which the caller holds locked: the writes of two
    /// changes never interleave, and the last change is the last written.
    fn write(&self, providers: &BTreeMap<String, Entry>) {
        let Some(path) = &self.path else {
            return;
        };
        if let Err(error) = write_atomic(path, &RecordFileRef { providers }) {
            eprintln!("cookie-agent: could not write {}: {error}", path.display());
        }
    }
}

/// Writes `file` through a temporary file renamed over `path`, so a reader
/// never sees half of it.
pub(crate) fn write_atomic(path: &Path, file: &impl Serialize) -> std::io::Result<()> {
    let bytes = serde_json::to_vec_pretty(file).map_err(std::io::Error::other)?;
    let temporary = path.with_extension("json.tmp");
    std::fs::write(&temporary, bytes)?;
    std::fs::rename(&temporary, path)
}

/// A digest of what defines `binding`'s provider rather than one of its
/// models: where it comes from, its endpoint, recipe, setup, and how it
/// authenticates. An entry marked under another digest is stale.
pub(crate) fn provider_config_digest(binding: &FrozenModelBinding) -> String {
    let source = match &binding.source {
        FrozenProviderSource::Managed {
            provider_recipe,
            recipe_fingerprint,
            ..
        } => serde_json::json!({
            "managed": provider_recipe,
            "recipe": recipe_fingerprint,
        }),
        FrozenProviderSource::Custom {
            safe_definition_fingerprint,
        } => serde_json::json!({ "custom": safe_definition_fingerprint }),
    };
    let value = serde_json::json!({
        "provider": binding.selection.model.provider_id(),
        "source": source,
        "endpoint": binding.endpoint_identity,
        "provider_recipe": binding.provider_recipe,
        "setup": binding.setup_binding.setup_fingerprint,
        "auth_method": binding.credential_binding.auth_method,
        "auth_parameters": binding.credential_binding.parameters,
    });
    let mut hasher = Sha256::new();
    hasher.update(b"cookie-agent/native-compaction-provider/v1\0");
    hasher.update(value.to_string().as_bytes());
    format!("sha256:{:x}", hasher.finalize())
}

/// How a failed compact call marks its provider, if at all: `seeded` says
/// the call carried the previous window. Only a definitive answer marks:
/// HTTP 404, 405, or 501, or a refusal that names the compaction item. A
/// refusal naming it when the call carried a window concerns the window.
/// Transient failures, rate limits, other 5xx, timeouts, and authentication
/// errors mark nothing.
pub(crate) fn compact_failure_mark(
    error: &ModelError,
    seeded: bool,
) -> Option<(MarkState, String)> {
    let status = error.diagnostics.http_status?;
    let detail = vendor_detail(error);
    if error_names_responses_native_context(error) {
        return Some(if seeded {
            (
                MarkState::RejectsWindows,
                format!("the compact call refused the window it carried (HTTP {status}{detail})"),
            )
        } else {
            (
                MarkState::Unsupported,
                format!(
                    "the compact call was refused naming the compaction item (HTTP {status}{detail})"
                ),
            )
        });
    }
    matches!(status, 404 | 405 | 501).then(|| {
        (
            MarkState::Unsupported,
            format!("the compact endpoint answered HTTP {status}{detail}"),
        )
    })
}

/// Why a provider's refusal of a request that carried a window marks it
/// `rejects_windows`, or `None` when the refusal does not name the window
/// (a 400 or 422 naming the compaction item or its encrypted content, as
/// OpenAI's `invalid_encrypted_content` does).
pub(crate) fn rejected_window_reason(error: &ModelError) -> Option<String> {
    error_names_responses_native_context(error).then(|| {
        format!(
            "a request carrying a compacted window was refused (HTTP {}{})",
            error.diagnostics.http_status.unwrap_or_default(),
            vendor_detail(error)
        )
    })
}

pub(crate) fn vendor_detail(error: &ModelError) -> String {
    error
        .diagnostics
        .vendor_code
        .as_deref()
        .map(|code| {
            let code = code
                .chars()
                .filter(|character| !character.is_control())
                .take(128)
                .collect::<String>();
            format!(", {code}")
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests;
