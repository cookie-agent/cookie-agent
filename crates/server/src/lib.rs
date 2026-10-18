//! The cookie-agent client protocol 0.51 JSON-RPC service, transport-neutral.

mod config;
mod providers;
mod rpc;
mod runtimes;
mod service;
mod token;

pub use service::{AllowedOrigins, RunningServer, Server, ServerError, TlsConfig, load_tls};
pub use token::{READY_LINE_PREFIX, TokenError, generate_token, ready_line};

#[cfg(test)]
mod tests;
