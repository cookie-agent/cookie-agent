//! Exact cookie-agent protocol 15 transport-neutral JSON-RPC service.

mod auth_token;
mod client;
mod providers;
mod rpc;
mod service;
mod transport;
mod websocket_url;

pub use auth_token::{TokenError, load_auth_token};
pub use client::Client;
pub use cookie_agent_protocol::{
    ClientDelivery, ClientError, ClientEventSink, ClientProtocol, MessageFrame, MessageStream,
    ServerContext, ServerFault, ServerProtocol, Transport, TransportError,
};
pub use service::{RunningServer, Server, ServerError};
pub use transport::{InProcessStream, WebSocketTransport, in_process_pair};
pub use websocket_url::{WebSocketUrlError, validate_websocket_url};

#[cfg(test)]
mod tests;
