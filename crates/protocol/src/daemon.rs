//! Restarting the daemon from a client (`docs/site/specs/engine-configuration.md`
//! section 11). Client protocol 1.19.0.
//!
//! A client asks with `daemon.restart`; the daemon tells every client with
//! `daemon.restarting` while the restart waits and then closes their
//! connections with WebSocket close code [`RESTART_CLOSE_CODE`], and they
//! reconnect as after any dropped connection.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub const DAEMON_RESTART_METHOD: &str = "daemon.restart";
pub const DAEMON_RESTARTING_METHOD: &str = "daemon.restarting";

/// The WebSocket close code a restarting daemon closes client connections
/// with: 1012, Service Restart.
pub const RESTART_CLOSE_CODE: u16 = 1012;
/// The close reason that goes with [`RESTART_CLOSE_CODE`].
pub const RESTART_CLOSE_REASON: &str = "restarting";

crate::open_enum! {
    /// When the daemon restarts.
    #[derive(Clone, Copy, Debug, Default, Deserialize, Eq, Hash, JsonSchema, PartialEq, Serialize)]
    #[serde(rename_all = "snake_case")]
    pub enum RestartWhen {
        /// Once no run is active in any session. New runs are refused
        /// meanwhile.
        #[default]
        Idle,
        /// At once: running turns end as interrupted.
        Now,
    }
}

/// Restart the daemon: it relaunches its own executable with the same
/// arguments and environment. A second call while a restart is scheduled
/// changes its `when`; `now` wins over `idle`.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DaemonRestartParams {
    #[serde(default)]
    pub when: RestartWhen,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct DaemonRestartResult {
    /// Whether this call scheduled the restart; `false` when one was already
    /// scheduled and this call only changed its `when`.
    pub scheduled: bool,
    /// The runs active when the daemon answered: what `idle` waits for and
    /// what `now` interrupts.
    pub active_runs: u32,
}

/// A restart is scheduled, sent to every client when it is and again as the
/// wait progresses.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct DaemonRestartingNotification {
    pub when: RestartWhen,
    /// The runs still active.
    pub active_runs: u32,
    /// What the restart waits for or is doing, for people to read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}
