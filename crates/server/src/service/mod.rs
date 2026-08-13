mod routes;
mod runtime_notifications;
mod subscriptions;
mod websocket;

use std::{io, net::SocketAddr, path::PathBuf, sync::Arc};

use cookie_agent_engine::Engine;
use cookie_agent_protocol::{MessageStream, TransportError};
use thiserror::Error;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::auth_token::{TokenError, standard_token_path};

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

    pub async fn serve_stream<S>(self: Arc<Self>, stream: S) -> Result<(), TransportError>
    where
        S: MessageStream,
    {
        let connection_shutdown = self.shutdown.child_token();
        cookie_agent_protocol::serve(self, stream, connection_shutdown).await
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
