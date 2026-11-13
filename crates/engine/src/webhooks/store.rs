//! The webhook store (`docs/site/specs/webhooks.md` sections 4, 7.6, and
//! 12): `<data_root>/webhooks.json`, one versioned file holding every
//! webhook with its secrets, state, and history, and
//! `<data_root>/webhook-events/<id>/<event id>`, the bodies of the events
//! still waiting or in the history.
//!
//! Every change rereads the file under the memory lock, as `crons.json`'s
//! do, and writes it through a flushed temporary file renamed over it, with
//! mode `0600` since it holds secrets. Reads take the copy kept in memory.
//! Unknown fields are refused.
//!
//! The counts of requests turned away (rejected, filtered, duplicate, over
//! the rate, backlog full) are kept in memory and written with the
//! webhook's next change, so noise from the internet never writes the file.

use std::{
    collections::{HashMap, HashSet},
    fs, io,
    path::{Path, PathBuf},
    sync::Mutex,
};

use cookie_agent_protocol::{
    CronActor, CronRunEnd, PermissionMode, ProducerDeliveryMode, ProducerMessageId, RunId,
    SessionId, Webhook, WebhookCondition, WebhookCounts, WebhookDisabledReason, WebhookEvent,
    WebhookEventOutcome, WebhookHeader, WebhookId, WebhookSignatureEncoding, WebhookTarget,
    WebhookTemplate, WebhookValueSource, WebhookVerify, WebhookVerifyKind,
};
use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use crate::crons::{StoredRunEnd, StoredTemplate};
use crate::memory::{MemoryError, MemoryRoot};
use crate::threads::{ThreadMap, ThreadOwner};
use crate::webhooks::{
    filter::Condition,
    request::ValueSource,
    verify::{SignatureEncoding, Verifier},
};

/// The store's file in the data root.
pub(crate) const WEBHOOKS_FILE: &str = "webhooks.json";
/// The folder of event bodies in the data root.
pub(crate) const EVENTS_DIR: &str = "webhook-events";
/// The format this build writes.
const WEBHOOKS_VERSION: u32 = 1;
/// The most delivery IDs a webhook remembers (section 5.3).
pub(crate) const DELIVERY_IDS_KEPT: usize = 1024;

/// Why a webhook request was refused or failed.
#[derive(Debug, thiserror::Error)]
pub enum WebhookError {
    #[error("there is no webhook {0}")]
    NotFound(WebhookId),
    #[error("webhook {id} has no event {event}")]
    EventNotFound { id: WebhookId, event: String },
    #[error("the webhook's {field} was refused: {reason}")]
    Invalid { field: &'static str, reason: String },
    /// Reading or writing `webhooks.json` or an event's body failed.
    #[error("{0}")]
    Store(String),
}

impl WebhookError {
    pub(crate) fn invalid(field: &'static str, reason: impl Into<String>) -> Self {
        Self::Invalid {
            field,
            reason: reason.into(),
        }
    }
}

impl From<MemoryError> for WebhookError {
    fn from(error: MemoryError) -> Self {
        Self::Store(error.to_string())
    }
}

/// `webhooks.json`.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct WebhooksFile {
    version: u32,
    #[serde(default)]
    webhooks: Vec<StoredWebhook>,
}

/// A stored new-session target: a cron template (whose `thread_key` is
/// unused) and where each request's thread key is read.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StoredNewTarget {
    pub(crate) template: StoredTemplate,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) thread_key: Option<WebhookValueSource>,
}

/// A stored target.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
#[allow(clippy::large_enum_variant)]
pub(crate) enum StoredWebhookTarget {
    Session(SessionId),
    Subagent(SessionId),
    New(StoredNewTarget),
}

impl StoredWebhookTarget {
    pub(crate) const fn kind(&self) -> cookie_agent_protocol::CronTargetKind {
        match self {
            Self::Session(_) => cookie_agent_protocol::CronTargetKind::Session,
            Self::Subagent(_) => cookie_agent_protocol::CronTargetKind::Subagent,
            Self::New(_) => cookie_agent_protocol::CronTargetKind::New,
        }
    }

    pub(crate) const fn session(&self) -> Option<SessionId> {
        match self {
            Self::Session(session) | Self::Subagent(session) => Some(*session),
            Self::New(_) => None,
        }
    }

    pub(crate) const fn new_target(&self) -> Option<&StoredNewTarget> {
        match self {
            Self::New(target) => Some(target),
            _ => None,
        }
    }

    pub(crate) fn wire(&self) -> WebhookTarget {
        match self {
            Self::Session(session) => WebhookTarget::Session(*session),
            Self::Subagent(session) => WebhookTarget::Subagent(*session),
            Self::New(target) => {
                let template = &target.template;
                WebhookTarget::New(WebhookTemplate {
                    agent: template.agent.clone(),
                    preset: template.preset.clone(),
                    model: template.model.clone(),
                    permission_rules: template.permission_rules.clone(),
                    runtimes: template.runtimes.clone(),
                    global: template.global,
                    cwd: template.cwd.clone(),
                    project: template.project.clone(),
                    acp_runtime: template.acp_runtime.clone(),
                    thread_key: target.thread_key.clone(),
                    thread_prompt: template.thread_prompt.clone(),
                    title: template.title.clone(),
                    title_auto: template.title_auto,
                })
            }
        }
    }
}

impl From<WebhookTarget> for StoredWebhookTarget {
    fn from(target: WebhookTarget) -> Self {
        match target {
            WebhookTarget::Session(session) => Self::Session(session),
            WebhookTarget::Subagent(session) => Self::Subagent(session),
            WebhookTarget::New(template) => Self::New(StoredNewTarget {
                thread_key: template.thread_key.clone(),
                template: StoredTemplate {
                    agent: template.agent,
                    preset: template.preset,
                    model: template.model,
                    permission_rules: template.permission_rules,
                    runtimes: template.runtimes,
                    global: template.global,
                    cwd: template.cwd,
                    project: template.project,
                    acp_runtime: template.acp_runtime,
                    thread_key: None,
                    thread_prompt: template.thread_prompt,
                    title: template.title,
                    title_auto: template.title_auto,
                },
            }),
        }
    }
}

/// A stored check, with its secret.
#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum StoredVerify {
    Github {
        secret: String,
    },
    Standard {
        secret: String,
    },
    HmacSha256 {
        secret: String,
        header: String,
        encoding: WebhookSignatureEncoding,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prefix: Option<String>,
    },
    Bearer {
        token: String,
    },
}

impl std::fmt::Debug for StoredVerify {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Secrets never reach logs.
        formatter
            .debug_struct("StoredVerify")
            .field("kind", &self.kind())
            .finish_non_exhaustive()
    }
}

impl StoredVerify {
    pub(crate) const fn kind(&self) -> WebhookVerifyKind {
        match self {
            Self::Github { .. } => WebhookVerifyKind::Github,
            Self::Standard { .. } => WebhookVerifyKind::Standard,
            Self::HmacSha256 { .. } => WebhookVerifyKind::HmacSha256,
            Self::Bearer { .. } => WebhookVerifyKind::Bearer,
        }
    }

    pub(crate) fn secret(&self) -> &str {
        match self {
            Self::Github { secret }
            | Self::Standard { secret }
            | Self::HmacSha256 { secret, .. }
            | Self::Bearer { token: secret } => secret,
        }
    }

    pub(crate) fn with_secret(&self, secret: String) -> Self {
        match self {
            Self::Github { .. } => Self::Github { secret },
            Self::Standard { .. } => Self::Standard { secret },
            Self::HmacSha256 {
                header,
                encoding,
                prefix,
                ..
            } => Self::HmacSha256 {
                secret,
                header: header.clone(),
                encoding: *encoding,
                prefix: prefix.clone(),
            },
            Self::Bearer { .. } => Self::Bearer { token: secret },
        }
    }

    pub(crate) fn verifier(&self) -> Verifier {
        match self.clone() {
            Self::Github { secret } => Verifier::GitHub { secret },
            Self::Standard { secret } => Verifier::Standard { secret },
            Self::HmacSha256 {
                secret,
                header,
                encoding,
                prefix,
            } => Verifier::HmacSha256 {
                secret,
                header,
                encoding: match encoding {
                    WebhookSignatureEncoding::Hex => SignatureEncoding::Hex,
                    WebhookSignatureEncoding::Base64 => SignatureEncoding::Base64,
                },
                prefix,
            },
            Self::Bearer { token } => Verifier::Bearer { token },
        }
    }

    /// The check as reads show it: no secret.
    pub(crate) fn wire(&self) -> WebhookVerify {
        let mut verify = WebhookVerify::of_kind(self.kind());
        if let Self::HmacSha256 {
            header,
            encoding,
            prefix,
            ..
        } = self
        {
            verify.header = Some(header.clone());
            verify.encoding = Some(*encoding);
            verify.prefix.clone_from(prefix);
        }
        verify
    }
}

/// What a stored event did.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum StoredEventOutcome {
    Pending,
    Waiting,
    Delivered,
    Failed { error: String },
}

impl StoredEventOutcome {
    /// Whether the event still waits to be delivered.
    pub(crate) const fn is_open(&self) -> bool {
        matches!(self, Self::Pending | Self::Waiting)
    }
}

/// One stored event: what clients see and what delivery needs of its
/// request, but its body, which is a file of its own.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StoredEvent {
    pub(crate) id: String,
    pub(crate) received_at: Timestamp,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(crate) test: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) delivery_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) thread_key: Option<String>,
    pub(crate) bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) content_type: Option<String>,
    pub(crate) method: String,
    /// The request's headers but the secret ones.
    #[serde(default)]
    pub(crate) headers: Vec<WebhookHeader>,
    pub(crate) outcome: StoredEventOutcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) session_id: Option<SessionId>,
    /// The producer message that carries the event.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) message_id: Option<ProducerMessageId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) run_id: Option<RunId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) run_end: Option<StoredRunEnd>,
}

impl StoredEvent {
    /// The producer idempotency key the event is sent with: its delivery
    /// ID, else its own ID (section 7.2).
    pub(crate) fn key(&self) -> &str {
        self.delivery_id.as_deref().unwrap_or(&self.id)
    }

    /// A delivered event whose run has not ended yet.
    pub(crate) fn awaits_run(&self) -> bool {
        matches!(self.outcome, StoredEventOutcome::Delivered)
            && self.run_end.is_none()
            && self.session_id.is_some()
    }

    fn wire(&self) -> WebhookEvent {
        WebhookEvent {
            id: self.id.clone(),
            received_at: self.received_at,
            test: self.test,
            delivery_id: self.delivery_id.clone(),
            thread_key: self.thread_key.clone(),
            bytes: self.bytes,
            content_type: self.content_type.clone(),
            outcome: match &self.outcome {
                StoredEventOutcome::Pending => WebhookEventOutcome::Pending,
                StoredEventOutcome::Waiting => WebhookEventOutcome::Waiting,
                StoredEventOutcome::Delivered => WebhookEventOutcome::Delivered,
                StoredEventOutcome::Failed { error } => WebhookEventOutcome::Failed {
                    error: error.clone(),
                },
            }
            .into(),
            session_id: self.session_id,
            run_id: self.run_id,
            run_end: self.run_end.as_ref().map(|end| CronRunEnd {
                status: end.status,
                at: end.at,
            }),
        }
    }
}

/// Why a stored webhook is disabled.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum StoredDisabled {
    Failed { failures: u32 },
    Disabled { by: CronActor },
}

/// One stored webhook: its definition, secrets, state, and history.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StoredWebhook {
    pub(crate) id: WebhookId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) name: Option<String>,
    pub(crate) url_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) prompt: Option<String>,
    pub(crate) target: StoredWebhookTarget,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) delivery: Option<ProducerDeliveryMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) verify: Option<StoredVerify>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) filter: Vec<WebhookCondition>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) headers: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) max_per_hour: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) max_concurrent: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) permission_mode: Option<PermissionMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) max_consecutive_failures: Option<i64>,
    pub(crate) enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) disabled_reason: Option<StoredDisabled>,
    pub(crate) created_by: CronActor,
    pub(crate) created_at: Timestamp,
    #[serde(default)]
    pub(crate) counts: WebhookCounts,
    /// When the events of the last hour arrived, for `max_per_hour`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) recent: Vec<Timestamp>,
    /// The last delivery IDs seen, oldest first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) delivery_ids: Vec<String>,
    #[serde(default)]
    pub(crate) consecutive_failures: u32,
    /// The events still waiting and the history, in arrival order.
    #[serde(default)]
    pub(crate) events: Vec<StoredEvent>,
    #[serde(default, skip_serializing_if = "ThreadMap::is_empty")]
    pub(crate) threads: ThreadMap,
}

impl StoredWebhook {
    /// The webhook as clients and tools read it, with `noise` added to its
    /// counts.
    pub(crate) fn wire(&self, noise: Option<&WebhookCounts>) -> Webhook {
        let mut counts = self.counts.clone();
        if let Some(noise) = noise {
            add_counts(&mut counts, noise);
        }
        Webhook {
            id: self.id.clone(),
            name: self.name.clone(),
            url_token: self.url_token.clone(),
            path: format!(
                "{}{}",
                cookie_agent_protocol::WEBHOOK_PATH_PREFIX,
                self.url_token
            ),
            prompt: self.prompt.clone(),
            target: self.target.wire(),
            delivery: self.delivery,
            verify: self.verify.as_ref().map(StoredVerify::wire),
            filter: self.filter.clone(),
            headers: self.headers.clone(),
            max_per_hour: self.max_per_hour,
            max_concurrent: self.max_concurrent,
            permission_mode: self.permission_mode,
            max_consecutive_failures: self.max_consecutive_failures,
            enabled: self.enabled,
            disabled_reason: self.disabled_reason.as_ref().map(|reason| {
                match reason {
                    StoredDisabled::Failed { failures } => WebhookDisabledReason::Failed {
                        failures: *failures,
                    },
                    StoredDisabled::Disabled { by } => {
                        WebhookDisabledReason::Disabled { by: by.clone() }
                    }
                }
                .into()
            }),
            created_by: self.created_by.clone(),
            created_at: self.created_at,
            counts,
            consecutive_failures: self.consecutive_failures,
            waiting: u32::try_from(
                self.events
                    .iter()
                    .filter(|event| event.outcome == StoredEventOutcome::Waiting)
                    .count(),
            )
            .unwrap_or(u32::MAX),
            history: self.events.iter().map(StoredEvent::wire).collect(),
            threads: self.threads.wire(),
        }
    }

    /// The owner of the webhook's threads.
    pub(crate) fn thread_owner(&self) -> ThreadOwner {
        ThreadOwner::Webhook(self.id.clone())
    }

    /// The name lists and titles show: the webhook's name, else its ID.
    pub(crate) fn label(&self) -> String {
        self.name
            .clone()
            .unwrap_or_else(|| format!("Webhook {}", self.id))
    }

    /// The filter as the request side reads it.
    pub(crate) fn conditions(&self) -> Vec<Condition> {
        self.filter.iter().filter_map(condition).collect()
    }

    /// Where a new-session webhook reads its thread key.
    pub(crate) fn thread_source(&self) -> Option<ValueSource> {
        source(self.target.new_target()?.thread_key.as_ref()?)
    }

    /// One event.
    pub(crate) fn event(&self, id: &str) -> Option<&StoredEvent> {
        self.events.iter().find(|event| event.id == id)
    }

    pub(crate) fn event_mut(&mut self, id: &str) -> Option<&mut StoredEvent> {
        self.events.iter_mut().find(|event| event.id == id)
    }

    /// Events still waiting to be delivered.
    pub(crate) fn waiting_count(&self) -> usize {
        self.events
            .iter()
            .filter(|event| event.outcome == StoredEventOutcome::Waiting)
            .count()
    }

    /// Whether `delivery` was seen before.
    pub(crate) fn seen(&self, delivery: &str) -> bool {
        self.delivery_ids.iter().any(|seen| seen == delivery)
    }

    /// The events of the hour before `now`.
    pub(crate) fn recent_count(&self, now: Timestamp) -> usize {
        self.recent
            .iter()
            .filter(|at| within_hour(**at, now))
            .count()
    }

    /// Records an accepted event at `now`: its delivery ID, the rate
    /// window, the count. The event itself goes in with
    /// [`Self::push_event`].
    pub(crate) fn note_accepted(&mut self, delivery: Option<&str>, now: Timestamp, rated: bool) {
        if let Some(delivery) = delivery {
            self.delivery_ids.push(delivery.to_owned());
            let excess = self.delivery_ids.len().saturating_sub(DELIVERY_IDS_KEPT);
            self.delivery_ids.drain(..excess);
        }
        self.recent.retain(|at| within_hour(*at, now));
        if rated {
            self.recent.push(now);
        }
        self.counts.accepted = self.counts.accepted.saturating_add(1);
        self.counts.last_accepted = Some(now);
    }

    /// Appends `event`, keeping every open event and the last `limit`
    /// others.
    pub(crate) fn push_event(&mut self, event: StoredEvent, limit: usize) {
        self.events.push(event);
        self.prune(limit);
    }

    /// Drops the oldest finished events past `limit`.
    pub(crate) fn prune(&mut self, limit: usize) {
        let finished = self
            .events
            .iter()
            .filter(|event| !event.outcome.is_open())
            .count();
        let mut excess = finished.saturating_sub(limit.max(1));
        self.events.retain(|event| {
            if excess > 0 && !event.outcome.is_open() {
                excess -= 1;
                false
            } else {
                true
            }
        });
    }

    /// The failures in a row that disable the webhook, `None` when none do.
    pub(crate) fn failure_limit(&self, configured: Option<u64>) -> Option<u64> {
        match self.max_consecutive_failures {
            Some(limit) => u64::try_from(limit).ok().filter(|limit| *limit > 0),
            None => configured,
        }
    }

    /// Disables the webhook for `reason`.
    pub(crate) fn disable(&mut self, reason: StoredDisabled) {
        self.enabled = false;
        self.disabled_reason = Some(reason);
    }

    /// Counts a failure: disabled at the limit.
    pub(crate) fn count_failure(&mut self, configured_limit: Option<u64>) {
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        if self.enabled
            && self
                .failure_limit(configured_limit)
                .is_some_and(|limit| u64::from(self.consecutive_failures) >= limit)
        {
            self.disable(StoredDisabled::Failed {
                failures: self.consecutive_failures,
            });
        }
    }
}

fn within_hour(at: Timestamp, now: Timestamp) -> bool {
    now.duration_since(at) < jiff::SignedDuration::from_hours(1)
}

/// The request side's reading of a wire source: exactly one of its
/// members.
pub(crate) fn source(source: &WebhookValueSource) -> Option<ValueSource> {
    match (&source.header, &source.json) {
        (Some(header), None) => Some(ValueSource::Header(header.clone())),
        (None, Some(json)) => Some(ValueSource::Json(json.clone())),
        _ => None,
    }
}

/// The request side's reading of a wire condition.
pub(crate) fn condition(condition: &WebhookCondition) -> Option<Condition> {
    Some(Condition {
        source: source(&WebhookValueSource {
            header: condition.header.clone(),
            json: condition.json.clone(),
        })?,
        one_of: condition.one_of.clone(),
    })
}

/// Why requests were turned away, for [`WebhookStore::note_noise`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Noise {
    Rejected,
    Filtered,
    Duplicate,
    RateLimited,
    BacklogFull,
}

fn add_counts(counts: &mut WebhookCounts, more: &WebhookCounts) {
    counts.rejected = counts.rejected.saturating_add(more.rejected);
    counts.filtered = counts.filtered.saturating_add(more.filtered);
    counts.duplicate = counts.duplicate.saturating_add(more.duplicate);
    counts.rate_limited = counts.rate_limited.saturating_add(more.rate_limited);
    counts.backlog_full = counts.backlog_full.saturating_add(more.backlog_full);
    for (into, from) in [
        (&mut counts.last_rejected, more.last_rejected),
        (&mut counts.last_filtered, more.last_filtered),
        (&mut counts.last_duplicate, more.last_duplicate),
        (&mut counts.last_rate_limited, more.last_rate_limited),
        (&mut counts.last_backlog_full, more.last_backlog_full),
    ] {
        if from > *into {
            *into = from;
        }
    }
}

/// The daemon's webhook store.
pub(crate) struct WebhookStore {
    path: PathBuf,
    events_dir: PathBuf,
    memory: MemoryRoot,
    /// The webhooks as last read or written, in creation order.
    webhooks: Mutex<Vec<StoredWebhook>>,
    /// Requests turned away since the last write, by webhook.
    noise: Mutex<HashMap<WebhookId, WebhookCounts>>,
    /// Serializes changes within this process; the memory lock serializes
    /// processes.
    mutation: Mutex<()>,
    /// Bumped whenever the webhooks change.
    changes: tokio::sync::watch::Sender<u64>,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl WebhookStore {
    /// Reads `<data_dir>/webhooks.json`; a store that does not exist yet
    /// has no webhooks. A file that does not parse fails.
    pub(crate) fn open(data_dir: &Path) -> Result<Self, WebhookError> {
        let path = data_dir.join(WEBHOOKS_FILE);
        let file = read(&path)?;
        Ok(Self {
            path,
            events_dir: data_dir.join(EVENTS_DIR),
            memory: MemoryRoot::new(data_dir),
            webhooks: Mutex::new(file.webhooks),
            noise: Mutex::new(HashMap::new()),
            mutation: Mutex::new(()),
            changes: tokio::sync::watch::channel(0).0,
        })
    }

    /// Every webhook, in creation order.
    pub(crate) fn webhooks(&self) -> Vec<StoredWebhook> {
        lock(&self.webhooks).clone()
    }

    /// One webhook.
    pub(crate) fn webhook(&self, id: &WebhookId) -> Option<StoredWebhook> {
        lock(&self.webhooks)
            .iter()
            .find(|webhook| webhook.id == *id)
            .cloned()
    }

    /// The webhook whose URL token is `token`.
    pub(crate) fn by_token(&self, token: &str) -> Option<StoredWebhook> {
        lock(&self.webhooks)
            .iter()
            .find(|webhook| webhook.url_token == token)
            .cloned()
    }

    /// Every webhook as clients read it.
    pub(crate) fn wire(&self) -> Vec<Webhook> {
        let noise = lock(&self.noise).clone();
        lock(&self.webhooks)
            .iter()
            .map(|webhook| webhook.wire(noise.get(&webhook.id)))
            .collect()
    }

    /// One webhook as clients read it.
    pub(crate) fn wire_of(&self, webhook: &StoredWebhook) -> Webhook {
        webhook.wire(lock(&self.noise).get(&webhook.id))
    }

    /// Counts a request webhook `id` turned away at `at`.
    pub(crate) fn note_noise(&self, id: &WebhookId, why: Noise, at: Timestamp) {
        let mut noise = lock(&self.noise);
        let counts = noise.entry(id.clone()).or_default();
        let (count, last) = match why {
            Noise::Rejected => (&mut counts.rejected, &mut counts.last_rejected),
            Noise::Filtered => (&mut counts.filtered, &mut counts.last_filtered),
            Noise::Duplicate => (&mut counts.duplicate, &mut counts.last_duplicate),
            Noise::RateLimited => (&mut counts.rate_limited, &mut counts.last_rate_limited),
            Noise::BacklogFull => (&mut counts.backlog_full, &mut counts.last_backlog_full),
        };
        *count = count.saturating_add(1);
        *last = Some(at);
    }

    /// Whether any webhook has an event delivered into `session` whose run
    /// has not ended.
    pub(crate) fn awaits_run_in(&self, session: SessionId) -> bool {
        lock(&self.webhooks).iter().any(|webhook| {
            webhook
                .events
                .iter()
                .any(|event| event.awaits_run() && event.session_id == Some(session))
        })
    }

    /// The webhooks with an event delivered into, or a thread in,
    /// `session`: the ones a run's end there may concern.
    pub(crate) fn concerned_by(&self, session: SessionId) -> Vec<WebhookId> {
        let sessions = HashSet::from([session]);
        lock(&self.webhooks)
            .iter()
            .filter(|webhook| {
                webhook.threads.names_any(&sessions)
                    || webhook
                        .events
                        .iter()
                        .any(|event| event.session_id == Some(session))
            })
            .map(|webhook| webhook.id.clone())
            .collect()
    }

    /// Signals every change to the webhooks.
    pub(crate) fn subscribe(&self) -> tokio::sync::watch::Receiver<u64> {
        self.changes.subscribe()
    }

    /// The file an event's body is kept in.
    fn body_path(&self, webhook: &WebhookId, event: &str) -> PathBuf {
        self.events_dir.join(webhook.as_str()).join(event)
    }

    /// Writes an event's body before its entry is written. Blocking.
    pub(crate) fn write_body(
        &self,
        webhook: &WebhookId,
        event: &str,
        body: &[u8],
    ) -> Result<(), WebhookError> {
        let path = self.body_path(webhook, event);
        let failed = |error: io::Error| WebhookError::Store(format!("{}: {error}", path.display()));
        let directory = path.parent().expect("an event's body has a folder");
        create_private_dir(&self.events_dir).map_err(failed)?;
        create_private_dir(directory).map_err(failed)?;
        write_private(&path, body).map_err(failed)
    }

    /// An event's body; `None` once it is gone. Blocking.
    pub(crate) fn read_body(
        &self,
        webhook: &WebhookId,
        event: &str,
    ) -> Result<Option<Vec<u8>>, WebhookError> {
        let path = self.body_path(webhook, event);
        match fs::read(&path) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(WebhookError::Store(format!("{}: {error}", path.display()))),
        }
    }

    /// Removes an event's body, as when its entry was never written.
    pub(crate) fn remove_body(&self, webhook: &WebhookId, event: &str) {
        let _ = fs::remove_file(self.body_path(webhook, event));
    }

    /// Runs `change` on the webhooks as they are on disk, under the memory
    /// lock, and writes the result when it changed anything: the counts
    /// held in memory go in with it, and the bodies of events and webhooks
    /// that left are removed. Blocking.
    pub(crate) fn mutate<T>(
        &self,
        change: impl FnOnce(&mut Vec<StoredWebhook>) -> Result<T, WebhookError>,
    ) -> Result<(T, bool), WebhookError> {
        let _mutation = lock(&self.mutation);
        let memory_lock = self.memory.lock()?;
        // Another daemon sharing the data root may have changed it.
        let current = read(&self.path)?.webhooks;
        let mut webhooks = current.clone();
        let value = change(&mut webhooks)?;
        let changed = webhooks != current;
        if changed {
            let noise = std::mem::take(&mut *lock(&self.noise));
            for webhook in &mut webhooks {
                if let Some(more) = noise.get(&webhook.id) {
                    add_counts(&mut webhook.counts, more);
                }
            }
            let file = WebhooksFile {
                version: WEBHOOKS_VERSION,
                webhooks: webhooks.clone(),
            };
            if let Err(error) = write(&self.path, &file) {
                // The counts stay for the next write.
                let mut held = lock(&self.noise);
                for (id, more) in noise {
                    add_counts(held.entry(id).or_default(), &more);
                }
                return Err(error);
            }
            self.remove_left_bodies(&current, &webhooks);
        }
        drop(memory_lock);
        let mut cached = lock(&self.webhooks);
        if *cached != webhooks {
            *cached = webhooks;
            drop(cached);
            self.changes.send_modify(|revision| *revision += 1);
        }
        Ok((value, changed))
    }

    /// Removes the bodies of the events in `before` that `after` no longer
    /// holds, and the folders of webhooks that left.
    fn remove_left_bodies(&self, before: &[StoredWebhook], after: &[StoredWebhook]) {
        for webhook in before {
            match after.iter().find(|kept| kept.id == webhook.id) {
                None => {
                    let _ = fs::remove_dir_all(self.events_dir.join(webhook.id.as_str()));
                }
                Some(kept) => {
                    for event in &webhook.events {
                        if kept.event(&event.id).is_none() {
                            self.remove_body(&webhook.id, &event.id);
                        }
                    }
                }
            }
        }
    }
}

/// A new event ID no event of `webhook` has: eight lowercase letters and
/// digits.
pub(crate) fn new_event_id(webhook: &StoredWebhook) -> Result<String, WebhookError> {
    crate::webhooks::ids::new_webhook_id(|id| webhook.event(id).is_some())
        .map_err(|error| WebhookError::Store(error.to_string()))
}

/// The store in `path`; empty when it does not exist.
fn read(path: &Path) -> Result<WebhooksFile, WebhookError> {
    let bytes = match cookie_agent_models::secure_store::read_replaced(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(WebhooksFile {
                version: WEBHOOKS_VERSION,
                webhooks: Vec::new(),
            });
        }
        Err(error) => return Err(WebhookError::Store(format!("{}: {error}", path.display()))),
    };
    let file: WebhooksFile = serde_json::from_slice(&bytes)
        .map_err(|error| WebhookError::Store(format!("{}: {error}", path.display())))?;
    if file.version != WEBHOOKS_VERSION {
        return Err(WebhookError::Store(format!(
            "{}: version {} is not {WEBHOOKS_VERSION}",
            path.display(),
            file.version
        )));
    }
    Ok(file)
}

fn create_private_dir(path: &Path) -> io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(path)
}

/// Writes `bytes` to `path` through a flushed temporary file of mode
/// `0600` in the same folder, renamed over it.
fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let directory = path
        .parent()
        .ok_or_else(|| io::Error::other("the file has no parent folder"))?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("file");
    let temporary = directory.join(format!(".{name}.{}.tmp", uuid::Uuid::now_v7()));
    let result = (|| {
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let mut output = options.open(&temporary)?;
        io::Write::write_all(&mut output, bytes)?;
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
    result
}

/// Writes `file` to `path`, privately.
fn write(path: &Path, file: &WebhooksFile) -> Result<(), WebhookError> {
    let failed = |error: io::Error| WebhookError::Store(format!("{}: {error}", path.display()));
    let directory = path
        .parent()
        .ok_or_else(|| WebhookError::Store(format!("{} has no parent folder", path.display())))?;
    fs::create_dir_all(directory).map_err(failed)?;
    let mut bytes = serde_json::to_vec_pretty(file).expect("the webhook store serializes");
    bytes.push(b'\n');
    write_private(path, &bytes).map_err(failed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(text: &str) -> Timestamp {
        text.parse().unwrap()
    }

    fn event(id: &str, outcome: StoredEventOutcome) -> StoredEvent {
        StoredEvent {
            id: id.to_owned(),
            received_at: at("2026-11-12T10:00:00Z"),
            test: false,
            delivery_id: None,
            thread_key: None,
            bytes: 2,
            content_type: None,
            method: "POST".into(),
            headers: Vec::new(),
            outcome,
            session_id: None,
            message_id: None,
            run_id: None,
            run_end: None,
        }
    }

    pub(crate) fn webhook(id: &str) -> StoredWebhook {
        StoredWebhook {
            id: WebhookId::new(id.to_owned()).unwrap(),
            name: None,
            url_token: "AAAAAAAAAAA".into(),
            prompt: None,
            target: StoredWebhookTarget::Session(SessionId::new_v7()),
            delivery: Some(ProducerDeliveryMode::Queue),
            verify: None,
            filter: Vec::new(),
            headers: Vec::new(),
            max_per_hour: None,
            max_concurrent: None,
            permission_mode: None,
            max_consecutive_failures: None,
            enabled: true,
            disabled_reason: None,
            created_by: CronActor::User,
            created_at: at("2026-11-12T00:00:00Z"),
            counts: WebhookCounts::default(),
            recent: Vec::new(),
            delivery_ids: Vec::new(),
            consecutive_failures: 0,
            events: Vec::new(),
            threads: ThreadMap::default(),
        }
    }

    #[test]
    fn pruning_keeps_open_events_and_the_last_finished_ones() {
        let mut hook = webhook("w7k2m9qa");
        hook.push_event(event("a", StoredEventOutcome::Delivered), 2);
        hook.push_event(event("b", StoredEventOutcome::Waiting), 2);
        hook.push_event(event("c", StoredEventOutcome::Delivered), 2);
        hook.push_event(event("d", StoredEventOutcome::Delivered), 2);
        let ids = hook
            .events
            .iter()
            .map(|event| event.id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(ids, ["b", "c", "d"]);
    }

    #[test]
    fn delivery_ids_and_the_rate_window_are_bounded() {
        let mut hook = webhook("w7k2m9qa");
        let start = at("2026-11-12T10:00:00Z");
        for index in 0..=DELIVERY_IDS_KEPT {
            hook.note_accepted(Some(&format!("d{index}")), start, true);
        }
        assert_eq!(hook.delivery_ids.len(), DELIVERY_IDS_KEPT);
        assert!(!hook.seen("d0"));
        assert!(hook.seen(&format!("d{DELIVERY_IDS_KEPT}")));
        // An hour later the window is empty, and the next event drops the
        // old times.
        let later = start + jiff::SignedDuration::from_hours(1);
        assert_eq!(hook.recent_count(later), 0);
        hook.note_accepted(None, later, true);
        assert_eq!(hook.recent.len(), 1);
    }

    #[test]
    fn failures_disable_at_the_webhook_limit_or_the_configured_one() {
        let mut hook = webhook("w7k2m9qa");
        hook.max_consecutive_failures = Some(2);
        hook.count_failure(Some(10));
        assert!(hook.enabled);
        hook.count_failure(Some(10));
        assert!(!hook.enabled);

        let mut never = webhook("w7k2m9qb");
        never.max_consecutive_failures = Some(0);
        for _ in 0..5 {
            never.count_failure(Some(1));
        }
        assert!(never.enabled);

        let mut configured = webhook("w7k2m9qc");
        configured.count_failure(Some(1));
        assert!(!configured.enabled);
        let mut unlimited = webhook("w7k2m9qd");
        unlimited.count_failure(None);
        assert!(unlimited.enabled);
    }

    #[test]
    fn the_store_writes_privately_and_keeps_noise_until_a_change() {
        let dir = tempfile::tempdir().unwrap();
        let store = WebhookStore::open(dir.path()).unwrap();
        store
            .mutate(|webhooks| {
                webhooks.push(webhook("w7k2m9qa"));
                Ok(())
            })
            .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(dir.path().join(WEBHOOKS_FILE))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o077, 0);
        }
        let id = WebhookId::new("w7k2m9qa".to_owned()).unwrap();
        store.note_noise(&id, Noise::Rejected, at("2026-11-12T11:00:00Z"));
        assert_eq!(store.wire()[0].counts.rejected, 1);
        assert_eq!(
            read(&dir.path().join(WEBHOOKS_FILE)).unwrap().webhooks[0]
                .counts
                .rejected,
            0
        );
        store
            .mutate(|webhooks| {
                webhooks[0].name = Some("named".into());
                Ok(())
            })
            .unwrap();
        assert_eq!(
            read(&dir.path().join(WEBHOOKS_FILE)).unwrap().webhooks[0]
                .counts
                .rejected,
            1
        );
        assert_eq!(store.wire()[0].counts.rejected, 1);
    }

    #[test]
    fn bodies_leave_with_their_events_and_webhooks() {
        let dir = tempfile::tempdir().unwrap();
        let store = WebhookStore::open(dir.path()).unwrap();
        let id = WebhookId::new("w7k2m9qa".to_owned()).unwrap();
        store
            .mutate(|webhooks| {
                let mut hook = webhook("w7k2m9qa");
                hook.events.push(event("a", StoredEventOutcome::Delivered));
                hook.events.push(event("b", StoredEventOutcome::Delivered));
                webhooks.push(hook);
                Ok(())
            })
            .unwrap();
        store.write_body(&id, "a", b"{}").unwrap();
        store.write_body(&id, "b", b"[]").unwrap();
        store
            .mutate(|webhooks| {
                webhooks[0].events.remove(0);
                Ok(())
            })
            .unwrap();
        assert_eq!(store.read_body(&id, "a").unwrap(), None);
        assert_eq!(
            store.read_body(&id, "b").unwrap().as_deref(),
            Some(&b"[]"[..])
        );
        store
            .mutate(|webhooks| {
                webhooks.clear();
                Ok(())
            })
            .unwrap();
        assert!(!dir.path().join(EVENTS_DIR).join("w7k2m9qa").exists());
    }

    #[test]
    fn secrets_never_reach_reads_or_debug_output() {
        let verify = StoredVerify::Github {
            secret: "s3cret".into(),
        };
        assert_eq!(verify.wire().secret, None);
        assert!(!format!("{verify:?}").contains("s3cret"));
        let mut hook = webhook("w7k2m9qa");
        hook.verify = Some(verify);
        assert!(
            !serde_json::to_string(&hook.wire(None))
                .unwrap()
                .contains("s3cret")
        );
    }

    #[test]
    fn unknown_fields_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join(WEBHOOKS_FILE),
            r#"{"version":1,"webhooks":[],"extra":true}"#,
        )
        .unwrap();
        assert!(WebhookStore::open(dir.path()).is_err());
    }
}
