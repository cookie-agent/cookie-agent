//! Messages between sessions and from producers: the mailbox of
//! pending input, agent-to-agent messaging, goal and message producers,
//! their prompt claims, and runtimes and plugins as producer sources.

pub(crate) mod api;
pub(super) mod claims;
pub(super) mod mailbox;
pub(crate) mod producers;
pub(super) mod runtime_producers;
