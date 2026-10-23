//! Cron jobs (`docs/site/specs/cron.md`): prompts the daemon fires on a
//! schedule, into an existing session or into a new session per fire.
//! Client protocol 1.15.0.
//!
//! The schedule, the target, and the actor are closed by design: a new kind
//! of schedule or target is a new kind of job, which an older client could
//! neither show nor edit. What a fire did and why a job is disabled are
//! open: an older client shows a value it does not know as such.

use std::{borrow::Cow, fmt};

use jiff::Timestamp;
use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::{
    AgentId, CronJobId, ModelSelection, Open, PermissionMode, PermissionRule, ProducerDeliveryMode,
    ProjectId, ProjectRuntime, RunId, SessionId, SessionStatus,
    agent::{AgentPresetNameSchema, deserialize_optional_preset},
};

pub const CRONS_LIST_METHOD: &str = "crons.list";
pub const CRONS_CREATE_METHOD: &str = "crons.create";
pub const CRONS_UPDATE_METHOD: &str = "crons.update";
pub const CRONS_DELETE_METHOD: &str = "crons.delete";
pub const CRONS_RUN_METHOD: &str = "crons.run";
pub const CRONS_CHANGED_METHOD: &str = "crons.changed";

/// The most characters a job's name may have.
pub const CRON_NAME_MAX_CHARS: usize = 80;
/// The most bytes a job's prompt may have.
pub const CRON_PROMPT_MAX_BYTES: usize = 32 * 1024;

const fn attach_global() -> bool {
    true
}

const fn firing_default() -> bool {
    true
}

#[allow(clippy::trivially_copy_pass_by_ref)]
const fn attaches_global(global: &bool) -> bool {
    *global
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

/// When a job fires. The daemon stores and returns `cron` and `at`; `in`
/// is accepted on create and update only and becomes `at` at once.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
#[schemars(transform = crate::event::open_object_schema)]
pub enum CronSchedule {
    /// A five-field cron expression (minute, hour, day of month, month, day
    /// of week) in the daemon host's local time; day of month and day of
    /// week combine with OR when both are restricted, as in Vixie cron.
    Cron(String),
    /// Once, this long from now: a whole number with `s`, `m`, `h`, or `d`,
    /// from `1s` to `365d`.
    In(String),
    /// Once, at this time.
    At(Timestamp),
}

/// How many fires a cron schedule delivers before the job finishes: a whole
/// number from 1, or `"unlimited"`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CronMaxRuns {
    Count(u32),
    Unlimited,
}

impl CronMaxRuns {
    const UNLIMITED: &'static str = "unlimited";

    /// Whether `runs` fires reach this limit.
    #[must_use]
    pub fn reached(self, runs: u64) -> bool {
        match self {
            Self::Count(count) => runs >= u64::from(count),
            Self::Unlimited => false,
        }
    }
}

impl Default for CronMaxRuns {
    fn default() -> Self {
        Self::Count(1)
    }
}

impl fmt::Display for CronMaxRuns {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Count(count) => count.fmt(formatter),
            Self::Unlimited => formatter.write_str(Self::UNLIMITED),
        }
    }
}

impl Serialize for CronMaxRuns {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Count(count) => serializer.serialize_u32(*count),
            Self::Unlimited => serializer.serialize_str(Self::UNLIMITED),
        }
    }
}

impl<'de> Deserialize<'de> for CronMaxRuns {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Wire {
            Count(u64),
            Text(String),
        }
        match Wire::deserialize(deserializer)? {
            Wire::Count(count) => u32::try_from(count)
                .ok()
                .filter(|count| *count >= 1)
                .map(Self::Count)
                .ok_or_else(|| serde::de::Error::custom("max_runs must be from 1 to 4294967295")),
            Wire::Text(text) if text == Self::UNLIMITED => Ok(Self::Unlimited),
            Wire::Text(_) => Err(serde::de::Error::custom(
                "max_runs must be a whole number from 1 or \"unlimited\"",
            )),
        }
    }
}

impl JsonSchema for CronMaxRuns {
    fn inline_schema() -> bool {
        true
    }

    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("CronMaxRuns")
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "oneOf": [
                {"type": "integer", "minimum": 1, "maximum": 4_294_967_295_u64},
                {"type": "string", "const": "unlimited"}
            ]
        })
    }
}

/// Where each fire goes.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
#[schemars(transform = crate::event::open_object_schema)]
pub enum CronTarget {
    /// A root session.
    Session(SessionId),
    /// A delegated session.
    Subagent(SessionId),
    /// A new root session per fire, from this template.
    New(CronTemplate),
}

impl CronTarget {
    /// The target kind, which is the resource of the `cron` permission.
    #[must_use]
    pub const fn kind(&self) -> CronTargetKind {
        match self {
            Self::Session(_) => CronTargetKind::Session,
            Self::Subagent(_) => CronTargetKind::Subagent,
            Self::New(_) => CronTargetKind::New,
        }
    }

    /// The session an in-session job fires into.
    #[must_use]
    pub const fn session(&self) -> Option<SessionId> {
        match self {
            Self::Session(session) | Self::Subagent(session) => Some(*session),
            Self::New(_) => None,
        }
    }
}

/// A job's target kind: the resource of the `cron` permission.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CronTargetKind {
    Session,
    Subagent,
    New,
}

impl CronTargetKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Session => "session",
            Self::Subagent => "subagent",
            Self::New => "new",
        }
    }
}

impl fmt::Display for CronTargetKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// What a new-session job's sessions start with. `runtimes`, `global`, and
/// `cwd` have the shape of `session.create`'s members of the same names.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct CronTemplate {
    /// The agent each run session starts with; it must be able to run as a
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
    /// Rules added to each run session as its permission overlay.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = 256))]
    pub permission_rules: Vec<PermissionRule>,
    /// The runtimes a run session attaches besides `global`, in order, as
    /// `session.create`'s `runtimes` names them; as `session.create`
    /// without `runtimes` when omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = 256))]
    pub runtimes: Option<Vec<ProjectRuntime>>,
    /// Whether a run session attaches the global runtime; `true` when
    /// omitted.
    #[serde(default = "attach_global", skip_serializing_if = "attaches_global")]
    pub global: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1, max = 4096))]
    pub cwd: Option<String>,
    /// The project the run sessions belong to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<ProjectId>,
}

/// Who created or changed a job: you, through a client, or an agent from a
/// session.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
#[schemars(transform = crate::event::open_object_schema)]
pub enum CronActor {
    User,
    Agent(CronAgentActor),
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct CronAgentActor {
    pub agent: AgentId,
    pub session: SessionId,
}

/// Why a job is disabled.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CronDisabledReason {
    /// It ran `max_runs` times, or its one-shot time passed.
    Finished,
    /// It failed this many times in a row.
    Failed { failures: u32 },
    /// You or an agent disabled it.
    Disabled { by: CronActor },
}

crate::open_enum!(tagged CronDisabledReason, tag = "kind", {
    Finished => "finished",
    Failed => "failed",
    Disabled => "disabled",
});

crate::open_enum! {
    /// Why a fire was skipped. A value this build does not know is still a
    /// skip.
    #[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, JsonSchema, PartialEq, Serialize)]
    #[serde(rename_all = "snake_case")]
    pub enum CronSkipReason {
        /// The job's previous fire into its session was still waiting.
        PreviousFireNotDelivered,
        /// The run session of the job's previous fire was still running.
        PreviousRunStillRunning,
        /// The daemon was not running at the fire time; `missed` counts the
        /// fires missed.
        DaemonWasDown,
        /// The daemon stopped after recording the fire and before
        /// delivering it.
        Interrupted,
        /// `[cron] enabled = false` held at the fire time.
        FiringDisabled,
        /// The daemon was waiting to restart (client protocol 1.19.0).
        DaemonRestarting,
    }
}

/// What a fire did.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CronFireOutcome {
    /// Recorded and being delivered.
    Pending,
    /// The prompt was accepted by its session, or its new session was
    /// created.
    Delivered,
    Skipped {
        reason: CronSkipReason,
    },
    /// The fire could not start, or the run it started failed.
    Failed {
        error: String,
    },
}

crate::open_enum!(tagged CronFireOutcome, tag = "kind", {
    Pending => "pending",
    Delivered => "delivered",
    Skipped => "skipped",
    Failed => "failed",
});

/// How the run a fire started, or steered into, ended.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct CronRunEnd {
    pub status: SessionStatus,
    pub at: Timestamp,
}

/// One entry of a job's history.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct CronFire {
    /// The schedule's time; absent for a run now.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scheduled_at: Option<Timestamp>,
    /// When the daemon fired, or recorded the skip.
    pub fired_at: Timestamp,
    /// A run now (`crons.run` or `cron_run`), which counts toward neither
    /// `max_runs` nor the failure count.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub run_now: bool,
    pub outcome: Open<CronFireOutcome>,
    /// For a fire skipped because the daemon was down, how many fires were
    /// missed, from `scheduled_at` on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub missed: Option<u64>,
    /// The session the fire went to: the new session for a new-session job,
    /// the child session for an agent's run now of one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionId>,
    /// The run the prompt started or joined, once it has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<RunId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_end: Option<CronRunEnd>,
}

/// One job with its state and history.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct CronJob {
    pub id: CronJobId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub prompt: String,
    pub schedule: CronSchedule,
    /// Meaningful for a cron schedule; a one-shot fires once.
    pub max_runs: CronMaxRuns,
    pub target: CronTarget,
    /// For an in-session job.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivery: Option<ProducerDeliveryMode>,
    /// For a new-session job: the run sessions' permission mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<PermissionMode>,
    /// The job's own `[cron] max_consecutive_failures`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_consecutive_failures: Option<u32>,
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disabled_reason: Option<Open<CronDisabledReason>>,
    pub created_by: CronActor,
    pub created_at: Timestamp,
    /// The next due time; absent for a disabled or finished job.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_fire: Option<Timestamp>,
    /// Scheduled fires delivered, counted against `max_runs`.
    #[serde(default)]
    pub runs: u64,
    #[serde(default)]
    pub consecutive_failures: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_fire: Option<Timestamp>,
    /// The latest fires, oldest first, as many as `[cron] history` keeps.
    #[serde(default)]
    pub history: Vec<CronFire>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CronsListParams {}

/// Every job, in creation order.
#[derive(Clone, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct CronsListResult {
    pub jobs: Vec<CronJob>,
    /// Whether the daemon fires jobs: `[cron] enabled`.
    #[serde(default = "firing_default")]
    pub firing: bool,
}

/// Create a job.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CronCreateParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1, max = 320))]
    pub name: Option<String>,
    #[schemars(length(min = 1, max = 32768))]
    pub prompt: String,
    pub schedule: CronSchedule,
    /// Cron schedules only; 1 when omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_runs: Option<CronMaxRuns>,
    pub target: CronTarget,
    /// In-session jobs only; `queue` when omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivery: Option<ProducerDeliveryMode>,
    /// New-session jobs only; `auto_approve_n` when omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<PermissionMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1))]
    pub max_consecutive_failures: Option<u32>,
    /// `true` when omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
}

/// Change a job. Only the members present change; `null` clears an
/// optional one. A new schedule or enabling the job computes its next fire
/// from now; enabling it also resets its failure count.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CronUpdateParams {
    pub id: CronJobId,
    #[serde(
        default,
        deserialize_with = "deserialize_present",
        skip_serializing_if = "Option::is_none"
    )]
    pub name: Option<Option<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schedule: Option<CronSchedule>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_runs: Option<CronMaxRuns>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<CronTarget>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivery: Option<ProducerDeliveryMode>,
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
    pub max_consecutive_failures: Option<Option<u32>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
}

impl CronUpdateParams {
    /// An update of `id` that changes nothing yet.
    #[must_use]
    pub const fn new(id: CronJobId) -> Self {
        Self {
            id,
            name: None,
            prompt: None,
            schedule: None,
            max_runs: None,
            target: None,
            delivery: None,
            permission_mode: None,
            max_consecutive_failures: None,
            enabled: None,
        }
    }
}

/// The job after a create or an update.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct CronJobResult {
    pub job: CronJob,
}

/// Delete a job. Its run sessions stay.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CronDeleteParams {
    pub id: CronJobId,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct CronDeleteResult {}

/// Fire a job now, as a scheduled fire would. It is recorded as a run now,
/// counts toward neither `max_runs` nor the failure count, and moves
/// nothing else.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CronRunParams {
    pub id: CronJobId,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct CronRunResult {
    pub job: CronJob,
    /// The session the fire went to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionId>,
}

/// A job was created, changed, fired, or deleted; refetch `crons.list`.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct CronsChangedNotification {}

/// The job a new-session job's run session was started by
/// (`SessionMeta::cron`).
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct SessionCron {
    pub job_id: CronJobId,
    pub fired_at: Timestamp,
}
