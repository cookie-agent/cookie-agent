//! Wire types of the runtime protocol between the daemon and a runtime, as
//! specified in `docs/site/specs/runtimes.md` section 9 and documented in
//! `docs/site/reference/runtime-protocol.md`.
//!
//! The protocol is JSON-RPC 2.0, one message per frame: a line over stdio or
//! a frame of an in-memory transport. Every message is bounded by
//! [`MAX_FRAME_BYTES`]. Both ends send requests: the daemon drives tools, and
//! a runtime reports progress back while a call runs. [`crate::peer`] carries
//! the messages.
//!
//! Every message is information except the requests and notifications a
//! runtime or plugin sends that ask the daemon to act (`tool/progress`,
//! `tool/detachable`, and `producer/*`): information ignores members it does
//! not know and reads an open enum's unknown value as its documented
//! fallback (`docs/site/specs/compatibility-v1.md` section 4); the daemon
//! decodes commands with [`crate::strict::decode_command`], which refuses
//! what their schema does not list. Command types also deny unknown fields
//! themselves.

use crate::{
    AdaptorId, AgentId, AgentMdEntry, AgentMdSkipped, DisplayBlock, MediaKindLabel, ModelKey, Open,
    PermissionAction, PreparedPresentation, RunId, RuntimeKind, SafeDisplayText, SessionId,
    SkillSource, ToolAttachment, ToolCallId, ToolCompletionOutput, ToolEmittedMessage,
    ToolOutputChunk, ToolOutputDeclaration,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The largest frame either end sends or accepts. It fits an `execute`
/// reply carrying [`MAX_INLINE_MEDIA_BYTES`] of base64 media next to a full
/// tool result.
pub const MAX_FRAME_BYTES: usize = 48 * 1024 * 1024;

/// The most media bytes, before base64, one `tool/execute` reply carries:
/// the largest single attachment a tool may return.
pub const MAX_INLINE_MEDIA_BYTES: u64 = 25 * 1024 * 1024;

/// The most calls one `tool/prepare` request carries.
pub const MAX_PREPARE_BATCH: usize = 256;

/// How the daemon refuses a runtime that dials in on
/// `/runtime?id=<runtime ID>`, since protocol 1.2.0. A refusal before the
/// WebSocket upgrade is an HTTP status whose body is the reason in plain
/// text; one after it is a close frame whose reason is the reason. A
/// runtime stops dialing in on a permanent refusal and dials in again,
/// after a wait, on any other.
pub mod dial_in {
    /// HTTP 400: the runtime ID is malformed. Permanent.
    pub const STATUS_INVALID_ID: u16 = 400;
    /// HTTP 401: the bearer token is missing or wrong. Permanent.
    pub const STATUS_UNAUTHORIZED: u16 = 401;
    /// HTTP 403: the runtime ID is reserved (`global`, `local`) or
    /// configured on the daemon, or the request carries an `Origin` header.
    /// Permanent.
    pub const STATUS_FORBIDDEN: u16 = 403;
    /// HTTP 409: a runtime is connected, or being accepted, under the ID.
    /// Transient: the daemon may not have noticed yet that an earlier
    /// connection of the same runtime ended.
    pub const STATUS_CONNECTED: u16 = 409;
    /// Close code 1000, normal closure: the end that closes is done with
    /// the connection, such as a runtime that stops.
    pub const CLOSE_NORMAL: u16 = 1000;
    /// Close code 1011: the runtime did not complete `runtime/initialize`,
    /// such as one of an incompatible version or one that did not answer in
    /// time. Transient.
    pub const CLOSE_INITIALIZE_FAILED: u16 = 1011;

    /// Whether a refusal with HTTP `status` is permanent.
    #[must_use]
    pub const fn is_permanent_status(status: u16) -> bool {
        matches!(
            status,
            STATUS_INVALID_ID | STATUS_UNAUTHORIZED | STATUS_FORBIDDEN
        )
    }
}

/// Method names.
pub mod method {
    /// daemon → runtime: version, identity, and settings; returns the
    /// capabilities the runtime offers.
    pub const INITIALIZE: &str = "runtime/initialize";
    /// daemon → runtime: open a capability for a workspace; returns the
    /// instance's identity, tools, and skills.
    pub const CAPABILITY_OPEN: &str = "capability/open";
    /// runtime → daemon (notification): an instance's tool list changed.
    pub const CAPABILITY_TOOLS_CHANGED: &str = "capability/tools_changed";
    /// plugin or runtime → daemon (notification): the tools outside any
    /// instance changed. A plugin connection has one implicit instance; a
    /// runtime declares here the tools of its runtime-scoped MCP servers
    /// (since 1.5.0), which `tool/prepare` without an `instance` reaches.
    pub const TOOLS_CHANGED: &str = "runtime/tools_changed";
    /// daemon → runtime: per-admission context (`AGENTS.md`).
    pub const CONTEXT: &str = "runtime/context";
    /// daemon → runtime: stop gracefully.
    pub const SHUTDOWN: &str = "runtime/shutdown";
    /// daemon → runtime: answer at once, with an empty result, while the
    /// connection is up (since 1.4.0). The daemon pings a connection it has
    /// heard nothing on for a while and drops one that stays silent; any
    /// answer counts, so an older runtime's `unknown_method` error does too.
    pub const PING: &str = "runtime/ping";
    /// daemon → runtime: prepare a batch of calls.
    pub const TOOL_PREPARE: &str = "tool/prepare";
    /// daemon → runtime: check a prepared call can still run as approved.
    pub const TOOL_REVALIDATE: &str = "tool/revalidate";
    /// daemon → runtime: execute a prepared call.
    pub const TOOL_EXECUTE: &str = "tool/execute";
    /// daemon → runtime (notification): discard a prepared call.
    pub const TOOL_RELEASE: &str = "tool/release";
    /// runtime → daemon: display and output of an executing call.
    pub const TOOL_PROGRESS: &str = "tool/progress";
    /// daemon → runtime (notification): cancel an executing call.
    pub const TOOL_CANCEL: &str = "tool/cancel";
    /// runtime → daemon (notification): an executing call reached its detach
    /// point.
    pub const TOOL_DETACHABLE: &str = "tool/detachable";
    /// runtime → daemon (notification): the status of the runtime's MCP
    /// servers changed, or one failed.
    pub const MCP_CHANGED: &str = "mcp/changed";
    /// daemon → runtime: reconnect one MCP server, or retry every failed
    /// one.
    pub const MCP_RECONNECT: &str = "mcp/reconnect";
    /// runtime → daemon (notification): the runtime's configured MCP
    /// servers changed status, or a runtime-scoped one failed (since 1.5.0).
    pub const RUNTIME_MCP_CHANGED: &str = "runtime/mcp_changed";
    /// daemon → runtime: reconnect one runtime-scoped MCP server, or retry
    /// every failed one (since 1.5.0).
    pub const RUNTIME_MCP_RECONNECT: &str = "runtime/mcp_reconnect";
    /// runtime → daemon: register a named producer for a session attached
    /// to the runtime.
    pub const PRODUCER_REGISTER: &str = "producer/register";
    /// runtime → daemon: send a message through a registered producer.
    pub const PRODUCER_SEND: &str = "producer/send";
    /// runtime → daemon: close a registration; its messages stay.
    pub const PRODUCER_UNREGISTER: &str = "producer/unregister";
    /// runtime → daemon: discard one of the runtime's waiting messages.
    pub const PRODUCER_DISCARD: &str = "producer/discard";
    /// daemon → runtime (notification): producer messaging is open; restore
    /// the external work the runtime tracks, then answer with
    /// [`PRODUCER_RECOVERY_COMPLETE`].
    pub const PRODUCER_RECOVERY_START: &str = "producer/recovery/start";
    /// runtime → daemon: the runtime's recovery outcome.
    pub const PRODUCER_RECOVERY_COMPLETE: &str = "producer/recovery/complete";
}

/// An empty params or result object.
#[derive(
    Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize, schemars::JsonSchema,
)]
pub struct Empty {}

// --- runtime/initialize ---------------------------------------------------

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize, schemars::JsonSchema)]
pub struct InitializeParams {
    /// The daemon's [`crate::RUNTIME_PROTOCOL_VERSION`].
    pub protocol_version: crate::SemVer,
    /// The daemon's own version, for logs and diagnostics.
    pub engine_version: String,
    /// What the daemon's configuration enables for this runtime. None are
    /// defined yet; a runtime ignores one it does not know.
    #[serde(default)]
    pub features: Vec<String>,
    /// The runtime's ID.
    pub runtime_id: String,
    pub settings: RuntimeSettings,
}

/// What the daemon's configuration says about the runtime's work. A runtime
/// rejects settings it does not support.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize, schemars::JsonSchema)]
pub struct RuntimeSettings {
    /// `[tool_output]`: preview limits of tool output.
    pub tool_output: ToolOutputLimits,
    /// `[agent_md]`: the `AGENTS.md` size rule.
    pub agent_md: AgentMdLimits,
    /// The template's MCP servers, which the runtime hosts on its own
    /// machine. A runtime that hosts no MCP servers rejects a nonempty list.
    #[serde(default)]
    pub mcp_servers: Vec<McpServerSettings>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, schemars::JsonSchema)]
pub struct ToolOutputLimits {
    pub max_lines: u64,
    pub max_bytes: u64,
    pub head_lines: u64,
    pub tail_lines: u64,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, schemars::JsonSchema)]
pub struct AgentMdLimits {
    /// Files larger than this are skipped. At most
    /// the `AGENTS.md` event bound.
    pub max_bytes: u64,
}

/// One MCP server of a runtime template.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize, schemars::JsonSchema)]
pub struct McpServerSettings {
    pub name: String,
    /// The server's table, as the `[mcp.servers.<name>]` schema reads it.
    pub config: Value,
}

/// The status of one MCP server a runtime hosts.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, schemars::JsonSchema)]
pub struct McpServerReport {
    pub name: String,
    pub state: McpServerState,
    /// Why the server failed, or what it is doing.
    #[serde(default)]
    pub message: Option<String>,
    /// The names of its tools as the runtime declares them.
    #[serde(default)]
    pub tools: Vec<String>,
    /// Where the runtime hosts the server (runtime protocol 1.5.0); absent
    /// reads as `workspace`, which is all a runtime hosted before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<McpServerScope>,
    /// How many of the tools the server lists its `include_tools` and
    /// `exclude_tools` filtered out (runtime protocol 1.6.0); absent when
    /// none were.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filtered_tools: Option<u32>,
}

/// Where a runtime hosts an MCP server (runtime protocol 1.5.0; client
/// protocol 1.13.0). Open: a value this build does not know reads as
/// `workspace`.
#[derive(
    Clone, Copy, Debug, Default, Deserialize, Eq, Hash, PartialEq, Serialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
#[schemars(extend("x-open" = true))]
pub enum McpServerScope {
    /// One process per runtime, started when the runtime initializes and
    /// shared by every workspace instance; its tools are the runtime's own,
    /// outside any instance.
    Runtime,
    /// One process per workspace instance, started when the instance
    /// opens.
    #[default]
    #[serde(other)]
    Workspace,
}

impl McpServerScope {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Runtime => "runtime",
            Self::Workspace => "workspace",
        }
    }
}

/// Open: a state this build does not know reads as `disconnected`, which
/// offers no tools and is not retried as a failure.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
#[schemars(extend("x-open" = true))]
pub enum McpServerState {
    Disabled,
    Connecting,
    Connected,
    /// The connection dropped and is being re-established: its tools stay
    /// declared and calls fail with `runtime_unavailable`.
    Reconnecting,
    Failed,
    #[serde(other)]
    Disconnected,
}

/// An MCP server that failed to connect, or dropped and did not reconnect.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, schemars::JsonSchema)]
pub struct McpFailureReport {
    pub server: String,
    pub reason: String,
}

// --- mcp/changed, mcp/reconnect --------------------------------------------------

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize, schemars::JsonSchema)]
pub struct McpChangedParams {
    pub instance: String,
    /// Every server the instance hosts, in name order.
    pub servers: Vec<McpServerReport>,
    /// Servers that failed since the last report, in order.
    #[serde(default)]
    pub failures: Vec<McpFailureReport>,
}

/// `runtime/mcp_changed` (runtime protocol 1.5.0): every MCP server the
/// runtime is configured with, as `runtime/initialize` reported them.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize, schemars::JsonSchema)]
pub struct RuntimeMcpChangedParams {
    /// Every configured server, in name order: the runtime-scoped ones with
    /// their live status and tools, the workspace-scoped ones as configured
    /// (`disconnected`, or `disabled`, with no tools; each instance reports
    /// its own with `mcp/changed`).
    pub servers: Vec<McpServerReport>,
    /// Runtime-scoped servers that failed since the last report, in order.
    #[serde(default)]
    pub failures: Vec<McpFailureReport>,
}

/// `runtime/mcp_reconnect` (runtime protocol 1.5.0).
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize, schemars::JsonSchema)]
pub struct RuntimeMcpReconnectParams {
    /// The runtime-scoped server to reconnect now; absent to retry every
    /// failed one.
    #[serde(default)]
    pub server: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize, schemars::JsonSchema)]
pub struct McpReconnectParams {
    pub instance: String,
    /// The server to reconnect now; absent to retry every failed server.
    #[serde(default)]
    pub server: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize, schemars::JsonSchema)]
pub struct InitializeResult {
    /// The runtime's [`crate::RUNTIME_PROTOCOL_VERSION`].
    pub protocol_version: crate::SemVer,
    /// What the runtime's own configuration enables. None are defined yet;
    /// the daemon ignores one it does not know, with a diagnostic.
    #[serde(default)]
    pub features: Vec<String>,
    /// Operating system, as Rust's `std::env::consts::OS` names it, for a
    /// runtime where it matters.
    #[serde(default)]
    pub platform: Option<String>,
    /// The machine the runtime runs on, which the daemon groups runtimes
    /// by (runtime protocol 1.1.0). Absent when the runtime cannot tell.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine: Option<RuntimeMachine>,
    /// The home directory of the user the runtime runs as, as a canonical
    /// absolute path (runtime protocol 1.3.0). Clients show the paths under
    /// it as `~/…`, and a permission rule's leading `~` expands against it.
    /// Absent when the runtime does not say; a reader treats one that
    /// [`is_valid_home`] refuses as absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub home: Option<String>,
    /// What the runtime offers; sessions attach each one separately.
    pub capabilities: Vec<CapabilityDeclaration>,
    /// The runtime sends producer messages to the sessions attached to its
    /// instances (`producer/*`), once the daemon sends
    /// `producer/recovery/start`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub producer_messaging: bool,
    /// Every MCP server the runtime is configured with (runtime protocol
    /// 1.5.0), in name order, as `runtime/mcp_changed` reports them later:
    /// the runtime-scoped ones, which start as the runtime initializes, with
    /// their status now, and the workspace-scoped ones as configured.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mcp_servers: Vec<McpServerReport>,
}

/// The machine a runtime runs on, as `runtime/initialize` reports it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, schemars::JsonSchema)]
pub struct RuntimeMachine {
    /// A stable, opaque ID of the machine, such as
    /// [`crate::paths::machine_id`]: 1 to [`RuntimeMachine::MAX_ID_BYTES`]
    /// bytes of printable ASCII. Runtimes that report one ID run on one
    /// machine.
    pub id: String,
    /// The machine's host name, which names a machine the daemon has not
    /// seen before; at most [`RuntimeMachine::MAX_HOST_NAME_BYTES`] bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_name: Option<String>,
}

impl RuntimeMachine {
    /// The longest machine ID.
    pub const MAX_ID_BYTES: usize = 128;
    /// The longest host name.
    pub const MAX_HOST_NAME_BYTES: usize = 255;

    /// This machine: [`crate::paths::machine_id`] and
    /// [`crate::paths::host_name`]. `None` when it has no machine ID.
    #[cfg(not(target_arch = "wasm32"))]
    #[must_use]
    pub fn this_machine() -> Option<Self> {
        let host_name = crate::paths::host_name();
        Some(Self {
            id: crate::paths::machine_id()?,
            host_name: (!host_name.is_empty()).then_some(host_name),
        })
        .filter(Self::is_valid)
    }

    /// Whether the ID and host name keep their bounds: a reader treats a
    /// machine that does not as unreported.
    #[must_use]
    pub fn is_valid(&self) -> bool {
        (1..=Self::MAX_ID_BYTES).contains(&self.id.len())
            && self.id.bytes().all(|byte| byte.is_ascii_graphic())
            && self.host_name.as_ref().is_none_or(|host| {
                host.len() <= Self::MAX_HOST_NAME_BYTES && !host.chars().any(char::is_control)
            })
    }
}

/// The longest home directory a runtime reports, in bytes.
pub const MAX_HOME_BYTES: usize = 4096;

/// Whether `home` is a home directory a reader takes: an absolute path in
/// POSIX (`/home/user`) or Windows (`C:\Users\user`, `C:/Users/user`,
/// `\\server\share\user`) form, of 1 to [`MAX_HOME_BYTES`] bytes and
/// without control characters. Only the spelling is checked: the path is on
/// the runtime's machine, which need not be the reader's.
#[must_use]
pub fn is_valid_home(home: &str) -> bool {
    let bytes = home.as_bytes();
    let absolute = bytes.first() == Some(&b'/')
        || home.starts_with(r"\\")
        || (bytes.len() >= 3
            && bytes[0].is_ascii_alphabetic()
            && bytes[1] == b':'
            && matches!(bytes[2], b'\\' | b'/'));
    absolute && home.len() <= MAX_HOME_BYTES && !home.chars().any(char::is_control)
}

/// One capability a runtime offers.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, schemars::JsonSchema)]
pub struct CapabilityDeclaration {
    /// `[a-z][a-z0-9_]{0,31}`, unique within the runtime.
    pub name: crate::CapabilityName,
    /// A self-declared label the daemon shows and never branches on.
    pub kind: RuntimeKind,
    /// Whether opening it requires a workspace.
    pub takes_workspace: bool,
    #[serde(default)]
    pub description: String,
}

// --- capability/open, capability/tools_changed --------------------------------

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, schemars::JsonSchema)]
pub struct CapabilityOpenParams {
    pub capability: crate::CapabilityName,
    /// The workspace, exactly as a session gave it: the runtime expands a
    /// leading `~` and resolves it on its own machine. Required exactly
    /// when the capability takes one.
    #[serde(default)]
    pub workspace: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize, schemars::JsonSchema)]
pub struct CapabilityOpenResult {
    /// The runtime-chosen handle of the instance, unique on the
    /// connection. Opening an instance that is already open returns it.
    pub instance: String,
    /// The name sessions attach the instance under.
    pub attachment_name: String,
    /// The canonical workspace on the runtime's machine.
    #[serde(default)]
    pub workspace: Option<String>,
    pub tools: RuntimeTools,
    pub skills: RuntimeSkills,
    /// The instance's MCP servers after their first connection attempt.
    /// Their tools are among `tools`.
    #[serde(default)]
    pub mcp_servers: Vec<McpServerReport>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize, schemars::JsonSchema)]
pub struct CapabilityToolsChangedParams {
    pub instance: String,
    pub tools: RuntimeTools,
}

/// A runtime's tools and their prompt sections.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize, schemars::JsonSchema)]
pub struct RuntimeTools {
    pub tools: Vec<ToolDeclaration>,
    #[serde(default)]
    pub prompt_sections: Vec<PromptSectionDeclaration>,
}

/// A tool as a runtime declares it.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize, schemars::JsonSchema)]
pub struct ToolDeclaration {
    pub name: String,
    #[serde(default)]
    pub collision: ToolCollision,
    /// The fixed permission action the tool is checked under; absent for a
    /// tool that needs no permission.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission: Option<PermissionAction>,
    pub description: String,
    /// JSON Schema of the arguments.
    pub parameters: Value,
    #[serde(default)]
    pub concurrency: ToolConcurrency,
    #[serde(default)]
    pub result_truncation: ToolResultTruncationPolicy,
    #[serde(default)]
    pub output: ToolOutputDeclaration,
    /// The tool supports the background (`docs/site/specs/background-tools.md`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub background: bool,
    /// Whether a call runs in the background when the model omits the
    /// argument; only valid with `background`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub background_default: bool,
}

/// The permission action a tool spec's permission name spells: any valid
/// action label.
#[must_use]
pub fn permission_action(name: &str) -> Option<PermissionAction> {
    PermissionAction::new(name).ok()
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, schemars::JsonSchema)]
pub struct PromptSectionDeclaration {
    pub title: String,
    pub body: String,
}

/// Skills a capability instance contributes, reported when `capability/open`
/// opens it.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize, schemars::JsonSchema)]
pub struct RuntimeSkills {
    /// Skills that won precedence, by name.
    pub skills: Vec<SkillDeclaration>,
    /// Every `SKILL.md` found, winners and shadowed ones.
    pub discoveries: Vec<SkillDiscoveryDeclaration>,
}

/// One skill: its `SKILL.md` text and where it lives on the runtime's
/// machine.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, schemars::JsonSchema)]
pub struct SkillDeclaration {
    /// The `SKILL.md` text, frontmatter and body.
    pub document: String,
    pub source: SkillSource,
    /// Absolute path of the `SKILL.md`.
    pub path: String,
    /// Absolute path of the skill directory (`${COOKIE_SKILL_DIR}`).
    pub base_dir: String,
    /// A sample of the skill's other files.
    pub supporting_files: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, schemars::JsonSchema)]
pub struct SkillDiscoveryDeclaration {
    pub name: String,
    pub source: SkillSource,
    pub path: String,
    pub precedence_winner: bool,
}

// --- runtime/tools_changed --------------------------------------------------

/// The runtime's whole new tool list.
pub type ToolsChangedParams = RuntimeTools;

// --- runtime/context ----------------------------------------------------------

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize, schemars::JsonSchema)]
pub struct ContextParams {
    pub instance: String,
    /// The run's agent preset.
    #[serde(default)]
    pub preset: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize, schemars::JsonSchema)]
pub struct ContextResult {
    pub agent_md: Vec<AgentMdEntry>,
    pub skipped: Vec<AgentMdSkipped>,
}

// --- tool/prepare -------------------------------------------------------------

/// The agent and model of one tool batch, as a runtime or plugin sees them.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize, schemars::JsonSchema)]
pub struct ToolTurn {
    pub agent: AgentId,
    pub model: ToolTurnModel,
}

/// The model of a tool batch: what a tool needs to word its results and to
/// decide which media it may return. Nothing about the model's provider
/// beyond its adapter label.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize, schemars::JsonSchema)]
pub struct ToolTurnModel {
    pub model: ModelKey,
    /// The adapter label, such as `openai-responses`.
    pub adapter: AdaptorId,
    /// The media kinds the model accepts from a tool, with their effective
    /// limits and how each reaches the model; a kind not listed is not
    /// accepted. Empty for a model that takes text only.
    #[serde(default)]
    pub input_media: Vec<ToolInputMedia>,
}

/// One media kind a tool may return to the turn's model. The limits are
/// effective: the lowest of the model's capability, its adapter family's
/// delivery limit, and the attachment caps.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, schemars::JsonSchema)]
pub struct ToolInputMedia {
    pub kind: MediaKindLabel,
    /// The media types accepted, such as `image/png`.
    pub mime_types: Vec<String>,
    /// The largest accepted file.
    pub max_bytes: u64,
    pub delivery: MediaDelivery,
}

/// How an attachment of a kind reaches the model.
///
/// Open: a delivery this build does not know reads as `unknown`, which a
/// tool cannot use: it does not attach the kind.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
#[schemars(extend("x-open" = true))]
pub enum MediaDelivery {
    /// As an attachment of the tool result.
    ToolResult,
    /// As a user message after the tool result, for an adapter whose tool
    /// results cannot carry the kind.
    UserMessage,
    #[serde(other)]
    Unknown,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize, schemars::JsonSchema)]
pub struct WireToolCall {
    pub id: ToolCallId,
    /// The name the runtime knows the tool by.
    pub name: String,
    pub arguments: Value,
}

/// One turn's calls to one runtime, prepared together so calls on the same
/// target chain.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize, schemars::JsonSchema)]
pub struct PrepareParams {
    /// The capability instance the calls go to; absent for a plugin, whose
    /// connection has one implicit instance, and for the tools a runtime
    /// declares outside any instance (`runtime/tools_changed`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance: Option<String>,
    pub session: SessionId,
    pub run: RunId,
    pub turn: ToolTurn,
    pub calls: Vec<WireToolCall>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize, schemars::JsonSchema)]
pub struct PrepareResult {
    /// One outcome per call, in order.
    pub results: Vec<PrepareOutcome>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize, schemars::JsonSchema)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum PrepareOutcome {
    Prepared(PreparedCall),
    Failed {
        error: WireToolError,
        /// How clients show the call that failed to prepare.
        presentation: PreparedPresentation,
    },
}

/// A prepared call. The runtime keeps its executor under `prepared_id`
/// until it is executed or released.
///
/// The daemon builds the call's approval identity from `permission`,
/// `permission_resource`, and `normalized_arguments` together with the tool
/// it knows the call goes to; a runtime never states it.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize, schemars::JsonSchema)]
pub struct PreparedCall {
    pub prepared_id: u64,
    /// The arguments as the call will run them, such as a path made
    /// relative to the workspace or defaults filled in. Not `null`.
    pub normalized_arguments: Value,
    /// The permission action the call is checked under; absent for a tool
    /// that needs no permission. It must be the one the tool declares.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission: Option<PermissionAction>,
    /// The permission resource of the normalized arguments, such as a path,
    /// a command, or a URL; absent for a call checked under the action alone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_resource: Option<String>,
    /// How clients show the call.
    pub presentation: PreparedPresentation,
    /// Calls with one key run one at a time; lowercase hex.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub serialization_key: Option<String>,
}

/// Names one prepared call.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, schemars::JsonSchema)]
pub struct PreparedRef {
    pub prepared_id: u64,
}

/// The outcome of a host service or a check: absent error means success.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize, schemars::JsonSchema)]
pub struct Outcome {
    #[serde(default)]
    pub error: Option<WireToolError>,
}

// --- tool/execute -------------------------------------------------------------

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize, schemars::JsonSchema)]
pub struct ExecuteParams {
    pub prepared_id: u64,
    pub session: SessionId,
    pub run: RunId,
    pub turn: ToolTurn,
    /// The model asked to run the call in the background.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub background: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize, schemars::JsonSchema)]
pub struct ExecuteResult {
    pub outcome: ExecuteOutcome,
    /// Bytes of the attachments the result references, for the daemon to
    /// retain. At most [`MAX_INLINE_MEDIA_BYTES`] in all.
    #[serde(default)]
    pub media: Vec<InlineMedia>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize, schemars::JsonSchema)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ExecuteOutcome {
    Completed(ToolResult),
    Failed { error: WireToolError },
}

/// A call's terminal result as a runtime or plugin returns it. The daemon
/// builds the result a session stores from it: it captures the output, fits
/// the display, and records the background state and retained output
/// itself.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize, schemars::JsonSchema)]
pub struct ToolResult {
    /// The output the model reads: the whole text, or `streamed` when the
    /// call's progress already sent it.
    pub output: ToolCompletionOutput,
    /// The result's title; the tool's name when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<SafeDisplayText>,
    /// What clients show. A `single` output with no display shows its text.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub display: Vec<Open<DisplayBlock>>,
    /// Structured data the model reads with the output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
    /// Files the result carries; their bytes travel in the reply's `media`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 256))]
    pub attachments: Vec<ToolAttachment>,
    /// Messages the result adds to the model's history after it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 4))]
    pub additional_messages: Vec<ToolEmittedMessage>,
    /// The call failed: the model reads the output as the failure.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub failed: bool,
}

impl ToolResult {
    /// The result's files: its attachments, then those of its added
    /// messages.
    pub fn all_attachments(&self) -> impl Iterator<Item = &ToolAttachment> {
        self.attachments.iter().chain(
            self.additional_messages
                .iter()
                .flat_map(|message| message.content.iter())
                .filter_map(|part| match part {
                    crate::ToolEmittedContent::Text(_) => None,
                    crate::ToolEmittedContent::File(attachment) => Some(attachment),
                }),
        )
    }
}

/// Attachment bytes, content-addressed.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, schemars::JsonSchema)]
pub struct InlineMedia {
    /// Lowercase hex SHA-256 of the bytes.
    pub sha256: String,
    /// Standard base64 of the bytes.
    pub data: String,
}

/// The reference the daemon's artifact store gives content with `sha256`;
/// a runtime returns it from retention so results reference media before
/// the daemon has stored it.
#[must_use]
pub fn artifact_reference(sha256: &str) -> crate::ArtifactReference {
    crate::ArtifactReference {
        uri: format!("artifact://sha256/{sha256}"),
    }
}

// --- tool/progress ---------------------------------------------------------------

/// Display and output deltas, bounded like in-process progress.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WireProgress {
    pub tool_call_id: ToolCallId,
    pub message: String,
    /// A change to the call's live display.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display: Option<crate::DisplayDelta>,
    #[serde(default)]
    pub output: Vec<ToolOutputChunk>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ToolProgressParams {
    pub prepared_id: u64,
    pub progress: WireProgress,
}

/// Removes from raw `tool/progress` params the display blocks of a `replace`
/// whose kind this build does not know, and returns their tags. `tool/progress`
/// is a command, and a command refuses what its schema does not list, but a
/// display block is only shown: the daemon drops such a block, with a
/// diagnostic, before it checks the rest of the command
/// (`docs/site/specs/compatibility-v1.md` section 12.4).
pub fn take_unknown_progress_blocks(params: &mut Value) -> Vec<String> {
    let Some(display) = params
        .pointer_mut("/progress/display")
        .filter(|display| display.get("type").and_then(Value::as_str) == Some("replace"))
    else {
        return Vec::new();
    };
    let Some(blocks) = display.get_mut("blocks").and_then(Value::as_array_mut) else {
        return Vec::new();
    };
    let mut unknown = Vec::new();
    blocks.retain(
        |block| match serde_json::from_value::<Open<DisplayBlock>>(block.clone()) {
            Ok(Open::Unknown { tag, .. }) => {
                unknown.push(tag);
                false
            }
            _ => true,
        },
    );
    unknown
}

// --- tool/detachable ---------------------------------------------------------

/// An executing call reached its detach point.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DetachableParams {
    pub prepared_id: u64,
    /// A line for the immediate result, such as a process ID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// The call reports its own results once detached.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub reports_itself: bool,
}

// --- errors ------------------------------------------------------------------------

/// A runtime API `ToolError` on the wire.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, schemars::JsonSchema)]
pub struct WireToolError {
    pub kind: ToolErrorKind,
    pub message: String,
}

/// Open: a kind this build does not know reads as `failed`, a generic tool
/// failure.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
#[schemars(extend("x-open" = true))]
pub enum ToolErrorKind {
    ProgressSinkClosed,
    OperationChanged,
    UnsupportedSecurity,
    UnsupportedPlatform,
    ResourceLimit,
    InvalidUrl,
    PermissionDenied,
    RedirectError,
    Timeout,
    TransportError,
    UnsupportedContentType,
    RuntimeUnavailable,
    #[serde(other)]
    Failed,
}

/// Lowercase hex of `bytes`.
#[must_use]
pub fn hex_encode(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        text.push(char::from(DIGITS[usize::from(byte >> 4)]));
        text.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    text
}

/// The bytes of lowercase hex `text`.
#[must_use]
pub fn hex_decode(text: &str) -> Option<Vec<u8>> {
    fn digit(byte: u8) -> Option<u8> {
        match byte {
            b'0'..=b'9' => Some(byte - b'0'),
            b'a'..=b'f' => Some(byte - b'a' + 10),
            _ => None,
        }
    }
    let mut bytes = text.bytes();
    let mut decoded = Vec::with_capacity(text.len() / 2);
    while let Some(high) = bytes.next() {
        let low = bytes.next()?;
        decoded.push(digit(high)? << 4 | digit(low)?);
    }
    Some(decoded)
}

// --- tool declaration enums -------------------------------------------------

/// How a tool behaves when another source offers the same name.
///
/// Open: a mode this build does not know reads as `prefix`.
#[derive(
    Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
#[schemars(extend("x-open" = true))]
pub enum ToolCollision {
    /// Hidden whenever a non-fallback tool with the same name is published.
    Fallback,
    /// Takes the bare name, and the tool it collides with is published under
    /// its own source's prefix (or hidden, if it is `fallback`). Two
    /// `override` tools with one name are both prefixed.
    Override,
    /// Takes the bare name while no other tool claims it. It is published as
    /// `<source>_<name>` when a `prefix` tool of another source has the same
    /// name, when one `override` tool takes the name, or when the name is a
    /// base-layer tool's.
    #[default]
    #[serde(other)]
    Prefix,
}

/// Declares whether calls to a tool may overlap with sibling calls from one model turn.
///
/// Open: a value this build does not know reads as `exclusive`, the most
/// conservative.
#[derive(
    Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
#[schemars(extend("x-open" = true))]
pub enum ToolConcurrency {
    Parallel,
    #[default]
    #[serde(other)]
    Exclusive,
}

/// Open: a policy this build does not know reads as `bounded`, so the
/// daemon still bounds the result.
#[derive(
    Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
#[schemars(extend("x-open" = true))]
pub enum ToolResultTruncationPolicy {
    OptOut,
    #[default]
    #[serde(other)]
    Bounded,
}

#[cfg(test)]
mod tests {
    use super::RuntimeMachine;

    fn machine(id: &str, host_name: Option<&str>) -> RuntimeMachine {
        RuntimeMachine {
            id: id.to_owned(),
            host_name: host_name.map(str::to_owned),
        }
    }

    #[test]
    fn a_home_is_an_absolute_path_of_either_platform() {
        for home in [
            "/",
            "/home/ada",
            r"C:\Users\ada",
            "C:/Users/ada",
            r"\\server\share\ada",
        ] {
            assert!(super::is_valid_home(home), "{home}");
        }
        let long = format!("/{}", "a".repeat(super::MAX_HOME_BYTES));
        for home in [
            "",
            "~",
            "~/x",
            "home/ada",
            "C:",
            "C:ada",
            "/home/a\nda",
            &long,
        ] {
            assert!(!super::is_valid_home(home), "{home:?}");
        }
        assert!(super::is_valid_home(&long[..super::MAX_HOME_BYTES]));
    }

    #[test]
    fn a_reported_machine_keeps_its_bounds() {
        assert!(machine("3f9c", Some("labpc")).is_valid());
        assert!(machine("3f9c", None).is_valid());
        assert!(machine(&"a".repeat(128), Some(&"h".repeat(255))).is_valid());
        assert!(!machine("", Some("labpc")).is_valid());
        assert!(!machine(&"a".repeat(129), None).is_valid());
        assert!(!machine("3f 9c", None).is_valid());
        assert!(!machine("3f9c", Some(&"h".repeat(256))).is_valid());
        assert!(!machine("3f9c", Some("lab\npc")).is_valid());
    }

    #[test]
    fn an_absent_machine_is_left_out_and_read_as_unreported() {
        let result: super::InitializeResult = serde_json::from_value(serde_json::json!({
            "protocol_version": "1.0.0",
            "capabilities": []
        }))
        .unwrap();
        assert_eq!(result.machine, None);
        let encoded = serde_json::to_value(&result).unwrap();
        assert!(encoded.get("machine").is_none(), "{encoded}");
    }

    #[test]
    fn a_runtime_reports_its_configured_servers_with_their_scope() {
        use super::{McpServerReport, McpServerScope, McpServerState, RuntimeMcpChangedParams};
        let result: super::InitializeResult = serde_json::from_value(serde_json::json!({
            "protocol_version": "1.5.0",
            "capabilities": [],
            "mcp_servers": [
                {"name": "browser", "state": "connecting", "scope": "runtime"},
                {"name": "lsp", "state": "disconnected", "scope": "workspace"}
            ]
        }))
        .unwrap();
        assert_eq!(
            result
                .mcp_servers
                .iter()
                .map(|server| (server.name.as_str(), server.scope, server.state))
                .collect::<Vec<_>>(),
            [
                (
                    "browser",
                    Some(McpServerScope::Runtime),
                    McpServerState::Connecting
                ),
                (
                    "lsp",
                    Some(McpServerScope::Workspace),
                    McpServerState::Disconnected
                ),
            ]
        );
        let round_trip: super::InitializeResult =
            serde_json::from_value(serde_json::to_value(&result).unwrap()).unwrap();
        assert_eq!(round_trip, result);

        // A runtime of 1.4 says nothing: no servers, and an instance's report
        // without a scope is workspace-scoped.
        let older: super::InitializeResult = serde_json::from_value(serde_json::json!({
            "protocol_version": "1.4.0",
            "capabilities": []
        }))
        .unwrap();
        assert!(older.mcp_servers.is_empty());
        assert!(
            serde_json::to_value(&older)
                .unwrap()
                .get("mcp_servers")
                .is_none()
        );
        let report: McpServerReport =
            serde_json::from_value(serde_json::json!({"name": "lsp", "state": "connected"}))
                .unwrap();
        assert_eq!(report.scope.unwrap_or_default(), McpServerScope::Workspace);
        assert!(
            serde_json::to_value(&report)
                .unwrap()
                .get("scope")
                .is_none()
        );
        // Open: a scope this build does not know reads as `workspace`.
        let future: McpServerScope = serde_json::from_value(serde_json::json!("machine")).unwrap();
        assert_eq!(future, McpServerScope::Workspace);

        let changed: RuntimeMcpChangedParams = serde_json::from_value(serde_json::json!({
            "servers": [{"name": "browser", "state": "failed", "scope": "runtime",
                         "message": "exited"}],
            "failures": [{"server": "browser", "reason": "exited"}]
        }))
        .unwrap();
        assert_eq!(changed.failures[0].server, "browser");
        assert_eq!(
            serde_json::from_value::<RuntimeMcpChangedParams>(
                serde_json::to_value(&changed).unwrap()
            )
            .unwrap(),
            changed
        );
    }
}
