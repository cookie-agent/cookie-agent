//! Versioned JSON-RPC wire types shared by cookie agent clients and the daemon.
//!
//! Data-carrying wire enums use an internally tagged, `snake_case`
//! representation with a `type` discriminator; unit enums are `snake_case`
//! strings. [`DepthLimit`] uses adjacent `kind`/`value` tags, while JSON-RPC
//! IDs and responses are untagged as required by JSON-RPC 2.0.

use std::{borrow::Cow, collections::BTreeMap, fmt, str::FromStr};

use jiff::Timestamp;
use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use ts_rs::TS;
use uuid::Uuid;

/// The only protocol version supported by this build.
pub const PROTOCOL_VERSION: u32 = 6;

/// The only durable event-record schema supported by this build.
pub const EVENT_SCHEMA_VERSION: u32 = 6;

/// Returns the TypeScript generation configuration required by this JSON wire.
/// JSON has one numeric type, so all Rust integer widths export as `number`.
#[must_use]
pub fn typescript_config() -> ts_rs::Config {
    ts_rs::Config::default().with_large_int("number")
}

/// Returns whether a peer protocol version is supported by this build.
#[must_use]
pub const fn supports_protocol_version(version: u32) -> bool {
    version == PROTOCOL_VERSION
}

/// Returns whether a durable event schema version is supported by this build.
#[must_use]
pub const fn supports_event_schema_version(version: u32) -> bool {
    version == EVENT_SCHEMA_VERSION
}

macro_rules! exact_numeric_wire_type {
    ($name:ident, $value:expr, $ts:literal, $description:literal) => {
        #[doc = $description]
        #[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq, TS)]
        #[ts(type = $ts)]
        pub struct $name(());

        impl $name {
            #[must_use]
            pub const fn current() -> Self {
                Self(())
            }

            #[must_use]
            pub const fn value(self) -> u32 {
                $value
            }
        }

        impl Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: serde::Serializer,
            {
                serializer.serialize_u32($value)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: serde::Deserializer<'de>,
            {
                let value = u32::deserialize(deserializer)?;
                if value == $value {
                    Ok(Self::current())
                } else {
                    Err(serde::de::Error::custom(format!(
                        "unsupported exact wire version {value}; expected {}",
                        $value
                    )))
                }
            }
        }

        impl JsonSchema for $name {
            fn inline_schema() -> bool {
                true
            }

            fn schema_name() -> Cow<'static, str> {
                Cow::Borrowed(stringify!($name))
            }

            fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
                json_schema!({"type": "integer", "const": $value})
            }
        }
    };
}

exact_numeric_wire_type!(
    ProtocolVersion,
    6,
    "6",
    "The exact cookie-agent protocol wire version."
);
exact_numeric_wire_type!(
    EventSchemaVersion,
    6,
    "6",
    "The exact durable event schema wire version."
);

/// The exact JSON-RPC wire version. It can only encode/decode `"2.0"`.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq, TS)]
#[ts(type = "\"2.0\"")]
pub struct JsonRpcVersion(());

impl JsonRpcVersion {
    #[must_use]
    pub const fn current() -> Self {
        Self(())
    }
}

impl Serialize for JsonRpcVersion {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str("2.0")
    }
}

impl<'de> Deserialize<'de> for JsonRpcVersion {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        if value == "2.0" {
            Ok(Self::current())
        } else {
            Err(serde::de::Error::custom(format!(
                "unsupported JSON-RPC version {value:?}; expected \"2.0\""
            )))
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

    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        json_schema!({"type": "string", "const": "2.0"})
    }
}

/// Preserves Serde's transparent UUID representation without presenting its
/// unsupported container attribute to `ts-rs`.
#[derive(Deserialize, Serialize)]
#[serde(transparent)]
struct TransparentUuid(Uuid);

macro_rules! uuid_id {
    ($name:ident) => {
        #[derive(Clone, Copy, Debug, Eq, Hash, JsonSchema, PartialEq, TS)]
        #[schemars(transparent)]
        pub struct $name(pub Uuid);

        impl Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: serde::Serializer,
            {
                TransparentUuid(self.0).serialize(serializer)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: serde::Deserializer<'de>,
            {
                TransparentUuid::deserialize(deserializer).map(|uuid| Self(uuid.0))
            }
        }

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
uuid_id!(InternalAgentInvocationId);
uuid_id!(InternalAgentRunId);
uuid_id!(ApprovalId);
uuid_id!(TreeApprovalGrantId);

/// A JSON-RPC request identifier. JSON-RPC allows strings, numbers, or null.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(untagged)]
pub enum JsonRpcId {
    Null,
    Number(i64),
    String(String),
}

/// A JSON-RPC 2.0 request envelope. `Debug` always redacts raw parameters.
#[derive(Clone, Deserialize, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
#[ts(optional_fields = nullable)]
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
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut debug = formatter.debug_struct("Request");
        debug
            .field("jsonrpc", &self.jsonrpc)
            .field("id", &self.id)
            .field("method", &self.method);
        if self.params.is_some() {
            debug.field("params", &"<redacted>");
        } else {
            debug.field("params", &Option::<()>::None);
        }
        debug.finish()
    }
}

/// A successful JSON-RPC 2.0 response envelope. `Debug` redacts the raw result.
#[derive(Clone, Deserialize, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct SuccessResponse {
    pub jsonrpc: JsonRpcVersion,
    pub id: JsonRpcId,
    pub result: Value,
}

impl fmt::Debug for SuccessResponse {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SuccessResponse")
            .field("jsonrpc", &self.jsonrpc)
            .field("id", &self.id)
            .field("result", &"<redacted>")
            .finish()
    }
}

/// A failed JSON-RPC 2.0 response envelope.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct ErrorResponse {
    pub jsonrpc: JsonRpcVersion,
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

/// JSON-RPC error details. `Debug` always redacts raw error data.
#[derive(Clone, Deserialize, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
#[ts(optional_fields = nullable)]
pub struct JsonRpcError {
    pub code: i32,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl fmt::Debug for JsonRpcError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut debug = formatter.debug_struct("JsonRpcError");
        debug
            .field("code", &self.code)
            .field("message", &self.message);
        if self.data.is_some() {
            debug.field("data", &"<redacted>");
        } else {
            debug.field("data", &Option::<()>::None);
        }
        debug.finish()
    }
}

/// A JSON-RPC 2.0 notification envelope. `Debug` always redacts raw parameters.
#[derive(Clone, Deserialize, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
#[ts(optional_fields = nullable)]
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

impl fmt::Debug for Notification {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut debug = formatter.debug_struct("Notification");
        debug
            .field("jsonrpc", &self.jsonrpc)
            .field("method", &self.method);
        if self.params.is_some() {
            debug.field("params", &"<redacted>");
        } else {
            debug.field("params", &Option::<()>::None);
        }
        debug.finish()
    }
}

/// Initial handshake payload sent by a client before issuing protocol calls.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct ClientHello {
    pub protocol_version: ProtocolVersion,
}

/// Handshake payload returned by a server that accepted a client version.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct ServerHello {
    pub protocol_version: ProtocolVersion,
}

/// A model entry in a profile fallback chain.
#[derive(Clone, Debug, Deserialize, Eq, Hash, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct ModelRef {
    /// Cookie-agent configuration alias.
    pub name: String,
    /// Serving provider identity.
    pub provider_id: String,
    /// Exact provider model, deployment, or resource identity.
    pub model_id: String,
    /// Oven adapter identity used for native replay compatibility.
    pub adapter_id: String,
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
#[serde(deny_unknown_fields)]
pub struct DelegationSnapshot {
    pub enabled: bool,
    pub allowed_profiles: Vec<String>,
    pub depth_limit: DepthLimit,
    pub result_limit_bytes: u64,
}

/// A resolved profile frozen when its session is created.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
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
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
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

/// A validated session title retained exactly as authored.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, TS)]
#[ts(type = "string")]
pub struct SessionTitle(String);

impl SessionTitle {
    /// Absolute UTF-8 wire/persistence bound for one title.
    pub const MAX_BYTES: usize = 512;

    pub fn new(value: impl Into<String>) -> Result<Self, SessionTitleError> {
        let value = value.into();
        if value.trim().is_empty() {
            return Err(SessionTitleError::Empty);
        }
        if value.len() > Self::MAX_BYTES {
            return Err(SessionTitleError::TooLong {
                found: value.len(),
                maximum: Self::MAX_BYTES,
            });
        }
        if value.chars().any(char::is_control) {
            return Err(SessionTitleError::ControlCharacter);
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SessionTitle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl Serialize for SessionTitle {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for SessionTitle {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

impl JsonSchema for SessionTitle {
    fn inline_schema() -> bool {
        true
    }

    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("SessionTitle")
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "string",
            "minLength": 1,
            "maxLength": 512,
            "description": "Non-blank UTF-8 title without control characters."
        })
    }
}

/// Validation error for a session title.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SessionTitleError {
    Empty,
    TooLong { found: usize, maximum: usize },
    ControlCharacter,
}

impl fmt::Display for SessionTitleError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => formatter.write_str("session title must not be blank"),
            Self::TooLong { found, maximum } => write!(
                formatter,
                "session title is {found} UTF-8 bytes, exceeding the {maximum}-byte limit"
            ),
            Self::ControlCharacter => {
                formatter.write_str("session title must not contain control characters")
            }
        }
    }
}

impl std::error::Error for SessionTitleError {}

/// Session metadata cached for querying; the event log remains authoritative.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct SessionMeta {
    pub id: SessionId,
    pub origin: SessionOrigin,
    pub cwd: String,
    pub profile: ProfileSnapshot,
    #[serde(deserialize_with = "deserialize_nullable_session_title")]
    #[schemars(required)]
    pub title: Option<SessionTitle>,
}

fn deserialize_nullable_session_title<'de, D>(
    deserializer: D,
) -> Result<Option<SessionTitle>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::<SessionTitle>::deserialize(deserializer)
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

/// Complete normalized Oven token accounting.
///
/// Inclusive totals are persisted exactly as reported and are never derived by
/// adding component fields. Provider-specific raw usage is intentionally not a
/// client protocol field.
#[derive(Clone, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct Usage {
    pub input_tokens: Option<u64>,
    pub input_tokens_no_cache: Option<u64>,
    pub input_tokens_cache_read: Option<u64>,
    pub input_tokens_cache_write: Option<u64>,
    pub output_tokens: Option<u64>,
    pub output_tokens_text: Option<u64>,
    pub output_tokens_reasoning: Option<u64>,
}

/// A lightweight delegated child projection.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
#[ts(optional_fields = nullable)]
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
#[serde(deny_unknown_fields)]
pub struct SessionTree {
    pub session: SessionMeta,
    pub children: Vec<SessionTree>,
}

/// The number of further delegation generations allowed beneath a session.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
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
#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize, TS)]
pub struct PermissionRule {
    pub id: String,
    pub action: ActionKind,
    pub resource: String,
    pub effect: Effect,
}

impl<'de> Deserialize<'de> for PermissionRule {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct WirePermissionRule {
            id: String,
            action: ActionKind,
            resource: String,
            effect: Effect,
        }

        let rule = WirePermissionRule::deserialize(deserializer)?;
        Ok(Self {
            id: rule.id,
            action: rule.action,
            resource: rule.resource,
            effect: rule.effect,
        })
    }
}

/// A candidate rule retained for client-visible permission explanations.
#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[ts(optional_fields = nullable)]
pub struct MatchedPermissionRule {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rule_id: Option<String>,
    pub source_layer: String,
    pub effect: Effect,
}

impl<'de> Deserialize<'de> for MatchedPermissionRule {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct WireMatchedPermissionRule {
            rule_id: Option<String>,
            source_layer: String,
            effect: Effect,
        }

        let rule = WireMatchedPermissionRule::deserialize(deserializer)?;
        Ok(Self {
            rule_id: rule.rule_id,
            source_layer: rule.source_layer,
            effect: rule.effect,
        })
    }
}

/// Complete derivation of a permission decision.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct DecisionTrace {
    pub action: ActionKind,
    pub normalized_resource: String,
    pub candidates: Vec<MatchedPermissionRule>,
    pub effect: Effect,
    pub precedence_reason: String,
}

/// A canonical SHA-256 digest encoded as exactly 64 lowercase hexadecimal bytes.
#[derive(Clone, Eq, Hash, PartialEq, TS)]
#[ts(type = "string")]
pub struct Sha256Digest(String);

impl Sha256Digest {
    pub const HEX_LENGTH: usize = 64;

    /// Parses a strict lowercase hexadecimal SHA-256 digest.
    pub fn new(value: impl Into<String>) -> Result<Self, Sha256DigestError> {
        let value = value.into();
        if value.len() != Self::HEX_LENGTH {
            return Err(Sha256DigestError::WrongLength { found: value.len() });
        }
        if !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(Sha256DigestError::Malformed);
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Computes the ordinary content-integrity SHA-256 digest of exact bytes.
    #[must_use]
    pub fn of_bytes(bytes: &[u8]) -> Self {
        Self(sha256_hex(bytes))
    }

    #[must_use]
    fn hash_domain_separated(domain: &[u8], canonical_bytes: &[u8]) -> Self {
        let mut input = Vec::with_capacity(domain.len() + 8 + canonical_bytes.len());
        input.extend_from_slice(domain);
        input.extend_from_slice(&(canonical_bytes.len() as u64).to_be_bytes());
        input.extend_from_slice(canonical_bytes);
        Self(sha256_hex(&input))
    }
}

fn sha256_hex(input: &[u8]) -> String {
    const INITIAL: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    const ROUND: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];

    let bit_length = (input.len() as u64).wrapping_mul(8);
    let mut message = Vec::with_capacity(input.len() + 72);
    message.extend_from_slice(input);
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&bit_length.to_be_bytes());

    let mut state = INITIAL;
    for chunk in message.chunks_exact(64) {
        let mut words = [0_u32; 64];
        for (index, bytes) in chunk.chunks_exact(4).enumerate() {
            words[index] = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        }
        for index in 16..64 {
            let sigma0 = words[index - 15].rotate_right(7)
                ^ words[index - 15].rotate_right(18)
                ^ (words[index - 15] >> 3);
            let sigma1 = words[index - 2].rotate_right(17)
                ^ words[index - 2].rotate_right(19)
                ^ (words[index - 2] >> 10);
            words[index] = words[index - 16]
                .wrapping_add(sigma0)
                .wrapping_add(words[index - 7])
                .wrapping_add(sigma1);
        }

        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = state;
        for index in 0..64 {
            let sum1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let choose = (e & f) ^ ((!e) & g);
            let temp1 = h
                .wrapping_add(sum1)
                .wrapping_add(choose)
                .wrapping_add(ROUND[index])
                .wrapping_add(words[index]);
            let sum0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let majority = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = sum0.wrapping_add(majority);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }
        state[0] = state[0].wrapping_add(a);
        state[1] = state[1].wrapping_add(b);
        state[2] = state[2].wrapping_add(c);
        state[3] = state[3].wrapping_add(d);
        state[4] = state[4].wrapping_add(e);
        state[5] = state[5].wrapping_add(f);
        state[6] = state[6].wrapping_add(g);
        state[7] = state[7].wrapping_add(h);
    }

    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(64);
    for byte in state.into_iter().flat_map(u32::to_be_bytes) {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}

impl fmt::Debug for Sha256Digest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl fmt::Display for Sha256Digest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl Serialize for Sha256Digest {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for Sha256Digest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

impl JsonSchema for Sha256Digest {
    fn inline_schema() -> bool {
        true
    }

    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("Sha256Digest")
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "string",
            "pattern": "^[0-9a-f]{64}$",
            "minLength": 64,
            "maxLength": 64
        })
    }
}

/// Validation error for a canonical SHA-256 digest.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Sha256DigestError {
    WrongLength { found: usize },
    Malformed,
}

impl fmt::Display for Sha256DigestError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WrongLength { found } => write!(
                formatter,
                "SHA-256 digest must be exactly 64 lowercase hexadecimal characters; found {found}"
            ),
            Self::Malformed => formatter
                .write_str("SHA-256 digest must contain only lowercase hexadecimal characters"),
        }
    }
}

impl std::error::Error for Sha256DigestError {}

/// The exact canonical operation fingerprint used for idempotency and consent.
#[derive(Clone, Debug, Deserialize, Eq, Hash, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct OperationFingerprint {
    digest: Sha256Digest,
}

impl OperationFingerprint {
    const DOMAIN: &'static [u8] = b"cookie-agent.operation-fingerprint.v6\0";

    /// Builds the fingerprint from the complete typed prepared-operation identity.
    #[must_use]
    pub fn from_prepared_operation(operation: &PreparedOperationIdentity) -> Self {
        Self {
            digest: Sha256Digest::hash_domain_separated(
                Self::DOMAIN,
                &operation.canonical_fingerprint_bytes(),
            ),
        }
    }

    #[must_use]
    pub const fn digest(&self) -> &Sha256Digest {
        &self.digest
    }
}

/// Domain-separated SHA-256 digest binding an immutable prepared resource to
/// the exact process-local capability held by the engine.
#[derive(Clone, Debug, Deserialize, Eq, Hash, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct PreparedResourceDigest {
    digest: Sha256Digest,
}

impl PreparedResourceDigest {
    const DOMAIN: &'static [u8] = b"cookie-agent.prepared-resource-digest.v6\0";

    /// Hashes canonical descriptor/resource binding bytes. Raw paths, file
    /// descriptor numbers, and temporary names must not be included.
    #[must_use]
    pub fn from_canonical_binding_bytes(canonical_binding_bytes: &[u8]) -> Self {
        Self {
            digest: Sha256Digest::hash_domain_separated(Self::DOMAIN, canonical_binding_bytes),
        }
    }

    #[must_use]
    pub const fn digest(&self) -> &Sha256Digest {
        &self.digest
    }
}

/// Stable logical identity for a prepared resource. The restricted grammar
/// deliberately cannot encode raw paths, descriptor numbers, or temp names.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, TS)]
#[ts(type = "string")]
pub struct PreparedResourceIdentity(String);

impl PreparedResourceIdentity {
    pub const MAX_BYTES: usize = 512;

    pub fn new(value: impl Into<String>) -> Result<Self, PreparedResourceIdentityError> {
        let value = value.into();
        if value.is_empty() {
            return Err(PreparedResourceIdentityError::Empty);
        }
        if value.len() > Self::MAX_BYTES {
            return Err(PreparedResourceIdentityError::TooLong {
                found: value.len(),
                maximum: Self::MAX_BYTES,
            });
        }
        if !value.bytes().all(|byte| {
            byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || matches!(byte, b'-' | b'_' | b'.' | b':')
        }) {
            return Err(PreparedResourceIdentityError::InvalidCharacter);
        }
        if value.starts_with("fd:") || value.starts_with("tmp:") || value.starts_with("temp:") {
            return Err(PreparedResourceIdentityError::UnstableIdentifier);
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for PreparedResourceIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl Serialize for PreparedResourceIdentity {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for PreparedResourceIdentity {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

impl JsonSchema for PreparedResourceIdentity {
    fn inline_schema() -> bool {
        true
    }

    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("PreparedResourceIdentity")
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "string",
            "pattern": "^[a-z0-9][a-z0-9._:-]*$",
            "minLength": 1,
            "maxLength": 512,
            "description": "Stable logical prepared-resource identity; never a raw path, descriptor, or temp name."
        })
    }
}

/// Invalid prepared-resource logical identity.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PreparedResourceIdentityError {
    Empty,
    TooLong { found: usize, maximum: usize },
    InvalidCharacter,
    UnstableIdentifier,
}

impl fmt::Display for PreparedResourceIdentityError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => formatter.write_str("prepared resource identity must not be empty"),
            Self::TooLong { found, maximum } => write!(
                formatter,
                "prepared resource identity is {found} bytes, exceeding the {maximum}-byte limit"
            ),
            Self::InvalidCharacter => formatter.write_str(
                "prepared resource identity must use lowercase ASCII letters, digits, '.', '_', '-', or ':'",
            ),
            Self::UnstableIdentifier => formatter.write_str(
                "prepared resource identity must not encode a descriptor or temporary identifier",
            ),
        }
    }
}

impl std::error::Error for PreparedResourceIdentityError {}

/// Stable semantic operation name for a prepared capability. It uses the same
/// path/descriptor-free grammar as prepared resource identities.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, TS)]
#[ts(type = "string")]
pub struct PreparedCapabilityOperation(PreparedResourceIdentity);

impl PreparedCapabilityOperation {
    pub fn new(value: impl Into<String>) -> Result<Self, PreparedResourceIdentityError> {
        PreparedResourceIdentity::new(value).map(Self)
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl fmt::Display for PreparedCapabilityOperation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl Serialize for PreparedCapabilityOperation {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for PreparedCapabilityOperation {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

impl JsonSchema for PreparedCapabilityOperation {
    fn inline_schema() -> bool {
        true
    }

    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("PreparedCapabilityOperation")
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "string",
            "pattern": "^[a-z0-9][a-z0-9._:-]*$",
            "minLength": 1,
            "maxLength": 512,
            "description": "Stable semantic prepared-capability operation; never a raw path, descriptor, or temp name."
        })
    }
}

/// Matching boundary for one canonical resource. Clients cannot edit it.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ApprovalBoundary {
    Exact,
    CommandPrefix { prefix: String },
    DelegationTree { root_session_id: SessionId },
}

/// Why a resource is part of an approval request.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalResourceSource {
    PrimaryOperation,
    SecondaryOperation,
    ExternalDirectoryGuard,
    ModelRequest,
    DoomLoopGuard,
}

/// Restart stability of a prepared resource binding. Filesystem descriptor
/// bindings are process-local and cannot back durable tree grants.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum PreparedBindingLifetime {
    ProcessLocal,
    RestartStable,
}

/// One immutable prepared canonical resource disclosed by an approval request.
/// `source` is provenance only and never contributes to operation identity.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct PreparedApprovalResource {
    pub capability: ActionKind,
    pub canonical: PreparedResourceIdentity,
    pub binding_digest: PreparedResourceDigest,
    pub binding_lifetime: PreparedBindingLifetime,
    pub boundary: ApprovalBoundary,
    pub source: ApprovalResourceSource,
}

/// One permission evaluation retained for an approval resource.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct ApprovalEvaluation {
    pub resource_digest: PreparedResourceDigest,
    pub effect: Effect,
    pub trace: DecisionTrace,
}

/// What caused approval evaluation to begin.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalTrigger {
    PermissionPolicy,
    ModelToolApproval,
    InternalAgent,
    DoomLoop,
}

/// One capability required by the exact operation.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct ApprovalCapability {
    pub action: ActionKind,
    pub operation: PreparedCapabilityOperation,
}

/// Prepared execution capabilities exist only in the preparing daemon process.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum PreparedCapabilityLifetime {
    ProcessLocal,
}

/// Complete canonical identity of one prepared tool operation. Arguments are
/// represented by the digest of their normalized, resource-reference form so
/// raw paths and process-local descriptor/temp identifiers never enter the
/// durable identity.
#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[schemars(deny_unknown_fields)]
pub struct PreparedOperationIdentity {
    normalized_arguments_digest: Sha256Digest,
    capabilities: Vec<ApprovalCapability>,
    resources: Vec<PreparedApprovalResource>,
    execution_context_digest: Sha256Digest,
    capability_lifetime: PreparedCapabilityLifetime,
}

impl PreparedOperationIdentity {
    pub fn new(
        normalized_arguments_digest: Sha256Digest,
        capabilities: Vec<ApprovalCapability>,
        resources: Vec<PreparedApprovalResource>,
        execution_context_digest: Sha256Digest,
    ) -> Result<Self, PreparedOperationIdentityError> {
        if capabilities.is_empty() {
            return Err(PreparedOperationIdentityError::MissingCapability);
        }
        if resources.iter().any(|resource| {
            !capabilities
                .iter()
                .any(|capability| capability.action == resource.capability)
        }) {
            return Err(PreparedOperationIdentityError::UnboundResourceCapability);
        }
        let mut binding_digests = resources
            .iter()
            .map(|resource| resource.binding_digest.digest().as_str())
            .collect::<Vec<_>>();
        binding_digests.sort_unstable();
        if binding_digests.windows(2).any(|pair| pair[0] == pair[1]) {
            return Err(PreparedOperationIdentityError::DuplicateResourceBinding);
        }

        Ok(Self {
            normalized_arguments_digest,
            capabilities,
            resources,
            execution_context_digest,
            capability_lifetime: PreparedCapabilityLifetime::ProcessLocal,
        })
    }

    #[must_use]
    pub const fn normalized_arguments_digest(&self) -> &Sha256Digest {
        &self.normalized_arguments_digest
    }

    #[must_use]
    pub fn capabilities(&self) -> &[ApprovalCapability] {
        &self.capabilities
    }

    #[must_use]
    pub fn resources(&self) -> &[PreparedApprovalResource] {
        &self.resources
    }

    #[must_use]
    pub const fn execution_context_digest(&self) -> &Sha256Digest {
        &self.execution_context_digest
    }

    #[must_use]
    pub const fn capability_lifetime(&self) -> PreparedCapabilityLifetime {
        self.capability_lifetime
    }

    fn canonical_fingerprint_bytes(&self) -> Vec<u8> {
        let mut canonical = Vec::new();
        append_length_prefixed(
            &mut canonical,
            self.normalized_arguments_digest.as_str().as_bytes(),
        );

        let mut capabilities = self
            .capabilities
            .iter()
            .map(|capability| {
                serde_json::to_vec(capability)
                    .expect("serializing a protocol capability cannot fail")
            })
            .collect::<Vec<_>>();
        capabilities.sort_unstable();
        canonical.extend_from_slice(&(capabilities.len() as u64).to_be_bytes());
        for capability in capabilities {
            append_length_prefixed(&mut canonical, &capability);
        }

        let mut resources = self
            .resources
            .iter()
            .map(|resource| {
                let mut identity = Vec::new();
                append_length_prefixed(
                    &mut identity,
                    &serde_json::to_vec(&resource.capability)
                        .expect("serializing a protocol action cannot fail"),
                );
                append_length_prefixed(&mut identity, resource.canonical.as_str().as_bytes());
                append_length_prefixed(
                    &mut identity,
                    resource.binding_digest.digest().as_str().as_bytes(),
                );
                append_length_prefixed(
                    &mut identity,
                    &serde_json::to_vec(&resource.binding_lifetime)
                        .expect("serializing a binding lifetime cannot fail"),
                );
                identity
            })
            .collect::<Vec<_>>();
        resources.sort_unstable();
        canonical.extend_from_slice(&(resources.len() as u64).to_be_bytes());
        for resource in resources {
            append_length_prefixed(&mut canonical, &resource);
        }

        append_length_prefixed(
            &mut canonical,
            self.execution_context_digest.as_str().as_bytes(),
        );
        append_length_prefixed(&mut canonical, b"process_local");
        canonical
    }
}

impl<'de> Deserialize<'de> for PreparedOperationIdentity {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct WireIdentity {
            normalized_arguments_digest: Sha256Digest,
            capabilities: Vec<ApprovalCapability>,
            resources: Vec<PreparedApprovalResource>,
            execution_context_digest: Sha256Digest,
            capability_lifetime: PreparedCapabilityLifetime,
        }

        let wire = WireIdentity::deserialize(deserializer)?;
        if wire.capability_lifetime != PreparedCapabilityLifetime::ProcessLocal {
            return Err(serde::de::Error::custom(
                "prepared capability lifetime must be process_local",
            ));
        }
        Self::new(
            wire.normalized_arguments_digest,
            wire.capabilities,
            wire.resources,
            wire.execution_context_digest,
        )
        .map_err(serde::de::Error::custom)
    }
}

fn append_length_prefixed(output: &mut Vec<u8>, value: &[u8]) {
    output.extend_from_slice(&(value.len() as u64).to_be_bytes());
    output.extend_from_slice(value);
}

/// Invalid or incomplete prepared-operation identity.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PreparedOperationIdentityError {
    MissingCapability,
    UnboundResourceCapability,
    DuplicateResourceBinding,
}

impl fmt::Display for PreparedOperationIdentityError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingCapability => {
                formatter.write_str("prepared operation requires at least one capability")
            }
            Self::UnboundResourceCapability => formatter.write_str(
                "every prepared resource capability must be present in the operation capabilities",
            ),
            Self::DuplicateResourceBinding => formatter
                .write_str("prepared resource binding digests must be unique per operation"),
        }
    }
}

impl std::error::Error for PreparedOperationIdentityError {}

/// Server-authored response constraints. There is intentionally no scope editor.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
#[ts(optional_fields = nullable)]
pub struct ApprovalConstraints {
    pub allow_once: bool,
    pub allow_tree_grant: bool,
    pub cancellable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<Timestamp>,
}

/// Stable machine-readable reasons for approval decisions and failures.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalReasonCode {
    PolicyAllowed,
    PolicyDenied,
    PolicyRequiresApproval,
    ModelRequested,
    InternalAgentAllowed,
    InternalAgentDenied,
    TreeGrantMatched,
    UserApprovedOnce,
    UserApprovedTree,
    UserRejected,
    UserCancelled,
    RequestCancelled,
    DoomLoopDetected,
    ConstraintViolation,
    OperationChanged,
    PreparedCapabilityLost,
    UnsupportedPlatform,
    ApprovalExpired,
    Escalated,
    Unattended,
    SystemError,
}

/// Origin of an approval decision.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalDecisionSource {
    Policy,
    Model,
    InternalAgent,
    TreeGrant,
    User,
    DoomLoopGuard,
    System,
}

/// Current lifecycle status of an approval request.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalStatus {
    Pending,
    Escalated,
    Approved,
    Rejected,
    Cancelled,
    Expired,
}

/// Optional human feedback associated with a user decision.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct ApprovalFeedback {
    pub message: String,
}

/// Engine/internal-agent decision before user interaction.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalInternalDecisionKind {
    Allow,
    Deny,
    Ask,
    Escalate,
}

/// Complete internal decision with stable explanation data.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct ApprovalInternalDecision {
    pub decision: ApprovalInternalDecisionKind,
    pub source: ApprovalDecisionSource,
    pub reason_code: ApprovalReasonCode,
    pub evaluations: Vec<ApprovalEvaluation>,
}

/// A user's constrained response. Grant boundaries remain server-authored.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalUserDecision {
    ApproveOnce,
    ApproveTree,
    Reject,
    Cancel,
}

/// Terminal approval outcome.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalFinalOutcome {
    Approved,
    Rejected,
    Cancelled,
    Expired,
}

/// Stable failure code for a tool operation that did not execute successfully.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum ToolCallFailureCode {
    OperationChanged,
    PreparedCapabilityLost,
    UnsupportedPlatform,
    ExecutionFailed,
}

/// Final decision persisted after policy, internal-agent, grant, or user input.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
#[ts(optional_fields = nullable)]
pub struct ApprovalFinalDecision {
    pub outcome: ApprovalFinalOutcome,
    pub source: ApprovalDecisionSource,
    pub reason_code: ApprovalReasonCode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub feedback: Option<ApprovalFeedback>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tree_grant_id: Option<TreeApprovalGrantId>,
}

/// Immutable request payload presented to clients.
#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[schemars(deny_unknown_fields)]
pub struct ApprovalRequest {
    approval_id: ApprovalId,
    revision: u64,
    trigger: ApprovalTrigger,
    operation_fingerprint: OperationFingerprint,
    operation: PreparedOperationIdentity,
    evaluations: Vec<ApprovalEvaluation>,
    constraints: ApprovalConstraints,
}

impl ApprovalRequest {
    pub fn new(
        approval_id: ApprovalId,
        revision: u64,
        trigger: ApprovalTrigger,
        operation: PreparedOperationIdentity,
        evaluations: Vec<ApprovalEvaluation>,
        constraints: ApprovalConstraints,
    ) -> Result<Self, ApprovalRequestError> {
        let mut resource_digests = operation
            .resources()
            .iter()
            .map(|resource| resource.binding_digest.digest().as_str())
            .collect::<Vec<_>>();
        resource_digests.sort_unstable();
        let mut evaluation_digests = evaluations
            .iter()
            .map(|evaluation| evaluation.resource_digest.digest().as_str())
            .collect::<Vec<_>>();
        evaluation_digests.sort_unstable();
        if resource_digests != evaluation_digests {
            return Err(ApprovalRequestError::IncompleteEvaluations);
        }
        if constraints.allow_tree_grant
            && operation.resources().iter().any(|resource| {
                resource.binding_lifetime == PreparedBindingLifetime::ProcessLocal
                    || matches!(
                        resource.capability,
                        ActionKind::Read
                            | ActionKind::Write
                            | ActionKind::Grep
                            | ActionKind::Glob
                            | ActionKind::ExternalDirectory
                    )
            })
        {
            return Err(ApprovalRequestError::InvalidTreeGrantConstraint);
        }
        let operation_fingerprint = OperationFingerprint::from_prepared_operation(&operation);
        Ok(Self {
            approval_id,
            revision,
            trigger,
            operation_fingerprint,
            operation,
            evaluations,
            constraints,
        })
    }

    #[must_use]
    pub const fn approval_id(&self) -> ApprovalId {
        self.approval_id
    }

    #[must_use]
    pub const fn operation_fingerprint(&self) -> &OperationFingerprint {
        &self.operation_fingerprint
    }

    #[must_use]
    pub const fn operation(&self) -> &PreparedOperationIdentity {
        &self.operation
    }

    #[must_use]
    pub fn evaluations(&self) -> &[ApprovalEvaluation] {
        &self.evaluations
    }

    #[must_use]
    pub const fn constraints(&self) -> &ApprovalConstraints {
        &self.constraints
    }
}

impl<'de> Deserialize<'de> for ApprovalRequest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct WireRequest {
            approval_id: ApprovalId,
            revision: u64,
            trigger: ApprovalTrigger,
            operation_fingerprint: OperationFingerprint,
            operation: PreparedOperationIdentity,
            evaluations: Vec<ApprovalEvaluation>,
            constraints: ApprovalConstraints,
        }

        let wire = WireRequest::deserialize(deserializer)?;
        let request = Self::new(
            wire.approval_id,
            wire.revision,
            wire.trigger,
            wire.operation,
            wire.evaluations,
            wire.constraints,
        )
        .map_err(serde::de::Error::custom)?;
        if request.operation_fingerprint != wire.operation_fingerprint {
            return Err(serde::de::Error::custom(
                "approval operation fingerprint does not match prepared operation identity",
            ));
        }
        Ok(request)
    }
}

/// Invalid immutable approval request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ApprovalRequestError {
    IncompleteEvaluations,
    InvalidTreeGrantConstraint,
}

impl fmt::Display for ApprovalRequestError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::IncompleteEvaluations => formatter.write_str(
                "approval evaluations must exactly cover every prepared resource binding",
            ),
            Self::InvalidTreeGrantConstraint => formatter.write_str(
                "process-local or filesystem prepared resources cannot allow durable tree grants",
            ),
        }
    }
}

impl std::error::Error for ApprovalRequestError {}

/// A durable exact grant shared by one delegation tree.
#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[schemars(deny_unknown_fields)]
pub struct TreeApprovalGrant {
    grant_id: TreeApprovalGrantId,
    root_session_id: SessionId,
    approval_id: ApprovalId,
    operation_fingerprint: OperationFingerprint,
    capabilities: Vec<ApprovalCapability>,
    resources: Vec<PreparedApprovalResource>,
    created_at: Timestamp,
}

impl TreeApprovalGrant {
    pub fn new(
        grant_id: TreeApprovalGrantId,
        root_session_id: SessionId,
        approval_id: ApprovalId,
        operation_fingerprint: OperationFingerprint,
        capabilities: Vec<ApprovalCapability>,
        resources: Vec<PreparedApprovalResource>,
        created_at: Timestamp,
    ) -> Result<Self, TreeApprovalGrantError> {
        if capabilities.is_empty() {
            return Err(TreeApprovalGrantError::MissingCapability);
        }
        if resources
            .iter()
            .any(|resource| resource.binding_lifetime != PreparedBindingLifetime::RestartStable)
        {
            return Err(TreeApprovalGrantError::ProcessLocalResource);
        }
        if resources.iter().any(|resource| {
            matches!(
                resource.capability,
                ActionKind::Read
                    | ActionKind::Write
                    | ActionKind::Grep
                    | ActionKind::Glob
                    | ActionKind::ExternalDirectory
            )
        }) {
            return Err(TreeApprovalGrantError::FilesystemResource);
        }
        if resources.iter().any(|resource| {
            !capabilities
                .iter()
                .any(|capability| capability.action == resource.capability)
        }) {
            return Err(TreeApprovalGrantError::UnboundResourceCapability);
        }
        Ok(Self {
            grant_id,
            root_session_id,
            approval_id,
            operation_fingerprint,
            capabilities,
            resources,
            created_at,
        })
    }

    #[must_use]
    pub const fn grant_id(&self) -> TreeApprovalGrantId {
        self.grant_id
    }

    #[must_use]
    pub const fn root_session_id(&self) -> SessionId {
        self.root_session_id
    }

    #[must_use]
    pub const fn approval_id(&self) -> ApprovalId {
        self.approval_id
    }

    #[must_use]
    pub const fn operation_fingerprint(&self) -> &OperationFingerprint {
        &self.operation_fingerprint
    }

    #[must_use]
    pub fn capabilities(&self) -> &[ApprovalCapability] {
        &self.capabilities
    }

    #[must_use]
    pub fn resources(&self) -> &[PreparedApprovalResource] {
        &self.resources
    }

    #[must_use]
    pub const fn created_at(&self) -> Timestamp {
        self.created_at
    }
}

impl<'de> Deserialize<'de> for TreeApprovalGrant {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct WireGrant {
            grant_id: TreeApprovalGrantId,
            root_session_id: SessionId,
            approval_id: ApprovalId,
            operation_fingerprint: OperationFingerprint,
            capabilities: Vec<ApprovalCapability>,
            resources: Vec<PreparedApprovalResource>,
            created_at: Timestamp,
        }

        let wire = WireGrant::deserialize(deserializer)?;
        Self::new(
            wire.grant_id,
            wire.root_session_id,
            wire.approval_id,
            wire.operation_fingerprint,
            wire.capabilities,
            wire.resources,
            wire.created_at,
        )
        .map_err(serde::de::Error::custom)
    }
}

/// Invalid durable tree grant.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TreeApprovalGrantError {
    MissingCapability,
    ProcessLocalResource,
    FilesystemResource,
    UnboundResourceCapability,
}

impl fmt::Display for TreeApprovalGrantError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingCapability => {
                formatter.write_str("tree grant requires at least one capability")
            }
            Self::ProcessLocalResource => formatter.write_str(
                "process-local prepared resources cannot be committed to durable tree grants",
            ),
            Self::FilesystemResource => {
                formatter.write_str("filesystem prepared resources cannot be durable tree grants")
            }
            Self::UnboundResourceCapability => formatter.write_str(
                "every tree-grant resource capability must be present in grant capabilities",
            ),
        }
    }
}

impl std::error::Error for TreeApprovalGrantError {}

/// Complete query projection for one approval request.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
#[ts(optional_fields = nullable)]
pub struct ApprovalRecord {
    pub session_id: SessionId,
    pub request: ApprovalRequest,
    pub status: ApprovalStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub internal_decision: Option<ApprovalInternalDecision>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_decision: Option<ApprovalUserDecision>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub final_decision: Option<ApprovalFinalDecision>,
}

/// An opaque, non-filesystem reference to a private engine artifact.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct ArtifactReference {
    pub uri: String,
}

/// A durable attachment produced by a tool. Binary bytes live only in the
/// engine artifact store and are never embedded in protocol events.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
#[ts(optional_fields = nullable)]
pub struct ToolAttachment {
    pub mime_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
    pub byte_length: u64,
    pub sha256: Sha256Digest,
    pub reference: ArtifactReference,
}

/// Details about engine-bounded output and its retained complete artifact.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct ToolOutputTruncation {
    pub original_bytes: u64,
    pub original_lines: u64,
    pub retained: ArtifactReference,
}

/// The rich result sent back to the model and persisted in session history.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
#[ts(optional_fields = nullable)]
pub struct ToolResult {
    pub title: String,
    pub output: String,
    pub metadata: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncation: Option<ToolOutputTruncation>,
    pub attachments: Vec<ToolAttachment>,
}

/// Provider-defined metadata that is safe to persist and disclose to clients.
pub type SafeModelMetadata = BTreeMap<String, Value>;

/// Provider, model, and behavior-affecting resource scope for native context.
#[derive(Clone, Debug, Deserialize, Eq, Hash, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct NativeContextScope {
    pub provider_id: String,
    pub model_id: String,
    pub resource_id: String,
}

/// Failure while constructing a bounded native replay artifact.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NativeReplayArtifactError {
    InvalidIdentity(&'static str),
    PayloadTooLarge { size: usize, maximum: usize },
    PayloadSerialization(String),
}

impl fmt::Display for NativeReplayArtifactError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidIdentity(field) => {
                write!(
                    formatter,
                    "native replay {field} must be non-empty and contain no control characters"
                )
            }
            Self::PayloadTooLarge { size, maximum } => write!(
                formatter,
                "native replay payload is {size} bytes, exceeding the {maximum}-byte limit"
            ),
            Self::PayloadSerialization(message) => {
                write!(
                    formatter,
                    "could not serialize native replay payload: {message}"
                )
            }
        }
    }
}

impl std::error::Error for NativeReplayArtifactError {}

/// A bounded Oven native replay artifact tied to an exact adapter and scope.
///
/// The payload is correctness state. Debug output redacts it, and construction
/// plus deserialization enforce the 2 MiB serialized-payload boundary.
#[derive(Clone, JsonSchema, PartialEq, Serialize, TS)]
#[schemars(deny_unknown_fields)]
pub struct NativeReplayArtifact {
    adapter_id: String,
    scope: NativeContextScope,
    #[schemars(extend("x-max-serialized-bytes" = 2_097_152))]
    payload: Value,
}

impl NativeReplayArtifact {
    /// Largest permitted serialized payload size, in bytes.
    pub const MAX_PAYLOAD_BYTES: usize = 2 * 1024 * 1024;

    /// Creates a validated, bounded replay artifact.
    pub fn new(
        adapter_id: String,
        scope: NativeContextScope,
        payload: Value,
    ) -> Result<Self, NativeReplayArtifactError> {
        validate_replay_identity("adapter_id", &adapter_id)?;
        validate_replay_identity("scope.provider_id", &scope.provider_id)?;
        validate_replay_identity("scope.model_id", &scope.model_id)?;
        validate_replay_identity("scope.resource_id", &scope.resource_id)?;
        let size = serde_json::to_vec(&payload)
            .map_err(|error| NativeReplayArtifactError::PayloadSerialization(error.to_string()))?
            .len();
        if size > Self::MAX_PAYLOAD_BYTES {
            return Err(NativeReplayArtifactError::PayloadTooLarge {
                size,
                maximum: Self::MAX_PAYLOAD_BYTES,
            });
        }
        Ok(Self {
            adapter_id,
            scope,
            payload,
        })
    }

    #[must_use]
    pub fn adapter_id(&self) -> &str {
        &self.adapter_id
    }

    #[must_use]
    pub const fn scope(&self) -> &NativeContextScope {
        &self.scope
    }

    #[must_use]
    pub const fn payload(&self) -> &Value {
        &self.payload
    }
}

fn validate_replay_identity(
    field: &'static str,
    value: &str,
) -> Result<(), NativeReplayArtifactError> {
    if value.trim().is_empty() || value.chars().any(char::is_control) {
        return Err(NativeReplayArtifactError::InvalidIdentity(field));
    }
    Ok(())
}

impl fmt::Debug for NativeReplayArtifact {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("NativeReplayArtifact")
            .field("adapter_id", &self.adapter_id)
            .field("scope", &self.scope)
            .field("payload", &"<redacted>")
            .finish()
    }
}

impl<'de> Deserialize<'de> for NativeReplayArtifact {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct WireArtifact {
            adapter_id: String,
            scope: NativeContextScope,
            payload: Value,
        }

        let wire = WireArtifact::deserialize(deserializer)?;
        Self::new(wire.adapter_id, wire.scope, wire.payload).map_err(serde::de::Error::custom)
    }
}

/// A bounded opaque provider-native context window returned by compaction.
///
/// Debug output redacts the payload. Construction and deserialization enforce
/// the exact 32 MiB serialized-payload boundary.
#[derive(Clone, JsonSchema, PartialEq, Serialize, TS)]
#[schemars(deny_unknown_fields)]
pub struct NativeContextWindow {
    adapter_id: String,
    scope: NativeContextScope,
    #[schemars(extend("x-max-serialized-bytes" = 33_554_432))]
    payload: Value,
}

impl NativeContextWindow {
    /// Largest permitted serialized payload size, in bytes.
    pub const MAX_PAYLOAD_BYTES: usize = 32 * 1024 * 1024;

    /// Creates a validated, bounded native-context window.
    pub fn new(
        adapter_id: String,
        scope: NativeContextScope,
        payload: Value,
    ) -> Result<Self, NativeReplayArtifactError> {
        validate_replay_identity("adapter_id", &adapter_id)?;
        validate_replay_identity("scope.provider_id", &scope.provider_id)?;
        validate_replay_identity("scope.model_id", &scope.model_id)?;
        validate_replay_identity("scope.resource_id", &scope.resource_id)?;
        let size = serde_json::to_vec(&payload)
            .map_err(|error| NativeReplayArtifactError::PayloadSerialization(error.to_string()))?
            .len();
        if size > Self::MAX_PAYLOAD_BYTES {
            return Err(NativeReplayArtifactError::PayloadTooLarge {
                size,
                maximum: Self::MAX_PAYLOAD_BYTES,
            });
        }
        Ok(Self {
            adapter_id,
            scope,
            payload,
        })
    }

    #[must_use]
    pub fn adapter_id(&self) -> &str {
        &self.adapter_id
    }

    #[must_use]
    pub const fn scope(&self) -> &NativeContextScope {
        &self.scope
    }

    #[must_use]
    pub const fn payload(&self) -> &Value {
        &self.payload
    }
}

impl fmt::Debug for NativeContextWindow {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("NativeContextWindow")
            .field("adapter_id", &self.adapter_id)
            .field("scope", &self.scope)
            .field("payload", &"<redacted>")
            .finish()
    }
}

impl<'de> Deserialize<'de> for NativeContextWindow {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct WireWindow {
            adapter_id: String,
            scope: NativeContextScope,
            payload: Value,
        }

        let wire = WireWindow::deserialize(deserializer)?;
        Self::new(wire.adapter_id, wire.scope, wire.payload).map_err(serde::de::Error::custom)
    }
}

/// Safe persisted metadata for a provider-native context artifact.
/// The opaque payload is retained only in the private artifact store.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct NativeContextArtifact {
    pub adapter_id: String,
    pub scope: NativeContextScope,
    #[serde(deserialize_with = "deserialize_native_context_byte_length")]
    #[schemars(range(max = 33_554_432))]
    pub byte_length: u64,
    pub sha256: Sha256Digest,
    pub reference: ArtifactReference,
}

fn deserialize_native_context_byte_length<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let byte_length = u64::deserialize(deserializer)?;
    if byte_length <= NativeContextWindow::MAX_PAYLOAD_BYTES as u64 {
        Ok(byte_length)
    } else {
        Err(serde::de::Error::custom(format!(
            "native context artifact is {byte_length} bytes, exceeding the {}-byte limit",
            NativeContextWindow::MAX_PAYLOAD_BYTES
        )))
    }
}

/// A semantic model completion reason.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum ModelFinishReason {
    Stop,
    ToolCalls,
    Length,
    ContentFilter,
    Cancelled,
    Error,
    Aborted,
    Timeout,
    Refused,
    Unknown,
    Other(String),
}

/// A MIME-typed model file source. Inline bytes, inline text, and base64 are
/// deliberately absent: persisted files are references only.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum PersistedFileSource {
    Artifact {
        byte_length: u64,
        sha256: Sha256Digest,
        reference: ArtifactReference,
    },
    Url {
        url: String,
    },
    ProviderReference {
        provider_id: String,
        id: String,
    },
}

/// A persisted MIME-typed file referenced by model content.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
#[ts(optional_fields = nullable)]
pub struct PersistedFilePart {
    pub media_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
    pub source: PersistedFileSource,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<SafeModelMetadata>,
}

/// One value inside mixed model-visible tool output.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize, TS)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum PersistedContentValue {
    Text { text: String },
    File { file: PersistedFilePart },
    Json { value: Value },
}

/// Model-visible tool output retained inside a committed assistant turn.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize, TS)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
#[ts(optional_fields = nullable)]
pub enum PersistedToolContent {
    Text {
        text: String,
    },
    Json {
        value: Value,
    },
    Mixed {
        values: Vec<PersistedContentValue>,
    },
    Denied {
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
}

/// Ordered replay-relevant content in one completed Oven assistant turn.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize, TS)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
#[ts(optional_fields = nullable)]
pub enum PersistedAssistantPart {
    Text {
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        metadata: Option<SafeModelMetadata>,
    },
    Reasoning {
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        metadata: Option<SafeModelMetadata>,
    },
    ToolCall {
        id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        provider_item_id: Option<String>,
        name: String,
        input: Value,
        #[serde(skip_serializing_if = "Option::is_none")]
        raw_input: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        metadata: Option<SafeModelMetadata>,
    },
    ToolResult {
        tool_call_id: String,
        content: PersistedToolContent,
        is_error: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        metadata: Option<SafeModelMetadata>,
    },
    File {
        file: PersistedFilePart,
    },
    Source {
        #[serde(skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        url: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        media_type: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        excerpt: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        metadata: Option<SafeModelMetadata>,
    },
    ToolApproval {
        tool_call_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        message: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        metadata: Option<SafeModelMetadata>,
    },
    Custom {
        kind: String,
        data: Value,
        #[serde(skip_serializing_if = "Option::is_none")]
        metadata: Option<SafeModelMetadata>,
    },
}

/// A complete committed Oven assistant turn used for durable normalized replay.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
#[ts(optional_fields = nullable)]
pub struct PersistedModelTurn {
    /// Ordered role-safe assistant content.
    pub content: Vec<PersistedAssistantPart>,
    /// Provider options scoped to the assistant message.
    pub provider_options: BTreeMap<String, Value>,
    pub finish_reason: ModelFinishReason,
    pub usage: Usage,
    /// Safe provider response metadata retained by Oven at finish.
    pub response_metadata: SafeModelMetadata,
    /// Safe provider-specific terminal metadata retained by Oven at finish.
    pub provider_metadata: SafeModelMetadata,
    pub warnings: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub native_replay: Option<NativeReplayArtifact>,
}

/// How Oven handled one assistant history entry while encoding a request.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ReplayDisposition {
    Replayed,
    NoArtifact,
    DiscardedForeignAdapter {
        found: String,
        expected: String,
    },
    DiscardedForeignScope {
        found: NativeContextScope,
        expected: NativeContextScope,
    },
    DiscardedInvalidPayload {
        reason: String,
    },
    ReconstructedNormalized,
}

/// One ordered replay decision for a request history entry.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct ReplayDecision {
    pub history_index: u64,
    pub disposition: ReplayDisposition,
}

/// The phase in which an Oven model error occurred.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum ModelErrorStage {
    #[default]
    Unknown,
    RequestValidation,
    RequestEncoding,
    Connect,
    ResponseHeaders,
    ResponseBody,
    StreamRead,
    StreamDecode,
    StreamEvent,
    StreamFinalize,
    ReplayEncode,
    ReplayDecode,
    NativeContextEncode,
    NativeContextDecode,
    Middleware,
}

/// The normalized category of an Oven model error.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum ModelErrorKind {
    Transport,
    Timeout,
    RateLimited,
    Auth,
    PermissionDenied,
    InvalidRequest,
    ModelNotFound,
    ContextLength,
    Quota,
    Overload,
    Unsupported,
    UnexpectedEof,
    InvalidResponse,
    InvalidToolInput,
    ContentFilter,
    Replay,
    NativeContext,
    Provider,
    Abort,
    Unknown,
}

/// Structured safe model failure details exposed by protocol events.
///
/// Oven's sanitized response body is intentionally excluded.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
#[ts(optional_fields = nullable)]
pub struct ModelErrorSummary {
    pub kind: ModelErrorKind,
    pub message: String,
    pub retryable: bool,
    pub stage: ModelErrorStage,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub http_status: Option<u16>,
    pub bytes_received: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vendor_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_after_ms: Option<u64>,
}

/// Identity of the profile selected for the current run.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct ProfileIdentity {
    pub name: String,
    pub agent_type: AgentType,
}

/// Engine-owned internal agent purpose.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum InternalAgentKind {
    Approval,
    ContextCompaction,
    SessionTitle,
}

/// Backend selected for one internal-agent attempt.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize, TS)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum InternalAgentBackend {
    Model {
        profile: Box<ProfileSnapshot>,
        model: ModelRef,
    },
    ProviderNative {
        model: ModelRef,
    },
    Builtin {
        name: String,
        revision: String,
    },
}

/// Sanitized description of an internal-agent call.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct SafeInternalAgentCall {
    pub name: String,
    pub input_summary: String,
    pub input_digest: Sha256Digest,
}

/// Sanitized terminal result metadata for an internal-agent call.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct SafeInternalAgentResult {
    pub output_summary: String,
    pub output_digest: Sha256Digest,
}

/// Sanitized internal-agent failure. Provider bodies and secrets are excluded.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
#[ts(optional_fields = nullable)]
pub struct InternalAgentFailure {
    pub code: String,
    pub message: String,
    pub retryable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_error: Option<ModelErrorSummary>,
}

/// Validated idempotency identity for one `session.rename` request.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, TS)]
#[ts(type = "string")]
pub struct ClientRenameId(String);

impl ClientRenameId {
    pub const MAX_BYTES: usize = 256;

    pub fn new(value: impl Into<String>) -> Result<Self, ClientRenameIdError> {
        let value = value.into();
        if value.is_empty() {
            return Err(ClientRenameIdError::Empty);
        }
        if value.len() > Self::MAX_BYTES {
            return Err(ClientRenameIdError::TooLong {
                found: value.len(),
                maximum: Self::MAX_BYTES,
            });
        }
        if value.chars().any(char::is_control) {
            return Err(ClientRenameIdError::ControlCharacter);
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ClientRenameId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl Serialize for ClientRenameId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for ClientRenameId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

impl JsonSchema for ClientRenameId {
    fn inline_schema() -> bool {
        true
    }

    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("ClientRenameId")
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "string",
            "minLength": 1,
            "maxLength": 256,
            "description": "Non-empty idempotency key without control characters."
        })
    }
}

/// Validation error for `client_rename_id`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ClientRenameIdError {
    Empty,
    TooLong { found: usize, maximum: usize },
    ControlCharacter,
}

impl fmt::Display for ClientRenameIdError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => formatter.write_str("client_rename_id must not be empty"),
            Self::TooLong { found, maximum } => write!(
                formatter,
                "client_rename_id is {found} bytes, exceeding the {maximum}-byte limit"
            ),
            Self::ControlCharacter => {
                formatter.write_str("client_rename_id must not contain control characters")
            }
        }
    }
}

impl std::error::Error for ClientRenameIdError {}

/// Strict durable title commit. Variant shape makes source/operation identity
/// and `client_rename_id` requirements unrepresentable as invalid combinations.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum SessionTitleCommit {
    UserSet {
        title: SessionTitle,
        client_rename_id: ClientRenameId,
    },
    UserClear {
        client_rename_id: ClientRenameId,
    },
    UserReset {
        client_rename_id: ClientRenameId,
    },
    InternalAgentSet {
        title: SessionTitle,
        invocation_id: InternalAgentInvocationId,
    },
    FallbackSet {
        title: SessionTitle,
    },
}

impl SessionTitleCommit {
    /// Reconstructs the durable idempotency record for a user rename event.
    #[must_use]
    pub fn user_rename_record(&self) -> Option<SessionRenameRecord> {
        match self {
            Self::UserSet {
                title,
                client_rename_id,
            } => Some(SessionRenameRecord {
                client_rename_id: client_rename_id.clone(),
                change: SessionRenameChange::Set {
                    title: title.clone(),
                },
            }),
            Self::UserClear { client_rename_id } => Some(SessionRenameRecord {
                client_rename_id: client_rename_id.clone(),
                change: SessionRenameChange::Clear,
            }),
            Self::UserReset { client_rename_id } => Some(SessionRenameRecord {
                client_rename_id: client_rename_id.clone(),
                change: SessionRenameChange::Reset,
            }),
            Self::InternalAgentSet { .. } | Self::FallbackSet { .. } => None,
        }
    }
}

/// Event-sequence boundaries covered by a context checkpoint.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
#[ts(optional_fields = nullable)]
pub struct ContextCheckpointBoundaries {
    pub source_from_seq: u64,
    pub source_through_seq: u64,
    pub input_through_seq: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prior_checkpoint_seq: Option<u64>,
}

/// Frozen token and byte budgets used to produce a checkpoint.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct ContextCheckpointBudgets {
    pub context_limit_tokens: u64,
    pub trigger_tokens: u64,
    pub target_tokens: u64,
    pub input_tokens_before: u64,
    pub input_tokens_after: u64,
    pub max_summary_bytes: SummaryByteLimit,
}

/// Configured summary-byte limit, always bounded by the global 2 MiB ceiling.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, TS)]
#[ts(type = "number")]
pub struct SummaryByteLimit(u64);

impl SummaryByteLimit {
    pub const GLOBAL_MAX_BYTES: u64 = 2 * 1024 * 1024;

    pub fn new(value: u64) -> Result<Self, ContextCheckpointError> {
        if value > Self::GLOBAL_MAX_BYTES {
            return Err(ContextCheckpointError::SummaryLimitTooLarge { found: value });
        }
        Ok(Self(value))
    }

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl Serialize for SummaryByteLimit {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_u64(self.0)
    }
}

impl<'de> Deserialize<'de> for SummaryByteLimit {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Self::new(u64::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

impl JsonSchema for SummaryByteLimit {
    fn inline_schema() -> bool {
        true
    }

    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("SummaryByteLimit")
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        json_schema!({"type": "integer", "minimum": 0, "maximum": 2_097_152})
    }
}

/// A bounded, integrity-checked internal summary checkpoint.
#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct InternalSummaryCheckpoint {
    summary: String,
    byte_length: u64,
    sha256: Sha256Digest,
    invocation_id: InternalAgentInvocationId,
    internal_run_id: InternalAgentRunId,
}

impl InternalSummaryCheckpoint {
    pub fn new(
        summary: String,
        invocation_id: InternalAgentInvocationId,
        internal_run_id: InternalAgentRunId,
        max_summary_bytes: SummaryByteLimit,
    ) -> Result<Self, ContextCheckpointError> {
        let byte_length = summary.len() as u64;
        if byte_length > SummaryByteLimit::GLOBAL_MAX_BYTES {
            return Err(ContextCheckpointError::SummaryTooLarge {
                found: byte_length,
                maximum: SummaryByteLimit::GLOBAL_MAX_BYTES,
            });
        }
        if byte_length > max_summary_bytes.get() {
            return Err(ContextCheckpointError::SummaryTooLarge {
                found: byte_length,
                maximum: max_summary_bytes.get(),
            });
        }
        let sha256 = Sha256Digest::of_bytes(summary.as_bytes());
        Ok(Self {
            summary,
            byte_length,
            sha256,
            invocation_id,
            internal_run_id,
        })
    }

    fn from_wire(
        summary: String,
        byte_length: u64,
        sha256: Sha256Digest,
        invocation_id: InternalAgentInvocationId,
        internal_run_id: InternalAgentRunId,
    ) -> Result<Self, ContextCheckpointError> {
        let actual_length = summary.len() as u64;
        if actual_length > SummaryByteLimit::GLOBAL_MAX_BYTES {
            return Err(ContextCheckpointError::SummaryTooLarge {
                found: actual_length,
                maximum: SummaryByteLimit::GLOBAL_MAX_BYTES,
            });
        }
        if byte_length != actual_length {
            return Err(ContextCheckpointError::SummaryByteLengthMismatch {
                declared: byte_length,
                actual: actual_length,
            });
        }
        let actual_sha256 = Sha256Digest::of_bytes(summary.as_bytes());
        if sha256 != actual_sha256 {
            return Err(ContextCheckpointError::SummaryDigestMismatch);
        }
        Ok(Self {
            summary,
            byte_length,
            sha256,
            invocation_id,
            internal_run_id,
        })
    }

    #[must_use]
    pub fn summary(&self) -> &str {
        &self.summary
    }

    #[must_use]
    pub const fn byte_length(&self) -> u64 {
        self.byte_length
    }

    #[must_use]
    pub const fn sha256(&self) -> &Sha256Digest {
        &self.sha256
    }
}

impl<'de> Deserialize<'de> for InternalSummaryCheckpoint {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct WireSummary {
            summary: String,
            byte_length: u64,
            sha256: Sha256Digest,
            invocation_id: InternalAgentInvocationId,
            internal_run_id: InternalAgentRunId,
        }

        let wire = WireSummary::deserialize(deserializer)?;
        Self::from_wire(
            wire.summary,
            wire.byte_length,
            wire.sha256,
            wire.invocation_id,
            wire.internal_run_id,
        )
        .map_err(serde::de::Error::custom)
    }
}

/// Validation failure for summary checkpoints and their configured budgets.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ContextCheckpointError {
    SummaryLimitTooLarge { found: u64 },
    SummaryTooLarge { found: u64, maximum: u64 },
    SummaryByteLengthMismatch { declared: u64, actual: u64 },
    SummaryDigestMismatch,
}

impl fmt::Display for ContextCheckpointError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SummaryLimitTooLarge { found } => write!(
                formatter,
                "summary byte limit {found} exceeds the global {}-byte ceiling",
                SummaryByteLimit::GLOBAL_MAX_BYTES
            ),
            Self::SummaryTooLarge { found, maximum } => {
                write!(
                    formatter,
                    "summary is {found} bytes, exceeding the {maximum}-byte limit"
                )
            }
            Self::SummaryByteLengthMismatch { declared, actual } => write!(
                formatter,
                "summary byte length mismatch: declared {declared}, actual {actual}"
            ),
            Self::SummaryDigestMismatch => formatter.write_str("summary SHA-256 digest mismatch"),
        }
    }
}

impl std::error::Error for ContextCheckpointError {}

/// Durable model-visible context checkpoint. Raw history remains in the log.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ContextCheckpoint {
    ProviderNative {
        model: ModelRef,
        native_context: NativeContextArtifact,
    },
    InternalSummary {
        #[serde(flatten)]
        checkpoint: InternalSummaryCheckpoint,
    },
}

/// A self-validating checkpoint commit with frozen boundaries and budgets.
#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct ContextCheckpointCommit {
    checkpoint: ContextCheckpoint,
    boundaries: ContextCheckpointBoundaries,
    budgets: ContextCheckpointBudgets,
}

impl ContextCheckpointCommit {
    pub fn new(
        checkpoint: ContextCheckpoint,
        boundaries: ContextCheckpointBoundaries,
        budgets: ContextCheckpointBudgets,
    ) -> Result<Self, ContextCheckpointError> {
        if let ContextCheckpoint::InternalSummary { checkpoint } = &checkpoint
            && checkpoint.byte_length() > budgets.max_summary_bytes.get()
        {
            return Err(ContextCheckpointError::SummaryTooLarge {
                found: checkpoint.byte_length(),
                maximum: budgets.max_summary_bytes.get(),
            });
        }
        Ok(Self {
            checkpoint,
            boundaries,
            budgets,
        })
    }

    #[must_use]
    pub const fn checkpoint(&self) -> &ContextCheckpoint {
        &self.checkpoint
    }

    #[must_use]
    pub const fn boundaries(&self) -> &ContextCheckpointBoundaries {
        &self.boundaries
    }

    #[must_use]
    pub const fn budgets(&self) -> &ContextCheckpointBudgets {
        &self.budgets
    }
}

impl<'de> Deserialize<'de> for ContextCheckpointCommit {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct WireCommit {
            checkpoint: ContextCheckpoint,
            boundaries: ContextCheckpointBoundaries,
            budgets: ContextCheckpointBudgets,
        }

        let wire = WireCommit::deserialize(deserializer)?;
        Self::new(wire.checkpoint, wire.boundaries, wire.budgets).map_err(serde::de::Error::custom)
    }
}

/// A durable event payload from a session event log.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize, TS)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
#[ts(optional_fields = nullable)]
pub enum Event {
    SessionCreated {
        meta: SessionMeta,
    },
    RunStarted {
        client_run_id: String,
        input: String,
        profile: ProfileSnapshot,
        current_profile: ProfileIdentity,
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
        /// Engine-generated durable execution and audit identity.
        tool_call_id: ToolCallId,
        /// Oven semantic tool-call identity used to pair model history.
        model_call_id: String,
        /// Provider item identity, when distinct from the Oven semantic ID.
        #[serde(skip_serializing_if = "Option::is_none")]
        provider_item_id: Option<String>,
        tool: String,
        arguments: Value,
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
        code: ToolCallFailureCode,
        message: String,
    },
    ApprovalRequested {
        request: ApprovalRequest,
    },
    ApprovalEvaluated {
        approval_id: ApprovalId,
        decision: ApprovalInternalDecision,
    },
    ApprovalEscalated {
        approval_id: ApprovalId,
        reason_code: ApprovalReasonCode,
    },
    ApprovalUserDecisionRecorded {
        approval_id: ApprovalId,
        client_response_id: String,
        decision: ApprovalUserDecision,
        #[serde(skip_serializing_if = "Option::is_none")]
        feedback: Option<ApprovalFeedback>,
    },
    ApprovalFinalized {
        approval_id: ApprovalId,
        decision: ApprovalFinalDecision,
    },
    ApprovalCancelled {
        approval_id: ApprovalId,
        reason_code: ApprovalReasonCode,
    },
    ApprovalDoomLoopDetected {
        approval_id: ApprovalId,
        operation_fingerprint: OperationFingerprint,
        repetitions: u32,
    },
    TreeApprovalGrantCommitted {
        grant: TreeApprovalGrant,
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
    /// Complete normalized assistant turn committed after a valid Oven finish.
    ModelTurnCommitted {
        model: ModelRef,
        input_through_seq: u64,
        turn: PersistedModelTurn,
    },
    /// Complete ordered Oven replay decisions made before model output.
    ModelReplayEvaluated {
        model: ModelRef,
        decisions: Vec<ReplayDecision>,
    },
    ModelFallback {
        from: ModelRef,
        to: ModelRef,
        error: ModelErrorSummary,
        attempts: u32,
    },
    InternalAgentStarted {
        invocation_id: InternalAgentInvocationId,
        internal_run_id: InternalAgentRunId,
        kind: InternalAgentKind,
        backend: InternalAgentBackend,
        call: SafeInternalAgentCall,
    },
    InternalAgentCompleted {
        invocation_id: InternalAgentInvocationId,
        internal_run_id: InternalAgentRunId,
        kind: InternalAgentKind,
        result: SafeInternalAgentResult,
    },
    InternalAgentFailed {
        invocation_id: InternalAgentInvocationId,
        internal_run_id: InternalAgentRunId,
        kind: InternalAgentKind,
        failure: InternalAgentFailure,
    },
    InternalAgentCancelled {
        invocation_id: InternalAgentInvocationId,
        internal_run_id: InternalAgentRunId,
        kind: InternalAgentKind,
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    InternalAgentInterrupted {
        invocation_id: InternalAgentInvocationId,
        internal_run_id: InternalAgentRunId,
        kind: InternalAgentKind,
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    InternalAgentFallback {
        invocation_id: InternalAgentInvocationId,
        internal_run_id: InternalAgentRunId,
        kind: InternalAgentKind,
        from: InternalAgentBackend,
        to: InternalAgentBackend,
        failure: InternalAgentFailure,
        attempts: u32,
    },
    ContextCheckpointCommitted {
        #[serde(flatten)]
        commit: ContextCheckpointCommit,
    },
    SessionTitleCommitted {
        input_through_seq: u64,
        commit: SessionTitleCommit,
    },
}

/// A persisted event with authoritative per-session ordering metadata.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
#[ts(optional_fields = nullable)]
pub struct EventEnvelope {
    pub schema_version: EventSchemaVersion,
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
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
pub struct OutputDelta {
    pub call_id: ToolCallId,
    pub stream: OutputStream,
    pub byte_offset: u64,
    pub data: String,
}

/// Ephemeral marker indicating a subscriber missed evicted or queued output.
/// This type is never persisted or cursor-replayed.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct OutputGap {
    pub call_id: ToolCallId,
    pub stream: OutputStream,
    pub next_offset: u64,
}

/// Ephemeral buffered output used for atomic snapshot-to-live handoff. This
/// type is never persisted or cursor-replayed.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
pub struct OutputSnapshotEnvelope {
    pub stream: OutputStream,
    pub snapshot: OutputSnapshot,
}

/// Parameters for `session.create`.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct SessionCreateParams {
    pub cwd: String,
    pub profile: String,
}

/// Result for `session.create`.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct SessionCreateResult {
    pub session: SessionMeta,
}

/// Parameters for `session.list`.
#[derive(Clone, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
#[ts(optional_fields = nullable)]
pub struct SessionListParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}

/// Result for `session.list`.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct SessionListResult {
    pub sessions: Vec<SessionMeta>,
}

/// Parameters for `session.get`.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct SessionGetParams {
    pub session_id: SessionId,
}

/// Result for `session.get`.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct SessionGetResult {
    pub session: SessionMeta,
}

/// Parameters for `session.children`.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct SessionChildrenParams {
    pub session_id: SessionId,
}

/// Result for `session.children`.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct SessionChildrenResult {
    pub children: Vec<ChildSummary>,
}

/// Parameters for `session.tree`.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct SessionTreeParams {
    pub session_id: SessionId,
}

/// Result for `session.tree`.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct SessionTreeResult {
    pub tree: SessionTree,
}

/// Parameters for `session.resume`.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct SessionResumeParams {
    pub session_id: SessionId,
}

/// Result for `session.resume`.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct SessionResumeResult {
    pub session: SessionMeta,
}

/// User-authored title mutation for `session.rename`.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum SessionRenameChange {
    Set { title: SessionTitle },
    Clear,
    Reset,
}

/// Parameters for idempotent `session.rename`.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct SessionRenameParams {
    pub session_id: SessionId,
    pub client_rename_id: ClientRenameId,
    pub change: SessionRenameChange,
}

/// Result for `session.rename`.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct SessionRenameResult {
    pub client_rename_id: ClientRenameId,
    pub session: SessionMeta,
}

/// Durable replay projection rebuilt from user-authored title commit events.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct SessionRenameRecord {
    pub client_rename_id: ClientRenameId,
    pub change: SessionRenameChange,
}

impl SessionRenameRecord {
    /// Returns whether reusing this durable ID conflicts with the request.
    #[must_use]
    pub fn conflicts_with(&self, request: &SessionRenameParams) -> bool {
        self.client_rename_id == request.client_rename_id && self.change != request.change
    }

    /// Returns whether this request is an exact idempotent replay.
    #[must_use]
    pub fn matches(&self, request: &SessionRenameParams) -> bool {
        self.client_rename_id == request.client_rename_id && self.change == request.change
    }
}

/// Stable `session.rename` error identifiers.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum SessionRenameErrorCode {
    SessionNotFound,
    InvalidTitle,
    IdempotencyConflict,
}

/// Typed safe error data for `session.rename`.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct SessionRenameError {
    pub code: SessionRenameErrorCode,
    pub session_id: SessionId,
    pub client_rename_id: ClientRenameId,
}

/// Post-MVP parameters for `session.fork`.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct SessionForkParams {
    pub source_session_id: SessionId,
    pub source_event_seq: u64,
}

/// Post-MVP result for `session.fork`.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct SessionForkResult {
    pub session: SessionMeta,
}

/// Parameters for `run.start`. `client_run_id` is an idempotency key.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
#[ts(optional_fields = nullable)]
pub struct RunStartParams {
    pub session_id: SessionId,
    pub client_run_id: String,
    pub input: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
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
#[serde(deny_unknown_fields)]
pub struct RunStartConflict {
    pub code: RunStartConflictCode,
    pub session_id: SessionId,
    pub client_run_id: String,
}

/// Result for `run.start`.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct RunStartResult {
    pub run_id: RunId,
}

/// Parameters for `run.steer`.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct RunSteerParams {
    pub run_id: RunId,
    pub input: String,
}

/// Result for `run.steer`.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct RunSteerResult {
    pub accepted: bool,
}

/// Parameters for `run.cancel`.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct RunCancelParams {
    pub run_id: RunId,
}

/// Result for `run.cancel`.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct RunCancelResult {
    pub cancelled: bool,
}

/// Parameters for `run.tool_stdin`. `data`, when present, is base64 bytes.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
#[ts(optional_fields = nullable)]
pub struct RunToolStdinParams {
    pub run_id: RunId,
    pub call_id: ToolCallId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<String>,
    pub eof: bool,
}

/// Result for `run.tool_stdin`.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct RunToolStdinResult {
    pub accepted: bool,
}

/// Parameters for `events.subscribe`. The optional cursor is a persisted-event
/// sequence number; ephemeral output is not replayed through it.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
#[ts(optional_fields = nullable)]
pub struct EventsSubscribeParams {
    pub session_id: SessionId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<u64>,
}

/// Initial replay result for `events.subscribe`; future events arrive as
/// notifications.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct EventsSubscribeResult {
    pub events: Vec<EventEnvelope>,
}

/// Parameters for `approval.respond`. The response is idempotent by
/// `client_response_id`; grant boundaries are never client-editable.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
#[ts(optional_fields = nullable)]
pub struct ApprovalRespondParams {
    pub session_id: SessionId,
    pub approval_id: ApprovalId,
    pub request_revision: u64,
    pub operation_fingerprint: OperationFingerprint,
    pub client_response_id: String,
    pub decision: ApprovalUserDecision,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub feedback: Option<ApprovalFeedback>,
}

/// Result for `approval.respond`.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct ApprovalRespondResult {
    pub client_response_id: String,
    pub approval: ApprovalRecord,
}

/// Stable error identifiers for `approval.respond`.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
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

/// Typed JSON-RPC error data for `approval.respond`.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
#[ts(optional_fields = nullable)]
pub struct ApprovalRespondError {
    pub code: ApprovalRespondErrorCode,
    pub session_id: SessionId,
    pub approval_id: ApprovalId,
    pub client_response_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected_revision: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub found_revision: Option<u64>,
}

/// Parameters for `approval.list`.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
#[ts(optional_fields = nullable)]
pub struct ApprovalListParams {
    pub root_session_id: SessionId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<ApprovalStatus>,
}

/// Result for `approval.list`.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct ApprovalListResult {
    pub approvals: Vec<ApprovalRecord>,
    pub tree_grants: Vec<TreeApprovalGrant>,
}

/// Immutable source snapshot identity for models.dev catalog projections.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct CatalogSnapshot {
    pub revision: String,
    pub source: String,
    pub fetched_at: Timestamp,
}

/// One models.dev provider projection. Environment variable names are safe;
/// credential values are never present.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
#[ts(optional_fields = nullable)]
pub struct CatalogProvider {
    pub id: String,
    pub name: String,
    pub credential_fields: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub npm: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub documentation_url: Option<String>,
}

/// Catalog model lifecycle status.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum CatalogModelStatus {
    Stable,
    Alpha,
    Beta,
    Deprecated,
}

/// Safe catalog capability flags.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct CatalogModelCapabilities {
    pub attachment: bool,
    pub reasoning: bool,
    pub tool_call: bool,
    pub structured_output: bool,
    pub temperature: bool,
}

/// Catalog token limits.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
#[ts(optional_fields = nullable)]
pub struct CatalogModelLimits {
    pub context: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input: Option<u64>,
    pub output: u64,
}

/// Catalog input and output modalities.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct CatalogModelModalities {
    pub input: Vec<String>,
    pub output: Vec<String>,
}

/// One provider-specific models.dev model projection.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
#[ts(optional_fields = nullable)]
pub struct CatalogModel {
    pub provider_id: String,
    pub model_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub canonical_model_id: Option<String>,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub family: Option<String>,
    pub capabilities: CatalogModelCapabilities,
    pub limits: CatalogModelLimits,
    pub modalities: CatalogModelModalities,
    pub status: CatalogModelStatus,
    pub release_date: String,
    pub last_updated: String,
}

/// Parameters for `catalog.provider.list`.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct CatalogProviderListParams {}

/// Result for `catalog.provider.list`.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct CatalogProviderListResult {
    pub snapshot: CatalogSnapshot,
    pub providers: Vec<CatalogProvider>,
}

/// Parameters for `catalog.model.list`.
#[derive(Clone, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
#[ts(optional_fields = nullable)]
pub struct CatalogModelListParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider_id: Option<String>,
}

/// Result for `catalog.model.list`.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct CatalogModelListResult {
    pub snapshot: CatalogSnapshot,
    pub models: Vec<CatalogModel>,
}

/// Stable catalog RPC error identifiers.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum CatalogErrorCode {
    CatalogUnavailable,
    CatalogSnapshotInvalid,
    CatalogRevisionNotFound,
}

/// Typed safe error data for catalog RPCs.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
#[ts(optional_fields = nullable)]
pub struct CatalogError {
    pub code: CatalogErrorCode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
}

/// Credential values accepted only by `provider.connect` request transport.
/// This type must never be embedded in events, results, errors, logs, examples,
/// or persistence records.
#[derive(Clone, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct ProviderCredentials {
    #[schemars(extend("writeOnly" = true))]
    pub values: BTreeMap<String, String>,
}

impl fmt::Debug for ProviderCredentials {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderCredentials")
            .field("values", &"<redacted>")
            .finish()
    }
}

/// Secret-bearing parameters for `provider.connect`; request transport is the
/// sole serialization boundary for `credentials`.
#[derive(Clone, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct ProviderConnectParams {
    pub client_connect_id: String,
    pub provider_id: String,
    pub catalog_revision: String,
    pub credentials: ProviderCredentials,
}

impl fmt::Debug for ProviderConnectParams {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderConnectParams")
            .field("client_connect_id", &self.client_connect_id)
            .field("provider_id", &self.provider_id)
            .field("catalog_revision", &self.catalog_revision)
            .field("credentials", &"<redacted>")
            .finish()
    }
}

/// Safe connection projection returned after credentials are stored.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct ProviderConnection {
    pub provider_id: String,
    pub credential_fields: Vec<String>,
    pub connected_at: Timestamp,
    pub catalog_revision: String,
}

/// Result for `provider.connect`; credential values are never echoed.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct ProviderConnectResult {
    pub client_connect_id: String,
    pub connection: ProviderConnection,
    pub model_revision: String,
}

/// Stable `provider.connect` error identifiers.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum ProviderConnectErrorCode {
    UnknownProvider,
    UnsupportedProvider,
    CatalogRevisionConflict,
    MissingCredential,
    InvalidCredential,
    CredentialStorageFailed,
    IdempotencyConflict,
}

/// Typed safe error data for `provider.connect`.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct ProviderConnectError {
    pub code: ProviderConnectErrorCode,
    pub provider_id: String,
    pub client_connect_id: String,
}

/// Parameters for `model.list`.
///
/// The method lists the current configured/connected runtime model snapshot.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct ModelListParams {}

/// Result for `model.list`.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
#[ts(optional_fields = nullable)]
pub struct ModelListResult {
    pub revision: String,
    pub generated_at: Timestamp,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub catalog_revision: Option<String>,
    pub models: Vec<ModelRef>,
}

/// Stable `model.list` error identifiers.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum ModelListErrorCode {
    ModelSnapshotUnavailable,
    ModelSnapshotInvalid,
}

/// Typed safe error data for `model.list`.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct ModelListError {
    pub code: ModelListErrorCode,
}

/// Parameters for `agent.list`.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
#[ts(optional_fields = nullable)]
pub struct AgentListParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_type: Option<AgentType>,
}

/// An agent profile descriptor returned by `agent.list`.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct AgentDescriptor {
    pub name: String,
    pub agent_type: AgentType,
    pub enabled: bool,
    pub models: Vec<ModelRef>,
}

/// Result for `agent.list`.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(deny_unknown_fields)]
pub struct AgentListResult {
    pub agents: Vec<AgentDescriptor>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use insta::{assert_json_snapshot, assert_snapshot};
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

    fn model() -> ModelRef {
        ModelRef {
            name: "primary-model".into(),
            provider_id: "test-provider".into(),
            model_id: "test-model".into(),
            adapter_id: "test-adapter".into(),
        }
    }

    fn usage() -> Usage {
        Usage {
            input_tokens: Some(10),
            input_tokens_no_cache: Some(7),
            input_tokens_cache_read: Some(2),
            input_tokens_cache_write: Some(1),
            output_tokens: Some(5),
            output_tokens_text: Some(3),
            output_tokens_reasoning: Some(2),
        }
    }

    fn replay_scope() -> NativeContextScope {
        NativeContextScope {
            provider_id: "test-provider".into(),
            model_id: "test-model".into(),
            resource_id: "sha256:resource".into(),
        }
    }

    fn replay_artifact() -> NativeReplayArtifact {
        NativeReplayArtifact::new(
            "test-adapter".into(),
            replay_scope(),
            serde_json::json!({"native": ["state"]}),
        )
        .expect("valid replay artifact")
    }

    fn persisted_file() -> PersistedFilePart {
        PersistedFilePart {
            media_type: "image/png".into(),
            filename: Some("image.png".into()),
            source: PersistedFileSource::Artifact {
                byte_length: 8,
                sha256: digest("persisted-file"),
                reference: ArtifactReference {
                    uri: format!("artifact://sha256/{}", "00".repeat(32)),
                },
            },
            metadata: None,
        }
    }

    fn persisted_turn() -> PersistedModelTurn {
        PersistedModelTurn {
            content: vec![
                PersistedAssistantPart::Text {
                    text: "answer".into(),
                    metadata: None,
                },
                PersistedAssistantPart::Reasoning {
                    text: "summary".into(),
                    metadata: None,
                },
                PersistedAssistantPart::ToolCall {
                    id: "model-call".into(),
                    provider_item_id: Some("provider-item".into()),
                    name: "read".into(),
                    input: serde_json::json!({"path": "x"}),
                    raw_input: Some("{\"path\":\"x\"}".into()),
                    metadata: None,
                },
                PersistedAssistantPart::ToolResult {
                    tool_call_id: "hosted-call".into(),
                    content: PersistedToolContent::Mixed {
                        values: vec![
                            PersistedContentValue::Text {
                                text: "result".into(),
                            },
                            PersistedContentValue::Json {
                                value: serde_json::json!({"ok": true}),
                            },
                            PersistedContentValue::File {
                                file: persisted_file(),
                            },
                        ],
                    },
                    is_error: false,
                    metadata: None,
                },
                PersistedAssistantPart::File {
                    file: persisted_file(),
                },
                PersistedAssistantPart::Source {
                    id: Some("source".into()),
                    url: Some("https://example.invalid/source".into()),
                    title: Some("Source".into()),
                    media_type: Some("text/html".into()),
                    excerpt: Some("excerpt".into()),
                    metadata: None,
                },
                PersistedAssistantPart::ToolApproval {
                    tool_call_id: "model-call".into(),
                    message: Some("confirm".into()),
                    metadata: None,
                },
                PersistedAssistantPart::Custom {
                    kind: "test.extension".into(),
                    data: serde_json::json!({"safe": true}),
                    metadata: None,
                },
            ],
            provider_options: BTreeMap::from([(
                "test".into(),
                serde_json::json!({"option": true}),
            )]),
            finish_reason: ModelFinishReason::ToolCalls,
            usage: usage(),
            response_metadata: BTreeMap::from([("request_id".into(), Value::from("req"))]),
            provider_metadata: BTreeMap::from([("tier".into(), Value::from("standard"))]),
            warnings: vec!["safe warning".into()],
            native_replay: Some(replay_artifact()),
        }
    }

    fn model_error() -> ModelErrorSummary {
        ModelErrorSummary {
            kind: ModelErrorKind::RateLimited,
            message: "rate limited".into(),
            retryable: true,
            stage: ModelErrorStage::ResponseHeaders,
            http_status: Some(429),
            bytes_received: 0,
            vendor_code: Some("rate_limit".into()),
            request_id: Some("request".into()),
            retry_after_ms: Some(100),
        }
    }

    fn profile() -> ProfileSnapshot {
        ProfileSnapshot {
            name: "primary".into(),
            agent_type: AgentType::Primary,
            models: vec![model()],
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

    fn title(value: &str) -> SessionTitle {
        SessionTitle::new(value).expect("valid session title")
    }

    fn rename_id(value: &str) -> ClientRenameId {
        ClientRenameId::new(value).expect("valid client rename id")
    }

    fn meta() -> SessionMeta {
        SessionMeta {
            id: session_id(),
            origin: SessionOrigin::Root,
            cwd: "/workspace".into(),
            profile: profile(),
            title: Some(title("Test session")),
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
            }],
            effect: Effect::Allow,
            precedence_reason: "last matching rule".into(),
        }
    }

    fn digest(value: &str) -> Sha256Digest {
        Sha256Digest::of_bytes(value.as_bytes())
    }

    fn approval_id() -> ApprovalId {
        ApprovalId(Uuid::from_u128(8))
    }

    fn prepared_resource(lifetime: PreparedBindingLifetime) -> PreparedApprovalResource {
        PreparedApprovalResource {
            capability: ActionKind::Bash,
            canonical: PreparedResourceIdentity::new("command:git-status")
                .expect("valid prepared resource identity"),
            binding_digest: PreparedResourceDigest::from_canonical_binding_bytes(
                b"executable-content-and-open-directory-binding",
            ),
            binding_lifetime: lifetime,
            boundary: ApprovalBoundary::CommandPrefix {
                prefix: "git status".into(),
            },
            source: ApprovalResourceSource::PrimaryOperation,
        }
    }

    fn prepared_operation(lifetime: PreparedBindingLifetime) -> PreparedOperationIdentity {
        PreparedOperationIdentity::new(
            digest("normalized arguments without raw paths"),
            vec![ApprovalCapability {
                action: ActionKind::Bash,
                operation: PreparedCapabilityOperation::new("execute")
                    .expect("valid prepared capability operation"),
            }],
            vec![prepared_resource(lifetime)],
            digest("execution context"),
        )
        .expect("valid prepared operation")
    }

    fn approval_request() -> ApprovalRequest {
        let operation = prepared_operation(PreparedBindingLifetime::ProcessLocal);
        let resource_digest = operation.resources()[0].binding_digest.clone();
        ApprovalRequest::new(
            approval_id(),
            1,
            ApprovalTrigger::PermissionPolicy,
            operation,
            vec![ApprovalEvaluation {
                resource_digest,
                effect: Effect::Ask,
                trace: trace(),
            }],
            ApprovalConstraints {
                allow_once: true,
                allow_tree_grant: false,
                cancellable: true,
                expires_at: None,
            },
        )
        .expect("valid approval request")
    }

    fn final_approval_decision() -> ApprovalFinalDecision {
        ApprovalFinalDecision {
            outcome: ApprovalFinalOutcome::Approved,
            source: ApprovalDecisionSource::User,
            reason_code: ApprovalReasonCode::UserApprovedOnce,
            feedback: Some(ApprovalFeedback {
                message: "okay".into(),
            }),
            tree_grant_id: None,
        }
    }

    fn envelope() -> EventEnvelope {
        EventEnvelope {
            schema_version: EventSchemaVersion::current(),
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
            jsonrpc: JsonRpcVersion::current(),
            id: JsonRpcId::Number(1),
            result: Value::Null,
        }));
        round_trip(Response::Error(ErrorResponse {
            jsonrpc: JsonRpcVersion::current(),
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
            ActionKind::Delegate,
            ActionKind::ExternalDirectory,
        ] {
            round_trip(action);
        }
        for effect in [Effect::Allow, Effect::Ask, Effect::Deny] {
            round_trip(effect);
        }
        for decision in [
            ApprovalUserDecision::ApproveOnce,
            ApprovalUserDecision::ApproveTree,
            ApprovalUserDecision::Reject,
            ApprovalUserDecision::Cancel,
        ] {
            round_trip(decision);
        }
        round_trip(ApprovalDecisionSource::Policy);
        round_trip(ApprovalDecisionSource::Model);
        round_trip(PreparedBindingLifetime::ProcessLocal);
        round_trip(PreparedBindingLifetime::RestartStable);
        round_trip(PreparedCapabilityLifetime::ProcessLocal);
        round_trip(ToolCallFailureCode::OperationChanged);
        round_trip(ToolCallFailureCode::PreparedCapabilityLost);
        round_trip(ToolCallFailureCode::UnsupportedPlatform);
        round_trip(ApprovalRespondErrorCode::OperationChanged);
        round_trip(ModelFinishReason::Stop);
        round_trip(ModelFinishReason::Other("provider_reason".into()));
        round_trip(ModelErrorStage::StreamFinalize);
        round_trip(ModelErrorKind::InvalidResponse);
        round_trip(ReplayDisposition::Replayed);
        round_trip(ReplayDisposition::DiscardedForeignScope {
            found: replay_scope(),
            expected: NativeContextScope {
                resource_id: "sha256:other".into(),
                ..replay_scope()
            },
        });
        round_trip(OutputStream::Stdout);
        round_trip(OutputStream::Stderr);
    }

    #[test]
    fn serde_round_trips_every_event_variant() {
        let model = model();
        let result = ToolResult {
            title: "Read file".into(),
            output: "done".into(),
            metadata: serde_json::json!({"path": "x"}),
            truncation: None,
            attachments: vec![ToolAttachment {
                mime_type: "image/png".into(),
                filename: Some("image.png".into()),
                byte_length: 8,
                sha256: digest("tool-attachment"),
                reference: ArtifactReference {
                    uri: format!("artifact://sha256/{}", "00".repeat(32)),
                },
            }],
        };
        let events = vec![
            Event::SessionCreated { meta: meta() },
            Event::RunStarted {
                client_run_id: "client-run".into(),
                input: "hello".into(),
                profile: profile(),
                current_profile: ProfileIdentity {
                    name: "primary".into(),
                    agent_type: AgentType::Primary,
                },
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
                model_call_id: "call_semantic".into(),
                provider_item_id: Some("item_native".into()),
                tool: "read".into(),
                arguments: serde_json::json!({"path": "x"}),
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
                code: ToolCallFailureCode::ExecutionFailed,
                message: "failed".into(),
            },
            Event::ApprovalRequested {
                request: approval_request(),
            },
            Event::ApprovalFinalized {
                approval_id: approval_id(),
                decision: final_approval_decision(),
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
            Event::ModelReplayEvaluated {
                model: model.clone(),
                decisions: vec![ReplayDecision {
                    history_index: 0,
                    disposition: ReplayDisposition::Replayed,
                }],
            },
            Event::ModelTurnCommitted {
                model: model.clone(),
                input_through_seq: 42,
                turn: persisted_turn(),
            },
            Event::ModelFallback {
                from: model.clone(),
                to: model,
                error: model_error(),
                attempts: 2,
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
    fn output_delivery_types_round_trip() {
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
    }

    #[test]
    fn legacy_permission_and_approval_shapes_are_rejected() {
        assert!(
            serde_json::from_value::<PermissionRule>(serde_json::json!({
                "id": "legacy",
                "action": "bash",
                "resource": "git status *",
                "effect": "allow",
                "hard": false
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<MatchedPermissionRule>(serde_json::json!({
                "rule_id": "legacy",
                "source_layer": "profile",
                "effect": "allow",
                "hard": false
            }))
            .is_err()
        );

        assert!(
            serde_json::from_value::<Event>(serde_json::json!({
                "type": "approval_requested",
                "approval_id": "legacy",
                "action": "bash",
                "resource": "git status",
                "suggested_pattern": "git status *",
                "decision_trace": trace()
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<Event>(serde_json::json!({
                "type": "approval_resolved",
                "approval_id": "legacy",
                "decision": "always",
                "approved_scope": "git status *"
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<Event>(serde_json::json!({
                "type": "approval_resolved",
                "approval_id": "legacy",
                "decision": "always"
            }))
            .is_err()
        );
    }

    #[test]
    fn approval_v6_wire_is_exact_and_has_no_scope_editor() {
        round_trip(Event::ApprovalRequested {
            request: approval_request(),
        });
        round_trip(Event::ApprovalFinalized {
            approval_id: approval_id(),
            decision: final_approval_decision(),
        });

        let fingerprint = approval_request().operation_fingerprint().clone();
        let params = ApprovalRespondParams {
            session_id: session_id(),
            approval_id: approval_id(),
            request_revision: 1,
            operation_fingerprint: fingerprint.clone(),
            client_response_id: "response-1".into(),
            decision: ApprovalUserDecision::ApproveTree,
            feedback: None,
        };
        let wire = serde_json::to_value(&params).expect("serialize approval response");
        assert_eq!(
            wire,
            serde_json::json!({
                "session_id": session_id(),
                "approval_id": approval_id(),
                "request_revision": 1,
                "operation_fingerprint": fingerprint,
                "client_response_id": "response-1",
                "decision": "approve_tree"
            })
        );
        round_trip(params);
        assert!(
            serde_json::from_value::<ApprovalRespondParams>(serde_json::json!({
                "session_id": session_id(),
                "approval_id": approval_id(),
                "request_revision": 1,
                "operation_fingerprint": approval_request().operation_fingerprint(),
                "client_response_id": "response-1",
                "decision": "approve_tree",
                "scope": "git status *"
            }))
            .is_err()
        );
        assert_eq!(
            serde_json::to_value(ApprovalRespondError {
                code: ApprovalRespondErrorCode::IdempotencyConflict,
                session_id: session_id(),
                approval_id: approval_id(),
                client_response_id: "response-1".into(),
                expected_revision: None,
                found_revision: None,
            })
            .expect("serialize approval idempotency error"),
            serde_json::json!({
                "code": "idempotency_conflict",
                "session_id": session_id(),
                "approval_id": approval_id(),
                "client_response_id": "response-1"
            })
        );
        assert_eq!(
            serde_json::to_value(ApprovalRespondError {
                code: ApprovalRespondErrorCode::ApprovalRevisionConflict,
                session_id: session_id(),
                approval_id: approval_id(),
                client_response_id: "response-2".into(),
                expected_revision: Some(2),
                found_revision: Some(1),
            })
            .expect("serialize approval revision error"),
            serde_json::json!({
                "code": "approval_revision_conflict",
                "session_id": session_id(),
                "approval_id": approval_id(),
                "client_response_id": "response-2",
                "expected_revision": 2,
                "found_revision": 1
            })
        );
    }

    #[test]
    fn legacy_tool_result_shapes_are_rejected() {
        assert!(
            serde_json::from_value::<ToolResult>(serde_json::json!({
                "content": "legacy",
                "truncated": false
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<Event>(serde_json::json!({
                "type": "tool_call_completed",
                "tool_call_id": call_id(),
                "result": {"content": "legacy", "truncated": false}
            }))
            .is_err()
        );
    }

    #[test]
    fn protocol_v6_rejects_v5_and_legacy_model_event_shapes() {
        assert_eq!(PROTOCOL_VERSION, 6);
        assert_eq!(EVENT_SCHEMA_VERSION, 6);
        assert!(!supports_protocol_version(4));
        assert!(!supports_event_schema_version(4));
        assert!(!supports_protocol_version(5));
        assert!(!supports_event_schema_version(5));
        assert!(supports_protocol_version(6));
        assert!(supports_event_schema_version(6));
        assert!(
            serde_json::from_value::<ClientHello>(serde_json::json!({
                "protocol_version": 5
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<ServerHello>(serde_json::json!({
                "protocol_version": 5
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<Request>(serde_json::json!({
                "jsonrpc": "1.0",
                "id": 1,
                "method": "model.list"
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<EventEnvelope>(serde_json::json!({
                "schema_version": 5,
                "session_id": session_id(),
                "run_id": null,
                "seq": 1,
                "timestamp": Timestamp::now(),
                "event": {"type": "text_delta", "text": "legacy"}
            }))
            .is_err()
        );
        let mut legacy_meta = serde_json::to_value(meta()).expect("serialize session meta");
        legacy_meta
            .as_object_mut()
            .expect("session meta object")
            .remove("title");
        assert!(serde_json::from_value::<SessionMeta>(legacy_meta).is_err());

        assert!(
            serde_json::from_value::<ModelRef>(serde_json::json!({
                "provider": "legacy",
                "model": "legacy-model"
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<Event>(serde_json::json!({
                "type": "turn_opaque",
                "state": {
                    "provider": "open_ai_responses",
                    "payload": {}
                }
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<Event>(serde_json::json!({
                "type": "usage_reported",
                "model": {
                    "provider": "legacy",
                    "model": "legacy-model"
                },
                "usage": {
                    "input_tokens": 1,
                    "output_tokens": 2,
                    "cached_input_tokens": null
                }
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<Event>(serde_json::json!({
                "type": "tool_call_started",
                "tool_call_id": call_id(),
                "model_call_id": "current-model-call",
                "tool": "read",
                "arguments": {},
                "provider_tool_call_id": "legacy-call",
                "provider_protocol": "open_ai_responses"
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<Event>(serde_json::json!({
                "type": "model_fallback",
                "from": model(),
                "to": model(),
                "reason": "legacy string reason",
                "attempts": 1
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<ModelListParams>(serde_json::json!({
                "provider": "legacy-filter"
            }))
            .is_err()
        );
    }

    #[test]
    fn exact_wire_versions_emit_only_protocol_6_event_6_and_jsonrpc_2() {
        assert_eq!(
            serde_json::to_value(ClientHello {
                protocol_version: ProtocolVersion::current(),
            })
            .expect("serialize client hello"),
            serde_json::json!({"protocol_version": 6})
        );
        assert_eq!(
            serde_json::to_value(ServerHello {
                protocol_version: ProtocolVersion::current(),
            })
            .expect("serialize server hello"),
            serde_json::json!({"protocol_version": 6})
        );
        assert_eq!(
            serde_json::to_value(Request::new(
                JsonRpcId::Number(1),
                "model.list",
                Some(serde_json::json!({})),
            ))
            .expect("serialize request")["jsonrpc"],
            "2.0"
        );
        assert_eq!(
            serde_json::to_value(Notification::new("events", None))
                .expect("serialize notification")["jsonrpc"],
            "2.0"
        );
        assert_eq!(
            serde_json::to_value(SuccessResponse {
                jsonrpc: JsonRpcVersion::current(),
                id: JsonRpcId::Number(1),
                result: Value::Null,
            })
            .expect("serialize success response")["jsonrpc"],
            "2.0"
        );
        assert_eq!(
            serde_json::to_value(envelope()).expect("serialize event envelope")["schema_version"],
            6
        );
    }

    #[test]
    fn current_model_turn_and_model_list_shapes_round_trip() {
        round_trip(persisted_turn());
        round_trip(ModelListParams {});
        round_trip(ModelListResult {
            revision: "models-1".into(),
            generated_at: Timestamp::now(),
            catalog_revision: Some("catalog-1".into()),
            models: vec![model()],
        });
    }

    #[test]
    fn native_replay_artifact_enforces_exact_two_mib_payload_boundary() {
        let boundary_payload =
            Value::String("x".repeat(NativeReplayArtifact::MAX_PAYLOAD_BYTES - 2));
        let artifact =
            NativeReplayArtifact::new("test-adapter".into(), replay_scope(), boundary_payload)
                .expect("serialized JSON string at boundary is accepted");
        assert_eq!(
            serde_json::to_vec(artifact.payload())
                .expect("serialize replay payload")
                .len(),
            NativeReplayArtifact::MAX_PAYLOAD_BYTES
        );

        let oversized = NativeReplayArtifact::new(
            "test-adapter".into(),
            replay_scope(),
            Value::String("x".repeat(NativeReplayArtifact::MAX_PAYLOAD_BYTES - 1)),
        );
        assert!(matches!(
            oversized,
            Err(NativeReplayArtifactError::PayloadTooLarge { .. })
        ));

        let wire = serde_json::json!({
            "adapter_id": "test-adapter",
            "scope": replay_scope(),
            "payload": Value::String("x".repeat(NativeReplayArtifact::MAX_PAYLOAD_BYTES - 1))
        });
        assert!(serde_json::from_value::<NativeReplayArtifact>(wire).is_err());
        assert!(
            serde_json::from_value::<NativeReplayArtifact>(serde_json::json!({
                "adapter_id": "test-adapter",
                "scope": replay_scope(),
                "payload": {},
                "legacy_protocol": "open_ai_responses"
            }))
            .is_err()
        );
    }

    #[test]
    fn sha256_and_v6_prepared_fingerprint_domains_are_strict_and_golden() {
        assert_eq!(
            Sha256Digest::of_bytes(b"abc").as_str(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let valid = "ab".repeat(32);
        assert_eq!(
            Sha256Digest::new(valid.clone())
                .expect("valid lowercase digest")
                .as_str(),
            valid
        );
        for invalid in [
            String::new(),
            "ab".repeat(31),
            "ab".repeat(33),
            "AB".repeat(32),
            format!("{}g", "a".repeat(63)),
        ] {
            assert!(Sha256Digest::new(invalid.clone()).is_err());
            assert!(serde_json::from_value::<Sha256Digest>(Value::String(invalid)).is_err());
        }

        let resource_digest =
            PreparedResourceDigest::from_canonical_binding_bytes(b"immutable prepared binding");
        assert_eq!(
            resource_digest.digest().as_str(),
            "a8bd6586abe4d3bc7ec951944d713025d9a79680ff468175b45e3040f17ca311"
        );

        let operation = prepared_operation(PreparedBindingLifetime::ProcessLocal);
        let fingerprint = OperationFingerprint::from_prepared_operation(&operation);
        assert_eq!(
            fingerprint.digest().as_str(),
            "87a4bc54435b2aa6d305ec0f423cb41f261d517b39918bcfe8a485e2508c81c5"
        );
        assert_eq!(
            fingerprint,
            OperationFingerprint::from_prepared_operation(&operation)
        );

        let mut provenance_changed = operation.clone();
        provenance_changed.resources[0].source = ApprovalResourceSource::SecondaryOperation;
        provenance_changed.resources[0].boundary = ApprovalBoundary::Exact;
        assert_eq!(
            fingerprint,
            OperationFingerprint::from_prepared_operation(&provenance_changed),
            "resource source and approval boundary are not execution identity"
        );

        let mut canonical_resource_changed = operation.clone();
        canonical_resource_changed.resources[0].canonical =
            PreparedResourceIdentity::new("command:git-diff")
                .expect("valid prepared resource identity");
        assert_ne!(
            fingerprint,
            OperationFingerprint::from_prepared_operation(&canonical_resource_changed)
        );

        let mut binding_changed = operation.clone();
        binding_changed.resources[0].binding_digest =
            PreparedResourceDigest::from_canonical_binding_bytes(b"replacement binding");
        assert_ne!(
            fingerprint,
            OperationFingerprint::from_prepared_operation(&binding_changed)
        );

        let mut context_changed = operation.clone();
        context_changed.execution_context_digest = digest("different execution context");
        assert_ne!(
            fingerprint,
            OperationFingerprint::from_prepared_operation(&context_changed)
        );

        let operation_wire =
            serde_json::to_value(&operation).expect("serialize prepared operation");
        assert_eq!(operation_wire["capability_lifetime"], "process_local");
        assert!(!operation_wire.to_string().contains("/tmp/"));
        assert_eq!(
            operation.resources()[0].canonical.as_str(),
            "command:git-status"
        );

        assert!(
            serde_json::from_value::<ToolAttachment>(serde_json::json!({
                "mime_type": "image/png",
                "byte_length": 1,
                "sha256": "A".repeat(64),
                "reference": {"uri": "artifact://sha256/invalid"}
            }))
            .is_err()
        );
    }

    #[test]
    fn prepared_operation_identity_rejects_incomplete_or_ambiguous_forms() {
        for unstable in [
            "/tmp/tool",
            "workspace/file",
            "fd:7",
            "tmp:123",
            "temp:write",
        ] {
            assert!(PreparedResourceIdentity::new(unstable).is_err());
            assert!(
                serde_json::from_value::<PreparedResourceIdentity>(Value::String(unstable.into()))
                    .is_err()
            );
        }

        let operation = prepared_operation(PreparedBindingLifetime::ProcessLocal);
        let wire = serde_json::to_value(&operation).expect("serialize prepared operation");

        let mut no_capabilities = wire.clone();
        no_capabilities["capabilities"] = serde_json::json!([]);
        assert!(serde_json::from_value::<PreparedOperationIdentity>(no_capabilities).is_err());

        let mut invalid_operation = wire.clone();
        invalid_operation["capabilities"][0]["operation"] = Value::String(" ".into());
        assert!(serde_json::from_value::<PreparedOperationIdentity>(invalid_operation).is_err());
        let mut raw_path_operation = wire.clone();
        raw_path_operation["capabilities"][0]["operation"] = Value::String("/tmp/tool".into());
        assert!(serde_json::from_value::<PreparedOperationIdentity>(raw_path_operation).is_err());

        let mut unbound_resource = wire.clone();
        unbound_resource["resources"][0]["capability"] = Value::String("read".into());
        assert!(serde_json::from_value::<PreparedOperationIdentity>(unbound_resource).is_err());

        let mut duplicate_binding = wire.clone();
        let duplicate = duplicate_binding["resources"][0].clone();
        duplicate_binding["resources"] = Value::Array(vec![duplicate.clone(), duplicate]);
        assert!(serde_json::from_value::<PreparedOperationIdentity>(duplicate_binding).is_err());

        let mut missing_binding = wire.clone();
        missing_binding["resources"][0]
            .as_object_mut()
            .expect("resource object")
            .remove("binding_digest");
        assert!(serde_json::from_value::<PreparedOperationIdentity>(missing_binding).is_err());

        let mut old_v5_resource = wire;
        let resource = old_v5_resource["resources"][0]
            .as_object_mut()
            .expect("resource object");
        resource.insert("digest".into(), Value::String("00".repeat(32)));
        resource.remove("binding_digest");
        assert!(serde_json::from_value::<PreparedOperationIdentity>(old_v5_resource).is_err());

        let request_wire = serde_json::to_value(approval_request()).expect("serialize approval");
        let mut changed_fingerprint = request_wire.clone();
        changed_fingerprint["operation_fingerprint"]["digest"] = Value::String("00".repeat(32));
        assert!(serde_json::from_value::<ApprovalRequest>(changed_fingerprint).is_err());

        let mut incomplete_evaluations = request_wire.clone();
        incomplete_evaluations["evaluations"] = serde_json::json!([]);
        assert!(serde_json::from_value::<ApprovalRequest>(incomplete_evaluations).is_err());

        let mut invalid_tree_constraint = request_wire;
        invalid_tree_constraint["constraints"]["allow_tree_grant"] = Value::Bool(true);
        assert!(serde_json::from_value::<ApprovalRequest>(invalid_tree_constraint).is_err());
    }

    #[test]
    fn native_context_window_enforces_exact_32_mib_boundary_and_rejects_legacy_scope_fields() {
        let boundary_payload =
            Value::String("x".repeat(NativeContextWindow::MAX_PAYLOAD_BYTES - 2));
        let window =
            NativeContextWindow::new("test-adapter".into(), replay_scope(), boundary_payload)
                .expect("serialized JSON string at boundary is accepted");
        assert_eq!(
            serde_json::to_vec(window.payload())
                .expect("serialize native context payload")
                .len(),
            NativeContextWindow::MAX_PAYLOAD_BYTES
        );
        assert!(matches!(
            NativeContextWindow::new(
                "test-adapter".into(),
                replay_scope(),
                Value::String("x".repeat(NativeContextWindow::MAX_PAYLOAD_BYTES - 1)),
            ),
            Err(NativeReplayArtifactError::PayloadTooLarge { .. })
        ));
        assert!(
            serde_json::from_value::<NativeContextWindow>(serde_json::json!({
                "adapter_id": "test-adapter",
                "native_context_scope": replay_scope(),
                "payload": {}
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<NativeReplayArtifact>(serde_json::json!({
                "adapter_id": "test-adapter",
                "replay_scope": replay_scope(),
                "payload": {}
            }))
            .is_err()
        );
    }

    #[test]
    fn run_profile_override_and_frozen_started_profile_have_exact_wire_shapes() {
        let params = RunStartParams {
            session_id: session_id(),
            client_run_id: "client-run".into(),
            input: "draft with reviewer".into(),
            profile: Some("reviewer".into()),
        };
        assert_eq!(
            serde_json::to_value(&params).expect("serialize run start"),
            serde_json::json!({
                "session_id": session_id(),
                "client_run_id": "client-run",
                "input": "draft with reviewer",
                "profile": "reviewer"
            })
        );
        round_trip(params);

        assert_eq!(
            serde_json::to_value(RunStartConflict {
                code: RunStartConflictCode::IdempotencyConflict,
                session_id: session_id(),
                client_run_id: "client-run".into(),
            })
            .expect("serialize run idempotency conflict"),
            serde_json::json!({
                "code": "idempotency_conflict",
                "session_id": session_id(),
                "client_run_id": "client-run"
            })
        );

        let started = Event::RunStarted {
            client_run_id: "client-run".into(),
            input: "draft with reviewer".into(),
            profile: profile(),
            current_profile: ProfileIdentity {
                name: "reviewer".into(),
                agent_type: AgentType::Primary,
            },
        };
        let wire = serde_json::to_value(&started).expect("serialize run started");
        assert_eq!(wire["type"], "run_started");
        assert_eq!(wire["current_profile"]["name"], "reviewer");
        assert_eq!(wire["profile"]["models"][0]["model_id"], "test-model");
        round_trip(started);
    }

    #[test]
    fn session_title_commits_rebuild_user_rename_idempotency() {
        let user_commits = [
            SessionTitleCommit::UserSet {
                title: title("User title"),
                client_rename_id: rename_id("rename-set"),
            },
            SessionTitleCommit::UserClear {
                client_rename_id: rename_id("rename-clear"),
            },
            SessionTitleCommit::UserReset {
                client_rename_id: rename_id("rename-reset"),
            },
        ];
        let mut rebuilt = BTreeMap::new();
        for commit in user_commits {
            let event = Event::SessionTitleCommitted {
                input_through_seq: 12,
                commit: commit.clone(),
            };
            let wire = serde_json::to_value(&event).expect("serialize user title commit");
            assert!(wire["commit"].get("client_rename_id").is_some());
            round_trip(event);
            let record = commit
                .user_rename_record()
                .expect("user commit reconstructs idempotency");
            rebuilt.insert(record.client_rename_id.clone(), record);
        }
        assert_eq!(rebuilt.len(), 3);

        let replay = SessionRenameParams {
            session_id: session_id(),
            client_rename_id: rename_id("rename-set"),
            change: SessionRenameChange::Set {
                title: title("User title"),
            },
        };
        let conflicting = SessionRenameParams {
            change: SessionRenameChange::Set {
                title: title("Different title"),
            },
            ..replay.clone()
        };
        let record = rebuilt
            .get(&rename_id("rename-set"))
            .expect("rebuilt set record");
        assert!(record.matches(&replay));
        assert!(!record.conflicts_with(&replay));
        assert!(record.conflicts_with(&conflicting));
        assert!(record.conflicts_with(&SessionRenameParams {
            change: SessionRenameChange::Clear,
            ..replay.clone()
        }));
        assert!(record.conflicts_with(&SessionRenameParams {
            change: SessionRenameChange::Reset,
            ..replay.clone()
        }));

        for (change, expected_type) in [
            (
                SessionRenameChange::Set {
                    title: title("User title"),
                },
                "set",
            ),
            (SessionRenameChange::Clear, "clear"),
            (SessionRenameChange::Reset, "reset"),
        ] {
            let params = SessionRenameParams {
                session_id: session_id(),
                client_rename_id: rename_id(&format!("rename-{expected_type}")),
                change,
            };
            let wire = serde_json::to_value(&params).expect("serialize session rename");
            assert_eq!(wire["change"]["type"], expected_type);
            round_trip(params);
        }

        for commit in [
            SessionTitleCommit::InternalAgentSet {
                title: title("Generated title"),
                invocation_id: InternalAgentInvocationId(Uuid::from_u128(9)),
            },
            SessionTitleCommit::FallbackSet {
                title: title("Fallback title"),
            },
        ] {
            assert!(commit.user_rename_record().is_none());
            let event = Event::SessionTitleCommitted {
                input_through_seq: 12,
                commit,
            };
            let wire = serde_json::to_value(&event).expect("serialize generated title commit");
            assert!(wire["commit"].get("client_rename_id").is_none());
            round_trip(event);
        }
    }

    #[test]
    fn session_title_commit_rejects_every_contradictory_cross_product() {
        for invalid_title in [
            String::new(),
            "   ".into(),
            "bad\ntitle".into(),
            "x".repeat(SessionTitle::MAX_BYTES + 1),
        ] {
            assert!(SessionTitle::new(invalid_title.clone()).is_err());
            assert!(serde_json::from_value::<SessionTitle>(Value::String(invalid_title)).is_err());
        }
        for invalid_id in [
            String::new(),
            "bad\nid".into(),
            "x".repeat(ClientRenameId::MAX_BYTES + 1),
        ] {
            assert!(ClientRenameId::new(invalid_id.clone()).is_err());
            assert!(serde_json::from_value::<ClientRenameId>(Value::String(invalid_id)).is_err());
        }

        let invocation_id = InternalAgentInvocationId(Uuid::from_u128(9));
        let invalid_commits = [
            serde_json::json!({"type": "user_set", "client_rename_id": "rename"}),
            serde_json::json!({"type": "user_set", "title": "", "client_rename_id": "rename"}),
            serde_json::json!({"type": "user_set", "title": "Title"}),
            serde_json::json!({"type": "user_set", "title": "Title", "client_rename_id": "rename", "invocation_id": invocation_id}),
            serde_json::json!({"type": "user_clear"}),
            serde_json::json!({"type": "user_clear", "client_rename_id": "rename", "title": "Title"}),
            serde_json::json!({"type": "user_clear", "client_rename_id": "rename", "invocation_id": invocation_id}),
            serde_json::json!({"type": "user_reset"}),
            serde_json::json!({"type": "user_reset", "client_rename_id": "rename", "title": "Title"}),
            serde_json::json!({"type": "user_reset", "client_rename_id": "rename", "invocation_id": invocation_id}),
            serde_json::json!({"type": "internal_agent_set", "invocation_id": invocation_id}),
            serde_json::json!({"type": "internal_agent_set", "title": "Title"}),
            serde_json::json!({"type": "internal_agent_set", "title": "Title", "invocation_id": invocation_id, "client_rename_id": "rename"}),
            serde_json::json!({"type": "internal_agent_clear", "invocation_id": invocation_id}),
            serde_json::json!({"type": "internal_agent_reset", "invocation_id": invocation_id}),
            serde_json::json!({"type": "fallback_set"}),
            serde_json::json!({"type": "fallback_set", "title": "Title", "client_rename_id": "rename"}),
            serde_json::json!({"type": "fallback_set", "title": "Title", "invocation_id": invocation_id}),
            serde_json::json!({"type": "fallback_clear"}),
            serde_json::json!({"type": "fallback_reset"}),
        ];
        for commit in invalid_commits {
            assert!(serde_json::from_value::<SessionTitleCommit>(commit.clone()).is_err());
            assert!(
                serde_json::from_value::<Event>(serde_json::json!({
                    "type": "session_title_committed",
                    "input_through_seq": 12,
                    "commit": commit
                }))
                .is_err()
            );
        }
        assert!(
            serde_json::from_value::<Event>(serde_json::json!({
                "type": "session_title_committed",
                "title": "legacy",
                "operation": "set",
                "source": "user",
                "input_through_seq": 12,
                "client_rename_id": "rename"
            }))
            .is_err()
        );
    }

    #[test]
    fn internal_agent_title_and_checkpoint_events_round_trip() {
        let invocation_id = InternalAgentInvocationId(Uuid::from_u128(9));
        let internal_run_id = InternalAgentRunId(Uuid::from_u128(10));
        let backend = InternalAgentBackend::Model {
            profile: Box::new(profile()),
            model: model(),
        };
        let failure = InternalAgentFailure {
            code: "model_unavailable".into(),
            message: "model unavailable".into(),
            retryable: true,
            model_error: Some(model_error()),
        };
        let summary_limit = SummaryByteLimit::new(65_536).expect("valid summary limit");
        let summary_checkpoint = InternalSummaryCheckpoint::new(
            "bounded summary".into(),
            invocation_id,
            internal_run_id,
            summary_limit,
        )
        .expect("valid internal summary");
        let checkpoint_commit = ContextCheckpointCommit::new(
            ContextCheckpoint::InternalSummary {
                checkpoint: summary_checkpoint,
            },
            ContextCheckpointBoundaries {
                source_from_seq: 1,
                source_through_seq: 40,
                input_through_seq: 40,
                prior_checkpoint_seq: None,
            },
            ContextCheckpointBudgets {
                context_limit_tokens: 200_000,
                trigger_tokens: 180_000,
                target_tokens: 100_000,
                input_tokens_before: 181_000,
                input_tokens_after: 99_000,
                max_summary_bytes: summary_limit,
            },
        )
        .expect("valid checkpoint commit");
        let events = [
            Event::InternalAgentStarted {
                invocation_id,
                internal_run_id,
                kind: InternalAgentKind::ContextCompaction,
                backend: backend.clone(),
                call: SafeInternalAgentCall {
                    name: "compact_context".into(),
                    input_summary: "history through seq 40".into(),
                    input_digest: digest("input"),
                },
            },
            Event::InternalAgentCompleted {
                invocation_id,
                internal_run_id,
                kind: InternalAgentKind::ContextCompaction,
                result: SafeInternalAgentResult {
                    output_summary: "checkpoint committed".into(),
                    output_digest: digest("output"),
                },
            },
            Event::InternalAgentFailed {
                invocation_id,
                internal_run_id,
                kind: InternalAgentKind::ContextCompaction,
                failure: failure.clone(),
            },
            Event::InternalAgentCancelled {
                invocation_id,
                internal_run_id,
                kind: InternalAgentKind::ContextCompaction,
                reason: Some("run cancelled".into()),
            },
            Event::InternalAgentInterrupted {
                invocation_id,
                internal_run_id,
                kind: InternalAgentKind::ContextCompaction,
                reason: Some("daemon restart".into()),
            },
            Event::InternalAgentFallback {
                invocation_id,
                internal_run_id,
                kind: InternalAgentKind::ContextCompaction,
                from: backend,
                to: InternalAgentBackend::Builtin {
                    name: "summary-fallback".into(),
                    revision: "1".into(),
                },
                failure,
                attempts: 1,
            },
            Event::ContextCheckpointCommitted {
                commit: checkpoint_commit,
            },
            Event::SessionTitleCommitted {
                input_through_seq: 12,
                commit: SessionTitleCommit::InternalAgentSet {
                    title: title("Implement protocol v6"),
                    invocation_id,
                },
            },
        ];
        for event in events {
            round_trip(event);
        }

        round_trip(ContextCheckpoint::ProviderNative {
            model: model(),
            native_context: NativeContextArtifact {
                adapter_id: "test-adapter".into(),
                scope: replay_scope(),
                byte_length: 128,
                sha256: digest("native-context"),
                reference: ArtifactReference {
                    uri: format!("artifact://sha256/{}", digest("native-context")),
                },
            },
        });
    }

    #[test]
    fn internal_summary_rejects_tampering_oversize_and_weakened_limits() {
        let invocation_id = InternalAgentInvocationId(Uuid::from_u128(9));
        let internal_run_id = InternalAgentRunId(Uuid::from_u128(10));
        let limit = SummaryByteLimit::new(64).expect("valid summary limit");
        let summary =
            InternalSummaryCheckpoint::new("summary".into(), invocation_id, internal_run_id, limit)
                .expect("valid summary");
        let wire = serde_json::to_value(&summary).expect("serialize summary");

        let mut wrong_length = wire.clone();
        wrong_length["byte_length"] = Value::from(8);
        assert!(serde_json::from_value::<InternalSummaryCheckpoint>(wrong_length).is_err());

        let mut wrong_digest = wire.clone();
        wrong_digest["sha256"] = Value::from("00".repeat(32));
        assert!(serde_json::from_value::<InternalSummaryCheckpoint>(wrong_digest).is_err());

        let oversized = "x".repeat(SummaryByteLimit::GLOBAL_MAX_BYTES as usize + 1);
        assert!(
            InternalSummaryCheckpoint::new(
                oversized,
                invocation_id,
                internal_run_id,
                SummaryByteLimit::new(SummaryByteLimit::GLOBAL_MAX_BYTES)
                    .expect("global maximum is valid"),
            )
            .is_err()
        );
        assert!(SummaryByteLimit::new(SummaryByteLimit::GLOBAL_MAX_BYTES + 1).is_err());
        assert!(
            serde_json::from_value::<SummaryByteLimit>(Value::from(
                SummaryByteLimit::GLOBAL_MAX_BYTES + 1
            ))
            .is_err()
        );

        let checkpoint = ContextCheckpoint::InternalSummary {
            checkpoint: summary,
        };
        let commit = serde_json::json!({
            "checkpoint": checkpoint,
            "boundaries": {
                "source_from_seq": 1,
                "source_through_seq": 2,
                "input_through_seq": 2
            },
            "budgets": {
                "context_limit_tokens": 100,
                "trigger_tokens": 90,
                "target_tokens": 50,
                "input_tokens_before": 91,
                "input_tokens_after": 49,
                "max_summary_bytes": 6
            }
        });
        assert!(serde_json::from_value::<ContextCheckpointCommit>(commit).is_err());
    }

    #[test]
    fn approval_lifecycle_and_tree_grant_shapes_round_trip() {
        let internal = ApprovalInternalDecision {
            decision: ApprovalInternalDecisionKind::Ask,
            source: ApprovalDecisionSource::Policy,
            reason_code: ApprovalReasonCode::PolicyRequiresApproval,
            evaluations: approval_request().evaluations().to_vec(),
        };
        let grant = TreeApprovalGrant::new(
            TreeApprovalGrantId(Uuid::from_u128(11)),
            session_id(),
            approval_id(),
            approval_request().operation_fingerprint().clone(),
            prepared_operation(PreparedBindingLifetime::RestartStable).capabilities,
            prepared_operation(PreparedBindingLifetime::RestartStable).resources,
            Timestamp::now(),
        )
        .expect("restart-stable resources may back a tree grant");
        assert!(
            TreeApprovalGrant::new(
                TreeApprovalGrantId(Uuid::from_u128(12)),
                session_id(),
                approval_id(),
                approval_request().operation_fingerprint().clone(),
                prepared_operation(PreparedBindingLifetime::ProcessLocal).capabilities,
                prepared_operation(PreparedBindingLifetime::ProcessLocal).resources,
                Timestamp::now(),
            )
            .is_err()
        );
        let mut filesystem_resource = prepared_resource(PreparedBindingLifetime::RestartStable);
        filesystem_resource.capability = ActionKind::Read;
        assert!(
            TreeApprovalGrant::new(
                TreeApprovalGrantId(Uuid::from_u128(13)),
                session_id(),
                approval_id(),
                approval_request().operation_fingerprint().clone(),
                vec![ApprovalCapability {
                    action: ActionKind::Read,
                    operation: PreparedCapabilityOperation::new("read").expect("valid operation"),
                }],
                vec![filesystem_resource],
                Timestamp::now(),
            )
            .is_err()
        );
        for event in [
            Event::ApprovalEvaluated {
                approval_id: approval_id(),
                decision: internal,
            },
            Event::ApprovalEscalated {
                approval_id: approval_id(),
                reason_code: ApprovalReasonCode::Escalated,
            },
            Event::ApprovalUserDecisionRecorded {
                approval_id: approval_id(),
                client_response_id: "response-1".into(),
                decision: ApprovalUserDecision::ApproveTree,
                feedback: None,
            },
            Event::ApprovalCancelled {
                approval_id: approval_id(),
                reason_code: ApprovalReasonCode::RequestCancelled,
            },
            Event::ApprovalDoomLoopDetected {
                approval_id: approval_id(),
                operation_fingerprint: approval_request().operation_fingerprint().clone(),
                repetitions: 3,
            },
            Event::TreeApprovalGrantCommitted {
                grant: grant.clone(),
            },
        ] {
            round_trip(event);
        }
        round_trip(ApprovalListResult {
            approvals: Vec::new(),
            tree_grants: vec![grant],
        });
    }

    #[test]
    fn provider_connect_serializes_secret_only_in_request_transport_and_redacts_safe_surfaces() {
        let secret = "credential-sentinel-must-never-appear-in-debug";
        let params = ProviderConnectParams {
            client_connect_id: "connect-1".into(),
            provider_id: "anthropic".into(),
            catalog_revision: "catalog-1".into(),
            credentials: ProviderCredentials {
                values: BTreeMap::from([("ANTHROPIC_API_KEY".into(), secret.into())]),
            },
        };
        assert!(!format!("{params:?}").contains(secret));
        assert!(
            serde_json::to_string(&params)
                .expect("serialize secret-bearing request")
                .contains(secret)
        );

        let secret_value = serde_json::to_value(&params).expect("serialize connect params value");
        let request = Request::new(
            JsonRpcId::String("connect-request".into()),
            "provider.connect",
            Some(secret_value.clone()),
        );
        let request_debug = format!("{request:?}");
        assert!(!request_debug.contains(secret));
        assert!(request_debug.contains("provider.connect"));
        assert!(request_debug.contains("<redacted>"));
        assert!(
            serde_json::to_string(&request)
                .expect("serialize inbound connect request envelope")
                .contains(secret)
        );

        let notification = Notification::new("future.raw", Some(secret_value.clone()));
        assert!(!format!("{notification:?}").contains(secret));
        let success = SuccessResponse {
            jsonrpc: JsonRpcVersion::current(),
            id: JsonRpcId::String("connect-request".into()),
            result: secret_value.clone(),
        };
        assert!(!format!("{success:?}").contains(secret));
        assert!(!format!("{:?}", Response::Success(success)).contains(secret));
        let rpc_error = JsonRpcError {
            code: -32_602,
            message: "invalid params".into(),
            data: Some(secret_value),
        };
        assert!(!format!("{rpc_error:?}").contains(secret));
        assert!(
            !format!(
                "{:?}",
                Response::Error(ErrorResponse {
                    jsonrpc: JsonRpcVersion::current(),
                    id: JsonRpcId::String("connect-request".into()),
                    error: rpc_error,
                })
            )
            .contains(secret)
        );

        let result = ProviderConnectResult {
            client_connect_id: "connect-1".into(),
            connection: ProviderConnection {
                provider_id: "anthropic".into(),
                credential_fields: vec!["ANTHROPIC_API_KEY".into()],
                connected_at: Timestamp::now(),
                catalog_revision: "catalog-1".into(),
            },
            model_revision: "models-2".into(),
        };
        let result_json = serde_json::to_string(&result).expect("serialize safe connect result");
        assert!(!result_json.contains(secret));
        assert!(!result_json.contains("credentials"));
        let error_json = serde_json::to_string(&ProviderConnectError {
            code: ProviderConnectErrorCode::InvalidCredential,
            provider_id: "anthropic".into(),
            client_connect_id: "connect-1".into(),
        })
        .expect("serialize safe connect error");
        assert!(!error_json.contains(secret));
        assert!(!error_json.contains("credentials"));
        let event_schema = serde_json::to_string(&schema_for!(Event)).expect("event schema");
        assert!(!event_schema.contains("credentials"));
        let result_ts = ProviderConnectResult::export_to_string(&typescript_config())
            .expect("export safe connect result TypeScript");
        assert!(!result_ts.contains("credentials"));
        round_trip(result);
    }

    #[test]
    fn catalog_and_model_snapshot_shapes_round_trip() {
        let snapshot = CatalogSnapshot {
            revision: "catalog-1".into(),
            source: "https://models.dev/api.json".into(),
            fetched_at: Timestamp::now(),
        };
        round_trip(CatalogProviderListResult {
            snapshot: snapshot.clone(),
            providers: vec![CatalogProvider {
                id: "anthropic".into(),
                name: "Anthropic".into(),
                credential_fields: vec!["ANTHROPIC_API_KEY".into()],
                npm: Some("@ai-sdk/anthropic".into()),
                api: None,
                documentation_url: Some("https://docs.anthropic.com".into()),
            }],
        });
        round_trip(CatalogModelListResult {
            snapshot,
            models: vec![CatalogModel {
                provider_id: "anthropic".into(),
                model_id: "claude-sonnet".into(),
                canonical_model_id: Some("anthropic/claude-sonnet".into()),
                name: "Claude Sonnet".into(),
                family: Some("claude".into()),
                capabilities: CatalogModelCapabilities {
                    attachment: true,
                    reasoning: true,
                    tool_call: true,
                    structured_output: false,
                    temperature: true,
                },
                limits: CatalogModelLimits {
                    context: 200_000,
                    input: None,
                    output: 64_000,
                },
                modalities: CatalogModelModalities {
                    input: vec!["text".into(), "image".into()],
                    output: vec!["text".into()],
                },
                status: CatalogModelStatus::Stable,
                release_date: "2026-01-01".into(),
                last_updated: "2026-07-01".into(),
            }],
        });
    }

    #[test]
    fn persisted_model_files_accept_references_only() {
        round_trip(persisted_file());
        for legacy_source in [
            serde_json::json!({"type": "bytes", "data": "aGVsbG8="}),
            serde_json::json!({"type": "text", "text": "inline"}),
            serde_json::json!({"type": "base64", "data": "aGVsbG8="}),
        ] {
            assert!(
                serde_json::from_value::<PersistedFileSource>(legacy_source).is_err(),
                "inline attachment source must be rejected"
            );
        }
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
    fn persisted_model_turn_schema_snapshot() {
        assert_json_snapshot!(schema_for!(PersistedModelTurn));
    }

    #[test]
    fn model_list_result_schema_snapshot() {
        assert_json_snapshot!(schema_for!(ModelListResult));
    }

    #[test]
    fn approval_respond_schema_snapshot() {
        assert_json_snapshot!(schema_for!(ApprovalRespondParams));
    }

    #[test]
    fn approval_request_schema_snapshot() {
        assert_json_snapshot!(schema_for!(ApprovalRequest));
    }

    #[test]
    fn catalog_provider_list_schema_snapshot() {
        assert_json_snapshot!(schema_for!(CatalogProviderListResult));
    }

    #[test]
    fn catalog_model_list_schema_snapshot() {
        assert_json_snapshot!(schema_for!(CatalogModelListResult));
    }

    #[test]
    fn provider_connect_result_schema_snapshot() {
        assert_json_snapshot!(schema_for!(ProviderConnectResult));
    }

    #[test]
    fn provider_connect_params_schema_snapshot() {
        let schema = schema_for!(ProviderConnectParams);
        let schema_json = serde_json::to_value(&schema).expect("serialize connect request schema");
        assert_eq!(
            schema_json["$defs"]["ProviderCredentials"]["properties"]["values"]["writeOnly"],
            true
        );
        assert!(schema_json.get("examples").is_none());
        assert_json_snapshot!(schema);
    }

    #[test]
    fn session_rename_schema_snapshot() {
        assert_json_snapshot!(schema_for!(SessionRenameParams));
    }

    #[test]
    fn session_title_commit_schema_snapshot() {
        assert_json_snapshot!(schema_for!(SessionTitleCommit));
    }

    #[test]
    fn native_context_window_schema_snapshot() {
        assert_json_snapshot!(schema_for!(NativeContextWindow));
    }

    #[test]
    fn persisted_model_turn_typescript_snapshot() {
        assert_snapshot!(
            PersistedModelTurn::export_to_string(&typescript_config())
                .expect("export persisted model turn TypeScript")
        );
    }

    #[test]
    fn model_list_result_typescript_snapshot() {
        assert_snapshot!(
            ModelListResult::export_to_string(&typescript_config())
                .expect("export model list TypeScript")
        );
    }

    #[test]
    fn approval_respond_typescript_snapshot() {
        assert_snapshot!(
            ApprovalRespondParams::export_to_string(&typescript_config())
                .expect("export approval response TypeScript")
        );
    }

    #[test]
    fn approval_request_typescript_snapshot() {
        assert_snapshot!(
            ApprovalRequest::export_to_string(&typescript_config())
                .expect("export approval request TypeScript")
        );
    }

    #[test]
    fn prepared_operation_identity_typescript_snapshot() {
        assert_snapshot!(
            PreparedOperationIdentity::export_to_string(&typescript_config())
                .expect("export prepared operation identity TypeScript")
        );
    }

    #[test]
    fn prepared_approval_resource_typescript_snapshot() {
        assert_snapshot!(
            PreparedApprovalResource::export_to_string(&typescript_config())
                .expect("export prepared approval resource TypeScript")
        );
    }

    #[test]
    fn catalog_provider_list_typescript_snapshot() {
        assert_snapshot!(
            CatalogProviderListResult::export_to_string(&typescript_config())
                .expect("export catalog provider list TypeScript")
        );
    }

    #[test]
    fn catalog_model_list_typescript_snapshot() {
        assert_snapshot!(
            CatalogModelListResult::export_to_string(&typescript_config())
                .expect("export catalog model list TypeScript")
        );
    }

    #[test]
    fn provider_connect_result_typescript_snapshot() {
        assert_snapshot!(
            ProviderConnectResult::export_to_string(&typescript_config())
                .expect("export provider connect result TypeScript")
        );
    }

    #[test]
    fn session_rename_typescript_snapshot() {
        assert_snapshot!(
            SessionRenameParams::export_to_string(&typescript_config())
                .expect("export session rename TypeScript")
        );
    }

    #[test]
    fn session_title_commit_typescript_snapshot() {
        assert_snapshot!(
            SessionTitleCommit::export_to_string(&typescript_config())
                .expect("export session title commit TypeScript")
        );
    }

    #[test]
    fn typescript_wire_compile_fixtures_use_optional_properties_and_json_numbers() {
        let config = typescript_config();
        let approval = ApprovalRespondParams::decl(&config);
        assert!(approval.contains("request_revision: number"));
        assert!(approval.contains("operation_fingerprint: OperationFingerprint"));
        assert!(approval.contains("feedback?: ApprovalFeedback | null"));
        assert!(!approval.contains("bigint"));

        let models = ModelListResult::decl(&config);
        assert!(models.contains("catalog_revision?: string | null"));
        assert!(!models.contains("bigint"));
        let model_wire = serde_json::to_value(ModelListResult {
            revision: "models-1".into(),
            generated_at: Timestamp::now(),
            catalog_revision: None,
            models: vec![model()],
        })
        .expect("serialize model list fixture");
        assert!(model_wire.get("catalog_revision").is_none());

        let envelope = EventEnvelope::inline(&config);
        assert!(!envelope.contains("bigint"));
        assert!(envelope.contains("run_id?: RunId | null"));
        let event = Event::inline(&config);
        assert!(!event.contains("bigint"));
        assert!(event.contains("final_text?: string | null"), "{event}");
        assert!(
            event.contains("provider_item_id?: string | null"),
            "{event}"
        );
    }

    #[test]
    fn typescript_export_smoke_test() {
        let config = typescript_config();
        let declaration = EventEnvelope::export_to_string(&config).expect("export TypeScript");
        assert!(declaration.contains("EventEnvelope"));
        assert!(
            PersistedModelTurn::export_to_string(&config)
                .expect("export persisted model turn TypeScript")
                .contains("PersistedModelTurn")
        );
        assert!(
            ModelListResult::export_to_string(&config)
                .expect("export model list TypeScript")
                .contains("ModelListResult")
        );
        for declaration in [
            SessionId::inline(&config),
            RunId::inline(&config),
            ToolCallId::inline(&config),
            InvocationId::inline(&config),
            InternalAgentInvocationId::inline(&config),
            InternalAgentRunId::inline(&config),
            ApprovalId::inline(&config),
            TreeApprovalGrantId::inline(&config),
        ] {
            assert_eq!(declaration, "string");
        }
        assert_eq!(ProtocolVersion::inline(&config), "6");
        assert_eq!(EventSchemaVersion::inline(&config), "6");
        assert_eq!(JsonRpcVersion::inline(&config), "\"2.0\"");
        assert_eq!(Sha256Digest::inline(&config), "string");
    }
}
