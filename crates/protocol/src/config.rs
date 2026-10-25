//! Reading and changing the daemon's configuration: `config.toml` and the
//! agent documents of its user root.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::ConfigRevision;

pub const CONFIG_GET_METHOD: &str = "config.get";
pub const CONFIG_APPLY_METHOD: &str = "config.apply";
pub const CONFIG_RELOAD_METHOD: &str = "config.reload";

/// A configuration part that takes effect only when the daemon restarts.
///
/// A value this build does not know reads as `server`: a restart of the whole
/// daemon.
#[derive(
    Clone, Copy, Debug, Deserialize, Eq, JsonSchema, Ord, PartialEq, PartialOrd, Serialize,
)]
#[serde(rename_all = "snake_case")]
#[schemars(extend("x-open" = true))]
pub enum ConfigRestartPart {
    Plugins,
    Runtimes,
    #[serde(other)]
    Server,
}

/// Where the daemon's configuration stands.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct ConfigStatus {
    /// The revision of the configuration the daemon runs on.
    pub applied_revision: ConfigRevision,
    /// Parts changed since the daemon started that apply only after a
    /// restart.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub restart_required: Vec<ConfigRestartPart>,
    /// Why the last reload failed, until one succeeds; the daemon keeps
    /// running on the applied configuration meanwhile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// What the applied configuration names that does not exist yet, such
    /// as a delegation target no agent has (an ACP agent whose runtime has
    /// not reported it, or a typo); the configuration still applies, and a
    /// diagnostic clears once the agent appears (client protocol 1.18.0).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 256))]
    pub diagnostics: Vec<String>,
    /// Providers the daemon stopped attempting native compaction for, or
    /// whose stored windows it ignores, until the mark expires or the
    /// provider's configuration changes (client protocol 1.29.0).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 1024))]
    pub native_compaction: Vec<NativeCompactionMark>,
}

/// One provider in the daemon's native compaction record.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct NativeCompactionMark {
    pub provider_id: String,
    pub state: NativeCompactionState,
    /// What the provider answered that marked it.
    pub reason: String,
    /// How long ago it was marked.
    pub age_seconds: u64,
}

/// Why a provider is in the native compaction record.
///
/// A value this build does not know reads as `unknown`: native compaction
/// is off for the provider for a reason it cannot name.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
#[schemars(extend("x-open" = true))]
pub enum NativeCompactionState {
    /// The compact call answered that the provider has no native compaction.
    Unsupported,
    /// The provider refused a request that carried a compacted window.
    RejectsWindows,
    #[serde(other)]
    Unknown,
}

/// Re-read the configuration from disk and apply it, for edits made by hand.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigReloadParams {}

/// The status afterwards; a configuration that failed validation is not
/// applied and its error is in `status.error`.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct ConfigReloadResult {
    pub status: ConfigStatus,
}

/// Read the configuration documents on disk.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigGetParams {}

/// One configuration document: `config.toml`, `agents/<id>.md`, or
/// `agents/<preset>/<id>.md`.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct ConfigDocument {
    pub path: String,
    pub text: String,
    /// `sha256:<hex>` of the document's bytes, for `config.apply`.
    pub fingerprint: String,
}

/// The configuration on disk, which differs from the applied one until it is
/// reloaded when it was edited by hand.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct ConfigGetResult {
    /// The revision of the documents on disk, for `config.apply`.
    pub revision: ConfigRevision,
    pub documents: Vec<ConfigDocument>,
    /// Preset directories that hold no document.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub empty_presets: Vec<String>,
    pub status: ConfigStatus,
}

/// One change of a `config.apply` set.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ConfigChange {
    /// Create (no `expected_fingerprint`) or replace a document.
    Put {
        path: String,
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expected_fingerprint: Option<String>,
    },
    /// Remove a document; removing a preset's last document removes the
    /// preset.
    Delete {
        path: String,
        expected_fingerprint: String,
    },
    /// Set one key of `config.toml`, given as its path of table and key
    /// names; `null` removes it, an object is a table.
    Set {
        #[schemars(length(min = 1, max = 32))]
        key: Vec<String>,
        value: serde_json::Value,
    },
}

/// Change the configuration: the set is checked against `expected_revision`
/// and each document's fingerprint, validated whole, written, and applied.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigApplyParams {
    pub expected_revision: ConfigRevision,
    #[schemars(length(min = 1, max = 256))]
    pub changes: Vec<ConfigChange>,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct ConfigApplyResult {
    /// The revision of the documents now on disk.
    pub revision: ConfigRevision,
    pub status: ConfigStatus,
}

/// Why a `config.apply` changed nothing: the `reason` of its
/// `config_apply_refused` error. A reason this build does not know reads as
/// `invalid_change`, whose detail says what to fix.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
#[schemars(extend("x-open" = true))]
pub enum ConfigApplyErrorCode {
    /// The configuration changed since it was read: refetch and merge.
    Conflict,
    /// The result is not a valid configuration.
    InvalidConfiguration,
    /// A change cannot be made as asked, such as a path that is not a
    /// document.
    #[serde(other)]
    InvalidChange,
}
