//! Receiving a request on a webhook's URL (`docs/site/specs/webhooks.md`
//! section 5): the checks in order, each with its answer, and storing the
//! event before the `202`. The daemon's HTTP route answers `405` for other
//! methods and reads the body up to [`Engine::webhook_body_limit`] (`413`)
//! after [`Engine::webhook_receives`] (`404`); everything else is here.

use cookie_agent_protocol::WebhookHeader;
use jiff::Timestamp;

use crate::runtime::{Engine, EngineError};
use crate::webhooks::{
    filter,
    render::ALWAYS_HIDDEN_HEADERS,
    request::{WebhookRequest, delivery_id, parse_body, thread_key},
    store::{Noise, StoredEvent, StoredEventOutcome, StoredWebhook, WebhookError, new_event_id},
};

/// How long a sender is asked to wait when the backlog is full.
const BACKLOG_RETRY_AFTER_SECONDS: u64 = 60;

/// The daemon's answer to a webhook request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WebhookReply {
    pub status: u16,
    /// A short JSON object.
    pub body: serde_json::Value,
    /// Seconds for `Retry-After`, with `429` and `503`.
    pub retry_after: Option<u64>,
}

impl WebhookReply {
    fn of(status: u16, body: serde_json::Value) -> Self {
        Self {
            status,
            body,
            retry_after: None,
        }
    }

    /// The answer to a URL that names no webhook, a disabled one, or any
    /// while webhooks are off: the same for every case.
    #[must_use]
    pub fn not_found() -> Self {
        Self::of(404, serde_json::json!({"error": "not_found"}))
    }
}

/// The headers an event keeps: all but the secret ones.
fn kept_headers(request: &WebhookRequest, secret: &[String]) -> Vec<WebhookHeader> {
    request
        .headers
        .iter()
        .filter(|(name, _)| {
            let lower = name.to_ascii_lowercase();
            !ALWAYS_HIDDEN_HEADERS.contains(&lower.as_str())
                && !secret
                    .iter()
                    .any(|header| header.eq_ignore_ascii_case(&lower))
        })
        .map(|(name, value)| WebhookHeader {
            name: name.clone(),
            value: value.clone(),
        })
        .collect()
}

impl Engine {
    /// The most body bytes a webhook request may have
    /// (`[webhooks] max_body_bytes`).
    #[must_use]
    pub fn webhook_body_limit(&self) -> u64 {
        self.inner.config.load().runtime.webhooks.max_body_bytes
    }

    /// Whether `token` names an enabled webhook while webhooks are on.
    #[must_use]
    pub fn webhook_receives(&self, token: &str) -> bool {
        self.receiving_webhook(token).is_some()
    }

    fn receiving_webhook(&self, token: &str) -> Option<StoredWebhook> {
        if !self.inner.config.load().runtime.webhooks.enabled
            || !crate::webhooks::ids::is_url_token(token)
        {
            return None;
        }
        self.inner
            .webhooks
            .by_token(token)
            .filter(|webhook| webhook.enabled)
    }

    /// Receives `request` on the URL of `token` (section 5.1): the check,
    /// duplicates, the filter, the rate, the backlog, then the event is
    /// stored and delivered afterwards.
    pub async fn receive_webhook(&self, token: &str, request: WebhookRequest) -> WebhookReply {
        let Some(webhook) = self.receiving_webhook(token) else {
            return WebhookReply::not_found();
        };
        let store = &self.inner.webhooks;
        let now = Timestamp::now();
        let verifier = Self::webhook_verifier(&webhook);
        if let Some(verifier) = &verifier
            && verifier.verify(&request, now).is_err()
        {
            store.note_noise(&webhook.id, Noise::Rejected, now);
            return WebhookReply::of(401, serde_json::json!({"error": "unauthorized"}));
        }
        let delivery = delivery_id(
            &request,
            verifier
                .as_ref()
                .and_then(|verifier| verifier.delivery_header()),
        );
        if delivery.as_deref().is_some_and(|id| webhook.seen(id)) {
            store.note_noise(&webhook.id, Noise::Duplicate, now);
            return WebhookReply::of(200, serde_json::json!({"duplicate": true}));
        }
        let parsed = parse_body(&request);
        if !filter::matches(&webhook.conditions(), &request, &parsed) {
            store.note_noise(&webhook.id, Noise::Filtered, now);
            return WebhookReply::of(204, serde_json::json!({}));
        }
        let config = self.inner.config.load().runtime.webhooks;
        let rate = self.webhook_rate_limit(&webhook);
        if let Some(limit) = rate
            && webhook.recent_count(now) >= limit
        {
            store.note_noise(&webhook.id, Noise::RateLimited, now);
            let oldest = webhook
                .recent
                .iter()
                .copied()
                .filter(|at| now.duration_since(*at) < jiff::SignedDuration::from_hours(1))
                .min()
                .unwrap_or(now);
            let wait = jiff::SignedDuration::from_hours(1) - now.duration_since(oldest);
            return WebhookReply {
                retry_after: Some(u64::try_from(wait.as_secs().max(1)).unwrap_or(1)),
                ..WebhookReply::of(429, serde_json::json!({"error": "rate_limited"}))
            };
        }
        if webhook.waiting_count() >= config.backlog {
            store.note_noise(&webhook.id, Noise::BacklogFull, now);
            return WebhookReply {
                retry_after: Some(BACKLOG_RETRY_AFTER_SECONDS),
                ..WebhookReply::of(503, serde_json::json!({"error": "backlog_full"}))
            };
        }
        match self.store_webhook_event(&webhook, request, false).await {
            Ok(event) => {
                self.spawn_webhook_pump(webhook.id.clone());
                WebhookReply::of(202, serde_json::json!({"event": event}))
            }
            Err(EngineError::Webhook(WebhookError::Invalid { .. })) => {
                // Another request with the same delivery ID, or over the
                // rate, got in first.
                store.note_noise(&webhook.id, Noise::Duplicate, now);
                WebhookReply::of(200, serde_json::json!({"duplicate": true}))
            }
            Err(error) => {
                eprintln!("cookie-agent: webhook {}: {error}", webhook.id);
                WebhookReply::of(500, serde_json::json!({"error": "not_stored"}))
            }
        }
    }

    /// The events per hour `webhook` takes; `None` for no limit.
    fn webhook_rate_limit(&self, webhook: &StoredWebhook) -> Option<usize> {
        let limit = match webhook.max_per_hour {
            Some(limit) => u64::try_from(limit).ok().filter(|limit| *limit > 0),
            None => self.inner.config.load().runtime.webhooks.per_hour_limit(),
        };
        limit.map(|limit| usize::try_from(limit).unwrap_or(usize::MAX))
    }

    /// Stores an event of `webhook` for `request`: its body, then its entry
    /// as pending (section 12). A test event skips the duplicate and rate
    /// checks and counts toward no rate. The event's ID.
    pub(crate) async fn store_webhook_event(
        &self,
        webhook: &StoredWebhook,
        request: WebhookRequest,
        test: bool,
    ) -> Result<String, EngineError> {
        let verifier = Self::webhook_verifier(webhook);
        let delivery = if test {
            None
        } else {
            delivery_id(
                &request,
                verifier
                    .as_ref()
                    .and_then(|verifier| verifier.delivery_header()),
            )
        };
        let thread = webhook
            .thread_source()
            .and_then(|source| thread_key(&request, &parse_body(&request), &source));
        let secret = verifier
            .as_ref()
            .map(crate::webhooks::verify::Verifier::secret_headers)
            .unwrap_or_default();
        let event_id = new_event_id(webhook)?;
        let now = Timestamp::now();
        let event = StoredEvent {
            id: event_id.clone(),
            received_at: now,
            test,
            delivery_id: delivery.clone(),
            thread_key: thread,
            bytes: request.body.len() as u64,
            content_type: request.header("content-type").map(str::to_owned),
            method: request.method.clone(),
            headers: kept_headers(&request, &secret),
            outcome: StoredEventOutcome::Pending,
            session_id: None,
            message_id: None,
            run_id: None,
            run_end: None,
        };
        let rate = self.webhook_rate_limit(webhook);
        let history = self.inner.config.load().runtime.webhooks.history;
        let store = std::sync::Arc::clone(&self.inner.webhooks);
        let id = webhook.id.clone();
        let body = request.body;
        let written_event = event_id.clone();
        let result = tokio::task::spawn_blocking(move || {
            store.write_body(&id, &written_event, &body)?;
            let stored = store.mutate(|webhooks| {
                let webhook = webhooks
                    .iter_mut()
                    .find(|webhook| webhook.id == id)
                    .ok_or_else(|| WebhookError::NotFound(id.clone()))?;
                if !test {
                    // Checked again under the lock: a request racing this
                    // one may have taken the ID or the last slot.
                    if delivery.as_deref().is_some_and(|id| webhook.seen(id))
                        || rate.is_some_and(|limit| webhook.recent_count(now) >= limit)
                    {
                        return Err(WebhookError::invalid("event", "taken by another request"));
                    }
                    webhook.note_accepted(delivery.as_deref(), now, rate.is_some());
                }
                webhook.push_event(event, history);
                Ok(())
            });
            if stored.is_err() {
                store.remove_body(&id, &written_event);
            }
            stored
        })
        .await
        .map_err(|error| WebhookError::Store(error.to_string()))?;
        result?;
        Ok(event_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kept_headers_leave_out_secret_ones() {
        let request = WebhookRequest {
            method: "POST".into(),
            headers: vec![
                ("Authorization".into(), "Bearer x".into()),
                ("X-Hub-Signature-256".into(), "sha256=00".into()),
                ("X-Custom-Signature".into(), "abc".into()),
                ("X-GitHub-Event".into(), "issues".into()),
            ],
            body: Vec::new(),
        };
        let kept = kept_headers(&request, &["x-custom-signature".to_owned()]);
        let names = kept
            .iter()
            .map(|header| header.name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(names, ["X-GitHub-Event"]);
    }
}
