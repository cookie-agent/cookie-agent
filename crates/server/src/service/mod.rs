mod page;
mod routes;
mod runtime_notifications;
mod websocket;
pub use page::page_url;
pub use websocket::{TlsConfig, load_tls};

use std::{
    io,
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use cookie_agent_engine::{Engine, status::StatusBoard};
use cookie_agent_protocol::{MessageStream, TransportError, in_process_pair};
use cookie_agent_protocol_transport::{Authentication, Client};
use thiserror::Error;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// Protocol service composed with one coherent engine runtime.
#[derive(Clone)]
pub struct Server {
    pub(crate) engine: Engine,
    pub(crate) shutdown: CancellationToken,
    /// Set when the daemon shuts down to restart: client connections then
    /// close with code 1012 and the reason `restarting`.
    pub(crate) restarting: Arc<AtomicBool>,
    /// Web Push notifications to the user's browsers.
    pub(crate) push: Arc<crate::push::PushService>,
    /// What the server shares with its listener.
    pub(crate) shared: Arc<Shared>,
}

/// A daemon's listener, which serves clients from before the daemon has
/// its [`Server`]: while it starts, clients connect and see its status
/// (`docs/site/specs/daemon-startup.md`), and once [`Self::hand_over`]
/// gives it the server, the same connections are served by it.
#[derive(Clone)]
pub struct DaemonListener {
    pub(crate) shared: Arc<Shared>,
    /// Set once the daemon is ready to serve. The server does not hold it,
    /// so the two do not keep each other alive.
    pub(crate) server: ServerSlot,
}

/// Where a listener finds the server it serves.
pub(crate) type ServerSlot = Arc<std::sync::OnceLock<Arc<Server>>>;

/// What a daemon's listener and its server share.
pub(crate) struct Shared {
    pub(crate) board: StatusBoard,
    pub(crate) shutdown: CancellationToken,
    /// Set when the daemon shuts down to restart: client connections then
    /// close with code 1012 and the reason `restarting`.
    pub(crate) restarting: Arc<AtomicBool>,
    /// Every WebSocket client connection holds a receiver of this channel
    /// until it has closed, so a shutdown can wait for each to send its
    /// close frame ([`RunningServer::wait`]).
    pub(crate) connections: Arc<tokio::sync::watch::Sender<()>>,
}

impl DaemonListener {
    /// A listener reporting `board`, the status the daemon's engine opens
    /// with.
    #[must_use]
    pub fn new(board: StatusBoard) -> Self {
        Self {
            server: Arc::new(std::sync::OnceLock::new()),
            shared: Arc::new(Shared {
                board,
                shutdown: CancellationToken::new(),
                restarting: Arc::new(AtomicBool::new(false)),
                connections: Arc::new(tokio::sync::watch::Sender::new(())),
            }),
        }
    }

    /// The daemon's status.
    #[must_use]
    pub fn board(&self) -> &StatusBoard {
        &self.shared.board
    }

    /// The server for `engine` that this listener hands over to: it shares
    /// the listener's shutdown.
    #[must_use]
    pub fn server(&self, engine: Engine) -> Server {
        Server::with_shared(engine, Arc::clone(&self.shared))
    }

    /// Serves `server` from now on, and says the daemon is ready.
    pub fn hand_over(&self, server: Arc<Server>) {
        let _ = self.server.set(server);
        self.shared.board.ready();
    }

    /// Says startup failed with `reason`, then waits until every client
    /// connected meanwhile has been told and has closed, for at most
    /// `wait`.
    pub async fn fail(&self, reason: &str, wait: Duration) {
        self.shared.board.fail(
            cookie_agent_protocol::errors::DAEMON_START_FAILED
                .error()
                .detail(reason)
                .build(),
        );
        let _ = tokio::time::timeout(wait, self.shared.connections.closed()).await;
        self.shared.shutdown.cancel();
    }

    /// Stops listening.
    pub fn shutdown(&self) {
        self.shared.shutdown.cancel();
    }
}

impl Server {
    /// A server for `engine`. Within a Tokio runtime it starts sending the
    /// engine's push notifications at once, until [`Self::shutdown`].
    #[must_use]
    pub fn new(engine: Engine) -> Self {
        let board = engine.status_board().clone();
        let listener = DaemonListener::new(board);
        Self::with_shared(engine, listener.shared)
    }

    fn with_shared(engine: Engine, shared: Arc<Shared>) -> Self {
        let push = crate::push::PushService::new(engine.clone());
        if tokio::runtime::Handle::try_current().is_ok() {
            push.start(shared.shutdown.clone());
        }
        Self {
            engine,
            shutdown: shared.shutdown.clone(),
            restarting: Arc::clone(&shared.restarting),
            shared,
            push,
        }
    }

    /// Stops serving: the daemon is `stopping`.
    pub fn shutdown(&self) {
        self.engine.status_board().stopping(false);
        self.shutdown.cancel();
    }

    /// Tells the devices with push notifications that the daemon goes down
    /// to restart, but the one that asked; waits a few seconds at most.
    pub async fn announce_restarting(&self) {
        self.push
            .announce_restarting(std::time::Duration::from_secs(5))
            .await;
    }

    /// Tells the devices with push notifications that the persistent daemon
    /// is up: restarted, or started. Sends in the background.
    pub fn announce_started(&self) {
        let push = Arc::clone(&self.push);
        tokio::spawn(async move {
            push.announce_started(std::time::Duration::from_secs(30))
                .await;
        });
    }

    /// Shuts down to restart: as [`Self::shutdown`], but client connections
    /// close saying the daemon is restarting
    /// ([`cookie_agent_protocol::RESTART_CLOSE_CODE`]).
    pub fn shutdown_for_restart(&self) {
        self.engine.status_board().stopping(true);
        self.restarting.store(true, Ordering::Release);
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
        cookie_agent_protocol_transport::serve(self, stream, connection_shutdown, authentication)
            .await
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

/// How long [`RunningServer::wait`] waits for client connections to close
/// once the listener stopped. Each gets the protocol's shutdown grace (one
/// second) to send what it queued and its close frame; a client slower than
/// this loses its close frame rather than holding up a stop or a restart.
const CONNECTIONS_CLOSE_WAIT: Duration = Duration::from_secs(2);

pub struct RunningServer {
    pub(super) address: SocketAddr,
    pub(super) task: JoinHandle<()>,
    pub(super) connections: Arc<tokio::sync::watch::Sender<()>>,
}

impl RunningServer {
    #[must_use]
    pub const fn address(&self) -> SocketAddr {
        self.address
    }

    /// Waits for the listener to stop after a shutdown, then, for up to
    /// `CONNECTIONS_CLOSE_WAIT`, for every client connection to close:
    /// each has then written its close frame, which says why it closed (the
    /// daemon restarting, say), and the client answered it or its grace ran
    /// out. Without this, a daemon exiting or re-executing right after
    /// would cut connections before their close frames left.
    pub async fn wait(self) {
        let _ = self.task.await;
        let _ = tokio::time::timeout(CONNECTIONS_CLOSE_WAIT, self.connections.closed()).await;
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
