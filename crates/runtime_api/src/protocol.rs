//! Wire types of the runtime protocol between the daemon and a runtime, as
//! specified in `docs/site/specs/runtimes.md` section 9 and documented in
//! `docs/site/reference/runtime-protocol.md`.
//!
//! The protocol is JSON-RPC 2.0, one message per frame: a line over stdio or
//! a frame of an in-memory transport. Every message is bounded by
//! [`MAX_FRAME_BYTES`] and every type rejects unknown fields. Both ends send
//! requests: the daemon drives tools, and a runtime reports progress and
//! background jobs back while a call runs. [`crate::rpc`] carries the
//! messages.

use cookie_agent_protocol::{
    AdaptorId, AgentId, AgentMdEntry, AgentMdSkipped, ModelCapabilities, ModelKey,
    PermissionAction, PersistedToolResult, PreparedOperationIdentity, RunId, SessionId,
    SkillSource, ToolCallId, ToolCallPresentation, ToolCompletionOutput, ToolOutputChunk,
    ToolOutputDeclaration,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    AdapterFamily, BackgroundReport, PromptSection, RuntimeKind, ToolCollision, ToolConcurrency,
    ToolError, ToolProgress, ToolResultTruncationPolicy, ToolSpec, TurnAgentContext,
};

/// The runtime protocol version. Both ends must speak exactly this version.
pub const RUNTIME_PROTOCOL_VERSION: u32 = 3;

/// The largest frame either end sends or accepts. It fits an `execute`
/// reply carrying [`MAX_INLINE_MEDIA_BYTES`] of base64 media next to a full
/// tool result.
pub const MAX_FRAME_BYTES: usize = 48 * 1024 * 1024;

/// The most media bytes, before base64, one `tool/execute` reply carries:
/// the largest single attachment a tool may return.
pub const MAX_INLINE_MEDIA_BYTES: u64 = crate::media::MAX_VIDEO_ATTACHMENT_BYTES;

/// The most calls one `tool/prepare` request carries.
pub const MAX_PREPARE_BATCH: usize = 256;

/// Method names.
pub mod method {
    /// daemon → runtime: version, identity, and settings; returns the
    /// runtime's identity, tools, and skills.
    pub const INITIALIZE: &str = "runtime/initialize";
    /// runtime → daemon (notification): the runtime's tool list changed.
    pub const TOOLS_CHANGED: &str = "runtime/tools_changed";
    /// daemon → runtime: per-admission context (`AGENTS.md`).
    pub const CONTEXT: &str = "runtime/context";
    /// daemon → runtime: stop gracefully.
    pub const SHUTDOWN: &str = "runtime/shutdown";
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
    /// runtime → daemon: an executing call reserves a background job.
    pub const JOB_STARTED: &str = "job/started";
    /// runtime → daemon: output of a background job.
    pub const JOB_PROGRESS: &str = "job/progress";
    /// runtime → daemon (notification): a background job finished.
    pub const JOB_COMPLETED: &str = "job/completed";
    /// daemon → runtime (notification): kill a background job.
    pub const JOB_KILL: &str = "job/kill";
    /// runtime → daemon (notification): the status of the runtime's MCP
    /// servers changed, or one failed.
    pub const MCP_CHANGED: &str = "mcp/changed";
    /// daemon → runtime: reconnect one MCP server, or retry every failed
    /// one.
    pub const MCP_RECONNECT: &str = "mcp/reconnect";
}

/// JSON-RPC error codes the runtime protocol uses besides the standard ones.
pub mod error_code {
    /// The params do not match the method.
    pub const INVALID_PARAMS: i32 = -32602;
    /// No such method.
    pub const METHOD_NOT_FOUND: i32 = -32601;
    /// The versions of the two ends differ.
    pub const VERSION_MISMATCH: i32 = -32010;
    /// The runtime rejects the initialize request or settings.
    pub const INITIALIZE_REJECTED: i32 = -32011;
    /// A request other than initialize arrived before initialize.
    pub const NOT_INITIALIZED: i32 = -32012;
    /// The prepared call or job the request names is unknown.
    pub const UNKNOWN_HANDLE: i32 = -32013;
    /// Reading the runtime's context failed; `data.path` names the file.
    pub const CONTEXT_FAILED: i32 = -32014;
    /// The reply would exceed the frame bound.
    pub const REPLY_TOO_LARGE: i32 = -32015;
}

/// An empty params or result object.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Empty {}

// --- runtime/initialize ---------------------------------------------------

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct InitializeParams {
    /// The daemon's [`RUNTIME_PROTOCOL_VERSION`].
    pub version: u32,
    /// The template's runtime ID.
    pub runtime_id: String,
    /// The workspace the daemon asks for, when the template has one.
    #[serde(default)]
    pub workspace: Option<String>,
    pub settings: RuntimeSettings,
}

/// What the daemon's configuration says about the runtime's work. A runtime
/// rejects settings it does not support.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
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

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ToolOutputLimits {
    pub max_lines: u64,
    pub max_bytes: u64,
    pub head_lines: u64,
    pub tail_lines: u64,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AgentMdLimits {
    /// Files larger than this are skipped. At most
    /// [`crate::context::AGENT_MD_MAX_BYTES`].
    pub max_bytes: u64,
}

/// One MCP server of a runtime template.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct McpServerSettings {
    pub name: String,
    /// The server's table, as the `[mcp.servers.<name>]` schema reads it.
    pub config: Value,
}

/// The status of one MCP server a runtime hosts.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct McpServerReport {
    pub name: String,
    pub state: McpServerState,
    /// Why the server failed, or what it is doing.
    #[serde(default)]
    pub message: Option<String>,
    /// The names of its tools as the runtime declares them.
    #[serde(default)]
    pub tools: Vec<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum McpServerState {
    Disabled,
    Disconnected,
    Connecting,
    Connected,
    /// The connection dropped and is being re-established: its tools stay
    /// declared and calls fail with `runtime_unavailable`.
    Reconnecting,
    Failed,
}

/// An MCP server that failed to connect, or dropped and did not reconnect.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct McpFailureReport {
    pub server: String,
    pub reason: String,
}

// --- mcp/changed, mcp/reconnect --------------------------------------------------

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct McpChangedParams {
    /// Every server the runtime hosts, in name order.
    pub servers: Vec<McpServerReport>,
    /// Servers that failed since the last report, in order.
    #[serde(default)]
    pub failures: Vec<McpFailureReport>,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct McpReconnectParams {
    /// The server to reconnect now; absent to retry every failed server.
    #[serde(default)]
    pub server: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct InitializeResult {
    /// The runtime's [`RUNTIME_PROTOCOL_VERSION`].
    pub version: u32,
    pub kind: RuntimeKind,
    /// The name sessions attach the instance under.
    pub attachment_name: String,
    /// The canonical workspace on the runtime's machine.
    #[serde(default)]
    pub workspace: Option<String>,
    /// Operating system, as Rust's `std::env::consts::OS` names it, for a
    /// runtime where it matters.
    #[serde(default)]
    pub platform: Option<String>,
    pub tools: RuntimeTools,
    pub skills: RuntimeSkills,
    /// The template's MCP servers after their first connection attempt.
    /// Their tools are among `tools`.
    #[serde(default)]
    pub mcp_servers: Vec<McpServerReport>,
}

/// A runtime's tools and their prompt sections.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeTools {
    pub tools: Vec<ToolDeclaration>,
    #[serde(default)]
    pub prompt_sections: Vec<PromptSectionDeclaration>,
}

/// A tool as a runtime declares it.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
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
}

impl ToolDeclaration {
    /// The declaration of `spec`, whose permission name, when it has one,
    /// must be a permission action.
    pub fn from_spec(spec: &ToolSpec) -> Result<Self, String> {
        Ok(Self {
            name: spec.name.clone(),
            collision: spec.collision,
            permission: spec
                .permission_name
                .as_deref()
                .map(|name| {
                    permission_action(name).ok_or_else(|| {
                        format!(
                            "tool `{}` has permission `{name}`, which is not a permission action",
                            spec.name
                        )
                    })
                })
                .transpose()?,
            description: spec.description.clone(),
            parameters: spec.parameters.clone(),
            concurrency: spec.concurrency,
            result_truncation: spec.result_truncation,
            output: spec.output.clone(),
        })
    }

    #[must_use]
    pub fn into_spec(self) -> ToolSpec {
        ToolSpec {
            name: self.name,
            collision: self.collision,
            permission_name: self
                .permission
                .map(|permission| permission.as_str().to_owned()),
            description: self.description,
            parameters: self.parameters,
            concurrency: self.concurrency,
            result_truncation: self.result_truncation,
            output: self.output,
        }
    }
}

/// The permission action a tool spec's permission name spells: any valid
/// action label. `plugin:<name>` and the reserved unconditional name are not
/// actions, so no runtime can declare them.
#[must_use]
pub fn permission_action(name: &str) -> Option<PermissionAction> {
    PermissionAction::new(name).ok()
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PromptSectionDeclaration {
    pub title: String,
    pub body: String,
}

impl From<PromptSection> for PromptSectionDeclaration {
    fn from(section: PromptSection) -> Self {
        Self {
            title: section.title,
            body: section.body,
        }
    }
}

impl From<PromptSectionDeclaration> for PromptSection {
    fn from(section: PromptSectionDeclaration) -> Self {
        Self {
            title: section.title,
            body: section.body,
        }
    }
}

/// Skills a runtime contributes, read once at initialize.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeSkills {
    /// Skills that won precedence, by name.
    pub skills: Vec<SkillDeclaration>,
    /// Every `SKILL.md` found, winners and shadowed ones.
    pub discoveries: Vec<SkillDiscoveryDeclaration>,
}

/// One skill: its `SKILL.md` text and where it lives on the runtime's
/// machine.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
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

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
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

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ContextParams {
    /// The run's agent preset.
    #[serde(default)]
    pub preset: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ContextResult {
    pub agent_md: Vec<AgentMdEntry>,
    pub skipped: Vec<AgentMdSkipped>,
}

// --- tool/prepare -------------------------------------------------------------

/// The agent and model context of one tool batch.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WireTurnContext {
    pub agent: AgentId,
    pub model: ModelKey,
    pub adapter: AdaptorId,
    pub adapter_family: AdapterFamily,
    pub capabilities: ModelCapabilities,
}

impl From<&TurnAgentContext> for WireTurnContext {
    fn from(context: &TurnAgentContext) -> Self {
        Self {
            agent: context.agent.clone(),
            model: context.model.clone(),
            adapter: context.adapter,
            adapter_family: context.adapter_family,
            capabilities: context.capabilities.clone(),
        }
    }
}

impl From<WireTurnContext> for TurnAgentContext {
    fn from(context: WireTurnContext) -> Self {
        Self {
            agent: context.agent,
            model: context.model,
            adapter: context.adapter,
            adapter_family: context.adapter_family,
            capabilities: context.capabilities,
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WireToolCall {
    pub id: ToolCallId,
    /// The name the runtime knows the tool by.
    pub name: String,
    pub arguments: Value,
}

/// One turn's calls to one runtime, prepared together so calls on the same
/// target chain.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PrepareParams {
    pub session: SessionId,
    pub run: RunId,
    pub turn: WireTurnContext,
    pub calls: Vec<WireToolCall>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PrepareResult {
    /// One outcome per call, in order.
    pub results: Vec<PrepareOutcome>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum PrepareOutcome {
    Prepared(PreparedCall),
    Failed {
        error: WireToolError,
        /// How clients show the call that failed to prepare.
        presentation: ToolCallPresentation,
    },
}

/// A prepared call. The runtime keeps its executor under `prepared_id`
/// until it is executed or released.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PreparedCall {
    pub prepared_id: u64,
    pub operation: PreparedOperationIdentity,
    /// The policy labels the tool set, one per operation resource.
    pub policy_labels: Vec<Option<String>>,
    pub normalized_arguments: Value,
    /// The permission action the call is checked under; absent for a tool
    /// that needs no permission.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission: Option<PermissionAction>,
    /// The permission resource of the normalized arguments.
    #[serde(default)]
    pub permission_resource: Option<String>,
    pub presentation: ToolCallPresentation,
    /// Calls with one key run one at a time; lowercase hex.
    #[serde(default)]
    pub serialization_key: Option<String>,
}

/// Names one prepared call.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PreparedRef {
    pub prepared_id: u64,
}

/// The outcome of a host service or a check: absent error means success.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Outcome {
    #[serde(default)]
    pub error: Option<WireToolError>,
}

impl Outcome {
    #[must_use]
    pub fn of(result: Result<(), ToolError>) -> Self {
        Self {
            error: result.err().as_ref().map(WireToolError::from),
        }
    }

    pub fn into_result(self) -> Result<(), ToolError> {
        self.error.map_or(Ok(()), |error| Err(error.into()))
    }
}

// --- tool/execute -------------------------------------------------------------

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecuteParams {
    pub prepared_id: u64,
    pub session: SessionId,
    pub run: RunId,
    pub turn: WireTurnContext,
    /// The call may start background jobs.
    pub background: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecuteResult {
    pub outcome: ExecuteOutcome,
    /// Bytes of the attachments the result references, for the daemon to
    /// retain. At most [`MAX_INLINE_MEDIA_BYTES`] in all.
    #[serde(default)]
    pub media: Vec<InlineMedia>,
    /// Jobs the call reserved with `job/started` and launched; the daemon
    /// releases the others.
    #[serde(default)]
    pub launched_jobs: Vec<u64>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
#[allow(clippy::large_enum_variant)]
pub enum ExecuteOutcome {
    Completed {
        output: ToolCompletionOutput,
        result: PersistedToolResult,
        failed: bool,
    },
    Failed {
        error: WireToolError,
    },
}

/// Attachment bytes, content-addressed.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
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
pub fn artifact_reference(sha256: &str) -> cookie_agent_protocol::ArtifactReference {
    cookie_agent_protocol::ArtifactReference {
        uri: format!("artifact://sha256/{sha256}"),
    }
}

// --- tool/progress ---------------------------------------------------------------

/// Display and output deltas, bounded like in-process progress.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WireProgress {
    pub tool_call_id: ToolCallId,
    pub message: String,
    #[serde(default)]
    pub display: Option<String>,
    #[serde(default)]
    pub output: Vec<ToolOutputChunk>,
}

impl From<ToolProgress> for WireProgress {
    fn from(progress: ToolProgress) -> Self {
        Self {
            tool_call_id: progress.tool_call_id,
            message: progress.message,
            display: progress.display,
            output: progress.output,
        }
    }
}

impl From<WireProgress> for ToolProgress {
    fn from(progress: WireProgress) -> Self {
        Self {
            tool_call_id: progress.tool_call_id,
            message: progress.message,
            display: progress.display,
            output: progress.output,
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ToolProgressParams {
    pub prepared_id: u64,
    pub progress: WireProgress,
}

// --- job/* ---------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct JobStartedParams {
    /// The executing call that reserves the job.
    pub prepared_id: u64,
    /// The runtime's ID for the job.
    pub job: u64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct JobProgressParams {
    pub job: u64,
    pub progress: WireProgress,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct JobCompletedParams {
    pub job: u64,
    pub report: WireBackgroundReport,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct JobRef {
    pub job: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WireBackgroundReport {
    pub summary: String,
    pub element: String,
    pub attributes: Vec<WireAttribute>,
    pub body: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WireAttribute {
    pub name: String,
    pub value: String,
}

impl From<BackgroundReport> for WireBackgroundReport {
    fn from(report: BackgroundReport) -> Self {
        Self {
            summary: report.summary,
            element: report.element,
            attributes: report
                .attributes
                .into_iter()
                .map(|(name, value)| WireAttribute { name, value })
                .collect(),
            body: report.body,
        }
    }
}

impl From<WireBackgroundReport> for BackgroundReport {
    fn from(report: WireBackgroundReport) -> Self {
        Self {
            summary: report.summary,
            element: report.element,
            attributes: report
                .attributes
                .into_iter()
                .map(|attribute| (attribute.name, attribute.value))
                .collect(),
            body: report.body,
        }
    }
}

// --- errors ------------------------------------------------------------------------

/// A [`ToolError`] on the wire.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WireToolError {
    pub kind: ToolErrorKind,
    pub message: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolErrorKind {
    ProgressSinkClosed,
    Failed,
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
}

impl From<&ToolError> for WireToolError {
    fn from(error: &ToolError) -> Self {
        let kind = match error {
            ToolError::ProgressSinkClosed => ToolErrorKind::ProgressSinkClosed,
            ToolError::Failed(_) => ToolErrorKind::Failed,
            ToolError::OperationChanged(_) => ToolErrorKind::OperationChanged,
            ToolError::UnsupportedSecurity(_) => ToolErrorKind::UnsupportedSecurity,
            ToolError::UnsupportedPlatform(_) => ToolErrorKind::UnsupportedPlatform,
            ToolError::ResourceLimit(_) => ToolErrorKind::ResourceLimit,
            ToolError::InvalidUrl(_) => ToolErrorKind::InvalidUrl,
            ToolError::PermissionDenied(_) => ToolErrorKind::PermissionDenied,
            ToolError::RedirectError(_) => ToolErrorKind::RedirectError,
            ToolError::Timeout(_) => ToolErrorKind::Timeout,
            ToolError::TransportError(_) => ToolErrorKind::TransportError,
            ToolError::UnsupportedContentType(_) => ToolErrorKind::UnsupportedContentType,
            ToolError::RuntimeUnavailable(_) => ToolErrorKind::RuntimeUnavailable,
        };
        Self {
            kind,
            message: error.message(),
        }
    }
}

impl From<WireToolError> for ToolError {
    fn from(error: WireToolError) -> Self {
        let message = error.message;
        match error.kind {
            ToolErrorKind::ProgressSinkClosed => Self::ProgressSinkClosed,
            ToolErrorKind::Failed => Self::Failed(message),
            ToolErrorKind::OperationChanged => Self::OperationChanged(message),
            ToolErrorKind::UnsupportedSecurity => Self::UnsupportedSecurity(message),
            ToolErrorKind::UnsupportedPlatform => Self::UnsupportedPlatform(message),
            ToolErrorKind::ResourceLimit => Self::ResourceLimit(message),
            ToolErrorKind::InvalidUrl => Self::InvalidUrl(message),
            ToolErrorKind::PermissionDenied => Self::PermissionDenied(message),
            ToolErrorKind::RedirectError => Self::RedirectError(message),
            ToolErrorKind::Timeout => Self::Timeout(message),
            ToolErrorKind::TransportError => Self::TransportError(message),
            ToolErrorKind::UnsupportedContentType => Self::UnsupportedContentType(message),
            ToolErrorKind::RuntimeUnavailable => Self::RuntimeUnavailable(message),
        }
    }
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
