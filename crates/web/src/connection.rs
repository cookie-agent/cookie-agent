//! Opening the daemon connection from the page, with the tab's identity.

use cookie_agent_protocol::{ClientIdentity, ClientKind, HandshakeToken, ServerHello};
use cookie_agent_protocol_transport::{Client, ClientError};

use crate::{browser, connect::IDENTITY_KEY};

/// This tab's client identity: the one kept in `sessionStorage`, or a new
/// one kept there. Every reconnect of the tab presents it, so its events
/// keep one origin; another tab is another client.
pub fn tab_identity() -> ClientIdentity {
    if let Some(identity) = browser::load_tab(IDENTITY_KEY)
        .and_then(|stored| serde_json::from_str::<ClientIdentity>(&stored).ok())
    {
        return identity;
    }
    let identity =
        ClientIdentity::generate(ClientKind::new("web").expect("`web` is a valid client kind"));
    if let Ok(stored) = serde_json::to_string(&identity) {
        browser::store_tab(IDENTITY_KEY, &stored);
    }
    identity
}

/// Connects to the daemon at `url` and authenticates as `identity` with
/// `token` in the handshake, as a browser must.
pub async fn open(
    url: &str,
    token: &str,
    identity: ClientIdentity,
) -> Result<(Client, ServerHello), ClientError> {
    let client = connect(url).await?;
    let hello = client
        .handshake_with_token(identity, HandshakeToken::new(token))
        .await?;
    Ok((client, hello))
}

#[cfg(target_arch = "wasm32")]
async fn connect(url: &str) -> Result<Client, ClientError> {
    Client::connect_browser_websocket(url).await
}

#[cfg(not(target_arch = "wasm32"))]
async fn connect(_url: &str) -> Result<Client, ClientError> {
    Err(ClientError::WebSocket(
        "the web client connects only from a browser".into(),
    ))
}
