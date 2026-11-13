//! The webhooks every client shows the same way
//! (`docs/site/specs/webhooks.md` section 14): whether the daemon keeps
//! webhooks, a webhook's URL and whether it is likely reachable only
//! privately, its title, target, check, state, counts, and what each event
//! did, the label of a thread prompt's message, and the webhook form as
//! `webhooks.create` and `webhooks.update` take it, with the `key: value`
//! text both clients edit.

use cookie_agent_protocol::{
    AgentId, EventOrigin, Open, PermissionMode, ProducerDeliveryMode, ProjectInfo, SemVer,
    SessionId, ThreadOwnerRef, Webhook, WebhookCondition, WebhookCounts, WebhookCreateParams,
    WebhookDisabledReason, WebhookEvent, WebhookEventOutcome, WebhookSignatureEncoding,
    WebhookTarget, WebhookTemplate, WebhookUpdateParams, WebhookValueSource, WebhookVerify,
    WebhookVerifyKind, thread_prompt_owner,
};
use jiff::{Timestamp, tz::TimeZone};

use crate::cron::{CronDetail, actor_label, delivery_label, local_minute, run_status_label};
use crate::session_actions::permission_mode_label;

/// The client protocol minor from which the daemon keeps webhooks
/// (`webhooks.*`, `SessionMeta::webhook`, thread prompts' origins).
pub const WEBHOOKS_MINOR: u32 = 96;

/// Whether a daemon speaking `version` keeps webhooks.
pub fn daemon_has_webhooks(version: Option<SemVer>) -> bool {
    version.is_some_and(|version| {
        version
            .shared_minor(cookie_agent_protocol::CLIENT_PROTOCOL_VERSION)
            .is_some_and(|minor| minor >= WEBHOOKS_MINOR)
    })
}

/// A webhook's URL: `path` (`/hooks/<token>`) below the daemon's public
/// base (`webhooks.list`'s `public_base`), else below `page_base`, the
/// address the client reached the daemon's page at.
pub fn webhook_url(public_base: Option<&str>, page_base: &str, path: &str) -> String {
    let base = public_base.unwrap_or(page_base).trim_end_matches('/');
    format!("{base}/{}", path.trim_start_matches('/'))
}

/// Whether the URL [`webhook_url`] builds probably works only where the
/// daemon's page can be reached, as the share dialog warns
/// ([`crate::shares::looks_private`]).
pub fn url_looks_private(public_base: Option<&str>, page_base: &str) -> bool {
    crate::shares::looks_private(public_base.unwrap_or(page_base), public_base.is_some())
}

/// What lists show for a webhook: its name, else `Webhook <id>`.
pub fn webhook_title(webhook: &Webhook) -> String {
    match webhook.name.as_deref().map(str::trim) {
        Some(name) if !name.is_empty() => name.to_owned(),
        _ => format!("Webhook {}", webhook.id),
    }
}

/// How lists name a webhook's target: the session's title,
/// `subagent <title>`, or `new session · <agent>`, then `· <project>`
/// when its sessions go in one and `· thread <source>` when it has a
/// thread key.
/// `title` names a session the client knows; `projects` is the client's
/// `projects.list`.
pub fn target_label(
    target: &WebhookTarget,
    projects: &[ProjectInfo],
    title: impl Fn(SessionId) -> Option<String>,
) -> String {
    let name = |session: SessionId| title(session).unwrap_or_else(|| short_session(session));
    match target {
        WebhookTarget::Session(id) => name(*id),
        WebhookTarget::Subagent(id) => format!("subagent {}", name(*id)),
        WebhookTarget::New(template) => {
            let mut label = format!("new session · {}", template.agent);
            if template.project.is_some() {
                label.push_str(&format!(" · {}", template_project(template, projects)));
            }
            if let Some(source) = &template.thread_key {
                label.push_str(&format!(" · thread {}", value_source_text(source)));
            }
            label
        }
    }
}

/// The project a new-session webhook's sessions go in, as
/// [`crate::cron::template_project`] names one.
pub fn template_project(template: &WebhookTemplate, projects: &[ProjectInfo]) -> String {
    match &template.project {
        Some(id) => crate::projects::find_project(projects, Some(id)).map_or_else(
            || id.to_string().chars().take(8).collect(),
            |project| crate::projects::project_label(Some(project)),
        ),
        None => crate::projects::NO_PROJECT.to_owned(),
    }
}

fn short_session(session: SessionId) -> String {
    session.to_string().chars().take(8).collect()
}

/// Whether the webhook is on, and why not: `enabled`, `failed 3 times in
/// a row`, `disabled by you`.
pub fn state_label(webhook: &Webhook) -> String {
    if webhook.enabled {
        return "enabled".to_owned();
    }
    match &webhook.disabled_reason {
        Some(Open::Known(reason)) => disabled_reason_label(reason),
        Some(Open::Unknown { .. }) | None => "disabled".to_owned(),
    }
}

/// Why a webhook is disabled, as [`state_label`] says it.
pub fn disabled_reason_label(reason: &WebhookDisabledReason) -> String {
    match reason {
        WebhookDisabledReason::Failed { failures } => {
            let times = if *failures == 1 { "time" } else { "times" };
            format!("failed {failures} {times} in a row")
        }
        WebhookDisabledReason::Disabled { by } => format!("disabled by {}", actor_label(by)),
    }
}

/// How a check is named: `URL only` without one, else its kind, such as
/// `GitHub signature`.
pub fn check_label(verify: Option<&WebhookVerify>) -> &'static str {
    match verify.map(|verify| verify.kind) {
        None => "URL only",
        Some(WebhookVerifyKind::Github) => "GitHub signature",
        Some(WebhookVerifyKind::Standard) => "Standard Webhooks signature",
        Some(WebhookVerifyKind::HmacSha256) => "HMAC-SHA256 signature",
        Some(WebhookVerifyKind::Bearer) => "bearer token",
    }
}

/// The counts a webhook keeps, nonzero ones only, in a fixed order:
/// `(what, count)` with `what` one of `accepted`, `rejected`, `filtered`,
/// `duplicate`, `rate limited`, `backlog full`.
pub fn counts(counts: &WebhookCounts) -> Vec<(&'static str, u64)> {
    [
        ("accepted", counts.accepted),
        ("rejected", counts.rejected),
        ("filtered", counts.filtered),
        ("duplicate", counts.duplicate),
        ("rate limited", counts.rate_limited),
        ("backlog full", counts.backlog_full),
    ]
    .into_iter()
    .filter(|(_, count)| *count > 0)
    .collect()
}

/// [`counts`] as one line, such as `12 accepted · 3 filtered`, or `no
/// requests yet`.
pub fn counts_summary(webhook_counts: &WebhookCounts) -> String {
    let parts: Vec<String> = counts(webhook_counts)
        .into_iter()
        .map(|(what, count)| format!("{count} {what}"))
        .collect();
    if parts.is_empty() {
        "no requests yet".to_owned()
    } else {
        parts.join(" · ")
    }
}

/// The kind of an event's outcome, for a list column or a colour.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OutcomeKind {
    /// Stored, not yet delivered.
    Pending,
    /// In the backlog, waiting for a session to finish.
    Waiting,
    /// Delivered; its run is still going, or its end is not known.
    Delivered,
    /// Delivered, and its run completed.
    Completed,
    /// Its delivery or its run failed.
    Failed,
    /// Its run was cancelled or ended otherwise.
    Ended,
    /// An outcome this client does not know.
    Unknown,
}

/// What an event's outcome comes to.
pub fn outcome_kind(event: &WebhookEvent) -> OutcomeKind {
    match &event.outcome {
        Open::Known(WebhookEventOutcome::Pending) => OutcomeKind::Pending,
        Open::Known(WebhookEventOutcome::Waiting) => OutcomeKind::Waiting,
        Open::Known(WebhookEventOutcome::Delivered) => match event.run_end.as_ref() {
            None => OutcomeKind::Delivered,
            Some(end) => match end.status {
                cookie_agent_protocol::SessionStatus::Completed => OutcomeKind::Completed,
                cookie_agent_protocol::SessionStatus::Failed => OutcomeKind::Failed,
                cookie_agent_protocol::SessionStatus::Running => OutcomeKind::Delivered,
                _ => OutcomeKind::Ended,
            },
        },
        Open::Known(WebhookEventOutcome::Failed { .. }) => OutcomeKind::Failed,
        Open::Unknown { .. } => OutcomeKind::Unknown,
    }
}

/// What an event did: `waiting`, `delivered · completed`, or
/// `failed: <error>`, prefixed `test · ` for a test event.
pub fn event_outcome(event: &WebhookEvent) -> String {
    let outcome = match &event.outcome {
        Open::Known(WebhookEventOutcome::Pending) => "pending".to_owned(),
        Open::Known(WebhookEventOutcome::Waiting) => "waiting".to_owned(),
        Open::Known(WebhookEventOutcome::Delivered) => match &event.run_end {
            Some(end) => format!("delivered · {}", run_status_label(end.status)),
            None if event.run_id.is_some() => "delivered · running".to_owned(),
            None => "delivered".to_owned(),
        },
        Open::Known(WebhookEventOutcome::Failed { error }) => format!("failed: {error}"),
        Open::Unknown { tag, .. } => tag.replace('_', " "),
    };
    if event.test {
        format!("test · {outcome}")
    } else {
        outcome
    }
}

/// The last event's outcome kind, `None` before the first event.
pub fn last_outcome(webhook: &Webhook) -> Option<OutcomeKind> {
    webhook.history.last().map(outcome_kind)
}

/// A value source as the form writes it: `json:<pointer>` or
/// `header:<name>`.
pub fn value_source_text(source: &WebhookValueSource) -> String {
    match (&source.header, &source.json) {
        (Some(header), _) => format!("header:{header}"),
        (None, Some(pointer)) => format!("json:{pointer}"),
        (None, None) => String::new(),
    }
}

/// Reads a value source as typed: `json:/issue/number` or
/// `header:X-GitHub-Event`.
pub fn parse_value_source(text: &str) -> Result<WebhookValueSource, String> {
    let text = text.trim();
    if let Some(name) = text.strip_prefix("header:") {
        let name = name.trim();
        if name.is_empty() || name.contains(char::is_whitespace) {
            return Err(
                "a header source names one header, such as `header:X-GitHub-Event`".to_owned(),
            );
        }
        return Ok(WebhookValueSource::header(name));
    }
    if let Some(pointer) = text.strip_prefix("json:") {
        let pointer = pointer.trim();
        if !(pointer.is_empty() || pointer.starts_with('/')) {
            return Err(format!(
                "a JSON source is a JSON Pointer starting with `/`, such as `json:/issue/number`, not `{pointer}`"
            ));
        }
        return Ok(WebhookValueSource::json(pointer));
    }
    Err(format!("`{text}` is not `json:/pointer` or `header:Name`"))
}

/// A filter condition as the form writes it: `<source>=<value>,<value>`.
pub fn condition_text(condition: &WebhookCondition) -> String {
    let source = WebhookValueSource {
        header: condition.header.clone(),
        json: condition.json.clone(),
    };
    format!(
        "{}={}",
        value_source_text(&source),
        condition.one_of.join(",")
    )
}

/// Reads a filter condition as typed: `header:X-GitHub-Event=issues,pull_request`
/// or `json:/action=opened`.
pub fn parse_condition(text: &str) -> Result<WebhookCondition, String> {
    let Some((source, values)) = text.split_once('=') else {
        return Err(format!(
            "`{}` is not `<source>=<value>,<value>`, such as `json:/action=opened`",
            text.trim()
        ));
    };
    let source = parse_value_source(source)?;
    let one_of: Vec<String> = values
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .collect();
    if one_of.is_empty() {
        return Err(format!("`{}` names no values to match", text.trim()));
    }
    Ok(WebhookCondition {
        header: source.header,
        json: source.json,
        one_of,
    })
}

/// A check as the form writes it: empty for none, `github`, `standard`,
/// `bearer`, or an HMAC check such as
/// `hmac_sha256 header=<name> encoding=hex prefix=<text>` (or
/// `encoding=base64`). Secrets are never part of it.
pub fn verify_text(verify: Option<&WebhookVerify>) -> String {
    let Some(verify) = verify else {
        return String::new();
    };
    let mut text = verify.kind.as_str().to_owned();
    if let Some(header) = &verify.header {
        text.push_str(&format!(" header={header}"));
    }
    if let Some(encoding) = verify.encoding {
        text.push_str(match encoding {
            WebhookSignatureEncoding::Hex => " encoding=hex",
            WebhookSignatureEncoding::Base64 => " encoding=base64",
        });
    }
    if let Some(prefix) = &verify.prefix {
        text.push_str(&format!(" prefix={prefix}"));
    }
    text
}

/// Reads a check as typed (see [`verify_text`]); `None` for an empty one.
pub fn parse_verify(text: &str) -> Result<Option<WebhookVerify>, String> {
    let mut words = text.split_whitespace();
    let Some(kind) = words.next() else {
        return Ok(None);
    };
    let kind = match kind {
        "github" => WebhookVerifyKind::Github,
        "standard" => WebhookVerifyKind::Standard,
        "hmac_sha256" => WebhookVerifyKind::HmacSha256,
        "bearer" => WebhookVerifyKind::Bearer,
        other => {
            return Err(format!(
                "a check is github, standard, hmac_sha256, or bearer, not `{other}`"
            ));
        }
    };
    let mut verify = WebhookVerify::of_kind(kind);
    for word in words {
        let Some((key, value)) = word.split_once('=') else {
            return Err(format!("`{word}` is not `key=value`"));
        };
        if kind != WebhookVerifyKind::HmacSha256 {
            return Err(format!("`{key}` is only for an hmac_sha256 check"));
        }
        match key {
            "header" if !value.is_empty() => verify.header = Some(value.to_owned()),
            "encoding" => {
                verify.encoding = Some(match value {
                    "hex" => WebhookSignatureEncoding::Hex,
                    "base64" => WebhookSignatureEncoding::Base64,
                    other => return Err(format!("encoding is hex or base64, not `{other}`")),
                });
            }
            "prefix" => verify.prefix = (!value.is_empty()).then(|| value.to_owned()),
            other => return Err(format!("`{other}` is not header, encoding, or prefix")),
        }
    }
    if kind == WebhookVerifyKind::HmacSha256 && verify.header.is_none() {
        return Err("an hmac_sha256 check needs header=<name>".to_owned());
    }
    Ok(Some(verify))
}

/// Reads a limit as typed: empty for the daemon's default, else a whole
/// number, where 0 or less means no limit.
pub fn parse_limit(text: &str, what: &str) -> Result<Option<i64>, String> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(None);
    }
    text.parse::<i64>()
        .map(Some)
        .map_err(|_| format!("{what} is a whole number; 0 or less means no limit"))
}

fn limit_text(limit: Option<i64>) -> String {
    limit.map(|limit| limit.to_string()).unwrap_or_default()
}

/// The label a thread prompt's message carries in place of the user's
/// name: `from webhook <name>` or `from cron job <name>`, with `name`
/// naming the owner the client knows (else its id). `None` for any other
/// origin.
pub fn thread_prompt_label(
    origin: &EventOrigin,
    name: impl Fn(&ThreadOwnerRef) -> Option<String>,
) -> Option<String> {
    let owner = thread_prompt_owner(origin)?;
    let (what, id) = match &owner {
        ThreadOwnerRef::Webhook(id) => ("webhook", id.to_string()),
        ThreadOwnerRef::Cron(id) => ("cron job", id.to_string()),
        #[allow(unreachable_patterns)]
        _ => return None,
    };
    Some(format!("from {what} {}", name(&owner).unwrap_or(id)))
}

/// The details of a webhook, in the order clients show them. `url` is its
/// address ([`webhook_url`]); `title` names a session the client knows and
/// `projects` is the client's `projects.list`.
pub fn detail_lines(
    webhook: &Webhook,
    url: &str,
    zone: &TimeZone,
    projects: &[ProjectInfo],
    title: impl Fn(SessionId) -> Option<String>,
) -> Vec<CronDetail> {
    let detail = |label: &'static str, value: String| CronDetail {
        label,
        value,
        session: None,
    };
    let mut lines = vec![
        detail("id", webhook.id.to_string()),
        detail("url", url.to_owned()),
        CronDetail {
            label: "target",
            value: target_label(&webhook.target, projects, &title),
            session: webhook.target.session(),
        },
        detail("state", state_label(webhook)),
        detail("check", check_label(webhook.verify.as_ref()).to_owned()),
    ];
    if let Some(delivery) = webhook.delivery {
        lines.push(detail("delivery", delivery_label(delivery).to_owned()));
    }
    if let WebhookTarget::New(template) = &webhook.target {
        lines.push(detail(
            "agent",
            match &template.preset {
                Some(preset) => format!("{} ({preset})", template.agent),
                None => template.agent.to_string(),
            },
        ));
        if let Some(model) = &template.model {
            lines.push(detail("model", model.to_string()));
        }
        lines.push(detail("project", template_project(template, projects)));
        if let Some(source) = &template.thread_key {
            lines.push(detail("thread key", value_source_text(source)));
            if !webhook.threads.is_empty() {
                lines.push(detail("threads", webhook.threads.len().to_string()));
            }
        }
        if template.thread_prompt.is_some() {
            lines.push(detail("thread prompt", "set".to_owned()));
        }
        if let Some(title) =
            crate::cron::session_title_reading(template.title.as_deref(), template.title_auto)
        {
            lines.push(detail("session title", title));
        }
    }
    for condition in &webhook.filter {
        lines.push(detail("filter", condition_text(condition)));
    }
    if !webhook.headers.is_empty() {
        lines.push(detail("headers", webhook.headers.join(", ")));
    }
    if let Some(limit) = webhook.max_per_hour {
        lines.push(detail("per hour", limit_reading(limit)));
    }
    if let Some(limit) = webhook.max_concurrent {
        lines.push(detail("at once", limit_reading(limit)));
    }
    if let Some(mode) = webhook.permission_mode {
        lines.push(detail("mode", permission_mode_label(mode).to_owned()));
    }
    if webhook.waiting > 0 {
        lines.push(detail("waiting", webhook.waiting.to_string()));
    }
    if webhook.consecutive_failures > 0 {
        lines.push(detail(
            "failures",
            format!("{} in a row", webhook.consecutive_failures),
        ));
    }
    if let Some(limit) = webhook.max_consecutive_failures {
        lines.push(detail("failure limit", limit_reading(limit)));
    }
    lines.push(detail("requests", counts_summary(&webhook.counts)));
    lines.push(detail(
        "created",
        format!(
            "{} by {}",
            local_minute(webhook.created_at, zone),
            actor_label(&webhook.created_by)
        ),
    ));
    if let cookie_agent_protocol::CronActor::Agent(agent) = &webhook.created_by {
        lines.push(CronDetail {
            label: "created in",
            value: title(agent.session).unwrap_or_else(|| short_session(agent.session)),
            session: Some(agent.session),
        });
    }
    lines
}

fn limit_reading(limit: i64) -> String {
    if limit <= 0 {
        "no limit".to_owned()
    } else {
        limit.to_string()
    }
}

/// The webhook with `id` in `webhooks`.
pub fn webhook_by_id<'a>(webhooks: &'a [Webhook], id: &str) -> Option<&'a Webhook> {
    webhooks.iter().find(|webhook| webhook.id.as_str() == id)
}

/// The events of `webhook`, newest first, as clients list them.
pub fn events_newest_first(webhook: &Webhook) -> impl Iterator<Item = &WebhookEvent> {
    webhook.history.iter().rev()
}

/// When an event arrived, to the minute, in `zone`.
pub fn event_time(event: &WebhookEvent, zone: &TimeZone) -> String {
    local_minute(event.received_at, zone)
}

/// Where a form's webhook delivers.
#[derive(Clone, Debug, Eq, PartialEq)]
// One per form; boxing the template would only move it.
#[allow(clippy::large_enum_variant)]
pub enum WebhookDraftTarget {
    /// An existing session: a root one, or a delegated one when
    /// `subagent`.
    Session { session: SessionId, subagent: bool },
    /// A new session per event (or per thread), from `template`; the form
    /// edits its agent, model pin, thread key, and thread prompt and keeps
    /// the rest.
    New { template: WebhookTemplate },
}

impl WebhookDraftTarget {
    /// The target `webhooks.create` takes, as the template stands.
    pub fn target(&self) -> WebhookTarget {
        match self {
            Self::Session {
                session,
                subagent: false,
            } => WebhookTarget::Session(*session),
            Self::Session {
                session,
                subagent: true,
            } => WebhookTarget::Subagent(*session),
            Self::New { template } => WebhookTarget::New(template.clone()),
        }
    }

    pub const fn is_new(&self) -> bool {
        matches!(self, Self::New { .. })
    }
}

/// The webhook form: what a user types to create or edit a webhook. Every
/// text field reads as the `parse_*` function of its kind says; a blank one
/// means none, or the daemon's default.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WebhookDraft {
    pub name: String,
    /// Sent with every event; may be blank.
    pub prompt: String,
    pub target: WebhookDraftTarget,
    /// For a session target, or a new-session one with a thread key.
    pub delivery: ProducerDeliveryMode,
    /// For a new-session target; the daemon's default when `None`.
    pub permission_mode: Option<PermissionMode>,
    /// For a new-session target, as [`crate::cron::parse_model_pin`] reads it.
    pub model: String,
    /// For a new-session target: the template's agent.
    pub agent: String,
    /// For a new-session target, as [`parse_value_source`] reads it.
    pub thread_key: String,
    /// For a new-session target: sent once to each session it creates, and
    /// again after each compaction.
    pub thread_prompt: String,
    /// For a new-session target: what its sessions are called, as
    /// [`crate::cron::parse_session_title`] reads it; the webhook's name
    /// when blank.
    pub title: String,
    /// For a new-session target: the automatic title names its sessions.
    pub title_auto: bool,
    /// As [`parse_verify`] reads it.
    pub verify: String,
    /// A secret to set with the check; blank to have the daemon make one
    /// (when the check is new or changes) or keep the current one.
    pub secret: String,
    /// One [`parse_condition`] per line.
    pub filter: String,
    /// Extra header names the model sees, separated by commas or spaces.
    pub headers: String,
    /// As [`parse_limit`] reads it.
    pub max_per_hour: String,
    /// As [`parse_limit`] reads it; new-session targets only.
    pub max_concurrent: String,
    /// As [`parse_limit`] reads it.
    pub max_consecutive_failures: String,
}

impl WebhookDraft {
    fn blank(target: WebhookDraftTarget) -> Self {
        Self {
            name: String::new(),
            prompt: String::new(),
            target,
            delivery: ProducerDeliveryMode::Queue,
            permission_mode: None,
            model: String::new(),
            agent: String::new(),
            thread_key: String::new(),
            thread_prompt: String::new(),
            title: String::new(),
            title_auto: false,
            verify: String::new(),
            secret: String::new(),
            filter: String::new(),
            headers: String::new(),
            max_per_hour: String::new(),
            max_concurrent: String::new(),
            max_consecutive_failures: String::new(),
        }
    }

    /// A form for a webhook that delivers into `session`, a delegated one
    /// when `subagent`.
    pub fn for_session(session: SessionId, subagent: bool) -> Self {
        Self::blank(WebhookDraftTarget::Session { session, subagent })
    }

    /// A form for a webhook that starts sessions from `template`.
    pub fn for_new_session(template: WebhookTemplate) -> Self {
        let mut draft = Self::blank(WebhookDraftTarget::New {
            template: template.clone(),
        });
        draft.fill_template(&template);
        draft
    }

    fn fill_template(&mut self, template: &WebhookTemplate) {
        self.agent = template.agent.to_string();
        self.model = template
            .model
            .as_ref()
            .map(crate::cron::model_pin_text)
            .unwrap_or_default();
        self.thread_key = template
            .thread_key
            .as_ref()
            .map(value_source_text)
            .unwrap_or_default();
        self.thread_prompt = template.thread_prompt.clone().unwrap_or_default();
        self.title = template.title.clone().unwrap_or_default();
        self.title_auto = template.title_auto;
    }

    /// The form filled from `webhook`, to edit it.
    pub fn of_webhook(webhook: &Webhook) -> Self {
        let target = match &webhook.target {
            WebhookTarget::Session(session) => WebhookDraftTarget::Session {
                session: *session,
                subagent: false,
            },
            WebhookTarget::Subagent(session) => WebhookDraftTarget::Session {
                session: *session,
                subagent: true,
            },
            WebhookTarget::New(template) => WebhookDraftTarget::New {
                template: template.clone(),
            },
        };
        let mut draft = Self::blank(target);
        if let WebhookTarget::New(template) = &webhook.target {
            draft.fill_template(template);
        }
        draft.name = webhook.name.clone().unwrap_or_default();
        draft.prompt = webhook.prompt.clone().unwrap_or_default();
        draft.delivery = webhook.delivery.unwrap_or(ProducerDeliveryMode::Queue);
        draft.permission_mode = webhook.permission_mode;
        draft.verify = verify_text(webhook.verify.as_ref());
        draft.filter = webhook
            .filter
            .iter()
            .map(condition_text)
            .collect::<Vec<_>>()
            .join("\n");
        draft.headers = webhook.headers.join(", ");
        draft.max_per_hour = limit_text(webhook.max_per_hour);
        draft.max_concurrent = limit_text(webhook.max_concurrent);
        draft.max_consecutive_failures = limit_text(webhook.max_consecutive_failures);
        draft
    }

    /// Whether the form's webhook starts sessions by a thread key, whose
    /// later events take its delivery.
    pub fn threaded(&self) -> bool {
        self.target.is_new() && !self.thread_key.trim().is_empty()
    }

    /// Whether the form's delivery applies: in a session, or a thread.
    pub fn takes_delivery(&self) -> bool {
        !self.target.is_new() || self.threaded()
    }

    /// The target with the form's agent, model pin, thread key, and thread
    /// prompt applied.
    fn resolved_target(&self) -> Result<WebhookTarget, String> {
        let WebhookDraftTarget::New { template } = &self.target else {
            if !self.thread_key.trim().is_empty() || !self.thread_prompt.trim().is_empty() {
                return Err(
                    "a thread key and a thread prompt are for a webhook that starts new sessions"
                        .to_owned(),
                );
            }
            if !self.title.trim().is_empty() || self.title_auto {
                return Err("a session title is for a webhook that starts new sessions".to_owned());
            }
            return Ok(self.target.target());
        };
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
        template.model = crate::cron::parse_model_pin(&self.model)?;
        let thread_key = self.thread_key.trim();
        template.thread_key = if thread_key.is_empty() {
            None
        } else {
            Some(parse_value_source(thread_key)?)
        };
        template.thread_prompt = (!self.thread_prompt.trim().is_empty())
            .then(|| self.thread_prompt.trim_end().to_owned());
        template.title = crate::cron::parse_session_title(&self.title)?;
        template.title_auto = self.title_auto;
        Ok(WebhookTarget::New(template))
    }

    fn checked_name(&self) -> Option<String> {
        let name = self.name.trim();
        (!name.is_empty()).then(|| name.to_owned())
    }

    fn checked_prompt(&self) -> Option<String> {
        (!self.prompt.trim().is_empty()).then(|| self.prompt.trim_end().to_owned())
    }

    fn checked_verify(&self) -> Result<Option<WebhookVerify>, String> {
        let mut verify = parse_verify(&self.verify)?;
        let secret = self.secret.trim();
        if !secret.is_empty() {
            let Some(verify) = verify.as_mut() else {
                return Err("a secret needs a check".to_owned());
            };
            verify.secret = Some(secret.to_owned());
        }
        Ok(verify)
    }

    fn checked_filter(&self) -> Result<Vec<WebhookCondition>, String> {
        self.filter
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(parse_condition)
            .collect()
    }

    fn checked_headers(&self) -> Vec<String> {
        self.headers
            .split([',', ' ', '\n', '\t'])
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_owned)
            .collect()
    }

    fn checked_max_concurrent(&self) -> Result<Option<i64>, String> {
        let limit = parse_limit(&self.max_concurrent, "max_concurrent")?;
        if limit.is_some() && !self.target.is_new() {
            return Err("max_concurrent is for a webhook that starts new sessions".to_owned());
        }
        Ok(limit)
    }

    /// `webhooks.create`'s params, or why the form cannot be sent.
    pub fn create_params(&self) -> Result<WebhookCreateParams, String> {
        let new = self.target.is_new();
        let mut params = WebhookCreateParams::new(self.resolved_target()?);
        params.name = self.checked_name();
        params.prompt = self.checked_prompt();
        params.delivery = self.takes_delivery().then_some(self.delivery);
        params.verify = self.checked_verify()?;
        params.filter = self.checked_filter()?;
        params.headers = self.checked_headers();
        params.max_per_hour = parse_limit(&self.max_per_hour, "max_per_hour")?;
        params.max_concurrent = self.checked_max_concurrent()?;
        params.permission_mode = if new { self.permission_mode } else { None };
        params.max_consecutive_failures =
            parse_limit(&self.max_consecutive_failures, "max_consecutive_failures")?;
        Ok(params)
    }

    /// `webhooks.update`'s params for what the form changed in `webhook`,
    /// or why the form cannot be sent. A secret typed into the form is
    /// sent with the check, which then counts as changed.
    pub fn update_params(&self, webhook: &Webhook) -> Result<WebhookUpdateParams, String> {
        let before = Self::of_webhook(webhook);
        let mut params = WebhookUpdateParams::new(webhook.id.clone());
        if self.name.trim() != before.name.trim() {
            params.name = Some(self.checked_name());
        }
        let prompt = self.checked_prompt();
        if prompt != webhook.prompt {
            params.prompt = Some(prompt);
        }
        let target = self.resolved_target()?;
        if target != webhook.target {
            params.target = Some(target);
        }
        if self.takes_delivery() && Some(self.delivery) != webhook.delivery {
            params.delivery = Some(self.delivery);
        }
        let verify = self.checked_verify()?;
        if verify_text(verify.as_ref()) != verify_text(webhook.verify.as_ref())
            || verify
                .as_ref()
                .is_some_and(|verify| verify.secret.is_some())
        {
            params.verify = Some(verify);
        }
        let filter = self.checked_filter()?;
        if filter != webhook.filter {
            params.filter = Some(filter);
        }
        let headers = self.checked_headers();
        if headers != webhook.headers {
            params.headers = Some(headers);
        }
        let max_per_hour = parse_limit(&self.max_per_hour, "max_per_hour")?;
        if max_per_hour != webhook.max_per_hour {
            params.max_per_hour = Some(max_per_hour);
        }
        let max_concurrent = self.checked_max_concurrent()?;
        if max_concurrent != webhook.max_concurrent {
            params.max_concurrent = Some(max_concurrent);
        }
        let failures = parse_limit(&self.max_consecutive_failures, "max_consecutive_failures")?;
        if failures != webhook.max_consecutive_failures {
            params.max_consecutive_failures = Some(failures);
        }
        if self.target.is_new() && self.permission_mode != webhook.permission_mode {
            params.permission_mode = Some(self.permission_mode);
        }
        Ok(params)
    }

    /// The form as the `key: value` text a user edits: comment lines, the
    /// fields, then the prompt after a `---` line and the thread prompt
    /// after a `--- thread prompt` line. The target is fixed when the form
    /// opens; `target` names it in a comment.
    pub fn form_text(&self, target: &str) -> String {
        let mut text = format!(
            "# A webhook. Lines starting with # are ignored; the prompt sent with every\n\
             # event follows the --- line, and a thread prompt, sent once to each session\n\
             # the webhook creates and again after each compaction, follows a\n\
             # `{THREAD_PROMPT_LINE}` line.\n\
             # Target: {target}.\n\
             # delivery: queue (after the current turn) or steer (into it); for a session\n\
             #   target, or a new-session one with a thread_key.\n\
             # agent, model, permission_mode, thread_key, max_concurrent: new-session targets.\n\
             #   model is provider/model or provider/model[variant], empty for the agent's own;\n\
             #   thread_key is json:/pointer or header:Name; events with the same key share a\n\
             #   session.\n\
             # title: new-session targets: what the sessions are called, followed by\n\
             #   \" · \" and the thread key or the time; empty for the webhook's name.\n\
             #   title_auto: yes lets the model name each session after its first turn.\n\
             # check: empty (the URL alone), github, standard, bearer, or\n\
             #   hmac_sha256 header=<name> encoding=hex|base64 prefix=<text>.\n\
             # secret: empty to keep the current one, or have one made for a new check.\n\
             # filter: one line each, header:Name=a,b or json:/pointer=a,b; all must match.\n\
             # headers: extra request headers the model sees, separated by commas.\n\
             # max_per_hour, max_concurrent, max_consecutive_failures: empty for the\n\
             #   daemon's default, 0 or less for no limit.\n\
             name: {}\n\
             delivery: {}\n",
            self.name,
            delivery_label(self.delivery),
        );
        if self.target.is_new() {
            text.push_str(&format!(
                "agent: {}\nmodel: {}\npermission_mode: {}\nthread_key: {}\ntitle: {}\ntitle_auto: {}\n",
                self.agent,
                self.model,
                self.permission_mode.map_or("", permission_mode_code),
                self.thread_key,
                self.title,
                if self.title_auto { "yes" } else { "no" },
            ));
        }
        text.push_str(&format!(
            "check: {}\nsecret: {}\n",
            self.verify, self.secret
        ));
        let filters: Vec<&str> = self
            .filter
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .collect();
        if filters.is_empty() {
            text.push_str("filter: \n");
        }
        for line in filters {
            text.push_str(&format!("filter: {line}\n"));
        }
        text.push_str(&format!(
            "headers: {}\nmax_per_hour: {}\n",
            self.headers, self.max_per_hour
        ));
        if self.target.is_new() {
            text.push_str(&format!("max_concurrent: {}\n", self.max_concurrent));
        }
        text.push_str(&format!(
            "max_consecutive_failures: {}\n---\n{}\n",
            self.max_consecutive_failures, self.prompt
        ));
        if self.target.is_new() && !self.thread_prompt.is_empty() {
            text.push_str(&format!("{THREAD_PROMPT_LINE}\n{}\n", self.thread_prompt));
        }
        text
    }

    /// Reads [`Self::form_text`]'s text back into this form, keeping its
    /// target, or says what is wrong.
    pub fn apply_form(&mut self, text: &str) -> Result<(), String> {
        let (header, prompts) = match text.split_once("\n---\n") {
            Some((header, prompts)) => (header, prompts),
            None => match text.strip_suffix("\n---") {
                Some(header) => (header, ""),
                None => return Err("the form needs the --- line before the prompt".to_owned()),
            },
        };
        let (prompt, thread_prompt) = match prompts.split_once(&format!("{THREAD_PROMPT_LINE}\n")) {
            Some((prompt, thread_prompt)) => (prompt, thread_prompt),
            None => match prompts.strip_suffix(THREAD_PROMPT_LINE) {
                Some(prompt) => (prompt, ""),
                None => (prompts, ""),
            },
        };
        let mut next = self.clone();
        next.prompt = prompt.trim_end().to_owned();
        next.thread_prompt = thread_prompt.trim_end().to_owned();
        let mut seen: Vec<&str> = Vec::new();
        let mut filters: Vec<&str> = Vec::new();
        for line in header.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once(':') else {
                return Err(format!("`{line}` is not `key: value`"));
            };
            let (key, value) = (key.trim(), value.trim());
            if key == "filter" {
                if !value.is_empty() {
                    filters.push(value);
                }
                continue;
            }
            if seen.contains(&key) {
                return Err(format!("`{key}` is given twice"));
            }
            seen.push(key);
            let new_only = matches!(
                key,
                "agent"
                    | "model"
                    | "permission_mode"
                    | "thread_key"
                    | "title"
                    | "title_auto"
                    | "max_concurrent"
            );
            if new_only
                && !self.target.is_new()
                && !value.is_empty()
                && !(key == "title_auto" && matches!(value, "no" | "false"))
            {
                return Err(format!("`{key}` is for a webhook that starts new sessions"));
            }
            match key {
                "name" => next.name = value.to_owned(),
                "delivery" => {
                    next.delivery = match value {
                        "" | "queue" => ProducerDeliveryMode::Queue,
                        "steer" => ProducerDeliveryMode::Steer,
                        other => return Err(format!("delivery is queue or steer, not `{other}`")),
                    }
                }
                "agent" => next.agent = value.to_owned(),
                "model" => next.model = value.to_owned(),
                "permission_mode" => {
                    next.permission_mode = if value.is_empty() {
                        None
                    } else {
                        Some(
                            parse_permission_mode(value)
                                .ok_or_else(|| format!("`{value}` is not a permission mode"))?,
                        )
                    }
                }
                "thread_key" => next.thread_key = value.to_owned(),
                "title" => next.title = value.to_owned(),
                "title_auto" => next.title_auto = crate::cron::parse_title_auto(value)?,
                "check" => next.verify = value.to_owned(),
                "secret" => next.secret = value.to_owned(),
                "headers" => next.headers = value.to_owned(),
                "max_per_hour" => next.max_per_hour = value.to_owned(),
                "max_concurrent" => next.max_concurrent = value.to_owned(),
                "max_consecutive_failures" => next.max_consecutive_failures = value.to_owned(),
                other => return Err(format!("`{other}` is not a field of a webhook")),
            }
        }
        next.filter = filters.join("\n");
        if !self.target.is_new() && !next.thread_prompt.is_empty() {
            return Err("a thread prompt is for a webhook that starts new sessions".to_owned());
        }
        // Check every field now, so the form is refused while still open.
        next.resolved_target()?;
        next.checked_verify()?;
        next.checked_filter()?;
        parse_limit(&next.max_per_hour, "max_per_hour")?;
        next.checked_max_concurrent()?;
        parse_limit(&next.max_consecutive_failures, "max_consecutive_failures")?;
        *self = next;
        Ok(())
    }
}

/// The line a form's thread prompt follows, after its prompt.
pub const THREAD_PROMPT_LINE: &str = "--- thread prompt";

/// The permission modes a form names, by the code it writes.
pub const PERMISSION_MODE_CODES: [(&str, PermissionMode); 5] = [
    ("auto_approve_n", PermissionMode::AutoApproveN),
    ("auto_approve", PermissionMode::AutoApprove),
    ("auto_approve_y", PermissionMode::AutoApproveY),
    ("ask", PermissionMode::Ask),
    ("yolo", PermissionMode::Yolo),
];

fn permission_mode_code(mode: PermissionMode) -> &'static str {
    PERMISSION_MODE_CODES
        .iter()
        .find(|(_, candidate)| *candidate == mode)
        .map_or("", |(code, _)| code)
}

fn parse_permission_mode(code: &str) -> Option<PermissionMode> {
    PERMISSION_MODE_CODES
        .iter()
        .find(|(candidate, _)| *candidate == code)
        .map(|(_, mode)| *mode)
}

/// Whether `update` changes anything.
pub fn update_changes_anything(update: &WebhookUpdateParams) -> bool {
    *update != WebhookUpdateParams::new(update.id.clone())
}

/// When `at` is, to the minute, in `zone`; the same reading cron uses.
pub fn minute(at: Timestamp, zone: &TimeZone) -> String {
    local_minute(at, zone)
}

#[cfg(test)]
#[path = "webhook_tests.rs"]
mod tests;
