//! Webhooks (`docs/site/specs/webhooks.md`): the client requests, receiving
//! a request on a webhook's URL, and delivering its events.

pub(crate) mod api;
pub(super) mod deliver;
pub(crate) mod receive;

pub(crate) use deliver::WebhookState;
