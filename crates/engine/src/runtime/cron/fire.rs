//! Firing cron jobs (`docs/site/specs/cron.md` sections 4 and 5): the
//! scheduler task, in-session and new-session fires, runs now, what the
//! daemon finds at start, and the run outcomes that count failures.
//!
//! A fire is keyed by its job and scheduled time. It is written to the
//! job's history, as pending, before it is delivered, under the store's
//! lock, so it is claimed once even by two daemons sharing the data root.
//! Delivery sends the prompt as a producer message owned by the job's
//! source (`cron:<job id>`, see [`cron_owner`]) with the fire's key as the
//! idempotency key, so a delivery retried after a restart is accepted once.
//! A fire's outcome is the run its prompt lands in, recorded when that run
//! ends.

use std::{sync::Mutex, time::Duration};

use cookie_agent_protocol::{
    CronJobId, CronRunResult, ModelSelection, PermissionMode, ProducerDeliveryMode,
    ProducerIdempotencyKey, ProducerMessageId, ProducerName, ProducerOwner, ProducerSendParams,
    ProducerSource, RunId, RunSelection, SessionCron, SessionId, SessionStatus, SessionTitle,
};
use jiff::Timestamp;
use tokio::task::JoinHandle;

use crate::crons::{
    CronError, StoredDisabled, StoredFire, StoredJob, StoredOutcome, StoredRunEnd, StoredSkip,
    StoredTarget, StoredTemplate,
};
use crate::runtime::{
    Engine, EngineError, cron::api::host_zone, messaging::producers::ProducerAuthority,
};

/// The longest the scheduler sleeps before a fire before it looks again, so
/// that a clock change or a suspended host is noticed. With no fire ahead it
/// sleeps until the jobs change.
const SCHEDULER_MAX_SLEEP: Duration = Duration::from_secs(60);
/// The most fires a `daemon was down` entry counts.
const MISSED_COUNT_LIMIT: u64 = 1_000_000;
/// The producer name of every fire.
const FIRE_PRODUCER_NAME: &str = "scheduled";

/// The scheduler's state.
pub(crate) struct CronSchedulerState {
    /// Wakes the scheduler: the jobs changed. Shared with the scheduler, which
    /// waits on it without holding the engine.
    pub(crate) wake: std::sync::Arc<tokio::sync::Notify>,
    task: Mutex<Option<JoinHandle<()>>>,
    /// When the engine opened: a fire due before it was missed while the
    /// daemon was down.
    pub(crate) started_at: Timestamp,
}

impl Default for CronSchedulerState {
    fn default() -> Self {
        Self {
            wake: std::sync::Arc::new(tokio::sync::Notify::new()),
            task: Mutex::new(None),
            started_at: Timestamp::now(),
        }
    }
}

/// The producer owner of job `job`'s fires: an external owner whose source
/// is `cron:<job id>`. `ProducerOwner` is a closed enum, so a fire reuses
/// the external shape with a source kind no connection can have.
pub(crate) fn cron_owner(job: &CronJobId) -> ProducerOwner {
    ProducerOwner::external(
        ProducerSource::cron(job),
        ProducerName::new(FIRE_PRODUCER_NAME).expect("static producer name"),
    )
}

/// Whether `owner` is a cron job's.
pub(crate) fn is_cron_owner(owner: &ProducerOwner) -> bool {
    owner
        .source()
        .is_some_and(|source| source.cron_job().is_some())
}

fn escape_attribute(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// `fired_at` in the host's local time, with its offset.
fn local_time(fired_at: Timestamp) -> String {
    fired_at
        .to_zoned(host_zone())
        .strftime("%Y-%m-%dT%H:%M:%S%:z")
        .to_string()
}

/// The prompt as the model sees it: marked as a scheduled job, never as
/// the user (section 4.2).
pub(crate) fn render_scheduled_job(job: &StoredJob, fired_at: Timestamp) -> String {
    let name = job
        .name
        .as_ref()
        .map(|name| format!(" name=\"{}\"", escape_attribute(name)))
        .unwrap_or_default();
    format!(
        "<scheduled_job id=\"{}\"{name} fired_at=\"{}\">\n{}\n</scheduled_job>",
        job.id,
        local_time(fired_at),
        job.prompt
    )
}

/// A new-session fire's session title: the job's label and the local time
/// of the fire.
fn run_title(job: &StoredJob, fired_at: Timestamp) -> Result<SessionTitle, String> {
    let local = fired_at
        .to_zoned(host_zone())
        .strftime("%Y-%m-%d %H:%M")
        .to_string();
    let mut label = job.label();
    // Room for the separator and the time within the title limit.
    let room = SessionTitle::MAX_BYTES - local.len() - 8;
    while label.len() > room {
        label.pop();
    }
    SessionTitle::new(format!("{label} · {local}")).map_err(|error| error.to_string())
}

/// Where a delivered fire went.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Delivery {
    pub(crate) session: SessionId,
    pub(crate) message: Option<ProducerMessageId>,
}

/// A fire claimed in a job's history, to deliver.
struct Claim {
    job: StoredJob,
    key: String,
    fired_at: Timestamp,
}

/// Removes or disables `jobs[index]`, which has nothing left to fire: an
/// agent's job is deleted, yours is kept, disabled as finished.
fn finish(jobs: &mut Vec<StoredJob>, index: usize) {
    if jobs[index].agent_created() {
        jobs.remove(index);
    } else if jobs[index].enabled {
        jobs[index].disable(StoredDisabled::Finished);
    }
}

/// Finishes `jobs[index]` when it is enabled with no next fire, as a
/// one-shot is after its time.
fn finish_if_done(jobs: &mut Vec<StoredJob>, index: usize) {
    if jobs[index].enabled && jobs[index].next_fire.is_none() {
        finish(jobs, index);
    }
}

/// Records the outcome of the fire keyed `key` of job `id`: delivered, which
/// counts toward `max_runs` unless it is a run now, or failed, which counts
/// toward the failure limit unless it is a run now.
fn settle_fire(
    jobs: &mut Vec<StoredJob>,
    id: &CronJobId,
    key: &str,
    result: &Result<Delivery, String>,
    failure_limit: u32,
) {
    let Some(index) = jobs.iter().position(|job| job.id == *id) else {
        return;
    };
    let job = &mut jobs[index];
    let Some(fire) = job
        .history
        .iter_mut()
        .rev()
        .find(|fire| fire.key.as_deref() == Some(key))
    else {
        return;
    };
    if !matches!(fire.outcome, StoredOutcome::Pending) {
        return;
    }
    let counted = !fire.run_now;
    match result {
        Ok(delivery) => {
            fire.outcome = StoredOutcome::Delivered;
            fire.session_id = Some(delivery.session);
            fire.message_id = delivery.message;
            if counted {
                job.runs = job.runs.saturating_add(1);
                if job.schedule.is_one_shot() || job.max_runs.reached(job.runs) {
                    finish(jobs, index);
                    return;
                }
            }
        }
        Err(error) => {
            fire.outcome = StoredOutcome::Failed {
                error: error.clone(),
            };
            if counted {
                job.count_failure(failure_limit);
            }
        }
    }
    if counted {
        finish_if_done(jobs, index);
    }
}

/// How long the scheduler sleeps at `now` before the earliest fire `next`:
/// until it is due, but at least `retry_after` while the store fails, and
/// at most [`SCHEDULER_MAX_SLEEP`]. With no fire ahead it waits for a change.
pub(crate) fn scheduler_sleep(
    next: Option<Timestamp>,
    now: Timestamp,
    retry_after: Option<Duration>,
) -> Option<Duration> {
    next.map(|next| {
        let millis = next.as_millisecond().saturating_sub(now.as_millisecond());
        Duration::from_millis(u64::try_from(millis).unwrap_or(0))
            .max(retry_after.unwrap_or_default())
            .min(SCHEDULER_MAX_SLEEP)
    })
}

/// Records `end`, the end of `run` of `session`, on every fire awaiting
/// its run there whose prompt `landed` in it: a failed run counts a
/// failure, a completed one resets the count, any other end changes
/// nothing, and a run now counts for neither.
pub(crate) fn record_run_end(
    jobs: &mut [StoredJob],
    session: SessionId,
    run: RunId,
    end: StoredRunEnd,
    landed: impl Fn(Option<ProducerMessageId>) -> bool,
    failure_limit: u32,
) {
    for job in jobs.iter_mut() {
        let (mut failed, mut completed) = (false, false);
        for fire in job
            .history
            .iter_mut()
            .filter(|fire| fire.awaits_run() && fire.session_id == Some(session))
        {
            if !landed(fire.message_id) {
                continue;
            }
            fire.run_id = Some(run);
            fire.run_end = Some(end.clone());
            if !fire.uncounted {
                failed |= end.status == SessionStatus::Failed;
                completed |= end.status == SessionStatus::Completed;
            }
        }
        if completed {
            job.consecutive_failures = 0;
        }
        if failed {
            job.count_failure(failure_limit);
        }
    }
}

impl Engine {
    /// Starts the scheduler, like the residency janitor: it first records
    /// what the daemon finds at start, then sleeps until the earliest next
    /// fire or until the jobs change, and fires every due job.
    pub(in crate::runtime) fn start_cron_scheduler(&self) {
        let Some(runtime) = self.inner.runtime.clone() else {
            return;
        };
        let weak = std::sync::Arc::downgrade(&self.inner);
        let task = runtime.spawn(async move {
            {
                let Some(inner) = weak.upgrade() else {
                    return;
                };
                let engine = Engine { inner };
                engine.reconcile_crons_at_start().await;
            }
            // While the store fails, a due fire stays due: the scheduler
            // waits a growing delay before it tries again.
            let mut backoff = crate::runtime::helpers::RETRY_BACKOFF.delays();
            let mut retry_after = None;
            loop {
                let Some(inner) = weak.upgrade() else {
                    break;
                };
                let engine = Engine { inner };
                if engine.admission_tasks_closing() {
                    break;
                }
                let sleep = scheduler_sleep(
                    engine.inner.crons.earliest_fire(),
                    Timestamp::now(),
                    retry_after,
                );
                let wake = std::sync::Arc::clone(&engine.inner.cron_scheduler.wake);
                drop(engine);
                // A change between reading the earliest fire and waiting is
                // not lost: `notify_one` keeps a permit for the next wait.
                match sleep {
                    Some(sleep) => tokio::select! {
                        () = tokio::time::sleep(sleep) => {}
                        () = wake.notified() => {}
                    },
                    None => wake.notified().await,
                }
                let Some(inner) = weak.upgrade() else {
                    break;
                };
                let engine = Engine { inner };
                // Once a restart is due, fires are left for the restarted
                // daemon; a due fire would otherwise wake this loop at once.
                if engine.admission_tasks_closing() || engine.restart_due() {
                    break;
                }
                if engine.cron_tick(Timestamp::now()).await {
                    retry_after = Some(backoff.next_delay());
                } else {
                    backoff.reset();
                    retry_after = None;
                }
            }
        });
        *self
            .inner
            .cron_scheduler
            .task
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(task);
    }

    /// Takes the scheduler task, for shutdown to stop it.
    pub(in crate::runtime) fn take_cron_scheduler(&self) -> Option<JoinHandle<()>> {
        self.inner
            .cron_scheduler
            .task
            .lock()
            .ok()
            .and_then(|mut task| task.take())
    }

    /// What the daemon finds at start (section 4.1): a fire written but
    /// neither delivered nor started is `skipped (interrupted)`, or
    /// delivered when its session shows it was; a job whose next fire passed
    /// while the daemon was down catches up its latest missed fire within
    /// `[cron] missed_grace`, which stays its next fire, and gets one
    /// `skipped (daemon was down)` entry for the rest, moving to its next
    /// future fire when nothing is caught up. Then every session a job waits
    /// on says so.
    pub(crate) async fn reconcile_crons_at_start(&self) {
        let started_at = self.inner.cron_scheduler.started_at;
        let mut found = Vec::new();
        for job in self.inner.crons.jobs() {
            for fire in &job.history {
                if !matches!(fire.outcome, StoredOutcome::Pending) {
                    continue;
                }
                let Some(key) = fire.key.clone() else {
                    continue;
                };
                let delivery = self.find_delivery(&job, &key, fire.fired_at).await;
                found.push((job.id.clone(), key, delivery));
            }
        }
        let limit = self
            .inner
            .config
            .load()
            .runtime
            .cron
            .max_consecutive_failures;
        let history = self.inner.config.load().runtime.cron.history;
        let grace = self.inner.config.load().runtime.cron.missed_grace;
        let result = self
            .crons_blocking(move |_, jobs| {
                for (id, key, delivery) in &found {
                    match delivery {
                        Some(delivery) => settle_fire(jobs, id, key, &Ok(*delivery), limit),
                        None => {
                            if let Some(index) = jobs.iter().position(|job| job.id == *id) {
                                if let Some(fire) = jobs[index]
                                    .history
                                    .iter_mut()
                                    .find(|fire| fire.key.as_deref() == Some(key.as_str()))
                                {
                                    fire.outcome = StoredOutcome::Skipped {
                                        reason: StoredSkip::Interrupted,
                                    };
                                }
                                finish_if_done(jobs, index);
                            }
                        }
                    }
                }
                let zone = host_zone();
                let mut index = 0;
                while index < jobs.len() {
                    let job = &mut jobs[index];
                    match job.next_fire {
                        Some(next) if job.enabled && next < started_at => {
                            let missed = 1 + job.schedule.count_between(
                                next,
                                started_at,
                                &zone,
                                MISSED_COUNT_LIMIT,
                            );
                            let caught_up = job.schedule.caught_up(
                                next,
                                started_at,
                                grace,
                                &zone,
                                MISSED_COUNT_LIMIT,
                            );
                            let skipped = missed - u64::from(caught_up.is_some());
                            if skipped > 0 {
                                let mut fire = StoredFire::skipped(
                                    Some(next),
                                    started_at,
                                    StoredSkip::DaemonWasDown,
                                );
                                fire.missed = Some(skipped);
                                job.record(fire, history);
                                job.last_fire = Some(started_at);
                            }
                            if let Some(caught_up) = caught_up {
                                // The scheduler fires it as soon as it
                                // looks, like any due fire.
                                job.next_fire = Some(caught_up);
                                index += 1;
                                continue;
                            }
                            job.next_fire = job.schedule.next_after(started_at, &zone);
                            let before = jobs.len();
                            finish_if_done(jobs, index);
                            if jobs.len() < before {
                                continue;
                            }
                        }
                        _ => {}
                    }
                    index += 1;
                }
                Ok(())
            })
            .await;
        if let Err(error) = result {
            eprintln!("cookie-agent: cron jobs at start: {error}");
        }
        for session in self
            .inner
            .crons
            .jobs()
            .iter()
            .filter(|job| job.enabled && job.next_fire.is_some())
            .filter_map(|job| job.target.session())
        {
            self.refresh_live_waiting(session);
        }
    }

    /// Where the pending fire `key` of `job` was delivered before a
    /// restart, if it was: its message in the target session, or the run
    /// session that records it. A run session created without its prompt
    /// gets the prompt now.
    async fn find_delivery(
        &self,
        job: &StoredJob,
        key: &str,
        fired_at: Timestamp,
    ) -> Option<Delivery> {
        let session = match &job.target {
            StoredTarget::Session(session) | StoredTarget::Subagent(session) => *session,
            StoredTarget::New(_) => {
                self.list_sessions()
                    .into_iter()
                    .find(|meta| {
                        meta.cron
                            .as_ref()
                            .is_some_and(|cron| cron.job_id == job.id && cron.fired_at == fired_at)
                    })?
                    .session_id
            }
        };
        let owner = cron_owner(&job.id);
        let message = self
            .goal_producer_projection(session)
            .ok()?
            .messages
            .iter()
            .find(|message| {
                message.producer_owner == owner && message.idempotency_key.as_str() == key
            })
            .map(|message| message.message_id);
        match (message, &job.target) {
            (Some(message), _) => Some(Delivery {
                session,
                message: Some(message),
            }),
            (None, StoredTarget::New(_)) => self
                .send_fire(job, session, key, fired_at, ProducerDeliveryMode::Queue)
                .await
                .ok(),
            (None, _) => None,
        }
    }

    /// Fires every enabled job due at `now`. Whether the store failed to
    /// claim a fire, which leaves it due.
    pub(crate) async fn cron_tick(&self, now: Timestamp) -> bool {
        let due = self.inner.crons.due(now);
        let fires = due
            .into_iter()
            .map(|(id, scheduled)| self.fire_scheduled(id, scheduled, now));
        futures_util::future::join_all(fires)
            .await
            .into_iter()
            .any(|failed| failed)
    }

    /// Why a fire of `job` is skipped now (sections 4.2 and 4.3): its
    /// previous fire's prompt still waits in its session, or, for a
    /// new-session job, its previous run session still runs.
    fn skip_reason(&self, job: &StoredJob) -> Option<StoredSkip> {
        if let StoredTarget::Session(session) | StoredTarget::Subagent(session) = &job.target
            && self.held_reason(*session).is_some()
        {
            return Some(StoredSkip::SessionHeld);
        }
        let previous = job
            .history
            .iter()
            .rev()
            .find(|fire| matches!(fire.outcome, StoredOutcome::Delivered))?;
        let session = previous.session_id?;
        let waiting = previous.message_id.is_some_and(|message| {
            self.goal_producer_projection(session)
                .ok()
                .and_then(|projection| {
                    projection
                        .messages
                        .iter()
                        .find(|record| record.message_id == message)
                        .map(|record| !record.consumed && !record.discarded)
                })
                .unwrap_or(false)
        });
        match &job.target {
            StoredTarget::New(_) => {
                let running = self
                    .inner
                    .store
                    .get(session)
                    .is_ok_and(|projection| projection.status == SessionStatus::Running);
                (running || waiting).then_some(StoredSkip::PreviousRunStillRunning)
            }
            StoredTarget::Session(_) | StoredTarget::Subagent(_) => {
                waiting.then_some(StoredSkip::PreviousFireNotDelivered)
            }
        }
    }

    /// Fires `id`'s fire scheduled at `scheduled`: claims it in the
    /// history, as skipped or pending, moves the job to its next fire, and
    /// delivers a pending fire. Whether the store failed to claim it.
    async fn fire_scheduled(&self, id: CronJobId, scheduled: Timestamp, now: Timestamp) -> bool {
        let Some(job) = self.inner.crons.job(&id) else {
            return false;
        };
        // A restart that is due would refuse the fire's run: leave it
        // unclaimed for the restarted daemon, which catches it up within
        // `[cron] missed_grace`.
        if self.restart_due() {
            return false;
        }
        let skip = if self.inner.config.load().runtime.cron.enabled {
            self.skip_reason(&job)
        } else {
            Some(StoredSkip::FiringDisabled)
        };
        let history = self.inner.config.load().runtime.cron.history;
        let claimed = self
            .crons_blocking(move |_, jobs| {
                let Some(index) = jobs.iter().position(|job| job.id == id) else {
                    return Ok(None);
                };
                let job = &mut jobs[index];
                let claimed_before = job
                    .history
                    .iter()
                    .any(|fire| !fire.run_now && fire.scheduled_at == Some(scheduled));
                if !job.enabled || job.next_fire != Some(scheduled) || claimed_before {
                    return Ok(None);
                }
                let fired_at = now;
                job.last_fire = Some(fired_at);
                job.next_fire = job
                    .schedule
                    .next_after(fired_at.max(scheduled), &host_zone());
                if let Some(reason) = skip {
                    job.record(
                        StoredFire::skipped(Some(scheduled), fired_at, reason),
                        history,
                    );
                    finish_if_done(jobs, index);
                    return Ok(None);
                }
                let key = format!("{}:{scheduled}", job.id);
                let mut fire =
                    StoredFire::with_outcome(Some(scheduled), fired_at, StoredOutcome::Pending);
                fire.key = Some(key.clone());
                job.record(fire, history);
                Ok(Some(Claim {
                    job: job.clone(),
                    key,
                    fired_at,
                }))
            })
            .await;
        match claimed {
            Ok(Some(claim)) => {
                let _ = self.deliver_claim(claim).await;
                false
            }
            Ok(None) => false,
            Err(error) => {
                eprintln!("cookie-agent: cron job: {error}");
                true
            }
        }
    }

    /// Delivers a claimed fire and records its outcome. The run the prompt
    /// landed in may have ended before the fire was recorded delivered,
    /// when nothing awaited it yet; its end is recorded now instead.
    async fn deliver_claim(&self, claim: Claim) -> Result<Delivery, EngineError> {
        let result = self.deliver(&claim.job, &claim.key, claim.fired_at).await;
        #[cfg(test)]
        self.wait_for_cron_settle_hook().await;
        let limit = self
            .inner
            .config
            .load()
            .runtime
            .cron
            .max_consecutive_failures;
        let id = claim.job.id.clone();
        let key = claim.key.clone();
        let recorded = result.clone();
        self.crons_blocking(move |_, jobs| {
            settle_fire(jobs, &id, &key, &recorded, limit);
            Ok(())
        })
        .await?;
        if let Ok(Delivery {
            session,
            message: Some(message),
        }) = result
            && let Some(run) = self.landed_run(session, message)
            && let Err(error) = self.settle_cron_runs(session, run).await
        {
            eprintln!("cookie-agent: cron run outcome: {error}");
        }
        result.map_err(EngineError::ToolFailed)
    }

    /// The run producer message `message` of `session` landed in: the run
    /// that consumed it or that it started.
    fn landed_run(&self, session: SessionId, message: ProducerMessageId) -> Option<RunId> {
        let producers = self.goal_producer_projection(session).ok()?;
        let record = producers
            .messages
            .iter()
            .find(|record| record.message_id == message)?;
        record.consumed_run.or(record.admission.map(|(run, _)| run))
    }

    #[cfg(test)]
    pub(crate) fn install_cron_settle_hook(
        &self,
    ) -> (
        tokio::sync::oneshot::Receiver<()>,
        std::sync::Arc<tokio::sync::Notify>,
    ) {
        let (reached, receiver) = tokio::sync::oneshot::channel();
        let release = std::sync::Arc::new(tokio::sync::Notify::new());
        *self
            .inner
            .test_hooks
            .cron_settle_hook
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) =
            Some(std::sync::Arc::new(crate::runtime::PagingRaceHook {
                reached: Mutex::new(Some(reached)),
                release: std::sync::Arc::clone(&release),
            }));
        (receiver, release)
    }

    #[cfg(test)]
    async fn wait_for_cron_settle_hook(&self) {
        let hook = self
            .inner
            .test_hooks
            .cron_settle_hook
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        if let Some(hook) = hook {
            if let Some(reached) = hook
                .reached
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .take()
            {
                let _ = reached.send(());
            }
            hook.release.notified().await;
        }
    }

    /// Delivers `job`'s prompt: into its session, or into a new session
    /// made from its template.
    async fn deliver(
        &self,
        job: &StoredJob,
        key: &str,
        fired_at: Timestamp,
    ) -> Result<Delivery, String> {
        match &job.target {
            StoredTarget::Session(session) | StoredTarget::Subagent(session) => {
                self.send_fire(
                    job,
                    *session,
                    key,
                    fired_at,
                    job.delivery.unwrap_or(ProducerDeliveryMode::Queue),
                )
                .await
            }
            StoredTarget::New(template) => {
                let session = self
                    .create_run_session(job, template, fired_at)
                    .await
                    .map_err(|error| error.to_string())?;
                self.send_fire(job, session, key, fired_at, ProducerDeliveryMode::Queue)
                    .await
            }
        }
    }

    /// Sends `job`'s marked prompt to `session` as a producer message, once
    /// per `key`. The message starts a run on an idle session, waits for
    /// the current turn as `queue`, or joins it as `steer`. A session that
    /// paged out is loaded again.
    pub(crate) async fn send_fire(
        &self,
        job: &StoredJob,
        session: SessionId,
        key: &str,
        fired_at: Timestamp,
        mode: ProducerDeliveryMode,
    ) -> Result<Delivery, String> {
        let authority = ProducerAuthority {
            owner: cron_owner(&job.id),
            connection_epoch: None,
        };
        let key = ProducerIdempotencyKey::new(key.to_owned()).map_err(|error| error.to_string())?;
        let body = render_scheduled_job(job, fired_at);
        let description = crate::runtime::messaging::producers::producer_description(
            "Scheduled job: ",
            &job.label(),
        );
        let producer_id = self
            .register_producer(session, authority.clone())
            .await
            .map_err(|error| error.to_string())?;
        let accepted = self
            .on_actor(session, move |engine| {
                engine.accept_producer_with_output_direct(
                    &authority,
                    ProducerSendParams {
                        session_id: session,
                        producer_id,
                        mode,
                        idempotency_key: key,
                        description,
                        body,
                    },
                    None,
                    None,
                )
            })
            .await;
        self.drop_producer_registration(session, producer_id);
        accepted
            .map(|message| Delivery {
                session,
                message: Some(message),
            })
            .map_err(|error| error.to_string())
    }

    /// The model selection a run of `template`'s agent starts with: its
    /// pin, else the first available model of the agent's chain.
    fn template_selection(&self, template: &StoredTemplate) -> Result<RunSelection, EngineError> {
        let runtime = self.current_runtime();
        let agents = runtime.agents_for_preset(template.preset.as_deref())?;
        let agent = crate::policy::resolve_agent(&agents, &template.agent)?;
        if !agent.runnable_as_root {
            return Err(EngineError::DisabledAgent(template.agent.clone()));
        }
        // An ACP agent chooses its own model.
        if agent.acp.is_some() {
            return Ok(RunSelection {
                agent: template.agent.clone(),
                model: cookie_agent_protocol::acp_model_selection(&template.agent),
                preset: template.preset.clone(),
            });
        }
        let model = match &template.model {
            Some(model) => model.clone(),
            None => agent
                .resolved_fallback
                .iter()
                .find_map(|fallback| match fallback {
                    crate::runtime_snapshot::ResolvedAgentFallback::Selection {
                        selection, ..
                    } if crate::runtime_snapshot::selection_available(
                        &runtime.models,
                        selection,
                    ) =>
                    {
                        Some(selection.clone())
                    }
                    _ => None,
                })
                .ok_or(EngineError::NoRunnableModel)?,
        };
        Ok(RunSelection {
            agent: template.agent.clone(),
            model: ModelSelection {
                model: model.model,
                variant: model.variant,
            },
            preset: template.preset.clone(),
        })
    }

    /// Creates the root session a new-session fire runs in (section 4.3):
    /// from the template, in its project, with the job's permission mode
    /// and the template's rules as its overlay, titled for the fire, and
    /// recording the job.
    async fn create_run_session(
        &self,
        job: &StoredJob,
        template: &StoredTemplate,
        fired_at: Timestamp,
    ) -> Result<SessionId, EngineError> {
        self.check_cron_template(template)?;
        let selection = self.template_selection(template)?;
        let runtimes = crate::runtime::attachments::SessionRuntimes {
            runtimes: template.runtimes.as_ref().map(|requests| {
                requests
                    .iter()
                    .map(
                        |request| crate::runtime::attachments::SessionRuntimeRequest {
                            runtime_id: request.runtime_id.to_string(),
                            capability: request.capability.as_ref().map(ToString::to_string),
                            workspace: request.workspace.as_ref().map(std::path::PathBuf::from),
                        },
                    )
                    .collect()
            }),
            global: template.global,
            cwd: template.cwd.as_ref().map(std::path::PathBuf::from),
        };
        let title = run_title(job, fired_at).map_err(EngineError::ToolFailed)?;
        let meta = self
            .create_root_session(
                selection,
                Some(job.permission_mode.unwrap_or(PermissionMode::AutoApproveN)),
                template.permission_rules.clone(),
                runtimes,
                template.project.clone(),
                Some((
                    SessionCron {
                        job_id: job.id.clone(),
                        fired_at,
                    },
                    title,
                )),
            )
            .await?;
        Ok(meta.session_id)
    }

    /// `crons.run`, or an agent's `cron_run` of an in-session job: fires
    /// `id` now, as a scheduled fire would, recorded as a run now that
    /// counts toward neither `max_runs` nor the failure count and moves
    /// nothing else.
    pub(crate) async fn run_cron_now(&self, id: CronJobId) -> Result<CronRunResult, EngineError> {
        self.ensure_not_shutting_down()?;
        let job = self
            .inner
            .crons
            .job(&id)
            .ok_or_else(|| CronError::NotFound(id.clone()))?;
        let skip = self.skip_reason(&job);
        let history = self.inner.config.load().runtime.cron.history;
        let now = Timestamp::now();
        let claim_key = format!("{}:now:{}", job.id, uuid::Uuid::now_v7());
        let claim = self
            .crons_blocking(move |_, jobs| {
                let job = jobs
                    .iter_mut()
                    .find(|job| job.id == id)
                    .ok_or_else(|| CronError::NotFound(id.clone()))?;
                let mut fire = match skip {
                    Some(reason) => StoredFire::skipped(None, now, reason),
                    None => StoredFire::with_outcome(None, now, StoredOutcome::Pending),
                };
                fire.run_now = true;
                fire.uncounted = true;
                if skip.is_none() {
                    fire.key = Some(claim_key.clone());
                }
                job.record(fire, history);
                Ok(skip.is_none().then(|| Claim {
                    job: job.clone(),
                    key: claim_key,
                    fired_at: now,
                }))
            })
            .await?;
        let session_id = match claim {
            Some(claim) => Some(self.deliver_claim(claim).await?.session),
            None => None,
        };
        let job = self
            .inner
            .crons
            .job(&job.id)
            .map(|job| job.wire())
            .unwrap_or_else(|| job.wire());
        Ok(CronRunResult { job, session_id })
    }

    /// Records the start of an agent's run now of a new-session job, whose
    /// child session `child` takes the prompt as its first input.
    pub(crate) async fn record_cron_child(
        &self,
        id: CronJobId,
        child: SessionId,
    ) -> Result<(), EngineError> {
        let history = self.inner.config.load().runtime.cron.history;
        let now = Timestamp::now();
        self.crons_blocking(move |_, jobs| {
            if let Some(job) = jobs.iter_mut().find(|job| job.id == id) {
                let mut fire = StoredFire::with_outcome(None, now, StoredOutcome::Delivered);
                fire.run_now = true;
                fire.uncounted = true;
                fire.session_id = Some(child);
                job.record(fire, history);
            }
            Ok(())
        })
        .await
    }

    /// Called once a run of `session` ended: a fire whose prompt landed in
    /// it records how it ended, and counts it.
    pub(in crate::runtime) fn observe_cron_run(&self, session: SessionId, run: RunId) {
        if !self.inner.crons.awaits_run_in(session) {
            return;
        }
        let Some(runtime) = self
            .inner
            .runtime
            .clone()
            .or_else(|| tokio::runtime::Handle::try_current().ok())
        else {
            return;
        };
        let engine = self.clone();
        let _ = self.spawn_admission_task(&runtime, async move {
            if let Err(error) = engine.settle_cron_runs(session, run).await {
                eprintln!("cookie-agent: cron run outcome: {error}");
            }
        });
    }

    /// Records the end of `run` of `session` on every fire whose prompt it
    /// took (section 4.4): a failed run counts a failure, a completed one
    /// resets the count, a cancelled or interrupted one changes nothing.
    /// A fire without a message, an agent's run now in a child session,
    /// takes the session's first run that ends.
    pub(crate) async fn settle_cron_runs(
        &self,
        session: SessionId,
        run: RunId,
    ) -> Result<(), EngineError> {
        let projection = self.inner.store.get(session)?;
        let Some(status) = projection.runs.get(&run).map(|run| run.status) else {
            return Ok(());
        };
        if !matches!(
            status,
            SessionStatus::Completed
                | SessionStatus::Failed
                | SessionStatus::Cancelled
                | SessionStatus::Interrupted
        ) {
            return Ok(());
        }
        let producers = self.goal_producer_projection(session)?;
        let landed = move |message: Option<ProducerMessageId>| {
            message.is_none_or(|message| {
                producers
                    .messages
                    .iter()
                    .find(|record| record.message_id == message)
                    .is_some_and(|record| {
                        record.consumed_run == Some(run)
                            || record
                                .admission
                                .is_some_and(|(admitted, _)| admitted == run)
                    })
            })
        };
        let limit = self
            .inner
            .config
            .load()
            .runtime
            .cron
            .max_consecutive_failures;
        let at = Timestamp::now();
        self.crons_blocking(move |_, jobs| {
            record_run_end(
                jobs,
                session,
                run,
                StoredRunEnd { status, at },
                landed,
                limit,
            );
            Ok(())
        })
        .await
    }
}
