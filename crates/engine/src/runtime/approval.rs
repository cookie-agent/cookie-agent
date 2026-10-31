//! Approvals: the client-facing approval API, the approval flow of a
//! gated tool call, the reviewer's conversation, and the projection of
//! approval records from session logs.

pub(super) mod api;
pub(super) mod context;
pub(super) mod flow;
pub(super) mod projection;
