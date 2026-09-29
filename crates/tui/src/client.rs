//! TUI adapter for the shared protocol client.

pub use cookie_agent_server::{ClientDelivery, ClientError, validate_websocket_url};

use std::{ops::Deref, sync::Arc};

use cookie_agent_protocol::Transport;
use cookie_agent_server::{Server, WebSocketTransport, in_process_pair};

/// Protocol client used by the TUI.
#[derive(Clone)]
pub struct Client(cookie_agent_protocol::Client);

impl Client {
    pub fn connect_stream<T: Transport + 'static>(transport: T) -> Self {
        Self(cookie_agent_protocol::Client::connect_stream(transport))
    }

    pub fn connect_in_process(server: Arc<Server>) -> Self {
        let (client, service) = in_process_pair(128);
        tokio::spawn(async move {
            let _ = server.serve_stream(service).await;
        });
        Self::connect_stream(client)
    }

    pub async fn connect_websocket_with_token(url: &str, token: &str) -> Result<Self, ClientError> {
        WebSocketTransport::connect_with_token(url, token)
            .await
            .map(Self::connect_stream)
            .map_err(|error| {
                ClientError::WebSocket(cookie_agent_protocol::diagnostics::sanitize(
                    &cookie_agent_protocol::diagnostics::error_chain(&error),
                    4096,
                ))
            })
    }
}

impl Deref for Client {
    type Target = cookie_agent_protocol::Client;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
