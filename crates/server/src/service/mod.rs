mod routes;
mod subscriptions;
mod websocket;

#[cfg(test)]
pub(crate) use websocket::authorized;

use std::{io, net::SocketAddr, path::PathBuf, sync::Arc};

use cookie_agent_config::LoadedConfiguration;
use cookie_agent_engine::Engine;
use cookie_agent_models::{Catalog, ModelSetManager};
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

/// Protocol service composed with one engine and its atomic model manager.
#[derive(Clone)]
pub struct Server {
    pub(crate) engine: Engine,
    pub(crate) model_manager: Arc<ModelSetManager>,
    pub(crate) catalog: Arc<Catalog>,
    pub(crate) configuration: Arc<LoadedConfiguration>,
    pub(crate) shutdown: CancellationToken,
    pub(crate) token_path: PathBuf,
}

impl Server {
    #[must_use]
    pub fn new(
        engine: Engine,
        model_manager: Arc<ModelSetManager>,
        catalog: Arc<Catalog>,
        configuration: Arc<LoadedConfiguration>,
    ) -> Self {
        Self {
            engine,
            model_manager,
            catalog,
            configuration,
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
                            let response = match result {
                                Ok(RouteResult::Handshake) => success_response(id, &ServerHello { protocol_version: ProtocolVersion::current() })?,
                                Ok(RouteResult::Value(value)) => success_response(id, &value)?,
                                Err(error) => error_response(Some(id), error)?,
                            };
                            stream.send(MessageFrame::Value(response)).await?;
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
