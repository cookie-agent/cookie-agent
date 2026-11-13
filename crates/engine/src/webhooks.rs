//! Incoming webhooks (`docs/site/specs/webhooks.md`).
//!
//! The request side, as pure functions over a request the HTTP layer has
//! read: [`request`] (the request, its body, delivery IDs, and values a
//! filter or a thread key reads), [`verify`] (the signature checks of
//! section 6), [`filter`] (section 5.2), [`render`] (the `<webhook_event>`
//! message of section 7.1), and [`ids`] (webhook IDs and URL tokens). They
//! live in the engine because delivery is the engine's; the daemon's HTTP
//! route reaches them through it.

pub mod filter;
pub mod ids;
pub mod render;
pub mod request;
pub mod verify;
