//! Threads (`docs/site/specs/webhooks.md` sections 7.4, 7.5, and 8): the
//! sessions a cron job (and, later, a webhook) sends its fires into by key,
//! and the origin of the thread prompt each such session gets.
//!
//! A thread prompt is a plain user message the engine submits on its
//! owner's behalf. Its event origin, `engine:cron-<job id>` or
//! `engine:webhook-<webhook id>` (client protocol 1.96.0), says which owner
//! sent it, for clients to label it; the model sees only the text.
//! Client protocol 1.95.0.

use crate::{CronJobId, EventOrigin, WebhookId};

const CRON_PREFIX: &str = "engine:cron-";
const WEBHOOK_PREFIX: &str = "engine:webhook-";

/// What a thread belongs to.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum ThreadOwnerRef {
    /// A cron job with a new-session target.
    Cron(CronJobId),
    /// A webhook with a new-session target (client protocol 1.96.0).
    Webhook(WebhookId),
}

/// The event origin of the thread prompts `owner` sends.
#[must_use]
pub fn thread_prompt_origin(owner: &ThreadOwnerRef) -> EventOrigin {
    match owner {
        ThreadOwnerRef::Cron(job) => EventOrigin::new(format!("{CRON_PREFIX}{job}"))
            .expect("a cron job ID forms a valid origin slug"),
        ThreadOwnerRef::Webhook(webhook) => EventOrigin::new(format!("{WEBHOOK_PREFIX}{webhook}"))
            .expect("a webhook ID forms a valid origin slug"),
    }
}

/// The owner whose thread prompt `origin` marks; `None` for any other
/// origin.
#[must_use]
pub fn thread_prompt_owner(origin: &EventOrigin) -> Option<ThreadOwnerRef> {
    if let Some(webhook) = origin.as_str().strip_prefix(WEBHOOK_PREFIX) {
        return WebhookId::new(webhook.to_owned())
            .ok()
            .map(ThreadOwnerRef::Webhook);
    }
    let job = origin.as_str().strip_prefix(CRON_PREFIX)?;
    CronJobId::new(job.to_owned())
        .ok()
        .map(ThreadOwnerRef::Cron)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cron_owner_round_trips_through_its_origin() {
        let owner = ThreadOwnerRef::Cron(CronJobId::new("k3v9q2xa".to_owned()).unwrap());
        let origin = thread_prompt_origin(&owner);
        assert_eq!(thread_prompt_owner(&origin), Some(owner));
    }

    #[test]
    fn a_webhook_owner_round_trips_through_its_origin() {
        let owner = ThreadOwnerRef::Webhook(WebhookId::new("w7k2m9qa".to_owned()).unwrap());
        let origin = thread_prompt_origin(&owner);
        assert_eq!(thread_prompt_owner(&origin), Some(owner));
    }

    #[test]
    fn other_origins_name_no_owner() {
        for origin in [
            "user",
            "engine:cron",
            "engine:producer",
            "engine:cron-short",
            "engine:webhook-short",
            "client:web",
        ] {
            assert_eq!(
                thread_prompt_owner(&EventOrigin::new(origin).unwrap()),
                None,
                "{origin}"
            );
        }
    }
}
