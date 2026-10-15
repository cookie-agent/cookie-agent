use std::{borrow::Cow, collections::BTreeMap, fmt};

use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::*;

pub const SESSION_TREE_USAGE_MISSING_SESSION_CODE: i32 = -32030;
pub const SESSION_TREE_USAGE_CORRUPT_DELEGATION_CODE: i32 = -32031;
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
#[derive(Clone, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
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

/// The kind of client on a connection, such as `tui` or `cli`: a lowercase
/// ASCII letter followed by up to 15 lowercase letters, digits, or hyphens.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ClientKind(String);
impl ClientKind {
    pub const MAX_LEN: usize = 16;

    pub fn new(value: impl Into<String>) -> Result<Self, WireStringError> {
        let value = value.into();
        let valid = value.len() <= Self::MAX_LEN
            && value.bytes().enumerate().all(|(index, byte)| {
                byte.is_ascii_lowercase() || (index > 0 && (byte.is_ascii_digit() || byte == b'-'))
            })
            && !value.is_empty();
        if valid {
            Ok(Self(value))
        } else {
            Err(WireStringError::Invalid("client kind"))
        }
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl Serialize for ClientKind {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}
impl<'de> Deserialize<'de> for ClientKind {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}
impl JsonSchema for ClientKind {
    fn inline_schema() -> bool {
        true
    }
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("ClientKind")
    }
    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type":"string",
            "pattern":"^[a-z][a-z0-9-]{0,15}$",
            "description":"Client kind, such as `tui` or `cli`."
        })
    }
}

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

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ClientHello {
    pub protocol_version: ProtocolVersion,
    pub client: ClientIdentity,
}
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ServerHello {
    pub protocol_version: ProtocolVersion,
    /// The origin the daemon stamps on events this connection causes.
    pub origin: EventOrigin,
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
    /// omitted, the session attaches `local` (the daemon's working
    /// directory). Instances that are not running are started first; a
    /// failure creates no session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = 256))]
    pub runtimes: Option<Vec<RuntimeAttachRequest>>,
    /// Whether the session attaches the global runtime; `true` when omitted.
    #[serde(default = "attach_global", skip_serializing_if = "attaches_global")]
    pub global: bool,
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
        }
    }
}

/// One runtime a session attaches.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeAttachRequest {
    pub runtime_id: RuntimeId,
    /// Required exactly when the runtime's template takes a workspace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionCreateResult {
    pub session: SessionMeta,
    /// The session's runtime attachments, in attach order.
    pub runtimes: Vec<RuntimeAttachment>,
}

pub const SESSION_RUNTIME_ATTACH_METHOD: &str = "session.runtime.attach";
pub const SESSION_RUNTIME_DETACH_METHOD: &str = "session.runtime.detach";
pub const RUNTIMES_LIST_METHOD: &str = "runtimes.list";
pub const RUNTIMES_CHANGED_METHOD: &str = "runtimes.changed";

/// Attach a runtime to a session that has no active run. The instance is
/// started first when it is not running; a failure records nothing.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionRuntimeAttachParams {
    pub session_id: SessionId,
    pub runtime_id: RuntimeId,
    /// Required exactly when the runtime's template takes a workspace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
pub struct SessionRuntimeDetachResult {
    /// Every attachment of the session afterwards, in attach order.
    pub runtimes: Vec<RuntimeAttachment>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimesListParams {}

/// The runtime templates sessions can attach and the instances this daemon
/// has started.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimesListResult {
    pub templates: Vec<RuntimeTemplateInfo>,
    pub instances: Vec<RuntimeInstanceInfo>,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeTemplateInfo {
    pub runtime_id: RuntimeId,
    /// Whether attaching the template requires a workspace.
    pub takes_workspace: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeInstanceInfo {
    pub runtime_id: RuntimeId,
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

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeInstanceStatus {
    Starting,
    Ready,
    Reconnecting,
    Failed,
    Stopped,
}

/// A runtime template was registered or an instance's status changed;
/// refetch `runtimes.list`.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimesChangedNotification {}
#[derive(Clone, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionListParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd_identity: Option<CwdIdentity>,
}
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionListResult {
    pub sessions: Vec<SessionMeta>,
}
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionGetParams {
    pub session_id: SessionId,
}
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
pub struct SessionGoalLifecycleResult {
    pub goal: GoalState,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionProducersParams {
    pub session_id: SessionId,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionProducersResult {
    pub producers: Vec<ProducerRegistration>,
    pub plugin_recovery: Vec<PluginRecoveryState>,
}

#[derive(Clone, Debug, Default, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
pub struct SessionUsageResult {
    pub session_id: SessionId,
    pub usage: UsageRollup,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionTreeUsageResult {
    pub session_id: SessionId,
    pub usage: UsageRollup,
    pub session_count: u64,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
pub struct SessionChildrenResult {
    pub children: Vec<ChildSummary>,
}
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionTreeParams {
    pub session_id: SessionId,
}
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionTreeResult {
    pub tree: SessionTree,
}
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionResumeParams {
    pub session_id: SessionId,
}
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
pub struct SessionSetPermissionModeResult {}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionRuleSource {
    SessionOverlay,
    AgentDocument,
    Default,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EffectivePermissionRule {
    /// The rule's runtime selector, when it has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<RuntimeId>,
    pub resource: WildcardPattern,
    pub effect: PermissionEffect,
    pub source: PermissionRuleSource,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
pub struct SessionPermissionPreviewResult {
    pub permissions: Vec<EffectivePermissionAction>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillSource {
    User,
    Project,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
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
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SkillsListParams {
    pub session_id: SessionId,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
pub struct SessionCompactResult {
    pub compacted: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cancellation_reason: Option<String>,
}
pub const SESSION_COMPACT_CANCEL_METHOD: &str = "session.compact.cancel";
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionCompactCancelParams {
    pub session_id: SessionId,
}
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
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
/// `session.revert` was refused: the session's log moved past the
/// `expected_last_seq` the client sent.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionRevertConflict {
    pub session_id: SessionId,
    pub current_last_seq: u64,
}
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionRevertResult {
    pub session: SessionMeta,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions_override: Option<String>,
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
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
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
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionRenameErrorCode {
    SessionNotFound,
    InvalidTitle,
    IdempotencyConflict,
}
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionRenameError {
    pub code: SessionRenameErrorCode,
    pub session_id: SessionId,
    pub client_rename_id: ClientRenameId,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RunStartParams {
    pub session_id: SessionId,
    pub client_run_id: ClientRunId,
    pub selection: RunSelection,
    pub input: String,
    // An explicit model/agent/preset choice may restore a previously skipped prefix.
    #[serde(default, skip_serializing_if = "is_false")]
    pub reset_fallback: bool,
}

fn is_false(value: &bool) -> bool {
    !value
}
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStartConflictCode {
    IdempotencyConflict,
    /// Another run is active; `session.submit_input` steers or queues
    /// instead.
    SessionRunning,
}
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RunStartConflict {
    pub code: RunStartConflictCode,
    pub session_id: SessionId,
    pub client_run_id: ClientRunId,
}
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RunStartResult {
    pub run_id: RunId,
}
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RunSteerParams {
    pub run_id: RunId,
    pub input: String,
}
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
pub struct EventsUnsubscribeResult {
    /// Whether this connection had a live tail for the session.
    pub unsubscribed: bool,
}

pub const SESSIONS_CHANGED_METHOD: &str = "sessions.changed";
pub const MCP_CHANGED_METHOD: &str = "mcp.changed";

/// A daemon-wide change to the session index, sent to every connection
/// whether or not it subscribed to the session. Clients keep a session list,
/// approval prompts, and permission modes current from it.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "change", rename_all = "snake_case", deny_unknown_fields)]
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

/// MCP server configuration or status changed; refetch `mcp.server.list`.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct McpChangedNotification {}

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
    pub input: String,
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
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case", deny_unknown_fields)]
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
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionRecallInputParams {
    pub session_id: SessionId,
    /// The seq of the pending `user_input_admitted` or the queued
    /// `user_input_queued` event to take back.
    #[schemars(range(min = 1))]
    pub seq: u64,
}
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case", deny_unknown_fields)]
pub enum SessionRecallInputResult {
    /// The input was withdrawn; its text is returned for editing.
    Recalled { input: String },
    /// The input is no longer pending or queued: the model already saw it,
    /// a run started with it, or it was voided.
    Gone,
    /// Another client submitted the input; only its author can recall it.
    NotOwner,
}
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RunCancelParams {
    pub run_id: RunId,
}
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RunCancelResult {
    pub cancelled: bool,
}
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RunToolStdinParams {
    pub run_id: RunId,
    pub call_id: ToolCallId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<String>,
    pub eof: bool,
}
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RunToolStdinResult {
    pub accepted: bool,
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
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
pub struct ApprovalRespondResult {
    pub client_response_id: ClientResponseId,
    pub approval: ApprovalRecord,
}
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
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalRespondError {
    pub code: ApprovalRespondErrorCode,
    pub session_id: SessionId,
    pub approval_id: ApprovalId,
    pub client_response_id: ClientResponseId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected_revision: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub found_revision: Option<u64>,
}
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalListParams {
    pub root_session_id: SessionId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<ApprovalStatus>,
}
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalListResult {
    pub approvals: Vec<ApprovalRecord>,
    pub tree_grants: Vec<TreeApprovalGrant>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum McpConfigSource {
    UserFile,
    Runtime,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum McpConfigTarget {
    UserFile,
}

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

#[derive(Clone, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
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
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct McpServerInfo {
    pub name: String,
    pub source: McpConfigSource,
    pub definition: McpServerDefinition,
    pub state: McpServerState,
    pub tool_count: u32,
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
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
pub struct McpServerMutationResult {
    #[serde(deserialize_with = "deserialize_required_option")]
    #[schemars(with = "crate::NullableSchema<McpServerInfo>", required)]
    pub server: Option<McpServerInfo>,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct McpAuthBeginParams {
    pub server: String,
    /// The client's own loopback callback, `http://127.0.0.1:{port}/{path}`
    /// or `http://[::1]:{port}/{path}`: the browser redirects to the machine
    /// running the client, which hands the redirect to `mcp.auth.complete`.
    pub redirect_uri: String,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
pub struct McpAuthCompleteResult {
    pub server: String,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct McpAuthCancelParams {
    pub server: String,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct McpAuthCancelResult {
    pub server: String,
}
