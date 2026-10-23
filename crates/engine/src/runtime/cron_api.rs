//! Cron job requests (`docs/site/specs/cron.md` sections 3, 6, 7, and 9):
//! listing, creating, changing, and deleting jobs, for clients and for the
//! cron tools, and the checks every definition passes.
//!
//! A client may set anything. An agent's request never names a permission
//! mode or a model (section 7), and its targets are checked by the tools
//! before they reach here.

use std::collections::HashSet;

use cookie_agent_protocol::{
    CronActor, CronAgentActor, CronCreateParams, CronJob, CronJobId, CronMaxRuns, CronSchedule,
    CronUpdateParams, CronsListResult, PermissionMode, ProducerDeliveryMode, ProjectId, SessionId,
    SessionOrigin,
};
use jiff::{Timestamp, tz::TimeZone};

use super::{Engine, EngineError};
use crate::crons::{
    Changed, CronError, CronStore, StoredDisabled, StoredJob, StoredSchedule, StoredTarget,
    StoredTemplate, new_job_id, schedule,
};

/// Who makes a cron request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum CronRequester {
    /// You, through a client.
    User,
    /// An agent, from a session.
    Agent(CronAgentActor),
}

impl CronRequester {
    pub(crate) fn actor(&self) -> CronActor {
        match self {
            Self::User => CronActor::User,
            Self::Agent(agent) => CronActor::Agent(agent.clone()),
        }
    }
}

/// The time zone schedules are evaluated in: the daemon host's.
pub(crate) fn host_zone() -> TimeZone {
    TimeZone::system()
}

/// A job's definition after its checks: what a create sets and an update
/// replaces.
#[derive(Clone, Debug)]
struct Definition {
    name: Option<String>,
    prompt: String,
    schedule: StoredSchedule,
    max_runs: CronMaxRuns,
    target: StoredTarget,
    delivery: Option<ProducerDeliveryMode>,
    permission_mode: Option<PermissionMode>,
    max_consecutive_failures: Option<u32>,
}

fn check_name(name: Option<String>) -> Result<Option<String>, CronError> {
    let Some(name) = name else {
        return Ok(None);
    };
    let name = name.trim().to_owned();
    if name.is_empty() {
        return Err(CronError::invalid("name", "must not be blank"));
    }
    if name.chars().count() > cookie_agent_protocol::CRON_NAME_MAX_CHARS {
        return Err(CronError::invalid(
            "name",
            format!(
                "must be at most {} characters",
                cookie_agent_protocol::CRON_NAME_MAX_CHARS
            ),
        ));
    }
    if name.chars().any(char::is_control) {
        return Err(CronError::invalid(
            "name",
            "must not contain control characters",
        ));
    }
    Ok(Some(name))
}

fn check_prompt(prompt: String) -> Result<String, CronError> {
    if prompt.trim().is_empty() {
        return Err(CronError::invalid("prompt", "must not be blank"));
    }
    if prompt.len() > cookie_agent_protocol::CRON_PROMPT_MAX_BYTES {
        return Err(CronError::invalid(
            "prompt",
            format!(
                "must be at most {} bytes",
                cookie_agent_protocol::CRON_PROMPT_MAX_BYTES
            ),
        ));
    }
    Ok(prompt)
}

/// The schedule to store for `schedule` given at `now`: `in` becomes `at`.
pub(crate) fn resolve_schedule(
    schedule: CronSchedule,
    now: Timestamp,
) -> Result<StoredSchedule, CronError> {
    match schedule {
        CronSchedule::Cron(expression) => {
            schedule::CronExpr::parse(&expression)
                .map_err(|reason| CronError::invalid("schedule", reason))?;
            Ok(StoredSchedule::Cron(
                expression.split_whitespace().collect::<Vec<_>>().join(" "),
            ))
        }
        CronSchedule::In(delay) => {
            let delay = schedule::parse_delay(&delay)
                .map_err(|reason| CronError::invalid("schedule", reason))?;
            schedule::delay_after(now, delay)
                .map(StoredSchedule::At)
                .map_err(|reason| CronError::invalid("schedule", reason))
        }
        CronSchedule::At(at) if at > now => Ok(StoredSchedule::At(at)),
        CronSchedule::At(at) => Err(CronError::invalid(
            "schedule",
            format!("{at} is not in the future"),
        )),
    }
}

/// Refuses what only a client may set in an agent's request: a permission
/// mode, or a model pin in a new-session template.
fn refuse_client_fields(
    permission_mode: bool,
    target: Option<&cookie_agent_protocol::CronTarget>,
) -> Result<(), CronError> {
    if permission_mode {
        return Err(CronError::invalid(
            "permission_mode",
            "only a client can set a job's permission mode",
        ));
    }
    if let Some(cookie_agent_protocol::CronTarget::New(template)) = target
        && template.model.is_some()
    {
        return Err(CronError::invalid(
            "target",
            "only a client can pin a job's model",
        ));
    }
    Ok(())
}

/// Whether `target`'s session exists and has the kind the target names.
fn check_session_target(engine: &Engine, target: &StoredTarget) -> Result<(), CronError> {
    let Some(session) = target.session() else {
        return Ok(());
    };
    let summary = engine
        .inner
        .store
        .summary(session)
        .map_err(|_| CronError::invalid("target", format!("there is no session {session}")))?;
    match (target, &summary.meta.origin) {
        (StoredTarget::Session(_), SessionOrigin::Root)
        | (StoredTarget::Subagent(_), SessionOrigin::Delegated { .. }) => Ok(()),
        (StoredTarget::Session(_), _) => Err(CronError::invalid(
            "target",
            format!("session {session} is a subagent's session; target it as `subagent`"),
        )),
        _ => Err(CronError::invalid(
            "target",
            format!("session {session} is a root session; target it as `session`"),
        )),
    }
}

impl Engine {
    /// Checks a new-session template as `session.create` would check it:
    /// the agent exists in its preset, is enabled, and can run as a root;
    /// a pinned model is available; the runtimes are known; the project
    /// exists; the rules are valid.
    pub(crate) fn check_cron_template(&self, template: &StoredTemplate) -> Result<(), CronError> {
        let refused = |reason: String| CronError::invalid("target", reason);
        let runtime = self.current_runtime();
        let agents = runtime
            .agents_for_preset(template.preset.as_deref())
            .map_err(|error| refused(error.to_string()))?;
        let agent = agents
            .get(&template.agent)
            .ok_or_else(|| refused(format!("there is no agent `{}`", template.agent)))?;
        if !agent.document.frontmatter.enabled {
            return Err(refused(format!("agent `{}` is disabled", template.agent)));
        }
        if !matches!(
            agent.document.frontmatter.mode,
            cookie_agent_config::AgentMode::Primary | cookie_agent_config::AgentMode::All
        ) {
            return Err(refused(format!(
                "agent `{}` cannot run as a root session (a new-session job's agent must be `primary` or `all`)",
                template.agent
            )));
        }
        if let Some(model) = &template.model
            && !crate::runtime_snapshot::selection_available(&runtime.models, model)
        {
            return Err(refused(format!("model `{}` is not available", model.model)));
        }
        for request in template.runtimes.iter().flatten() {
            let id = request.runtime_id.as_str();
            if !self.inner.runtimes.knows(id)
                && self.inner.machines.record(&request.runtime_id).is_none()
            {
                return Err(refused(format!("there is no runtime `{id}`")));
            }
        }
        if let Some(project) = &template.project {
            self.inner
                .projects
                .get(project)
                .map_err(|error| refused(error.to_string()))?;
        }
        crate::permissions::check_rule_selectors(&template.permission_rules)
            .map_err(|error| refused(error.to_string()))?;
        cookie_agent_protocol::SessionPermissionOverlay {
            rules: template.permission_rules.clone(),
        }
        .validate()
        .map_err(|error| refused(error.to_string()))?;
        Ok(())
    }

    /// Checks a definition and fills in the defaults of its kind.
    #[allow(clippy::too_many_arguments)]
    fn check_definition(
        &self,
        name: Option<String>,
        prompt: String,
        schedule: StoredSchedule,
        max_runs: Option<CronMaxRuns>,
        target: StoredTarget,
        delivery: Option<ProducerDeliveryMode>,
        permission_mode: Option<PermissionMode>,
        max_consecutive_failures: Option<u32>,
    ) -> Result<Definition, CronError> {
        let max_runs = match (&schedule, max_runs) {
            (StoredSchedule::At(_), Some(max_runs)) if max_runs != CronMaxRuns::Count(1) => {
                return Err(CronError::invalid(
                    "max_runs",
                    "a one-shot schedule fires once; max_runs applies to cron schedules",
                ));
            }
            (_, max_runs) => max_runs.unwrap_or_default(),
        };
        if max_consecutive_failures == Some(0) {
            return Err(CronError::invalid(
                "max_consecutive_failures",
                "must be at least 1",
            ));
        }
        check_session_target(self, &target)?;
        let (delivery, permission_mode) = match &target {
            StoredTarget::New(template) => {
                self.check_cron_template(template)?;
                if delivery.is_some() {
                    return Err(CronError::invalid(
                        "delivery",
                        "a new-session job starts a session; delivery applies to in-session jobs",
                    ));
                }
                (
                    None,
                    Some(permission_mode.unwrap_or(PermissionMode::AutoApproveN)),
                )
            }
            StoredTarget::Session(_) | StoredTarget::Subagent(_) => {
                if permission_mode.is_some() {
                    return Err(CronError::invalid(
                        "permission_mode",
                        "an in-session job runs with its session's permission mode",
                    ));
                }
                (Some(delivery.unwrap_or(ProducerDeliveryMode::Queue)), None)
            }
        };
        Ok(Definition {
            name: check_name(name)?,
            prompt: check_prompt(prompt)?,
            schedule,
            max_runs,
            target,
            delivery,
            permission_mode,
            max_consecutive_failures,
        })
    }

    /// Runs `change` on the cron store off the async workers, then
    /// announces it: the sessions whose jobs changed refresh what they wait
    /// on, and the scheduler looks again.
    pub(crate) async fn crons_blocking<T: Send + 'static>(
        &self,
        change: impl FnOnce(&Engine, &mut Vec<StoredJob>) -> Result<T, CronError> + Send + 'static,
    ) -> Result<T, EngineError> {
        let engine = self.clone();
        let (value, changed) = tokio::task::spawn_blocking(move || {
            let store = std::sync::Arc::clone(&engine.inner.crons);
            store.mutate(|jobs| change(&engine, jobs))
        })
        .await
        .map_err(|error| CronError::Store(format!("the request did not finish: {error}")))??;
        self.announce_cron_change(changed);
        Ok(value)
    }

    /// [`Self::crons_blocking`] for a caller that is not async.
    pub(crate) fn crons_now<T>(
        &self,
        change: impl FnOnce(&Engine, &mut Vec<StoredJob>) -> Result<T, CronError>,
    ) -> Result<T, EngineError> {
        let (value, changed) =
            crate::blocking::off_worker(|| self.inner.crons.mutate(|jobs| change(self, jobs)))?;
        self.announce_cron_change(changed);
        Ok(value)
    }

    fn announce_cron_change(&self, changed: Changed) {
        for session in changed.sessions {
            self.refresh_live_waiting(session);
        }
        self.inner.cron_scheduler.wake.notify_one();
    }

    /// The frozen policy of the active run `run`.
    pub(crate) fn active_run_policy(
        &self,
        run: cookie_agent_protocol::RunId,
    ) -> Option<std::sync::Arc<crate::policy::FrozenRunPolicy>> {
        self.inner
            .sessions
            .active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&run)
            .map(|active| std::sync::Arc::clone(&active.policy))
    }

    pub(crate) fn cron_store(&self) -> &CronStore {
        &self.inner.crons
    }

    /// Signals every change to the cron jobs.
    #[must_use]
    pub fn subscribe_cron_changes(&self) -> tokio::sync::watch::Receiver<u64> {
        self.inner.crons.subscribe()
    }

    /// `crons.list`: every job, in creation order.
    #[must_use]
    pub fn list_crons(&self) -> CronsListResult {
        CronsListResult {
            jobs: self
                .inner
                .crons
                .jobs()
                .iter()
                .map(StoredJob::wire)
                .collect(),
            firing: self.inner.config.load().runtime.cron.enabled,
        }
    }

    /// `crons.create` as you.
    pub async fn create_cron(&self, params: CronCreateParams) -> Result<CronJob, EngineError> {
        self.create_cron_as(CronRequester::User, params).await
    }

    /// `crons.update` as you.
    pub async fn update_cron(&self, params: CronUpdateParams) -> Result<CronJob, EngineError> {
        self.update_cron_as(CronRequester::User, params).await
    }

    /// `crons.run`: fires a job now.
    pub async fn run_cron(
        &self,
        id: CronJobId,
    ) -> Result<cookie_agent_protocol::CronRunResult, EngineError> {
        self.run_cron_now(id).await
    }

    /// `crons.delete`.
    pub async fn delete_cron(&self, id: CronJobId) -> Result<(), EngineError> {
        self.crons_blocking(move |_, jobs| {
            let before = jobs.len();
            jobs.retain(|job| job.id != id);
            if jobs.len() == before {
                return Err(CronError::NotFound(id));
            }
            Ok(())
        })
        .await
    }

    /// Creates a job for `requester`.
    pub(crate) async fn create_cron_as(
        &self,
        requester: CronRequester,
        params: CronCreateParams,
    ) -> Result<CronJob, EngineError> {
        self.ensure_not_shutting_down()?;
        let now = Timestamp::now();
        let schedule = resolve_schedule(params.schedule, now)?;
        if matches!(requester, CronRequester::Agent(_)) {
            refuse_client_fields(params.permission_mode.is_some(), Some(&params.target))?;
        }
        self.crons_blocking(move |engine, jobs| {
            let definition = engine.check_definition(
                params.name,
                params.prompt,
                schedule,
                params.max_runs,
                params.target.into(),
                params.delivery,
                params.permission_mode,
                params.max_consecutive_failures,
            )?;
            let enabled = params.enabled.unwrap_or(true);
            let next_fire = definition.schedule.next_after(now, &host_zone());
            if enabled && next_fire.is_none() {
                return Err(CronError::invalid(
                    "schedule",
                    "the schedule has no next fire",
                ));
            }
            let job = StoredJob {
                id: new_job_id(jobs),
                name: definition.name,
                prompt: definition.prompt,
                schedule: definition.schedule,
                max_runs: definition.max_runs,
                target: definition.target,
                delivery: definition.delivery,
                permission_mode: definition.permission_mode,
                max_consecutive_failures: definition.max_consecutive_failures,
                enabled,
                disabled_reason: (!enabled).then(|| StoredDisabled::Disabled {
                    by: requester.actor(),
                }),
                created_by: requester.actor(),
                created_at: now,
                next_fire: next_fire.filter(|_| enabled),
                runs: 0,
                consecutive_failures: 0,
                last_fire: None,
                history: Vec::new(),
            };
            let wire = job.wire();
            jobs.push(job);
            Ok(wire)
        })
        .await
    }

    /// Changes a job for `requester`. A new schedule, a new target, or
    /// enabling the job computes its next fire from now; enabling also
    /// resets its failure count, and a finished job's runs.
    pub(crate) async fn update_cron_as(
        &self,
        requester: CronRequester,
        params: CronUpdateParams,
    ) -> Result<CronJob, EngineError> {
        self.ensure_not_shutting_down()?;
        let now = Timestamp::now();
        let schedule = params
            .schedule
            .clone()
            .map(|schedule| resolve_schedule(schedule, now))
            .transpose()?;
        // An agent may keep a mode or a model pin you set; it may not set
        // one.
        if matches!(requester, CronRequester::Agent(_)) {
            refuse_client_fields(params.permission_mode.is_some(), params.target.as_ref())?;
        }
        self.crons_blocking(move |engine, jobs| {
            let job = jobs
                .iter_mut()
                .find(|job| job.id == params.id)
                .ok_or_else(|| CronError::NotFound(params.id.clone()))?;
            let target: StoredTarget = params
                .target
                .clone()
                .map_or_else(|| job.target.clone(), Into::into);
            let kind_changed = target.kind() != job.target.kind();
            let in_session = target.session().is_some();
            // A delivery or mode the new kind has no use for goes with the
            // old kind, unless the update names one.
            let delivery = params.delivery.or(if in_session && !kind_changed {
                job.delivery
            } else {
                None
            });
            let permission_mode = match params.permission_mode {
                Some(mode) => mode,
                None if !in_session && !kind_changed => job.permission_mode,
                None => None,
            };
            let schedule_changed = schedule.is_some();
            let new_schedule = schedule.unwrap_or_else(|| job.schedule.clone());
            let max_runs = match params.max_runs {
                Some(max_runs) => Some(max_runs),
                None if new_schedule.is_one_shot() => None,
                None => Some(job.max_runs),
            };
            let definition = engine.check_definition(
                match params.name.clone() {
                    Some(name) => name,
                    None => job.name.clone(),
                },
                params.prompt.clone().unwrap_or_else(|| job.prompt.clone()),
                new_schedule,
                max_runs,
                target,
                delivery,
                permission_mode,
                match params.max_consecutive_failures {
                    Some(limit) => limit,
                    None => job.max_consecutive_failures,
                },
            )?;
            let target_changed = definition.target != job.target;
            job.name = definition.name;
            job.prompt = definition.prompt;
            job.schedule = definition.schedule;
            job.max_runs = definition.max_runs;
            job.target = definition.target;
            job.delivery = definition.delivery;
            job.permission_mode = definition.permission_mode;
            job.max_consecutive_failures = definition.max_consecutive_failures;
            match params.enabled {
                Some(false) => {
                    if job.enabled {
                        job.disable(StoredDisabled::Disabled {
                            by: requester.actor(),
                        });
                    }
                }
                Some(true) if !job.enabled => {
                    if matches!(job.disabled_reason, Some(StoredDisabled::Finished)) {
                        job.runs = 0;
                    }
                    job.enabled = true;
                    job.disabled_reason = None;
                    job.consecutive_failures = 0;
                    job.next_fire = job.schedule.next_after(now, &host_zone());
                    if job.next_fire.is_none() {
                        return Err(CronError::invalid(
                            "schedule",
                            "the job's time has passed; give it a new schedule to enable it",
                        ));
                    }
                }
                _ => {
                    if job.enabled && (schedule_changed || target_changed) {
                        job.next_fire = job.schedule.next_after(now, &host_zone());
                        if job.next_fire.is_none() {
                            return Err(CronError::invalid(
                                "schedule",
                                "the schedule has no next fire",
                            ));
                        }
                    }
                }
            }
            Ok(job.wire())
        })
        .await
    }

    /// Deletes the jobs that target any of `deleted`, which are deleted
    /// sessions (`docs/site/specs/cron.md` section 6).
    pub(crate) fn delete_crons_targeting(&self, deleted: &HashSet<SessionId>) {
        if !self.inner.crons.jobs().iter().any(|job| {
            job.target
                .session()
                .is_some_and(|session| deleted.contains(&session))
        }) {
            return;
        }
        let deleted = deleted.clone();
        if let Err(error) = self.crons_now(move |_, jobs| {
            jobs.retain(|job| {
                !job.target
                    .session()
                    .is_some_and(|session| deleted.contains(&session))
            });
            Ok(())
        }) {
            eprintln!("cookie-agent: cron jobs of deleted sessions: {error}");
        }
    }

    /// Deletes the new-session jobs whose template names `project`, which
    /// is being deleted.
    pub(crate) async fn delete_crons_of_project(
        &self,
        project: ProjectId,
    ) -> Result<(), EngineError> {
        self.crons_blocking(move |_, jobs| {
            jobs.retain(|job| {
                job.target
                    .template()
                    .is_none_or(|template| template.project.as_ref() != Some(&project))
            });
            Ok(())
        })
        .await
    }
}
