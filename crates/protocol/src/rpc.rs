use std::{borrow::Cow, collections::BTreeMap, fmt};

use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::*;

pub const SESSION_GOAL_GET_METHOD: &str = "session.goal.get";
pub const SESSION_GOAL_SET_METHOD: &str = "session.goal.set";
pub const SESSION_GOAL_LIFECYCLE_METHOD: &str = "session.goal.lifecycle";
pub const SESSION_PRODUCERS_METHOD: &str = "session.producers";

#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub struct JsonRpcVersion(());
impl JsonRpcVersion {
    #[must_use]
    pub const fn current() -> Self {
        Self(())
    }
}
impl Serialize for JsonRpcVersion {
    fn serialize<S>(&self, s: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        s.serialize_str("2.0")
    }
}
impl<'de> Deserialize<'de> for JsonRpcVersion {
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(d)?;
        if value == "2.0" {
            Ok(Self::current())
        } else {
            Err(serde::de::Error::custom(
                "JSON-RPC version must be exactly 2.0",
            ))
        }
    }
}
impl JsonSchema for JsonRpcVersion {
    fn inline_schema() -> bool {
        true
    }
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("JsonRpcVersion")
    }
    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({"type":"string","const":"2.0"})
    }
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(untagged)]
pub enum JsonRpcId {
    Null,
    Number(i64),
    String(String),
}
/// A JSON-RPC request. The envelope is information on both sides: members
/// it does not name are ignored.
#[derive(Clone, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct Request {
    pub jsonrpc: JsonRpcVersion,
    pub id: JsonRpcId,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}
impl Request {
    #[must_use]
    pub fn new(id: JsonRpcId, method: impl Into<String>, params: Option<Value>) -> Self {
        Self {
            jsonrpc: JsonRpcVersion::current(),
            id,
            method: method.into(),
            params,
        }
    }
}
impl fmt::Debug for Request {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Request")
            .field("jsonrpc", &self.jsonrpc)
            .field("id", &self.id)
            .field("method", &self.method)
            .field("params", &self.params.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}
#[derive(Clone, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct SuccessResponse {
    pub jsonrpc: JsonRpcVersion,
    pub id: JsonRpcId,
    pub result: Value,
}
impl fmt::Debug for SuccessResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SuccessResponse")
            .field("jsonrpc", &self.jsonrpc)
            .field("id", &self.id)
            .field("result", &"<redacted>")
            .finish()
    }
}
#[derive(Clone, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct JsonRpcError {
    pub code: i32,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}
impl fmt::Debug for JsonRpcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JsonRpcError")
            .field("code", &self.code)
            .field("message", &self.message)
            .field("data", &self.data.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct ErrorResponse {
    pub jsonrpc: JsonRpcVersion,
    pub id: JsonRpcId,
    pub error: JsonRpcError,
}
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(untagged)]
pub enum Response {
    Success(SuccessResponse),
    Error(ErrorResponse),
}
#[derive(Clone, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct Notification {
    pub jsonrpc: JsonRpcVersion,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}
impl Notification {
    #[must_use]
    pub fn new(method: impl Into<String>, params: Option<Value>) -> Self {
        Self {
            jsonrpc: JsonRpcVersion::current(),
            method: method.into(),
            params,
        }
    }
}
/// A notification that borrows its params, serialized straight to text
/// without first building a [`Value`] tree.
#[derive(Serialize)]
pub struct NotificationRef<'a, P: ?Sized> {
    pub jsonrpc: JsonRpcVersion,
    pub method: &'a str,
    pub params: &'a P,
}
impl<'a, P: Serialize + ?Sized> NotificationRef<'a, P> {
    #[must_use]
    pub const fn new(method: &'a str, params: &'a P) -> Self {
        Self {
            jsonrpc: JsonRpcVersion::current(),
            method,
            params,
        }
    }

    /// The notification's JSON text.
    pub fn to_text(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }
}
impl fmt::Debug for Notification {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Notification")
            .field("jsonrpc", &self.jsonrpc)
            .field("method", &self.method)
            .field("params", &self.params.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

/// A self-declared kind label: a lowercase ASCII letter followed by up to 15
/// lowercase letters, digits, or hyphens. The daemon only displays it and
/// never branches on it.
macro_rules! kind_label {
    ($(#[$meta:meta])* $name:ident, $what:literal, $description:literal) => {
        $(#[$meta])*
        #[derive(Clone, Debug, Eq, Hash, PartialEq)]
        pub struct $name(String);
        impl $name {
            pub const MAX_LEN: usize = 16;

            pub fn new(value: impl Into<String>) -> Result<Self, WireStringError> {
                let value = value.into();
                let valid = value.len() <= Self::MAX_LEN
                    && value.bytes().enumerate().all(|(index, byte)| {
                        byte.is_ascii_lowercase()
                            || (index > 0 && (byte.is_ascii_digit() || byte == b'-'))
                    })
                    && !value.is_empty();
                if valid {
                    Ok(Self(value))
                } else {
                    Err(WireStringError::Invalid($what))
                }
            }

            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }
        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(&self.0)
            }
        }
        impl Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: serde::Serializer,
            {
                serializer.serialize_str(&self.0)
            }
        }
        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: serde::Deserializer<'de>,
            {
                Self::new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
            }
        }
        impl JsonSchema for $name {
            fn inline_schema() -> bool {
                true
            }
            fn schema_name() -> Cow<'static, str> {
                Cow::Borrowed(stringify!($name))
            }
            fn json_schema(_: &mut SchemaGenerator) -> Schema {
                json_schema!({
                    "type":"string",
                    "pattern":"^[a-z][a-z0-9-]{0,15}$",
                    "description":$description
                })
            }
        }
    };
}

kind_label!(
    /// The kind of client on a connection, such as `tui` or `cli`.
    ClientKind,
    "client kind",
    "Client kind, such as `tui` or `cli`."
);
kind_label!(
    /// The kind a runtime declares for itself, such as `coding` or `global`.
    RuntimeKind,
    "runtime kind",
    "Runtime kind the runtime declares, such as `coding` or `global`."
);

/// Who is on a connection. The kind names the client program; the instance
/// identifies one running client and stays the same across its reconnects,
/// so it can recognize events it caused.
#[derive(Clone, Debug, Deserialize, Eq, Hash, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ClientIdentity {
    pub kind: ClientKind,
    pub instance_id: ClientInstanceId,
}
impl ClientIdentity {
    /// A new identity with a fresh instance id.
    #[must_use]
    pub fn generate(kind: ClientKind) -> Self {
        Self {
            kind,
            instance_id: ClientInstanceId::new_v7(),
        }
    }

    /// The origin stamped on events this client causes:
    /// `client:<kind>-<instance id as 32 hex digits>`.
    #[must_use]
    pub fn origin(&self) -> EventOrigin {
        EventOrigin::new(format!(
            "client:{}-{}",
            self.kind.as_str(),
            self.instance_id.0.simple()
        ))
        .expect("a client kind and a hex instance id form a valid origin")
    }
}

/// `handshake` params: the client's version and identity, and the daemon's
/// token when the connection did not present it at the WebSocket upgrade.
#[derive(Clone, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ClientHello {
    /// The client's [`crate::CLIENT_PROTOCOL_VERSION`].
    pub protocol_version: SemVer,
    pub client: ClientIdentity,
    /// The daemon's bearer token. A browser cannot set `Authorization` on a
    /// WebSocket, so it authenticates here; a client that sent the header
    /// may leave it out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<HandshakeToken>,
    /// What the client's configuration enables. None are defined yet; the
    /// daemon ignores one it does not know.
    #[serde(default)]
    pub features: Vec<String>,
}
impl ClientHello {
    /// The hello of this build's protocol version, without a token or
    /// features.
    #[must_use]
    pub const fn new(client: ClientIdentity) -> Self {
        Self {
            protocol_version: CLIENT_PROTOCOL_VERSION,
            client,
            token: None,
            features: Vec::new(),
        }
    }
}
impl fmt::Debug for ClientHello {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ClientHello")
            .field("protocol_version", &self.protocol_version)
            .field("client", &self.client)
            .field("token", &self.token.as_ref().map(|_| "<redacted>"))
            .field("features", &self.features)
            .finish()
    }
}

/// The daemon's bearer token as a handshake carries it. It is wiped when
/// dropped and never printed.
#[derive(Clone, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(transparent)]
pub struct HandshakeToken(#[schemars(length(max = 256))] String);
impl HandshakeToken {
    #[must_use]
    pub fn new(token: impl Into<String>) -> Self {
        Self(token.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl Drop for HandshakeToken {
    fn drop(&mut self) {
        zeroize::Zeroize::zeroize(&mut self.0);
    }
}

/// The `handshake` result.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct ServerHello {
    /// The daemon's [`crate::CLIENT_PROTOCOL_VERSION`]. The client sends a
    /// method or member only if this version includes it.
    pub protocol_version: SemVer,
    /// The daemon's own version, for display and logs.
    pub engine_version: String,
    /// The origin the daemon stamps on events this connection causes.
    pub origin: EventOrigin,
    /// An opaque identity of the daemon's machine
    /// (`paths::machine_id`), so a client can tell whether it runs on the
    /// same machine; absent when the daemon cannot tell.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine_id: Option<String>,
    /// What the daemon's configuration enables for clients. None are defined
    /// yet; a client ignores one it does not know.
    #[serde(default)]
    pub features: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionCreateParams {
    pub selection: RunSelection,
    /// The new tree's permission mode; the default mode when omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<PermissionMode>,
    /// The session's initial permission overlay, committed with its
    /// creation; empty when omitted.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 256))]
    pub permission_rules: Vec<PermissionRule>,
    /// The runtimes the session attaches besides `global`, in order. When
    /// omitted, the session attaches `local`'s `coding` capability at `cwd`,
    /// or at the daemon's working directory. Instances that are not running are started first; a
    /// failure creates no session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = 256))]
    pub runtimes: Option<Vec<RuntimeAttachRequest>>,
    /// Whether the session attaches the global runtime; `true` when omitted.
    #[serde(default = "attach_global", skip_serializing_if = "attaches_global")]
    pub global: bool,
    /// The client's directory, as the client names it: the workspace of
    /// `local`'s `coding` capability when `runtimes` is omitted. The
    /// session's `cwd_identity` is the canonical workspace its first
    /// attachment with a workspace reports; only a session without one takes
    /// this directory instead, canonicalized on the daemon's machine (as
    /// given when that fails). The daemon's own directory when omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// The project the session belongs to (client protocol 1.8.0); an
    /// unknown ID is refused before anything is attached. When `runtimes`
    /// is omitted, the session attaches the project's default `global` and
    /// `runtimes` instead of the usual ones, if it has defaults.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<ProjectId>,
    /// Values for the config options of the session's ACP agent (client
    /// protocol 1.26.0), such as its model and effort, which its new ACP
    /// sessions start with, on top of the agent document's `acp.config`.
    /// The options are the agent's `AgentRuntime.options`; one the agent
    /// does not offer when its session starts is skipped with a
    /// diagnostic. Refused for an agent that is not an ACP agent.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 64))]
    pub agent_options: Vec<crate::AcpOptionChoice>,
}

const fn attach_global() -> bool {
    true
}

#[allow(clippy::trivially_copy_pass_by_ref)]
const fn attaches_global(global: &bool) -> bool {
    *global
}

impl SessionCreateParams {
    /// Create a session with the default mode, no overlay rules, and the
    /// default `global` and `local` runtimes.
    #[must_use]
    pub const fn new(selection: RunSelection) -> Self {
        Self {
            selection,
            permission_mode: None,
            permission_rules: Vec::new(),
            runtimes: None,
            global: true,
            cwd: None,
            project: None,
            agent_options: Vec::new(),
        }
    }
}

/// One runtime a session attaches.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeAttachRequest {
    pub runtime_id: RuntimeId,
    /// The capability to attach; the runtime's only capability when
    /// omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capability: Option<CapabilityName>,
    /// Required exactly when the capability takes a workspace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct SessionCreateResult {
    pub session: SessionMeta,
    /// The session's runtime attachments, in attach order.
    pub runtimes: Vec<RuntimeAttachment>,
}

pub const SESSION_RUNTIME_ATTACH_METHOD: &str = "session.runtime.attach";
pub const SESSION_RUNTIME_DETACH_METHOD: &str = "session.runtime.detach";
pub const RUNTIMES_LIST_METHOD: &str = "runtimes.list";
pub const RUNTIMES_CHANGED_METHOD: &str = "runtimes.changed";
pub const RUNTIMES_RECONNECT_METHOD: &str = "runtimes.reconnect";
pub const MACHINES_LIST_METHOD: &str = "machines.list";
pub const MACHINES_RENAME_METHOD: &str = "machines.rename";
pub const MACHINES_ASSIGN_METHOD: &str = "machines.assign";
pub const MACHINES_UNASSIGN_METHOD: &str = "machines.unassign";
pub const MACHINES_MERGE_METHOD: &str = "machines.merge";
pub const MACHINES_FORGET_METHOD: &str = "machines.forget";
pub const MACHINES_FORGET_RUNTIME_METHOD: &str = "machines.forget_runtime";
pub const MACHINES_CHANGED_METHOD: &str = "machines.changed";
pub const MEMORY_LIST_METHOD: &str = "memory.list";
pub const MEMORY_READ_METHOD: &str = "memory.read";
pub const MEMORY_WRITE_METHOD: &str = "memory.write";
pub const MEMORY_DELETE_METHOD: &str = "memory.delete";
pub const MEMORY_CHANGED_METHOD: &str = "memory.changed";
pub const PLUGINS_LIST_METHOD: &str = "plugins.list";
pub const PLUGINS_RESTART_METHOD: &str = "plugins.restart";
pub const PLUGINS_CHANGED_METHOD: &str = "plugins.changed";
pub const RUN_TOOL_BACKGROUND_METHOD: &str = "run.tool_background";
pub const SESSION_BACKGROUND_LIST_METHOD: &str = "session.background.list";
pub const SESSION_BACKGROUND_STOP_METHOD: &str = "session.background.stop";
pub const BACKGROUND_CHANGED_METHOD: &str = "background.changed";

/// Move an executing call of a background-capable tool to the background,
/// as `docs/site/specs/background-tools.md` section 4 specifies. The call
/// detaches once it reaches its detach point.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RunToolBackgroundParams {
    pub run_id: RunId,
    pub call_id: ToolCallId,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct RunToolBackgroundResult {
    /// False when the call is not an executing call of a background-capable
    /// tool.
    pub accepted: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionBackgroundListParams {
    pub session_id: SessionId,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct SessionBackgroundListResult {
    pub calls: Vec<BackgroundCall>,
}

/// Stop a background call as the user. The call still reports to the model.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionBackgroundStopParams {
    pub session_id: SessionId,
    pub job: String,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct SessionBackgroundStopResult {}

/// A session's background calls changed.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct BackgroundChangedNotification {
    pub session_id: SessionId,
    /// The session's background calls now.
    pub calls: Vec<BackgroundCall>,
}

/// One call running in the background.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct BackgroundCall {
    pub job: String,
    /// The tool's name as the model called it.
    pub tool: String,
    pub tool_call_id: ToolCallId,
    /// The call's display argument.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display: Option<String>,
    pub started_at: jiff::Timestamp,
    pub detached_at: jiff::Timestamp,
    pub by: BackgroundActor,
}

/// Who moved a call to the background.
///
/// A value this build does not know reads as `model`.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
#[schemars(extend("x-open" = true))]
pub enum BackgroundActor {
    User,
    #[serde(other)]
    Model,
}

/// Attach a runtime to a session that has no active run. The instance is
/// started first when it is not running; a failure records nothing.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionRuntimeAttachParams {
    pub session_id: SessionId,
    pub runtime_id: RuntimeId,
    /// The capability to attach; the runtime's only capability when
    /// omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capability: Option<CapabilityName>,
    /// Required exactly when the capability takes a workspace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct SessionRuntimeAttachResult {
    /// The new attachment, under the name the session gave it.
    pub attachment: RuntimeAttachment,
    /// Every attachment of the session afterwards, in attach order.
    pub runtimes: Vec<RuntimeAttachment>,
}

/// Remove an attachment from a session that has no active run.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionRuntimeDetachParams {
    pub session_id: SessionId,
    pub name: AttachmentName,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct SessionRuntimeDetachResult {
    /// Every attachment of the session afterwards, in attach order.
    pub runtimes: Vec<RuntimeAttachment>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimesListParams {}

/// The runtimes sessions can attach and the capability instances this
/// daemon has opened.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct RuntimesListResult {
    pub runtimes: Vec<RuntimeListing>,
    pub instances: Vec<RuntimeInstanceInfo>,
}

/// Retry a runtime instance now: a failed instance is started again, under a
/// new generation; a ready or reconnecting one is left as it is, and one
/// that is not running is started.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimesReconnectParams {
    pub runtime_id: RuntimeId,
    /// The instance's capability; the runtime's only capability when
    /// omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capability: Option<CapabilityName>,
    /// Required exactly when the capability takes a workspace: the
    /// instance's workspace as `runtimes.list` reports it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct RuntimesReconnectResult {
    /// The instance afterwards.
    pub instance: RuntimeInstanceInfo,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct RuntimeListing {
    pub runtime_id: RuntimeId,
    pub lifetime: RuntimeLifetime,
    /// Whether the daemon holds a connection to the runtime now.
    pub connected: bool,
    /// What the runtime offers; empty until it has connected once.
    pub capabilities: Vec<RuntimeCapabilityInfo>,
    /// The runtime's operating system, once it has connected and said.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
    /// The machine the runtime belongs to: the one it is assigned to while
    /// it is connected, else the one it is pinned to or the daemon last saw
    /// it on (client protocol 1.10.0: remembered across restarts). Absent
    /// while the daemon cannot tell.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine: Option<MachineName>,
    /// The home directory the runtime reported when it last connected, as a
    /// canonical absolute path on its machine (client protocol 1.7.0).
    /// Clients show the paths of the runtime's attachments under it as `~/…`;
    /// without one they show them absolute.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub home: Option<String>,
    /// Why the last attempt to connect failed, while it is not connected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MachinesListParams {}

/// Every machine of the daemon's machine registry, in name order. Each
/// `machines.*` change answers with the machines afterwards.
#[derive(Clone, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct MachinesListResult {
    pub machines: Vec<MachineInfo>,
}

/// One machine: a computer that runtimes run on.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct MachineInfo {
    pub name: MachineName,
    /// The opaque machine IDs the machine stands for: a runtime that
    /// reports one of them belongs to it.
    pub ids: Vec<String>,
    /// Runtime IDs pinned to the machine, whatever they report.
    pub pinned: Vec<RuntimeId>,
    /// The connected runtimes assigned to the machine, by runtime ID.
    pub connected: Vec<MachineRuntime>,
    /// The runtimes not connected now that the daemon remembers on the
    /// machine, by runtime ID (client protocol 1.10.0). Empty from an older
    /// daemon.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remembered: Vec<RememberedRuntime>,
    /// The operating system its connected runtimes report, when they agree.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
    /// The memory files in the machine's memory folder.
    pub memories: u64,
}

/// A connected runtime assigned to a machine.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct MachineRuntime {
    pub runtime_id: RuntimeId,
    pub assigned_by: MachineAssignment,
    /// The operating system the runtime reports.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
}

/// A runtime the daemon remembers on a machine while it is not connected
/// (client protocol 1.10.0): one that connected before, whose machine ID,
/// platform, and home the registry recorded.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct RememberedRuntime {
    pub runtime_id: RuntimeId,
    /// `pinned` when the machine pins the runtime ID, else `last_seen`.
    pub assigned_by: MachineAssignment,
    /// The operating system the runtime reported when it last connected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
    /// The home directory the runtime reported when it last connected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub home: Option<String>,
    /// When the daemon last saw the runtime connect or disconnect.
    pub last_seen: jiff::Timestamp,
}

crate::open_enum! {
    /// Why a runtime belongs to its machine.
    #[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, JsonSchema, PartialEq, Serialize)]
    #[serde(rename_all = "snake_case")]
    pub enum MachineAssignment {
        /// The machine pins the runtime ID.
        Pinned,
        /// The machine stands for the machine ID the runtime reports.
        MachineId,
        /// The runtime is not connected, and the machine stands for the
        /// machine ID it reported when the daemon last saw it (client
        /// protocol 1.10.0).
        LastSeen,
    }
}

/// Rename a machine; its memory folder moves with it.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MachinesRenameParams {
    pub machine: MachineName,
    pub name: MachineName,
}

/// Pin a runtime ID to a machine, created when the name is new, and remove
/// its pin from any other machine.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MachinesAssignParams {
    pub runtime: RuntimeId,
    pub machine: MachineName,
}

/// Remove a runtime's pin, so that it belongs to the machine of the machine
/// ID it reports.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MachinesUnassignParams {
    pub runtime: RuntimeId,
}

/// Move one machine's IDs, pins, and memories into another and remove it.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MachinesMergeParams {
    pub from: MachineName,
    pub into: MachineName,
}

/// Remove a machine that has no connected runtimes and no memories. The
/// records of runtimes remembered on it go with it.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MachinesForgetParams {
    pub machine: MachineName,
}

/// Drop the record of a runtime that is not connected, so the daemon no
/// longer remembers where it ran (client protocol 1.10.0). A pin stays.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MachinesForgetRuntimeParams {
    pub runtime: RuntimeId,
}

/// The machine registry changed, or a runtime was assigned to a new
/// machine; refetch `machines.list` and `runtimes.list`.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct MachinesChangedNotification {}

/// How the daemon reaches a runtime.
///
/// A value this build does not know reads as `spawned`.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
#[schemars(extend("x-open" = true))]
pub enum RuntimeLifetime {
    /// Built into the daemon: `global` and `local`.
    Builtin,
    /// Connected to the daemon on its own: a persistent runtime, or one a
    /// client runs for as long as it does.
    DialIn,
    /// Started by the daemon from a `[runtimes.<id>]` template.
    #[serde(other)]
    Spawned,
}

/// One capability a runtime offers.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct RuntimeCapabilityInfo {
    pub name: CapabilityName,
    /// The capability's self-declared kind label.
    pub kind: RuntimeKind,
    /// Whether attaching it requires a workspace.
    pub takes_workspace: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct RuntimeInstanceInfo {
    pub runtime_id: RuntimeId,
    pub capability: CapabilityName,
    /// The name the instance reports; absent until it has started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attachment_name: Option<AttachmentName>,
    /// The kind the instance reports; absent until it has started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<RuntimeKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
    pub status: RuntimeInstanceStatus,
    /// Why the instance failed, when it did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

crate::open_enum! {
    #[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, JsonSchema, PartialEq, Serialize)]
    #[serde(rename_all = "snake_case")]
    pub enum RuntimeInstanceStatus {
        Starting,
        Ready,
        Reconnecting,
        Failed,
        Stopped,
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PluginsListParams {}

/// The configured plugins, in the order their hooks run.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct PluginsListResult {
    pub plugins: Vec<PluginInfo>,
}

/// Start a plugin again now: a failed or stopped plugin restarts, and a
/// connected one is stopped and restarted.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PluginsRestartParams {
    pub name: String,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct PluginsRestartResult {}

/// A plugin's status changed; refetch `plugins.list`.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct PluginsChangedNotification {}

/// One configured plugin as clients see it.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct PluginInfo {
    pub name: String,
    pub state: PluginState,
    /// Why the plugin failed or last disconnected, or what it could not do.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// The names its tools are declared under.
    pub tools: Vec<String>,
    /// The hooks it intercepts, in no particular order.
    pub hooks: Vec<crate::HookName>,
    /// The event types it subscribes to; empty for none.
    pub subscribe_events: Vec<crate::EventPattern>,
    pub subscribe_bus: bool,
    pub publish_bus: bool,
    /// Its producer messaging is open: the plugin declares it and its
    /// configuration enables it.
    pub producer_messaging: bool,
    /// Events and bus notifications dropped because it fell behind.
    pub dropped_events: u64,
    /// What the plugin last wrote to standard error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stderr: Option<String>,
}

crate::open_enum! {
    #[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, JsonSchema, PartialEq, Serialize)]
    #[serde(rename_all = "snake_case")]
    pub enum PluginState {
        /// Configured with `enabled = false`.
        Disabled,
        Starting,
        Connected,
        /// The connection dropped and the daemon is restarting it.
        Reconnecting,
        /// Every start attempt failed; `plugins.restart` tries again.
        Failed,
        /// Stopped with the daemon.
        Stopped,
    }
}

/// A runtime template was registered or an instance's status changed;
/// refetch `runtimes.list`.
#[derive(Clone, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct RuntimesChangedNotification {
    /// Instances that failed to start, or dropped and did not reconnect,
    /// that sessions this connection subscribes to are attached to. Absent
    /// when the change carries no failure.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failures: Vec<RuntimeFailure>,
}

/// A runtime instance that is unavailable to the sessions attached to it.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct RuntimeFailure {
    pub runtime_id: RuntimeId,
    pub capability: CapabilityName,
    /// The instance's workspace, for a template that takes one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    pub reason: String,
    /// The affected sessions among those this connection subscribes to.
    pub session_ids: Vec<SessionId>,
}
#[derive(Clone, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionListParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd_identity: Option<CwdIdentity>,
    /// Only the sessions of one project, or with `"none"` those in no
    /// project (client protocol 1.8.0).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<ProjectFilter>,
}
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct SessionListResult {
    pub sessions: Vec<SessionMeta>,
}
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionGetParams {
    pub session_id: SessionId,
}
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct SessionGetResult {
    pub session: SessionMeta,
    /// The session's runtime attachments, in attach order.
    pub runtimes: Vec<RuntimeAttachment>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionGoalGetParams {
    pub session_id: SessionId,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct SessionGoalGetResult {
    #[serde(deserialize_with = "crate::deserialize_required_option")]
    #[schemars(with = "crate::NullableSchema<GoalState>", required)]
    pub goal: Option<GoalState>,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionGoalSetParams {
    pub session_id: SessionId,
    pub objective: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selection: Option<RunSelection>,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct SessionGoalSetResult {
    pub goal: GoalState,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionGoalLifecycleParams {
    pub session_id: SessionId,
    pub goal_id: GoalId,
    pub expected_revision: u64,
    pub action: GoalLifecycleAction,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selection: Option<RunSelection>,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct SessionGoalLifecycleResult {
    pub goal: GoalState,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionProducersParams {
    pub session_id: SessionId,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct SessionProducersResult {
    pub producers: Vec<ProducerRegistration>,
    /// The recovery state of every producer source the session can hear from.
    pub sources: Vec<ProducerSourceState>,
}

#[derive(Clone, Debug, Default, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct UsageRollup {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub request_count: u64,
    #[serde(deserialize_with = "deserialize_required_option")]
    #[schemars(with = "crate::NullableSchema<f64>", required)]
    pub cache_hit_rate: Option<f64>,
    #[serde(deserialize_with = "deserialize_required_option")]
    #[schemars(with = "crate::NullableSchema<f64>", required)]
    pub estimated_cost_usd: Option<f64>,
    pub by_model: BTreeMap<ModelKey, ModelUsageRollup>,
    #[serde(skip)]
    #[schemars(skip)]
    pub arithmetic_overflow: bool,
}

#[derive(Clone, Debug, Default, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct ModelUsageRollup {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub request_count: u64,
    #[serde(deserialize_with = "deserialize_required_option")]
    #[schemars(with = "crate::NullableSchema<f64>", required)]
    pub cache_hit_rate: Option<f64>,
    #[serde(deserialize_with = "deserialize_required_option")]
    #[schemars(with = "crate::NullableSchema<f64>", required)]
    pub estimated_cost_usd: Option<f64>,
    #[serde(skip)]
    #[schemars(skip)]
    pub observations: Vec<Usage>,
    #[serde(skip)]
    #[schemars(skip)]
    pub cost_provenance: Vec<UsageCostProvenance>,
    #[serde(skip)]
    #[schemars(skip)]
    pub arithmetic_overflow: bool,
}

#[doc(hidden)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UsageCostProvenance {
    Stamped(Option<u64>),
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionUsageParams {
    pub session_id: SessionId,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct SessionUsageResult {
    pub session_id: SessionId,
    pub usage: UsageRollup,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct SessionTreeUsageResult {
    pub session_id: SessionId,
    pub usage: UsageRollup,
    pub session_count: u64,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct ChildSummary {
    pub session_id: SessionId,
    pub agent: AgentId,
    #[serde(deserialize_with = "deserialize_nullable_title")]
    #[schemars(with = "crate::NullableSchema<SessionTitle>", required)]
    pub title: Option<SessionTitle>,
    pub title_updated_seq: u64,
    pub status: SessionStatus,
    #[serde(deserialize_with = "deserialize_nullable_usage")]
    #[schemars(with = "crate::NullableSchema<Usage>", required)]
    pub usage: Option<Usage>,
}
fn deserialize_nullable_title<'de, D>(d: D) -> Result<Option<SessionTitle>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::deserialize(d)
}
fn deserialize_nullable_usage<'de, D>(d: D) -> Result<Option<Usage>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::deserialize(d)
}
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct SessionTree {
    pub session: SessionMeta,
    pub children: Vec<SessionTree>,
}
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionChildrenParams {
    pub session_id: SessionId,
}
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct SessionChildrenResult {
    pub children: Vec<ChildSummary>,
}
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionTreeParams {
    pub session_id: SessionId,
}
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct SessionTreeResult {
    pub tree: SessionTree,
}
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionResumeParams {
    pub session_id: SessionId,
}
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct SessionResumeResult {
    pub session: SessionMeta,
    /// The session's runtime attachments, in attach order.
    pub runtimes: Vec<RuntimeAttachment>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionSetPermissionModeParams {
    pub session_id: SessionId,
    pub mode: PermissionMode,
}
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct SessionSetPermissionModeResult {}

/// A value this build does not know reads as `default`.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
#[schemars(extend("x-open" = true))]
pub enum PermissionRuleSource {
    SessionOverlay,
    AgentDocument,
    #[serde(other)]
    Default,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct EffectivePermissionRule {
    /// The rule's runtime selector, when it has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<RuntimeId>,
    pub resource: WildcardPattern,
    pub effect: PermissionEffect,
    pub source: PermissionRuleSource,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct EffectivePermissionAction {
    pub action: PermissionAction,
    pub effect: PermissionEffect,
    pub source: PermissionRuleSource,
    pub patterns: Vec<EffectivePermissionRule>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionPermissionGetParams {
    pub session_id: SessionId,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct SessionPermissionGetResult {
    pub permissions: Vec<EffectivePermissionAction>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_mode: Option<PermissionMode>,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionPermissionSetParams {
    pub session_id: SessionId,
    pub action: PermissionAction,
    /// Runtime selector; absent for a rule that applies everywhere.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<RuntimeId>,
    pub resource: WildcardPattern,
    pub effect: PermissionEffect,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionPermissionClearParams {
    pub session_id: SessionId,
    pub action: PermissionAction,
    /// Runtime selector of the rule to clear; absent for an unqualified rule.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<RuntimeId>,
    pub resource: WildcardPattern,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct SessionPermissionMutationResult {
    pub permissions: Vec<EffectivePermissionAction>,
}

/// Preview the effective permissions a session created with `selection`
/// and the overlay `rules` would have, without creating it.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionPermissionPreviewParams {
    pub selection: RunSelection,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 256))]
    pub rules: Vec<PermissionRule>,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct SessionPermissionPreviewResult {
    pub permissions: Vec<EffectivePermissionAction>,
}

/// Where a skill comes from: `project`, `official` (an official skill the
/// daemon keeps up to date, since client protocol 1.22.0), or `user`. A
/// value this build does not know reads as `user`.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
#[schemars(extend("x-open" = true))]
pub enum SkillSource {
    Project,
    // An official skill the daemon keeps up to date (client protocol
    // 1.22.0); a user or project skill of the same name wins.
    Official,
    #[serde(other)]
    User,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct SkillDescriptor {
    pub name: String,
    pub description: String,
    #[serde(deserialize_with = "crate::deserialize_required_option")]
    #[schemars(with = "crate::NullableSchema<String>", required)]
    pub when_to_use: Option<String>,
    pub location: String,
    pub source: SkillSource,
    pub precedence_winner: bool,
    pub permission_effect: PermissionEffect,
    pub visible: bool,
    pub user_invocable: bool,
    #[serde(deserialize_with = "crate::deserialize_required_option")]
    #[schemars(with = "crate::NullableSchema<String>", required)]
    pub argument_hint: Option<String>,
    /// An official skill's version (client protocol 1.22.0).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SkillsListParams {
    /// The session whose skills to list; the daemon's own (user and
    /// official skills) when absent (client protocol 1.22.0).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "SessionId")]
    pub session_id: Option<SessionId>,
}

pub const SKILLS_UPDATE_METHOD: &str = "skills.update";

/// `skills.update`: refresh the official skills now.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SkillsUpdateParams {}

/// One official skill the daemon keeps.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct OfficialSkill {
    pub name: String,
    pub version: String,
}

/// What `skills.update` did: the official skills kept afterwards, what
/// changed, and why the refresh failed, if it did (the cached skills stay).
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct SkillsUpdateResult {
    /// `false` when `[skills] official` is off: nothing was refreshed.
    pub enabled: bool,
    #[schemars(length(max = 1024))]
    pub skills: Vec<OfficialSkill>,
    /// Skills added or changed by this refresh.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub updated: Vec<String>,
    /// Skills removed by this refresh.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub removed: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct SkillsListResult {
    pub skills: Vec<SkillDescriptor>,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SkillsGetParams {
    pub session_id: SessionId,
    pub name: String,
    #[serde(default)]
    pub args: String,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct SkillsGetResult {
    pub skill: SkillDescriptor,
    pub rendered: String,
}

const SKILL_SUBMISSION_PREFIX: &str = "\u{0}cookie-skill:";

#[must_use]
pub fn encode_skill_submission(name: &str, args: &str) -> String {
    encode_skill_submission_with_prompt(name, args, None)
}

#[must_use]
pub fn encode_skill_submission_with_prompt(name: &str, args: &str, prompt: Option<&str>) -> String {
    format!(
        "{SKILL_SUBMISSION_PREFIX}{}",
        serde_json::json!({"name": name, "args": args, "prompt": prompt})
    )
}

#[must_use]
pub fn decode_skill_submission(input: &str) -> Option<(String, String, Option<String>)> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Submission {
        name: String,
        args: String,
        prompt: Option<String>,
    }
    let submission: Submission =
        serde_json::from_str(input.strip_prefix(SKILL_SUBMISSION_PREFIX)?).ok()?;
    Some((submission.name, submission.args, submission.prompt))
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionCompactParams {
    pub session_id: SessionId,
    #[serde(deserialize_with = "crate::deserialize_required_option")]
    #[schemars(with = "crate::NullableSchema<SafeDisplayText>", required)]
    pub focus: Option<SafeDisplayText>,
}
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct SessionCompactResult {
    pub compacted: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cancellation_reason: Option<String>,
}
/// Clears a held session's hold and sends its pending producer messages
/// (`docs/site/specs/held-sessions.md`, client protocol 1.34.0).
pub const SESSION_HELD_RESUME_METHOD: &str = "session.held.resume";
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionHeldResumeParams {
    pub session_id: SessionId,
}
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct SessionHeldResumeResult {
    /// Whether the session was held; a session that was not is left as it
    /// is.
    pub resumed: bool,
}
pub const SESSION_COMPACT_CANCEL_METHOD: &str = "session.compact.cancel";
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionCompactCancelParams {
    pub session_id: SessionId,
}
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct SessionCompactCancelResult {
    /// Whether a manual compaction was running and is now cancelled; its
    /// `session.compact` returns without a checkpoint.
    pub cancelled: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionRevertParams {
    pub session_id: SessionId,
    #[schemars(range(min = 1))]
    pub through_seq: u64,
    /// The session's last event seq the client saw. A revert of a session
    /// that has moved on since is refused rather than silently deleting
    /// what another client added.
    #[schemars(range(min = 1))]
    pub expected_last_seq: u64,
}
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct SessionRevertResult {
    pub session: SessionMeta,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions_override: Option<String>,
}

pub const SESSION_DELETE_METHOD: &str = "session.delete";
/// Deletes a root session's whole tree from disk: the root, every delegated
/// session under it, their events, metadata, and artifacts.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionDeleteParams {
    /// A root session. A delegated session is refused with
    /// `not_a_root_session`: its tree is deleted through its root.
    pub session_id: SessionId,
    /// The root's `last_event_seq` the user saw before confirming. A root
    /// whose last event is now another one is refused with
    /// `session_changed`, so a delete never removes what the user has not
    /// seen.
    #[schemars(range(min = 1))]
    pub expected_last_seq: u64,
}
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct SessionDeleteResult {
    /// Every session that was deleted, the root first.
    pub deleted: Vec<SessionId>,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionForkParams {
    pub session_id: SessionId,
    #[schemars(range(min = 1))]
    pub through_seq: u64,
    /// A retry with the same key returns the fork the first request made,
    /// for as long as the daemon runs.
    pub client_fork_id: ClientForkId,
}
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct SessionForkResult {
    pub session_id: SessionId,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum SessionRenameChange {
    Set { title: SessionTitle },
    Clear,
    Reset,
}
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionRenameParams {
    pub session_id: SessionId,
    pub client_rename_id: ClientRenameId,
    pub change: SessionRenameChange,
}
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct SessionRenameResult {
    pub client_rename_id: ClientRenameId,
    pub session: SessionMeta,
}
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionRenameRecord {
    pub client_rename_id: ClientRenameId,
    pub change: SessionRenameChange,
}
impl SessionRenameRecord {
    #[must_use]
    pub fn conflicts_with(&self, request: &SessionRenameParams) -> bool {
        self.client_rename_id == request.client_rename_id && self.change != request.change
    }
    #[must_use]
    pub fn matches(&self, request: &SessionRenameParams) -> bool {
        self.client_rename_id == request.client_rename_id && self.change == request.change
    }
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RunStartParams {
    pub session_id: SessionId,
    pub client_run_id: ClientRunId,
    pub selection: RunSelection,
    /// The user's input: text and file parts, each file an artifact of the
    /// session tree's store (`artifact.upload.*`).
    pub content: UserContent,
    // An explicit model/agent/preset choice may restore a previously skipped prefix.
    #[serde(default, skip_serializing_if = "is_false")]
    pub reset_fallback: bool,
}

fn is_false(value: &bool) -> bool {
    !value
}
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct RunStartResult {
    pub run_id: RunId,
}
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RunSteerParams {
    pub run_id: RunId,
    /// As in `run.start`.
    pub content: UserContent,
}
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct RunSteerResult {
    pub accepted: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handled_reason: Option<String>,
    /// The seq of the `user_input_admitted` event when accepted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_input_seq: Option<u64>,
}

pub const SESSION_SELECT_MODEL_METHOD: &str = "session.select_model";
/// Sets the session's model selection for every client.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionSelectModelParams {
    pub session_id: SessionId,
    pub selection: RunSelection,
}
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct SessionSelectModelResult {
    /// False when the session already had this selection.
    pub changed: bool,
}

pub const EVENTS_UNSUBSCRIBE_METHOD: &str = "events.unsubscribe";
/// Ends this connection's live tail for a session.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EventsUnsubscribeParams {
    pub session_id: SessionId,
}
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct EventsUnsubscribeResult {
    /// Whether this connection had a live tail for the session.
    pub unsubscribed: bool,
}

pub const SESSIONS_CHANGED_METHOD: &str = "sessions.changed";
pub const MCP_CHANGED_METHOD: &str = "mcp.changed";

/// A daemon-wide change to the session index, sent to every connection
/// whether or not it subscribed to the session. Clients keep a session list,
/// approval prompts, and permission modes current from it. A client reads it
/// open: a change it does not know is treated as `resync`.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "change", rename_all = "snake_case")]
#[schemars(extend("x-open" = true))]
pub enum SessionsChangedNotification {
    /// A session was created, persisted, or reverted, or its status or
    /// title changed. Delegated sessions are included; `session.list` shows
    /// only roots.
    Upserted { session: Box<SessionMeta> },
    /// A delegated session was deleted with the revert that removed it.
    Removed { session_id: SessionId },
    /// An approval in `session_id` was requested, escalated, decided, or
    /// cancelled; `approval.list` for the root shows the result.
    ApprovalsChanged {
        root_session_id: SessionId,
        session_id: SessionId,
    },
    /// The tree's permission mode changed. It is runtime-only state, so
    /// this is the only way other clients learn of it.
    PermissionModeChanged {
        root_session_id: SessionId,
        mode: PermissionMode,
        changed_by: EventOrigin,
    },
    /// The daemon dropped changes this connection was too slow to take;
    /// refetch `session.list`, approvals, and permission modes.
    Resync,
}
crate::open_enum!(tagged SessionsChangedNotification, tag = "change", tags = [
    "approvals_changed",
    "permission_mode_changed",
    "removed",
    "resync",
    "upserted",
]);

/// MCP server configuration or status changed; refetch `mcp.server.list`.
#[derive(Clone, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct McpChangedNotification {
    /// Servers that failed to connect, or dropped and did not reconnect, in
    /// runtimes that sessions this connection subscribes to are attached
    /// to. Absent when the change carries no failure.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failures: Vec<McpServerFailure>,
}

/// An MCP server whose tools are unavailable to the sessions using it.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct McpServerFailure {
    /// The runtime hosting the server; `global` for top-level `[mcp]`.
    pub runtime_id: RuntimeId,
    /// The hosting instance's workspace, when its runtime has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    pub server: String,
    pub reason: String,
    /// The affected sessions among those this connection subscribes to.
    pub session_ids: Vec<SessionId>,
}

pub const SESSION_SUBMIT_INPUT_METHOD: &str = "session.submit_input";
pub const SESSION_RECALL_INPUT_METHOD: &str = "session.recall_input";

/// How `session.submit_input` delivers input to a session that is running.
/// An idle session starts a run either way.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InputDelivery {
    /// Admit into the running run's pending lane; the model sees it at the
    /// next tool or completion boundary.
    Steer,
    /// Wait for the running run to complete, then start a new run with it.
    Queue,
}
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionSubmitInputParams {
    pub session_id: SessionId,
    /// As in `run.start`.
    pub content: UserContent,
    pub delivery: InputDelivery,
    /// The selection for a run this input starts, now or after the queue
    /// drains. Omitted means the session's current selection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selection: Option<RunSelection>,
    /// The run idempotency key of a run this input starts. A queued input
    /// keeps it, and the run it later starts carries it.
    pub client_run_id: ClientRunId,
    /// As in `run.start`, for a run this input starts immediately.
    #[serde(default, skip_serializing_if = "is_false")]
    pub reset_fallback: bool,
}
/// What `session.submit_input` did. A client reads it open: an outcome it
/// does not know fails the call with [`crate::ClientError::UnknownOutcome`].
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
#[schemars(extend("x-open" = true))]
pub enum SessionSubmitInputResult {
    /// The session was idle; the input started this run.
    Started { run_id: RunId },
    /// Admitted into the running run's pending lane.
    Steered { run_id: RunId, user_input_seq: u64 },
    /// Queued behind the running run as the `user_input_queued` event at
    /// `queued_seq`.
    Queued { queued_seq: u64 },
    /// A plugin handled the input; nothing was submitted.
    Handled { reason: String },
}
crate::open_enum!(tagged SessionSubmitInputResult, tag = "outcome", tags = [
    "handled",
    "queued",
    "started",
    "steered",
]);
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionRecallInputParams {
    pub session_id: SessionId,
    /// The seq of the pending `user_input_admitted` or the queued
    /// `user_input_queued` event to take back.
    #[schemars(range(min = 1))]
    pub seq: u64,
}
/// What `session.recall_input` did. A client reads it open: an outcome it
/// does not know fails the call with [`crate::ClientError::UnknownOutcome`].
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
#[schemars(extend("x-open" = true))]
pub enum SessionRecallInputResult {
    /// The input was withdrawn; its content is returned for editing.
    Recalled { content: UserContent },
    /// The input is no longer pending or queued: the model already saw it,
    /// a run started with it, or it was voided.
    Gone,
    /// Another client submitted the input; only its author can recall it.
    /// A daemon before client protocol 1.31.0 answers it; later ones let
    /// any client recall any waiting input.
    NotOwner,
}
crate::open_enum!(tagged SessionRecallInputResult, tag = "outcome", tags = [
    "gone",
    "not_owner",
    "recalled",
]);
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RunCancelParams {
    pub run_id: RunId,
}
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct RunCancelResult {
    pub cancelled: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EventsSubscribeParams {
    pub session_id: SessionId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<u64>,
    /// Timestamp of the event at `cursor` as the caller received it. When the
    /// session's event there differs (a revert cut the log and reused the
    /// sequence), the result is `stale_cursor` instead of events.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor_timestamp: Option<jiff::Timestamp>,
    /// Most events to return. The server may end a page sooner to bound its
    /// size, but a page always holds at least one event while any remain.
    /// When more remain after them, the result sets `has_more` and registers
    /// no live tail; the client asks again from the last returned sequence.
    /// Only the final page subscribes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<std::num::NonZeroU32>,
}
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct EventsSubscribeResult {
    /// Shared with the log that holds them, so a page copies no event.
    pub events: Vec<std::sync::Arc<StoredEvent>>,
    /// This is one page of a limited request and more events follow it.
    #[serde(default, skip_serializing_if = "is_false")]
    pub has_more: bool,
    /// `cursor_timestamp` no longer matches the event at `cursor`: nothing is
    /// returned and no tail is registered. The caller replays from the start.
    #[serde(default, skip_serializing_if = "is_false")]
    pub stale_cursor: bool,
}

impl EventsSubscribeResult {
    /// The answer to a request whose `cursor_timestamp` no longer matches.
    #[must_use]
    pub fn stale() -> Self {
        Self {
            events: Vec::new(),
            has_more: false,
            stale_cursor: true,
        }
    }
}

#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalRespondParams {
    pub session_id: SessionId,
    pub approval_id: ApprovalId,
    #[schemars(range(min = 1))]
    pub request_revision: u64,
    pub operation_fingerprint: OperationFingerprint,
    pub client_response_id: ClientResponseId,
    pub decision: ApprovalUserDecision,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub feedback: Option<ApprovalFeedback>,
}
impl ApprovalRespondParams {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.request_revision == 0 {
            return Err("approval request revision must be positive");
        }
        if self.feedback.is_some() && self.decision != ApprovalUserDecision::Reject {
            return Err("approval feedback is allowed only with reject");
        }
        Ok(())
    }
}
impl<'de> Deserialize<'de> for ApprovalRespondParams {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            session_id: SessionId,
            approval_id: ApprovalId,
            request_revision: u64,
            operation_fingerprint: OperationFingerprint,
            client_response_id: ClientResponseId,
            decision: ApprovalUserDecision,
            feedback: Option<ApprovalFeedback>,
        }
        let wire = Wire::deserialize(deserializer)?;
        let value = Self {
            session_id: wire.session_id,
            approval_id: wire.approval_id,
            request_revision: wire.request_revision,
            operation_fingerprint: wire.operation_fingerprint,
            client_response_id: wire.client_response_id,
            decision: wire.decision,
            feedback: wire.feedback,
        };
        value.validate().map_err(serde::de::Error::custom)?;
        Ok(value)
    }
}
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct ApprovalRespondResult {
    pub client_response_id: ClientResponseId,
    pub approval: ApprovalRecord,
}
/// Why the daemon refused an approval response; each is a registry code.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalRespondErrorCode {
    ApprovalNotFound,
    ApprovalNotPending,
    ApprovalRevisionConflict,
    DecisionNotAllowed,
    OperationFingerprintMismatch,
    OperationChanged,
    IdempotencyConflict,
}

impl ApprovalRespondErrorCode {
    /// The registry entry the daemon answers with.
    #[must_use]
    pub fn error_code(self) -> &'static crate::errors::ErrorCode {
        use crate::errors;
        match self {
            Self::ApprovalNotFound => &errors::APPROVAL_NOT_FOUND,
            Self::ApprovalNotPending => &errors::APPROVAL_NOT_PENDING,
            Self::ApprovalRevisionConflict => &errors::APPROVAL_REVISION_CONFLICT,
            Self::DecisionNotAllowed => &errors::DECISION_NOT_ALLOWED,
            Self::OperationFingerprintMismatch => &errors::OPERATION_FINGERPRINT_MISMATCH,
            Self::OperationChanged => &errors::OPERATION_CHANGED,
            Self::IdempotencyConflict => &errors::IDEMPOTENCY_CONFLICT,
        }
    }

    /// The code of an approval error the daemon sent, when it is one.
    #[must_use]
    pub fn from_error_code(code: &str) -> Option<Self> {
        [
            Self::ApprovalNotFound,
            Self::ApprovalNotPending,
            Self::ApprovalRevisionConflict,
            Self::DecisionNotAllowed,
            Self::OperationFingerprintMismatch,
            Self::OperationChanged,
            Self::IdempotencyConflict,
        ]
        .into_iter()
        .find(|candidate| candidate.error_code().code == code)
    }
}
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalListParams {
    pub root_session_id: SessionId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<ApprovalStatus>,
}
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct ApprovalListResult {
    pub approvals: Vec<ApprovalRecord>,
    pub tree_grants: Vec<TreeApprovalGrant>,
}

/// A value this build does not know reads as `runtime`: a server the user file
/// does not define, which `mcp.server.*` cannot edit. `machine` (client
/// protocol 1.12.0) is a server the hosting runtime's own machine defines (a
/// dial-in runtime's `[runtimes.local.mcp]`), which the daemon does not read:
/// its `definition` is empty.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
#[schemars(extend("x-open" = true))]
pub enum McpConfigSource {
    UserFile,
    Machine,
    #[serde(other)]
    Runtime,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum McpConfigTarget {
    UserFile,
}

crate::open_enum! {
    #[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
    #[serde(rename_all = "snake_case")]
    pub enum McpServerState {
        Connected,
        Connecting,
        /// The connection dropped and is being re-established. The server's
        /// tools stay published; calls fail with `runtime_unavailable`.
        Reconnecting,
        Failed,
        NeedsAuth,
        Disabled,
        Disconnected,
    }
}

#[derive(Clone, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct McpOAuthSettingsDefinition {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_metadata_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scopes: Option<Vec<String>>,
}

impl std::fmt::Debug for McpOAuthSettingsDefinition {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("McpOAuthSettingsDefinition")
            .field("client_id", &self.client_id)
            .field(
                "client_secret",
                &self.client_secret.as_ref().map(|_| "[REDACTED]"),
            )
            .field("client_metadata_url", &self.client_metadata_url)
            .field("scopes", &self.scopes)
            .finish()
    }
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(untagged)]
pub enum McpOAuthDefinition {
    Bool(bool),
    Settings(McpOAuthSettingsDefinition),
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct McpServerDefinition {
    #[serde(deserialize_with = "deserialize_required_option")]
    #[schemars(with = "crate::NullableSchema<String>", required)]
    pub command: Option<String>,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    #[serde(deserialize_with = "deserialize_required_option")]
    #[schemars(with = "crate::NullableSchema<String>", required)]
    pub cwd: Option<String>,
    #[serde(deserialize_with = "deserialize_required_option")]
    #[schemars(with = "crate::NullableSchema<String>", required)]
    pub url: Option<String>,
    pub headers: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oauth: Option<McpOAuthDefinition>,
    pub enabled: bool,
    #[serde(deserialize_with = "deserialize_required_option")]
    #[schemars(with = "crate::NullableSchema<u64>", required)]
    pub timeout_ms: Option<u64>,
    /// Patterns over the server's own tool names (`*`, `?`): only matching
    /// tools are published (client protocol 1.16.0). Absent publishes every
    /// tool; present, it may not be empty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub include_tools: Option<Vec<String>>,
    /// Patterns over the server's own tool names, applied after
    /// `include_tools`: matching tools are not published (client protocol
    /// 1.16.0).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude_tools: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct McpServerInfo {
    /// The runtime hosting the server; `global` for top-level `[mcp]`
    /// servers, the only ones `mcp.server.*` mutations edit.
    pub runtime_id: RuntimeId,
    /// The hosting instance's canonical workspace, when its runtime has one;
    /// it tells instances of one runtime apart.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    pub name: String,
    /// Where a runtime other than `global` hosts the server (client protocol
    /// 1.13.0): `runtime`, one process the runtime shares across its
    /// workspaces, listed without a workspace; or `workspace`, one per
    /// instance, listed with the instance's workspace, and once without one
    /// (`disconnected`, "starts when a workspace opens") while no instance of
    /// a connected runtime hosts it. Absent for `global`'s servers and from
    /// runtimes that do not say.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<crate::runtime_protocol::McpServerScope>,
    pub source: McpConfigSource,
    pub definition: McpServerDefinition,
    pub state: McpServerState,
    pub tool_count: u32,
    /// How many of the tools the server lists its `include_tools` and
    /// `exclude_tools` filtered out (client protocol 1.16.0); absent when
    /// none were, and from runtimes that do not say.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filtered_tools: Option<u32>,
    #[serde(deserialize_with = "deserialize_required_option")]
    #[schemars(with = "crate::NullableSchema<String>", required)]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_in_progress: Option<bool>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct McpServerListParams {}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct McpServerListResult {
    pub servers: Vec<McpServerInfo>,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct McpServerAddParams {
    pub name: String,
    pub definition: McpServerDefinition,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct McpServerEditParams {
    pub name: String,
    pub definition: McpServerDefinition,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct McpServerNameParams {
    pub name: String,
}

/// `mcp.server.reconnect`: reconnects one server now. A server hosted by a
/// runtime other than `global` is named with its runtime ID and the
/// instance's workspace, as `mcp.server.list` reports them.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct McpServerReconnectParams {
    pub name: String,
    /// The hosting runtime; `global` when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_id: Option<RuntimeId>,
    /// The hosting instance's workspace, when its runtime has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct McpServerSetEnabledParams {
    pub name: String,
    pub enabled: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct McpServerPersistParams {
    pub name: String,
    pub target: McpConfigTarget,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct McpServerMutationResult {
    #[serde(deserialize_with = "deserialize_required_option")]
    #[schemars(with = "crate::NullableSchema<McpServerInfo>", required)]
    pub server: Option<McpServerInfo>,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct McpAuthBeginParams {
    pub server: String,
    /// Where the browser lands after authorizing, which the client catches
    /// and hands to `mcp.auth.complete`: the client's own loopback callback,
    /// plain `http` to a loopback IP literal or `localhost`
    /// (`http://127.0.0.1:{port}/{path}`), or an `https` address, such as a
    /// web page's own with `?oauth=callback` (client protocol 1.11.0; before
    /// it, loopback IP literals with a port only). Any path and query; no
    /// user info or fragment.
    pub redirect_uri: String,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct McpAuthBeginResult {
    pub server: String,
    pub authorization_url: String,
}

/// The browser's redirect to the client's callback, carried back to the daemon
/// that holds the PKCE verifier. Only the single-use authorization code
/// crosses the connection; tokens stay in the daemon.
#[derive(Clone, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct McpAuthCompleteParams {
    pub server: String,
    pub callback_url: String,
}

impl std::fmt::Debug for McpAuthCompleteParams {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("McpAuthCompleteParams")
            .field("server", &self.server)
            .field("callback_url", &"<redacted>")
            .finish()
    }
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct McpAuthCompleteResult {
    pub server: String,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct McpAuthCancelParams {
    pub server: String,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct McpAuthCancelResult {
    pub server: String,
}
