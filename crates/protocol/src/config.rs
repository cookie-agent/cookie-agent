//! Reading and changing the daemon's configuration: `config.toml` and the
//! agent documents of its user root.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::ConfigRevision;

pub const CONFIG_RELOAD_METHOD: &str = "config.reload";

/// A configuration part that takes effect only when the daemon restarts.
#[derive(
    Clone, Copy, Debug, Deserialize, Eq, JsonSchema, Ord, PartialEq, PartialOrd, Serialize,
)]
#[serde(rename_all = "snake_case")]
pub enum ConfigRestartPart {
    Server,
    Plugins,
    Runtimes,
}

/// Where the daemon's configuration stands.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
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
}

/// Re-read the configuration from disk and apply it, for edits made by hand.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigReloadParams {}

/// The status afterwards; a configuration that failed validation is not
/// applied and its error is in `status.error`.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigReloadResult {
    pub status: ConfigStatus,
}
