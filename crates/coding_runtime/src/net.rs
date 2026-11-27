//! The runtime's side of forwards (`docs/site/specs/forwards.md` section
//! 4): TCP and UDP listeners and connections the daemon opens with the
//! `net/*` requests, whose bytes travel as `net/data` and `net/datagram`
//! notifications over the runtime's connection.
//!
//! A TCP connection reads its socket only while less than
//! [`NET_WINDOW_BYTES`] it sent are unacknowledged, and acknowledges what it
//! wrote, so neither side buffers more than a window. A connection begins
//! reading once the daemon has its number: an accepted one after
//! `net/accepted` is sent, a connected one after the reply to `net/connect`.

use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use base64::Engine as _;
use cookie_agent_protocol::{
    peer::Peer,
    runtime_protocol::{
        MAX_NET_DATA_BYTES, NET_EOF, NET_WINDOW_BYTES, NetAcceptedParams, NetAckParams,
        NetCloseParams, NetDataParams, NetDatagramParams, NetProtocol, method,
    },
};
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    net::{TcpListener, TcpStream, UdpSocket},
    sync::{Notify, mpsc},
};
use tokio_util::sync::CancellationToken;

/// How long `net/connect` waits for a TCP connection.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// A `net/*` request that failed, said for the user.
#[derive(Debug)]
pub(crate) struct NetError(pub(crate) String);

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The runtime's listeners and connections; dropped with its connection to
/// the daemon, which closes them all.
pub(crate) struct Net {
    peer: Peer,
    next: AtomicU64,
    listeners: Mutex<HashMap<u64, Listener>>,
    connections: Mutex<HashMap<u64, Connection>>,
    stop: CancellationToken,
}

struct Listener {
    stop: CancellationToken,
    /// A UDP listener's socket, which replies to its peers go out of.
    udp: Option<Arc<UdpSocket>>,
}

enum Connection {
    Tcp {
        writes: mpsc::UnboundedSender<Write>,
        window: Arc<Window>,
        /// Set once the connection's reader may start.
        start: Option<Arc<Notify>>,
        stop: CancellationToken,
    },
    Udp {
        socket: Arc<UdpSocket>,
        stop: CancellationToken,
    },
}

enum Write {
    Data(Vec<u8>),
    Eof,
}

/// What a connection sent and the daemon has not acknowledged.
#[derive(Default)]
struct Window {
    unacked: Mutex<u64>,
    acked: Notify,
}

impl Window {
    async fn wait_room(&self) {
        loop {
            let notified = self.acked.notified();
            if *lock(&self.unacked) < NET_WINDOW_BYTES {
                return;
            }
            notified.await;
        }
    }

    fn sent(&self, bytes: u64) {
        *lock(&self.unacked) += bytes;
    }

    fn acked(&self, bytes: u64) {
        let mut unacked = lock(&self.unacked);
        *unacked = unacked.saturating_sub(bytes);
        drop(unacked);
        self.acked.notify_waiters();
    }
}

impl Drop for Net {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

impl Net {
    pub(crate) fn new(peer: Peer, stop: CancellationToken) -> Arc<Self> {
        Arc::new(Self {
            peer,
            next: AtomicU64::new(1),
            listeners: Mutex::default(),
            connections: Mutex::default(),
            stop: stop.child_token(),
        })
    }

    fn id(&self) -> u64 {
        self.next.fetch_add(1, Ordering::Relaxed)
    }

    /// `net/listen`.
    pub(crate) async fn listen(
        self: &Arc<Self>,
        protocol: NetProtocol,
        bind: &str,
        port: u16,
    ) -> Result<(u64, u16), NetError> {
        let address = (bind, port);
        let failed =
            |error: std::io::Error| NetError(format!("listening on {bind}:{port} failed: {error}"));
        let listener = self.id();
        let stop = self.stop.child_token();
        let bound = match protocol {
            NetProtocol::Tcp => {
                let socket = TcpListener::bind(address).await.map_err(failed)?;
                let port = socket.local_addr().map_err(failed)?.port();
                tokio::spawn(Arc::clone(self).accept(listener, socket, stop.clone()));
                lock(&self.listeners).insert(listener, Listener { stop, udp: None });
                port
            }
            NetProtocol::Udp => {
                let socket = Arc::new(UdpSocket::bind(address).await.map_err(failed)?);
                let port = socket.local_addr().map_err(failed)?.port();
                tokio::spawn(Arc::clone(self).receive_listener(
                    listener,
                    Arc::clone(&socket),
                    stop.clone(),
                ));
                lock(&self.listeners).insert(
                    listener,
                    Listener {
                        stop,
                        udp: Some(socket),
                    },
                );
                port
            }
        };
        Ok((listener, bound))
    }

    /// `net/unlisten`.
    pub(crate) fn unlisten(&self, listener: u64) {
        if let Some(listener) = lock(&self.listeners).remove(&listener) {
            listener.stop.cancel();
        }
    }

    /// `net/connect`. A TCP connection reads its socket once
    /// [`Self::start`] is called, after the daemon has its number.
    pub(crate) async fn connect(
        self: &Arc<Self>,
        protocol: NetProtocol,
        host: &str,
        port: u16,
    ) -> Result<u64, NetError> {
        let failed = |error: std::io::Error| {
            NetError(format!("connecting to {host}:{port} failed: {error}"))
        };
        match protocol {
            NetProtocol::Tcp => {
                let stream =
                    tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect((host, port)))
                        .await
                        .map_err(|_| {
                            NetError(format!(
                                "connecting to {host}:{port} failed: no answer in {} seconds",
                                CONNECT_TIMEOUT.as_secs()
                            ))
                        })?
                        .map_err(failed)?;
                let connection = self.id();
                let start = Arc::new(Notify::new());
                self.tcp(connection, stream, Some(Arc::clone(&start)));
                Ok(connection)
            }
            NetProtocol::Udp => {
                let target = tokio::net::lookup_host((host, port))
                    .await
                    .map_err(failed)?
                    .next()
                    .ok_or_else(|| NetError(format!("{host} has no address")))?;
                let local = if target.is_ipv6() {
                    "[::]:0"
                } else {
                    "0.0.0.0:0"
                };
                let socket = UdpSocket::bind(local).await.map_err(failed)?;
                socket.connect(target).await.map_err(failed)?;
                let socket = Arc::new(socket);
                let connection = self.id();
                let stop = self.stop.child_token();
                lock(&self.connections).insert(
                    connection,
                    Connection::Udp {
                        socket: Arc::clone(&socket),
                        stop: stop.clone(),
                    },
                );
                tokio::spawn(Arc::clone(self).receive_connection(connection, socket, stop));
                Ok(connection)
            }
        }
    }

    /// Lets connection `connection` read its socket.
    pub(crate) fn start(&self, connection: u64) {
        if let Some(Connection::Tcp { start, .. }) = lock(&self.connections).get_mut(&connection)
            && let Some(start) = start.take()
        {
            start.notify_one();
        }
    }

    /// `net/data` from the daemon.
    pub(crate) fn data(&self, params: &NetDataParams) -> Result<(), NetError> {
        let bytes = decode(&params.data)?;
        match lock(&self.connections).get(&params.connection) {
            Some(Connection::Tcp { writes, .. }) => {
                let _ = writes.send(Write::Data(bytes));
                Ok(())
            }
            _ => Err(NetError(format!("no connection {}", params.connection))),
        }
    }

    /// `net/ack` from the daemon.
    pub(crate) fn ack(&self, params: &NetAckParams) {
        if let Some(Connection::Tcp { window, .. }) =
            lock(&self.connections).get(&params.connection)
        {
            window.acked(params.bytes);
        }
    }

    /// `net/close` from the daemon.
    pub(crate) fn close(&self, params: &NetCloseParams) {
        let mut connections = lock(&self.connections);
        if params.reason.as_deref() == Some(NET_EOF) {
            if let Some(Connection::Tcp { writes, .. }) = connections.get(&params.connection) {
                let _ = writes.send(Write::Eof);
            }
            return;
        }
        match connections.remove(&params.connection) {
            Some(Connection::Tcp { stop, .. } | Connection::Udp { stop, .. }) => stop.cancel(),
            None => {}
        }
    }

    /// `net/datagram` from the daemon: out of a UDP listener to its peer, or
    /// of a UDP connection.
    pub(crate) async fn datagram(&self, params: &NetDatagramParams) -> Result<(), NetError> {
        let bytes = decode(&params.data)?;
        let failed =
            |error: std::io::Error| NetError(format!("sending a datagram failed: {error}"));
        if let Some(listener) = params.listener {
            let socket = lock(&self.listeners)
                .get(&listener)
                .and_then(|listener| listener.udp.clone())
                .ok_or_else(|| NetError(format!("no UDP listener {listener}")))?;
            let peer = params
                .peer
                .as_deref()
                .ok_or_else(|| NetError("a listener's datagram names its peer".into()))?;
            let peer: std::net::SocketAddr = peer
                .parse()
                .map_err(|_| NetError(format!("{peer} is not an address")))?;
            socket.send_to(&bytes, peer).await.map_err(failed)?;
            return Ok(());
        }
        let connection = params
            .connection
            .ok_or_else(|| NetError("a datagram names a listener or a connection".into()))?;
        let socket = match lock(&self.connections).get(&connection) {
            Some(Connection::Udp { socket, .. }) => Arc::clone(socket),
            _ => return Err(NetError(format!("no UDP connection {connection}"))),
        };
        socket.send(&bytes).await.map_err(failed)?;
        Ok(())
    }

    async fn accept(self: Arc<Self>, listener: u64, socket: TcpListener, stop: CancellationToken) {
        loop {
            let accepted = tokio::select! {
                () = stop.cancelled() => return,
                accepted = socket.accept() => accepted,
            };
            let Ok((stream, peer)) = accepted else {
                continue;
            };
            let connection = self.id();
            let start = Arc::new(Notify::new());
            self.tcp(connection, stream, Some(Arc::clone(&start)));
            let sent = self
                .peer
                .notify(
                    method::NET_ACCEPTED,
                    &NetAcceptedParams {
                        listener,
                        connection,
                        peer: peer.to_string(),
                    },
                )
                .await;
            if sent.is_err() {
                return;
            }
            self.start(connection);
        }
    }

    /// Registers TCP connection `connection` over `stream`, which reads once
    /// `start` is notified.
    fn tcp(self: &Arc<Self>, connection: u64, stream: TcpStream, start: Option<Arc<Notify>>) {
        let (reader, writer) = stream.into_split();
        let (writes, queued) = mpsc::unbounded_channel();
        let window = Arc::new(Window::default());
        let stop = self.stop.child_token();
        let started = start.clone();
        lock(&self.connections).insert(
            connection,
            Connection::Tcp {
                writes,
                window: Arc::clone(&window),
                start,
                stop: stop.clone(),
            },
        );
        tokio::spawn(Arc::clone(self).write_tcp(connection, writer, queued, stop.clone()));
        tokio::spawn(Arc::clone(self).read_tcp(connection, reader, window, started, stop));
    }

    async fn read_tcp(
        self: Arc<Self>,
        connection: u64,
        mut reader: tokio::net::tcp::OwnedReadHalf,
        window: Arc<Window>,
        start: Option<Arc<Notify>>,
        stop: CancellationToken,
    ) {
        if let Some(start) = start {
            tokio::select! {
                () = stop.cancelled() => return,
                () = start.notified() => {}
            }
        }
        let mut buffer = vec![0u8; MAX_NET_DATA_BYTES];
        let ended = loop {
            tokio::select! {
                () = stop.cancelled() => return,
                () = window.wait_room() => {}
            }
            let read = tokio::select! {
                () = stop.cancelled() => return,
                read = reader.read(&mut buffer) => read,
            };
            match read {
                Ok(0) => break NET_EOF.to_owned(),
                Ok(count) => {
                    window.sent(count as u64);
                    let params = NetDataParams {
                        connection,
                        data: base64::engine::general_purpose::STANDARD.encode(&buffer[..count]),
                    };
                    if self.peer.notify(method::NET_DATA, &params).await.is_err() {
                        return;
                    }
                }
                Err(error) => break error.to_string(),
            }
        };
        let reset = ended != NET_EOF;
        let _ = self
            .peer
            .notify(
                method::NET_CLOSE,
                &NetCloseParams {
                    connection,
                    reason: Some(ended),
                },
            )
            .await;
        if reset {
            self.forget(connection);
        }
    }

    async fn write_tcp(
        self: Arc<Self>,
        connection: u64,
        mut writer: tokio::net::tcp::OwnedWriteHalf,
        mut queued: mpsc::UnboundedReceiver<Write>,
        stop: CancellationToken,
    ) {
        loop {
            let write = tokio::select! {
                () = stop.cancelled() => return,
                write = queued.recv() => write,
            };
            match write {
                Some(Write::Data(bytes)) => {
                    if let Err(error) = writer.write_all(&bytes).await {
                        let _ = self
                            .peer
                            .notify(
                                method::NET_CLOSE,
                                &NetCloseParams {
                                    connection,
                                    reason: Some(error.to_string()),
                                },
                            )
                            .await;
                        self.forget(connection);
                        return;
                    }
                    let _ = self
                        .peer
                        .notify(
                            method::NET_ACK,
                            &NetAckParams {
                                connection,
                                bytes: bytes.len() as u64,
                            },
                        )
                        .await;
                }
                Some(Write::Eof) => {
                    let _ = writer.shutdown().await;
                }
                None => return,
            }
        }
    }

    async fn receive_listener(
        self: Arc<Self>,
        listener: u64,
        socket: Arc<UdpSocket>,
        stop: CancellationToken,
    ) {
        let mut buffer = vec![0u8; MAX_NET_DATA_BYTES];
        loop {
            let received = tokio::select! {
                () = stop.cancelled() => return,
                received = socket.recv_from(&mut buffer) => received,
            };
            let Ok((count, peer)) = received else {
                continue;
            };
            let params = NetDatagramParams {
                listener: Some(listener),
                connection: None,
                peer: Some(peer.to_string()),
                data: base64::engine::general_purpose::STANDARD.encode(&buffer[..count]),
            };
            if self
                .peer
                .notify(method::NET_DATAGRAM, &params)
                .await
                .is_err()
            {
                return;
            }
        }
    }

    async fn receive_connection(
        self: Arc<Self>,
        connection: u64,
        socket: Arc<UdpSocket>,
        stop: CancellationToken,
    ) {
        let mut buffer = vec![0u8; MAX_NET_DATA_BYTES];
        loop {
            let received = tokio::select! {
                () = stop.cancelled() => return,
                received = socket.recv(&mut buffer) => received,
            };
            let Ok(count) = received else {
                continue;
            };
            let params = NetDatagramParams {
                listener: None,
                connection: Some(connection),
                peer: None,
                data: base64::engine::general_purpose::STANDARD.encode(&buffer[..count]),
            };
            if self
                .peer
                .notify(method::NET_DATAGRAM, &params)
                .await
                .is_err()
            {
                return;
            }
        }
    }

    fn forget(&self, connection: u64) {
        if let Some(Connection::Tcp { stop, .. } | Connection::Udp { stop, .. }) =
            lock(&self.connections).remove(&connection)
        {
            stop.cancel();
        }
    }
}

fn decode(data: &str) -> Result<Vec<u8>, NetError> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data)
        .map_err(|error| NetError(format!("the data does not decode: {error}")))?;
    if bytes.len() > MAX_NET_DATA_BYTES {
        return Err(NetError(format!(
            "{} bytes in one message, more than {MAX_NET_DATA_BYTES}",
            bytes.len()
        )));
    }
    Ok(bytes)
}
