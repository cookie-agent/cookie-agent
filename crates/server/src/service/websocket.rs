use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::Path,
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use axum::{
    Router,
    extract::{Query, State, WebSocketUpgrade},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use futures_util::StreamExt;
use subtle::ConstantTimeEq;
use tokio::net::{TcpListener, TcpStream};
use zeroize::Zeroizing;

use super::{RunningServer, Server, ServerError};
use cookie_agent_protocol::{
    MAX_WEBSOCKET_MESSAGE_BYTES, MessageFrame, MessageStream, TransportError,
};
use cookie_agent_protocol_transport::Authentication;

struct WebSocketStream {
    socket: axum::extract::ws::WebSocket,
    /// The close frame this end closes with, once its handler has chosen
    /// it; a normal closure when there is none.
    closing: Option<tokio::sync::watch::Receiver<Option<axum::extract::ws::CloseFrame>>>,
    /// Set when the daemon shuts down to restart, which the closing frame
    /// says.
    restarting: Option<Arc<std::sync::atomic::AtomicBool>>,
    /// A client's stream pings once the connection has been quiet this
    /// long.
    ping_after: Option<Duration>,
    /// When a frame last went either way.
    active: tokio::time::Instant,
    /// When the daemon pinged a quiet connection that has sent nothing
    /// since.
    pinged: Option<tokio::time::Instant>,
}

/// How long a closing stream waits for its handler to choose the close
/// frame.
const CLOSE_CHOICE_WAIT: Duration = Duration::from_secs(1);

/// How long a client's connection may be quiet before the daemon pings it.
/// A browser cannot ping, but answers a ping with a pong, so a connection
/// to a quiet session still carries traffic: a proxy in front of the
/// daemon ends an idle one (a Cloudflare tunnel after 100 s), which the
/// page took for a lost daemon and reconnected.
const CLIENT_PING_AFTER: Duration = Duration::from_secs(30);

/// How long a pinged client has to send anything, a pong included, before
/// the daemon takes the connection for half-open and closes it: a client
/// that went away without a close (a laptop that slept, a dropped network)
/// otherwise stays connected for good.
const CLIENT_PONG_WAIT: Duration = Duration::from_secs(30);

/// [`CLIENT_PONG_WAIT`], or in a test what it set in milliseconds.
fn client_pong_wait() -> Duration {
    #[cfg(test)]
    {
        let millis = tests::PONG_WAIT_MS.load(std::sync::atomic::Ordering::Relaxed);
        if millis > 0 {
            return Duration::from_millis(millis);
        }
    }
    CLIENT_PONG_WAIT
}

/// [`CLIENT_PING_AFTER`], or in a test what it set in milliseconds.
fn client_ping_after() -> Duration {
    #[cfg(test)]
    {
        let millis = tests::PING_AFTER_MS.load(std::sync::atomic::Ordering::Relaxed);
        if millis > 0 {
            return Duration::from_millis(millis);
        }
    }
    CLIENT_PING_AFTER
}

impl WebSocketStream {
    /// A client's stream: it closes with a normal closure, or saying the
    /// daemon is restarting once `restarting` is set.
    fn client(
        socket: axum::extract::ws::WebSocket,
        restarting: Arc<std::sync::atomic::AtomicBool>,
    ) -> Self {
        Self {
            socket,
            closing: None,
            restarting: Some(restarting),
            ping_after: Some(client_ping_after()),
            active: tokio::time::Instant::now(),
            pinged: None,
        }
    }

    /// A stream that closes with the frame sent on the returned sender,
    /// for a handler that learns how the connection ends only after the
    /// stream is handed on.
    fn choosing_close(
        socket: axum::extract::ws::WebSocket,
    ) -> (
        Self,
        tokio::sync::watch::Sender<Option<axum::extract::ws::CloseFrame>>,
    ) {
        let (choice, closing) = tokio::sync::watch::channel(None);
        (
            Self {
                socket,
                closing: Some(closing),
                restarting: None,
                ping_after: None,
                active: tokio::time::Instant::now(),
                pinged: None,
            },
            choice,
        )
    }

    /// The frame this end closes with.
    async fn close_frame(&mut self) -> axum::extract::ws::CloseFrame {
        let normal = axum::extract::ws::CloseFrame {
            code: axum::extract::ws::close_code::NORMAL,
            reason: "".into(),
        };
        if self
            .restarting
            .as_ref()
            .is_some_and(|restarting| restarting.load(std::sync::atomic::Ordering::Acquire))
        {
            return axum::extract::ws::CloseFrame {
                code: cookie_agent_protocol::RESTART_CLOSE_CODE,
                reason: cookie_agent_protocol::RESTART_CLOSE_REASON.into(),
            };
        }
        let Some(closing) = &mut self.closing else {
            return normal;
        };
        match tokio::time::timeout(CLOSE_CHOICE_WAIT, closing.wait_for(Option::is_some)).await {
            Ok(Ok(chosen)) => chosen.clone().unwrap_or(normal),
            _ => normal,
        }
    }
}

#[async_trait]
impl MessageStream for WebSocketStream {
    async fn send(&mut self, frame: MessageFrame) -> Result<(), TransportError> {
        self.active = tokio::time::Instant::now();
        self.socket
            .send(axum::extract::ws::Message::Text(frame.into_text()?.into()))
            .await
            .map_err(|error| TransportError::Other(error.to_string()))
    }

    async fn recv(&mut self) -> Result<Option<MessageFrame>, TransportError> {
        loop {
            let message = match self.ping_after {
                Some(ping_after) => {
                    let due = match self.pinged {
                        Some(pinged) => pinged + client_pong_wait(),
                        None => self.active + ping_after,
                    };
                    tokio::select! {
                        message = self.socket.next() => Some(message),
                        () = tokio::time::sleep_until(due) => None,
                    }
                }
                None => Some(self.socket.next().await),
            };
            let now = tokio::time::Instant::now();
            self.active = now;
            let Some(message) = message else {
                if self.pinged.is_some() {
                    return Err(TransportError::Other(format!(
                        "the client did not answer a ping within {}",
                        cookie_agent_protocol::diagnostics::duration(client_pong_wait())
                    )));
                }
                self.socket
                    .send(axum::extract::ws::Message::Ping(Default::default()))
                    .await
                    .map_err(|error| TransportError::Other(error.to_string()))?;
                self.pinged = Some(now);
                continue;
            };
            self.pinged = None;
            match message {
                Some(Ok(axum::extract::ws::Message::Text(text))) => {
                    return Ok(Some(MessageFrame::Bytes(text.into())));
                }
                Some(Ok(axum::extract::ws::Message::Close(frame))) => return closed(frame),
                None => return Ok(None),
                Some(Ok(_)) => {}
                Some(Err(error)) => return Err(TransportError::Other(error.to_string())),
            }
        }
    }

    /// Sends a close frame, a normal closure unless the handler chose
    /// another, and waits for the other end's answer. When the other end
    /// closed first, sending fails and reading on sends the answer the
    /// socket queued.
    async fn close(&mut self) {
        let frame = self.close_frame().await;
        #[cfg(debug_assertions)]
        if let Some(delay) = test_close_delay() {
            tokio::time::sleep(delay).await;
        }
        let _ = self
            .socket
            .send(axum::extract::ws::Message::Close(Some(frame)))
            .await;
        while let Some(Ok(_)) = self.socket.next().await {}
    }
}

/// A delay before every close frame, from `COOKIE_AGENT_TEST_CLOSE_DELAY_MS`
/// in debug builds: a test makes closing slow to prove the daemon waits for
/// its close frames before it exits or restarts.
#[cfg(debug_assertions)]
fn test_close_delay() -> Option<Duration> {
    static DELAY: std::sync::OnceLock<Option<Duration>> = std::sync::OnceLock::new();
    *DELAY.get_or_init(|| {
        std::env::var("COOKIE_AGENT_TEST_CLOSE_DELAY_MS")
            .ok()?
            .parse()
            .ok()
            .map(Duration::from_millis)
    })
}

/// What a close frame the other end sent means: a normal closure (1000),
/// going away (1001), or a frame without a code ends the connection
/// cleanly; any other code ends it with that code and its reason.
fn closed(
    frame: Option<axum::extract::ws::CloseFrame>,
) -> Result<Option<MessageFrame>, TransportError> {
    use axum::extract::ws::close_code;
    match frame {
        None => Ok(None),
        Some(frame) if matches!(frame.code, close_code::NORMAL | close_code::AWAY) => Ok(None),
        Some(frame) if frame.reason.is_empty() => Err(TransportError::Other(format!(
            "the other end closed the connection with code {}",
            frame.code
        ))),
        Some(frame) => Err(TransportError::Other(format!(
            "the other end closed the connection with code {}: {}",
            frame.code,
            frame.reason.as_str()
        ))),
    }
}

/// How long a client gets to finish its TLS handshake.
const TLS_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// The TLS identity a daemon serves `wss` with.
pub type TlsConfig = Arc<rustls::ServerConfig>;

/// The TLS identity a daemon serves `wss` with, from PEM files.
pub fn load_tls(cert: &Path, key: &Path) -> Result<TlsConfig, ServerError> {
    use rustls_pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject as _};
    let failed = |path: &Path, reason: String| ServerError::Tls {
        path: path.to_owned(),
        reason,
    };
    let chain = CertificateDer::pem_file_iter(cert)
        .map_err(|error| failed(cert, error.to_string()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| failed(cert, error.to_string()))?;
    if chain.is_empty() {
        return Err(failed(cert, "holds no certificate".into()));
    }
    let key_der =
        PrivateKeyDer::from_pem_file(key).map_err(|error| failed(key, error.to_string()))?;
    rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(chain, key_der)
        .map(Arc::new)
        .map_err(|error| failed(cert, error.to_string()))
}

impl Server {
    pub fn router(self: Arc<Self>, token: Arc<Zeroizing<String>>) -> Router {
        Router::new()
            .route("/ws", get(websocket_upgrade))
            .route("/runtime", get(runtime_upgrade))
            .fallback(super::page::page)
            .with_state(WebSocketState {
                server: self,
                token,
            })
    }

    /// Serves clients on the loopback `port`.
    pub async fn serve(
        self: Arc<Self>,
        port: u16,
        token: Zeroizing<String>,
    ) -> Result<RunningServer, ServerError> {
        self.serve_on(
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port),
            token,
            None,
        )
        .await
    }

    /// Serves clients (`/ws`) and dial-in runtimes (`/runtime`) on
    /// `address`, over TLS when `tls` is given. Every connection presents
    /// `token`: a runtime in `Authorization`, a client there or in its
    /// handshake. A client may come from any web origin, or none: the
    /// token is the guard.
    pub async fn serve_on(
        self: Arc<Self>,
        address: SocketAddr,
        token: Zeroizing<String>,
        tls: Option<Arc<rustls::ServerConfig>>,
    ) -> Result<RunningServer, ServerError> {
        let router = self.clone().router(Arc::new(token));
        let listener = TcpListener::bind(address)
            .await
            .map_err(ServerError::Listen)?;
        let address = listener.local_addr().map_err(ServerError::Listen)?;
        let shutdown = self.shutdown.clone();
        let task = match tls {
            None => tokio::spawn(async move {
                let _ = axum::serve(listener, router.into_make_service())
                    .with_graceful_shutdown(shutdown.cancelled_owned())
                    .await;
            }),
            Some(tls) => {
                let listener = TlsListener::start(listener, tls, shutdown.clone());
                tokio::spawn(async move {
                    let _ = axum::serve(listener, router.into_make_service())
                        .with_graceful_shutdown(shutdown.cancelled_owned())
                        .await;
                })
            }
        };
        Ok(RunningServer {
            address,
            task,
            connections: Arc::clone(&self.connections),
        })
    }
}

/// Accepts TCP connections and hands them on once their TLS handshake is
/// done; handshakes run concurrently, so a slow client delays no other.
struct TlsListener {
    accepted: tokio::sync::mpsc::Receiver<(tokio_rustls::server::TlsStream<TcpStream>, SocketAddr)>,
    local: SocketAddr,
}

impl TlsListener {
    fn start(
        listener: TcpListener,
        tls: Arc<rustls::ServerConfig>,
        shutdown: tokio_util::sync::CancellationToken,
    ) -> Self {
        let local = listener
            .local_addr()
            .unwrap_or_else(|_| SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0));
        let (sender, accepted) = tokio::sync::mpsc::channel(64);
        let acceptor = tokio_rustls::TlsAcceptor::from(tls);
        tokio::spawn(async move {
            loop {
                let (stream, peer) = tokio::select! {
                    () = shutdown.cancelled() => return,
                    accepted = listener.accept() => match accepted {
                        Ok(accepted) => accepted,
                        Err(_) => {
                            tokio::time::sleep(Duration::from_millis(50)).await;
                            continue;
                        }
                    },
                };
                let acceptor = acceptor.clone();
                let sender = sender.clone();
                tokio::spawn(async move {
                    if let Ok(Ok(stream)) =
                        tokio::time::timeout(TLS_HANDSHAKE_TIMEOUT, acceptor.accept(stream)).await
                    {
                        let _ = sender.send((stream, peer)).await;
                    }
                });
            }
        });
        Self { accepted, local }
    }
}

impl axum::serve::Listener for TlsListener {
    type Io = tokio_rustls::server::TlsStream<TcpStream>;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        match self.accepted.recv().await {
            Some(accepted) => accepted,
            // The acceptor stopped with the daemon; nothing arrives again.
            None => std::future::pending().await,
        }
    }

    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        Ok(self.local)
    }
}

#[derive(Clone)]
struct WebSocketState {
    server: Arc<Server>,
    token: Arc<Zeroizing<String>>,
}

/// Refuses a runtime's request that carries a browser `Origin` or lacks the
/// daemon's token in `Authorization`, saying why in the body. The token is
/// the only gate: the daemon listens where it is told to.
fn refused(headers: &HeaderMap, token: &str) -> Option<Response> {
    if headers.contains_key(header::ORIGIN) {
        return Some(
            (
                StatusCode::FORBIDDEN,
                "a runtime's request must not carry an Origin header",
            )
                .into_response(),
        );
    }
    (!authorized(headers, token)).then(|| {
        (
            StatusCode::UNAUTHORIZED,
            "the daemon token is missing or wrong",
        )
            .into_response()
    })
}

/// The HTTP status a refused dial-in is answered with; see
/// `cookie_agent_protocol::runtime_protocol::dial_in`.
fn dial_in_status(refusal: &cookie_agent_engine::DialInRefusal) -> StatusCode {
    use cookie_agent_engine::DialInRefusal;
    use cookie_agent_protocol::runtime_protocol::dial_in;
    let status = match refusal {
        DialInRefusal::InvalidId(_) => dial_in::STATUS_INVALID_ID,
        DialInRefusal::Reserved(_) | DialInRefusal::Configured(_) => dial_in::STATUS_FORBIDDEN,
        // A claim never fails to initialize: that comes after the upgrade.
        DialInRefusal::Connected(_) | DialInRefusal::Initialize(..) => dial_in::STATUS_CONNECTED,
    };
    StatusCode::from_u16(status).unwrap_or(StatusCode::FORBIDDEN)
}

/// How a client's upgrade request authenticates: with the token in
/// `Authorization`, or, without that header, in its handshake. Its `Origin`,
/// if any, plays no part; a wrong `Authorization` is refused.
fn client_authentication(
    headers: &HeaderMap,
    state: &WebSocketState,
) -> Result<Authentication, StatusCode> {
    if !headers.contains_key(header::AUTHORIZATION) {
        return Ok(Authentication::Token(Arc::clone(&state.token)));
    }
    if authorized(headers, state.token.as_str()) {
        Ok(Authentication::Authenticated)
    } else {
        Err(StatusCode::UNAUTHORIZED)
    }
}

async fn websocket_upgrade(
    State(state): State<WebSocketState>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response {
    let authentication = match client_authentication(&headers, &state) {
        Ok(authentication) => authentication,
        Err(refused) => return refused.into_response(),
    };
    let server = state.server;
    // Held until the connection has closed, from before the upgrade so a
    // shutdown also waits for a connection still upgrading.
    let open = server.connections.subscribe();
    upgrade
        .max_message_size(MAX_WEBSOCKET_MESSAGE_BYTES)
        .max_frame_size(MAX_WEBSOCKET_MESSAGE_BYTES)
        .on_upgrade(move |socket| async move {
            let stream = WebSocketStream::client(socket, Arc::clone(&server.restarting));
            let _ = server.serve_stream(stream, authentication).await;
            drop(open);
        })
}

#[derive(serde::Deserialize)]
struct RuntimeQuery {
    id: String,
}

/// A runtime dialing in as `id`: the daemon claims the ID, then initializes
/// the runtime and makes it the runtime of that ID. An ID it cannot take is
/// refused before the upgrade, with an HTTP status and the reason as the
/// body; a runtime that does not initialize sees the connection close with
/// code 1011 and the reason.
async fn runtime_upgrade(
    State(state): State<WebSocketState>,
    Query(query): Query<RuntimeQuery>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response {
    if let Some(refused) = refused(&headers, state.token.as_str()) {
        return refused;
    }
    let claim = match state.server.engine.claim_dial_in_runtime(&query.id) {
        Ok(claim) => claim,
        Err(refusal) => {
            eprintln!("cookie-agent: refused a runtime dialing in: {refusal}");
            return (dial_in_status(&refusal), refusal.to_string()).into_response();
        }
    };
    upgrade
        .max_message_size(MAX_WEBSOCKET_MESSAGE_BYTES)
        .max_frame_size(MAX_WEBSOCKET_MESSAGE_BYTES)
        .on_upgrade(move |socket| async move {
            let (stream, choice) = WebSocketStream::choosing_close(socket);
            let frame = match claim.accept(Box::new(stream)).await {
                Ok(()) => axum::extract::ws::CloseFrame {
                    code: axum::extract::ws::close_code::NORMAL,
                    reason: "".into(),
                },
                Err(refusal) => {
                    eprintln!("cookie-agent: {refusal}");
                    axum::extract::ws::CloseFrame {
                        code: cookie_agent_protocol::runtime_protocol::dial_in::CLOSE_INITIALIZE_FAILED,
                        reason: close_reason(&refusal.to_string()).into(),
                    }
                }
            };
            let _ = choice.send(Some(frame));
        })
}

/// `reason` cut to fit a close frame, whose reason holds at most 123 bytes.
fn close_reason(reason: &str) -> String {
    const MAX_BYTES: usize = 123;
    if reason.len() <= MAX_BYTES {
        return reason.to_owned();
    }
    let mut end = MAX_BYTES - '…'.len_utf8();
    while !reason.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &reason[..end])
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

#[cfg(test)]
mod tests {
    use axum::http::{HeaderMap, HeaderValue, header};

    use super::authorized;

    /// A test's client ping interval in milliseconds; zero keeps the
    /// daemon's.
    pub(super) static PING_AFTER_MS: std::sync::atomic::AtomicU64 =
        std::sync::atomic::AtomicU64::new(0);

    /// A test's pong wait in milliseconds; zero keeps the daemon's.
    pub(super) static PONG_WAIT_MS: std::sync::atomic::AtomicU64 =
        std::sync::atomic::AtomicU64::new(0);

    fn bearer(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_str(value).expect("authorization header"),
        );
        headers
    }

    #[test]
    fn in_memory_token_is_validated_without_any_file_read() {
        let token = "A".repeat(43);
        assert!(authorized(&bearer(&format!("Bearer {token}")), &token));
        assert!(!authorized(&bearer("Bearer wrong"), &token));
        assert!(!authorized(
            &bearer(&format!("Bearer {}", "A".repeat(42))),
            &token
        ));
        assert!(!authorized(&bearer("Basic abc"), &token));
        assert!(!authorized(&HeaderMap::new(), &token));
    }
}

#[cfg(test)]
mod tls_tests {
    use std::sync::Arc;

    use futures_util::{SinkExt as _, StreamExt as _};
    use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest as _};

    /// A self-signed certificate for `localhost`, as PEM files in a fresh
    /// directory, and its DER for a client to trust.
    fn certificate() -> (tempfile::TempDir, rustls_pki_types::CertificateDer<'static>) {
        let generated = rcgen::generate_simple_self_signed(vec!["localhost".into()])
            .expect("self-signed certificate");
        let directory = tempfile::tempdir().expect("certificate directory");
        std::fs::write(directory.path().join("cert.pem"), generated.cert.pem()).unwrap();
        std::fs::write(
            directory.path().join("key.pem"),
            generated.signing_key.serialize_pem(),
        )
        .unwrap();
        (directory, generated.cert.der().clone())
    }

    #[test]
    fn a_tls_identity_needs_a_certificate_and_its_key() {
        let (directory, _) = certificate();
        let cert = directory.path().join("cert.pem");
        let key = directory.path().join("key.pem");
        super::load_tls(&cert, &key).expect("TLS identity");
        let missing = directory.path().join("missing.pem");
        let error = super::load_tls(&missing, &key).expect_err("no certificate file");
        assert!(error.to_string().contains("missing.pem"), "{error}");
        let error = super::load_tls(&key, &key).expect_err("a key is no certificate");
        assert!(error.to_string().contains("no certificate"), "{error}");
        let error = super::load_tls(&cert, &cert).expect_err("a certificate is no key");
        assert!(error.to_string().contains("key.pem") || error.to_string().contains("cert.pem"));
    }

    /// A `wss` daemon answers a client that trusts its certificate and
    /// presents the token, and refuses one with a wrong token.
    #[tokio::test]
    async fn the_daemon_serves_websocket_clients_over_tls() {
        let (directory, der) = certificate();
        let tls = super::load_tls(
            &directory.path().join("cert.pem"),
            &directory.path().join("key.pem"),
        )
        .unwrap();
        let fixture = crate::tests::harness();
        let server = Arc::clone(&fixture.server);
        let token = crate::generate_token().unwrap();
        let running = server
            .clone()
            .serve_on(
                std::net::SocketAddr::from(([127, 0, 0, 1], 0)),
                token.clone(),
                Some(tls),
            )
            .await
            .unwrap();
        let mut roots = rustls::RootCertStore::empty();
        roots.add(der).unwrap();
        let client = Arc::new(
            rustls::ClientConfig::builder()
                .with_root_certificates(roots)
                .with_no_client_auth(),
        );
        let connect = |token: Option<&str>| {
            let client = Arc::clone(&client);
            let url = format!("wss://localhost:{}/ws", running.address().port());
            let token = token.map(str::to_owned);
            async move {
                let mut request = url.into_client_request().unwrap();
                if let Some(token) = token {
                    request
                        .headers_mut()
                        .insert("authorization", format!("Bearer {token}").parse().unwrap());
                }
                tokio_tungstenite::connect_async_tls_with_config(
                    request,
                    None,
                    false,
                    Some(tokio_tungstenite::Connector::Rustls(client)),
                )
                .await
            }
        };
        let error = connect(Some("wrong")).await.expect_err("wrong token");
        assert!(error.to_string().contains("401"), "{error}");
        let (mut socket, _) = connect(Some(token.as_str())).await.expect("wss client");
        socket
            .send(Message::Text(
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "handshake",
                    "params": {
                        "protocol_version": cookie_agent_protocol::CLIENT_PROTOCOL_VERSION,
                        "client": { "kind": "test", "instance_id": uuid_like() },
                    },
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();
        let reply = socket.next().await.expect("a reply").expect("a frame");
        assert!(reply.to_text().unwrap().contains("\"id\":1"), "{reply}");
        server.shutdown();
        running.wait().await;
        fixture.engine.shutdown().await;
    }

    fn uuid_like() -> &'static str {
        "0192f3a4-5b6c-7d8e-9f00-112233445566"
    }
}

#[cfg(test)]
mod authentication_tests {
    use std::sync::Arc;

    use futures_util::{SinkExt as _, StreamExt as _};
    use serde_json::{Value, json};
    use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest as _};

    type Socket = tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >;

    struct Daemon {
        running: crate::RunningServer,
        token: zeroize::Zeroizing<String>,
        fixture: crate::tests::Harness,
    }

    impl Daemon {
        async fn start() -> Self {
            Self::start_on(0).await
        }

        async fn start_on(port: u16) -> Self {
            let fixture = crate::tests::harness();
            let token = crate::generate_token().unwrap();
            let running = Arc::clone(&fixture.server)
                .serve_on(
                    std::net::SocketAddr::from(([127, 0, 0, 1], port)),
                    token.clone(),
                    None,
                )
                .await
                .unwrap();
            Self {
                running,
                token,
                fixture,
            }
        }

        async fn connect(
            &self,
            path: &str,
            authorization: Option<&str>,
            origin: Option<&str>,
        ) -> Result<Socket, tokio_tungstenite::tungstenite::Error> {
            let url = format!("ws://127.0.0.1:{}{path}", self.running.address().port());
            let mut request = url.into_client_request().unwrap();
            if let Some(token) = authorization {
                request
                    .headers_mut()
                    .insert("authorization", format!("Bearer {token}").parse().unwrap());
            }
            if let Some(origin) = origin {
                request
                    .headers_mut()
                    .insert("origin", origin.parse().unwrap());
            }
            tokio_tungstenite::connect_async(request)
                .await
                .map(|(socket, _)| socket)
        }

        async fn stop(self) {
            self.fixture.server.shutdown();
            self.running.wait().await;
            self.fixture.engine.shutdown().await;
        }
    }

    async fn call(socket: &mut Socket, id: u64, method: &str, params: Value) -> Value {
        socket
            .send(Message::Text(
                json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();
        loop {
            let frame = tokio::time::timeout(std::time::Duration::from_secs(5), socket.next())
                .await
                .expect("a reply in time")
                .expect("a frame")
                .expect("a readable frame");
            let Message::Text(text) = frame else {
                continue;
            };
            let value: Value = serde_json::from_str(&text).unwrap();
            if value["id"] == json!(id) {
                return value;
            }
        }
    }

    fn hello(token: Option<&str>) -> Value {
        let mut hello = json!({
            "protocol_version": cookie_agent_protocol::CLIENT_PROTOCOL_VERSION,
            "client": {"kind": "web", "instance_id": "0192f3a4-5b6c-7d8e-9f00-112233445566"},
        });
        if let Some(token) = token {
            hello["token"] = json!(token);
        }
        hello
    }

    /// The socket's next frame is its close, or it ends.
    async fn closes(socket: &mut Socket) -> bool {
        loop {
            match tokio::time::timeout(std::time::Duration::from_secs(5), socket.next()).await {
                Err(_) => return false,
                Ok(None | Some(Err(_)) | Some(Ok(Message::Close(_)))) => return true,
                Ok(Some(Ok(_))) => {}
            }
        }
    }

    #[tokio::test]
    async fn the_daemon_pings_a_quiet_client_and_a_pong_keeps_it() {
        super::tests::PING_AFTER_MS.store(100, std::sync::atomic::Ordering::Relaxed);
        let daemon = Daemon::start().await;
        let mut socket = daemon
            .connect("/ws", Some(daemon.token.as_str()), None)
            .await
            .expect("upgrade");
        let reply = call(&mut socket, 1, "handshake", hello(None)).await;
        assert!(reply["result"]["origin"].is_string(), "{reply}");
        // Quiet, the connection is pinged again and again; reading answers
        // each ping with a pong, as a browser does.
        let mut pings = 0;
        while pings < 3 {
            let frame = tokio::time::timeout(std::time::Duration::from_secs(5), socket.next())
                .await
                .expect("a frame of the quiet connection")
                .expect("an open connection")
                .expect("a readable frame");
            if matches!(frame, Message::Ping(_)) {
                pings += 1;
            }
        }
        // It still answers.
        let reply = call(&mut socket, 2, "session.list", json!({})).await;
        assert!(reply["result"]["sessions"].is_array(), "{reply}");
        daemon.stop().await;
    }

    /// A client that stops answering pings, as one whose connection went
    /// half-open does, is closed once its pong is overdue.
    #[tokio::test]
    async fn the_daemon_closes_a_client_that_does_not_answer_its_ping() {
        super::tests::PING_AFTER_MS.store(100, std::sync::atomic::Ordering::Relaxed);
        super::tests::PONG_WAIT_MS.store(200, std::sync::atomic::Ordering::Relaxed);
        let daemon = Daemon::start().await;
        let mut socket = daemon
            .connect("/ws", Some(daemon.token.as_str()), None)
            .await
            .expect("upgrade");
        let reply = call(&mut socket, 1, "handshake", hello(None)).await;
        assert!(reply["result"]["origin"].is_string(), "{reply}");
        // Not reading sends no pong.
        tokio::time::sleep(std::time::Duration::from_millis(800)).await;
        assert!(
            closes(&mut socket).await,
            "the daemon closed the connection"
        );
        daemon.stop().await;
    }

    #[tokio::test]
    async fn a_client_authenticates_with_the_header_or_in_its_handshake() {
        let daemon = Daemon::start().await;
        // With the header, the handshake needs no token.
        let mut socket = daemon
            .connect("/ws", Some(daemon.token.as_str()), None)
            .await
            .expect("header-authenticated upgrade");
        let reply = call(&mut socket, 1, "handshake", hello(None)).await;
        assert!(reply["result"]["origin"].is_string(), "{reply}");
        let reply = call(&mut socket, 2, "session.list", json!({})).await;
        assert!(reply["result"]["sessions"].is_array(), "{reply}");

        // Without it, the upgrade succeeds and only `handshake` is answered
        // until the handshake carries the token.
        let mut socket = daemon
            .connect("/ws", None, None)
            .await
            .expect("an upgrade without the header");
        let reply = call(&mut socket, 1, "session.list", json!({})).await;
        assert_eq!(reply["error"]["code"], -32003, "{reply}");
        assert_eq!(reply["error"]["data"]["code"], "unauthenticated");
        let reply = call(
            &mut socket,
            2,
            "handshake",
            hello(Some(daemon.token.as_str())),
        )
        .await;
        assert!(reply["result"]["origin"].is_string(), "{reply}");
        let reply = call(&mut socket, 3, "session.list", json!({})).await;
        assert!(reply["result"]["sessions"].is_array(), "{reply}");

        // A wrong token is answered, then the connection closes.
        let mut socket = daemon.connect("/ws", None, None).await.unwrap();
        let reply = call(&mut socket, 1, "handshake", hello(Some("wrong"))).await;
        assert_eq!(reply["error"]["code"], -32003, "{reply}");
        assert!(closes(&mut socket).await);
        // So does a handshake without one.
        let mut socket = daemon.connect("/ws", None, None).await.unwrap();
        let reply = call(&mut socket, 1, "handshake", hello(None)).await;
        assert_eq!(reply["error"]["code"], -32003, "{reply}");
        assert!(closes(&mut socket).await);

        // A wrong header is refused at the upgrade.
        let error = daemon
            .connect("/ws", Some("wrong"), None)
            .await
            .expect_err("a wrong header");
        assert!(error.to_string().contains("401"), "{error}");
        // Runtimes keep header authentication.
        let error = daemon
            .connect("/runtime?id=devbox", None, None)
            .await
            .expect_err("a runtime without the header");
        assert!(error.to_string().contains("401"), "{error}");
        daemon.stop().await;
    }

    /// `/ws` takes a client from any web origin, which then authenticates
    /// in its handshake like any other; `/runtime` refuses every request
    /// that carries an origin, since a browser is never a runtime.
    #[tokio::test]
    async fn any_web_origin_connects_and_a_runtime_never_carries_one() {
        let daemon = Daemon::start().await;
        for origin in ["https://anything.example", "http://localhost:5173", "null"] {
            let mut socket = daemon
                .connect("/ws", None, Some(origin))
                .await
                .unwrap_or_else(|error| panic!("{origin}: {error}"));
            // The token stays the guard.
            let reply = call(&mut socket, 1, "session.list", json!({})).await;
            assert_eq!(reply["error"]["code"], -32003, "{origin}: {reply}");
            let reply = call(
                &mut socket,
                2,
                "handshake",
                hello(Some(daemon.token.as_str())),
            )
            .await;
            assert!(reply["result"]["origin"].is_string(), "{origin}: {reply}");
            let reply = call(&mut socket, 3, "session.list", json!({})).await;
            assert!(reply["result"]["sessions"].is_array(), "{origin}: {reply}");
        }
        let error = daemon
            .connect(
                "/runtime?id=devbox",
                Some(daemon.token.as_str()),
                Some("http://localhost:5173"),
            )
            .await
            .expect_err("a runtime with an origin");
        assert_eq!(
            refusal(error),
            (
                403,
                "a runtime's request must not carry an Origin header".to_owned()
            )
        );
        daemon.stop().await;
    }

    /// One plain HTTP request to the daemon: its status, headers (names
    /// lowercased), and body.
    async fn http(
        daemon: &Daemon,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
    ) -> (u16, std::collections::BTreeMap<String, String>, Vec<u8>) {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let mut stream =
            tokio::net::TcpStream::connect(("127.0.0.1", daemon.running.address().port()))
                .await
                .unwrap();
        let mut request =
            format!("{method} {path} HTTP/1.1\r\nhost: 127.0.0.1\r\nconnection: close\r\n");
        for (name, value) in headers {
            request.push_str(&format!("{name}: {value}\r\n"));
        }
        request.push_str("\r\n");
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).await.unwrap();
        let split = response
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .expect("a header block");
        let head = String::from_utf8_lossy(&response[..split]).into_owned();
        let mut lines = head.lines();
        let status = lines
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|code| code.parse().ok())
            .expect("a status line");
        let headers = lines
            .filter_map(|line| line.split_once(':'))
            .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_owned()))
            .collect();
        (status, headers, response[split + 4..].to_vec())
    }

    /// The built page's service worker and manifest, for installing it:
    /// the worker beside `index.html` under its own name, revalidated on
    /// every load like it, needing no wider scope, and caching nothing;
    /// the manifest with what Chromium's install criteria ask for.
    async fn the_page_installs_as_an_app(daemon: &Daemon) {
        let (status, headers, body) = http(daemon, "GET", "/sw.js", &[]).await;
        assert_eq!(status, 200);
        assert_eq!(headers["content-type"], "text/javascript; charset=utf-8");
        assert_eq!(headers["cache-control"], "no-cache");
        assert!(!headers.contains_key("service-worker-allowed"));
        assert!(
            headers["content-security-policy"].contains("worker-src 'self';"),
            "{}",
            headers["content-security-policy"]
        );
        let worker = String::from_utf8(body).expect("JavaScript");
        assert!(worker.contains("self.skipWaiting()"), "{worker}");
        assert!(worker.contains("self.clients.claim()"), "{worker}");
        assert!(
            !worker.contains("caches."),
            "the service worker caches nothing"
        );
        assert!(
            worker.lines().last().is_some_and(|line| {
                line.strip_prefix("// build: ")
                    .is_some_and(|build| build.len() == 16 && build != "source")
            }),
            "scripts/build-web.sh stamps the build"
        );
        let (status, headers, body) = http(daemon, "GET", "/manifest.webmanifest", &[]).await;
        assert_eq!(status, 200);
        assert_eq!(headers["content-type"], "application/manifest+json");
        let manifest: serde_json::Value = serde_json::from_slice(&body).expect("a manifest");
        for field in ["name", "short_name", "background_color", "theme_color"] {
            assert!(
                manifest[field]
                    .as_str()
                    .is_some_and(|value| !value.is_empty()),
                "{field}"
            );
        }
        // Relative, so the page installs from any path.
        for field in ["id", "start_url", "scope"] {
            assert_eq!(manifest[field], "./", "{field}");
        }
        assert_eq!(manifest["display"], "standalone");
        assert_ne!(manifest["prefer_related_applications"], true);
        let icons = manifest["icons"].as_array().expect("icons");
        for size in ["192x192", "512x512"] {
            for purpose in ["any", "maskable"] {
                let icon = icons
                    .iter()
                    .find(|icon| icon["sizes"] == size && icon["purpose"] == purpose)
                    .unwrap_or_else(|| panic!("a {size} {purpose} icon"));
                let src = icon["src"].as_str().expect("its path");
                let (status, headers, _) = http(daemon, "GET", &format!("/{src}"), &[]).await;
                assert_eq!(status, 200, "{src}");
                assert_eq!(headers["content-type"], "image/png", "{src}");
            }
        }
        for screenshot in manifest["screenshots"].as_array().into_iter().flatten() {
            let src = screenshot["src"].as_str().expect("its path");
            let (status, headers, _) = http(daemon, "GET", &format!("/{src}"), &[]).await;
            assert_eq!(status, 200, "{src}");
            assert_eq!(headers["content-type"], screenshot["type"], "{src}");
        }
    }

    #[tokio::test]
    async fn the_daemon_serves_the_page_beside_its_endpoints() {
        let daemon = Daemon::start().await;
        let index = cookie_agent_web_assets::get("index.html").expect("an index");
        let (status, headers, body) = http(&daemon, "GET", "/", &[]).await;
        assert_eq!(status, 200);
        assert_eq!(headers["content-type"], "text/html; charset=utf-8");
        assert_eq!(headers["cache-control"], "no-cache");
        assert_eq!(headers["x-content-type-options"], "nosniff");
        assert_eq!(body, index.bytes);
        // Revalidating an unchanged file costs no body.
        let (status, _, body) = http(&daemon, "GET", "/", &[("if-none-match", index.etag)]).await;
        assert_eq!(status, 304);
        assert!(body.is_empty());
        for asset in cookie_agent_web_assets::assets() {
            let (status, headers, body) =
                http(&daemon, "GET", &format!("/{}", asset.path), &[]).await;
            assert_eq!(status, 200, "{}", asset.path);
            assert_eq!(
                headers["content-type"], asset.content_type,
                "{}",
                asset.path
            );
            assert_eq!(
                headers["cache-control"],
                if asset.immutable {
                    "public, max-age=31536000, immutable"
                } else {
                    "no-cache"
                },
                "{}",
                asset.path
            );
            assert_eq!(body, asset.bytes, "{}", asset.path);
            assert_eq!(
                headers["content-security-policy"],
                cookie_agent_web_assets::content_security_policy()
            );
            assert!(!headers.contains_key("content-encoding"), "{}", asset.path);
            // A browser that takes gzip gets the compressed copy, under a
            // tag of its own.
            let Some(gzip) = asset.gzip else {
                continue;
            };
            assert_eq!(headers["vary"], "accept-encoding", "{}", asset.path);
            let (status, headers, body) = http(
                &daemon,
                "GET",
                &format!("/{}", asset.path),
                &[("accept-encoding", "gzip, br")],
            )
            .await;
            assert_eq!(status, 200, "{}", asset.path);
            assert_eq!(headers["content-encoding"], "gzip", "{}", asset.path);
            assert_ne!(headers["etag"], asset.etag, "{}", asset.path);
            assert_eq!(body, gzip, "{}", asset.path);
            let (status, _, _) = http(
                &daemon,
                "GET",
                &format!("/{}", asset.path),
                &[
                    ("accept-encoding", "gzip"),
                    ("if-none-match", &headers["etag"]),
                ],
            )
            .await;
            assert_eq!(status, 304, "{}", asset.path);
        }
        assert!(
            cookie_agent_web_assets::content_security_policy().contains("worker-src 'self';"),
            "the page's service worker is allowed"
        );
        if cookie_agent_web_assets::is_built() {
            the_page_installs_as_an_app(&daemon).await;
        }
        // Any other path, or another method, is not the page.
        for (method, path) in [
            ("GET", "/missing.js"),
            ("GET", "/ws/index.html"),
            ("POST", "/"),
            ("DELETE", "/index.html"),
        ] {
            let (status, _, _) = http(&daemon, method, path, &[]).await;
            assert_eq!(status, 404, "{method} {path}");
        }
        // `/ws` and `/runtime` keep their meaning: a request that is no
        // upgrade is refused there, not answered with the page.
        for path in ["/ws", "/runtime?id=devbox"] {
            let (status, _, body) = http(&daemon, "GET", path, &[]).await;
            assert_ne!(status, 200, "{path}");
            assert_ne!(status, 404, "{path}");
            assert_ne!(body, index.bytes, "{path}");
        }
        daemon.stop().await;
    }

    #[tokio::test]
    async fn the_page_connects_from_its_own_origin() {
        // The page's origin names the daemon's own port, so pick it first.
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let origin = format!("http://127.0.0.1:{port}");
        let daemon = Daemon::start_on(port).await;
        // The page loads without a token, then authenticates in its
        // handshake.
        let (status, _, _) = http(&daemon, "GET", "/", &[("origin", &origin)]).await;
        assert_eq!(status, 200);
        let mut socket = daemon
            .connect("/ws", None, Some(&origin))
            .await
            .expect("the page's own origin");
        let reply = call(
            &mut socket,
            1,
            "handshake",
            hello(Some(daemon.token.as_str())),
        )
        .await;
        assert!(reply["result"]["origin"].is_string(), "{reply}");
        daemon.stop().await;
    }

    /// A close frame's reason holds at most 123 bytes: a longer one is cut
    /// at a character boundary and marked.
    #[test]
    fn a_long_close_reason_is_cut_to_fit() {
        assert_eq!(super::close_reason("short"), "short");
        let fits = "a".repeat(123);
        assert_eq!(super::close_reason(&fits), fits);
        let cut = super::close_reason(&"é".repeat(100));
        assert!(cut.len() <= 123, "{}", cut.len());
        assert!(cut.ends_with('…'), "{cut}");
        assert!(cut.starts_with("éé"), "{cut}");
    }

    /// The status and body of a refused upgrade.
    fn refusal(error: tokio_tungstenite::tungstenite::Error) -> (u16, String) {
        let tokio_tungstenite::tungstenite::Error::Http(response) = error else {
            panic!("an HTTP refusal, not {error}");
        };
        (
            response.status().as_u16(),
            String::from_utf8_lossy(response.body().as_deref().unwrap_or_default()).into_owned(),
        )
    }

    /// A runtime dialing in under an ID the daemon cannot give it is refused
    /// before the upgrade with a status and the reason; one that does not
    /// initialize sees the connection close with code 1011 and the reason,
    /// and the ID is free again.
    #[tokio::test]
    async fn a_refused_dial_in_learns_why() {
        let daemon = Daemon::start().await;
        let token = Some(daemon.token.as_str());
        for (path, status, reason) in [
            (
                "/runtime?id=global",
                403,
                "runtime ID `global` is reserved for the daemon's built-in runtime",
            ),
            (
                "/runtime?id=Dev-Box",
                400,
                "invalid runtime ID `Dev-Box`: it must match [a-z][a-z0-9_]{0,31}",
            ),
        ] {
            let error = daemon
                .connect(path, token, None)
                .await
                .expect_err("refused");
            assert_eq!(refusal(error), (status, reason.to_owned()), "{path}");
        }
        let error = daemon
            .connect("/runtime?id=devbox", Some("wrong"), None)
            .await
            .expect_err("a wrong token");
        assert_eq!(
            refusal(error),
            (401, "the daemon token is missing or wrong".to_owned())
        );

        // A runtime of another major version does not initialize.
        let mut socket = daemon
            .connect("/runtime?id=devbox", token, None)
            .await
            .expect("upgraded");
        let initialize = tokio::time::timeout(std::time::Duration::from_secs(5), socket.next())
            .await
            .expect("initialize in time")
            .expect("a frame")
            .expect("a readable frame");
        let Message::Text(initialize) = initialize else {
            panic!("initialize is text: {initialize:?}");
        };
        let initialize: Value = serde_json::from_str(&initialize).unwrap();
        assert_eq!(initialize["method"], "runtime/initialize");
        // Held by the runtime being accepted.
        let error = daemon
            .connect("/runtime?id=devbox", token, None)
            .await
            .expect_err("being accepted");
        assert_eq!(
            refusal(error),
            (409, "runtime `devbox` is already connected".to_owned())
        );
        socket
            .send(Message::Text(
                json!({
                    "jsonrpc": "2.0",
                    "id": initialize["id"],
                    "result": {"protocol_version": "99.0.0", "capabilities": []},
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();
        let close = loop {
            match tokio::time::timeout(std::time::Duration::from_secs(5), socket.next())
                .await
                .expect("the daemon closes in time")
            {
                Some(Ok(Message::Close(frame))) => break frame.expect("a close frame"),
                Some(Ok(_)) => {}
                other => panic!("a close frame, not {other:?}"),
            }
        };
        assert_eq!(u16::from(close.code), 1011);
        assert!(
            close
                .reason
                .starts_with("runtime `devbox` failed to start: "),
            "{}",
            close.reason
        );
        assert!(close.reason.len() <= 123);
        drop(socket);
        daemon
            .connect("/runtime?id=devbox", token, None)
            .await
            .expect("the ID is free again");
        daemon.stop().await;
    }
}
