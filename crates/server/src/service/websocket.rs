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
    Authentication, MAX_WEBSOCKET_MESSAGE_BYTES, MessageFrame, MessageStream, TransportError,
};

struct WebSocketStream {
    socket: axum::extract::ws::WebSocket,
}

#[async_trait]
impl MessageStream for WebSocketStream {
    async fn send(&mut self, frame: MessageFrame) -> Result<(), TransportError> {
        self.socket
            .send(axum::extract::ws::Message::Text(frame.into_text()?.into()))
            .await
            .map_err(|error| TransportError::Other(error.to_string()))
    }

    async fn recv(&mut self) -> Result<Option<MessageFrame>, TransportError> {
        loop {
            match self.socket.next().await {
                Some(Ok(axum::extract::ws::Message::Text(text))) => {
                    return Ok(Some(MessageFrame::Bytes(text.into())));
                }
                Some(Ok(axum::extract::ws::Message::Close(_))) | None => return Ok(None),
                Some(Ok(_)) => {}
                Some(Err(error)) => return Err(TransportError::Other(error.to_string())),
            }
        }
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

/// The web origins whose pages may open a client connection
/// (`[server] allowed_origins`). A request without `Origin` is a native
/// client and is never refused for it.
#[derive(Clone, Debug, Default)]
pub enum AllowedOrigins {
    /// No web origin: every request with `Origin` is refused.
    #[default]
    None,
    /// Any web origin; the token is the only protection.
    Any,
    /// Exactly these origins, as browsers send them.
    Only(Arc<std::collections::BTreeSet<String>>),
}

impl AllowedOrigins {
    /// The origins a validated `[server] allowed_origins` names.
    #[must_use]
    pub fn from_config(entries: &[String]) -> Self {
        if entries
            .iter()
            .any(|entry| entry == cookie_agent_config::ANY_ORIGIN)
        {
            Self::Any
        } else if entries.is_empty() {
            Self::None
        } else {
            Self::Only(Arc::new(entries.iter().cloned().collect()))
        }
    }

    fn allows(&self, origin: &header::HeaderValue) -> bool {
        match self {
            Self::None => false,
            Self::Any => true,
            Self::Only(origins) => origin.to_str().is_ok_and(|origin| origins.contains(origin)),
        }
    }
}

impl Server {
    pub fn router(
        self: Arc<Self>,
        token: Arc<Zeroizing<String>>,
        origins: AllowedOrigins,
    ) -> Router {
        Router::new()
            .route("/ws", get(websocket_upgrade))
            .route("/runtime", get(runtime_upgrade))
            .with_state(WebSocketState {
                server: self,
                token,
                origins,
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
            AllowedOrigins::None,
        )
        .await
    }

    /// Serves clients (`/ws`) and dial-in runtimes (`/runtime`) on
    /// `address`, over TLS when `tls` is given. Every connection presents
    /// `token`: a runtime in `Authorization`, a client there or in its
    /// handshake. Web pages of `origins` may open client connections.
    pub async fn serve_on(
        self: Arc<Self>,
        address: SocketAddr,
        token: Zeroizing<String>,
        tls: Option<Arc<rustls::ServerConfig>>,
        origins: AllowedOrigins,
    ) -> Result<RunningServer, ServerError> {
        let router = self.clone().router(Arc::new(token), origins);
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
        Ok(RunningServer { address, task })
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
    origins: AllowedOrigins,
}

/// Refuses a runtime's request that carries a browser `Origin` or lacks the
/// daemon's token in `Authorization`. The token is the only gate: the daemon
/// listens where it is told to.
fn refused(headers: &HeaderMap, token: &str) -> Option<Response> {
    if headers.contains_key(header::ORIGIN) {
        return Some(StatusCode::FORBIDDEN.into_response());
    }
    (!authorized(headers, token)).then(|| StatusCode::UNAUTHORIZED.into_response())
}

/// How a client's upgrade request authenticates: with the token in
/// `Authorization`, or, without that header, in its handshake. A page of an
/// origin `origins` does not allow, and a wrong `Authorization`, are refused.
fn client_authentication(
    headers: &HeaderMap,
    state: &WebSocketState,
) -> Result<Authentication, StatusCode> {
    if let Some(origin) = headers.get(header::ORIGIN)
        && !state.origins.allows(origin)
    {
        return Err(StatusCode::FORBIDDEN);
    }
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
    upgrade
        .max_message_size(MAX_WEBSOCKET_MESSAGE_BYTES)
        .max_frame_size(MAX_WEBSOCKET_MESSAGE_BYTES)
        .on_upgrade(move |socket| async move {
            let _ = server
                .serve_stream(WebSocketStream { socket }, authentication)
                .await;
        })
}

#[derive(serde::Deserialize)]
struct RuntimeQuery {
    id: String,
}

/// A runtime dialing in as `id`: the daemon initializes it and makes it the
/// runtime of that ID. A refused runtime sees its connection close.
async fn runtime_upgrade(
    State(state): State<WebSocketState>,
    Query(query): Query<RuntimeQuery>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response {
    if let Some(refused) = refused(&headers, state.token.as_str()) {
        return refused;
    }
    let server = state.server;
    upgrade
        .max_message_size(MAX_WEBSOCKET_MESSAGE_BYTES)
        .max_frame_size(MAX_WEBSOCKET_MESSAGE_BYTES)
        .on_upgrade(move |socket| async move {
            if let Err(error) = server
                .engine
                .accept_dial_in_runtime(&query.id, Box::new(WebSocketStream { socket }))
                .await
            {
                eprintln!("cookie-agent: {error}");
            }
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

#[cfg(test)]
mod tests {
    use axum::http::{HeaderMap, HeaderValue, header};

    use super::authorized;

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
                super::AllowedOrigins::None,
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

    use super::AllowedOrigins;

    type Socket = tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >;

    struct Daemon {
        running: crate::RunningServer,
        token: zeroize::Zeroizing<String>,
        fixture: crate::tests::Harness,
    }

    impl Daemon {
        async fn start(origins: AllowedOrigins) -> Self {
            let fixture = crate::tests::harness();
            let token = crate::generate_token().unwrap();
            let running = Arc::clone(&fixture.server)
                .serve_on(
                    std::net::SocketAddr::from(([127, 0, 0, 1], 0)),
                    token.clone(),
                    None,
                    origins,
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
    async fn a_client_authenticates_with_the_header_or_in_its_handshake() {
        let daemon = Daemon::start(AllowedOrigins::None).await;
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

    #[tokio::test]
    async fn web_origins_connect_only_when_allowed() {
        let daemon = Daemon::start(AllowedOrigins::None).await;
        let error = daemon
            .connect("/ws", None, Some("http://localhost:5173"))
            .await
            .expect_err("no origin is allowed by default");
        assert!(error.to_string().contains("403"), "{error}");
        daemon.stop().await;

        let daemon = Daemon::start(AllowedOrigins::from_config(&[
            "http://localhost:5173".to_owned()
        ]))
        .await;
        let mut socket = daemon
            .connect("/ws", None, Some("http://localhost:5173"))
            .await
            .expect("an allowed origin");
        let reply = call(
            &mut socket,
            1,
            "handshake",
            hello(Some(daemon.token.as_str())),
        )
        .await;
        assert!(reply["result"]["origin"].is_string(), "{reply}");
        for refused in ["http://localhost:5174", "https://localhost:5173", "null"] {
            let error = daemon
                .connect("/ws", None, Some(refused))
                .await
                .expect_err("an origin the list does not name");
            assert!(error.to_string().contains("403"), "{refused}: {error}");
        }
        // Runtimes are never web pages.
        let error = daemon
            .connect(
                "/runtime?id=devbox",
                Some(daemon.token.as_str()),
                Some("http://localhost:5173"),
            )
            .await
            .expect_err("a runtime with an origin");
        assert!(error.to_string().contains("403"), "{error}");
        daemon.stop().await;

        let daemon = Daemon::start(AllowedOrigins::from_config(&["*".to_owned()])).await;
        for origin in ["https://anything.example", "null"] {
            let mut socket = daemon
                .connect("/ws", None, Some(origin))
                .await
                .expect("any origin");
            // The token stays the only protection.
            let reply = call(&mut socket, 1, "session.list", json!({})).await;
            assert_eq!(reply["error"]["code"], -32003, "{reply}");
        }
        daemon.stop().await;
    }
}
