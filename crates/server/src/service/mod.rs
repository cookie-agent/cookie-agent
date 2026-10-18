mod routes;
mod runtime_notifications;
mod websocket;
pub use websocket::{AllowedOrigins, TlsConfig, load_tls};

use std::{io, net::SocketAddr, sync::Arc};

use cookie_agent_engine::Engine;
use cookie_agent_protocol::{
    Authentication, Client, MessageStream, TransportError, in_process_pair,
};
use thiserror::Error;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// Protocol service composed with one coherent engine runtime.
#[derive(Clone)]
pub struct Server {
    pub(crate) engine: Engine,
    pub(crate) shutdown: CancellationToken,
}

impl Server {
    #[must_use]
    pub fn new(engine: Engine) -> Self {
        Self {
            engine,
            shutdown: CancellationToken::new(),
        }
    }

    pub fn shutdown(&self) {
        self.shutdown.cancel();
    }

    /// Serves one client connection that authenticates as `authentication`
    /// says.
    pub async fn serve_stream<S>(
        self: Arc<Self>,
        stream: S,
        authentication: Authentication,
    ) -> Result<(), TransportError>
    where
        S: MessageStream,
    {
        let connection_shutdown = self.shutdown.child_token();
        cookie_agent_protocol::serve(self, stream, connection_shutdown, authentication).await
    }

    /// Serves one in-process connection on a spawned task and returns the
    /// protocol client attached to it. The session ends with the server's
    /// shutdown or when the client is dropped.
    pub fn connect_in_process(self: Arc<Self>) -> Client {
        let (client, service) = in_process_pair(128);
        tokio::spawn(async move {
            let _ = self
                .serve_stream(service, Authentication::Authenticated)
                .await;
        });
        Client::connect_stream(client)
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
    #[error("could not bind websocket listener: {0}")]
    Listen(#[source] io::Error),
    #[error("TLS certificate or key {path}: {reason}")]
    Tls {
        path: std::path::PathBuf,
        reason: String,
    },
}
