//! Exact cookie-agent protocol 20 transport-neutral JSON-RPC service.

mod client;
mod providers;
mod rpc;
mod service;
mod token;
mod transport;
mod websocket_url;

pub use client::Client;
pub use cookie_agent_protocol::{
    ClientDelivery, ClientError, ClientEventSink, ClientProtocol, MessageFrame, MessageStream,
    ServerContext, ServerFault, ServerProtocol, Transport, TransportError,
};
pub use service::{RunningServer, Server, ServerError};
pub use token::{READY_LINE_PREFIX, TokenError, generate_token, ready_line};
pub use transport::{InProcessStream, WebSocketTransport, in_process_pair};
pub use websocket_url::{WebSocketUrlError, validate_websocket_url};

#[cfg(test)]
mod tests;
