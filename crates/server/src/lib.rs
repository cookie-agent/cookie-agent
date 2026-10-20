//! The cookie-agent client protocol 1.4 JSON-RPC service, transport-neutral.

mod config;
mod providers;
mod rpc;
mod runtimes;
mod service;
mod token;

pub use service::{
    AllowedOrigins, RunningServer, Server, ServerError, TlsConfig, load_tls, page_url,
};
pub use token::{READY_LINE_PREFIX, TokenError, generate_token, ready_line};

#[cfg(test)]
mod corpus_tests;
#[cfg(test)]
mod tests;
