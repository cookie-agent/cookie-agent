//! Transport-neutral protocol-8 JSON-RPC service.

mod auth_token;
mod providers;
mod rpc;
mod service;
mod transport;

pub use auth_token::{TokenError, load_auth_token};
pub use service::{RunningServer, Server, ServerError};
pub use transport::{
    InProcessStream, MessageFrame, MessageStream, TransportError, in_process_pair,
};

#[cfg(test)]
mod tests;
