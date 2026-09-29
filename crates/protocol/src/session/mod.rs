mod client;
mod server;
mod transport;
#[cfg(feature = "websocket")]
mod websocket;

pub use client::{Client, ClientDelivery, ClientError};
#[cfg(feature = "test-support")]
pub use server::test_server_context;
pub use server::{ServerContext, ServerFault, ServerProtocol, serve};
pub use transport::{
    InProcessStream, MessageFrame, MessageStream, Transport, TransportError, in_process_pair,
};
#[cfg(feature = "websocket")]
pub use websocket::{WebSocketTransport, WebSocketUrlError, validate_websocket_url};
