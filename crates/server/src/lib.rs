//! Exact cookie-agent protocol 46 transport-neutral JSON-RPC service.

mod providers;
mod rpc;
mod runtimes;
mod service;
mod token;

pub use service::{RunningServer, Server, ServerError, TlsConfig, load_tls};
pub use token::{READY_LINE_PREFIX, TokenError, generate_token, ready_line};

#[cfg(test)]
mod tests;
