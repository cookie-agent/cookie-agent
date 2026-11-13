//! The `/webhooks` panel (`docs/site/specs/webhooks.md` section 14): the
//! daemon's webhooks with their target, state, check, last outcome, and
//! request counts; the selected webhook's details (its URL first) and
//! history; one event's request; and creating, editing, enabling or
//! disabling, testing, rotating, revealing the secret, copying the URL, and
//! deleting webhooks. A webhook is created and edited as a short form in
//! `$VISUAL` or `$EDITOR` through the editor handoff `/cron` uses.

use cookie_agent_client_state::{
    cron::CronDetail,
    memory::message_of,
    webhook::{
        OutcomeKind, WebhookDraft, check_label, counts_summary, daemon_has_webhooks, detail_lines,
        event_outcome, event_time, events_newest_first, last_outcome, state_label, target_label,
        update_changes_anything, url_looks_private, webhook_title, webhook_url,
    },
};
use cookie_agent_protocol::{
    CronActor, SessionId, SessionOrigin, Webhook, WebhookEventResult, WebhookId, WebhookRotate,
    WebhookTarget, WebhookTemplate, WebhookTestParams, WebhookUpdateParams, WebhooksListResult,
};
use crossterm::event::{KeyCode, KeyEvent};
use jiff::tz::TimeZone;
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    text::{Line, Span},
    widgets::{Clear, List, ListItem, ListState, Paragraph, Wrap},
};

use super::{App, Modal, RpcUpdate, centered, paint_panel};
use crate::theme::Theme;

/// The mark of a session a webhook started, in the session list.
pub(in crate::ui) const WEBHOOK_RUN_MARK: &str = "⚡";

/// A webhook form to open in the editor; the event loop runs the editor
/// and hands the result to [`App::finish_webhook_edit`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::ui) struct WebhookEditRequest {
    /// The webhook edited; `None` creates one.
    pub(in crate::ui) webhook: Option<Webhook>,
    /// The form the text was made from; its target is fixed.
    pub(in crate::ui) draft: WebhookDraft,
    pub(in crate::ui) text: String,
    /// The form as it was opened.
    pub(in crate::ui) original: String,
}

/// An action the panel offers, by key or by button.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::ui) enum WebhookAction {
    Edit,
    /// A webhook that delivers into the session on screen.
    NewHere,
    /// A webhook that starts new sessions.
    NewSessions,
    Enable,
    Disable,
    Test,
    CopyUrl,
    RevealSecret,
    RotateUrl,
    RotateSecret,
    Events,
    Delete,
    Confirm,
    CopySecret,
    OlderEvent,
    NewerEvent,
    Cancel,
    EditAgain,
}

impl WebhookAction {
    const fn key_label(self) -> &'static str {
        match self {
            Self::Edit | Self::EditAgain => "e",
            Self::NewHere => "n",
            Self::NewSessions => "s",
            Self::Enable | Self::Disable => "t",
            Self::Test => "p",
            Self::CopyUrl | Self::CopySecret => "c",
            Self::RevealSecret => "r",
            Self::RotateUrl => "u",
            Self::RotateSecret => "k",
            Self::Events => "v",
            Self::Delete => "d",
            Self::Confirm => "y",
            Self::OlderEvent => "←",
            Self::NewerEvent => "→",
            Self::Cancel => "esc",
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::Edit => "edit",
            Self::NewHere => "new for this session",
            Self::NewSessions => "new for new sessions",
            Self::Enable => "enable",
            Self::Disable => "disable",
            Self::Test => "send a test event",
            Self::CopyUrl => "copy URL",
            Self::RevealSecret => "show secret",
            Self::RotateUrl => "new URL",
            Self::RotateSecret => "new secret",
            Self::Events => "events",
            Self::Delete => "delete",
            Self::Confirm => "confirm",
            Self::CopySecret => "copy secret",
            Self::OlderEvent => "older",
            Self::NewerEvent => "newer",
            Self::Cancel => "close",
            Self::EditAgain => "edit again",
        }
    }

    /// The key that runs the action, as a terminal reports it.
    fn matches(self, code: KeyCode) -> bool {
        match self {
            Self::Cancel => code == KeyCode::Esc,
            Self::OlderEvent => code == KeyCode::Left,
            Self::NewerEvent => code == KeyCode::Right,
            _ => matches!(code, KeyCode::Char(key) if self.key_label() == key.to_string()),
        }
    }
}

/// What waits for a `y`: a delete or a rotation of the webhook.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::ui) enum WebhookConfirm {
    Delete(Webhook),
    Rotate(Webhook, WebhookRotate),
}

/// One event of a webhook's history, opened to its request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::ui) struct EventView {
    pub(in crate::ui) webhook: WebhookId,
    /// Its place among the webhook's events, newest first.
    pub(in crate::ui) index: usize,
    /// `webhooks.event`, or why it failed; `None` until it answers.
    pub(in crate::ui) request: Option<Result<WebhookEventResult, String>>,
    pub(in crate::ui) scroll: u16,
}

/// Where the panel's clickable parts landed this frame.
#[derive(Clone, Debug, Default)]
pub(in crate::ui) struct WebhookHits {
    /// (row rect, webhook index).
    pub(in crate::ui) rows: Vec<(Rect, usize)>,
    pub(in crate::ui) actions: Vec<(Rect, WebhookAction)>,
    /// The details that name a session: the target and the creating one.
    pub(in crate::ui) sessions: Vec<(Rect, SessionId)>,
    /// (history row rect, event index, newest first).
    pub(in crate::ui) events: Vec<(Rect, usize)>,
}

#[derive(Default)]
pub(in crate::ui) struct WebhookPanelState {
    /// `webhooks.list`, or why it failed; `None` until it answers.
    pub(in crate::ui) listing: Option<Result<WebhooksListResult, String>>,
    pub(in crate::ui) selection: ListState,
    pub(in crate::ui) refresh: super::ListingRefresh,
    pub(in crate::ui) confirm: Option<WebhookConfirm>,
    /// A form the panel or the daemon refused, with why.
    pub(in crate::ui) pending: Option<(WebhookEditRequest, String)>,
    /// The webhook to select once the listing has it: one just created.
    pub(in crate::ui) focus: Option<WebhookId>,
    /// A secret on show, with the webhook it is of: after a create or a
    /// rotation, which return it once, or when asked for.
    pub(in crate::ui) secret: Option<(String, String)>,
    pub(in crate::ui) event: Option<EventView>,
    listing_revision: u64,
    targets: Option<((u64, u64, usize), Vec<String>)>,
}

impl WebhookPanelState {
    pub(in crate::ui) fn webhooks(&self) -> &[Webhook] {
        match &self.listing {
            Some(Ok(listing)) => &listing.webhooks,
            _ => &[],
        }
    }

    pub(in crate::ui) fn selected_webhook(&self) -> Option<&Webhook> {
        self.webhooks().get(self.selection.selected()?)
    }

    fn public_base(&self) -> Option<&str> {
        match &self.listing {
            Some(Ok(listing)) => listing.public_base.as_deref(),
            _ => None,
        }
    }

    /// The actions the panel offers now; `here` is whether a session is
    /// on screen to deliver into.
    pub(in crate::ui) fn actions(&self, here: bool) -> Vec<WebhookAction> {
        use WebhookAction as A;
        if self.pending.is_some() {
            return vec![A::EditAgain, A::Cancel];
        }
        if self.confirm.is_some() {
            return vec![A::Confirm, A::Cancel];
        }
        if self.secret.is_some() {
            return vec![A::CopySecret, A::Cancel];
        }
        if let Some(view) = &self.event {
            let count = self
                .webhooks()
                .iter()
                .find(|webhook| webhook.id == view.webhook)
                .map_or(0, |webhook| webhook.history.len());
            let mut actions = Vec::new();
            if view.index + 1 < count {
                actions.push(A::OlderEvent);
            }
            if view.index > 0 {
                actions.push(A::NewerEvent);
            }
            actions.push(A::Cancel);
            return actions;
        }
        if !matches!(self.listing, Some(Ok(_))) {
            return Vec::new();
        }
        let mut actions = Vec::new();
        if let Some(webhook) = self.selected_webhook() {
            actions.push(A::Edit);
            actions.push(A::CopyUrl);
            actions.push(A::Test);
            if !webhook.history.is_empty() {
                actions.push(A::Events);
            }
            actions.push(if webhook.enabled {
                A::Disable
            } else {
                A::Enable
            });
            if webhook.verify.is_some() {
                actions.push(A::RevealSecret);
                actions.push(A::RotateSecret);
            }
            actions.push(A::RotateUrl);
            actions.push(A::Delete);
        }
        if here {
            actions.push(A::NewHere);
        }
        actions.push(A::NewSessions);
        actions
    }

    fn install(&mut self, listing: Result<WebhooksListResult, String>) {
        let selected = self.selected_webhook().map(|webhook| webhook.id.clone());
        self.listing = Some(listing);
        self.listing_revision = self.listing_revision.wrapping_add(1);
        let count = self.webhooks().len();
        let index_of = |id: &WebhookId| self.webhooks().iter().position(|hook| hook.id == *id);
        if let Some(index) = self.focus.as_ref().and_then(index_of) {
            self.focus = None;
            self.selection.select(Some(index));
            return;
        }
        if let Some(index) = selected.as_ref().and_then(index_of) {
            self.selection.select(Some(index));
            return;
        }
        self.selection
            .select((count > 0).then(|| self.selection.selected().unwrap_or(0).min(count - 1)));
    }
}

/// A webhook's row: title, target, state, check, last outcome, counts.
fn row_line(webhook: &Webhook, target: &str, theme: &Theme) -> Line<'static> {
    let outcome = last_outcome(webhook);
    let outcome_style = if outcome == Some(OutcomeKind::Failed) {
        theme.error()
    } else {
        theme.muted()
    };
    let last = webhook.history.last().map_or_else(
        || "no events".to_owned(),
        |event| format!("last {}", event_outcome(event)),
    );
    let counts = counts_summary(&webhook.counts);
    let mut spans = vec![
        Span::styled(webhook_title(webhook), theme.body()),
        Span::styled(format!("  → {target}"), theme.muted()),
        Span::styled(
            format!("  {}", state_label(webhook)),
            if webhook.enabled {
                theme.muted()
            } else {
                theme.warning()
            },
        ),
        Span::styled(
            format!("  {}", check_label(webhook.verify.as_ref())),
            theme.muted(),
        ),
        Span::styled(format!("  {last}"), outcome_style),
    ];
    if !counts.is_empty() {
        spans.push(Span::styled(format!("  {counts}"), theme.muted()));
    }
    Line::from(spans)
}

/// The URL a webhook is reached at, or its path when the TUI knows no page
/// address (a daemon inside this process), and whether the address looks
/// private.
fn url_of(panel: &WebhookPanelState, webhook: &Webhook, page: Option<&str>) -> (String, bool) {
    let public = panel.public_base();
    match (public, page) {
        (None, None) => (webhook.path.clone(), true),
        (public, page) => {
            let page = page.unwrap_or_default();
            (
                webhook_url(public, page, &webhook.path),
                url_looks_private(public, page),
            )
        }
    }
}

/// The details pane's text, below the details that link to a session.
fn detail_text(
    panel: &WebhookPanelState,
    details: &[CronDetail],
    private: bool,
    here: bool,
) -> String {
    if let Some((_, message)) = &panel.pending {
        return format!("Not saved: {message}\n\ne edits your form again; Esc drops it.");
    }
    match &panel.confirm {
        Some(WebhookConfirm::Delete(webhook)) => {
            return format!(
                "Delete {}? Its URL stops working at once; its sessions stay. y deletes; Esc cancels.",
                webhook_title(webhook)
            );
        }
        Some(WebhookConfirm::Rotate(webhook, WebhookRotate::Url)) => {
            return format!(
                "Give {} a new URL? The old one stops working at once, so the sender needs the new one. y confirms; Esc cancels.",
                webhook_title(webhook)
            );
        }
        Some(WebhookConfirm::Rotate(webhook, WebhookRotate::Secret)) => {
            return format!(
                "Make a new secret for {}? Requests signed with the old one are refused at once. y confirms; Esc cancels.",
                webhook_title(webhook)
            );
        }
        None => {}
    }
    if let Some((title, secret)) = &panel.secret {
        return format!(
            "The secret of {title}:\n\n{secret}\n\nGive it to the sender. c copies it; Esc hides it."
        );
    }
    match &panel.listing {
        None => return "Loading webhooks...".into(),
        Some(Err(error)) => return format!("Webhooks are unavailable: {error}"),
        Some(Ok(_)) => {}
    }
    let Some(webhook) = panel.selected_webhook() else {
        let into = if here {
            "n makes one that delivers into this session, s one that starts new sessions."
        } else {
            "s makes one that starts a new session per event, or per thread key."
        };
        return format!("No webhooks yet. {into}");
    };
    let mut lines: Vec<String> = details
        .iter()
        .filter(|detail| detail.session.is_none())
        .map(|detail| format!("{}: {}", detail.label, detail.value))
        .collect();
    if private {
        lines.insert(
            2.min(lines.len()),
            "! This URL works only where cookie's page can be reached; set [public] url for senders on the internet.".into(),
        );
    }
    if let Some(prompt) = &webhook.prompt {
        lines.push(String::new());
        lines.push(prompt.clone());
    }
    if let WebhookTarget::New(WebhookTemplate {
        thread_prompt: Some(thread_prompt),
        ..
    }) = &webhook.target
    {
        lines.push(String::new());
        lines.push("Thread prompt:".into());
        lines.push(thread_prompt.clone());
    }
    lines.join("\n")
}

/// Whether the details pane shows the selected webhook's details.
fn shows_details(panel: &WebhookPanelState) -> bool {
    panel.pending.is_none()
        && panel.confirm.is_none()
        && panel.secret.is_none()
        && matches!(panel.listing, Some(Ok(_)))
        && panel.selected_webhook().is_some()
}

/// The history pane's lines, newest first.
fn history_lines(webhook: Option<&Webhook>) -> Vec<String> {
    let Some(webhook) = webhook else {
        return Vec::new();
    };
    if webhook.history.is_empty() {
        return vec!["No events yet. p sends a test event.".into()];
    }
    let zone = TimeZone::system();
    events_newest_first(webhook)
        .map(|event| {
            let thread = event
                .thread_key
                .as_ref()
                .map_or_else(String::new, |key| format!("  thread {key}"));
            let test = if event.test { "  test" } else { "" };
            format!(
                "{}  {}{thread}{test}",
                event_time(event, &zone),
                event_outcome(event)
            )
        })
        .collect()
}

/// An event's request as the event view shows it.
fn event_text(webhook: Option<&Webhook>, view: &EventView) -> String {
    let zone = TimeZone::system();
    let event = webhook.and_then(|webhook| events_newest_first(webhook).nth(view.index));
    let mut lines = Vec::new();
    if let Some(event) = event {
        lines.push(format!(
            "{}  {}  {} bytes",
            event_time(event, &zone),
            event_outcome(event),
            event.bytes
        ));
        if let Some(delivery) = &event.delivery_id {
            lines.push(format!("delivery: {delivery}"));
        }
        if let Some(key) = &event.thread_key {
            lines.push(format!("thread: {key}"));
        }
        lines.push(String::new());
    }
    match &view.request {
        None => lines.push("Loading the request...".into()),
        Some(Err(error)) => lines.push(format!("The request is unavailable: {error}")),
        Some(Ok(request)) => {
            lines.push(request.method.clone());
            for header in &request.headers {
                lines.push(format!("{}: {}", header.name, header.value));
            }
            lines.push(String::new());
            match (&request.body_text, &request.body_base64) {
                (Some(text), _) => lines.push(pretty_body(text)),
                (None, Some(base64)) => {
                    lines.push(format!("(binary, {} base64 characters)", base64.len()));
                }
                (None, None) => lines.push("(no body)".into()),
            }
        }
    }
    lines.join("\n")
}

/// A JSON body pretty-printed, anything else as it came.
fn pretty_body(text: &str) -> String {
    serde_json::from_str::<serde_json::Value>(text)
        .ok()
        .and_then(|value| serde_json::to_string_pretty(&value).ok())
        .unwrap_or_else(|| text.to_owned())
}

impl App {
    /// Whether the daemon keeps webhooks (client protocol 1.96).
    pub(in crate::ui) fn daemon_has_webhooks(&self) -> bool {
        daemon_has_webhooks(self.client.daemon_version())
    }

    pub(in crate::ui) fn open_webhook_panel(&mut self) {
        if !self.daemon_has_webhooks() {
            self.status =
                "This daemon does not keep webhooks; it needs client protocol 1.96.".into();
            return;
        }
        self.webhook_panel = WebhookPanelState::default();
        self.modal = Modal::Webhooks;
        self.poll_webhooks();
    }

    /// `/webhooks new`: the panel, with a new webhook's form for the
    /// session on screen.
    pub(in crate::ui) fn webhook_for_session(&mut self) {
        self.open_webhook_panel();
        if self.modal == Modal::Webhooks {
            self.begin_new_webhook(true);
        }
    }

    /// Fetches `webhooks.list` for the open panel.
    pub(super) fn poll_webhooks(&mut self) {
        if self.modal != Modal::Webhooks || !self.webhook_panel.refresh.start() {
            return;
        }
        let client = self.client.clone();
        let updates = self.rpc_updates_tx.clone();
        self.spawn_rpc(async move {
            let result = client
                .list_webhooks()
                .await
                .map_err(|error| message_of(&error));
            let _ = updates.send(RpcUpdate::WebhooksListed { result });
        });
    }

    pub(in crate::ui) fn install_webhook_listing(
        &mut self,
        result: Result<WebhooksListResult, String>,
    ) {
        let again = self.webhook_panel.refresh.landed();
        self.webhook_panel.install(result);
        if again {
            self.poll_webhooks();
        }
    }

    /// The session on screen as a target: its ID and whether it is a
    /// delegated one.
    fn viewed_webhook_session(&self) -> Option<(SessionId, bool)> {
        let session = self.viewed_session()?;
        let subagent = self
            .selected_session_meta()
            .is_some_and(|meta| matches!(meta.origin, SessionOrigin::Delegated { .. }));
        Some((session, subagent))
    }

    /// What a new-session webhook starts from by default: the current
    /// agent, its preset, the session's project, and this directory.
    fn default_webhook_template(&self) -> Option<WebhookTemplate> {
        let selection = self
            .draft
            .clone()
            .or_else(|| {
                self.selected_session_meta()
                    .map(|meta| meta.creation_selection.clone())
            })
            .or_else(|| self.default_draft_selection())?;
        let mut template = WebhookTemplate::for_agent(selection.agent);
        template.preset = selection.preset;
        template.project = self
            .selected_session_meta()
            .and_then(|meta| meta.project.clone());
        template.cwd = self.cwd.clone();
        Some(template)
    }

    fn webhook_session_title(&self, session: SessionId) -> Option<String> {
        self.sessions
            .iter()
            .find(|meta| meta.session_id == session)
            .and_then(|meta| meta.title.as_ref())
            .map(ToString::to_string)
    }

    fn webhook_target_label(&self, target: &WebhookTarget) -> String {
        target_label(target, &self.projects, |session| {
            self.webhook_session_title(session)
        })
    }

    /// Opens a new webhook's form: into the session on screen when `here`
    /// and one is, else for new sessions.
    fn begin_new_webhook(&mut self, here: bool) {
        let draft = match (here.then(|| self.viewed_webhook_session()).flatten(), here) {
            (Some((session, subagent)), _) => WebhookDraft::for_session(session, subagent),
            (None, true) => {
                self.status = "No session is open to deliver into.".into();
                return;
            }
            (None, false) => match self.default_webhook_template() {
                Some(template) => WebhookDraft::for_new_session(template),
                None => {
                    self.status = "No agent is available to run a new-session webhook.".into();
                    return;
                }
            },
        };
        let target = self.webhook_target_label(&draft.target.target());
        let text = draft.form_text(&target);
        self.status = "editing a new webhook".into();
        self.pending_webhook_editor = Some(WebhookEditRequest {
            webhook: None,
            draft,
            text: text.clone(),
            original: text,
        });
    }

    fn begin_webhook_edit(&mut self, webhook: Webhook) {
        let draft = WebhookDraft::of_webhook(&webhook);
        let target = self.webhook_target_label(&webhook.target);
        let text = draft.form_text(&target);
        self.status = format!("editing {}", webhook_title(&webhook));
        self.pending_webhook_editor = Some(WebhookEditRequest {
            webhook: Some(webhook),
            draft,
            text: text.clone(),
            original: text,
        });
    }

    /// What the editor left for `request`: sent unless unchanged.
    pub(in crate::ui) fn finish_webhook_edit(
        &mut self,
        request: WebhookEditRequest,
        edited: Result<String, String>,
    ) {
        let text = match edited {
            Ok(text) => text,
            Err(error) => {
                self.status = error;
                return;
            }
        };
        if text == request.original {
            self.webhook_panel.pending = None;
            self.status = match &request.webhook {
                Some(webhook) => format!("No changes to {}.", webhook_title(webhook)),
                None => "No changes; the webhook was not created.".into(),
            };
            return;
        }
        let request = WebhookEditRequest { text, ..request };
        let mut draft = request.draft.clone();
        if let Err(message) = draft.apply_form(&request.text) {
            return self.refuse_webhook_form(request, message);
        }
        match &request.webhook {
            None => match draft.create_params() {
                Ok(params) => self.dispatch_webhook_save(request, WebhookSave::Create(params)),
                Err(message) => self.refuse_webhook_form(request, message),
            },
            Some(webhook) => match draft.update_params(webhook) {
                Ok(params) if !update_changes_anything(&params) => {
                    self.webhook_panel.pending = None;
                    self.status = format!("No changes to {}.", webhook_title(webhook));
                }
                Ok(params) => self.dispatch_webhook_save(request, WebhookSave::Update(params)),
                Err(message) => self.refuse_webhook_form(request, message),
            },
        }
    }

    fn refuse_webhook_form(&mut self, request: WebhookEditRequest, message: String) {
        self.status = format!("Not saved: {message}");
        self.webhook_panel.pending = Some((request, message));
    }

    fn dispatch_webhook_save(&mut self, request: WebhookEditRequest, save: WebhookSave) {
        self.status = "saving the webhook…".into();
        let client = self.client.clone();
        let updates = self.rpc_updates_tx.clone();
        self.spawn_rpc(async move {
            let result = match save {
                WebhookSave::Create(params) => client.create_webhook(params).await,
                WebhookSave::Update(params) => client.update_webhook(params).await,
            }
            .map(|result| (result.webhook, result.secret))
            .map_err(|error| message_of(&error));
            let _ = updates.send(RpcUpdate::WebhookSaved {
                request: Box::new(request),
                result: Box::new(result),
            });
        });
    }

    pub(in crate::ui) fn install_webhook_save(
        &mut self,
        request: WebhookEditRequest,
        result: Result<(Webhook, Option<String>), String>,
    ) {
        match result {
            Ok((webhook, secret)) => {
                self.webhook_panel.pending = None;
                let verb = if request.webhook.is_some() {
                    "Saved"
                } else {
                    "Created"
                };
                let (url, _) = url_of(&self.webhook_panel, &webhook, self.page_url.as_deref());
                self.status = format!("{verb} {}: {url}", webhook_title(&webhook));
                if let Some(secret) = secret {
                    self.webhook_panel.secret = Some((webhook_title(&webhook), secret));
                }
                self.webhook_panel.focus = Some(webhook.id);
            }
            Err(message) => self.refuse_webhook_form(request, message),
        }
        self.poll_webhooks();
    }

    /// Runs a change of one webhook; the answer is a status line, and a
    /// secret to show when it made or read one.
    fn dispatch_webhook_change(&mut self, pending: String, change: WebhookChange) {
        self.status = pending;
        let client = self.client.clone();
        let updates = self.rpc_updates_tx.clone();
        let page = self.page_url.clone();
        let public = self.webhook_panel.public_base().map(str::to_owned);
        self.spawn_rpc(async move {
            let failed =
                |title: &str, what: &str, error: &cookie_agent_protocol_transport::ClientError| {
                    format!("{title} {what}: {}", message_of(error))
                };
            let result = match change {
                WebhookChange::Enabled { title, id, enabled } => {
                    let mut params = WebhookUpdateParams::new(id);
                    params.enabled = Some(enabled);
                    let verb = if enabled { "Enabled" } else { "Disabled" };
                    client
                        .update_webhook(params)
                        .await
                        .map(|_| (format!("{verb} {title}."), None))
                        .map_err(|error| failed(&title, "was not changed", &error))
                }
                WebhookChange::Test { title, id } => client
                    .test_webhook(WebhookTestParams {
                        id,
                        body: None,
                        content_type: None,
                        headers: Vec::new(),
                    })
                    .await
                    .map(|result| {
                        let outcome = result
                            .webhook
                            .history
                            .iter()
                            .find(|event| event.id == result.event)
                            .map_or_else(|| "accepted".to_owned(), event_outcome);
                        (format!("Sent a test event to {title}: {outcome}."), None)
                    })
                    .map_err(|error| failed(&title, "got no test event", &error)),
                WebhookChange::Rotate { title, id, what } => client
                    .rotate_webhook(id, what)
                    .await
                    .map(|result| match what {
                        WebhookRotate::Url => {
                            let url = webhook_url(
                                public.as_deref(),
                                page.as_deref().unwrap_or_default(),
                                &result.webhook.path,
                            );
                            (format!("{title} has a new URL: {url}"), None)
                        }
                        WebhookRotate::Secret => (
                            format!("{title} has a new secret."),
                            result.secret.map(|secret| (title.clone(), secret)),
                        ),
                    })
                    .map_err(|error| failed(&title, "was not changed", &error)),
                WebhookChange::Secret { title, id } => client
                    .webhook_secret(id)
                    .await
                    .map(|result| match result.secret {
                        Some(secret) => (String::new(), Some((title.clone(), secret))),
                        None => (format!("{title} has no secret."), None),
                    })
                    .map_err(|error| failed(&title, "did not show its secret", &error)),
                WebhookChange::Delete { title, id } => client
                    .delete_webhook(id)
                    .await
                    .map(|_| (format!("Deleted {title}."), None))
                    .map_err(|error| failed(&title, "was not deleted", &error)),
            };
            let _ = updates.send(RpcUpdate::WebhookChanged { result });
        });
    }

    pub(in crate::ui) fn install_webhook_change(
        &mut self,
        result: Result<(String, Option<(String, String)>), String>,
    ) {
        match result {
            Ok((status, secret)) => {
                if !status.is_empty() {
                    self.status = status;
                }
                if secret.is_some() {
                    self.webhook_panel.secret = secret;
                }
            }
            Err(error) => self.status = error,
        }
        self.poll_webhooks();
    }

    /// Opens the event at `index` (newest first) of the selected webhook.
    pub(in crate::ui) fn open_webhook_event(&mut self, index: usize) {
        let Some(webhook) = self.webhook_panel.selected_webhook() else {
            return;
        };
        let Some(event) = events_newest_first(webhook).nth(index) else {
            return;
        };
        let (id, event) = (webhook.id.clone(), event.id.clone());
        self.webhook_panel.event = Some(EventView {
            webhook: id.clone(),
            index,
            request: None,
            scroll: 0,
        });
        let client = self.client.clone();
        let updates = self.rpc_updates_tx.clone();
        self.spawn_rpc(async move {
            let result = client
                .webhook_event(id.clone(), event)
                .await
                .map_err(|error| message_of(&error));
            let _ = updates.send(RpcUpdate::WebhookEventRead {
                webhook: id,
                index,
                result: Box::new(result),
            });
        });
    }

    pub(in crate::ui) fn install_webhook_event(
        &mut self,
        webhook: WebhookId,
        index: usize,
        result: Result<WebhookEventResult, String>,
    ) {
        if let Some(view) = &mut self.webhook_panel.event
            && view.webhook == webhook
            && view.index == index
        {
            view.request = Some(result);
        }
    }

    /// Acts on `action`, from its key or its button.
    pub(in crate::ui) fn activate_webhook_action(&mut self, action: WebhookAction) {
        let here = self.viewed_webhook_session().is_some();
        if !self.webhook_panel.actions(here).contains(&action) {
            return;
        }
        let selected = self.webhook_panel.selected_webhook().cloned();
        let title = selected.as_ref().map(webhook_title).unwrap_or_default();
        match action {
            WebhookAction::Edit => {
                if let Some(webhook) = selected {
                    self.begin_webhook_edit(webhook);
                }
            }
            WebhookAction::NewHere => self.begin_new_webhook(true),
            WebhookAction::NewSessions => self.begin_new_webhook(false),
            WebhookAction::Enable | WebhookAction::Disable => {
                if let Some(webhook) = selected {
                    let enabled = action == WebhookAction::Enable;
                    let verb = if enabled { "enabling" } else { "disabling" };
                    self.dispatch_webhook_change(
                        format!("{verb} {title}…"),
                        WebhookChange::Enabled {
                            title,
                            id: webhook.id,
                            enabled,
                        },
                    );
                }
            }
            WebhookAction::Test => {
                if let Some(webhook) = selected {
                    self.dispatch_webhook_change(
                        format!("sending a test event to {title}…"),
                        WebhookChange::Test {
                            title,
                            id: webhook.id,
                        },
                    );
                }
            }
            WebhookAction::CopyUrl => {
                if let Some(webhook) = selected {
                    let (url, _) = url_of(&self.webhook_panel, &webhook, self.page_url.as_deref());
                    self.copy_to_clipboard(url);
                }
            }
            WebhookAction::RevealSecret => {
                if let Some(webhook) = selected {
                    self.dispatch_webhook_change(
                        format!("reading the secret of {title}…"),
                        WebhookChange::Secret {
                            title,
                            id: webhook.id,
                        },
                    );
                }
            }
            WebhookAction::RotateUrl | WebhookAction::RotateSecret => {
                if let Some(webhook) = selected {
                    let what = if action == WebhookAction::RotateUrl {
                        WebhookRotate::Url
                    } else {
                        WebhookRotate::Secret
                    };
                    self.webhook_panel.confirm = Some(WebhookConfirm::Rotate(webhook, what));
                }
            }
            WebhookAction::Events => self.open_webhook_event(0),
            WebhookAction::Delete => {
                self.webhook_panel.confirm = selected.map(WebhookConfirm::Delete);
            }
            WebhookAction::Confirm => match self.webhook_panel.confirm.take() {
                Some(WebhookConfirm::Delete(webhook)) => {
                    let title = webhook_title(&webhook);
                    self.dispatch_webhook_change(
                        format!("deleting {title}…"),
                        WebhookChange::Delete {
                            title,
                            id: webhook.id,
                        },
                    );
                }
                Some(WebhookConfirm::Rotate(webhook, what)) => {
                    let title = webhook_title(&webhook);
                    self.dispatch_webhook_change(
                        format!("rotating {title}…"),
                        WebhookChange::Rotate {
                            title,
                            id: webhook.id,
                            what,
                        },
                    );
                }
                None => {}
            },
            WebhookAction::CopySecret => {
                if let Some((_, secret)) = self.webhook_panel.secret.clone() {
                    self.copy_to_clipboard(secret);
                }
            }
            WebhookAction::OlderEvent | WebhookAction::NewerEvent => {
                if let Some(view) = &self.webhook_panel.event {
                    let index = if action == WebhookAction::OlderEvent {
                        view.index + 1
                    } else {
                        view.index.saturating_sub(1)
                    };
                    self.open_webhook_event(index);
                }
            }
            WebhookAction::Cancel => {
                let panel = &mut self.webhook_panel;
                if panel.confirm.take().is_some() {
                    self.status = "Cancelled.".into();
                } else if panel.pending.take().is_some() {
                    self.status = "Your form was dropped.".into();
                } else if panel.secret.take().is_none() {
                    panel.event = None;
                }
            }
            WebhookAction::EditAgain => {
                if let Some((request, _)) = self.webhook_panel.pending.take() {
                    self.pending_webhook_editor = Some(request);
                }
            }
        }
    }

    pub(super) async fn handle_webhook_key(&mut self, key: KeyEvent) {
        let here = self.viewed_webhook_session().is_some();
        let actions = self.webhook_panel.actions(here);
        if key.code == KeyCode::Esc && !actions.contains(&WebhookAction::Cancel) {
            self.modal = Modal::None;
            return;
        }
        if let Some(view) = &mut self.webhook_panel.event {
            match key.code {
                KeyCode::Up => view.scroll = view.scroll.saturating_sub(1),
                KeyCode::Down => view.scroll = view.scroll.saturating_add(1),
                KeyCode::PageUp => view.scroll = view.scroll.saturating_sub(10),
                KeyCode::PageDown => view.scroll = view.scroll.saturating_add(10),
                _ => {}
            }
        }
        let quiet = self.webhook_panel.confirm.is_none()
            && self.webhook_panel.pending.is_none()
            && self.webhook_panel.secret.is_none()
            && self.webhook_panel.event.is_none();
        match key.code {
            KeyCode::Enter if quiet => return self.activate_webhook_action(WebhookAction::Edit),
            KeyCode::Char(key @ ('o' | 'i')) if quiet => {
                let session = self.webhook_panel.selected_webhook().and_then(|webhook| {
                    if key == 'o' {
                        webhook.target.session()
                    } else {
                        match &webhook.created_by {
                            CronActor::Agent(agent) => Some(agent.session),
                            CronActor::User => None,
                        }
                    }
                });
                if let Some(session) = session {
                    self.open_webhook_session(session).await;
                }
                return;
            }
            KeyCode::Up | KeyCode::Down if quiet => {
                let count = self.webhook_panel.webhooks().len();
                super::move_picker_selection(
                    &mut self.webhook_panel.selection,
                    count,
                    key.code == KeyCode::Up,
                );
                return;
            }
            _ => {}
        }
        if let Some(action) = actions.into_iter().find(|action| action.matches(key.code)) {
            self.activate_webhook_action(action);
        }
    }

    /// Shows `session`'s transcript, closing the panel.
    pub(in crate::ui) async fn open_webhook_session(&mut self, session: SessionId) {
        self.modal = Modal::None;
        self.open_session(session).await;
    }

    pub(super) fn render_webhook_panel(&mut self, frame: &mut Frame) {
        let area = centered(frame.area(), 92, 84);
        paint_panel(frame, area, &self.theme);
        let key = (
            self.webhook_panel.listing_revision,
            self.sessions_revision,
            self.sessions.len(),
        );
        let targets = match self.webhook_panel.targets.take() {
            Some((cached, targets)) if cached == key => targets,
            _ => self
                .webhook_panel
                .webhooks()
                .iter()
                .map(|webhook| self.webhook_target_label(&webhook.target))
                .collect(),
        };
        let (details, private) = self.webhook_panel.selected_webhook().map_or_else(
            || (Vec::new(), false),
            |webhook| {
                let (url, private) = url_of(&self.webhook_panel, webhook, self.page_url.as_deref());
                (
                    detail_lines(
                        webhook,
                        &url,
                        &TimeZone::system(),
                        &self.projects,
                        |session| self.webhook_session_title(session),
                    ),
                    private,
                )
            },
        );
        let here = self.viewed_webhook_session().is_some();
        self.hit_map.webhooks = render_webhooks(
            frame,
            area,
            &mut self.webhook_panel,
            &targets,
            &details,
            Shown { private, here },
            &self.theme,
        );
        self.webhook_panel.targets = Some((key, targets));
    }
}

/// A form's request to the daemon.
enum WebhookSave {
    Create(cookie_agent_protocol::WebhookCreateParams),
    Update(WebhookUpdateParams),
}

/// A change to one webhook, named by its title for the status line.
enum WebhookChange {
    Enabled {
        title: String,
        id: WebhookId,
        enabled: bool,
    },
    Test {
        title: String,
        id: WebhookId,
    },
    Rotate {
        title: String,
        id: WebhookId,
        what: WebhookRotate,
    },
    Secret {
        title: String,
        id: WebhookId,
    },
    Delete {
        title: String,
        id: WebhookId,
    },
}

/// What the frame knows besides the panel: whether the URL looks private
/// and whether a session is on screen.
#[derive(Clone, Copy)]
struct Shown {
    private: bool,
    here: bool,
}

/// The panel: the webhooks, the selected one's details and history (or an
/// event's request), and the actions.
fn render_webhooks(
    frame: &mut Frame,
    area: Rect,
    panel: &mut WebhookPanelState,
    targets: &[String],
    details: &[CronDetail],
    shown: Shown,
    theme: &Theme,
) -> WebhookHits {
    let mut hits = WebhookHits::default();
    let count = panel.webhooks().len();
    panel
        .selection
        .select((count > 0).then(|| panel.selection.selected().unwrap_or(0).min(count - 1)));
    let [list_area, bottom_area, actions_area] = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(4),
            Constraint::Percentage(60),
            Constraint::Length(2),
        ])
        .areas(area);
    let items = panel
        .webhooks()
        .iter()
        .zip(targets)
        .map(|(webhook, target)| ListItem::new(row_line(webhook, target, theme)))
        .collect::<Vec<_>>();
    let receiving = !matches!(&panel.listing, Some(Ok(listing)) if !listing.receiving);
    let title = if receiving {
        "Webhooks"
    } else {
        "Webhooks · receiving is off ([webhooks] enabled = false)"
    };
    let block = crate::ui::panel_block()
        .border_style(theme.panel_border())
        .title(crate::ui::panel_title(title))
        .title_bottom(crate::ui::panel_title(
            Line::from(Span::styled(
                "↑↓ select | o open target | v events | esc close",
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
    let quiet = panel.confirm.is_none() && panel.pending.is_none() && panel.secret.is_none();
    if quiet && panel.event.is_none() {
        let offset = panel.selection.offset();
        for (line, index) in (offset..count).take(usize::from(inner.height)).enumerate() {
            let y = inner.y + u16::try_from(line).unwrap_or(u16::MAX);
            hits.rows
                .push((Rect::new(inner.x, y, inner.width, 1), index));
        }
    }
    if let Some(view) = &panel.event {
        let webhook = panel.webhooks().iter().find(|hook| hook.id == view.webhook);
        let title = webhook.map_or_else(String::new, webhook_title);
        frame.render_widget(Clear, bottom_area);
        frame.render_widget(
            Paragraph::new(event_text(webhook, view))
                .wrap(Wrap { trim: false })
                .scroll((view.scroll, 0))
                .block(
                    crate::ui::panel_block()
                        .border_style(theme.panel_border())
                        .title(crate::ui::panel_title(format!(
                            "{title} · event {} · ↑↓ scroll",
                            view.index + 1
                        ))),
                ),
            bottom_area,
        );
    } else {
        render_details(frame, bottom_area, panel, details, shown, theme, &mut hits);
    }
    let mut lines = vec![Vec::new(), Vec::new()];
    let mut row = 0;
    let mut x = actions_area.x;
    for action in panel.actions(shown.here) {
        let label = format!(" {} ({}) ", action.label(), action.key_label());
        let width = u16::try_from(label.chars().count()).unwrap_or(u16::MAX);
        if x.saturating_add(width) > actions_area.right() && row == 0 {
            row = 1;
            x = actions_area.x;
        }
        if x.saturating_add(width) <= actions_area.right() && row < 2 {
            let y = actions_area.y + u16::try_from(row).unwrap_or(0);
            if y < actions_area.bottom() {
                hits.actions.push((Rect::new(x, y, width, 1), action));
            }
            lines[row].push(Span::styled(label, theme.link()));
            lines[row].push(Span::raw(" "));
            x = x.saturating_add(width + 1);
        }
    }
    frame.render_widget(
        Paragraph::new(lines.into_iter().map(Line::from).collect::<Vec<_>>()),
        actions_area,
    );
    hits
}

/// The details pane beside the history pane.
fn render_details(
    frame: &mut Frame,
    area: Rect,
    panel: &WebhookPanelState,
    details: &[CronDetail],
    shown: Shown,
    theme: &Theme,
    hits: &mut WebhookHits,
) {
    let [detail_area, history_area] = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(58), Constraint::Percentage(42)])
        .areas(area);
    let detail_block = crate::ui::panel_block()
        .border_style(theme.panel_border())
        .title(crate::ui::panel_title("Details"));
    let mut detail_inner = detail_block.inner(detail_area);
    frame.render_widget(detail_block, detail_area);
    let linked: Vec<_> = if shows_details(panel) {
        details
            .iter()
            .filter_map(|detail| Some((detail, detail.session?)))
            .collect()
    } else {
        Vec::new()
    };
    for (detail, session) in linked {
        if detail_inner.height == 0 {
            break;
        }
        let key = if detail.label == "target" { "o" } else { "i" };
        let label = Span::raw(format!("{}: ", detail.label));
        let value = Span::styled(detail.value.clone(), theme.link());
        let row = Rect::new(detail_inner.x, detail_inner.y, detail_inner.width, 1);
        let label_width = u16::try_from(label.width()).unwrap_or(u16::MAX);
        let value_width = u16::try_from(value.width()).unwrap_or(u16::MAX);
        let link = Rect::new(
            row.x.saturating_add(label_width),
            row.y,
            value_width.min(row.width.saturating_sub(label_width)),
            1,
        );
        if link.width > 0 {
            hits.sessions.push((link, session));
        }
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                label,
                value,
                Span::styled(format!("  ({key})"), theme.muted()),
            ])),
            row,
        );
        detail_inner.y += 1;
        detail_inner.height -= 1;
    }
    frame.render_widget(
        Paragraph::new(detail_text(panel, details, shown.private, shown.here))
            .wrap(Wrap { trim: false }),
        detail_inner,
    );
    let history_block = crate::ui::panel_block()
        .border_style(theme.panel_border())
        .title(crate::ui::panel_title("Events, newest first"));
    let history_inner = history_block.inner(history_area);
    frame.render_widget(history_block, history_area);
    let webhook = shows_details(panel)
        .then(|| panel.selected_webhook())
        .flatten();
    let lines = history_lines(webhook);
    let clickable = webhook.is_some_and(|webhook| !webhook.history.is_empty());
    for (index, text) in lines
        .into_iter()
        .take(usize::from(history_inner.height))
        .enumerate()
    {
        let y = history_inner.y + u16::try_from(index).unwrap_or(u16::MAX);
        let row = Rect::new(history_inner.x, y, history_inner.width, 1);
        if clickable {
            hits.events.push((row, index));
        }
        frame.render_widget(Paragraph::new(text), row);
    }
}

/// A listing with an in-session webhook that has received an event and a
/// disabled new-session webhook with a GitHub check and a thread key.
#[cfg(test)]
pub(in crate::ui) fn sample_webhooks() -> WebhooksListResult {
    serde_json::from_value(serde_json::json!({
        "webhooks": [
            {
                "id": "w7k2m9qa",
                "name": "CI failures",
                "url_token": "Xq3vT9aLk2M",
                "path": "/hooks/Xq3vT9aLk2M",
                "prompt": "Look at the failed build.",
                "target": {"session": "0190a5b2-0000-7000-8000-000000000001"},
                "delivery": "queue",
                "enabled": true,
                "created_by": "user",
                "created_at": "2026-11-12T18:00:00Z",
                "counts": {"accepted": 1},
                "history": [{
                    "id": "e3j8x0pq",
                    "received_at": "2026-11-12T18:04:11Z",
                    "bytes": 120,
                    "content_type": "application/json",
                    "outcome": {"kind": "delivered"},
                    "session_id": "0190a5b2-0000-7000-8000-000000000001"
                }]
            },
            {
                "id": "m7p2r4st",
                "url_token": "Ab3vT9aLk2M",
                "path": "/hooks/Ab3vT9aLk2M",
                "target": {"new": {
                    "agent": "builder",
                    "thread_key": {"json": "/issue/number"},
                    "thread_prompt": "You look after one GitHub issue."
                }},
                "verify": {"kind": "github"},
                "filter": [{"header": "X-GitHub-Event", "one_of": ["issues"]}],
                "permission_mode": "auto_approve_n",
                "enabled": false,
                "disabled_reason": {"kind": "failed", "failures": 3},
                "created_by": "user",
                "created_at": "2026-11-11T08:00:00Z",
                "counts": {},
                "consecutive_failures": 3,
                "history": []
            }
        ],
        "receiving": true,
        "public_base": "https://hooks.example.com"
    }))
    .expect("sample webhooks")
}
