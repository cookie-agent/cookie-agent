//! The cookie-agent client protocol's connection client ([`Client`]), the
//! daemon's server loop, and the WebSocket transports, over the wire types
//! of `cookie_agent_protocol`.
//!
//! It also builds for `wasm32-unknown-unknown`, as the [`Client`] only: the
//! server and the native WebSocket transport are left out there. With the
//! `websocket` feature a page connects with
//! `Client::connect_browser_websocket` and authenticates with
//! [`Client::handshake_with_token`].

#[cfg(all(feature = "websocket", target_arch = "wasm32"))]
mod browser_websocket;
mod client;
#[cfg(feature = "websocket")]
mod daemon_url;
mod rt;
#[cfg(not(target_arch = "wasm32"))]
mod server;
#[cfg(all(feature = "websocket", not(target_arch = "wasm32")))]
mod websocket;

// The crate's own tests run natively; on wasm32 the browser tests in
// `tests/browser.rs` cover the client.
#[cfg(all(test, not(target_arch = "wasm32")))]
mod corpus_tests;

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
#[cfg(all(feature = "websocket", not(target_arch = "wasm32")))]
pub use websocket::{
    CONNECT_TIMEOUT, DIAL_IN_DEAD_AFTER, DIAL_IN_PING_AFTER, DialInError, WebSocketTransport,
};
