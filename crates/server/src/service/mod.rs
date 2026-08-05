mod routes;
mod runtime_notifications;
mod subscriptions;
mod websocket;

use std::{io, net::SocketAddr, path::PathBuf, sync::Arc};

use cookie_agent_engine::Engine;
use cookie_agent_protocol::{ProtocolVersion, ServerHello};
use thiserror::Error;
use tokio::{sync::mpsc, task::JoinHandle};
use tokio_util::sync::CancellationToken;

use crate::{
    auth_token::{TokenError, standard_token_path},
    rpc::{
        Incoming, RouteResult, classify_incoming, error_response, parse_incoming, success_response,
    },
    transport::{MessageFrame, MessageStream, TransportError},
};

const OUTBOUND_QUEUE_CAPACITY: usize = 512;

struct ConnectionShutdown(CancellationToken);

impl Drop for ConnectionShutdown {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

/// Protocol service composed with one coherent engine runtime.
#[derive(Clone)]
pub struct Server {
    pub(crate) engine: Engine,
    pub(crate) shutdown: CancellationToken,
    pub(crate) token_path: PathBuf,
}

impl Server {
    #[must_use]
    pub fn new(engine: Engine) -> Self {
        Self {
            engine,
            shutdown: CancellationToken::new(),
            token_path: standard_token_path().unwrap_or_default(),
        }
    }

    pub fn shutdown(&self) {
        self.shutdown.cancel();
    }

    pub async fn serve_stream<S>(self: Arc<Self>, mut stream: S) -> Result<(), TransportError>
    where
        S: MessageStream,
    {
        let (notifications, mut notification_rx) = mpsc::channel(OUTBOUND_QUEUE_CAPACITY);
        let mut handshaken = false;
        let mut runtime_notifications_started = false;
        let connection_shutdown = self.shutdown.child_token();
        let _guard = ConnectionShutdown(connection_shutdown.clone());
        loop {
            tokio::select! {
                _ = connection_shutdown.cancelled() => return Ok(()),
                incoming = stream.recv() => {
                    let Some(frame) = incoming? else { return Ok(()); };
                    let incoming = match parse_incoming(frame).and_then(classify_incoming) {
                        Ok(incoming) => incoming,
                        Err(error) => {
                            stream.send(MessageFrame::Value(error_response(None, error)?)).await?;
                            continue;
                        }
                    };
                    match incoming {
                        Incoming::Request { id, method, params } => {
                            let result = self.route_after_handshake(
                                &mut handshaken,
                                &method,
                                params,
                                true,
                                notifications.clone(),
                                &connection_shutdown,
                            ).await;
                            let (response, start_runtime_notifications) = match result {
                                Ok(RouteResult::Handshake) => (
                                    success_response(id, &ServerHello { protocol_version: ProtocolVersion::current() })?,
                                    true,
                                ),
                                Ok(RouteResult::Value(value)) => (success_response(id, &value)?, false),
                                Err(error) => (error_response(Some(id), error)?, false),
                            };
                            stream.send(MessageFrame::Value(response)).await?;
                            if start_runtime_notifications && !runtime_notifications_started {
                                runtime_notifications_started = true;
                                self.start_runtime_notifications(
                                    notifications.clone(),
                                    connection_shutdown.child_token(),
                                );
                            }
                        }
                        Incoming::Notification { method, params } => {
                            let _ = self.route_after_handshake(
                                &mut handshaken,
                                &method,
                                params,
                                false,
                                notifications.clone(),
                                &connection_shutdown,
                            ).await;
                        }
                    }
                }
                Some(notification) = notification_rx.recv() => {
                    stream.send(MessageFrame::Value(notification)).await?;
                }
            }
        }
    }
}

pub struct RunningServer {
    pub(super) address: SocketAddr,
    pub(super) task: JoinHandle<()>,
}

impl RunningServer {
    #[must_use]
    pub const fn address(&self) -> SocketAddr {
        self.address
    }

    pub async fn wait(self) {
        let _ = self.task.await;
    }
}

#[derive(Debug, Error)]
pub enum ServerError {
    #[error("could not bind localhost websocket listener: {0}")]
    Listen(#[source] io::Error),
    #[error("could not prepare websocket authentication token")]
    Token(#[from] TokenError),
}
