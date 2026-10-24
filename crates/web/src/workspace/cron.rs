//! The Scheduled prompts view's calls (`crons.*`, client protocol 1.15):
//! the listing, kept fresh by `crons.changed`, the job form, and a job's
//! actions. The view is `crate::views::cron`.

use cookie_agent_client_state::cron::{CronDraft, job_title, update_changes_anything};
use cookie_agent_protocol::{
    CronJob, CronJobId, CronTemplate, CronUpdateParams, SessionId, SessionOrigin,
};
use leptos::{prelude::*, task::spawn_local};

use super::Workspace;
use crate::{
    cron::{new_template, session_target, zone_at},
    layout::View,
};

/// The job form: what it edits and the targets it switches between.
#[derive(Clone, Debug, PartialEq)]
pub struct CronForm {
    /// The job an edit changes; `None` for a new job.
    pub editing: Option<CronJob>,
    /// The form as it opened.
    pub draft: CronDraft,
    /// The session "this session" is, and whether it is a subagent's;
    /// `None` when the form opened without one.
    pub session: Option<(SessionId, bool)>,
    /// The template "a new session each run" starts from.
    pub template: CronTemplate,
}

impl Workspace {
    /// Reads `crons.list`, when the daemon keeps jobs.
    pub fn poll_crons(self) {
        if !self.has_crons.get_untracked() {
            return;
        }
        let Some(client) = self.client() else {
            return;
        };
        let read = self.begin_read(|reads| &mut reads.crons);
        spawn_local(async move {
            let result = client
                .list_crons()
                .await
                .map_err(|error| crate::errors::describe_full(&error));
            if self.read_is_current(|reads| &mut reads.crons, read) {
                self.crons.set(Some(result));
            }
        });
    }

    /// Opening the view: the list, with no form open.
    pub(super) fn open_crons(self) {
        self.cron_form.set(None);
        self.cron_error.set(None);
        self.poll_crons();
    }

    /// Opens the view at job `id`: a run session's clock mark.
    pub fn open_cron_job(self, id: CronJobId) {
        self.open_view(View::Cron, None);
        self.cron_selected.set(Some(id));
    }

    /// The jobs listed, if they are.
    pub fn cron_jobs(self) -> Vec<CronJob> {
        self.crons.with(|crons| {
            crons
                .as_ref()
                .and_then(|crons| crons.as_ref().ok())
                .map(|crons| crons.jobs.clone())
                .unwrap_or_default()
        })
    }

    /// The open session as a job's target, when one is open.
    fn cron_session(self) -> Option<(SessionId, bool)> {
        if self.new_session.get_untracked() {
            return None;
        }
        self.sessions.with_untracked(|sessions| {
            session_target(
                sessions,
                self.selected.get_untracked(),
                self.tree_root.get_untracked(),
            )
        })
    }

    /// The template a new-session job starts from: the open session's
    /// agent (else the new session's), the preset in use, and the open
    /// session's project (else the new session's).
    fn cron_template(self) -> Option<CronTemplate> {
        let agent = self
            .draft
            .get_untracked()
            .map(|draft| draft.agent)
            .or_else(|| {
                untrack(|| self.selectable_agents())
                    .into_iter()
                    .next()
                    .map(|agent| agent.id)
            })?;
        let selected = self.selected.get_untracked();
        let project = self
            .sessions
            .with_untracked(|sessions| {
                let session = sessions
                    .iter()
                    .find(|session| Some(session.session_id) == selected)?;
                let root = match &session.origin {
                    SessionOrigin::Root => session,
                    SessionOrigin::Delegated {
                        root_session_id, ..
                    } => sessions
                        .iter()
                        .find(|candidate| candidate.session_id == *root_session_id)?,
                };
                root.project.clone()
            })
            .filter(|_| !self.new_session.get_untracked())
            .or_else(|| self.new_project.get_untracked());
        Some(new_template(agent, self.preset.get_untracked(), project))
    }

    /// The ⋮ menu's "Schedule a prompt": the form, for a job that fires
    /// into the open session.
    pub fn schedule_prompt(self) {
        let Some((session, subagent)) = self.cron_session() else {
            self.status
                .set("open a session before scheduling a prompt into it".into());
            return;
        };
        self.open_view(View::Cron, None);
        self.cron_form.set(Some(CronForm {
            editing: None,
            draft: CronDraft::for_session(session, subagent),
            session: Some((session, subagent)),
            template: self.cron_template().unwrap_or_else(|| {
                new_template("primary".parse().expect("an agent ID"), None, None)
            }),
        }));
    }

    /// The view's "New": the form, for a job that fires into the open
    /// session, else one that starts a new session each run.
    pub fn new_cron_job(self) {
        let session = self.cron_session();
        let Some(template) = self.cron_template() else {
            if session.is_none() {
                self.cron_error
                    .set(Some("no agent can start a session".into()));
                return;
            }
            return self.schedule_prompt();
        };
        let draft = match session {
            Some((session, subagent)) => CronDraft::for_session(session, subagent),
            None => CronDraft::for_new_session(template.clone()),
        };
        self.cron_error.set(None);
        self.cron_form.set(Some(CronForm {
            editing: None,
            draft,
            session,
            template,
        }));
    }

    /// The form, filled from `job`.
    pub fn edit_cron(self, job: CronJob) {
        let zone = zone_at(jiff::Timestamp::now());
        let session = job.target.session().map(|session| {
            (
                session,
                matches!(job.target, cookie_agent_protocol::CronTarget::Subagent(_)),
            )
        });
        let template = match &job.target {
            cookie_agent_protocol::CronTarget::New(template) => Some(template.clone()),
            _ => self.cron_template(),
        }
        .unwrap_or_else(|| new_template("primary".parse().expect("an agent ID"), None, None));
        self.cron_error.set(None);
        self.cron_form.set(Some(CronForm {
            draft: CronDraft::of_job(&job, &zone),
            editing: Some(job),
            session,
            template,
        }));
    }

    /// Closes the form without sending it.
    pub fn close_cron_form(self) {
        self.cron_form.set(None);
        self.cron_error.set(None);
    }

    /// Sends the form: `crons.create` for a new job, `crons.update` with
    /// what changed for an edit.
    pub fn save_cron(self, draft: CronDraft) {
        let Some(client) = self.client() else {
            return;
        };
        let Some(form) = self.cron_form.get_untracked() else {
            return;
        };
        let zone = zone_at(jiff::Timestamp::now());
        let request = match &form.editing {
            None => draft.create_params().map(Ok),
            Some(job) => draft.update_params(job, &zone).map(Err),
        };
        let request = match request {
            Ok(request) => request,
            Err(message) => {
                self.cron_error.set(Some(message));
                return;
            }
        };
        if let Err(update) = &request
            && !update_changes_anything(update)
        {
            self.close_cron_form();
            return;
        }
        self.cron_error.set(None);
        spawn_local(async move {
            let (verb, result) = match request {
                Ok(create) => ("Scheduled", client.create_cron(create).await),
                Err(update) => ("Saved", client.update_cron(update).await),
            };
            match result {
                Ok(result) => {
                    self.status
                        .set(format!("{verb} {}.", job_title(&result.job)));
                    self.cron_selected.set(Some(result.job.id.clone()));
                    self.cron_form.set(None);
                    self.poll_crons();
                }
                Err(error) => self
                    .cron_error
                    .set(Some(crate::errors::describe_full(&error))),
            }
        });
    }

    /// Turns job `id` on or off.
    pub fn set_cron_enabled(self, id: CronJobId, enabled: bool) {
        let Some(client) = self.client() else {
            return;
        };
        let mut update = CronUpdateParams::new(id);
        update.enabled = Some(enabled);
        spawn_local(async move {
            match client.update_cron(update).await {
                Ok(result) => {
                    let verb = if enabled { "Enabled" } else { "Disabled" };
                    self.status
                        .set(format!("{verb} {}.", job_title(&result.job)));
                    self.cron_error.set(None);
                    self.poll_crons();
                }
                Err(error) => self.cron_error.set(Some(crate::errors::describe(&error))),
            }
        });
    }

    /// Fires job `id` now.
    pub fn run_cron(self, id: CronJobId) {
        let Some(client) = self.client() else {
            return;
        };
        spawn_local(async move {
            match client.run_cron(id).await {
                Ok(result) => {
                    let title = job_title(&result.job);
                    self.status.set(match result.session_id {
                        Some(_) => format!("Ran {title} now."),
                        None => format!("{title} did not run: its previous fire still waits."),
                    });
                    self.cron_error.set(None);
                    self.poll_crons();
                }
                Err(error) => self.cron_error.set(Some(crate::errors::describe(&error))),
            }
        });
    }

    /// Deletes job `id`; its run sessions stay.
    pub fn delete_cron(self, id: CronJobId, title: String) {
        let Some(client) = self.client() else {
            return;
        };
        spawn_local(async move {
            match client.delete_cron(id.clone()).await {
                Ok(_) => {
                    self.status.set(format!("Deleted {title}."));
                    if self.cron_selected.get_untracked() == Some(id) {
                        self.cron_selected.set(None);
                    }
                    self.cron_error.set(None);
                    self.poll_crons();
                }
                Err(error) => self.cron_error.set(Some(crate::errors::describe(&error))),
            }
        });
    }
}
