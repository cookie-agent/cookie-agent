use std::{ops::Deref, sync::Arc};

use cookie_agent_protocol::{ClientError, Transport};

use crate::{Server, WebSocketTransport, in_process_pair};

/// Protocol client connected through transports provided by this crate.
#[derive(Clone)]
pub struct Client(cookie_agent_protocol::Client);

impl Client {
    /// Start the shared protocol client over an existing transport.
    pub fn connect_stream<T>(transport: T) -> Self
    where
        T: Transport + 'static,
    {
        Self(cookie_agent_protocol::Client::connect_stream(transport))
    }

    /// Pair an in-process client transport with a real server session.
    pub fn connect_in_process(server: Arc<Server>) -> Self {
        let (client, service) = in_process_pair(128);
        tokio::spawn(async move {
            let _ = server.serve_stream(service).await;
        });
        Self::connect_stream(client)
    }

    /// Connect to the daemon using its standard authentication token.
    pub async fn connect_websocket(url: &str) -> Result<Self, ClientError> {
        WebSocketTransport::connect(url)
            .await
            .map(Self::connect_stream)
            .map_err(|error| ClientError::WebSocket(error.to_string()))
    }

    /// Connect using an explicit daemon authentication token.
    pub async fn connect_websocket_with_token(url: &str, token: &str) -> Result<Self, ClientError> {
        WebSocketTransport::connect_with_token(url, token)
            .await
            .map(Self::connect_stream)
            .map_err(|error| ClientError::WebSocket(error.to_string()))
    }
}

impl Deref for Client {
    type Target = cookie_agent_protocol::Client;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
