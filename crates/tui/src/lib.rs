//! Ratatui frontend for Cookie Agent's versioned JSON-RPC protocol.

pub mod config;
pub mod markdown;
pub mod state;
pub mod terminal_detect;
pub mod theme;
pub mod ui;

pub use cookie_agent_protocol::{Client, ClientDelivery, ClientError};
pub use ui::{
    ConnectionView, Reconnector, SessionPlacement, run_attached, run_with_client,
    run_with_new_session,
};

#[cfg(test)]
#[path = "../../../test-support/config_harness.rs"]
mod config_harness;
#[cfg(test)]
#[path = "../../../test-support/eventually.rs"]
mod eventually;

#[cfg(test)]
mod tests;
