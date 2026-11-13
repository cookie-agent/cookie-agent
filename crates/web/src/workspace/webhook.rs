//! The Webhooks view's calls (`webhooks.*`, client protocol 1.96.0): the
//! listing, kept fresh by `webhooks.changed`, the webhook form, a
//! webhook's actions, the secret shown once, and an event's request. The
//! view is `crate::views::webhooks`.

use cookie_agent_client_state::webhook::{
    WebhookDraft, update_changes_anything, url_looks_private, webhook_title, webhook_url,
};
use cookie_agent_protocol::{
    SessionId, Webhook, WebhookEventResult, WebhookId, WebhookRotate, WebhookTemplate,
    WebhookTestParams, WebhookUpdateParams,
};
use leptos::{prelude::*, task::spawn_local};

use super::Workspace;
use crate::{layout::View, webhook::new_template};

/// The webhook form: what it edits and the targets it switches between.
#[derive(Clone, Debug, PartialEq)]
pub struct WebhookForm {
    /// The webhook an edit changes; `None` for a new one.
    pub editing: Option<Webhook>,
    /// The form as it opened.
    pub draft: WebhookDraft,
    /// The session "this session" is, and whether it is a subagent's;
    /// `None` when the form opened without one.
    pub session: Option<(SessionId, bool)>,
    /// The template "a new session per event" starts from.
    pub template: WebhookTemplate,
}

/// A secret or URL the daemon returned once, shown until dismissed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WebhookRevealed {
    pub webhook: WebhookId,
    /// The webhook's title, for the note.
    pub title: String,
    /// The signing secret or bearer token; `None` when only the URL is new.
    pub secret: Option<String>,
    /// Whether the URL is new (a create, or a rotated URL).
    pub new_url: bool,
}

/// An event open in the view: which, and its request once read.
#[derive(Clone, Debug, PartialEq)]
pub struct WebhookEventOpen {
    pub webhook: WebhookId,
    pub event: String,
    pub request: Option<Result<WebhookEventResult, String>>,
}

impl Workspace {
    /// Reads `webhooks.list`, when the daemon keeps webhooks.
    pub fn poll_webhooks(self) {
        if self.is_guest() || !self.has_webhooks.get_untracked() {
            return;
        }
        let Some(client) = self.client() else {
            return;
        };
        let read = self.begin_read(|reads| &mut reads.webhooks);
        spawn_local(async move {
            let result = client
                .list_webhooks()
                .await
                .map_err(|error| crate::errors::describe_full(&error));
            if self.read_is_current(|reads| &mut reads.webhooks, read) {
                self.webhooks.set(Some(result));
            }
        });
    }

    /// Opening the view: the list, with no form open.
    pub(super) fn open_webhooks(self) {
        self.webhook_form.set(None);
        self.webhook_error.set(None);
        self.webhook_event.set(None);
        self.poll_webhooks();
    }

    /// Opens the view at webhook `id`: a webhook session's mark.
    pub fn open_webhook(self, id: WebhookId) {
        self.open_view(View::Webhooks, None);
        self.webhook_selected.set(Some(id));
    }

    /// The webhooks listed, if they are.
    pub fn webhook_list(self) -> Vec<Webhook> {
        self.webhooks.with(|webhooks| {
            webhooks
                .as_ref()
                .and_then(|webhooks| webhooks.as_ref().ok())
                .map(|webhooks| webhooks.webhooks.clone())
                .unwrap_or_default()
        })
    }

    /// The names of the cron jobs and webhooks the page knows, for the
    /// labels of their thread prompts.
    pub fn thread_owner_names(self) -> Vec<(cookie_agent_protocol::ThreadOwnerRef, String)> {
        use cookie_agent_protocol::ThreadOwnerRef;
        let mut names: Vec<(ThreadOwnerRef, String)> = self
            .webhook_list()
            .iter()
            .map(|webhook| {
                (
                    ThreadOwnerRef::Webhook(webhook.id.clone()),
                    webhook_title(webhook),
                )
            })
            .collect();
        names.extend(self.cron_jobs().iter().map(|job| {
            (
                ThreadOwnerRef::Cron(job.id.clone()),
                cookie_agent_client_state::cron::job_title(job),
            )
        }));
        names
    }

    /// The daemon's public base, as `webhooks.list` gave it.
    fn webhook_public_base(self) -> Option<String> {
        self.webhooks.with_untracked(|webhooks| {
            webhooks
                .as_ref()
                .and_then(|webhooks| webhooks.as_ref().ok())
                .and_then(|webhooks| webhooks.public_base.clone())
        })
    }

    /// `webhook`'s URL: below the daemon's public base, else the address
    /// this page reached the daemon at.
    pub fn webhook_url(self, webhook: &Webhook) -> String {
        webhook_url(
            self.webhook_public_base().as_deref(),
            &self.share_page_base.get_untracked(),
            &webhook.path,
        )
    }

    /// Whether webhook URLs probably work only where this page can be
    /// reached.
    pub fn webhook_urls_private(self) -> bool {
        url_looks_private(
            self.webhook_public_base().as_deref(),
            &self.share_page_base.get_untracked(),
        )
    }

    /// The template a new-session webhook starts from: the one a new
    /// cron job would.
    fn webhook_template(self) -> WebhookTemplate {
        match self.cron_template() {
            Some(template) => new_template(template.agent, template.preset, template.project),
            None => new_template(self.default_agent(), None, None),
        }
    }

    /// The ⋮ menu's "Add a webhook": the form, for a webhook that delivers
    /// into the open session.
    pub fn add_webhook(self) {
        let Some((session, subagent)) = self.cron_session() else {
            self.status
                .set("open a session before adding a webhook to it".into());
            return;
        };
        self.open_view(View::Webhooks, None);
        self.webhook_form.set(Some(WebhookForm {
            editing: None,
            draft: WebhookDraft::for_session(session, subagent),
            session: Some((session, subagent)),
            template: self.webhook_template(),
        }));
    }

    /// The view's "New": the form, for a webhook that delivers into the
    /// open session, else one that starts a new session per event.
    pub fn new_webhook(self) {
        let session = self.cron_session();
        let template = self.webhook_template();
        let draft = match session {
            Some((session, subagent)) => WebhookDraft::for_session(session, subagent),
            None => WebhookDraft::for_new_session(template.clone()),
        };
        self.webhook_error.set(None);
        self.webhook_form.set(Some(WebhookForm {
            editing: None,
            draft,
            session,
            template,
        }));
    }

    /// The form, filled from `webhook`.
    pub fn edit_webhook(self, webhook: Webhook) {
        let session = webhook.target.session().map(|session| {
            (
                session,
                matches!(
                    webhook.target,
                    cookie_agent_protocol::WebhookTarget::Subagent(_)
                ),
            )
        });
        let template = match &webhook.target {
            cookie_agent_protocol::WebhookTarget::New(template) => template.clone(),
            _ => self.webhook_template(),
        };
        self.webhook_error.set(None);
        self.webhook_form.set(Some(WebhookForm {
            draft: WebhookDraft::of_webhook(&webhook),
            editing: Some(webhook),
            session,
            template,
        }));
    }

    /// Closes the form without sending it.
    pub fn close_webhook_form(self) {
        self.webhook_form.set(None);
        self.webhook_error.set(None);
    }

    /// Sends the form: `webhooks.create` for a new webhook,
    /// `webhooks.update` with what changed for an edit.
    pub fn save_webhook(self, draft: WebhookDraft) {
        let Some(client) = self.client() else {
            return;
        };
        let Some(form) = self.webhook_form.get_untracked() else {
            return;
        };
        let request = match &form.editing {
            None => draft.create_params().map(Ok),
            Some(webhook) => draft.update_params(webhook).map(Err),
        };
        let request = match request {
            Ok(request) => request,
            Err(message) => {
                self.webhook_error.set(Some(message));
                return;
            }
        };
        if let Err(update) = &request
            && !update_changes_anything(update)
        {
            self.close_webhook_form();
            return;
        }
        self.webhook_error.set(None);
        spawn_local(async move {
            let created = request.is_ok();
            let (verb, result) = match request {
                Ok(create) => ("Added", client.create_webhook(create).await),
                Err(update) => ("Saved", client.update_webhook(update).await),
            };
            match result {
                Ok(result) => {
                    let title = webhook_title(&result.webhook);
                    self.status.set(format!("{verb} {title}."));
                    self.webhook_selected.set(Some(result.webhook.id.clone()));
                    if created || result.secret.is_some() {
                        self.webhook_revealed.set(Some(WebhookRevealed {
                            webhook: result.webhook.id.clone(),
                            title,
                            secret: result.secret,
                            new_url: created,
                        }));
                    }
                    self.webhook_form.set(None);
                    self.poll_webhooks();
                }
                Err(error) => self
                    .webhook_error
                    .set(Some(crate::errors::describe_full(&error))),
            }
        });
    }

    /// Turns webhook `id` on or off.
    pub fn set_webhook_enabled(self, id: WebhookId, enabled: bool) {
        let Some(client) = self.client() else {
            return;
        };
        let mut update = WebhookUpdateParams::new(id);
        update.enabled = Some(enabled);
        spawn_local(async move {
            match client.update_webhook(update).await {
                Ok(result) => {
                    let verb = if enabled { "Enabled" } else { "Disabled" };
                    self.status
                        .set(format!("{verb} {}.", webhook_title(&result.webhook)));
                    self.webhook_error.set(None);
                    self.poll_webhooks();
                }
                Err(error) => self
                    .webhook_error
                    .set(Some(crate::errors::describe(&error))),
            }
        });
    }

    /// Sends webhook `id` a test event, skipping its checks and limits.
    pub fn test_webhook(self, id: WebhookId) {
        let Some(client) = self.client() else {
            return;
        };
        let params = WebhookTestParams {
            id,
            body: Some("{\"test\": true}".to_owned()),
            content_type: Some("application/json".to_owned()),
            headers: Vec::new(),
        };
        spawn_local(async move {
            match client.test_webhook(params).await {
                Ok(result) => {
                    self.status.set(format!(
                        "Sent {} a test event.",
                        webhook_title(&result.webhook)
                    ));
                    self.webhook_error.set(None);
                    self.poll_webhooks();
                }
                Err(error) => self
                    .webhook_error
                    .set(Some(crate::errors::describe(&error))),
            }
        });
    }

    /// Replaces webhook `id`'s URL or secret; a new secret is shown once.
    pub fn rotate_webhook(self, id: WebhookId, what: WebhookRotate) {
        let Some(client) = self.client() else {
            return;
        };
        spawn_local(async move {
            match client.rotate_webhook(id, what).await {
                Ok(result) => {
                    let title = webhook_title(&result.webhook);
                    self.status.set(match what {
                        WebhookRotate::Url => format!("{title} has a new URL."),
                        WebhookRotate::Secret => format!("{title} has a new secret."),
                    });
                    self.webhook_revealed.set(Some(WebhookRevealed {
                        webhook: result.webhook.id.clone(),
                        title,
                        secret: result.secret,
                        new_url: what == WebhookRotate::Url,
                    }));
                    self.webhook_error.set(None);
                    self.poll_webhooks();
                }
                Err(error) => self
                    .webhook_error
                    .set(Some(crate::errors::describe(&error))),
            }
        });
    }

    /// Shows webhook `id`'s signing secret.
    pub fn reveal_webhook_secret(self, id: WebhookId, title: String) {
        let Some(client) = self.client() else {
            return;
        };
        spawn_local(async move {
            match client.webhook_secret(id.clone()).await {
                Ok(result) => {
                    self.webhook_error.set(None);
                    match result.secret {
                        Some(secret) => self.webhook_revealed.set(Some(WebhookRevealed {
                            webhook: id,
                            title,
                            secret: Some(secret),
                            new_url: false,
                        })),
                        None => self.status.set(format!("{title} has no secret.")),
                    }
                }
                Err(error) => self
                    .webhook_error
                    .set(Some(crate::errors::describe(&error))),
            }
        });
    }

    /// Deletes webhook `id`; its sessions stay.
    pub fn delete_webhook(self, id: WebhookId, title: String) {
        let Some(client) = self.client() else {
            return;
        };
        spawn_local(async move {
            match client.delete_webhook(id.clone()).await {
                Ok(_) => {
                    self.status.set(format!("Deleted {title}."));
                    if self.webhook_selected.get_untracked() == Some(id.clone()) {
                        self.webhook_selected.set(None);
                    }
                    if self.webhook_revealed.with_untracked(|shown| {
                        shown.as_ref().is_some_and(|shown| shown.webhook == id)
                    }) {
                        self.webhook_revealed.set(None);
                    }
                    self.webhook_error.set(None);
                    self.poll_webhooks();
                }
                Err(error) => self
                    .webhook_error
                    .set(Some(crate::errors::describe(&error))),
            }
        });
    }

    /// Opens event `event` of webhook `id`, reading its request; a second
    /// press on the same event closes it.
    pub fn toggle_webhook_event(self, id: WebhookId, event: String) {
        let open = self.webhook_event.with_untracked(|open| {
            open.as_ref()
                .is_some_and(|open| open.webhook == id && open.event == event)
        });
        if open {
            self.webhook_event.set(None);
            return;
        }
        let Some(client) = self.client() else {
            return;
        };
        self.webhook_event.set(Some(WebhookEventOpen {
            webhook: id.clone(),
            event: event.clone(),
            request: None,
        }));
        spawn_local(async move {
            let result = client
                .webhook_event(id.clone(), event.clone())
                .await
                .map_err(|error| crate::errors::describe(&error));
            self.webhook_event.update(|open| {
                if let Some(open) = open
                    && open.webhook == id
                    && open.event == event
                {
                    open.request = Some(result);
                }
            });
        });
    }
}
