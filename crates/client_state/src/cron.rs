//! The cron jobs every client shows the same way (`docs/site/specs/cron.md`
//! section 12): whether the daemon keeps jobs, a job's title, its schedule
//! and a plain reading of it, its target, its state, what each fire did,
//! and the job form's fields as `crons.create` and `crons.update` take
//! them.

use cookie_agent_protocol::{
    AgentId, CronCreateParams, CronDisabledReason, CronFire, CronFireOutcome, CronJob, CronMaxRuns,
    CronSchedule, CronSkipReason, CronTarget, CronTemplate, CronUpdateParams, ModelSelection, Open,
    PermissionMode, ProducerDeliveryMode, SemVer, SessionId, SessionStatus,
};
use jiff::{Timestamp, tz::TimeZone};

use crate::session_actions::permission_mode_label;

/// The client protocol minor from which the daemon keeps cron jobs
/// (`crons.*`, `SessionMeta::cron`).
pub const CRONS_MINOR: u32 = 15;

/// Whether a daemon speaking `version` keeps cron jobs.
pub fn daemon_has_crons(version: Option<SemVer>) -> bool {
    version.is_some_and(|version| {
        version
            .shared_minor(cookie_agent_protocol::CLIENT_PROTOCOL_VERSION)
            .is_some_and(|minor| minor >= CRONS_MINOR)
    })
}

/// The most characters of a prompt a job without a name shows as its title.
pub const TITLE_PROMPT_CHARS: usize = 48;

/// What lists show for a job: its name, else the start of its prompt's first
/// line.
pub fn job_title(job: &CronJob) -> String {
    match job.name.as_deref().map(str::trim) {
        Some(name) if !name.is_empty() => name.to_owned(),
        _ => prompt_start(&job.prompt),
    }
}

fn prompt_start(prompt: &str) -> String {
    let line = prompt
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("");
    let mut chars = line.chars();
    let start: String = chars.by_ref().take(TITLE_PROMPT_CHARS).collect();
    if chars.next().is_some() || prompt.trim() != line {
        format!("{start}…")
    } else {
        start
    }
}

/// The schedule as a user would type it again: the cron expression, `at
/// <time>` in `zone`, or `in <delay>`.
pub fn schedule_text(schedule: &CronSchedule, zone: &TimeZone) -> String {
    match schedule {
        CronSchedule::Cron(expression) => expression.clone(),
        CronSchedule::In(delay) => format!("in {delay}"),
        CronSchedule::At(at) => format!("at {}", local_minute(*at, zone)),
    }
}

/// A plain reading of the schedule and its run limit, such as `every day at
/// 03:00`, `every 15 minutes, 3 runs`, or `once at 2026-10-23 03:00`. Cron
/// times are the daemon host's; `zone` shows a one-shot's time.
pub fn schedule_reading(schedule: &CronSchedule, max_runs: CronMaxRuns, zone: &TimeZone) -> String {
    match schedule {
        CronSchedule::Cron(expression) => {
            let reading = cron_reading(expression).unwrap_or_else(|| format!("cron {expression}"));
            match max_runs {
                CronMaxRuns::Unlimited => reading,
                CronMaxRuns::Count(1) => format!("{reading}, once"),
                CronMaxRuns::Count(count) => format!("{reading}, {count} runs"),
            }
        }
        CronSchedule::In(delay) => format!("once in {delay}"),
        CronSchedule::At(at) => format!("once at {}", local_minute(*at, zone)),
    }
}

/// `at` in `zone` to the minute, such as `2026-10-23 03:00`. Formatted by
/// hand: jiff's strftime would grow the web page.
pub fn local_minute(at: Timestamp, zone: &TimeZone) -> String {
    let time = zone.to_datetime(at);
    format!(
        "{}-{:02}-{:02} {:02}:{:02}",
        time.year(),
        time.month(),
        time.day(),
        time.hour(),
        time.minute()
    )
}

/// How long until `then` from `now`: `in 5 minutes`, `in 2 hours`, `in 3
/// days`, or `now` once it is due.
pub fn until(then: Timestamp, now: Timestamp) -> String {
    let seconds = then.as_second() - now.as_second();
    if seconds <= 0 {
        return "now".to_owned();
    }
    let (count, unit) = if seconds < 60 {
        (seconds, "second")
    } else if seconds < 3600 {
        (seconds / 60, "minute")
    } else if seconds < 86_400 {
        (seconds / 3600, "hour")
    } else {
        (seconds / 86_400, "day")
    };
    let plural = if count == 1 { "" } else { "s" };
    format!("in {count} {unit}{plural}")
}

const WEEKDAYS: [&str; 7] = [
    "Sunday",
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
];
const MONTHS: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];

/// One field of an expression, in the shapes a reading names.
#[derive(Debug, Eq, PartialEq)]
enum Field {
    Any,
    /// `*/n`.
    Every(u32),
    /// Values and ranges, in the order written.
    Values(Vec<(u32, u32)>),
}

fn field(text: &str, names: &[&str], sunday_seven: bool) -> Option<Field> {
    if text == "*" {
        return Some(Field::Any);
    }
    if let Some(step) = text.strip_prefix("*/") {
        return step.parse().ok().filter(|step| *step > 0).map(Field::Every);
    }
    let value = |part: &str| -> Option<u32> {
        let number = part.parse::<u32>().ok().or_else(|| {
            names
                .iter()
                .position(|name| name[..3].eq_ignore_ascii_case(part))
                .and_then(|index| u32::try_from(index).ok())
                .map(|index| if names.len() == 12 { index + 1 } else { index })
        })?;
        Some(if sunday_seven && number == 7 {
            0
        } else {
            number
        })
    };
    let mut values = Vec::new();
    for part in text.split(',') {
        if part.contains('/') {
            return None;
        }
        let range = match part.split_once('-') {
            Some((low, high)) => (value(low)?, value(high)?),
            None => {
                let single = value(part)?;
                (single, single)
            }
        };
        values.push(range);
    }
    Some(Field::Values(values))
}

fn single(field: &Field) -> Option<u32> {
    match field {
        Field::Values(values) => match values.as_slice() {
            [(low, high)] if low == high => Some(*low),
            _ => None,
        },
        _ => None,
    }
}

fn singles(field: &Field) -> Option<Vec<u32>> {
    match field {
        Field::Values(values) => values
            .iter()
            .map(|(low, high)| (low == high).then_some(*low))
            .collect(),
        _ => None,
    }
}

fn listing(parts: &[String]) -> String {
    match parts {
        [] => String::new(),
        [one] => one.clone(),
        [init @ .., last] => format!("{} and {last}", init.join(", ")),
    }
}

fn named(field: &Field, names: &[&str], offset: u32) -> Option<String> {
    let Field::Values(values) = field else {
        return None;
    };
    let name = |value: u32| -> Option<&str> {
        names
            .get(usize::try_from(value.checked_sub(offset)?).ok()?)
            .copied()
    };
    let parts = values
        .iter()
        .map(|(low, high)| {
            if low == high {
                name(*low).map(str::to_owned)
            } else {
                Some(format!("{} to {}", name(*low)?, name(*high)?))
            }
        })
        .collect::<Option<Vec<_>>>()?;
    Some(listing(&parts))
}

fn ordinal(day: u32) -> String {
    let suffix = match (day % 10, day % 100) {
        (1, 11) | (2, 12) | (3, 13) => "th",
        (1, _) => "st",
        (2, _) => "nd",
        (3, _) => "rd",
        _ => "th",
    };
    format!("{day}{suffix}")
}

/// A plain reading of a five-field cron expression, for the shapes people
/// write: `every day at 03:00`, `every Monday to Friday at 09:00 and 17:00`,
/// `every 15 minutes`, `every hour at :05`, `on the 1st of every month at
/// 08:00`. `None` for anything else, which clients show as the expression.
pub fn cron_reading(expression: &str) -> Option<String> {
    let fields: Vec<&str> = expression.split_whitespace().collect();
    let [minute, hour, day, month, weekday] = fields.as_slice() else {
        return None;
    };
    let minute = field(minute, &[], false)?;
    let hour = field(hour, &[], false)?;
    let day = field(day, &[], false)?;
    let month = field(month, &MONTHS, false)?;
    let weekday = field(weekday, &WEEKDAYS, true)?;

    let time = match (&minute, &hour) {
        (Field::Any, Field::Any) => "every minute".to_owned(),
        (Field::Every(step), Field::Any) => format!("every {step} minutes"),
        (minute, Field::Any) => format!("every hour at :{:02}", single(minute)?),
        (minute, Field::Every(step)) => {
            format!("every {step} hours at :{:02}", single(minute)?)
        }
        (minute, hours) => {
            let minute = single(minute)?;
            let hours = singles(hours)?;
            let times: Vec<String> = hours
                .iter()
                .map(|hour| format!("{hour:02}:{minute:02}"))
                .collect();
            format!("at {}", listing(&times))
        }
    };
    let daily = time.starts_with("at ");

    let months = match &month {
        Field::Any => None,
        Field::Every(_) => return None,
        month => Some(named(month, &MONTHS, 1)?),
    };
    let in_months = months
        .as_deref()
        .map(|months| format!(" in {months}"))
        .unwrap_or_default();
    let days = match (&day, &weekday) {
        (Field::Any, Field::Any) => months.as_ref().map(|_| format!("every day{in_months}")),
        (Field::Any, weekday) => Some(format!(
            "every {}{in_months}",
            named(weekday, &WEEKDAYS, 0)?
        )),
        (day, Field::Any) => {
            let days: Vec<String> = singles(day)?.into_iter().map(ordinal).collect();
            let of = months.as_deref().unwrap_or("every month");
            Some(format!("on the {} of {of}", listing(&days)))
        }
        (day, weekday) => {
            let days: Vec<String> = singles(day)?.into_iter().map(ordinal).collect();
            Some(format!(
                "on the {} and every {}{in_months}",
                listing(&days),
                named(weekday, &WEEKDAYS, 0)?
            ))
        }
    };
    let reading = match (days, daily) {
        (None, true) => format!("every day {time}"),
        (Some(days), true) => format!("{days} {time}"),
        (None, false) => time,
        (Some(days), false) => format!("{time}, {days}"),
    };
    Some(reading)
}

/// How lists name a job's target: the session's title,
/// `subagent <title>` for a delegated one, or `new session · <agent>`.
/// `title` names a session the client knows; another shows its id's start.
pub fn target_label(target: &CronTarget, title: impl Fn(SessionId) -> Option<String>) -> String {
    let name = |session: SessionId| title(session).unwrap_or_else(|| short_session(session));
    match target {
        CronTarget::Session(id) => name(*id),
        CronTarget::Subagent(id) => format!("subagent {}", name(*id)),
        CronTarget::New(template) => format!("new session · {}", template.agent),
    }
}

fn short_session(session: SessionId) -> String {
    session.to_string().chars().take(8).collect()
}

/// Whether the job is on, and why not: `enabled`, `finished`, `failed 3
/// times in a row`, `disabled by you`, `disabled by agent builder`.
pub fn state_label(job: &CronJob) -> String {
    if job.enabled {
        return "enabled".to_owned();
    }
    match &job.disabled_reason {
        Some(Open::Known(reason)) => disabled_reason_label(reason),
        Some(Open::Unknown { .. }) | None => "disabled".to_owned(),
    }
}

/// Why a job is disabled, as `state_label` says it.
pub fn disabled_reason_label(reason: &CronDisabledReason) -> String {
    match reason {
        CronDisabledReason::Finished => "finished".to_owned(),
        CronDisabledReason::Failed { failures } => {
            let times = if *failures == 1 { "time" } else { "times" };
            format!("failed {failures} {times} in a row")
        }
        CronDisabledReason::Disabled { by } => format!("disabled by {}", actor_label(by)),
    }
}

/// Who made or changed a job: `you`, or `agent <id>`.
pub fn actor_label(actor: &cookie_agent_protocol::CronActor) -> String {
    match actor {
        cookie_agent_protocol::CronActor::User => "you".to_owned(),
        cookie_agent_protocol::CronActor::Agent(agent) => format!("agent {}", agent.agent),
    }
}

/// Why a fire was skipped.
pub const fn skip_reason_label(reason: CronSkipReason) -> &'static str {
    match reason {
        CronSkipReason::PreviousFireNotDelivered => "previous fire not delivered",
        CronSkipReason::PreviousRunStillRunning => "previous run still running",
        CronSkipReason::DaemonWasDown => "daemon was down",
        CronSkipReason::Interrupted => "interrupted",
        CronSkipReason::FiringDisabled => "firing turned off",
        CronSkipReason::DaemonRestarting => "daemon was restarting",
        CronSkipReason::SessionHeld => "session held",
        CronSkipReason::Unknown => "another reason",
    }
}

/// What a fire did, and how the run it led to ended: `delivered ·
/// completed`, `skipped: daemon was down (3 missed)`, `failed: no model`,
/// prefixed `run now · ` for a run now.
pub fn fire_outcome(fire: &CronFire) -> String {
    let outcome = match &fire.outcome {
        Open::Known(CronFireOutcome::Pending) => "pending".to_owned(),
        Open::Known(CronFireOutcome::Delivered) => match &fire.run_end {
            Some(end) => format!("delivered · {}", run_status_label(end.status)),
            None if fire.run_id.is_some() => "delivered · running".to_owned(),
            None => "delivered".to_owned(),
        },
        Open::Known(CronFireOutcome::Skipped { reason }) => match fire.missed {
            Some(missed) if missed > 1 => {
                format!("skipped: {} ({missed} missed)", skip_reason_label(*reason))
            }
            _ => format!("skipped: {}", skip_reason_label(*reason)),
        },
        Open::Known(CronFireOutcome::Failed { error }) => format!("failed: {error}"),
        Open::Unknown { tag, .. } => tag.replace('_', " "),
    };
    if fire.run_now {
        format!("run now · {outcome}")
    } else {
        outcome
    }
}

/// A short word for a fire's outcome, for a list column: `delivered`,
/// `completed`, `failed`, `skipped`, `pending`, or `never` for a job that
/// has not fired.
pub fn last_outcome(job: &CronJob) -> &'static str {
    let Some(fire) = job.history.last() else {
        return "never";
    };
    match &fire.outcome {
        Open::Known(CronFireOutcome::Pending) => "pending",
        Open::Known(CronFireOutcome::Delivered) => match &fire.run_end {
            Some(end) => run_status_label(end.status),
            None => "delivered",
        },
        Open::Known(CronFireOutcome::Skipped { .. }) => "skipped",
        Open::Known(CronFireOutcome::Failed { .. }) => "failed",
        Open::Unknown { .. } => "unknown",
    }
}

/// How a run a fire led to ended.
pub const fn run_status_label(status: SessionStatus) -> &'static str {
    match status {
        SessionStatus::Completed => "completed",
        SessionStatus::Failed => "failed",
        SessionStatus::Cancelled => "cancelled",
        SessionStatus::Running => "running",
        _ => "ended",
    }
}

/// One detail of a job: `label: value`, and the session it names, which
/// clients link to.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CronDetail {
    pub label: &'static str,
    pub value: String,
    pub session: Option<SessionId>,
}

impl CronDetail {
    fn new(label: &'static str, value: String) -> Self {
        Self {
            label,
            value,
            session: None,
        }
    }
}

/// The details of a job, in the order clients show them: its target and,
/// for an agent's job, the session that created it name their sessions.
/// `title` names a session the client knows, as `target_label` reads it.
pub fn detail_lines(
    job: &CronJob,
    now: Timestamp,
    zone: &TimeZone,
    title: impl Fn(SessionId) -> Option<String>,
) -> Vec<CronDetail> {
    let mut lines = vec![
        CronDetail::new("id", job.id.to_string()),
        CronDetail::new("schedule", schedule_text(&job.schedule, zone)),
        CronDetail::new("reads", schedule_reading(&job.schedule, job.max_runs, zone)),
        CronDetail {
            label: "target",
            value: target_label(&job.target, &title),
            session: job.target.session(),
        },
        CronDetail::new("state", state_label(job)),
    ];
    if let Some(next) = job.next_fire {
        lines.push(CronDetail::new(
            "next fire",
            format!("{} ({})", local_minute(next, zone), until(next, now)),
        ));
    }
    if matches!(job.schedule, CronSchedule::Cron(_)) {
        let runs = match job.max_runs {
            CronMaxRuns::Count(count) => format!("{} of {count}", job.runs),
            CronMaxRuns::Unlimited => format!("{}, no limit", job.runs),
        };
        lines.push(CronDetail::new("runs", runs));
    }
    if let Some(delivery) = job.delivery {
        lines.push(CronDetail::new(
            "delivery",
            delivery_label(delivery).to_owned(),
        ));
    }
    if let CronTarget::New(template) = &job.target {
        lines.push(CronDetail::new("agent", template_agent(template)));
        if let Some(model) = &template.model {
            lines.push(CronDetail::new("model", model.to_string()));
        }
        if let Some(project) = &template.project {
            lines.push(CronDetail::new("project", project.to_string()));
        }
        if !template.permission_rules.is_empty() {
            lines.push(CronDetail::new(
                "rules",
                format!("{} added", template.permission_rules.len()),
            ));
        }
    }
    if let Some(mode) = job.permission_mode {
        lines.push(CronDetail::new(
            "mode",
            permission_mode_label(mode).to_owned(),
        ));
    }
    if job.consecutive_failures > 0 {
        lines.push(CronDetail::new(
            "failures",
            format!("{} in a row", job.consecutive_failures),
        ));
    }
    if let Some(limit) = job.max_consecutive_failures {
        lines.push(CronDetail::new("failure limit", limit.to_string()));
    }
    lines.push(CronDetail::new(
        "created",
        format!(
            "{} by {}",
            local_minute(job.created_at, zone),
            actor_label(&job.created_by)
        ),
    ));
    if let cookie_agent_protocol::CronActor::Agent(agent) = &job.created_by {
        lines.push(CronDetail {
            label: "created in",
            value: title(agent.session).unwrap_or_else(|| short_session(agent.session)),
            session: Some(agent.session),
        });
    }
    lines
}

fn template_agent(template: &CronTemplate) -> String {
    match &template.preset {
        Some(preset) => format!("{} ({preset})", template.agent),
        None => template.agent.to_string(),
    }
}

/// How clients name a delivery mode.
pub const fn delivery_label(delivery: ProducerDeliveryMode) -> &'static str {
    match delivery {
        ProducerDeliveryMode::Steer => "steer",
        ProducerDeliveryMode::Queue => "queue",
    }
}

/// Reads a schedule as typed: `in <delay>` (such as `in 20m`), `at <RFC
/// 3339 time>`, or a five-field cron expression.
pub fn parse_schedule(text: &str) -> Result<CronSchedule, String> {
    let text = text.trim();
    if let Some(delay) = text.strip_prefix("in ") {
        let delay = delay.trim();
        if delay.is_empty() {
            return Err("a delay needs a length, such as `in 20m`".to_owned());
        }
        return Ok(CronSchedule::In(delay.to_owned()));
    }
    if let Some(at) = text.strip_prefix("at ") {
        return at
            .trim()
            .parse::<Timestamp>()
            .map(CronSchedule::At)
            .map_err(|_| "a time needs RFC 3339, such as `at 2026-10-23T03:00:00Z`".to_owned());
    }
    let fields: Vec<&str> = text.split_whitespace().collect();
    if fields.len() == 5 {
        return Ok(CronSchedule::Cron(fields.join(" ")));
    }
    Err("a schedule is five cron fields (`0 3 * * *`) or a delay (`in 20m`)".to_owned())
}

/// Reads a run limit as typed: empty for the daemon's default, or a whole
/// number, where 0 or less runs until the job is deleted or disabled.
pub fn parse_max_runs(text: &str) -> Result<Option<CronMaxRuns>, String> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(None);
    }
    text.parse::<i64>()
        .ok()
        .and_then(CronMaxRuns::from_count)
        .map(Some)
        .ok_or_else(|| "runs are a whole number; 0 or less runs without a limit".to_owned())
}

/// Reads a model pin as typed: empty for none, `provider/model`, or
/// `provider/model[variant]`.
pub fn parse_model_pin(text: &str) -> Result<Option<ModelSelection>, String> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(None);
    }
    let (model, variant) = match text.strip_suffix(']').and_then(|rest| rest.split_once('[')) {
        Some((model, "base")) => (model, None),
        Some((model, variant)) => (model, Some(variant)),
        None => (text, None),
    };
    serde_json::from_value(serde_json::json!({"model": model, "variant": variant}))
        .map(Some)
        .map_err(|_| "a model is `provider/model` or `provider/model[variant]`".to_owned())
}

/// Where a form's job fires.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CronDraftTarget {
    /// An existing session: a root one, or a delegated one when
    /// `subagent`.
    Session { session: SessionId, subagent: bool },
    /// A new session per fire, from `template`; the form edits its agent
    /// and model pin and keeps the rest.
    New { template: CronTemplate },
}

impl CronDraftTarget {
    /// The target `crons.create` takes.
    pub fn target(&self) -> CronTarget {
        match self {
            Self::Session {
                session,
                subagent: false,
            } => CronTarget::Session(*session),
            Self::Session {
                session,
                subagent: true,
            } => CronTarget::Subagent(*session),
            Self::New { template } => CronTarget::New(template.clone()),
        }
    }

    pub const fn is_new(&self) -> bool {
        matches!(self, Self::New { .. })
    }
}

/// A new-session template running `agent` with nothing else set.
pub fn bare_template(agent: AgentId) -> CronTemplate {
    CronTemplate {
        agent,
        preset: None,
        model: None,
        permission_rules: Vec::new(),
        runtimes: None,
        global: true,
        cwd: None,
        project: None,
    }
}

/// The job form: what a user types to create or edit a job.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CronDraft {
    pub name: String,
    pub prompt: String,
    /// As [`parse_schedule`] reads it.
    pub schedule: String,
    /// As [`parse_max_runs`] reads it.
    pub max_runs: String,
    pub target: CronDraftTarget,
    /// For an in-session job.
    pub delivery: ProducerDeliveryMode,
    /// For a new-session job; the daemon's default when `None`.
    pub permission_mode: Option<PermissionMode>,
    /// For a new-session job, as [`parse_model_pin`] reads it.
    pub model: String,
    /// For a new-session job: the template's agent.
    pub agent: String,
}

impl CronDraft {
    /// A form for a job that fires into `session`, a delegated one when
    /// `subagent`.
    pub fn for_session(session: SessionId, subagent: bool) -> Self {
        Self {
            name: String::new(),
            prompt: String::new(),
            schedule: String::new(),
            max_runs: String::new(),
            target: CronDraftTarget::Session { session, subagent },
            delivery: ProducerDeliveryMode::Queue,
            permission_mode: None,
            model: String::new(),
            agent: String::new(),
        }
    }

    /// A form for a job that starts a session from `template` per fire.
    pub fn for_new_session(template: CronTemplate) -> Self {
        let agent = template.agent.to_string();
        let model = template
            .model
            .as_ref()
            .map(model_pin_text)
            .unwrap_or_default();
        Self {
            name: String::new(),
            prompt: String::new(),
            schedule: String::new(),
            max_runs: String::new(),
            target: CronDraftTarget::New { template },
            delivery: ProducerDeliveryMode::Queue,
            permission_mode: None,
            model,
            agent,
        }
    }

    /// The form filled from `job`, to edit it.
    pub fn of_job(job: &CronJob, zone: &TimeZone) -> Self {
        let target = match &job.target {
            CronTarget::Session(session) => CronDraftTarget::Session {
                session: *session,
                subagent: false,
            },
            CronTarget::Subagent(session) => CronDraftTarget::Session {
                session: *session,
                subagent: true,
            },
            CronTarget::New(template) => CronDraftTarget::New {
                template: template.clone(),
            },
        };
        let (agent, model) = match &job.target {
            CronTarget::New(template) => (
                template.agent.to_string(),
                template
                    .model
                    .as_ref()
                    .map(model_pin_text)
                    .unwrap_or_default(),
            ),
            _ => (String::new(), String::new()),
        };
        Self {
            name: job.name.clone().unwrap_or_default(),
            prompt: job.prompt.clone(),
            schedule: match &job.schedule {
                CronSchedule::At(at) => format!("at {at}"),
                schedule => schedule_text(schedule, zone),
            },
            max_runs: match job.schedule {
                CronSchedule::Cron(_) => job.max_runs.count().to_string(),
                _ => String::new(),
            },
            target,
            delivery: job.delivery.unwrap_or(ProducerDeliveryMode::Queue),
            permission_mode: job.permission_mode,
            model,
            agent,
        }
    }

    /// The target with the form's agent and model pin applied.
    fn resolved_target(&self) -> Result<CronTarget, String> {
        match &self.target {
            CronDraftTarget::New { template } => {
                let agent = self.agent.trim();
                let agent = if agent.is_empty() {
                    template.agent.clone()
                } else {
                    agent
                        .parse::<AgentId>()
                        .map_err(|_| format!("`{agent}` is not an agent id"))?
                };
                let mut template = template.clone();
                template.agent = agent;
                template.model = parse_model_pin(&self.model)?;
                Ok(CronTarget::New(template))
            }
            target => Ok(target.target()),
        }
    }

    fn checked_prompt(&self) -> Result<String, String> {
        let prompt = self.prompt.trim();
        if prompt.is_empty() {
            return Err("a job needs a prompt".to_owned());
        }
        Ok(self.prompt.trim_end().to_owned())
    }

    fn checked_name(&self) -> Option<String> {
        let name = self.name.trim();
        (!name.is_empty()).then(|| name.to_owned())
    }

    /// `crons.create`'s params, or why the form cannot be sent.
    pub fn create_params(&self) -> Result<CronCreateParams, String> {
        let schedule = parse_schedule(&self.schedule)?;
        let max_runs = match schedule {
            CronSchedule::Cron(_) => parse_max_runs(&self.max_runs)?,
            _ => None,
        };
        let new = self.target.is_new();
        Ok(CronCreateParams {
            name: self.checked_name(),
            prompt: self.checked_prompt()?,
            schedule,
            max_runs,
            target: self.resolved_target()?,
            delivery: (!new).then_some(self.delivery),
            permission_mode: if new { self.permission_mode } else { None },
            max_consecutive_failures: None,
            enabled: None,
        })
    }

    /// `crons.update`'s params for what the form changed in `job`, or why
    /// the form cannot be sent.
    pub fn update_params(
        &self,
        job: &CronJob,
        zone: &TimeZone,
    ) -> Result<CronUpdateParams, String> {
        let before = Self::of_job(job, zone);
        let mut params = CronUpdateParams::new(job.id.clone());
        if self.name.trim() != before.name.trim() {
            params.name = Some(self.checked_name());
        }
        let prompt = self.checked_prompt()?;
        if prompt != job.prompt {
            params.prompt = Some(prompt);
        }
        let schedule = parse_schedule(&self.schedule)?;
        let schedule_changed = self.schedule.trim() != before.schedule.trim();
        if schedule_changed {
            params.schedule = Some(schedule.clone());
        }
        if matches!(schedule, CronSchedule::Cron(_))
            && (self.max_runs.trim() != before.max_runs.trim() || schedule_changed)
        {
            params.max_runs = parse_max_runs(&self.max_runs)?;
            if params.max_runs == Some(job.max_runs) && !schedule_changed {
                params.max_runs = None;
            }
        }
        let target = self.resolved_target()?;
        if target != job.target {
            params.target = Some(target);
        }
        if !self.target.is_new() && Some(self.delivery) != job.delivery {
            params.delivery = Some(self.delivery);
        }
        if self.target.is_new() && self.permission_mode != job.permission_mode {
            params.permission_mode = Some(self.permission_mode);
        }
        Ok(params)
    }
}

/// A model pin as [`parse_model_pin`] reads it back.
pub fn model_pin_text(model: &ModelSelection) -> String {
    match &model.variant {
        Some(variant) => format!("{}[{variant}]", model.model),
        None => model.model.to_string(),
    }
}

/// Whether `update` changes anything.
pub fn update_changes_anything(update: &CronUpdateParams) -> bool {
    *update != CronUpdateParams::new(update.id.clone())
}

/// The job with `id` in `jobs`.
pub fn job_by_id<'a>(jobs: &'a [CronJob], id: &str) -> Option<&'a CronJob> {
    jobs.iter().find(|job| job.id.as_str() == id)
}

#[cfg(test)]
#[path = "cron_tests.rs"]
mod tests;
