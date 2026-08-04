use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::Arc,
};

use async_trait::async_trait;
use axum::{
    Router,
    extract::{ConnectInfo, State, WebSocketUpgrade},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use futures_util::StreamExt;
use subtle::ConstantTimeEq;
use tokio::net::TcpListener;

use super::{RunningServer, Server, ServerError};
use crate::{
    auth_token::load_or_create_token,
    transport::{MessageFrame, MessageStream, TransportError},
};

struct WebSocketStream {
    socket: axum::extract::ws::WebSocket,
}

#[async_trait]
impl MessageStream for WebSocketStream {
    async fn send(&mut self, frame: MessageFrame) -> Result<(), TransportError> {
        let text = match frame {
            MessageFrame::Text(text) => text,
            MessageFrame::Value(value) => {
                serde_json::to_string(&value).map_err(|_| TransportError::Closed)?
            }
        };
        self.socket
            .send(axum::extract::ws::Message::Text(text.into()))
            .await
            .map_err(TransportError::from)
    }

    async fn recv(&mut self) -> Result<Option<MessageFrame>, TransportError> {
        loop {
            match self.socket.next().await {
                Some(Ok(axum::extract::ws::Message::Text(text))) => {
                    return Ok(Some(MessageFrame::Text(text.to_string())));
                }
                Some(Ok(axum::extract::ws::Message::Close(_))) | None => return Ok(None),
                Some(Ok(_)) => {}
                Some(Err(error)) => return Err(TransportError::from(error)),
            }
        }
    }
}

impl Server {
    pub fn router(self: Arc<Self>) -> Result<Router, ServerError> {
        let token = Arc::new(load_or_create_token(&self.token_path)?);
        Ok(Router::new()
            .route("/ws", get(websocket_upgrade))
            .with_state(WebSocketState {
                server: self,
                token,
            }))
    }

    pub async fn serve(self: Arc<Self>, port: u16) -> Result<RunningServer, ServerError> {
        let router = self.clone().router()?;
        let listener = TcpListener::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port))
            .await
            .map_err(ServerError::Listen)?;
        let address = listener.local_addr().map_err(ServerError::Listen)?;
        let shutdown = self.shutdown.clone();
        let task = tokio::spawn(async move {
            let _ = axum::serve(
                listener,
                router.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .with_graceful_shutdown(shutdown.cancelled_owned())
            .await;
        });
        Ok(RunningServer { address, task })
    }
}

#[derive(Clone)]
struct WebSocketState {
    server: Arc<Server>,
    token: Arc<String>,
}

async fn websocket_upgrade(
    State(state): State<WebSocketState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response {
    if !peer.ip().is_loopback() || headers.contains_key(header::ORIGIN) {
        return StatusCode::FORBIDDEN.into_response();
    }
    if !authorized(&headers, &state.token) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let server = state.server;
    upgrade.on_upgrade(move |socket| async move {
        let _ = server.serve_stream(WebSocketStream { socket }).await;
    })
}

pub(crate) fn authorized(headers: &HeaderMap, expected: &str) -> bool {
    let mut values = headers.get_all(header::AUTHORIZATION).iter();
    let Some(value) = values.next() else {
        return false;
    };
    if values.next().is_some() {
        return false;
    }
    let Ok(value) = value.to_str() else {
        return false;
    };
    let Some(token) = value.strip_prefix("Bearer ") else {
        return false;
    };
    token.len() == expected.len() && bool::from(token.as_bytes().ct_eq(expected.as_bytes()))
}
