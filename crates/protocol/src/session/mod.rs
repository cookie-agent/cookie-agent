mod client;
mod server;
mod transport;
#[cfg(feature = "websocket")]
mod websocket;

pub use client::{Client, ClientDelivery, ClientError};
#[cfg(feature = "test-support")]
pub use server::test_server_context;
pub use server::{
    AUTHENTICATION_DEADLINE, Authentication, ServerContext, ServerFault, ServerProtocol, serve,
};
pub use transport::{
    InProcessStream, MAX_WEBSOCKET_MESSAGE_BYTES, MessageFrame, MessageStream, Transport,
    TransportError, in_process_pair,
};
#[cfg(feature = "websocket")]
pub use websocket::{
    WebSocketTransport, WebSocketUrlError, runtime_dial_in_url, validate_websocket_url,
};
