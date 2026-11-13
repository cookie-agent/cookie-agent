//! What the web client's Webhooks view reads from a webhook
//! (`docs/site/specs/webhooks-view.md`): a row's target in words, its tags,
//! and its health line (H1); an open webhook's counters and what happens to
//! its events (H2, H3); the form's presets, its "one session per" choice,
//! and its filter as conditions (H4).

use cookie_agent_protocol::{
    Open, ProducerDeliveryMode, ProjectInfo, SessionId, Webhook, WebhookDisabledReason,
    WebhookTarget, WebhookValueSource, WebhookVerify, WebhookVerifyKind,
};
use jiff::Timestamp;

use crate::webhook::{OutcomeKind, event_outcome, outcome_kind, parse_condition};

/// Where a webhook's events go, in words: the session it delivers into,
/// linked, then the rest, such as `queued after the current turn`, or for
/// new sessions `new sessions · primary · one per issue.number`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TargetWords {
    /// The session events go into, and its name.
    pub session: Option<(SessionId, String)>,
    pub rest: String,
}

/// [`TargetWords`] for `webhook`. `title` names a session the client knows;
/// `projects` is the client's `projects.list`.
pub fn target_words(
    webhook: &Webhook,
    projects: &[ProjectInfo],
    title: impl Fn(SessionId) -> Option<String>,
) -> TargetWords {
    let name = |session: SessionId| {
        title(session).unwrap_or_else(|| session.to_string().chars().take(8).collect())
    };
    let delivery = match webhook.delivery.unwrap_or(ProducerDeliveryMode::Queue) {
        ProducerDeliveryMode::Steer => "into the running turn",
        _ => "queued after the current turn",
    };
    match &webhook.target {
        WebhookTarget::Session(session) => TargetWords {
            session: Some((*session, name(*session))),
            rest: delivery.to_owned(),
        },
        WebhookTarget::Subagent(session) => TargetWords {
            session: Some((*session, name(*session))),
            rest: format!("a subagent · {delivery}"),
        },
        WebhookTarget::New(template) => {
            let mut rest = format!("new sessions · {}", template.agent);
            if template.project.is_some() {
                rest.push_str(&format!(
                    " · {}",
                    crate::webhook::template_project(template, projects)
                ));
            }
            rest.push_str(&match &template.thread_key {
                Some(source) => format!(" · one per {}", source_words(source)),
                None => " · one per event".to_owned(),
            });
            TargetWords {
                session: None,
                rest,
            }
        }
    }
}

/// A thread key's source as a person reads it: a JSON Pointer as a dotted
/// path (`/issue/number` is `issue.number`), a header as `X-Thread header`.
pub fn source_words(source: &WebhookValueSource) -> String {
    match (&source.header, &source.json) {
        (Some(header), _) => format!("{header} header"),
        (None, Some(pointer)) => {
            let path = pointer
                .trim_start_matches('/')
                .split('/')
                .map(|part| part.replace("~1", "/").replace("~0", "~"))
                .collect::<Vec<_>>()
                .join(".");
            if path.is_empty() {
                "body".to_owned()
            } else {
                path
            }
        }
        (None, None) => "event".to_owned(),
    }
}

/// The tags a webhook's row carries, for what its switch cannot say: off
/// after failures, and no check (W4).
pub fn row_tags(webhook: &Webhook) -> Vec<String> {
    let mut tags = Vec::new();
    if !webhook.enabled
        && let Some(Open::Known(WebhookDisabledReason::Failed { failures })) =
            &webhook.disabled_reason
    {
        let noun = if *failures == 1 {
            "failure"
        } else {
            "failures"
        };
        tags.push(format!("off after {failures} {noun}"));
    }
    if webhook.verify.is_none() {
        tags.push("no check".to_owned());
    }
    tags
}

/// How a webhook is doing, for its row's dot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Tone {
    /// Its last event was delivered, or waits to be.
    Good,
    /// A request was refused since its last event.
    Warn,
    /// Its last event failed, or failures turned it off.
    Bad,
    /// No events yet.
    Quiet,
}

/// A request turned away: the newest of a kind that means something may be
/// wrong (rejected, rate-limited, or the backlog full).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Refusal {
    pub at: Timestamp,
    /// How many of its kind.
    pub count: u64,
    /// `rejected`, `rate-limited`, or `turned away`.
    pub what: &'static str,
    /// What it means for this webhook, as [`rejection_reason`] says it.
    pub reason: String,
}

/// A row's health line (H1): its tone, its last event (tests included,
/// W2), and the last refusal (W3).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Health {
    pub tone: Tone,
    /// When the newest event arrived, and what it did.
    pub last_event: Option<(Timestamp, String)>,
    pub refusal: Option<Refusal>,
}

/// [`Health`] for `webhook`.
pub fn health(webhook: &Webhook) -> Health {
    let newest = webhook.history.iter().max_by_key(|event| event.received_at);
    let last_event = newest.map(|event| (event.received_at, event_outcome(event)));
    let counts = &webhook.counts;
    let refusal = [
        (counts.last_rejected, counts.rejected, "rejected"),
        (
            counts.last_rate_limited,
            counts.rate_limited,
            "rate-limited",
        ),
        (counts.last_backlog_full, counts.backlog_full, "turned away"),
    ]
    .into_iter()
    .filter_map(|(at, count, what)| Some((at?, count, what)))
    .max_by_key(|(at, _, _)| *at)
    .map(|(at, count, what)| Refusal {
        at,
        count,
        what,
        reason: match what {
            "rejected" => rejection_reason(webhook.verify.as_ref()).to_owned(),
            "rate-limited" => rate_reason(webhook.max_per_hour),
            _ => "too many events were waiting".to_owned(),
        },
    });
    let failed_off = !webhook.enabled
        && matches!(
            webhook.disabled_reason,
            Some(Open::Known(WebhookDisabledReason::Failed { .. }))
        );
    let tone =
        if failed_off || newest.is_some_and(|event| outcome_kind(event) == OutcomeKind::Failed) {
            Tone::Bad
        } else if refusal
            .as_ref()
            .is_some_and(|refusal| last_event.as_ref().is_none_or(|(at, _)| refusal.at > *at))
        {
            Tone::Warn
        } else if last_event.is_some() {
            Tone::Good
        } else {
            Tone::Quiet
        };
    Health {
        tone,
        last_event,
        refusal,
    }
}

/// What a rejected request (`401`) means for a webhook's check.
pub fn rejection_reason(verify: Option<&WebhookVerify>) -> &'static str {
    match verify.map(|verify| verify.kind) {
        Some(WebhookVerifyKind::Github | WebhookVerifyKind::HmacSha256) => {
            "the signature didn't match the secret"
        }
        Some(WebhookVerifyKind::Standard) => "the signature didn't match, or it was too old",
        Some(WebhookVerifyKind::Bearer) => "the bearer token was missing or wrong",
        None => "it was refused",
    }
}

fn rate_reason(max_per_hour: Option<i64>) -> String {
    match max_per_hour {
        Some(limit) if limit > 0 => format!("over {limit} events an hour"),
        _ => "over the hourly limit".to_owned(),
    }
}

/// One of an open webhook's counters (H3).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Counter {
    /// `Delivered`, `Rejected`, `Filtered`, `Rate-limited`, `Duplicates`,
    /// `Backlog full`.
    pub what: &'static str,
    /// The status the sender was answered.
    pub status: u16,
    pub count: u64,
    pub last: Option<Timestamp>,
    pub meaning: String,
    /// Whether a nonzero count means something may be wrong.
    pub warn: bool,
}

/// The counters an open webhook shows: delivered, rejected, filtered, and
/// rate-limited always; duplicates and a full backlog once they happen.
pub fn counters(webhook: &Webhook) -> Vec<Counter> {
    let counts = &webhook.counts;
    let counter = |what, status, count: u64, last, meaning: String, warn: bool| Counter {
        what,
        status,
        count,
        last,
        meaning,
        warn: warn && count > 0,
    };
    let mut counters = vec![
        counter(
            "Delivered",
            202,
            counts.accepted,
            counts.last_accepted,
            "accepted and passed on; test events are not counted".to_owned(),
            false,
        ),
        counter(
            "Rejected",
            401,
            counts.rejected,
            counts.last_rejected,
            rejection_reason(webhook.verify.as_ref()).to_owned(),
            true,
        ),
        counter(
            "Filtered",
            204,
            counts.filtered,
            counts.last_filtered,
            "didn't match the filter".to_owned(),
            false,
        ),
        counter(
            "Rate-limited",
            429,
            counts.rate_limited,
            counts.last_rate_limited,
            rate_reason(webhook.max_per_hour),
            true,
        ),
    ];
    if counts.duplicate > 0 {
        counters.push(counter(
            "Duplicates",
            200,
            counts.duplicate,
            counts.last_duplicate,
            "a delivery ID seen before".to_owned(),
            false,
        ));
    }
    if counts.backlog_full > 0 {
        counters.push(counter(
            "Backlog full",
            503,
            counts.backlog_full,
            counts.last_backlog_full,
            "too many events were waiting".to_owned(),
            true,
        ));
    }
    counters
}

/// One line of an open webhook's "What happens" (H2).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Happening {
    pub heading: String,
    pub detail: Option<String>,
}

/// What happens to an event, in a few lines: where it goes, what a new
/// session is told first, what comes with every event, and what the
/// sessions are called. `title` names a session the client knows.
pub fn what_happens(
    webhook: &Webhook,
    title: impl Fn(SessionId) -> Option<String>,
) -> Vec<Happening> {
    let line = |heading: String, detail: Option<String>| Happening { heading, detail };
    let mut lines = Vec::new();
    let at_once = match webhook.max_concurrent {
        Some(limit) if limit > 0 => format!("up to {limit} at once"),
        Some(_) => "any number at once".to_owned(),
        None => "as many at once as the daemon allows".to_owned(),
    };
    match &webhook.target {
        WebhookTarget::Session(session) | WebhookTarget::Subagent(session) => {
            let name = title(*session).unwrap_or_else(|| "its session".to_owned());
            let when = match webhook.delivery.unwrap_or(ProducerDeliveryMode::Queue) {
                ProducerDeliveryMode::Steer => "into the turn running then",
                _ => "after the turn running then",
            };
            lines.push(line(
                format!("Each event goes into {name}"),
                Some(when.to_owned()),
            ));
        }
        WebhookTarget::New(template) => {
            match &template.thread_key {
                Some(source) => {
                    let key = source_words(source);
                    lines.push(line(
                        format!("Each {key} gets its own session, with {}", template.agent),
                        Some(format!(
                            "Later events with the same {key} go into it · {at_once}"
                        )),
                    ));
                }
                None => lines.push(line(
                    format!("Each event starts a new session, with {}", template.agent),
                    Some(at_once),
                )),
            }
            if let Some(prompt) = &template.thread_prompt {
                lines.push(line("Its first message".to_owned(), Some(prompt.clone())));
            }
            lines.push(line(
                "Sessions are called".to_owned(),
                Some(session_title_example(webhook)),
            ));
        }
    }
    lines.push(line(
        "With every event".to_owned(),
        Some(
            webhook
                .prompt
                .clone()
                .unwrap_or_else(|| "The request alone, marked as a webhook event".to_owned()),
        ),
    ));
    lines
}

/// How a new-session webhook's sessions are called, as an example: the
/// title or the webhook's name, then the thread key or the time.
pub fn session_title_example(webhook: &Webhook) -> String {
    let WebhookTarget::New(template) = &webhook.target else {
        return String::new();
    };
    if template.title_auto {
        return "named by the model after the first turn".to_owned();
    }
    let name = template
        .title
        .clone()
        .unwrap_or_else(|| crate::webhook::webhook_title(webhook));
    let suffix = match &template.thread_key {
        Some(source) => format!("the {}", source_words(source)),
        None => "the time".to_owned(),
    };
    format!("{name} · {suffix}")
}

/// Where the form starts from (H4, D2): the fields a sender needs, which
/// stay editable.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Preset {
    Github,
    /// Senders that sign as Standard Webhooks do (Svix and many others).
    Standard,
    Bearer,
    Anything,
}

/// The fields a [`Preset`] fills, as the form writes them.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PresetFields {
    pub verify: String,
    pub filter: String,
    /// For a new-session webhook.
    pub thread_key: String,
    pub headers: String,
    /// A title the user edits, for a new-session webhook.
    pub title: String,
}

impl Preset {
    pub const ALL: [Self; 4] = [Self::Github, Self::Standard, Self::Bearer, Self::Anything];

    pub fn id(self) -> &'static str {
        match self {
            Self::Github => "github",
            Self::Standard => "standard",
            Self::Bearer => "bearer",
            Self::Anything => "anything",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Github => "GitHub",
            Self::Standard => "Standard Webhooks",
            Self::Bearer => "Bearer token",
            Self::Anything => "Anything",
        }
    }

    /// One line under the label.
    pub fn blurb(self) -> &'static str {
        match self {
            Self::Github => "signature, issue events, one session per issue",
            Self::Standard => "Svix and other senders that sign this way",
            Self::Bearer => "CI, scripts, anything that sets a header",
            Self::Anything => "the URL is the key; add a check later",
        }
    }

    pub fn fields(self) -> PresetFields {
        match self {
            Self::Github => PresetFields {
                verify: "github".to_owned(),
                filter: "header:X-GitHub-Event=issues,issue_comment".to_owned(),
                thread_key: "json:/issue/number".to_owned(),
                headers: "X-GitHub-Event".to_owned(),
                title: "Issues".to_owned(),
            },
            Self::Standard => PresetFields {
                verify: "standard".to_owned(),
                ..PresetFields::default()
            },
            Self::Bearer => PresetFields {
                verify: "bearer".to_owned(),
                ..PresetFields::default()
            },
            Self::Anything => PresetFields::default(),
        }
    }

    /// Whether the preset is meant for a webhook that starts new sessions.
    pub fn starts_sessions(self) -> bool {
        self == Self::Github
    }

    /// What to do at the sender, for the setup tab and the ready card.
    pub fn steps(self) -> &'static str {
        match self {
            Self::Github => {
                "In the repository, open Settings › Webhooks › Add webhook. Paste the URL as the Payload URL, set Content type to application/json, paste the secret, and choose Issues and Issue comments."
            }
            Self::Standard => {
                "Add an endpoint with this URL in the sender's webhook settings. If the sender shows its own signing secret (whsec_…), give it to this webhook with Edit › Use my own; otherwise paste this one there."
            }
            Self::Bearer => {
                "Send each request with the header Authorization: Bearer and the secret, such as curl -H \"Authorization: Bearer <secret>\" -d @event.json <URL>."
            }
            Self::Anything => {
                "Send a POST with any body to the URL. Anyone with the URL can send; edit the webhook to add a check."
            }
        }
    }
}

/// The preset a webhook's check matches, for its icon and setup steps.
pub fn preset_of(verify: Option<&WebhookVerify>) -> Preset {
    match verify.map(|verify| verify.kind) {
        Some(WebhookVerifyKind::Github) => Preset::Github,
        Some(WebhookVerifyKind::Standard) => Preset::Standard,
        Some(WebhookVerifyKind::Bearer) => Preset::Bearer,
        Some(WebhookVerifyKind::HmacSha256) | None => Preset::Anything,
    }
}

/// What the form's "one session per" reads (H4).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ThreadBy {
    /// A JSON field of the body.
    Json,
    Header,
    /// No thread key: each event starts its own session.
    Event,
}

/// Reads the form's thread key into its choice and the field's text.
pub fn thread_by(thread_key: &str) -> (ThreadBy, String) {
    let text = thread_key.trim();
    if let Some(name) = text.strip_prefix("header:") {
        (ThreadBy::Header, name.trim().to_owned())
    } else if let Some(pointer) = text.strip_prefix("json:") {
        (ThreadBy::Json, pointer.trim().to_owned())
    } else if text.is_empty() {
        (ThreadBy::Event, String::new())
    } else {
        // Typed without its prefix: a pointer is JSON, else a header.
        if text.starts_with('/') {
            (ThreadBy::Json, text.to_owned())
        } else {
            (ThreadBy::Header, text.to_owned())
        }
    }
}

/// The thread key the form writes for a choice and its field.
pub fn thread_key_text(by: ThreadBy, value: &str) -> String {
    let value = value.trim();
    match by {
        ThreadBy::Event => String::new(),
        ThreadBy::Json => format!("json:{value}"),
        ThreadBy::Header => format!("header:{value}"),
    }
}

/// One condition of the form's filter, as its builder edits it (H4).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Condition {
    /// A header, else a JSON field.
    pub header: bool,
    /// The header's name or the JSON Pointer.
    pub name: String,
    pub values: Vec<String>,
}

/// The form's filter lines as conditions, or what is wrong with one.
pub fn conditions(filter: &str) -> Result<Vec<Condition>, String> {
    filter
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(|line| {
            let condition = parse_condition(line)?;
            Ok(match (condition.header, condition.json) {
                (Some(header), _) => Condition {
                    header: true,
                    name: header,
                    values: condition.one_of,
                },
                (None, json) => Condition {
                    header: false,
                    name: json.unwrap_or_default(),
                    values: condition.one_of,
                },
            })
        })
        .collect()
}

/// Conditions back as the form's filter lines; a condition with neither a
/// name nor values is left out.
pub fn conditions_text(conditions: &[Condition]) -> String {
    conditions
        .iter()
        .filter(|condition| !condition.name.trim().is_empty() || !condition.values.is_empty())
        .map(|condition| {
            let kind = if condition.header { "header" } else { "json" };
            format!(
                "{kind}:{}={}",
                condition.name.trim(),
                condition.values.join(",")
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
#[path = "webhook_view_tests.rs"]
mod tests;
