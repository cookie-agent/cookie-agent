//! Exact cookie-agent protocol 25 transport-neutral JSON-RPC service.

mod providers;
mod rpc;
mod service;
mod token;

pub use service::{RunningServer, Server, ServerError};
pub use token::{READY_LINE_PREFIX, TokenError, generate_token, ready_line};

#[cfg(test)]
mod tests;
