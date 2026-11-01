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
        /// Once no run is active in any session and no background call is
        /// pending. Runs keep starting meanwhile, and the restart waits for
        /// them too; only once it is due are new runs refused.
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
    /// The background calls still running when the daemon answered, which
    /// `idle` waits for too and `now` ends (client protocol 1.35.0; absent
    /// from older daemons, which did not wait for them).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub background_calls: u32,
}

/// A restart is scheduled, sent to every client when it is and again as the
/// wait progresses.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct DaemonRestartingNotification {
    pub when: RestartWhen,
    /// The runs still active.
    pub active_runs: u32,
    /// The background calls still running (client protocol 1.35.0).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub background_calls: u32,
    /// What the restart waits for or is doing, for people to read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[allow(clippy::trivially_copy_pass_by_ref)]
const fn is_zero(count: &u32) -> bool {
    *count == 0
}

// The daemon's status (`docs/site/specs/daemon-startup.md`). Client
// protocol 1.65.0.

pub const DAEMON_STATUS_METHOD: &str = "daemon.status";

/// The first client protocol that reads [`DaemonStatus`]: the daemon answers
/// its handshake while it starts, sends it `daemon.status`, and no longer
/// `daemon.restarting`.
pub const DAEMON_STATUS_SINCE: crate::SemVer = crate::SemVer::new(1, 65, 0);

/// Whether a client of protocol `version` reads [`DaemonStatus`].
#[must_use]
pub fn reads_daemon_status(version: crate::SemVer) -> bool {
    version >= DAEMON_STATUS_SINCE
}

crate::open_enum! {
    /// What the daemon accepts now.
    #[derive(Clone, Copy, Debug, Default, Deserialize, Eq, Hash, JsonSchema, PartialEq, Serialize)]
    #[serde(rename_all = "snake_case")]
    pub enum DaemonPhase {
        /// Starting: only `handshake` and `daemon.status` are answered; any
        /// other request is refused with `daemon_starting`.
        Starting,
        /// Everything works, whatever tasks are running.
        #[default]
        Ready,
        /// Going down, for a stop or a restart that is due: new runs are
        /// refused, and connections close soon.
        Stopping,
        /// Startup failed, as `error` says; the daemon exits.
        Failed,
    }
}

/// The daemon's phase and what it is busy with: the `daemon.status` result,
/// the `daemon.status` notification, and [`crate::ServerHello::status`].
#[derive(Clone, Debug, Default, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct DaemonStatus {
    pub phase: DaemonPhase,
    /// In phase `stopping`: the daemon relaunches.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub restart: bool,
    /// What the daemon is doing that a user would want to see, oldest
    /// first. Free-form: clients show any task the same way.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tasks: Vec<DaemonTask>,
    /// In phase `failed`: why, as a registry error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<crate::JsonRpcError>,
}

impl DaemonStatus {
    /// The status of a daemon that is ready and busy with nothing.
    #[must_use]
    pub fn ready() -> Self {
        Self::default()
    }

    /// The tasks the daemon is not ready until they end.
    pub fn blocking(&self) -> impl Iterator<Item = &DaemonTask> {
        self.tasks.iter().filter(|task| task.blocking)
    }
}

/// One thing the daemon is doing.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct DaemonTask {
    /// Stable for the task's life; opaque.
    pub id: String,
    /// What it is, for people: "Moving sessions to the new storage layout".
    pub label: String,
    /// What it is on now, for people.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress: Option<TaskProgress>,
    /// The daemon is not ready until this task ends.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub blocking: bool,
    pub started_at: jiff::Timestamp,
}

/// How far a task is.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct TaskProgress {
    pub done: u64,
    /// Absent while the task does not know how much there is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total: Option<u64>,
    /// What `done` and `total` count, for people: "sessions".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
}

/// `daemon.status` takes no parameters.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DaemonStatusParams {}
