//! A session's cron jobs and webhooks (`docs/site/specs/session-schedules.md`):
//! what fires or delivers into it, what an agent made in it, and what
//! started it; and the clock the open session's title bar shows for its
//! next fire.

use cookie_agent_protocol::{
    CronActor, CronFireOutcome, CronJob, CronTarget, Open, SessionId, SessionMeta, Webhook,
    WebhookTarget,
};
use jiff::Timestamp;

/// A session's things in the three groups its panel tab lists, each in
/// the daemon's order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionGroups<T> {
    /// Those that fire or deliver into the session.
    pub into: Vec<T>,
    /// Those an agent made in the session that start sessions of their own
    /// (or go into another one).
    pub made_here: Vec<T>,
    /// The one whose fire or event started the session.
    pub started_by: Option<T>,
}

impl<T> SessionGroups<T> {
    pub fn is_empty(&self) -> bool {
        self.into.is_empty() && self.made_here.is_empty() && self.started_by.is_none()
    }

    /// How many there are, each counted once.
    pub fn len(&self) -> usize {
        self.into.len() + self.made_here.len() + usize::from(self.started_by.is_some())
    }
}

fn made_in(actor: &CronActor, session: SessionId) -> bool {
    matches!(actor, CronActor::Agent(agent) if agent.session == session)
}

/// `session`'s cron jobs. A job falls in the first group it fits.
pub fn session_jobs(session: &SessionMeta, jobs: &[CronJob]) -> SessionGroups<CronJob> {
    let id = session.session_id;
    let started = session.cron.as_ref().map(|cron| &cron.job_id);
    let mut groups = SessionGroups {
        into: Vec::new(),
        made_here: Vec::new(),
        started_by: None,
    };
    for job in jobs {
        if job_fires_into(job, id) {
            groups.into.push(job.clone());
        } else if started == Some(&job.id) {
            groups.started_by = Some(job.clone());
        } else if made_in(&job.created_by, id) {
            groups.made_here.push(job.clone());
        }
    }
    groups
}

/// Whether `job` fires into session `id`.
pub fn job_fires_into(job: &CronJob, id: SessionId) -> bool {
    matches!(job.target, CronTarget::Session(target) | CronTarget::Subagent(target) if target == id)
}

/// `session`'s webhooks. A webhook falls in the first group it fits.
pub fn session_webhooks(session: &SessionMeta, webhooks: &[Webhook]) -> SessionGroups<Webhook> {
    let id = session.session_id;
    let started = session.webhook.as_ref().map(|webhook| &webhook.webhook_id);
    let mut groups = SessionGroups {
        into: Vec::new(),
        made_here: Vec::new(),
        started_by: None,
    };
    for webhook in webhooks {
        let into = matches!(
            webhook.target,
            WebhookTarget::Session(target) | WebhookTarget::Subagent(target) if target == id
        );
        if into {
            groups.into.push(webhook.clone());
        } else if started == Some(&webhook.id) {
            groups.started_by = Some(webhook.clone());
        } else if made_in(&webhook.created_by, id) {
            groups.made_here.push(webhook.clone());
        }
    }
    groups
}

/// `jobs` with `session`'s own (in any of its groups) first, each part in
/// its order: the terminal UI's `/cron` (D4).
pub fn jobs_own_first(session: &SessionMeta, jobs: Vec<CronJob>) -> Vec<CronJob> {
    let groups = session_jobs(session, &jobs);
    let own = |job: &CronJob| {
        groups.into.iter().any(|kept| kept.id == job.id)
            || groups.made_here.iter().any(|kept| kept.id == job.id)
            || groups
                .started_by
                .as_ref()
                .is_some_and(|kept| kept.id == job.id)
    };
    let (mut first, rest): (Vec<_>, Vec<_>) = jobs.into_iter().partition(|job| own(job));
    first.extend(rest);
    first
}

/// `webhooks` with `session`'s own first: the terminal UI's `/webhooks`.
pub fn webhooks_own_first(session: &SessionMeta, webhooks: Vec<Webhook>) -> Vec<Webhook> {
    let groups = session_webhooks(session, &webhooks);
    let own = |webhook: &Webhook| {
        groups.into.iter().any(|kept| kept.id == webhook.id)
            || groups.made_here.iter().any(|kept| kept.id == webhook.id)
            || groups
                .started_by
                .as_ref()
                .is_some_and(|kept| kept.id == webhook.id)
    };
    let (mut first, rest): (Vec<_>, Vec<_>) =
        webhooks.into_iter().partition(|webhook| own(webhook));
    first.extend(rest);
    first
}

/// What the title bar's clock says (D1).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NextFire {
    /// A fire was delivered and waits behind the running turn.
    pub queued: bool,
    /// The soonest next fire, when one is due.
    pub at: Option<Timestamp>,
    /// How many jobs fire into the session.
    pub jobs: usize,
    /// Each job's title and its next fire, soonest first, for the tooltip.
    pub each: Vec<(String, Option<Timestamp>)>,
}

/// The clock for session `id`, or `None` while no enabled job fires into
/// it (and none of its fires waits there).
pub fn next_fire(id: SessionId, jobs: &[CronJob]) -> Option<NextFire> {
    let into: Vec<&CronJob> = jobs.iter().filter(|job| job_fires_into(job, id)).collect();
    let queued = into.iter().any(|job| {
        job.history.last().is_some_and(|fire| {
            fire.run_id.is_none()
                && matches!(
                    fire.outcome,
                    Open::Known(CronFireOutcome::Pending | CronFireOutcome::Delivered)
                )
        })
    });
    let mut each: Vec<(String, Option<Timestamp>)> = into
        .iter()
        .filter(|job| job.enabled && job.next_fire.is_some())
        .map(|job| (crate::cron::job_title(job), job.next_fire))
        .collect();
    if each.is_empty() && !queued {
        return None;
    }
    each.sort_by_key(|(_, at)| *at);
    Some(NextFire {
        queued,
        at: each.first().and_then(|(_, at)| *at),
        jobs: each.len(),
        each,
    })
}

/// The clock's text: `queued`, else the time to the next fire as a badge
/// has room for (`now`, `40s`, `12m`, `3h`, `2d`), with `· 2` for two jobs.
pub fn next_fire_text(next: &NextFire, now: Timestamp) -> String {
    let when = if next.queued {
        "queued".to_owned()
    } else {
        next.at.map_or_else(String::new, |at| short_until(at, now))
    };
    if next.jobs > 1 {
        format!("{when} · {}", next.jobs)
    } else {
        when
    }
}

/// How long until `then`, in a badge's few characters.
pub fn short_until(then: Timestamp, now: Timestamp) -> String {
    let seconds = then.as_second() - now.as_second();
    match seconds {
        ..=0 => "now".to_owned(),
        1..60 => format!("{seconds}s"),
        60..3600 => format!("{}m", seconds / 60),
        3600..86_400 => format!("{}h", seconds / 3600),
        _ => format!("{}d", seconds / 86_400),
    }
}

#[cfg(test)]
#[path = "session_schedules_tests.rs"]
mod tests;
