//! Delivering webhook events (`docs/site/specs/webhooks.md` sections 7.2
//! to 7.6): each webhook's events go out in arrival order, one at a time,
//! from a pump that runs after an event is stored, after a run of one of
//! the webhook's sessions ends, when the webhook changes, and at start.
//!
//! An event into an existing session is a producer message owned by the
//! webhook's source (`webhook:<id>`, see [`webhook_owner`]), with its
//! delivery ID, else its own ID, as the idempotency key, so a delivery
//! retried after a restart is accepted once. An event that needs a new
//! session waits in the backlog while the webhook's `max_concurrent`
//! sessions run; one whose thread has a session goes into it whatever the
//! limit. An event's outcome is the run it lands in, recorded when that run
//! ends, as a cron fire's is.

use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex},
};

use cookie_agent_protocol::{
    PermissionMode, ProducerDeliveryMode, ProducerIdempotencyKey, ProducerMessageId, ProducerName,
    ProducerOwner, ProducerSendParams, ProducerSource, RunId, SessionId, SessionStatus,
    SessionTitle, SessionWebhook, WebhookId,
};
use jiff::Timestamp;

use crate::crons::StoredRunEnd;
use crate::runtime::{
    Engine, EngineError,
    cron::api::host_zone,
    messaging::producers::ProducerAuthority,
    sessions::{CreatedBy, SessionCreator},
};
use crate::webhooks::{
    render::{EventAttributes, EventMessage, render_event, shown_body},
    request::{WebhookRequest, parse_body},
    store::{StoredEvent, StoredEventOutcome, StoredNewTarget, StoredWebhook, StoredWebhookTarget},
};

/// The producer name of every event.
const EVENT_PRODUCER_NAME: &str = "event";

/// The delivery state of webhooks.
#[derive(Default)]
pub(crate) struct WebhookState {
    /// Each webhook's pump lock: one pump delivers its events at a time.
    pumps: Mutex<HashMap<WebhookId, Arc<tokio::sync::Mutex<()>>>>,
    /// The events pending when the engine started, whose new session a
    /// daemon may have made before it stopped.
    started_pending: Mutex<HashSet<(WebhookId, String)>>,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The producer owner of webhook `id`'s events: an external owner whose
/// source is `webhook:<id>`, the daemon's own, as a cron job's is.
pub(crate) fn webhook_owner(id: &WebhookId) -> ProducerOwner {
    ProducerOwner::external(
        ProducerSource::webhook(id),
        ProducerName::new(EVENT_PRODUCER_NAME).expect("static producer name"),
    )
}

/// Where a delivered event went.
#[derive(Clone, Copy, Debug)]
struct Delivery {
    session: SessionId,
    message: ProducerMessageId,
}

/// What the pump does with an event now.
enum Plan {
    /// Into this session, with this delivery mode.
    Into(SessionId, ProducerDeliveryMode),
    /// Into a new session, or the key's thread.
    New,
    /// Wait in the backlog.
    Wait,
}

/// A new-session event's title: the webhook's label and the thread key or
/// the local time it arrived.
fn session_title(webhook: &StoredWebhook, event: &StoredEvent) -> Result<SessionTitle, String> {
    let suffix = match &event.thread_key {
        Some(key) => key.clone(),
        None => event
            .received_at
            .to_zoned(host_zone())
            .strftime("%Y-%m-%d %H:%M")
            .to_string(),
    };
    let mut label = webhook.label();
    let room = SessionTitle::MAX_BYTES.saturating_sub(suffix.len() + 8);
    while label.len() > room {
        label.pop();
    }
    SessionTitle::new(format!("{label} · {suffix}")).map_err(|error| error.to_string())
}

impl Engine {
    /// At start: the events left pending or waiting are delivered.
    pub(in crate::runtime) fn start_webhooks(&self) {
        let webhooks = self.inner.webhooks.webhooks();
        {
            let mut pending = lock(&self.inner.webhook_state.started_pending);
            for webhook in &webhooks {
                for event in &webhook.events {
                    if event.outcome == StoredEventOutcome::Pending {
                        pending.insert((webhook.id.clone(), event.id.clone()));
                    }
                }
            }
        }
        for webhook in webhooks {
            if webhook.events.iter().any(|event| event.outcome.is_open()) {
                self.spawn_webhook_pump(webhook.id);
            }
        }
    }

    /// Delivers what webhook `id` has to deliver, off the caller's task.
    pub(crate) fn spawn_webhook_pump(&self, id: WebhookId) {
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
            engine.pump_webhook(&id).await;
        });
    }

    /// Delivers webhook `id`'s open events in arrival order until each is
    /// delivered, failed, or waiting.
    pub(crate) async fn pump_webhook(&self, id: &WebhookId) {
        let pump = Arc::clone(
            lock(&self.inner.webhook_state.pumps)
                .entry(id.clone())
                .or_default(),
        );
        let _pumping = pump.lock().await;
        loop {
            if self.ensure_not_shutting_down().is_err() {
                return;
            }
            let Some(webhook) = self.inner.webhooks.webhook(id) else {
                return;
            };
            if !webhook.enabled {
                return;
            }
            let Some((event, plan)) = self.next_webhook_step(&webhook) else {
                return;
            };
            let result = match plan {
                Plan::Wait => {
                    if !self.record_webhook_waiting(&webhook, &event).await {
                        return;
                    }
                    continue;
                }
                Plan::Into(session, mode) => {
                    self.send_webhook_event(&webhook, &event, session, mode)
                        .await
                }
                Plan::New => self.deliver_new_webhook_session(&webhook, &event).await,
            };
            // A store that cannot record the outcome leaves the event open;
            // the next pump tries again rather than this one spinning.
            if !self.record_webhook_delivery(&webhook, &event, result).await {
                return;
            }
        }
    }

    /// The first open event that can go now and where, or the first
    /// pending one that has to wait. `None` when every open event waits.
    fn next_webhook_step(&self, webhook: &StoredWebhook) -> Option<(StoredEvent, Plan)> {
        let mut blocked_keys = HashSet::new();
        let mut new_blocked = false;
        let mut running = None;
        for event in webhook
            .events
            .iter()
            .filter(|event| event.outcome.is_open())
        {
            let plan = match &webhook.target {
                StoredWebhookTarget::Session(session) | StoredWebhookTarget::Subagent(session) => {
                    Plan::Into(
                        *session,
                        webhook.delivery.unwrap_or(ProducerDeliveryMode::Queue),
                    )
                }
                StoredWebhookTarget::New(_) => {
                    let thread = event.thread_key.as_ref().and_then(|key| {
                        webhook
                            .threads
                            .session(key)
                            .filter(|session| self.thread_session_live(*session))
                    });
                    let key_blocked = event
                        .thread_key
                        .as_ref()
                        .is_some_and(|key| blocked_keys.contains(key));
                    match thread {
                        // A key's events keep their order.
                        _ if key_blocked => Plan::Wait,
                        Some(session) => Plan::Into(
                            session,
                            webhook.delivery.unwrap_or(ProducerDeliveryMode::Queue),
                        ),
                        None => {
                            let running = *running
                                .get_or_insert_with(|| self.webhook_running_sessions(webhook));
                            let at_limit =
                                concurrency_limit(webhook).is_some_and(|limit| running >= limit);
                            if new_blocked || at_limit {
                                new_blocked = true;
                                Plan::Wait
                            } else {
                                Plan::New
                            }
                        }
                    }
                }
            };
            match plan {
                Plan::Wait => {
                    if let Some(key) = &event.thread_key {
                        blocked_keys.insert(key.clone());
                    }
                    if event.outcome == StoredEventOutcome::Pending {
                        return Some((event.clone(), Plan::Wait));
                    }
                }
                plan => return Some((event.clone(), plan)),
            }
        }
        None
    }

    /// How many of `webhook`'s sessions are running: busy, or holding one
    /// of its events no run has taken yet.
    fn webhook_running_sessions(&self, webhook: &StoredWebhook) -> usize {
        let sessions = webhook
            .events
            .iter()
            .filter(|event| event.outcome == StoredEventOutcome::Delivered)
            .filter_map(|event| event.session_id)
            .collect::<HashSet<_>>();
        sessions
            .into_iter()
            .filter(|session| {
                self.tree_busy(*session)
                    || webhook.events.iter().any(|event| {
                        event.session_id == Some(*session)
                            && event.awaits_run()
                            && event.message_id.is_some_and(|message| {
                                self.webhook_landed_run(*session, message).is_none()
                            })
                    })
            })
            .count()
    }

    /// The run producer message `message` of `session` landed in.
    fn webhook_landed_run(&self, session: SessionId, message: ProducerMessageId) -> Option<RunId> {
        let producers = self.goal_producer_projection(session).ok()?;
        let record = producers
            .messages
            .iter()
            .find(|record| record.message_id == message)?;
        record.consumed_run.or(record.admission.map(|(run, _)| run))
    }

    /// Marks a pending `event` as waiting. Whether the store recorded it.
    async fn record_webhook_waiting(&self, webhook: &StoredWebhook, event: &StoredEvent) -> bool {
        let (id, event) = (webhook.id.clone(), event.id.clone());
        if let Err(error) = self
            .webhooks_blocking(move |_, webhooks| {
                if let Some(event) = webhooks
                    .iter_mut()
                    .find(|webhook| webhook.id == id)
                    .and_then(|webhook| webhook.event_mut(&event))
                    && event.outcome == StoredEventOutcome::Pending
                {
                    event.outcome = StoredEventOutcome::Waiting;
                }
                Ok(())
            })
            .await
        {
            eprintln!("cookie-agent: webhook event: {error}");
            return false;
        }
        true
    }

    /// Records where `event` went, or why it failed, and counts a failure.
    /// The run it landed in may have ended already; its end is recorded
    /// now then. Whether the store recorded it.
    async fn record_webhook_delivery(
        &self,
        webhook: &StoredWebhook,
        event: &StoredEvent,
        result: Result<Delivery, String>,
    ) -> bool {
        let limit = self.inner.config.load().runtime.webhooks.failure_limit();
        let history = self.inner.config.load().runtime.webhooks.history;
        let (id, event_id) = (webhook.id.clone(), event.id.clone());
        let recorded = result.clone();
        if let Err(error) = self
            .webhooks_blocking(move |_, webhooks| {
                let Some(webhook) = webhooks.iter_mut().find(|webhook| webhook.id == id) else {
                    return Ok(());
                };
                let Some(event) = webhook.event_mut(&event_id) else {
                    return Ok(());
                };
                let test = event.test;
                match recorded {
                    Ok(delivery) => {
                        event.outcome = StoredEventOutcome::Delivered;
                        event.session_id = Some(delivery.session);
                        event.message_id = Some(delivery.message);
                    }
                    Err(error) => {
                        event.outcome = StoredEventOutcome::Failed { error };
                        if !test {
                            webhook.count_failure(limit);
                        }
                    }
                }
                webhook.prune(history);
                Ok(())
            })
            .await
        {
            eprintln!("cookie-agent: webhook event: {error}");
            return false;
        }
        lock(&self.inner.webhook_state.started_pending)
            .remove(&(webhook.id.clone(), event.id.clone()));
        if let Ok(delivery) = result
            && let Some(run) = self.webhook_landed_run(delivery.session, delivery.message)
            && let Err(error) = self.settle_webhook_runs(delivery.session, run).await
        {
            eprintln!("cookie-agent: webhook run outcome: {error}");
        }
        true
    }

    /// Delivers `event` into a new session from the webhook's template, or,
    /// with a thread key, the key's thread, which the first of its events
    /// creates.
    async fn deliver_new_webhook_session(
        &self,
        webhook: &StoredWebhook,
        event: &StoredEvent,
    ) -> Result<Delivery, String> {
        let Some(target) = webhook.target.new_target() else {
            return Err("the webhook no longer starts sessions".into());
        };
        let Some(key) = &event.thread_key else {
            let session = self
                .webhook_event_session(webhook, target, event)
                .await
                .map_err(|error| error.to_string())?;
            return self
                .send_webhook_event(webhook, event, session, ProducerDeliveryMode::Queue)
                .await;
        };
        let id = webhook.id.clone();
        let thread = self
            .thread_target(
                &webhook.thread_owner(),
                key,
                |engine| {
                    engine
                        .inner
                        .webhooks
                        .webhook(&id)
                        .and_then(|webhook| webhook.threads.session(key))
                },
                || async {
                    self.webhook_event_session(webhook, target, event)
                        .await
                        .map_err(|error| error.to_string())
                },
                |session| {
                    let id = webhook.id.clone();
                    let key = key.clone();
                    let at = event.received_at;
                    async move {
                        self.webhooks_blocking(move |_, webhooks| {
                            if let Some(webhook) =
                                webhooks.iter_mut().find(|webhook| webhook.id == id)
                            {
                                webhook.threads.record(&key, session, at);
                            }
                            Ok(())
                        })
                        .await
                        .map_err(|error| error.to_string())
                    }
                },
            )
            .await?;
        let mode = if thread.created {
            ProducerDeliveryMode::Queue
        } else {
            webhook.delivery.unwrap_or(ProducerDeliveryMode::Queue)
        };
        let delivered = self
            .send_webhook_event(webhook, event, thread.session, mode)
            .await;
        drop(thread.guard);
        delivered
    }

    /// The session `event` starts: one a daemon made for it before it
    /// stopped, else a new one.
    async fn webhook_event_session(
        &self,
        webhook: &StoredWebhook,
        target: &StoredNewTarget,
        event: &StoredEvent,
    ) -> Result<SessionId, EngineError> {
        let maybe_made = lock(&self.inner.webhook_state.started_pending)
            .contains(&(webhook.id.clone(), event.id.clone()));
        if maybe_made
            && let Some(made) = self.list_sessions().into_iter().find(|meta| {
                meta.webhook.as_ref().is_some_and(|created| {
                    created.webhook_id == webhook.id && created.event_id == event.id
                })
            })
        {
            return Ok(made.session_id);
        }
        self.create_webhook_session(webhook, target, event).await
    }

    /// Creates the root session a new-session event runs in: from the
    /// template, in its project, with the webhook's permission mode and the
    /// template's rules as its overlay, titled for the event, and recording
    /// the webhook.
    async fn create_webhook_session(
        &self,
        webhook: &StoredWebhook,
        target: &StoredNewTarget,
        event: &StoredEvent,
    ) -> Result<SessionId, EngineError> {
        let template = &target.template;
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
        let title = session_title(webhook, event).map_err(EngineError::ToolFailed)?;
        let meta = self
            .create_root_session(
                selection,
                Some(
                    webhook
                        .permission_mode
                        .unwrap_or(PermissionMode::AutoApproveN),
                ),
                template.permission_rules.clone(),
                runtimes,
                template.project.clone(),
                Some(SessionCreator {
                    by: CreatedBy::Webhook(SessionWebhook {
                        webhook_id: webhook.id.clone(),
                        event_id: event.id.clone(),
                        thread_key: event.thread_key.clone(),
                    }),
                    title,
                }),
                None,
            )
            .await?;
        Ok(meta.session_id)
    }

    /// Sends `event` to `session` as a producer message, once per its key:
    /// its body read back, a large or binary one stored in the session's
    /// artifacts, rendered as `<webhook_event>`.
    async fn send_webhook_event(
        &self,
        webhook: &StoredWebhook,
        event: &StoredEvent,
        session: SessionId,
        mode: ProducerDeliveryMode,
    ) -> Result<Delivery, String> {
        let store = Arc::clone(&self.inner.webhooks);
        let (id, event_id) = (webhook.id.clone(), event.id.clone());
        let body = tokio::task::spawn_blocking(move || store.read_body(&id, &event_id))
            .await
            .map_err(|error| error.to_string())?
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "the event's body is gone".to_owned())?;
        let request = WebhookRequest {
            method: event.method.clone(),
            headers: event
                .headers
                .iter()
                .map(|header| (header.name.clone(), header.value.clone()))
                .collect(),
            body,
        };
        let parsed = parse_body(&request);
        let inline = usize::try_from(self.inner.config.load().runtime.webhooks.inline_body_bytes)
            .unwrap_or(usize::MAX);
        let shown = shown_body(&request, &parsed, inline);
        // The artifact stays out of a revert's collection until the message
        // naming it is accepted.
        let publication = self.inner.artifacts.publication();
        let _publishing = publication.read().await;
        let artifact = if shown.wants_artifact() {
            Some(self.store_webhook_artifact(session, &request.body)?)
        } else {
            None
        };
        let received_at = event.received_at.to_zoned(host_zone());
        let secret = Self::webhook_verifier(webhook)
            .map(|verifier| verifier.secret_headers())
            .unwrap_or_default();
        let body = render_event(&EventMessage {
            attributes: EventAttributes {
                webhook_id: webhook.id.as_str(),
                name: webhook.name.as_deref(),
                event_id: &event.id,
                received_at: &received_at,
                delivery: event.delivery_id.as_deref(),
                thread: event.thread_key.as_deref(),
            },
            instructions: webhook.prompt.as_deref(),
            request: &request,
            extra_headers: &webhook.headers,
            secret_headers: &secret,
            body: &shown,
            artifact: artifact.as_deref(),
        });
        let authority = ProducerAuthority {
            owner: webhook_owner(&webhook.id),
            connection_epoch: None,
        };
        let key = ProducerIdempotencyKey::new(event.key().to_owned())
            .map_err(|error| error.to_string())?;
        let description = crate::runtime::messaging::producers::producer_description(
            "Webhook: ",
            &webhook.label(),
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
            .map(|message| Delivery { session, message })
            .map_err(|error| error.to_string())
    }

    /// Stores `body` in `session`'s artifacts; its `artifact://` URI.
    fn store_webhook_artifact(&self, session: SessionId, body: &[u8]) -> Result<String, String> {
        use sha2::Digest as _;
        let name = format!(".upload-{}.tmp", uuid::Uuid::now_v7());
        let artifacts = &self.inner.artifacts;
        let write = || -> std::io::Result<cookie_agent_protocol::ArtifactReference> {
            let mut file = artifacts.create_capture_file(session, &name)?;
            std::io::Write::write_all(&mut file, body)?;
            file.sync_all()?;
            drop(file);
            let digest = format!("{:x}", sha2::Sha256::digest(body));
            artifacts.commit_capture(session, &name, &digest, body.len() as u64)
        };
        crate::blocking::off_worker(write)
            .map(|reference| reference.uri)
            .map_err(|error| {
                artifacts.discard_capture(session, &name);
                format!("the body could not be stored: {error}")
            })
    }

    /// Called once a run of `session` ended: an event that landed in it
    /// records how it ended, and the webhooks it concerns deliver what
    /// waited.
    pub(in crate::runtime) fn observe_webhook_run(&self, session: SessionId, run: RunId) {
        let concerned = self.inner.webhooks.concerned_by(session);
        if concerned.is_empty() {
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
            if engine.inner.webhooks.awaits_run_in(session)
                && let Err(error) = engine.settle_webhook_runs(session, run).await
            {
                eprintln!("cookie-agent: webhook run outcome: {error}");
            }
            for id in concerned {
                engine.pump_webhook(&id).await;
            }
        });
    }

    /// Records the end of `run` of `session` on every event that landed in
    /// it: a failed run counts a failure, a completed one resets the count,
    /// a cancelled or interrupted one changes nothing. Test events count
    /// for nothing.
    pub(crate) async fn settle_webhook_runs(
        &self,
        session: SessionId,
        run: RunId,
    ) -> Result<(), EngineError> {
        let projection = self.inner.store.get(session)?;
        let Some(status) = projection.run_outcome(run)?.map(|run| run.status) else {
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
            message.is_some_and(|message| {
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
        let limit = self.inner.config.load().runtime.webhooks.failure_limit();
        let at = Timestamp::now();
        self.webhooks_blocking(move |_, webhooks| {
            for webhook in webhooks.iter_mut() {
                let mut failed = 0;
                let mut completed = false;
                for event in &mut webhook.events {
                    if event.session_id != Some(session)
                        || !event.awaits_run()
                        || !landed(event.message_id)
                    {
                        continue;
                    }
                    event.run_id = Some(run);
                    event.run_end = Some(StoredRunEnd { status, at });
                    if event.test {
                        continue;
                    }
                    match status {
                        SessionStatus::Failed => failed += 1,
                        SessionStatus::Completed => completed = true,
                        _ => {}
                    }
                }
                if completed {
                    webhook.consecutive_failures = 0;
                }
                for _ in 0..failed {
                    webhook.count_failure(limit);
                }
            }
            Ok(())
        })
        .await
    }
}

/// The most sessions of `webhook` that may run at once; `None` for no
/// limit.
fn concurrency_limit(webhook: &StoredWebhook) -> Option<usize> {
    match webhook.max_concurrent {
        None => Some(1),
        Some(limit) => usize::try_from(limit).ok().filter(|limit| *limit > 0),
    }
}
