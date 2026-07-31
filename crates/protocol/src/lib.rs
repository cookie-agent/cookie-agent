//! Versioned JSON-RPC wire types shared by CookieCode clients and the daemon.
//!
//! Data-carrying wire enums use an internally tagged, `snake_case`
//! representation with a `type` discriminator; unit enums are `snake_case`
//! strings. [`DepthLimit`] uses adjacent `kind`/`value` tags, while JSON-RPC
//! IDs and responses are untagged as required by JSON-RPC 2.0.

use std::{fmt, str::FromStr};

use jiff::Timestamp;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use ts_rs::TS;
use uuid::Uuid;

/// The only protocol version supported by this build.
pub const PROTOCOL_VERSION: u32 = 1;

macro_rules! uuid_id {
    ($name:ident) => {
        #[derive(
            Clone, Copy, Debug, Deserialize, Eq, Hash, JsonSchema, PartialEq, Serialize, TS,
        )]
        #[serde(transparent)]
        pub struct $name(pub Uuid);

        impl $name {
            #[must_use]
            pub fn new_v7() -> Self {
                Self(Uuid::now_v7())
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(formatter)
            }
        }

        impl FromStr for $name {
            type Err = uuid::Error;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                value.parse().map(Self)
            }
        }
    };
}

uuid_id!(SessionId);
uuid_id!(RunId);
uuid_id!(ToolCallId);
uuid_id!(InvocationId);

/// A JSON-RPC request identifier. JSON-RPC allows strings, numbers, or null.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(untagged)]
pub enum JsonRpcId {
    Null,
    Number(i64),
    String(String),
}

/// A JSON-RPC 2.0 request envelope.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize, TS)]
pub struct Request {
    pub jsonrpc: String,
    pub id: JsonRpcId,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

impl Request {
    #[must_use]
    pub fn new(id: JsonRpcId, method: impl Into<String>, params: Option<Value>) -> Self {
        Self {
            jsonrpc: "2.0".to_owned(),
            id,
            method: method.into(),
            params,
        }
    }
}

/// A successful JSON-RPC 2.0 response envelope.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize, TS)]
pub struct SuccessResponse {
    pub jsonrpc: String,
    pub id: JsonRpcId,
    pub result: Value,
}

/// A failed JSON-RPC 2.0 response envelope.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize, TS)]
pub struct ErrorResponse {
    pub jsonrpc: String,
    pub id: JsonRpcId,
    pub error: JsonRpcError,
}

/// A JSON-RPC response. The untagged representation preserves the standard
/// `result`-or-`error` response shape.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize, TS)]
#[serde(untagged)]
pub enum Response {
    Success(SuccessResponse),
    Error(ErrorResponse),
}

/// JSON-RPC error details.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize, TS)]
pub struct JsonRpcError {
    pub code: i32,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

/// A JSON-RPC 2.0 notification envelope.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize, TS)]
pub struct Notification {
    pub jsonrpc: String,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

impl Notification {
    #[must_use]
    pub fn new(method: impl Into<String>, params: Option<Value>) -> Self {
        Self {
            jsonrpc: "2.0".to_owned(),
            method: method.into(),
            params,
        }
    }
}

/// Initial handshake payload sent by a client before issuing protocol calls.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
pub struct ClientHello {
    pub protocol_version: u32,
}

/// Handshake payload returned by a server that accepted a client version.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
pub struct ServerHello {
    pub protocol_version: u32,
}

/// A model entry in a profile fallback chain.
#[derive(Clone, Debug, Deserialize, Eq, Hash, JsonSchema, PartialEq, Serialize, TS)]
pub struct ModelRef {
    pub provider: String,
    pub model: String,
}

/// Whether a profile can create root sessions, delegated sessions, both, or neither.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "lowercase")]
pub enum AgentType {
    #[default]
    All,
    Primary,
    #[serde(rename = "subagent")]
    SubAgent,
    Internal,
}

/// The frozen delegation portion of a session profile.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
pub struct DelegationSnapshot {
    pub enabled: bool,
    pub allowed_profiles: Vec<String>,
    pub depth_limit: DepthLimit,
    pub result_limit_bytes: u64,
}

/// A resolved profile frozen when its session is created.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize, TS)]
pub struct ProfileSnapshot {
    pub name: String,
    pub agent_type: AgentType,
    pub models: Vec<ModelRef>,
    pub tools: Vec<String>,
    pub delegation: DelegationSnapshot,
    pub permission_rules: Vec<PermissionRule>,
}

/// The creation provenance of a session.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionOrigin {
    Root,
    Delegated {
        root_session_id: SessionId,
        parent_session_id: SessionId,
        parent_run_id: RunId,
        parent_tool_call_id: ToolCallId,
        invocation_id: InvocationId,
        depth: u32,
    },
    Forked {
        source_session_id: SessionId,
        source_event_seq: u64,
    },
}

/// Session metadata cached for querying; the event log remains authoritative.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize, TS)]
pub struct SessionMeta {
    pub id: SessionId,
    pub origin: SessionOrigin,
    pub cwd: String,
    pub profile: ProfileSnapshot,
}

/// The current terminal or active status presented in session tree projections.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Idle,
    Running,
    Completed,
    Failed,
    Cancelled,
    Interrupted,
}

/// Aggregate model usage attributed to a child session.
#[derive(Clone, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cached_input_tokens: Option<u64>,
}

/// A lightweight delegated child projection.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize, TS)]
pub struct ChildSummary {
    pub id: SessionId,
    pub profile: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_excerpt: Option<String>,
    pub status: SessionStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
}

/// A recursive projection of delegated children. Forks are intentionally absent.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize, TS)]
pub struct SessionTree {
    pub session: SessionMeta,
    pub children: Vec<SessionTree>,
}

/// The number of further delegation generations allowed beneath a session.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum DepthLimit {
    Finite(u32),
    Unlimited,
}

impl DepthLimit {
    /// Returns whether this session may expose the delegate tool.
    #[must_use]
    pub const fn allows_delegation(self) -> bool {
        !matches!(self, Self::Finite(0))
    }

    /// Calculates a child's frozen limit from this parent and the child's
    /// configured limit (`None` means the child is configured as unlimited).
    ///
    /// This must only be called after [`Self::allows_delegation`] succeeds.
    #[must_use]
    pub const fn child_limit(self, configured: Option<u32>) -> Self {
        match (configured, self) {
            (Some(child), Self::Finite(parent)) => {
                let remaining = parent - 1;
                Self::Finite(if child < remaining { child } else { remaining })
            }
            (Some(child), Self::Unlimited) => Self::Finite(child),
            (None, Self::Finite(parent)) => Self::Finite(parent - 1),
            (None, Self::Unlimited) => Self::Unlimited,
        }
    }
}

/// A permission capability used by configured rules and runtime approvals.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum ActionKind {
    Read,
    Write,
    Bash,
    Grep,
    Glob,
    List,
    Delegate,
    ExternalDirectory,
}

/// The result of a configured permission rule evaluation.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum Effect {
    Allow,
    Ask,
    Deny,
}

/// One ordered configured permission rule.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
pub struct PermissionRule {
    pub id: String,
    pub action: ActionKind,
    pub resource: String,
    pub effect: Effect,
    pub hard: bool,
}

/// A candidate rule retained for client-visible permission explanations.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
pub struct MatchedPermissionRule {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rule_id: Option<String>,
    pub source_layer: String,
    pub effect: Effect,
    pub hard: bool,
}

/// Complete derivation of a permission decision.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
pub struct DecisionTrace {
    pub action: ActionKind,
    pub normalized_resource: String,
    pub candidates: Vec<MatchedPermissionRule>,
    pub effect: Effect,
    pub precedence_reason: String,
}

/// One resource disclosed by an approval request.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
pub struct ApprovalResource {
    pub action: ActionKind,
    pub resource: String,
    pub suggested_pattern: String,
}

/// One runtime approval scope granted by an `always` response.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
pub struct ApprovedScope {
    pub action: ActionKind,
    pub resource: String,
    pub scope: String,
}

/// A user's answer to a pending approval request.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalDecision {
    Once,
    Always,
    Reject,
}

/// The result sent back to the model when a tool call completes.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize, TS)]
pub struct ToolResult {
    pub content: String,
    pub truncated: bool,
}

/// The exact provider wire protocol that produced an opaque assistant artifact.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum ProviderProtocol {
    AnthropicMessages,
    OpenAiChatCompletions,
    OpenAiResponses,
    OpenAiCompatible,
}

/// Provider-native state needed to replay an assistant turn exactly.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize, TS)]
pub struct TurnOpaque {
    pub provider: ProviderProtocol,
    pub payload: Value,
}

/// A durable event payload from a session event log.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize, TS)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    SessionCreated {
        meta: SessionMeta,
    },
    RunStarted {
        client_run_id: String,
        input: String,
    },
    /// Model-visible input injected into an already-running turn.
    UserInputSubmitted {
        input: String,
    },
    /// Durable association of accepted steering with the provider attempt that
    /// consumed it. `user_input_seq` references `UserInputSubmitted`.
    UserInputApplied {
        user_input_seq: u64,
    },
    RunCompleted {
        #[serde(skip_serializing_if = "Option::is_none")]
        final_text: Option<String>,
    },
    RunFailed {
        message: String,
    },
    RunCancelled {
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    RunInterrupted {
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    TextDelta {
        text: String,
    },
    ReasoningDelta {
        text: String,
    },
    ToolCallStarted {
        tool_call_id: ToolCallId,
        tool: String,
        arguments: Value,
        /// The provider-native call ID, retained for same-protocol replay.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_tool_call_id: Option<String>,
        /// Protocol that issued `provider_tool_call_id`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_protocol: Option<ProviderProtocol>,
    },
    ToolCallProgress {
        tool_call_id: ToolCallId,
        message: String,
    },
    ToolCallCompleted {
        tool_call_id: ToolCallId,
        result: ToolResult,
    },
    ToolCallFailed {
        tool_call_id: ToolCallId,
        message: String,
    },
    ApprovalRequested {
        approval_id: String,
        action: ActionKind,
        resource: String,
        suggested_pattern: String,
        #[serde(default)]
        resources: Vec<ApprovalResource>,
        decision_trace: DecisionTrace,
    },
    ApprovalResolved {
        approval_id: String,
        decision: ApprovalDecision,
        #[serde(skip_serializing_if = "Option::is_none")]
        approved_scope: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        approved_scopes: Vec<ApprovedScope>,
        #[serde(skip_serializing_if = "Option::is_none")]
        feedback: Option<String>,
    },
    ToolStdinSubmitted {
        tool_call_id: ToolCallId,
        byte_count: u64,
    },
    ToolCallLinked {
        tool_call_id: ToolCallId,
        child_session_id: SessionId,
    },
    /// Boundary after provider output from an attempt that did not commit.
    AttemptAbandoned,
    /// Opaque native assistant state emitted by a provider at turn finalization.
    TurnOpaque {
        state: TurnOpaque,
    },
    ModelFallback {
        from: ModelRef,
        to: ModelRef,
        reason: String,
        attempts: u32,
    },
    UsageReported {
        model: ModelRef,
        usage: Usage,
    },
}

/// A persisted event with authoritative per-session ordering metadata.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize, TS)]
pub struct EventEnvelope {
    pub session_id: SessionId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_id: Option<RunId>,
    pub seq: u64,
    pub timestamp: Timestamp,
    pub event: Event,
}

/// A live persisted-event subscription message.
///
/// `Gap` is never persisted. Its `last_delivered_seq` is an exclusive replay
/// cursor: clients must re-subscribe with that cursor to receive the first
/// omitted event. `session_id` identifies the subscription whose tail lagged.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize, TS)]
#[serde(tag = "type", rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)]
pub enum EventSubscriptionMessage {
    Event {
        event: EventEnvelope,
    },
    Gap {
        session_id: SessionId,
        last_delivered_seq: u64,
    },
}

/// A tool output channel. Byte offsets are independent for each stream.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum OutputStream {
    Stdout,
    Stderr,
}

/// Ephemeral base64-encoded tool output. This type is never persisted or
/// cursor-replayed; `byte_offset` counts decoded bytes.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
pub struct OutputDelta {
    pub call_id: ToolCallId,
    pub stream: OutputStream,
    pub byte_offset: u64,
    pub data: String,
}

/// Ephemeral marker indicating a subscriber missed evicted or queued output.
/// This type is never persisted or cursor-replayed.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
pub struct OutputGap {
    pub call_id: ToolCallId,
    pub stream: OutputStream,
    pub next_offset: u64,
}

/// Ephemeral buffered output used for atomic snapshot-to-live handoff. This
/// type is never persisted or cursor-replayed.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
pub struct OutputSnapshot {
    pub call_id: ToolCallId,
    pub start_offset: u64,
    pub end_offset: u64,
    pub chunks: Vec<OutputDelta>,
}

/// A retained output snapshot paired with the stream it represents. This is
/// required on the wire because an empty snapshot has no chunk from which a
/// client could otherwise determine whether it is stdout or stderr.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
pub struct OutputSnapshotEnvelope {
    pub stream: OutputStream,
    pub snapshot: OutputSnapshot,
}

/// Parameters for `session.create`.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
pub struct SessionCreateParams {
    pub cwd: String,
    pub profile: String,
}

/// Result for `session.create`.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize, TS)]
pub struct SessionCreateResult {
    pub session: SessionMeta,
}

/// Parameters for `session.list`.
#[derive(Clone, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
pub struct SessionListParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}

/// Result for `session.list`.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize, TS)]
pub struct SessionListResult {
    pub sessions: Vec<SessionMeta>,
}

/// Parameters for `session.get`.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
pub struct SessionGetParams {
    pub session_id: SessionId,
}

/// Result for `session.get`.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize, TS)]
pub struct SessionGetResult {
    pub session: SessionMeta,
}

/// Parameters for `session.children`.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
pub struct SessionChildrenParams {
    pub session_id: SessionId,
}

/// Result for `session.children`.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize, TS)]
pub struct SessionChildrenResult {
    pub children: Vec<ChildSummary>,
}

/// Parameters for `session.tree`.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
pub struct SessionTreeParams {
    pub session_id: SessionId,
}

/// Result for `session.tree`.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize, TS)]
pub struct SessionTreeResult {
    pub tree: SessionTree,
}

/// Parameters for `session.resume`.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
pub struct SessionResumeParams {
    pub session_id: SessionId,
}

/// Result for `session.resume`.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize, TS)]
pub struct SessionResumeResult {
    pub session: SessionMeta,
}

/// Post-MVP parameters for `session.fork`.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
pub struct SessionForkParams {
    pub source_session_id: SessionId,
    pub source_event_seq: u64,
}

/// Post-MVP result for `session.fork`.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize, TS)]
pub struct SessionForkResult {
    pub session: SessionMeta,
}

/// Parameters for `run.start`. `client_run_id` is an idempotency key.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
pub struct RunStartParams {
    pub session_id: SessionId,
    pub client_run_id: String,
    pub input: String,
}

/// Stable error discriminator for a reused `run.start` idempotency key whose
/// parameters differ from the request that first claimed it.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum RunStartConflictCode {
    IdempotencyConflict,
}

/// JSON-RPC error data for a conflicting `run.start` idempotency request.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
pub struct RunStartConflict {
    pub code: RunStartConflictCode,
    pub session_id: SessionId,
    pub client_run_id: String,
}

/// Result for `run.start`.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
pub struct RunStartResult {
    pub run_id: RunId,
}

/// Parameters for `run.steer`.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
pub struct RunSteerParams {
    pub run_id: RunId,
    pub input: String,
}

/// Result for `run.steer`.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
pub struct RunSteerResult {
    pub accepted: bool,
}

/// Parameters for `run.cancel`.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
pub struct RunCancelParams {
    pub run_id: RunId,
}

/// Result for `run.cancel`.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
pub struct RunCancelResult {
    pub cancelled: bool,
}

/// Parameters for `run.tool_stdin`. `data`, when present, is base64 bytes.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
pub struct RunToolStdinParams {
    pub run_id: RunId,
    pub call_id: ToolCallId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<String>,
    pub eof: bool,
}

/// Result for `run.tool_stdin`.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
pub struct RunToolStdinResult {
    pub accepted: bool,
}

/// Parameters for `events.subscribe`. The optional cursor is a persisted-event
/// sequence number; ephemeral output is not replayed through it.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
pub struct EventsSubscribeParams {
    pub session_id: SessionId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<u64>,
}

/// Initial replay result for `events.subscribe`; future events arrive as
/// notifications.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize, TS)]
pub struct EventsSubscribeResult {
    pub events: Vec<EventEnvelope>,
}

/// Parameters for `approval.respond`. `scope` carries an optionally edited
/// suggested pattern when granting `always`.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
pub struct ApprovalRespondParams {
    /// The session which owns the pending approval.
    pub session_id: SessionId,
    pub approval_id: String,
    pub decision: ApprovalDecision,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub feedback: Option<String>,
}

/// Result for `approval.respond`.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
pub struct ApprovalRespondResult {
    pub approval_id: String,
    pub decision: ApprovalDecision,
}

/// Parameters for `provider.list_models`.
#[derive(Clone, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
pub struct ProviderListModelsParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
}

/// One model advertised by a provider.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
pub struct ModelDescriptor {
    pub model: ModelRef,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
}

/// Result for `provider.list_models`.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
pub struct ProviderListModelsResult {
    pub models: Vec<ModelDescriptor>,
}

/// Parameters for `agent.list`.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
pub struct AgentListParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_type: Option<AgentType>,
}

/// An agent profile descriptor returned by `agent.list`.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
pub struct AgentDescriptor {
    pub name: String,
    pub agent_type: AgentType,
    pub enabled: bool,
    pub models: Vec<ModelRef>,
}

/// Result for `agent.list`.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
pub struct AgentListResult {
    pub agents: Vec<AgentDescriptor>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use insta::assert_json_snapshot;
    use schemars::schema_for;

    fn round_trip<T>(value: T)
    where
        T: std::fmt::Debug + PartialEq + Serialize + for<'de> Deserialize<'de>,
    {
        let json = serde_json::to_string(&value).expect("serialize test value");
        let decoded = serde_json::from_str(&json).expect("deserialize test value");
        assert_eq!(value, decoded);
    }

    fn session_id() -> SessionId {
        SessionId(Uuid::from_u128(1))
    }

    fn run_id() -> RunId {
        RunId(Uuid::from_u128(2))
    }

    fn call_id() -> ToolCallId {
        ToolCallId(Uuid::from_u128(3))
    }

    fn profile() -> ProfileSnapshot {
        ProfileSnapshot {
            name: "primary".into(),
            agent_type: AgentType::Primary,
            models: vec![ModelRef {
                provider: "test".into(),
                model: "model".into(),
            }],
            tools: vec!["read".into()],
            delegation: DelegationSnapshot {
                enabled: true,
                allowed_profiles: vec!["worker".into()],
                depth_limit: DepthLimit::Finite(2),
                result_limit_bytes: 1024,
            },
            permission_rules: vec![],
        }
    }

    fn meta() -> SessionMeta {
        SessionMeta {
            id: session_id(),
            origin: SessionOrigin::Root,
            cwd: "/workspace".into(),
            profile: profile(),
        }
    }

    fn trace() -> DecisionTrace {
        DecisionTrace {
            action: ActionKind::Bash,
            normalized_resource: "git status".into(),
            candidates: vec![MatchedPermissionRule {
                rule_id: Some("status".into()),
                source_layer: "profile".into(),
                effect: Effect::Allow,
                hard: false,
            }],
            effect: Effect::Allow,
            precedence_reason: "last matching rule".into(),
        }
    }

    fn envelope() -> EventEnvelope {
        EventEnvelope {
            session_id: session_id(),
            run_id: Some(run_id()),
            seq: 3,
            timestamp: Timestamp::now(),
            event: Event::TextDelta {
                text: "text".into(),
            },
        }
    }

    #[test]
    fn serde_round_trips_all_enums() {
        round_trip(JsonRpcId::Null);
        round_trip(JsonRpcId::Number(1));
        round_trip(JsonRpcId::String("request".into()));
        round_trip(RunStartConflictCode::IdempotencyConflict);
        round_trip(RunStartConflict {
            code: RunStartConflictCode::IdempotencyConflict,
            session_id: session_id(),
            client_run_id: "client-run".into(),
        });
        round_trip(Response::Success(SuccessResponse {
            jsonrpc: "2.0".into(),
            id: JsonRpcId::Number(1),
            result: Value::Null,
        }));
        round_trip(Response::Error(ErrorResponse {
            jsonrpc: "2.0".into(),
            id: JsonRpcId::String("request".into()),
            error: JsonRpcError {
                code: -1,
                message: "error".into(),
                data: None,
            },
        }));
        round_trip(SessionOrigin::Root);
        round_trip(SessionOrigin::Delegated {
            root_session_id: session_id(),
            parent_session_id: SessionId(Uuid::from_u128(4)),
            parent_run_id: run_id(),
            parent_tool_call_id: call_id(),
            invocation_id: InvocationId(Uuid::from_u128(5)),
            depth: 1,
        });
        round_trip(SessionOrigin::Forked {
            source_session_id: session_id(),
            source_event_seq: 6,
        });
        round_trip(DepthLimit::Finite(1));
        round_trip(DepthLimit::Unlimited);
        round_trip(AgentType::All);
        round_trip(AgentType::Primary);
        round_trip(AgentType::SubAgent);
        round_trip(AgentType::Internal);
        for status in [
            SessionStatus::Idle,
            SessionStatus::Running,
            SessionStatus::Completed,
            SessionStatus::Failed,
            SessionStatus::Cancelled,
            SessionStatus::Interrupted,
        ] {
            round_trip(status);
        }
        for action in [
            ActionKind::Read,
            ActionKind::Write,
            ActionKind::Bash,
            ActionKind::Grep,
            ActionKind::Glob,
            ActionKind::List,
            ActionKind::Delegate,
            ActionKind::ExternalDirectory,
        ] {
            round_trip(action);
        }
        for effect in [Effect::Allow, Effect::Ask, Effect::Deny] {
            round_trip(effect);
        }
        for decision in [
            ApprovalDecision::Once,
            ApprovalDecision::Always,
            ApprovalDecision::Reject,
        ] {
            round_trip(decision);
        }
        round_trip(OutputStream::Stdout);
        round_trip(OutputStream::Stderr);
    }

    #[test]
    fn serde_round_trips_every_event_variant() {
        let model = ModelRef {
            provider: "test".into(),
            model: "model".into(),
        };
        let result = ToolResult {
            content: "done".into(),
            truncated: false,
        };
        let events = vec![
            Event::SessionCreated { meta: meta() },
            Event::RunStarted {
                client_run_id: "client-run".into(),
                input: "hello".into(),
            },
            Event::UserInputSubmitted {
                input: "steering".into(),
            },
            Event::UserInputApplied { user_input_seq: 2 },
            Event::RunCompleted {
                final_text: Some("done".into()),
            },
            Event::RunFailed {
                message: "failed".into(),
            },
            Event::RunCancelled { reason: None },
            Event::RunInterrupted { reason: None },
            Event::TextDelta {
                text: "text".into(),
            },
            Event::ReasoningDelta {
                text: "reasoning".into(),
            },
            Event::ToolCallStarted {
                tool_call_id: call_id(),
                tool: "read".into(),
                arguments: serde_json::json!({"path": "x"}),
                provider_tool_call_id: Some("call_native".into()),
                provider_protocol: Some(ProviderProtocol::OpenAiResponses),
            },
            Event::ToolCallProgress {
                tool_call_id: call_id(),
                message: "working".into(),
            },
            Event::ToolCallCompleted {
                tool_call_id: call_id(),
                result,
            },
            Event::ToolCallFailed {
                tool_call_id: call_id(),
                message: "failed".into(),
            },
            Event::ApprovalRequested {
                approval_id: "approval".into(),
                action: ActionKind::Bash,
                resource: "git status".into(),
                suggested_pattern: "git status *".into(),
                resources: vec![ApprovalResource {
                    action: ActionKind::Bash,
                    resource: "git status".into(),
                    suggested_pattern: "git status *".into(),
                }],
                decision_trace: trace(),
            },
            Event::ApprovalResolved {
                approval_id: "approval".into(),
                decision: ApprovalDecision::Always,
                approved_scope: Some("git status *".into()),
                approved_scopes: vec![ApprovedScope {
                    action: ActionKind::Bash,
                    resource: "git status".into(),
                    scope: "git status *".into(),
                }],
                feedback: Some("okay".into()),
            },
            Event::ToolStdinSubmitted {
                tool_call_id: call_id(),
                byte_count: 4,
            },
            Event::ToolCallLinked {
                tool_call_id: call_id(),
                child_session_id: SessionId(Uuid::from_u128(7)),
            },
            Event::AttemptAbandoned,
            Event::TurnOpaque {
                state: TurnOpaque {
                    provider: ProviderProtocol::OpenAiResponses,
                    payload: serde_json::json!({"items": []}),
                },
            },
            Event::ModelFallback {
                from: model.clone(),
                to: model.clone(),
                reason: "rate limited".into(),
                attempts: 2,
            },
            Event::UsageReported {
                model,
                usage: Usage {
                    input_tokens: 1,
                    output_tokens: 2,
                    cached_input_tokens: None,
                },
            },
        ];
        for event in events {
            round_trip(event);
        }
    }

    #[test]
    fn serde_round_trips_event_subscription_messages() {
        round_trip(EventSubscriptionMessage::Event { event: envelope() });
        round_trip(EventSubscriptionMessage::Gap {
            session_id: session_id(),
            last_delivered_seq: 3,
        });
    }

    #[test]
    fn output_delivery_types_round_trip_and_legacy_approval_resolution_decodes() {
        let delta = OutputDelta {
            call_id: call_id(),
            stream: OutputStream::Stdout,
            byte_offset: 4,
            data: "b3V0".into(),
        };
        round_trip(delta.clone());
        round_trip(OutputGap {
            call_id: call_id(),
            stream: OutputStream::Stderr,
            next_offset: 7,
        });
        round_trip(OutputSnapshot {
            call_id: call_id(),
            start_offset: 1,
            end_offset: 7,
            chunks: vec![delta],
        });
        round_trip(OutputSnapshotEnvelope {
            stream: OutputStream::Stdout,
            snapshot: OutputSnapshot {
                call_id: call_id(),
                start_offset: 0,
                end_offset: 0,
                chunks: Vec::new(),
            },
        });

        let legacy: Event = serde_json::from_value(serde_json::json!({
            "type": "approval_resolved",
            "approval_id": "legacy",
            "decision": "always",
            "approved_scope": "git status *"
        }))
        .expect("legacy approval resolution");
        assert!(matches!(
            legacy,
            Event::ApprovalResolved { approved_scopes, .. } if approved_scopes.is_empty()
        ));
    }

    #[test]
    fn depth_limit_child_arithmetic_matches_specification() {
        assert_eq!(
            DepthLimit::Finite(3).child_limit(Some(9)),
            DepthLimit::Finite(2)
        );
        assert_eq!(
            DepthLimit::Finite(3).child_limit(None),
            DepthLimit::Finite(2)
        );
        assert_eq!(
            DepthLimit::Unlimited.child_limit(Some(9)),
            DepthLimit::Finite(9)
        );
        assert_eq!(
            DepthLimit::Unlimited.child_limit(None),
            DepthLimit::Unlimited
        );
        assert!(!DepthLimit::Finite(0).allows_delegation());
    }

    #[test]
    fn event_envelope_schema_snapshot() {
        assert_json_snapshot!(schema_for!(EventEnvelope));
    }

    #[test]
    fn event_subscription_message_schema_snapshot() {
        assert_json_snapshot!(schema_for!(EventSubscriptionMessage));
    }

    #[test]
    fn typescript_export_smoke_test() {
        let declaration =
            EventEnvelope::export_to_string(&ts_rs::Config::default()).expect("export TypeScript");
        assert!(declaration.contains("EventEnvelope"));
    }
}
