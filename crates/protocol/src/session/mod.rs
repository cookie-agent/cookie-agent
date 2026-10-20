#[cfg(all(feature = "websocket", target_arch = "wasm32"))]
mod browser_websocket;
mod client;
#[cfg(feature = "websocket")]
mod daemon_url;
mod rt;
#[cfg(not(target_arch = "wasm32"))]
mod server;
mod transport;
#[cfg(all(feature = "websocket", not(target_arch = "wasm32")))]
mod websocket;

#[cfg(all(feature = "websocket", target_arch = "wasm32"))]
pub use browser_websocket::BrowserWebSocketTransport;
pub use client::{Client, ClientDelivery, ClientError};
#[cfg(feature = "websocket")]
pub use daemon_url::{
    WebSocketUrlError, runtime_dial_in_url, validate_page_websocket_url, validate_websocket_url,
};
#[cfg(all(feature = "test-support", not(target_arch = "wasm32")))]
pub use server::test_server_context;
#[cfg(not(target_arch = "wasm32"))]
pub use server::{
    AUTHENTICATION_DEADLINE, Authentication, ServerContext, ServerFault, ServerProtocol, serve,
};
pub use transport::{
    InProcessStream, MAX_WEBSOCKET_MESSAGE_BYTES, MessageFrame, MessageStream, Transport,
    TransportError, in_process_pair,
};
#[cfg(all(feature = "websocket", not(target_arch = "wasm32")))]
pub use websocket::WebSocketTransport;
