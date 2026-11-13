//! Webhook requests (`docs/site/specs/webhooks.md` sections 4, 6, 9, 10,
//! and 13): listing, creating, changing, rotating, and deleting webhooks,
//! reading a secret or an event, and test events, for clients and for the
//! webhook tools, and the checks every definition passes.
//!
//! A client may set anything. An agent's request never names a permission
//! mode or a model, and never reads or rotates a secret after it created
//! the webhook; its targets are checked by the tools before they reach
//! here.

use std::collections::HashSet;

use base64::Engine as _;
use cookie_agent_protocol::{
    PermissionMode, ProducerDeliveryMode, ProjectId, SessionId, SessionOrigin, WebhookCondition,
    WebhookCreateParams, WebhookEventParams, WebhookEventResult, WebhookId, WebhookResult,
    WebhookRotate, WebhookSignatureEncoding, WebhookTarget, WebhookTestParams, WebhookTestResult,
    WebhookUpdateParams, WebhookVerify, WebhookVerifyKind, WebhooksListResult,
};
use jiff::Timestamp;

use crate::runtime::{Engine, EngineError, cron::api::CronRequester};
use crate::webhooks::{
    filter,
    request::{ValueSource, WebhookRequest},
    store::{
        StoredDisabled, StoredVerify, StoredWebhook, StoredWebhookTarget, WebhookError, source,
    },
    verify::{SecretKind, Verifier, generate_secret},
};

/// A webhook's definition after its checks: what a create sets and an
/// update replaces.
#[derive(Clone, Debug)]
struct Definition {
    name: Option<String>,
    prompt: Option<String>,
    target: StoredWebhookTarget,
    delivery: Option<ProducerDeliveryMode>,
    filter: Vec<WebhookCondition>,
    headers: Vec<String>,
    max_per_hour: Option<i64>,
    max_concurrent: Option<i64>,
    permission_mode: Option<PermissionMode>,
    max_consecutive_failures: Option<i64>,
}

fn check_name(name: Option<String>) -> Result<Option<String>, WebhookError> {
    let Some(name) = name else {
        return Ok(None);
    };
    let name = name.trim().to_owned();
    if name.is_empty() {
        return Err(WebhookError::invalid("name", "must not be blank"));
    }
    if name.chars().count() > cookie_agent_protocol::WEBHOOK_NAME_MAX_CHARS {
        return Err(WebhookError::invalid(
            "name",
            format!(
                "must be at most {} characters",
                cookie_agent_protocol::WEBHOOK_NAME_MAX_CHARS
            ),
        ));
    }
    if name.chars().any(char::is_control) {
        return Err(WebhookError::invalid(
            "name",
            "must not contain control characters",
        ));
    }
    Ok(Some(name))
}

fn check_prompt(prompt: Option<String>) -> Result<Option<String>, WebhookError> {
    let Some(prompt) = prompt else {
        return Ok(None);
    };
    if prompt.trim().is_empty() {
        return Err(WebhookError::invalid("prompt", "must not be blank"));
    }
    if prompt.len() > cookie_agent_protocol::WEBHOOK_PROMPT_MAX_BYTES {
        return Err(WebhookError::invalid(
            "prompt",
            format!(
                "must be at most {} bytes",
                cookie_agent_protocol::WEBHOOK_PROMPT_MAX_BYTES
            ),
        ));
    }
    Ok(Some(prompt))
}

fn check_filter(conditions: &[WebhookCondition]) -> Result<(), WebhookError> {
    let mut read = Vec::new();
    for condition in conditions {
        let Some(condition) = crate::webhooks::store::condition(condition) else {
            return Err(WebhookError::invalid(
                "filter",
                "each condition names exactly one of `header` and `json`",
            ));
        };
        read.push(condition);
    }
    filter::validate(&read).map_err(|error| WebhookError::invalid("filter", error.to_string()))
}

fn check_headers(headers: Vec<String>) -> Result<Vec<String>, WebhookError> {
    if headers.len() > cookie_agent_protocol::WEBHOOK_HEADERS_MAX {
        return Err(WebhookError::invalid(
            "headers",
            format!(
                "at most {} headers",
                cookie_agent_protocol::WEBHOOK_HEADERS_MAX
            ),
        ));
    }
    let mut seen = HashSet::new();
    let mut kept = Vec::new();
    for header in headers {
        ValueSource::Header(header.clone())
            .validate()
            .map_err(|error| WebhookError::invalid("headers", error.to_string()))?;
        if seen.insert(header.to_ascii_lowercase()) {
            kept.push(header);
        }
    }
    Ok(kept)
}

/// The stored check for `verify`: its secret, else `kept` when the kind
/// and shape stay, else a new one. Whether the secret is new.
fn stored_verify(
    verify: WebhookVerify,
    kept: Option<&StoredVerify>,
) -> Result<(StoredVerify, bool), WebhookError> {
    let refused = |reason: String| WebhookError::invalid("verify", reason);
    let hmac = verify.kind == WebhookVerifyKind::HmacSha256;
    if !hmac && (verify.header.is_some() || verify.encoding.is_some() || verify.prefix.is_some()) {
        return Err(refused(
            "header, encoding, and prefix belong to hmac_sha256".into(),
        ));
    }
    let (secret, fresh) = match (
        verify.secret,
        kept.filter(|kept| kept.kind() == verify.kind),
    ) {
        (Some(secret), _) => (secret, true),
        (None, Some(kept)) => (kept.secret().to_owned(), false),
        (None, None) => (
            generate_secret(if verify.kind == WebhookVerifyKind::Standard {
                SecretKind::Standard
            } else {
                SecretKind::Plain
            })
            .map_err(|error| WebhookError::Store(error.to_string()))?,
            true,
        ),
    };
    let stored = match verify.kind {
        WebhookVerifyKind::Github => StoredVerify::Github { secret },
        WebhookVerifyKind::Standard => StoredVerify::Standard { secret },
        WebhookVerifyKind::Bearer => StoredVerify::Bearer { token: secret },
        WebhookVerifyKind::HmacSha256 => StoredVerify::HmacSha256 {
            secret,
            header: verify
                .header
                .ok_or_else(|| refused("hmac_sha256 needs a header".into()))?,
            encoding: verify.encoding.unwrap_or(WebhookSignatureEncoding::Hex),
            prefix: verify.prefix.filter(|prefix| !prefix.is_empty()),
        },
    };
    stored
        .verifier()
        .validate()
        .map_err(|error| refused(error.to_string()))?;
    Ok((stored, fresh))
}

/// A secret for `verify`'s kind, made now.
fn new_secret(verify: &StoredVerify) -> Result<String, WebhookError> {
    generate_secret(if verify.kind() == WebhookVerifyKind::Standard {
        SecretKind::Standard
    } else {
        SecretKind::Plain
    })
    .map_err(|error| WebhookError::Store(error.to_string()))
}

/// A URL token no webhook in `webhooks` has.
fn new_token(webhooks: &[StoredWebhook]) -> Result<String, WebhookError> {
    loop {
        let token = crate::webhooks::ids::new_url_token()
            .map_err(|error| WebhookError::Store(error.to_string()))?;
        if webhooks.iter().all(|webhook| webhook.url_token != token) {
            return Ok(token);
        }
    }
}

/// Refuses what only a client may set in an agent's request: a permission
/// mode, or a model pin.
fn refuse_client_fields(
    permission_mode: bool,
    target: Option<&WebhookTarget>,
) -> Result<(), WebhookError> {
    if permission_mode {
        return Err(WebhookError::invalid(
            "permission_mode",
            "only a client can set a webhook's permission mode",
        ));
    }
    if let Some(WebhookTarget::New(template)) = target
        && template.model.is_some()
    {
        return Err(WebhookError::invalid(
            "target",
            "only a client can pin a webhook's model",
        ));
    }
    Ok(())
}

impl Engine {
    /// Whether `target`'s session exists and has the kind the target names.
    fn check_webhook_session(&self, target: &StoredWebhookTarget) -> Result<(), WebhookError> {
        let Some(session) = target.session() else {
            return Ok(());
        };
        let summary = self.inner.store.summary(session).map_err(|_| {
            WebhookError::invalid("target", format!("there is no session {session}"))
        })?;
        match (target, &summary.meta.origin) {
            (StoredWebhookTarget::Session(_), SessionOrigin::Root)
            | (StoredWebhookTarget::Subagent(_), SessionOrigin::Delegated { .. }) => Ok(()),
            (StoredWebhookTarget::Session(_), _) => Err(WebhookError::invalid(
                "target",
                format!("session {session} is a subagent's session; target it as `subagent`"),
            )),
            _ => Err(WebhookError::invalid(
                "target",
                format!("session {session} is a root session; target it as `session`"),
            )),
        }
    }

    /// Checks a definition and fills in the defaults of its kind.
    fn check_webhook_definition(&self, definition: Definition) -> Result<Definition, WebhookError> {
        self.check_webhook_session(&definition.target)?;
        let (delivery, permission_mode, max_concurrent) = match &definition.target {
            StoredWebhookTarget::New(target) => {
                self.check_cron_template(&target.template)
                    .map_err(|error| match error {
                        crate::crons::CronError::Invalid { reason, .. } => {
                            WebhookError::invalid("target", reason)
                        }
                        other => WebhookError::Store(other.to_string()),
                    })?;
                if let Some(key) = &target.thread_key {
                    source(key)
                        .ok_or_else(|| {
                            WebhookError::invalid(
                                "target",
                                "thread_key names exactly one of `header` and `json`",
                            )
                        })?
                        .validate()
                        .map_err(|error| {
                            WebhookError::invalid("target", format!("thread_key: {error}"))
                        })?;
                }
                let delivery = if target.thread_key.is_some() {
                    Some(definition.delivery.unwrap_or(ProducerDeliveryMode::Queue))
                } else if definition.delivery.is_some() {
                    return Err(WebhookError::invalid(
                        "delivery",
                        "a new-session webhook without a thread key starts a session for every \
                         event; delivery applies to existing sessions and thread keys",
                    ));
                } else {
                    None
                };
                (
                    delivery,
                    Some(
                        definition
                            .permission_mode
                            .unwrap_or(PermissionMode::AutoApproveN),
                    ),
                    definition.max_concurrent,
                )
            }
            StoredWebhookTarget::Session(_) | StoredWebhookTarget::Subagent(_) => {
                if definition.permission_mode.is_some() {
                    return Err(WebhookError::invalid(
                        "permission_mode",
                        "an existing-session webhook runs with its session's permission mode",
                    ));
                }
                if definition.max_concurrent.is_some() {
                    return Err(WebhookError::invalid(
                        "max_concurrent",
                        "applies to new-session webhooks",
                    ));
                }
                (
                    Some(definition.delivery.unwrap_or(ProducerDeliveryMode::Queue)),
                    None,
                    None,
                )
            }
        };
        check_filter(&definition.filter)?;
        Ok(Definition {
            name: check_name(definition.name)?,
            prompt: check_prompt(definition.prompt)?,
            headers: check_headers(definition.headers)?,
            delivery,
            permission_mode,
            max_concurrent,
            ..definition
        })
    }

    /// Runs `change` on the webhook store off the async workers.
    pub(crate) async fn webhooks_blocking<T: Send + 'static>(
        &self,
        change: impl FnOnce(&Engine, &mut Vec<StoredWebhook>) -> Result<T, WebhookError>
        + Send
        + 'static,
    ) -> Result<T, EngineError> {
        let engine = self.clone();
        let (value, _) = tokio::task::spawn_blocking(move || {
            let store = std::sync::Arc::clone(&engine.inner.webhooks);
            store.mutate(|webhooks| change(&engine, webhooks))
        })
        .await
        .map_err(|error| WebhookError::Store(format!("the request did not finish: {error}")))??;
        Ok(value)
    }

    /// [`Self::webhooks_blocking`] for a caller that is not async.
    pub(crate) fn webhooks_now<T>(
        &self,
        change: impl FnOnce(&Engine, &mut Vec<StoredWebhook>) -> Result<T, WebhookError>,
    ) -> Result<T, EngineError> {
        let (value, _) = crate::blocking::off_worker(|| {
            self.inner
                .webhooks
                .mutate(|webhooks| change(self, webhooks))
        })?;
        Ok(value)
    }

    /// Signals every change to the webhooks.
    #[must_use]
    pub fn subscribe_webhook_changes(&self) -> tokio::sync::watch::Receiver<u64> {
        self.inner.webhooks.subscribe()
    }

    /// `webhooks.list`: every webhook, in creation order.
    #[must_use]
    pub fn list_webhooks(&self) -> WebhooksListResult {
        WebhooksListResult {
            webhooks: self.inner.webhooks.wire(),
            receiving: self.inner.config.load().runtime.webhooks.enabled,
            public_base: self.share_base(),
        }
    }

    /// `webhooks.create` as you.
    pub async fn create_webhook(
        &self,
        params: WebhookCreateParams,
    ) -> Result<WebhookResult, EngineError> {
        self.create_webhook_as(CronRequester::User, params).await
    }

    /// `webhooks.update` as you.
    pub async fn update_webhook(
        &self,
        params: WebhookUpdateParams,
    ) -> Result<WebhookResult, EngineError> {
        self.update_webhook_as(CronRequester::User, params).await
    }

    /// `webhooks.delete`.
    pub async fn delete_webhook(&self, id: WebhookId) -> Result<(), EngineError> {
        self.webhooks_blocking(move |_, webhooks| {
            let before = webhooks.len();
            webhooks.retain(|webhook| webhook.id != id);
            if webhooks.len() == before {
                return Err(WebhookError::NotFound(id));
            }
            Ok(())
        })
        .await
    }

    /// `webhooks.rotate`: a new URL token, or a new signing secret.
    pub async fn rotate_webhook(
        &self,
        id: WebhookId,
        what: WebhookRotate,
    ) -> Result<WebhookResult, EngineError> {
        self.ensure_not_shutting_down()?;
        let (webhook, secret) = self
            .webhooks_blocking(move |_, webhooks| {
                let token = match what {
                    WebhookRotate::Url => Some(new_token(webhooks)?),
                    WebhookRotate::Secret => None,
                };
                let webhook = webhooks
                    .iter_mut()
                    .find(|webhook| webhook.id == id)
                    .ok_or_else(|| WebhookError::NotFound(id.clone()))?;
                let mut secret = None;
                match what {
                    WebhookRotate::Url => {
                        webhook.url_token = token.expect("made above");
                    }
                    WebhookRotate::Secret => {
                        let verify = webhook.verify.as_ref().ok_or_else(|| {
                            WebhookError::invalid("verify", "the webhook has no signature check")
                        })?;
                        let fresh = new_secret(verify)?;
                        webhook.verify = Some(verify.with_secret(fresh.clone()));
                        secret = Some(fresh);
                    }
                }
                Ok((webhook.clone(), secret))
            })
            .await?;
        Ok(WebhookResult {
            webhook: self.inner.webhooks.wire_of(&webhook),
            secret,
        })
    }

    /// `webhooks.secret`: the signing secret, or bearer token.
    pub fn webhook_secret(&self, id: &WebhookId) -> Result<Option<String>, EngineError> {
        let webhook = self
            .inner
            .webhooks
            .webhook(id)
            .ok_or_else(|| WebhookError::NotFound(id.clone()))?;
        Ok(webhook
            .verify
            .as_ref()
            .map(|verify| verify.secret().to_owned()))
    }

    /// `webhooks.event`: one event with its request.
    pub async fn webhook_event(
        &self,
        params: WebhookEventParams,
    ) -> Result<WebhookEventResult, EngineError> {
        let webhook = self
            .inner
            .webhooks
            .webhook(&params.id)
            .ok_or_else(|| WebhookError::NotFound(params.id.clone()))?;
        let stored =
            webhook
                .event(&params.event)
                .cloned()
                .ok_or_else(|| WebhookError::EventNotFound {
                    id: params.id.clone(),
                    event: params.event.clone(),
                })?;
        let store = std::sync::Arc::clone(&self.inner.webhooks);
        let (id, event) = (params.id.clone(), params.event.clone());
        let body = tokio::task::spawn_blocking(move || store.read_body(&id, &event))
            .await
            .map_err(|error| WebhookError::Store(error.to_string()))??;
        let wire = self
            .inner
            .webhooks
            .wire_of(&webhook)
            .history
            .into_iter()
            .find(|event| event.id == params.event)
            .expect("the event is in the history");
        let (body_text, body_base64) = match body {
            None => (None, None),
            Some(bytes) => match String::from_utf8(bytes) {
                Ok(text) => (Some(text), None),
                Err(error) => (
                    None,
                    Some(base64::engine::general_purpose::STANDARD.encode(error.into_bytes())),
                ),
            },
        };
        Ok(WebhookEventResult {
            event: wire,
            method: stored.method,
            headers: stored.headers,
            body_text,
            body_base64,
        })
    }

    /// `webhooks.test`: an event that skips the checks, the filter, and the
    /// limits, delivered as any other.
    pub async fn test_webhook(
        &self,
        params: WebhookTestParams,
    ) -> Result<WebhookTestResult, EngineError> {
        self.ensure_not_shutting_down()?;
        let webhook = self
            .inner
            .webhooks
            .webhook(&params.id)
            .ok_or_else(|| WebhookError::NotFound(params.id.clone()))?;
        let mut headers = params
            .headers
            .into_iter()
            .map(|header| (header.name, header.value))
            .collect::<Vec<_>>();
        if !headers
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case("content-type"))
        {
            headers.push((
                "Content-Type".to_owned(),
                params
                    .content_type
                    .unwrap_or_else(|| "application/json".to_owned()),
            ));
        }
        let request = WebhookRequest {
            method: "POST".to_owned(),
            headers,
            body: params.body.unwrap_or_else(|| "{}".to_owned()).into_bytes(),
        };
        let event = self.store_webhook_event(&webhook, request, true).await?;
        self.spawn_webhook_pump(webhook.id.clone());
        let webhook = self.inner.webhooks.webhook(&webhook.id).map_or_else(
            || self.inner.webhooks.wire_of(&webhook),
            |webhook| self.inner.webhooks.wire_of(&webhook),
        );
        Ok(WebhookTestResult { webhook, event })
    }

    /// Creates a webhook for `requester`; the result carries its secret.
    pub(crate) async fn create_webhook_as(
        &self,
        requester: CronRequester,
        params: WebhookCreateParams,
    ) -> Result<WebhookResult, EngineError> {
        self.ensure_not_shutting_down()?;
        if matches!(requester, CronRequester::Agent(_)) {
            refuse_client_fields(params.permission_mode.is_some(), Some(&params.target))?;
        }
        let now = Timestamp::now();
        let (webhook, secret) = self
            .webhooks_blocking(move |engine, webhooks| {
                let definition = engine.check_webhook_definition(Definition {
                    name: params.name,
                    prompt: params.prompt,
                    target: params.target.into(),
                    delivery: params.delivery,
                    filter: params.filter,
                    headers: params.headers,
                    max_per_hour: params.max_per_hour,
                    max_concurrent: params.max_concurrent,
                    permission_mode: params.permission_mode,
                    max_consecutive_failures: params.max_consecutive_failures,
                })?;
                let verify = params
                    .verify
                    .map(|verify| stored_verify(verify, None))
                    .transpose()?;
                let secret = verify
                    .as_ref()
                    .map(|(verify, _)| verify.secret().to_owned());
                let id = crate::webhooks::ids::new_webhook_id(|id| {
                    webhooks.iter().any(|webhook| webhook.id.as_str() == id)
                })
                .map_err(|error| WebhookError::Store(error.to_string()))?;
                let enabled = params.enabled.unwrap_or(true);
                let webhook = StoredWebhook {
                    id: WebhookId::new(id).expect("eight base-36 characters"),
                    name: definition.name,
                    url_token: new_token(webhooks)?,
                    prompt: definition.prompt,
                    target: definition.target,
                    delivery: definition.delivery,
                    verify: verify.map(|(verify, _)| verify),
                    filter: definition.filter,
                    headers: definition.headers,
                    max_per_hour: definition.max_per_hour,
                    max_concurrent: definition.max_concurrent,
                    permission_mode: definition.permission_mode,
                    max_consecutive_failures: definition.max_consecutive_failures,
                    enabled,
                    disabled_reason: (!enabled).then(|| StoredDisabled::Disabled {
                        by: requester.actor(),
                    }),
                    created_by: requester.actor(),
                    created_at: now,
                    counts: Default::default(),
                    recent: Vec::new(),
                    delivery_ids: Vec::new(),
                    consecutive_failures: 0,
                    events: Vec::new(),
                    threads: crate::threads::ThreadMap::default(),
                };
                webhooks.push(webhook.clone());
                Ok((webhook, secret))
            })
            .await?;
        Ok(WebhookResult {
            webhook: self.inner.webhooks.wire_of(&webhook),
            secret,
        })
    }

    /// Changes a webhook for `requester`. Enabling it resets its failure
    /// count and delivers what waited.
    pub(crate) async fn update_webhook_as(
        &self,
        requester: CronRequester,
        params: WebhookUpdateParams,
    ) -> Result<WebhookResult, EngineError> {
        self.ensure_not_shutting_down()?;
        if matches!(requester, CronRequester::Agent(_)) {
            refuse_client_fields(params.permission_mode.is_some(), params.target.as_ref())?;
        }
        let id = params.id.clone();
        let (webhook, secret) = self
            .webhooks_blocking(move |engine, webhooks| {
                let webhook = webhooks
                    .iter_mut()
                    .find(|webhook| webhook.id == params.id)
                    .ok_or_else(|| WebhookError::NotFound(params.id.clone()))?;
                let target: StoredWebhookTarget = params
                    .target
                    .clone()
                    .map_or_else(|| webhook.target.clone(), Into::into);
                let kind_changed = target.kind() != webhook.target.kind();
                let in_session = target.session().is_some();
                let threaded = target
                    .new_target()
                    .is_some_and(|target| target.thread_key.is_some());
                let threads_kept = !kind_changed
                    && target.new_target().map(|target| &target.thread_key)
                        == webhook.target.new_target().map(|target| &target.thread_key);
                let delivery = params.delivery.or(
                    if (in_session || threaded) && !kind_changed && webhook.delivery.is_some() {
                        webhook.delivery
                    } else {
                        None
                    },
                );
                let permission_mode = match params.permission_mode {
                    Some(mode) => mode,
                    None if !in_session && !kind_changed => webhook.permission_mode,
                    None => None,
                };
                let max_concurrent = match params.max_concurrent {
                    Some(limit) => limit,
                    None if !in_session => webhook.max_concurrent,
                    None => None,
                };
                let definition = engine.check_webhook_definition(Definition {
                    name: match params.name.clone() {
                        Some(name) => name,
                        None => webhook.name.clone(),
                    },
                    prompt: match params.prompt.clone() {
                        Some(prompt) => prompt,
                        None => webhook.prompt.clone(),
                    },
                    target,
                    delivery,
                    filter: params
                        .filter
                        .clone()
                        .unwrap_or_else(|| webhook.filter.clone()),
                    headers: params
                        .headers
                        .clone()
                        .unwrap_or_else(|| webhook.headers.clone()),
                    max_per_hour: match params.max_per_hour {
                        Some(limit) => limit,
                        None => webhook.max_per_hour,
                    },
                    max_concurrent,
                    permission_mode,
                    max_consecutive_failures: match params.max_consecutive_failures {
                        Some(limit) => limit,
                        None => webhook.max_consecutive_failures,
                    },
                })?;
                let mut secret = None;
                match params.verify.clone() {
                    None => {}
                    Some(None) => webhook.verify = None,
                    Some(Some(verify)) => {
                        let (verify, fresh) = stored_verify(verify, webhook.verify.as_ref())?;
                        if fresh {
                            secret = Some(verify.secret().to_owned());
                        }
                        webhook.verify = Some(verify);
                    }
                }
                webhook.name = definition.name;
                webhook.prompt = definition.prompt;
                webhook.target = definition.target;
                webhook.delivery = definition.delivery;
                webhook.filter = definition.filter;
                webhook.headers = definition.headers;
                webhook.max_per_hour = definition.max_per_hour;
                webhook.max_concurrent = definition.max_concurrent;
                webhook.permission_mode = definition.permission_mode;
                webhook.max_consecutive_failures = definition.max_consecutive_failures;
                if !threads_kept {
                    webhook.threads = crate::threads::ThreadMap::default();
                }
                match params.enabled {
                    Some(false) if webhook.enabled => {
                        webhook.disable(StoredDisabled::Disabled {
                            by: requester.actor(),
                        });
                    }
                    Some(true) if !webhook.enabled => {
                        webhook.enabled = true;
                        webhook.disabled_reason = None;
                        webhook.consecutive_failures = 0;
                    }
                    _ => {}
                }
                Ok((webhook.clone(), secret))
            })
            .await?;
        // Enabling it, or a new target, lets what waited go.
        self.spawn_webhook_pump(id);
        Ok(WebhookResult {
            webhook: self.inner.webhooks.wire_of(&webhook),
            secret,
        })
    }

    /// Deletes the webhooks that target any of `deleted`, which are deleted
    /// sessions; deleted sessions also leave every webhook's threads
    /// (section 13).
    pub(crate) fn delete_webhooks_targeting(&self, deleted: &HashSet<SessionId>) {
        if !self.inner.webhooks.webhooks().iter().any(|webhook| {
            webhook
                .target
                .session()
                .is_some_and(|session| deleted.contains(&session))
                || webhook.threads.names_any(deleted)
        }) {
            return;
        }
        let deleted = deleted.clone();
        if let Err(error) = self.webhooks_now(move |_, webhooks| {
            webhooks.retain(|webhook| {
                !webhook
                    .target
                    .session()
                    .is_some_and(|session| deleted.contains(&session))
            });
            for webhook in webhooks.iter_mut() {
                webhook.threads.forget_sessions(&deleted);
            }
            Ok(())
        }) {
            eprintln!("cookie-agent: webhooks of deleted sessions: {error}");
        }
    }

    /// Deletes the new-session webhooks whose template names `project`,
    /// which is being deleted.
    pub(crate) async fn delete_webhooks_of_project(
        &self,
        project: ProjectId,
    ) -> Result<(), EngineError> {
        self.webhooks_blocking(move |_, webhooks| {
            webhooks.retain(|webhook| {
                webhook
                    .target
                    .new_target()
                    .is_none_or(|target| target.template.project.as_ref() != Some(&project))
            });
            Ok(())
        })
        .await
    }

    /// The check a stored webhook makes, for the request side.
    pub(crate) fn webhook_verifier(webhook: &StoredWebhook) -> Option<Verifier> {
        webhook.verify.as_ref().map(StoredVerify::verifier)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_kept_check_keeps_its_secret_and_another_kind_gets_a_new_one() {
        let kept = StoredVerify::Github {
            secret: "old".into(),
        };
        let (same, fresh) = stored_verify(
            WebhookVerify::of_kind(WebhookVerifyKind::Github),
            Some(&kept),
        )
        .unwrap();
        assert_eq!(same.secret(), "old");
        assert!(!fresh);
        let (other, fresh) = stored_verify(
            WebhookVerify::of_kind(WebhookVerifyKind::Bearer),
            Some(&kept),
        )
        .unwrap();
        assert_ne!(other.secret(), "old");
        assert!(fresh);
        let (standard, _) =
            stored_verify(WebhookVerify::of_kind(WebhookVerifyKind::Standard), None).unwrap();
        assert!(standard.secret().starts_with("whsec_"));
    }

    #[test]
    fn hmac_members_belong_to_hmac_only() {
        let mut verify = WebhookVerify::of_kind(WebhookVerifyKind::Github);
        verify.header = Some("X-Signature".into());
        assert!(stored_verify(verify, None).is_err());
        assert!(
            stored_verify(WebhookVerify::of_kind(WebhookVerifyKind::HmacSha256), None).is_err()
        );
        let mut hmac = WebhookVerify::of_kind(WebhookVerifyKind::HmacSha256);
        hmac.header = Some("X-Signature".into());
        let (stored, _) = stored_verify(hmac, None).unwrap();
        assert!(matches!(
            stored,
            StoredVerify::HmacSha256 {
                encoding: WebhookSignatureEncoding::Hex,
                ..
            }
        ));
    }

    #[test]
    fn conditions_name_one_source_and_headers_are_checked() {
        let both = WebhookCondition {
            header: Some("X-Event".into()),
            json: Some("/a".into()),
            one_of: vec!["x".into()],
        };
        assert!(check_filter(&[both]).is_err());
        let neither = WebhookCondition {
            header: None,
            json: None,
            one_of: vec!["x".into()],
        };
        assert!(check_filter(&[neither]).is_err());
        let one = WebhookCondition {
            header: Some("X-Event".into()),
            json: None,
            one_of: vec!["x".into()],
        };
        assert!(check_filter(&[one]).is_ok());
        assert!(check_headers(vec!["Bad Header".into()]).is_err());
        assert_eq!(
            check_headers(vec!["X-A".into(), "x-a".into()]).unwrap(),
            ["X-A"]
        );
        assert!(check_headers(vec!["X".into(); 33]).is_err());
    }
}
