//! The cron store (`docs/site/specs/cron.md` sections 3 and 11):
//! `<data_root>/crons.json`, one versioned file holding every job with its
//! state and history.
//!
//! Every change rereads the file under the memory lock
//! ([`crate::memory::MemoryLock`]), changes it, and writes it through a
//! flushed temporary file renamed over it, as `machines.json` is written, so
//! two daemons sharing a data root never lose each other's changes and a
//! fire recorded by one is never recorded again by the other. Reads take
//! the copy kept in memory. Unknown fields are refused.

pub(crate) mod schedule;
#[cfg(test)]
mod tests;

use std::{
    fs, io,
    path::{Path, PathBuf},
    sync::Mutex,
};

use cookie_agent_protocol::{
    AgentId, CronActor, CronDisabledReason, CronFire, CronFireOutcome, CronJob, CronJobId,
    CronMaxRuns, CronRunEnd, CronSchedule, CronSkipReason, CronTarget, CronTargetKind,
    CronTemplate, ModelSelection, PermissionMode, PermissionRule, ProducerDeliveryMode,
    ProducerMessageId, ProjectId, ProjectRuntime, RunId, SessionId, SessionStatus,
};
use jiff::{Timestamp, tz::TimeZone};
use serde::{Deserialize, Serialize};

use crate::memory::{MemoryError, MemoryRoot};

/// The store's file in the data root.
pub(crate) const CRONS_FILE: &str = "crons.json";
/// The format this build writes.
const CRONS_VERSION: u32 = 1;

/// Why a cron request was refused or failed.
#[derive(Debug, thiserror::Error)]
pub enum CronError {
    #[error("there is no cron job {0}")]
    NotFound(CronJobId),
    #[error("the job's {field} was refused: {reason}")]
    Invalid { field: &'static str, reason: String },
    /// Reading or writing `crons.json` failed.
    #[error("{0}")]
    Store(String),
}

impl CronError {
    pub(crate) fn invalid(field: &'static str, reason: impl Into<String>) -> Self {
        Self::Invalid {
            field,
            reason: reason.into(),
        }
    }
}

impl From<MemoryError> for CronError {
    fn from(error: MemoryError) -> Self {
        Self::Store(error.to_string())
    }
}

/// `crons.json`.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct CronFile {
    version: u32,
    #[serde(default)]
    jobs: Vec<StoredJob>,
}

/// A stored schedule: `in` is never stored.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum StoredSchedule {
    Cron(String),
    At(Timestamp),
}

impl StoredSchedule {
    /// The first time after `after` this schedule fires, in `zone`; none
    /// for a one-shot whose time is not after `after`.
    pub(crate) fn next_after(&self, after: Timestamp, zone: &TimeZone) -> Option<Timestamp> {
        match self {
            Self::Cron(expression) => schedule::CronExpr::parse(expression)
                .ok()?
                .next_after(after, zone),
            Self::At(at) => (*at > after).then_some(*at),
        }
    }

    /// How many fires after `after` and up to `until` this schedule has,
    /// up to `limit`.
    pub(crate) fn count_between(
        &self,
        after: Timestamp,
        until: Timestamp,
        zone: &TimeZone,
        limit: u64,
    ) -> u64 {
        match self {
            Self::Cron(expression) => schedule::CronExpr::parse(expression)
                .map_or(0, |expr| expr.count_between(after, until, zone, limit)),
            Self::At(at) => u64::from(*at > after && *at <= until),
        }
    }

    pub(crate) const fn is_one_shot(&self) -> bool {
        matches!(self, Self::At(_))
    }

    fn wire(&self) -> CronSchedule {
        match self {
            Self::Cron(expression) => CronSchedule::Cron(expression.clone()),
            Self::At(at) => CronSchedule::At(*at),
        }
    }
}

/// A stored new-session template.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StoredTemplate {
    pub(crate) agent: AgentId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) preset: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) model: Option<ModelSelection>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) permission_rules: Vec<PermissionRule>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) runtimes: Option<Vec<ProjectRuntime>>,
    pub(crate) global: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) project: Option<ProjectId>,
}

impl From<CronTemplate> for StoredTemplate {
    fn from(template: CronTemplate) -> Self {
        Self {
            agent: template.agent,
            preset: template.preset,
            model: template.model,
            permission_rules: template.permission_rules,
            runtimes: template.runtimes,
            global: template.global,
            cwd: template.cwd,
            project: template.project,
        }
    }
}

impl StoredTemplate {
    fn wire(&self) -> CronTemplate {
        CronTemplate {
            agent: self.agent.clone(),
            preset: self.preset.clone(),
            model: self.model.clone(),
            permission_rules: self.permission_rules.clone(),
            runtimes: self.runtimes.clone(),
            global: self.global,
            cwd: self.cwd.clone(),
            project: self.project.clone(),
        }
    }
}

/// A stored target.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum StoredTarget {
    Session(SessionId),
    Subagent(SessionId),
    New(StoredTemplate),
}

impl StoredTarget {
    pub(crate) const fn kind(&self) -> CronTargetKind {
        match self {
            Self::Session(_) => CronTargetKind::Session,
            Self::Subagent(_) => CronTargetKind::Subagent,
            Self::New(_) => CronTargetKind::New,
        }
    }

    /// The session an in-session job fires into.
    pub(crate) const fn session(&self) -> Option<SessionId> {
        match self {
            Self::Session(session) | Self::Subagent(session) => Some(*session),
            Self::New(_) => None,
        }
    }

    pub(crate) const fn template(&self) -> Option<&StoredTemplate> {
        match self {
            Self::New(template) => Some(template),
            _ => None,
        }
    }

    fn wire(&self) -> CronTarget {
        match self {
            Self::Session(session) => CronTarget::Session(*session),
            Self::Subagent(session) => CronTarget::Subagent(*session),
            Self::New(template) => CronTarget::New(template.wire()),
        }
    }
}

impl From<CronTarget> for StoredTarget {
    fn from(target: CronTarget) -> Self {
        match target {
            CronTarget::Session(session) => Self::Session(session),
            CronTarget::Subagent(session) => Self::Subagent(session),
            CronTarget::New(template) => Self::New(template.into()),
        }
    }
}

/// Why a stored job is disabled.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum StoredDisabled {
    Finished,
    Failed { failures: u32 },
    Disabled { by: CronActor },
}

impl StoredDisabled {
    fn wire(&self) -> CronDisabledReason {
        match self {
            Self::Finished => CronDisabledReason::Finished,
            Self::Failed { failures } => CronDisabledReason::Failed {
                failures: *failures,
            },
            Self::Disabled { by } => CronDisabledReason::Disabled { by: by.clone() },
        }
    }
}

/// Why a stored fire was skipped.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum StoredSkip {
    PreviousFireNotDelivered,
    PreviousRunStillRunning,
    DaemonWasDown,
    Interrupted,
    FiringDisabled,
    DaemonRestarting,
    /// The target session was held (`docs/site/specs/held-sessions.md`).
    SessionHeld,
}

impl StoredSkip {
    const fn wire(self) -> CronSkipReason {
        match self {
            Self::PreviousFireNotDelivered => CronSkipReason::PreviousFireNotDelivered,
            Self::PreviousRunStillRunning => CronSkipReason::PreviousRunStillRunning,
            Self::DaemonWasDown => CronSkipReason::DaemonWasDown,
            Self::Interrupted => CronSkipReason::Interrupted,
            Self::FiringDisabled => CronSkipReason::FiringDisabled,
            Self::DaemonRestarting => CronSkipReason::DaemonRestarting,
            Self::SessionHeld => CronSkipReason::SessionHeld,
        }
    }
}

/// What a stored fire did.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum StoredOutcome {
    Pending,
    Delivered,
    Skipped { reason: StoredSkip },
    Failed { error: String },
}

/// How the run of a stored fire ended.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StoredRunEnd {
    pub(crate) status: SessionStatus,
    pub(crate) at: Timestamp,
}

/// One stored fire.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StoredFire {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) scheduled_at: Option<Timestamp>,
    pub(crate) fired_at: Timestamp,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(crate) run_now: bool,
    pub(crate) outcome: StoredOutcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) missed: Option<u64>,
    /// The producer idempotency key the prompt is sent with.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) session_id: Option<SessionId>,
    /// The producer message that carries the prompt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) message_id: Option<ProducerMessageId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) run_id: Option<RunId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) run_end: Option<StoredRunEnd>,
    /// Whether the fire's run counts toward the failure count: scheduled
    /// fires do, runs now do not.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(crate) uncounted: bool,
}

impl StoredFire {
    /// A fire skipped at `fired_at` for `reason`.
    pub(crate) fn skipped(
        scheduled_at: Option<Timestamp>,
        fired_at: Timestamp,
        reason: StoredSkip,
    ) -> Self {
        Self {
            scheduled_at,
            fired_at,
            run_now: false,
            outcome: StoredOutcome::Skipped { reason },
            missed: None,
            key: None,
            session_id: None,
            message_id: None,
            run_id: None,
            run_end: None,
            uncounted: false,
        }
    }

    /// A fire at `fired_at` with `outcome`.
    pub(crate) fn with_outcome(
        scheduled_at: Option<Timestamp>,
        fired_at: Timestamp,
        outcome: StoredOutcome,
    ) -> Self {
        Self {
            outcome,
            ..Self::skipped(scheduled_at, fired_at, StoredSkip::Interrupted)
        }
    }

    /// A delivered fire whose run has not ended yet.
    pub(crate) fn awaits_run(&self) -> bool {
        matches!(self.outcome, StoredOutcome::Delivered)
            && self.run_end.is_none()
            && self.session_id.is_some()
    }

    fn wire(&self) -> CronFire {
        CronFire {
            scheduled_at: self.scheduled_at,
            fired_at: self.fired_at,
            run_now: self.run_now,
            outcome: match &self.outcome {
                StoredOutcome::Pending => CronFireOutcome::Pending,
                StoredOutcome::Delivered => CronFireOutcome::Delivered,
                StoredOutcome::Skipped { reason } => CronFireOutcome::Skipped {
                    reason: reason.wire(),
                },
                StoredOutcome::Failed { error } => CronFireOutcome::Failed {
                    error: error.clone(),
                },
            }
            .into(),
            missed: self.missed,
            session_id: self.session_id,
            run_id: self.run_id,
            run_end: self.run_end.as_ref().map(|end| CronRunEnd {
                status: end.status,
                at: end.at,
            }),
        }
    }
}

/// One stored job: its definition, state, and history.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StoredJob {
    pub(crate) id: CronJobId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) name: Option<String>,
    pub(crate) prompt: String,
    pub(crate) schedule: StoredSchedule,
    pub(crate) max_runs: CronMaxRuns,
    pub(crate) target: StoredTarget,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) delivery: Option<ProducerDeliveryMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) permission_mode: Option<PermissionMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) max_consecutive_failures: Option<u32>,
    pub(crate) enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) disabled_reason: Option<StoredDisabled>,
    pub(crate) created_by: CronActor,
    pub(crate) created_at: Timestamp,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) next_fire: Option<Timestamp>,
    #[serde(default)]
    pub(crate) runs: u64,
    #[serde(default)]
    pub(crate) consecutive_failures: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) last_fire: Option<Timestamp>,
    #[serde(default)]
    pub(crate) history: Vec<StoredFire>,
}

impl StoredJob {
    /// The job as clients and tools read it.
    pub(crate) fn wire(&self) -> CronJob {
        CronJob {
            id: self.id.clone(),
            name: self.name.clone(),
            prompt: self.prompt.clone(),
            schedule: self.schedule.wire(),
            max_runs: self.max_runs,
            target: self.target.wire(),
            delivery: self.delivery,
            permission_mode: self.permission_mode,
            max_consecutive_failures: self.max_consecutive_failures,
            enabled: self.enabled,
            disabled_reason: self
                .disabled_reason
                .as_ref()
                .map(|reason| reason.wire().into()),
            created_by: self.created_by.clone(),
            created_at: self.created_at,
            next_fire: self.next_fire,
            runs: self.runs,
            consecutive_failures: self.consecutive_failures,
            last_fire: self.last_fire,
            history: self.history.iter().map(StoredFire::wire).collect(),
        }
    }

    /// Whether the job keeps its target session waiting: enabled, in a
    /// session, with a next fire time.
    pub(crate) fn waits_on(&self, session: SessionId) -> bool {
        self.enabled && self.next_fire.is_some() && self.target.session() == Some(session)
    }

    /// Whether the job fires once by its definition: an `at` schedule (what
    /// `in` becomes) or a `cron` schedule with `max_runs = 1`. The agent
    /// waking itself through one is the agent still at work, which holds a
    /// push notification back (push notifications spec section 2).
    pub(crate) fn one_time(&self) -> bool {
        match self.schedule {
            StoredSchedule::At(_) => true,
            StoredSchedule::Cron(_) => self.max_runs == CronMaxRuns::Count(1),
        }
    }

    /// The name lists show: the job's name, else the start of its prompt.
    pub(crate) fn label(&self) -> String {
        if let Some(name) = &self.name {
            return name.clone();
        }
        let line = self.prompt.lines().next().unwrap_or_default().trim();
        let mut label = line.chars().take(40).collect::<String>();
        if line.chars().count() > 40 {
            label.push('…');
        }
        label
    }

    /// Whether an agent created the job, so that reaching `max_runs`
    /// deletes it.
    pub(crate) const fn agent_created(&self) -> bool {
        matches!(self.created_by, CronActor::Agent(_))
    }

    /// The failures in a row that disable the job.
    pub(crate) fn failure_limit(&self, configured: u32) -> u32 {
        self.max_consecutive_failures.unwrap_or(configured)
    }

    /// Appends `fire`, keeping the last `limit` fires.
    pub(crate) fn record(&mut self, fire: StoredFire, limit: usize) {
        self.history.push(fire);
        let excess = self.history.len().saturating_sub(limit.max(1));
        self.history.drain(..excess);
    }

    /// Disables the job for `reason`; it has no next fire.
    pub(crate) fn disable(&mut self, reason: StoredDisabled) {
        self.enabled = false;
        self.disabled_reason = Some(reason);
        self.next_fire = None;
    }

    /// Counts a failed run or fire: disabled at the limit.
    pub(crate) fn count_failure(&mut self, configured_limit: u32) {
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        if self.enabled && self.consecutive_failures >= self.failure_limit(configured_limit) {
            self.disable(StoredDisabled::Failed {
                failures: self.consecutive_failures,
            });
        }
    }
}

/// What a store change did, for the caller to announce.
#[derive(Debug, Default)]
pub(crate) struct Changed {
    /// The sessions whose jobs changed: targets before and after.
    pub(crate) sessions: Vec<SessionId>,
}

/// The daemon's cron store.
pub(crate) struct CronStore {
    path: PathBuf,
    memory: MemoryRoot,
    /// The jobs as last read or written, in creation order.
    jobs: Mutex<Vec<StoredJob>>,
    /// Serializes changes within this process; the memory lock serializes
    /// processes.
    mutation: Mutex<()>,
    /// Bumped whenever the jobs change.
    changes: tokio::sync::watch::Sender<u64>,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl CronStore {
    /// Reads `<data_dir>/crons.json`; a store that does not exist yet has
    /// no jobs. A file that does not parse fails.
    pub(crate) fn open(data_dir: &Path) -> Result<Self, CronError> {
        let path = data_dir.join(CRONS_FILE);
        let file = read(&path)?;
        Ok(Self {
            path,
            memory: MemoryRoot::new(data_dir),
            jobs: Mutex::new(file.jobs),
            mutation: Mutex::new(()),
            changes: tokio::sync::watch::channel(0).0,
        })
    }

    /// Every job, in creation order.
    pub(crate) fn jobs(&self) -> Vec<StoredJob> {
        lock(&self.jobs).clone()
    }

    /// One job.
    pub(crate) fn job(&self, id: &CronJobId) -> Option<StoredJob> {
        lock(&self.jobs).iter().find(|job| job.id == *id).cloned()
    }

    /// Whether an enabled job with a next fire targets `session`.
    pub(crate) fn waits_on(&self, session: SessionId) -> bool {
        lock(&self.jobs).iter().any(|job| job.waits_on(session))
    }

    /// Whether an enabled one-time job with a next fire targets `session`.
    pub(crate) fn waits_on_once(&self, session: SessionId) -> bool {
        lock(&self.jobs)
            .iter()
            .any(|job| job.one_time() && job.waits_on(session))
    }

    /// Whether a delivered fire into `session` waits for its run's end.
    pub(crate) fn awaits_run_in(&self, session: SessionId) -> bool {
        lock(&self.jobs).iter().any(|job| {
            job.history
                .iter()
                .any(|fire| fire.awaits_run() && fire.session_id == Some(session))
        })
    }

    /// The earliest next fire of an enabled job.
    pub(crate) fn earliest_fire(&self) -> Option<Timestamp> {
        lock(&self.jobs)
            .iter()
            .filter(|job| job.enabled)
            .filter_map(|job| job.next_fire)
            .min()
    }

    /// The enabled jobs whose next fire is at or before `now`, with that
    /// fire.
    pub(crate) fn due(&self, now: Timestamp) -> Vec<(CronJobId, Timestamp)> {
        lock(&self.jobs)
            .iter()
            .filter(|job| job.enabled)
            .filter_map(|job| {
                job.next_fire
                    .filter(|next| *next <= now)
                    .map(|next| (job.id.clone(), next))
            })
            .collect()
    }

    /// Signals every change to the jobs.
    pub(crate) fn subscribe(&self) -> tokio::sync::watch::Receiver<u64> {
        self.changes.subscribe()
    }

    /// Runs `change` on the jobs as they are on disk, under the memory
    /// lock, and writes the result when it changed anything. Blocking.
    pub(crate) fn mutate<T>(
        &self,
        change: impl FnOnce(&mut Vec<StoredJob>) -> Result<T, CronError>,
    ) -> Result<(T, Changed), CronError> {
        let _mutation = lock(&self.mutation);
        let memory_lock = self.memory.lock()?;
        // Another daemon sharing the data root may have changed it.
        let current = read(&self.path)?.jobs;
        let mut jobs = current.clone();
        let value = change(&mut jobs)?;
        let mut changed = Changed::default();
        if jobs != current {
            let file = CronFile {
                version: CRONS_VERSION,
                jobs: jobs.clone(),
            };
            write(&self.path, &file)?;
            let targets = |jobs: &[StoredJob]| {
                jobs.iter()
                    .filter_map(|job| job.target.session())
                    .collect::<Vec<_>>()
            };
            changed.sessions = targets(&current);
            changed.sessions.extend(targets(&jobs));
            changed.sessions.sort_unstable();
            changed.sessions.dedup();
        }
        drop(memory_lock);
        let mut cached = lock(&self.jobs);
        if *cached != jobs {
            *cached = jobs;
            drop(cached);
            self.changes.send_modify(|revision| *revision += 1);
        }
        Ok((value, changed))
    }
}

/// A new job ID no job in `jobs` has.
pub(crate) fn new_job_id(jobs: &[StoredJob]) -> CronJobId {
    use std::hash::{BuildHasher, Hasher};
    const ALPHABET: &[u8; 36] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    loop {
        let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
        hasher.write_u128(Timestamp::now().as_nanosecond().unsigned_abs());
        let mut value = hasher.finish();
        let id = (0..8)
            .map(|_| {
                let character = ALPHABET[usize::try_from(value % 36).expect("small")];
                value /= 36;
                char::from(character)
            })
            .collect::<String>();
        let id = CronJobId::new(id).expect("eight base-36 characters");
        if jobs.iter().all(|job| job.id != id) {
            return id;
        }
    }
}

/// The store in `path`; empty when it does not exist.
fn read(path: &Path) -> Result<CronFile, CronError> {
    let bytes = match cookie_agent_models::secure_store::read_replaced(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(CronFile {
                version: CRONS_VERSION,
                jobs: Vec::new(),
            });
        }
        Err(error) => return Err(CronError::Store(format!("{}: {error}", path.display()))),
    };
    let file: CronFile = serde_json::from_slice(&bytes)
        .map_err(|error| CronError::Store(format!("{}: {error}", path.display())))?;
    if file.version != CRONS_VERSION {
        return Err(CronError::Store(format!(
            "{}: version {} is not {CRONS_VERSION}",
            path.display(),
            file.version
        )));
    }
    Ok(file)
}

/// Writes `file` to `path` through a flushed temporary file in the same
/// folder, renamed over it.
fn write(path: &Path, file: &CronFile) -> Result<(), CronError> {
    let failed = |error: io::Error| CronError::Store(format!("{}: {error}", path.display()));
    let directory = path
        .parent()
        .ok_or_else(|| CronError::Store(format!("{} has no parent folder", path.display())))?;
    fs::create_dir_all(directory).map_err(failed)?;
    let mut bytes = serde_json::to_vec_pretty(file).expect("the cron store serializes");
    bytes.push(b'\n');
    let temporary = directory.join(format!(".{CRONS_FILE}.{}.tmp", uuid::Uuid::now_v7()));
    let result = (|| {
        let mut output = fs::File::create(&temporary)?;
        io::Write::write_all(&mut output, &bytes)?;
        output.sync_all()?;
        drop(output);
        fs::rename(&temporary, path)?;
        #[cfg(unix)]
        fs::File::open(directory)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result.map_err(failed)
}
