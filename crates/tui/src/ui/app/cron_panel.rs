//! The `/cron` panel (`docs/site/specs/cron.md` section 12): the daemon's
//! cron jobs with their schedule and its plain reading, next fire, target,
//! state, and last outcome; the selected job's details and history; and
//! creating, editing, enabling or disabling, running now, and deleting
//! jobs. A job is created and edited as a short form in `$VISUAL` or
//! `$EDITOR`, through the editor handoff `/memory` and `/config` use: a
//! `key: value` header, then the prompt after a `---` line.

use cookie_agent_client_state::{
    cron::{
        CronDraft, CronDraftTarget, bare_template, daemon_has_crons, delivery_label, detail_lines,
        fire_outcome, job_title, last_outcome, local_minute, schedule_reading, state_label,
        target_label, until, update_changes_anything,
    },
    memory::message_of,
};
use cookie_agent_protocol::{
    CronJob, CronJobId, CronTemplate, CronUpdateParams, CronsListResult, PermissionMode,
    ProducerDeliveryMode, SessionId, SessionOrigin,
};
use crossterm::event::{KeyCode, KeyEvent};
use jiff::tz::TimeZone;
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    text::{Line, Span},
    widgets::{List, ListItem, ListState, Paragraph, Wrap},
};

use super::{App, Modal, RpcUpdate, centered, paint_panel};
use crate::theme::Theme;

/// The clock that marks a session a cron job started, in the session list.
pub(in crate::ui) const CRON_RUN_MARK: &str = "🕒";

/// A job form to open in the editor; the event loop runs the editor and
/// hands the result to [`App::finish_cron_edit`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::ui) struct CronEditRequest {
    /// The job edited; `None` creates one.
    pub(in crate::ui) job: Option<CronJob>,
    /// The session `target: session` fires into, and whether it is a
    /// delegated one: the job's own for an in-session job, else the
    /// session on screen.
    pub(in crate::ui) session: Option<(SessionId, bool)>,
    /// What `target: new` starts from: the job's template, else the
    /// current session's agent, project, and directory.
    pub(in crate::ui) template: Option<CronTemplate>,
    pub(in crate::ui) text: String,
    /// The form as it was opened.
    pub(in crate::ui) original: String,
}

/// An action the panel offers, by key or by button.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::ui) enum CronAction {
    Edit,
    New,
    Enable,
    Disable,
    RunNow,
    Delete,
    ConfirmDelete,
    Cancel,
    EditAgain,
}

impl CronAction {
    const fn key_label(self) -> &'static str {
        match self {
            Self::Edit | Self::EditAgain => "e",
            Self::New => "n",
            Self::Enable | Self::Disable => "t",
            Self::RunNow => "r",
            Self::Delete => "d",
            Self::ConfirmDelete => "y",
            Self::Cancel => "esc",
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::Edit => "edit",
            Self::New => "new job",
            Self::Enable => "enable",
            Self::Disable => "disable",
            Self::RunNow => "run now",
            Self::Delete | Self::ConfirmDelete => "delete",
            Self::Cancel => "cancel",
            Self::EditAgain => "edit again",
        }
    }
}

/// Where the panel's clickable parts landed this frame.
#[derive(Clone, Debug, Default)]
pub(in crate::ui) struct CronHits {
    /// (row rect, job index).
    pub(in crate::ui) rows: Vec<(Rect, usize)>,
    pub(in crate::ui) actions: Vec<(Rect, CronAction)>,
}

#[derive(Default)]
pub(in crate::ui) struct CronPanelState {
    /// `crons.list`, or why it failed; `None` until it answers.
    pub(in crate::ui) listing: Option<Result<CronsListResult, String>>,
    pub(in crate::ui) selection: ListState,
    pub(in crate::ui) refresh: super::ListingRefresh,
    /// The job a delete waits to be confirmed for.
    pub(in crate::ui) confirm_delete: Option<CronJob>,
    /// A form the panel or the daemon refused, with why: edit it again or
    /// drop it.
    pub(in crate::ui) pending: Option<(CronEditRequest, String)>,
    /// The job to select once the listing has it: one just created.
    pub(in crate::ui) focus: Option<CronJobId>,
}

impl CronPanelState {
    pub(in crate::ui) fn jobs(&self) -> &[CronJob] {
        match &self.listing {
            Some(Ok(listing)) => &listing.jobs,
            _ => &[],
        }
    }

    pub(in crate::ui) fn selected_job(&self) -> Option<&CronJob> {
        self.jobs().get(self.selection.selected()?)
    }

    /// The actions the panel offers now.
    pub(in crate::ui) fn actions(&self) -> Vec<CronAction> {
        if self.pending.is_some() {
            return vec![CronAction::EditAgain, CronAction::Cancel];
        }
        if self.confirm_delete.is_some() {
            return vec![CronAction::ConfirmDelete, CronAction::Cancel];
        }
        if !matches!(self.listing, Some(Ok(_))) {
            return Vec::new();
        }
        match self.selected_job() {
            Some(job) => vec![
                CronAction::Edit,
                CronAction::New,
                if job.enabled {
                    CronAction::Disable
                } else {
                    CronAction::Enable
                },
                CronAction::RunNow,
                CronAction::Delete,
            ],
            None => vec![CronAction::New],
        }
    }

    fn install(&mut self, listing: Result<CronsListResult, String>) {
        self.listing = Some(listing);
        let count = self.jobs().len();
        if let Some(index) = self
            .focus
            .as_ref()
            .and_then(|id| self.jobs().iter().position(|job| job.id == *id))
        {
            self.focus = None;
            self.selection.select(Some(index));
            return;
        }
        self.selection
            .select((count > 0).then(|| self.selection.selected().unwrap_or(0).min(count - 1)));
    }
}

/// The permission modes a form names, the default first.
const PERMISSION_MODES: [(&str, PermissionMode); 5] = [
    ("auto_approve_n", PermissionMode::AutoApproveN),
    ("auto_approve", PermissionMode::AutoApprove),
    ("auto_approve_y", PermissionMode::AutoApproveY),
    ("ask", PermissionMode::Ask),
    ("yolo", PermissionMode::Yolo),
];

fn mode_name(mode: PermissionMode) -> &'static str {
    PERMISSION_MODES
        .iter()
        .find(|(_, candidate)| *candidate == mode)
        .map_or("", |(name, _)| name)
}

/// The form a job is created or edited as.
pub(in crate::ui) fn form_text(draft: &CronDraft, session: Option<&str>) -> String {
    let target = if draft.target.is_new() {
        "new"
    } else {
        "session"
    };
    let session = session.map_or_else(
        || "session is not offered: no session is open".to_owned(),
        |label| format!("session fires into {label}"),
    );
    format!(
        "# A cron job. Lines starting with # are ignored; the prompt follows the --- line.\n\
         # schedule: five cron fields in the daemon host's local time (0 3 * * *),\n\
         #   a delay (in 20m), or a time (at 2026-10-23T03:00:00Z).\n\
         # max_runs: cron schedules only: a whole number from 1, or unlimited; empty is 1.\n\
         # target: session or new (a new session per fire); {session}.\n\
         # delivery: session targets: queue (after the current turn) or steer (into it).\n\
         # agent, model, permission_mode: new-session targets. model is provider/model or\n\
         #   provider/model[variant], empty for the agent's own; permission_mode is empty\n\
         #   for auto_approve_n, or auto_approve, auto_approve_y, ask, yolo.\n\
         name: {}\n\
         schedule: {}\n\
         max_runs: {}\n\
         target: {target}\n\
         delivery: {}\n\
         agent: {}\n\
         model: {}\n\
         permission_mode: {}\n\
         ---\n\
         {}\n",
        draft.name,
        draft.schedule,
        draft.max_runs,
        delivery_label(draft.delivery),
        draft.agent,
        draft.model,
        draft.permission_mode.map_or("", mode_name),
        draft.prompt,
    )
}

/// Reads a form back as a draft for `request`'s job, or says what is wrong.
pub(in crate::ui) fn parse_form(
    text: &str,
    request: &CronEditRequest,
) -> Result<CronDraft, String> {
    let (header, prompt) = match text.split_once("\n---\n") {
        Some((header, prompt)) => (header, prompt),
        None => match text.strip_suffix("\n---") {
            Some(header) => (header, ""),
            None => return Err("the form needs the --- line before the prompt".to_owned()),
        },
    };
    let mut fields: Vec<(&str, &str)> = Vec::new();
    for line in header.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            return Err(format!("`{line}` is not `key: value`"));
        };
        let key = key.trim();
        if !matches!(
            key,
            "name"
                | "schedule"
                | "max_runs"
                | "target"
                | "delivery"
                | "agent"
                | "model"
                | "permission_mode"
        ) {
            return Err(format!("`{key}` is not a field of a job"));
        }
        if fields.iter().any(|(seen, _)| *seen == key) {
            return Err(format!("`{key}` is given twice"));
        }
        fields.push((key, value.trim()));
    }
    let field = |key: &str| {
        fields
            .iter()
            .find(|(seen, _)| *seen == key)
            .map_or("", |(_, value)| value)
    };
    let delivery = match field("delivery") {
        "" | "queue" => ProducerDeliveryMode::Queue,
        "steer" => ProducerDeliveryMode::Steer,
        other => return Err(format!("delivery is queue or steer, not `{other}`")),
    };
    let permission_mode = match field("permission_mode") {
        "" => None,
        name => Some(
            PERMISSION_MODES
                .iter()
                .find(|(candidate, _)| *candidate == name)
                .map(|(_, mode)| *mode)
                .ok_or_else(|| format!("`{name}` is not a permission mode"))?,
        ),
    };
    let agent = field("agent");
    let target = match field("target") {
        "session" => {
            let (session, subagent) = request
                .session
                .ok_or("target: session needs an open session; use target: new")?;
            CronDraftTarget::Session { session, subagent }
        }
        "new" => {
            let template = match &request.template {
                Some(template) => template.clone(),
                None => bare_template(
                    agent
                        .parse()
                        .map_err(|_| "a new-session job needs an agent".to_owned())?,
                ),
            };
            CronDraftTarget::New { template }
        }
        other => return Err(format!("target is session or new, not `{other}`")),
    };
    Ok(CronDraft {
        name: field("name").to_owned(),
        prompt: prompt.to_owned(),
        schedule: field("schedule").to_owned(),
        max_runs: field("max_runs").to_owned(),
        target,
        delivery,
        permission_mode,
        model: field("model").to_owned(),
        agent: agent.to_owned(),
    })
}

/// A job's row: title, schedule reading, next fire, target, state, and
/// last outcome.
fn row_line(job: &CronJob, target: &str, theme: &Theme) -> Line<'static> {
    let now = jiff::Timestamp::now();
    let zone = TimeZone::system();
    let next = job.next_fire.map_or_else(
        || "no next fire".to_owned(),
        |next| format!("next {}", until(next, now)),
    );
    let outcome = last_outcome(job);
    let outcome_style = if outcome == "failed" {
        theme.error()
    } else {
        theme.muted()
    };
    Line::from(vec![
        Span::styled(job_title(job), theme.body()),
        Span::styled(
            format!("  {}", schedule_reading(&job.schedule, job.max_runs, &zone)),
            theme.muted(),
        ),
        Span::styled(format!("  {next}"), theme.muted()),
        Span::styled(format!("  → {target}"), theme.muted()),
        Span::styled(
            format!("  {}", state_label(job)),
            if job.enabled {
                theme.muted()
            } else {
                theme.warning()
            },
        ),
        Span::styled(format!("  last {outcome}"), outcome_style),
    ])
}

fn detail_text(panel: &CronPanelState, target: Option<&str>) -> String {
    if let Some((_, message)) = &panel.pending {
        return format!("Not saved: {message}\n\ne edits your form again; Esc drops it.");
    }
    if let Some(job) = &panel.confirm_delete {
        return format!(
            "Delete {}? Its run sessions stay. y deletes; Esc cancels.",
            job_title(job)
        );
    }
    match &panel.listing {
        None => return "Loading cron jobs...".into(),
        Some(Err(error)) => return format!("Cron jobs are unavailable: {error}"),
        Some(Ok(_)) => {}
    }
    let Some(job) = panel.selected_job() else {
        return "No cron jobs yet. n schedules a prompt: into this session when one is open, \
                or into a new session per fire."
            .into();
    };
    let zone = TimeZone::system();
    let mut lines: Vec<String> =
        detail_lines(job, target.unwrap_or(""), jiff::Timestamp::now(), &zone)
            .into_iter()
            .map(|(label, value)| format!("{label}: {value}"))
            .collect();
    lines.push(String::new());
    lines.push(job.prompt.clone());
    lines.join("\n")
}

fn history_text(panel: &CronPanelState) -> String {
    let Some(job) = panel.selected_job() else {
        return String::new();
    };
    if job.history.is_empty() {
        return "No fires yet.".into();
    }
    let zone = TimeZone::system();
    job.history
        .iter()
        .rev()
        .map(|fire| {
            format!(
                "{}  {}",
                local_minute(fire.fired_at, &zone),
                fire_outcome(fire)
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

impl App {
    /// Whether the daemon keeps cron jobs (client protocol 1.15).
    pub(in crate::ui) fn daemon_has_crons(&self) -> bool {
        daemon_has_crons(self.client.daemon_version())
    }

    pub(in crate::ui) fn open_cron_panel(&mut self) {
        if !self.daemon_has_crons() {
            self.status =
                "This daemon does not keep cron jobs; it needs client protocol 1.15.".into();
            return;
        }
        self.cron_panel = CronPanelState::default();
        self.modal = Modal::Cron;
        self.poll_crons();
    }

    /// `/cron new`: the panel, with a new job's form for the session on
    /// screen.
    pub(in crate::ui) fn schedule_in_session(&mut self) {
        self.open_cron_panel();
        if self.modal == Modal::Cron {
            self.begin_new_cron();
        }
    }

    /// Fetches `crons.list` for the open panel.
    pub(super) fn poll_crons(&mut self) {
        if self.modal != Modal::Cron || !self.cron_panel.refresh.start() {
            return;
        }
        let client = self.client.clone();
        let updates = self.rpc_updates_tx.clone();
        self.spawn_rpc(async move {
            let result = client
                .list_crons()
                .await
                .map_err(|error| message_of(&error));
            let _ = updates.send(RpcUpdate::CronsListed { result });
        });
    }

    pub(in crate::ui) fn install_cron_listing(&mut self, result: Result<CronsListResult, String>) {
        let again = self.cron_panel.refresh.landed();
        self.cron_panel.install(result);
        if again {
            self.poll_crons();
        }
    }

    /// The session on screen as a target: its ID and whether it is a
    /// delegated one.
    fn viewed_cron_session(&self) -> Option<(SessionId, bool)> {
        let session = self.viewed_session()?;
        let subagent = self
            .selected_session_meta()
            .is_some_and(|meta| matches!(meta.origin, SessionOrigin::Delegated { .. }));
        Some((session, subagent))
    }

    /// What a new-session job starts from by default: the current agent,
    /// its preset, the session's project, and this directory.
    fn default_cron_template(&self) -> Option<CronTemplate> {
        let selection = self
            .draft
            .clone()
            .or_else(|| {
                self.selected_session_meta()
                    .map(|meta| meta.creation_selection.clone())
            })
            .or_else(|| self.default_draft_selection())?;
        let mut template = bare_template(selection.agent);
        template.preset = selection.preset;
        template.project = self
            .selected_session_meta()
            .and_then(|meta| meta.project.clone());
        template.cwd = self.cwd.clone();
        Some(template)
    }

    /// How the panel names a job's target.
    fn cron_target_label(&self, job: &CronJob) -> String {
        target_label(&job.target, self.viewed_session(), |session| {
            self.sessions
                .iter()
                .find(|meta| meta.session_id == session)
                .and_then(|meta| meta.title.as_ref())
                .map(ToString::to_string)
        })
    }

    fn session_target_label(&self, session: (SessionId, bool)) -> String {
        let target = if session.1 {
            cookie_agent_protocol::CronTarget::Subagent(session.0)
        } else {
            cookie_agent_protocol::CronTarget::Session(session.0)
        };
        target_label(&target, self.viewed_session(), |id| {
            self.sessions
                .iter()
                .find(|meta| meta.session_id == id)
                .and_then(|meta| meta.title.as_ref())
                .map(ToString::to_string)
        })
    }

    /// Opens a new job's form: into the session on screen, or into a new
    /// session per fire when none is.
    fn begin_new_cron(&mut self) {
        let session = self.viewed_cron_session();
        let template = self.default_cron_template();
        let mut draft = match (session, &template) {
            (Some((session, subagent)), _) => CronDraft::for_session(session, subagent),
            (None, Some(template)) => CronDraft::for_new_session(template.clone()),
            (None, None) => {
                self.status = "No agent is available to run a new-session job.".into();
                return;
            }
        };
        if draft.agent.is_empty()
            && let Some(template) = &template
        {
            draft.agent = template.agent.to_string();
        }
        let label = session.map(|session| self.session_target_label(session));
        let text = form_text(&draft, label.as_deref());
        self.status = "editing a new cron job".into();
        self.pending_cron_editor = Some(CronEditRequest {
            job: None,
            session,
            template,
            text: text.clone(),
            original: text,
        });
    }

    fn begin_cron_edit(&mut self, job: CronJob) {
        let zone = TimeZone::system();
        let mut draft = CronDraft::of_job(&job, &zone);
        let session = job
            .target
            .session()
            .map(|session| {
                (
                    session,
                    matches!(job.target, cookie_agent_protocol::CronTarget::Subagent(_)),
                )
            })
            .or_else(|| self.viewed_cron_session());
        let template = match &job.target {
            cookie_agent_protocol::CronTarget::New(template) => Some(template.clone()),
            _ => self.default_cron_template(),
        };
        if draft.agent.is_empty()
            && let Some(template) = &template
        {
            draft.agent = template.agent.to_string();
        }
        let label = session.map(|session| self.session_target_label(session));
        let text = form_text(&draft, label.as_deref());
        self.status = format!("editing {}", job_title(&job));
        self.pending_cron_editor = Some(CronEditRequest {
            job: Some(job),
            session,
            template,
            text: text.clone(),
            original: text,
        });
    }

    /// What the editor left for `request`: sent unless unchanged.
    pub(in crate::ui) fn finish_cron_edit(
        &mut self,
        request: CronEditRequest,
        edited: Result<String, String>,
    ) {
        let text = match edited {
            Ok(text) => text,
            Err(error) => {
                self.status = error;
                return;
            }
        };
        let title = request.job.as_ref().map(job_title);
        if text == request.original {
            self.cron_panel.pending = None;
            self.status = match &title {
                Some(title) => format!("No changes to {title}."),
                None => "No changes; the job was not created.".into(),
            };
            return;
        }
        let request = CronEditRequest { text, ..request };
        let draft = match parse_form(&request.text, &request) {
            Ok(draft) => draft,
            Err(message) => return self.refuse_cron_form(request, message),
        };
        let zone = TimeZone::system();
        match &request.job {
            None => match draft.create_params() {
                Ok(params) => self.dispatch_cron_save(request, CronSave::Create(params)),
                Err(message) => self.refuse_cron_form(request, message),
            },
            Some(job) => match draft.update_params(job, &zone) {
                Ok(params) if !update_changes_anything(&params) => {
                    self.cron_panel.pending = None;
                    self.status = format!("No changes to {}.", job_title(job));
                }
                Ok(params) => self.dispatch_cron_save(request, CronSave::Update(params)),
                Err(message) => self.refuse_cron_form(request, message),
            },
        }
    }

    fn refuse_cron_form(&mut self, request: CronEditRequest, message: String) {
        self.status = format!("Not saved: {message}");
        self.cron_panel.pending = Some((request, message));
    }

    fn dispatch_cron_save(&mut self, request: CronEditRequest, save: CronSave) {
        self.status = "saving the cron job…".into();
        let client = self.client.clone();
        let updates = self.rpc_updates_tx.clone();
        self.spawn_rpc(async move {
            let result = match save {
                CronSave::Create(params) => client.create_cron(params).await,
                CronSave::Update(params) => client.update_cron(params).await,
            }
            .map(|result| result.job)
            .map_err(|error| message_of(&error));
            let _ = updates.send(RpcUpdate::CronSaved {
                request: Box::new(request),
                result: Box::new(result),
            });
        });
    }

    pub(in crate::ui) fn install_cron_save(
        &mut self,
        request: CronEditRequest,
        result: Result<CronJob, String>,
    ) {
        match result {
            Ok(job) => {
                self.cron_panel.pending = None;
                let zone = TimeZone::system();
                let next = job.next_fire.map_or_else(String::new, |next| {
                    format!("; next fire {}", local_minute(next, &zone))
                });
                let verb = if request.job.is_some() {
                    "Saved"
                } else {
                    "Created"
                };
                self.status = format!("{verb} {}{next}.", job_title(&job));
                self.cron_panel.focus = Some(job.id);
            }
            Err(message) => self.refuse_cron_form(request, message),
        }
        self.poll_crons();
    }

    /// Runs a change of one job whose answer is a status line either way.
    fn dispatch_cron_change(&mut self, pending: String, change: CronChange) {
        self.status = pending;
        let client = self.client.clone();
        let updates = self.rpc_updates_tx.clone();
        self.spawn_rpc(async move {
            let result = match change {
                CronChange::Enabled { title, id, enabled } => {
                    let mut params = CronUpdateParams::new(id);
                    params.enabled = Some(enabled);
                    let verb = if enabled { "Enabled" } else { "Disabled" };
                    client
                        .update_cron(params)
                        .await
                        .map(|_| format!("{verb} {title}."))
                        .map_err(|error| format!("{title} was not changed: {}", message_of(&error)))
                }
                CronChange::RunNow { title, id } => client
                    .run_cron(id)
                    .await
                    .map(|run| {
                        let outcome = run
                            .job
                            .history
                            .iter()
                            .rev()
                            .find(|fire| fire.run_now)
                            .map_or_else(|| "delivered".to_owned(), fire_outcome);
                        format!("Ran {title} now: {outcome}.")
                    })
                    .map_err(|error| format!("{title} did not run: {}", message_of(&error))),
                CronChange::Delete { title, id } => client
                    .delete_cron(id)
                    .await
                    .map(|_| format!("Deleted {title}."))
                    .map_err(|error| format!("{title} was not deleted: {}", message_of(&error))),
            };
            let _ = updates.send(RpcUpdate::CronChanged { result });
        });
    }

    pub(in crate::ui) fn install_cron_change(&mut self, result: Result<String, String>) {
        self.status = match result {
            Ok(done) | Err(done) => done,
        };
        self.poll_crons();
    }

    /// Acts on `action`, from its key or its button.
    pub(in crate::ui) fn activate_cron_action(&mut self, action: CronAction) {
        if !self.cron_panel.actions().contains(&action) {
            return;
        }
        let selected = self.cron_panel.selected_job().cloned();
        match action {
            CronAction::Edit => {
                if let Some(job) = selected {
                    self.begin_cron_edit(job);
                }
            }
            CronAction::New => self.begin_new_cron(),
            CronAction::Enable | CronAction::Disable => {
                if let Some(job) = selected {
                    let enabled = action == CronAction::Enable;
                    let title = job_title(&job);
                    let verb = if enabled { "enabling" } else { "disabling" };
                    self.dispatch_cron_change(
                        format!("{verb} {title}…"),
                        CronChange::Enabled {
                            title,
                            id: job.id,
                            enabled,
                        },
                    );
                }
            }
            CronAction::RunNow => {
                if let Some(job) = selected {
                    let title = job_title(&job);
                    self.dispatch_cron_change(
                        format!("running {title}…"),
                        CronChange::RunNow { title, id: job.id },
                    );
                }
            }
            CronAction::Delete => self.cron_panel.confirm_delete = selected,
            CronAction::ConfirmDelete => {
                if let Some(job) = self.cron_panel.confirm_delete.take() {
                    let title = job_title(&job);
                    self.dispatch_cron_change(
                        format!("deleting {title}…"),
                        CronChange::Delete { title, id: job.id },
                    );
                }
            }
            CronAction::Cancel => {
                if self.cron_panel.confirm_delete.take().is_some() {
                    self.status = "Delete cancelled.".into();
                } else if self.cron_panel.pending.take().is_some() {
                    self.status = "Your form was dropped.".into();
                }
            }
            CronAction::EditAgain => {
                if let Some((request, _)) = self.cron_panel.pending.take() {
                    self.pending_cron_editor = Some(request);
                }
            }
        }
    }

    pub(super) fn handle_cron_key(&mut self, key: KeyEvent) {
        let actions = self.cron_panel.actions();
        let action = match key.code {
            KeyCode::Esc if actions.contains(&CronAction::Cancel) => Some(CronAction::Cancel),
            KeyCode::Esc => {
                self.modal = Modal::None;
                return;
            }
            KeyCode::Enter => Some(CronAction::Edit),
            KeyCode::Char(character) => actions
                .iter()
                .copied()
                .find(|action| action.key_label() == character.to_string()),
            KeyCode::Up | KeyCode::Down => {
                if self.cron_panel.confirm_delete.is_none() && self.cron_panel.pending.is_none() {
                    let count = self.cron_panel.jobs().len();
                    super::move_picker_selection(
                        &mut self.cron_panel.selection,
                        count,
                        key.code == KeyCode::Up,
                    );
                }
                return;
            }
            _ => None,
        };
        if let Some(action) = action {
            self.activate_cron_action(action);
        }
    }

    pub(super) fn render_cron_panel(&mut self, frame: &mut Frame) {
        let area = centered(frame.area(), 92, 80);
        paint_panel(frame, area, &self.theme);
        let targets: Vec<String> = self
            .cron_panel
            .jobs()
            .iter()
            .map(|job| self.cron_target_label(job))
            .collect();
        self.hit_map.cron = render_cron(frame, area, &mut self.cron_panel, &targets, &self.theme);
    }
}

/// A form's request to the daemon.
enum CronSave {
    Create(cookie_agent_protocol::CronCreateParams),
    Update(CronUpdateParams),
}

/// A change to one job, named by its title for the status line.
enum CronChange {
    Enabled {
        title: String,
        id: CronJobId,
        enabled: bool,
    },
    RunNow {
        title: String,
        id: CronJobId,
    },
    Delete {
        title: String,
        id: CronJobId,
    },
}

/// The panel: the jobs, the selected job's details and history, and the
/// actions.
fn render_cron(
    frame: &mut Frame,
    area: Rect,
    panel: &mut CronPanelState,
    targets: &[String],
    theme: &Theme,
) -> CronHits {
    let mut hits = CronHits::default();
    let count = panel.jobs().len();
    panel
        .selection
        .select((count > 0).then(|| panel.selection.selected().unwrap_or(0).min(count - 1)));
    let [list_area, bottom_area, actions_area] = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(4),
            Constraint::Length(14),
            Constraint::Length(1),
        ])
        .areas(area);
    let [detail_area, history_area] = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
        .areas(bottom_area);
    let items = panel
        .jobs()
        .iter()
        .zip(targets)
        .map(|(job, target)| ListItem::new(row_line(job, target, theme)))
        .collect::<Vec<_>>();
    let firing = !matches!(&panel.listing, Some(Ok(listing)) if !listing.firing);
    let title = if firing {
        "Cron jobs"
    } else {
        "Cron jobs · firing is off ([cron] enabled = false)"
    };
    let block = crate::ui::panel_block()
        .border_style(theme.panel_border())
        .title(crate::ui::panel_title(title))
        .title_bottom(crate::ui::panel_title(
            Line::from(Span::styled(
                "e edit | n new | t enable/disable | r run now | d delete | esc close",
                theme.internal(),
            ))
            .right_aligned(),
        ));
    let inner = block.inner(list_area);
    frame.render_stateful_widget(
        List::new(items)
            .highlight_symbol("> ")
            .highlight_style(theme.selected())
            .block(block),
        list_area,
        &mut panel.selection,
    );
    let quiet = panel.confirm_delete.is_some() || panel.pending.is_some();
    if !quiet {
        let offset = panel.selection.offset();
        for (line, index) in (offset..count).take(usize::from(inner.height)).enumerate() {
            let y = inner.y + u16::try_from(line).unwrap_or(u16::MAX);
            hits.rows
                .push((Rect::new(inner.x, y, inner.width, 1), index));
        }
    }
    let target = panel
        .selection
        .selected()
        .and_then(|index| targets.get(index))
        .map(String::as_str);
    frame.render_widget(
        Paragraph::new(detail_text(panel, target))
            .wrap(Wrap { trim: false })
            .block(
                crate::ui::panel_block()
                    .border_style(theme.panel_border())
                    .title(crate::ui::panel_title("Details")),
            ),
        detail_area,
    );
    frame.render_widget(
        Paragraph::new(history_text(panel))
            .wrap(Wrap { trim: false })
            .block(
                crate::ui::panel_block()
                    .border_style(theme.panel_border())
                    .title(crate::ui::panel_title("History, newest first")),
            ),
        history_area,
    );
    let mut spans = Vec::new();
    let mut x = actions_area.x;
    for action in panel.actions() {
        let label = format!(" {} ({}) ", action.label(), action.key_label());
        let width = u16::try_from(label.chars().count()).unwrap_or(u16::MAX);
        if x.saturating_add(width) <= actions_area.right() {
            hits.actions
                .push((Rect::new(x, actions_area.y, width, 1), action));
        }
        spans.push(Span::styled(label, theme.link()));
        spans.push(Span::raw(" "));
        x = x.saturating_add(width + 1);
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), actions_area);
    hits
}

/// A listing with an in-session job that has fired and a disabled
/// new-session job.
#[cfg(test)]
pub(in crate::ui) fn sample_crons() -> CronsListResult {
    serde_json::from_value(serde_json::json!({
        "jobs": [
            {
                "id": "k3v9q2xa",
                "name": "Nightly check",
                "prompt": "Check for outdated dependencies.",
                "schedule": {"cron": "0 3 * * *"},
                "max_runs": "unlimited",
                "target": {"session": "0190a5b2-0000-7000-8000-000000000001"},
                "delivery": "queue",
                "enabled": true,
                "created_by": "user",
                "created_at": "2026-10-22T18:00:00Z",
                "next_fire": "2099-10-24T03:00:00Z",
                "runs": 1,
                "history": [{
                    "scheduled_at": "2026-10-23T03:00:00Z",
                    "fired_at": "2026-10-23T03:00:00Z",
                    "outcome": {"kind": "delivered"},
                    "session_id": "0190a5b2-0000-7000-8000-000000000001",
                    "run_end": {"status": "completed", "at": "2026-10-23T03:02:00Z"}
                }]
            },
            {
                "id": "m7p2r4st",
                "prompt": "Summarize yesterday's commits.",
                "schedule": {"cron": "30 9 * * 1-5"},
                "max_runs": 1,
                "target": {"new": {"agent": "builder"}},
                "permission_mode": "auto_approve_n",
                "enabled": false,
                "disabled_reason": {"kind": "failed", "failures": 3},
                "created_by": "user",
                "created_at": "2026-10-20T08:00:00Z",
                "consecutive_failures": 3,
                "history": [{
                    "scheduled_at": "2026-10-22T09:30:00Z",
                    "fired_at": "2026-10-22T09:30:00Z",
                    "outcome": {"kind": "failed", "error": "no model is available"}
                }]
            }
        ],
        "firing": true
    }))
    .expect("sample crons")
}
