//! The engine's storage layer: session directories and their event logs,
//! the projections folded from those logs (sessions, goals and producer
//! messages, delegations, usage), the session-log converters, and tree
//! ownership.
//!
//! It knows nothing of runs, runtimes, or the session actors: the engine
//! reaches into it, never the other way round. Where a store operation needs
//! engine knowledge (a lazy tree load's products, the ACP sessions a fork
//! leaves behind) the engine passes it in through a hook.

pub mod bindings;
pub mod blocking;
pub mod delegation_events;
pub mod events;
pub mod goal_projection;
pub mod ownership;
pub mod session;
#[cfg(test)]
#[path = "../../../test-support/model_fixtures.rs"]
mod test_support;
pub mod timing;
pub mod usage;

pub use session::TailOwner;
