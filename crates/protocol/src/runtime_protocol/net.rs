//! The `net/*` messages of forwards (runtime protocol 1.19.0,
//! `docs/site/specs/forwards.md` section 4). The daemon asks a runtime that
//! reported `net` in its `runtime/initialize` result to listen on a port or
//! connect to one, and relays the bytes between two runtimes, or between a
//! runtime and a browser, over each runtime's connection.
//!
//! Listeners and connections are numbers the runtime chooses. Bytes travel
//! as base64 in `net/data`, at most [`MAX_NET_DATA_BYTES`] at a time, and
//! each side has at most [`NET_WINDOW_BYTES`] sent on a connection and not
//! yet acknowledged with `net/ack`: it stops reading its socket until acks
//! arrive, so a slow end slows the other.

use serde::{Deserialize, Serialize};

/// The most bytes one `net/data` or `net/datagram` carries.
pub const MAX_NET_DATA_BYTES: usize = 64 * 1024;
/// The most bytes a side has sent on a connection and not had
/// acknowledged.
pub const NET_WINDOW_BYTES: u64 = 1024 * 1024;

/// A transport a forward carries.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum NetProtocol {
    Tcp,
    Udp,
}

impl NetProtocol {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Tcp => "tcp",
            Self::Udp => "udp",
        }
    }
}

/// `net/listen`: listen on `bind:port` of the runtime's machine.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, schemars::JsonSchema)]
pub struct NetListenParams {
    pub protocol: NetProtocol,
    /// An address of the machine, such as `127.0.0.1`.
    pub bind: String,
    /// `0` picks a free port.
    pub port: u16,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, schemars::JsonSchema)]
pub struct NetListenResult {
    pub listener: u64,
    /// The port it listens on.
    pub port: u16,
}

/// `net/unlisten`: stop listening. Connections it accepted stay until
/// closed.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, schemars::JsonSchema)]
pub struct NetListenerRef {
    pub listener: u64,
}

/// `net/accepted` (runtime → daemon): a TCP listener accepted a
/// connection.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, schemars::JsonSchema)]
pub struct NetAcceptedParams {
    pub listener: u64,
    pub connection: u64,
    /// The peer's address, `host:port`.
    pub peer: String,
}

/// `net/connect`: connect to `host:port` from the runtime's machine.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, schemars::JsonSchema)]
pub struct NetConnectParams {
    pub protocol: NetProtocol,
    pub host: String,
    pub port: u16,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, schemars::JsonSchema)]
pub struct NetConnectResult {
    pub connection: u64,
}

/// `net/data` (either direction): bytes of a TCP connection, in order.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, schemars::JsonSchema)]
pub struct NetDataParams {
    pub connection: u64,
    /// Base64, at most [`MAX_NET_DATA_BYTES`] decoded.
    pub data: String,
}

/// `net/ack` (either direction): bytes of `net/data` the receiver has
/// written to its socket, which the sender may send again.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, schemars::JsonSchema)]
pub struct NetAckParams {
    pub connection: u64,
    pub bytes: u64,
}

/// `net/close` (either direction): a connection's end. `eof` ends only the
/// sender's direction; anything else ends the connection.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, schemars::JsonSchema)]
pub struct NetCloseParams {
    pub connection: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// What [`NetCloseParams::reason`] says when the sender only stopped
/// writing.
pub const NET_EOF: &str = "eof";

/// `net/datagram` (either direction): one UDP datagram, of a UDP listener
/// and its `peer`, or of a UDP connection.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, schemars::JsonSchema)]
pub struct NetDatagramParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub listener: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection: Option<u64>,
    /// With a listener: the peer it came from, or goes to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peer: Option<String>,
    /// Base64, at most [`MAX_NET_DATA_BYTES`] decoded.
    pub data: String,
}
