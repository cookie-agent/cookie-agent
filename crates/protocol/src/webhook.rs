//! Webhooks (`docs/site/specs/webhooks.md`): URLs the daemon serves whose
//! requests become messages in an existing session, or start new sessions.
//! Client protocol 1.96.0.
//!
//! A webhook's target is closed by design, as a cron job's is: a new kind
//! of target is a new kind of webhook, which an older client could neither
//! show nor edit. What an event did and why a webhook is disabled are open.

use std::fmt;

use jiff::Timestamp;
use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};

use crate::{
    AgentId, CronActor, CronRunEnd, CronTargetKind, CronThread, ModelSelection, Open,
    PermissionMode, PermissionRule, ProducerDeliveryMode, ProjectId, ProjectRuntime, RunId,
    SessionId, WebhookId,
    agent::{AgentPresetNameSchema, deserialize_optional_preset},
};

pub const WEBHOOKS_LIST_METHOD: &str = "webhooks.list";
pub const WEBHOOKS_CREATE_METHOD: &str = "webhooks.create";
pub const WEBHOOKS_UPDATE_METHOD: &str = "webhooks.update";
pub const WEBHOOKS_DELETE_METHOD: &str = "webhooks.delete";
pub const WEBHOOKS_ROTATE_METHOD: &str = "webhooks.rotate";
pub const WEBHOOKS_SECRET_METHOD: &str = "webhooks.secret";
pub const WEBHOOKS_EVENT_METHOD: &str = "webhooks.event";
pub const WEBHOOKS_TEST_METHOD: &str = "webhooks.test";
pub const WEBHOOKS_CHANGED_METHOD: &str = "webhooks.changed";

/// The most characters a webhook's name may have.
pub const WEBHOOK_NAME_MAX_CHARS: usize = 80;
/// The most bytes a webhook's prompt may have.
pub const WEBHOOK_PROMPT_MAX_BYTES: usize = 32 * 1024;
/// The most extra headers a webhook shows the model.
pub const WEBHOOK_HEADERS_MAX: usize = 32;
/// The path every webhook URL starts with.
pub const WEBHOOK_PATH_PREFIX: &str = "/hooks/";

const fn attach_global() -> bool {
    true
}

#[allow(clippy::trivially_copy_pass_by_ref)]
const fn attaches_global(global: &bool) -> bool {
    *global
}

const fn receiving_default() -> bool {
    true
}

/// Reads a member that is present as `Some`, so that `Option<Option<T>>`
/// tells an absent member (`None`) from `null` (`Some(None)`).
fn deserialize_present<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(deserializer).map(Some)
}

/// Where a filter condition or a thread key reads its value: exactly one of
/// a request header, by name, or a JSON Pointer (RFC 6901) into a JSON or
/// form body.
#[derive(Clone, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct WebhookValueSource {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1, max = 128))]
    pub header: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = 512))]
    pub json: Option<String>,
}

impl WebhookValueSource {
    /// A source reading the header `name`.
    #[must_use]
    pub fn header(name: impl Into<String>) -> Self {
        Self {
            header: Some(name.into()),
            json: None,
        }
    }

    /// A source reading the JSON Pointer `pointer`.
    #[must_use]
    pub fn json(pointer: impl Into<String>) -> Self {
        Self {
            header: None,
            json: Some(pointer.into()),
        }
    }
}

/// One condition of a webhook's filter: the value its source reads must be
/// one of `one_of`.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct WebhookCondition {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1, max = 128))]
    pub header: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = 512))]
    pub json: Option<String>,
    #[schemars(length(min = 1, max = 64))]
    pub one_of: Vec<String>,
}

/// Where each event goes.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
#[schemars(transform = crate::event::open_object_schema)]
#[allow(clippy::large_enum_variant)]
pub enum WebhookTarget {
    /// A root session.
    Session(SessionId),
    /// A delegated session.
    Subagent(SessionId),
    /// A new root session per event, or per thread key, from this
    /// template.
    New(WebhookTemplate),
}

impl WebhookTarget {
    /// The target kind, which is the resource of the `webhook` permission.
    #[must_use]
    pub const fn kind(&self) -> CronTargetKind {
        match self {
            Self::Session(_) => CronTargetKind::Session,
            Self::Subagent(_) => CronTargetKind::Subagent,
            Self::New(_) => CronTargetKind::New,
        }
    }

    /// The session an existing-session webhook delivers into.
    #[must_use]
    pub const fn session(&self) -> Option<SessionId> {
        match self {
            Self::Session(session) | Self::Subagent(session) => Some(*session),
            Self::New(_) => None,
        }
    }
}

/// What a new-session webhook's sessions start with: a cron template's
/// members, with a thread key read from each request.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct WebhookTemplate {
    /// The agent each session starts with; it must be able to run as a
    /// root (`primary` or `all`).
    pub agent: AgentId,
    #[serde(
        default,
        deserialize_with = "deserialize_optional_preset",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "Option<AgentPresetNameSchema>")]
    pub preset: Option<String>,
    /// A model pin; the agent's own model chain when omitted. Only clients
    /// set it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelSelection>,
    /// Rules added to each session as its permission overlay.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 256))]
    pub permission_rules: Vec<PermissionRule>,
    /// The runtimes a session attaches besides `global`, as
    /// `session.create`'s `runtimes` names them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = 256))]
    pub runtimes: Option<Vec<ProjectRuntime>>,
    /// Whether a session attaches the global runtime; `true` when omitted.
    #[serde(default = "attach_global", skip_serializing_if = "attaches_global")]
    pub global: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1, max = 4096))]
    pub cwd: Option<String>,
    /// The project the sessions belong to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<ProjectId>,
    /// For an ACP agent, the runtime its runs run it on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acp_runtime: Option<crate::RuntimeId>,
    /// Where each request's thread key is read (section 7.4): events with
    /// the same key go into one session, which the first of them creates.
    /// Each event creates a session when omitted, and so does an event
    /// whose key is missing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_key: Option<WebhookValueSource>,
    /// Instructions each session the webhook creates gets once, as a plain
    /// user message before its first event, and again before the first
    /// event after each compaction of that session (section 7.5).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1, max = 32768))]
    pub thread_prompt: Option<String>,
    /// What the sessions it creates are called (client protocol 1.97.0;
    /// `docs/site/specs/webhooks-view.md` section 6): plain text, given when
    /// the webhook is made, in place of its name in their titles, which the
    /// thread key or the time still follows. Its name when omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1, max = 384))]
    pub title: Option<String>,
    /// Whether each session it creates is left untitled for the automatic
    /// title to name after its first turn, as a session the user starts is
    /// (client protocol 1.97.0); `title` then goes unused. Off when
    /// `[session_title] generate_on_first_turn` is.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub title_auto: bool,
}

impl WebhookTemplate {
    /// A template that runs `agent` with every other member at its default.
    #[must_use]
    pub fn for_agent(agent: AgentId) -> Self {
        Self {
            agent,
            preset: None,
            model: None,
            permission_rules: Vec::new(),
            runtimes: None,
            global: true,
            cwd: None,
            project: None,
            acp_runtime: None,
            thread_key: None,
            thread_prompt: None,
            title: None,
            title_auto: false,
        }
    }
}

/// A webhook's signature check (section 6).
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WebhookVerifyKind {
    /// `X-Hub-Signature-256: sha256=<hex HMAC-SHA256 of the body>`.
    Github,
    /// Standard Webhooks: `webhook-id`, `webhook-timestamp`, and
    /// `webhook-signature`, with a `whsec_` secret.
    Standard,
    /// `<header>: <prefix><HMAC-SHA256 of the body>` in `encoding`.
    HmacSha256,
    /// A bearer token in the `Authorization` header.
    Bearer,
}

impl WebhookVerifyKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Github => "github",
            Self::Standard => "standard",
            Self::HmacSha256 => "hmac_sha256",
            Self::Bearer => "bearer",
        }
    }
}

impl fmt::Display for WebhookVerifyKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// How a generic HMAC signature is written.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WebhookSignatureEncoding {
    Hex,
    Base64,
}

/// A webhook's check as commands set it and reads show it. `secret` is
/// the secret, or the bearer token, on a create or an update; the daemon
/// makes one when it is omitted and never returns it here (see
/// `webhooks.secret`). `header`, `encoding`, and `prefix` belong to
/// `hmac_sha256` only.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct WebhookVerify {
    pub kind: WebhookVerifyKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1, max = 1024))]
    pub secret: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1, max = 128))]
    pub header: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encoding: Option<WebhookSignatureEncoding>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = 64))]
    pub prefix: Option<String>,
}

impl WebhookVerify {
    /// A check of `kind` whose secret the daemon makes.
    #[must_use]
    pub const fn of_kind(kind: WebhookVerifyKind) -> Self {
        Self {
            kind,
            secret: None,
            header: None,
            encoding: None,
            prefix: None,
        }
    }
}

/// Why a webhook is disabled.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WebhookDisabledReason {
    /// Its events failed this many times in a row.
    Failed { failures: u32 },
    /// You or an agent disabled it.
    Disabled { by: CronActor },
}

crate::open_enum!(tagged WebhookDisabledReason, tag = "kind", {
    Failed => "failed",
    Disabled => "disabled",
});

/// What an event did.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WebhookEventOutcome {
    /// Stored and being delivered.
    Pending,
    /// In the backlog: the webhook's `max_concurrent` sessions are running.
    Waiting,
    /// Accepted by its session, or its new session was created.
    Delivered,
    /// It could not be delivered, or the run it started or joined failed.
    Failed { error: String },
}

crate::open_enum!(tagged WebhookEventOutcome, tag = "kind", {
    Pending => "pending",
    Waiting => "waiting",
    Delivered => "delivered",
    Failed => "failed",
});

/// One event of a webhook's history.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct WebhookEvent {
    pub id: String,
    pub received_at: Timestamp,
    /// A test event (`webhooks.test`), which skipped the checks and limits
    /// and counts toward no failure.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub test: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivery_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_key: Option<String>,
    /// The body's size.
    pub bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
    pub outcome: Open<WebhookEventOutcome>,
    /// The session the event went to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionId>,
    /// The run it started or joined, once it has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<RunId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_end: Option<CronRunEnd>,
}

/// How many requests a webhook turned away, and when it last did, by
/// reason. Approximate: the daemon writes them with the webhook's next
/// change.
#[derive(Clone, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct WebhookCounts {
    /// Events accepted.
    #[serde(default)]
    pub accepted: u64,
    /// Requests that failed the signature check (`401`).
    #[serde(default)]
    pub rejected: u64,
    /// Requests the filter turned away (`204`).
    #[serde(default)]
    pub filtered: u64,
    /// Requests with a delivery ID seen before (`200`).
    #[serde(default)]
    pub duplicate: u64,
    /// Requests over `max_per_hour` (`429`).
    #[serde(default)]
    pub rate_limited: u64,
    /// Requests that found the backlog full (`503`).
    #[serde(default)]
    pub backlog_full: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_accepted: Option<Timestamp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_rejected: Option<Timestamp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_filtered: Option<Timestamp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_duplicate: Option<Timestamp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_rate_limited: Option<Timestamp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_backlog_full: Option<Timestamp>,
}

/// One webhook with its state and history.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct Webhook {
    pub id: WebhookId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The token in the webhook's URL, which rotating changes.
    pub url_token: String,
    /// The URL's path, `/hooks/<url_token>`, which clients put after the
    /// public base.
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    pub target: WebhookTarget,
    /// For an existing-session webhook, and a new-session one with a
    /// thread key, whose events into an existing thread use it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivery: Option<ProducerDeliveryMode>,
    /// The signature check, without its secret.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verify: Option<WebhookVerify>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub filter: Vec<WebhookCondition>,
    /// Extra request headers the model sees.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub headers: Vec<String>,
    /// The webhook's own limit of events per rolling hour; 0 or less is
    /// none. `[webhooks] max_per_hour` when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_per_hour: Option<i64>,
    /// New-session webhooks: how many of its sessions may run at once; 0 or
    /// less is no limit. 1 when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_concurrent: Option<i64>,
    /// New-session webhooks: the sessions' permission mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<PermissionMode>,
    /// The webhook's own `[webhooks] max_consecutive_failures`; 0 or less
    /// never disables.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_consecutive_failures: Option<i64>,
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disabled_reason: Option<Open<WebhookDisabledReason>>,
    pub created_by: CronActor,
    pub created_at: Timestamp,
    #[serde(default)]
    pub counts: WebhookCounts,
    #[serde(default)]
    pub consecutive_failures: u32,
    /// Events in the backlog.
    #[serde(default)]
    pub waiting: u32,
    /// The latest events, oldest first, as many as `[webhooks] history`
    /// keeps, and every event still waiting.
    #[serde(default)]
    pub history: Vec<WebhookEvent>,
    /// A new-session webhook's threads, the most recently used last.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub threads: Vec<CronThread>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WebhooksListParams {}

/// Every webhook, in creation order.
#[derive(Clone, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct WebhooksListResult {
    pub webhooks: Vec<Webhook>,
    /// Whether the daemon receives requests: `[webhooks] enabled`.
    #[serde(default = "receiving_default")]
    pub receiving: bool,
    /// The public base URLs are built on, when the daemon has one
    /// (`[public] url`, else `[server] page_url`); clients use the address
    /// they reached the daemon at otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_base: Option<String>,
}

/// Create a webhook.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WebhookCreateParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1, max = 320))]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1, max = 32768))]
    pub prompt: Option<String>,
    pub target: WebhookTarget,
    /// Existing-session webhooks and new-session ones with a thread key;
    /// `queue` when omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivery: Option<ProducerDeliveryMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verify: Option<WebhookVerify>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 32))]
    pub filter: Vec<WebhookCondition>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 32))]
    pub headers: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_per_hour: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_concurrent: Option<i64>,
    /// New-session webhooks only; `auto_approve_n` when omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<PermissionMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_consecutive_failures: Option<i64>,
    /// `true` when omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
}

impl WebhookCreateParams {
    /// A webhook delivering into `target` with every other member at its
    /// default.
    #[must_use]
    pub const fn new(target: WebhookTarget) -> Self {
        Self {
            name: None,
            prompt: None,
            target,
            delivery: None,
            verify: None,
            filter: Vec::new(),
            headers: Vec::new(),
            max_per_hour: None,
            max_concurrent: None,
            permission_mode: None,
            max_consecutive_failures: None,
            enabled: None,
        }
    }
}

/// Change a webhook. Only the members present change; `null` clears an
/// optional one. A `verify` of another kind, or with a `secret`, replaces
/// the secret; one of the same kind without a secret keeps it. Enabling the
/// webhook resets its failure count.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WebhookUpdateParams {
    pub id: WebhookId,
    #[serde(
        default,
        deserialize_with = "deserialize_present",
        skip_serializing_if = "Option::is_none"
    )]
    pub name: Option<Option<String>>,
    #[serde(
        default,
        deserialize_with = "deserialize_present",
        skip_serializing_if = "Option::is_none"
    )]
    pub prompt: Option<Option<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<WebhookTarget>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivery: Option<ProducerDeliveryMode>,
    #[serde(
        default,
        deserialize_with = "deserialize_present",
        skip_serializing_if = "Option::is_none"
    )]
    pub verify: Option<Option<WebhookVerify>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter: Option<Vec<WebhookCondition>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<Vec<String>>,
    #[serde(
        default,
        deserialize_with = "deserialize_present",
        skip_serializing_if = "Option::is_none"
    )]
    pub max_per_hour: Option<Option<i64>>,
    #[serde(
        default,
        deserialize_with = "deserialize_present",
        skip_serializing_if = "Option::is_none"
    )]
    pub max_concurrent: Option<Option<i64>>,
    #[serde(
        default,
        deserialize_with = "deserialize_present",
        skip_serializing_if = "Option::is_none"
    )]
    pub permission_mode: Option<Option<PermissionMode>>,
    #[serde(
        default,
        deserialize_with = "deserialize_present",
        skip_serializing_if = "Option::is_none"
    )]
    pub max_consecutive_failures: Option<Option<i64>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
}

impl WebhookUpdateParams {
    /// An update of `id` that changes nothing yet.
    #[must_use]
    pub const fn new(id: WebhookId) -> Self {
        Self {
            id,
            name: None,
            prompt: None,
            target: None,
            delivery: None,
            verify: None,
            filter: None,
            headers: None,
            max_per_hour: None,
            max_concurrent: None,
            permission_mode: None,
            max_consecutive_failures: None,
            enabled: None,
        }
    }
}

/// A webhook after a create, an update, or a rotation, with its new
/// signing secret when the request made one.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct WebhookResult {
    pub webhook: Webhook,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret: Option<String>,
}

/// Delete a webhook. Its sessions stay.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WebhookDeleteParams {
    pub id: WebhookId,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct WebhookDeleteResult {}

/// What `webhooks.rotate` replaces.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WebhookRotate {
    /// The URL token: the old URL answers `404` at once.
    Url,
    /// The signing secret, or the bearer token.
    Secret,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WebhookRotateParams {
    pub id: WebhookId,
    pub what: WebhookRotate,
}

/// Read a webhook's signing secret, or bearer token.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WebhookSecretParams {
    pub id: WebhookId,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct WebhookSecretResult {
    /// Absent for a webhook without a check.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret: Option<String>,
}

/// One request header.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct WebhookHeader {
    #[schemars(length(min = 1, max = 128))]
    pub name: String,
    #[schemars(length(max = 8192))]
    pub value: String,
}

/// Read one event of a webhook's history with its request.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WebhookEventParams {
    pub id: WebhookId,
    pub event: String,
}

/// An event with its request: the method, the headers but the secret ones,
/// and the body, as text when it is UTF-8 and as base64 otherwise.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct WebhookEventResult {
    pub event: WebhookEvent,
    pub method: String,
    #[serde(default)]
    pub headers: Vec<WebhookHeader>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body_text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body_base64: Option<String>,
}

/// Deliver a test event: no check, filter, or limit applies.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WebhookTestParams {
    pub id: WebhookId,
    /// `{}` when omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = 1_048_576))]
    pub body: Option<String>,
    /// `application/json` when omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1, max = 256))]
    pub content_type: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 32))]
    pub headers: Vec<WebhookHeader>,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct WebhookTestResult {
    pub webhook: Webhook,
    /// The test event's ID.
    pub event: String,
}

/// A webhook was created, changed, received an event, or was deleted;
/// refetch `webhooks.list`.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct WebhooksChangedNotification {}

/// The webhook whose event created a session (`SessionMeta::webhook`).
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct SessionWebhook {
    pub webhook_id: WebhookId,
    pub event_id: String,
    /// The thread the session is, for a webhook with a thread key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_key: Option<String>,
}
