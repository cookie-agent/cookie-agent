//! TUI adapter for the shared protocol client.

pub use cookie_agent_server::{
    Client, ClientDelivery, ClientError, ClientProtocol, load_auth_token as read_daemon_token,
    validate_websocket_url,
};
