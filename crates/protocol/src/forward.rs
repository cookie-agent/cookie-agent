//! Forwards (`docs/site/specs/forwards.md`, client protocol 1.109.0): a
//! machine's web app at the daemon's address, and a TCP or UDP port of one
//! machine on another, relayed by the daemon over each runtime's connection.
//!
//! Clients list, make, change, and delete forwards with the `forward*`
//! methods and follow `forwards.changed`; agents use the `forwards` tools.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{AgentId, SessionId};

pub const FORWARDS_LIST_METHOD: &str = "forwards.list";
pub const FORWARD_CREATE_METHOD: &str = "forward.create";
pub const FORWARD_UPDATE_METHOD: &str = "forward.update";
pub const FORWARD_DELETE_METHOD: &str = "forward.delete";
pub const FORWARDS_CHANGED_METHOD: &str = "forwards.changed";

/// How long a forward lives when its maker names nothing (decision 3).
pub const FORWARD_DEFAULT_HOURS: i64 = 24;
/// The most characters a forward's title has.
pub const FORWARD_TITLE_MAX_CHARS: usize = 200;
/// How many recent connections a forward reports.
pub const FORWARD_RECENT_MAX: usize = 50;

/// What a forward carries.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ForwardKind {
    /// A web app, served under `/p/<handle>/` on the daemon's listener.
    Http,
    Tcp,
    Udp,
}

impl ForwardKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Http => "http",
            Self::Tcp => "tcp",
            Self::Udp => "udp",
        }
    }
}

/// Where a forward's connections go: `host:port` reached from `machine`.
#[derive(Clone, Debug, Deserialize, Eq, Hash, JsonSchema, PartialEq, Serialize)]
pub struct ForwardTarget {
    /// A machine, by the name the machines block and the clients show.
    pub machine: String,
    pub host: String,
    pub port: u16,
}

/// Where a TCP or UDP forward listens: `bind:port` of `machine`.
#[derive(Clone, Debug, Deserialize, Eq, Hash, JsonSchema, PartialEq, Serialize)]
pub struct ForwardListen {
    pub machine: String,
    pub bind: String,
    /// `0` picks a free port, which [`Forward::address`] then names.
    pub port: u16,
}

/// How a forward is doing now.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ForwardStatus {
    Active,
    /// A machine it needs is not connected; it starts when it is.
    Waiting,
    /// Listening failed, such as on a port in use; `reason` says why.
    Failed,
    Expired,
}

/// Who made a forward, and when.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct ForwardStamp {
    pub at: jiff::Timestamp,
    /// The session whose call made it; none for the user's own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<SessionId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<AgentId>,
}

/// One connection a forward carried, or carries.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct ForwardConnection {
    pub at: jiff::Timestamp,
    /// The address it came from: a browser's for `http`, or the peer the
    /// listening machine saw.
    pub peer: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<jiff::Timestamp>,
    pub bytes_in: u64,
    pub bytes_out: u64,
    /// How it ended: closed, or why it failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended: Option<String>,
}

/// A forward as it is now (section 3).
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct Forward {
    pub id: String,
    /// `fw_` and 8 hex digits: what the model and the URL name it by.
    pub handle: String,
    pub kind: ForwardKind,
    pub to: ForwardTarget,
    /// Where a TCP or UDP forward listens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<ForwardListen>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// The root session whose tree made it; none for the user's own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<SessionId>,
    pub created: ForwardStamp,
    /// When it ends; none for never.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<jiff::Timestamp>,
    pub status: ForwardStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Where it is reached: an `http` forward's path from the daemon's root
    /// (`p/<handle>/`), or the `bind:port` a TCP or UDP forward listens on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
    pub connections_open: u64,
    pub connections_total: u64,
    pub bytes_in: u64,
    pub bytes_out: u64,
    /// The latest connections, newest first, at most
    /// [`FORWARD_RECENT_MAX`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recent: Vec<ForwardConnection>,
}

impl Forward {
    /// `http → labpc 127.0.0.1:5173`, or `tcp labserver 127.0.0.1:15432 →
    /// labpc 127.0.0.1:5432`.
    #[must_use]
    pub fn route(&self) -> String {
        let to = format!("{} {}:{}", self.to.machine, self.to.host, self.to.port);
        match &self.from {
            Some(from) => {
                // A port of 0 is the one the listener picked, once it has.
                let listen = match &self.address {
                    Some(address) if from.port == 0 => address.clone(),
                    _ => format!("{}:{}", from.bind, from.port),
                };
                format!("{} {} {listen} → {to}", self.kind.as_str(), from.machine)
            }
            None => format!("{} → {to}", self.kind.as_str()),
        }
    }
}

/// A forward as an attachment showed it (`<attach forward/>`, client
/// protocol 1.110.0): which one, and what it was then. Clients show it as
/// it is now.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct ForwardBlock {
    pub handle: String,
    pub kind: ForwardKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// [`Forward::route`] then.
    pub route: String,
}

impl Forward {
    /// The forward as an attachment shows it.
    #[must_use]
    pub fn block(&self) -> ForwardBlock {
        ForwardBlock {
            handle: self.handle.clone(),
            kind: self.kind,
            title: self.title.clone(),
            route: self.route(),
        }
    }
}

/// `forwards.list`: every forward, or a session tree's.
#[derive(Clone, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ForwardsListParams {
    /// Only the forwards of this session's tree.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<SessionId>,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct ForwardsListResult {
    pub forwards: Vec<Forward>,
}

/// `forward.create`: the user makes a forward, which belongs to no session.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ForwardCreateParams {
    pub kind: ForwardKind,
    pub to: ForwardTarget,
    /// For `tcp` and `udp`: where it listens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<ForwardListen>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Hours it lives; default 24; `0` or less never ends.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_in_hours: Option<i64>,
}

/// `forward.update`: a new title, or a new expiry counted from now.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ForwardUpdateParams {
    /// Its handle.
    pub forward: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Hours from now; `0` or less never ends.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_in_hours: Option<i64>,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct ForwardResult {
    pub forward: Forward,
}

/// `forward.delete`.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ForwardDeleteParams {
    pub forward: String,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct ForwardDeleteResult {}

/// `forwards.changed`: forwards were made, changed, deleted, or their
/// status or counts moved; read them again with `forwards.list`.
#[derive(Clone, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct ForwardsChangedNotification {}
