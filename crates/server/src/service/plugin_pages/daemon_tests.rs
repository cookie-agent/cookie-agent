//! Plugin pages through a running daemon: a fixture plugin declares pages
//! served by a recording upstream, and requests reach it through the
//! daemon's listener.

use std::{collections::BTreeMap, sync::Arc, time::Duration};

use cookie_agent_config::PluginConfig;
use cookie_agent_protocol::PluginState;
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    net::{TcpListener, TcpStream},
};

use super::{TICKET_COOKIE, page_ticket};

const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../engine/tests/fixtures/fake_plugin.py"
);
#[cfg(unix)]
const PYTHON: &str = "python3";
#[cfg(windows)]
const PYTHON: &str = "python";

/// A liveness bound on the Python fixture, not a latency assertion.
const WAIT: Duration = Duration::from_secs(30);

/// An HTTP server that answers every request with the request it got (its
/// head and body) as a `text/plain` body, and joins a WebSocket upgrade to
/// an echo of the bytes that follow.
async fn upstream() -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(answer(stream));
        }
    });
    port
}

async fn answer(mut stream: TcpStream) {
    let mut received = Vec::new();
    let mut buffer = [0_u8; 4096];
    let head_end = loop {
        let Ok(read) = stream.read(&mut buffer).await else {
            return;
        };
        if read == 0 {
            return;
        }
        received.extend_from_slice(&buffer[..read]);
        if let Some(end) = received.windows(4).position(|window| window == b"\r\n\r\n") {
            break end + 4;
        }
    };
    let head = String::from_utf8_lossy(&received[..head_end]).to_ascii_lowercase();
    if head.contains("\r\nupgrade: websocket") {
        let _ = stream
            .write_all(
                b"HTTP/1.1 101 Switching Protocols\r\nupgrade: websocket\r\nconnection: upgrade\r\n\r\n",
            )
            .await;
        let (mut reader, mut writer) = stream.into_split();
        let _ = tokio::io::copy(&mut reader, &mut writer).await;
        return;
    }
    let length = head
        .lines()
        .find_map(|line| line.strip_prefix("content-length:"))
        .and_then(|value| value.trim().parse::<usize>().ok())
        .unwrap_or(0);
    while received.len() < head_end + length {
        let Ok(read) = stream.read(&mut buffer).await else {
            return;
        };
        if read == 0 {
            break;
        }
        received.extend_from_slice(&buffer[..read]);
    }
    let response = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\nx-frame-options: SAMEORIGIN\r\n\
         set-cookie: own=1; Path=/plugins/pages/\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        received.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.write_all(&received).await;
}

fn plugin(name: &str, ui: Option<String>) -> PluginConfig {
    let mut env = BTreeMap::from([
        ("FIXTURE_NAME".to_owned(), name.to_owned()),
        ("FIXTURE_TOOLS".to_owned(), "[]".to_owned()),
    ]);
    if let Some(ui) = ui {
        env.insert("FIXTURE_RESULT_EXTRA".to_owned(), ui);
    }
    PluginConfig {
        command: vec![PYTHON.into(), FIXTURE.into()],
        env,
        cwd: None,
        enabled: true,
        producer_messaging: false,
        interception_timeout_ms: 2_000,
        startup_timeout_ms: 10_000,
        shutdown_grace_ms: 3_000,
        always_load: false,
    }
}

struct Daemon {
    running: crate::RunningServer,
    token: zeroize::Zeroizing<String>,
    fixture: crate::tests::Harness,
}

impl Daemon {
    /// A daemon with plugin `pages`, whose pages are `upstream` with the
    /// public path `/oauth/callback`, plugin `plain` without pages, and
    /// plugin `broken`, which never starts.
    async fn start(upstream: u16) -> Self {
        let ui = format!(
            r#"{{"ui":{{"url":"http://127.0.0.1:{upstream}","public_paths":["/oauth/callback"]}}}}"#
        );
        let fixture = crate::tests::harness_configured(|config| {
            config
                .plugins
                .insert("pages".into(), plugin("pages", Some(ui)));
            config.plugins.insert("plain".into(), plugin("plain", None));
            let mut broken = plugin("broken", None);
            broken.command = vec![PYTHON.into(), "-c".into(), "raise SystemExit(3)".into()];
            config.plugins.insert("broken".into(), broken);
        });
        let token = crate::generate_token().unwrap();
        let running = Arc::clone(&fixture.server)
            .serve_on(
                std::net::SocketAddr::from(([127, 0, 0, 1], 0)),
                token.clone(),
                None,
            )
            .await
            .unwrap();
        let daemon = Self {
            running,
            token,
            fixture,
        };
        tokio::time::timeout(WAIT, async {
            loop {
                let statuses = daemon.fixture.engine.plugin_statuses();
                let state = |name: &str| {
                    statuses
                        .iter()
                        .find(|plugin| plugin.name == name)
                        .map(|plugin| plugin.state)
                };
                if state("pages") == Some(PluginState::Connected)
                    && state("plain") == Some(PluginState::Connected)
                    && state("broken") != Some(PluginState::Starting)
                {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the plugins start");
        daemon
    }

    fn cookie(&self) -> String {
        format!("theme=dark; {TICKET_COOKIE}={}", page_ticket(&self.token))
    }

    async fn stop(self) {
        self.fixture.server.shutdown();
        self.running.wait().await;
        self.fixture.engine.shutdown().await;
    }

    /// Sends `request` (its head without the final blank line) and reads
    /// the answer to the end.
    async fn send(&self, head: &str, body: &str) -> Answer {
        let mut stream = TcpStream::connect(("127.0.0.1", self.running.address().port()))
            .await
            .unwrap();
        let request = format!("{head}\r\nconnection: close\r\n\r\n{body}");
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut response = Vec::new();
        tokio::time::timeout(WAIT, stream.read_to_end(&mut response))
            .await
            .expect("an answer")
            .unwrap();
        Answer::parse(&response)
    }

    async fn get(&self, path: &str, headers: &[(&str, &str)]) -> Answer {
        let mut head = format!("GET {path} HTTP/1.1\r\nhost: 127.0.0.1");
        for (name, value) in headers {
            head.push_str(&format!("\r\n{name}: {value}"));
        }
        self.send(&head, "").await
    }
}

struct Answer {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
}

impl Answer {
    fn parse(response: &[u8]) -> Self {
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
        Self {
            status,
            headers,
            body: String::from_utf8_lossy(&response[split + 4..]).into_owned(),
        }
    }

    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(header, _)| header == name)
            .map(|(_, value)| value.as_str())
    }

    /// The request line and headers the upstream received, lowercased.
    fn forwarded(&self) -> (String, Vec<String>) {
        assert_eq!(self.status, 200, "{}", self.body);
        let mut lines = self.body.split("\r\n");
        let request_line = lines.next().unwrap().to_owned();
        let headers = lines
            .take_while(|line| !line.is_empty())
            .map(str::to_ascii_lowercase)
            .collect();
        (request_line, headers)
    }
}

#[tokio::test]
async fn without_access_only_public_paths_are_forwarded_and_nothing_tells_plugins_apart() {
    let daemon = Daemon::start(upstream().await).await;
    for path in [
        "/plugins/pages/",
        "/plugins/pages/settings",
        "/plugins/pages",
        "/plugins/plain/",
        "/plugins/broken/",
        "/plugins/unknown/",
        "/plugins/pages/oauth/callbackx",
        "/plugins/pages/oauth/callback/../../settings",
        "/plugins/pages/oauth/callback/%2e%2e/%2e%2e/settings",
    ] {
        let answer = daemon.get(path, &[]).await;
        assert_eq!(answer.status, 401, "{path}");
    }
    let wrong = daemon
        .get(
            "/plugins/pages/",
            &[("cookie", &format!("{TICKET_COOKIE}=00"))],
        )
        .await;
    assert_eq!(wrong.status, 401);
    let callback = daemon
        .get("/plugins/pages/oauth/callback?code=1&state=2", &[])
        .await;
    let (request_line, _) = callback.forwarded();
    assert_eq!(request_line, "GET /oauth/callback?code=1&state=2 HTTP/1.1");
    daemon.stop().await;
}

#[tokio::test]
async fn with_access_requests_reach_the_plugin_below_its_root() {
    let port = upstream().await;
    let daemon = Daemon::start(port).await;
    let cookie = daemon.cookie();
    let answer = daemon
        .get(
            "/plugins/pages/settings?tab=keys",
            &[
                ("cookie", &cookie),
                ("x-forwarded-for", "203.0.113.7"),
                ("x-forwarded-prefix", "/spoofed"),
            ],
        )
        .await;
    let (request_line, headers) = answer.forwarded();
    assert_eq!(request_line, "GET /settings?tab=keys HTTP/1.1");
    let has = |line: &str| headers.iter().any(|header| header == line);
    assert!(has(&format!("host: 127.0.0.1:{port}")), "{headers:?}");
    assert!(has("cookie: theme=dark"), "{headers:?}");
    // Without `page_url`, a front's prefix is kept in front of the page's.
    assert!(
        has("x-forwarded-prefix: /spoofed/plugins/pages"),
        "{headers:?}"
    );
    assert!(has("x-forwarded-proto: http"), "{headers:?}");
    assert!(has("x-forwarded-host: 127.0.0.1"), "{headers:?}");
    assert!(
        has("x-forwarded-for: 203.0.113.7, 127.0.0.1"),
        "{headers:?}"
    );
    assert!(
        !headers.iter().any(|header| header.contains(TICKET_COOKIE)),
        "{headers:?}"
    );
    // The plugin's own response headers pass through.
    assert_eq!(answer.header("x-frame-options"), Some("SAMEORIGIN"));
    assert_eq!(
        answer.header("set-cookie"),
        Some("own=1; Path=/plugins/pages/")
    );

    let bearer = format!("Bearer {}", daemon.token.as_str());
    let answer = daemon
        .get("/plugins/pages/", &[("authorization", &bearer)])
        .await;
    let (request_line, headers) = answer.forwarded();
    assert_eq!(request_line, "GET / HTTP/1.1");
    assert!(
        !headers
            .iter()
            .any(|header| header.starts_with("authorization")),
        "{headers:?}"
    );

    let body = "a=1&b=2";
    let answer = daemon
        .send(
            &format!(
                "POST /plugins/pages/save HTTP/1.1\r\nhost: 127.0.0.1\r\ncookie: {cookie}\r\n\
                 content-type: application/x-www-form-urlencoded\r\ncontent-length: {}",
                body.len()
            ),
            body,
        )
        .await;
    assert_eq!(answer.forwarded().0, "POST /save HTTP/1.1");
    assert!(answer.body.ends_with(body), "{}", answer.body);
    daemon.stop().await;
}

#[tokio::test]
async fn with_access_unknown_and_stopped_plugins_answer_for_themselves() {
    let daemon = Daemon::start(upstream().await).await;
    let cookie = daemon.cookie();
    let headers = [("cookie", cookie.as_str())];
    assert_eq!(daemon.get("/plugins/unknown/", &headers).await.status, 404);
    assert_eq!(daemon.get("/plugins/plain/", &headers).await.status, 404);
    assert_eq!(daemon.get("/plugins/broken/", &headers).await.status, 503);
    let redirect = daemon.get("/plugins/pages?x=1", &headers).await;
    assert_eq!(redirect.status, 308);
    assert_eq!(redirect.header("location"), Some("pages/?x=1"));
    daemon.stop().await;
}

#[tokio::test]
async fn a_websocket_upgrade_is_joined_to_the_plugin() {
    let daemon = Daemon::start(upstream().await).await;
    let mut stream = TcpStream::connect(("127.0.0.1", daemon.running.address().port()))
        .await
        .unwrap();
    let request = format!(
        "GET /plugins/pages/live HTTP/1.1\r\nhost: 127.0.0.1\r\ncookie: {}\r\n\
         upgrade: websocket\r\nconnection: Upgrade\r\nsec-websocket-version: 13\r\n\
         sec-websocket-key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n",
        daemon.cookie()
    );
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut head = Vec::new();
    let mut byte = [0_u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        tokio::time::timeout(WAIT, stream.read_exact(&mut byte))
            .await
            .expect("a response head")
            .unwrap();
        head.push(byte[0]);
    }
    let head = String::from_utf8_lossy(&head).to_ascii_lowercase();
    assert!(head.starts_with("http/1.1 101"), "{head}");
    stream.write_all(b"ping through").await.unwrap();
    let mut echoed = [0_u8; 12];
    tokio::time::timeout(WAIT, stream.read_exact(&mut echoed))
        .await
        .expect("the echo")
        .unwrap();
    assert_eq!(&echoed, b"ping through");
    daemon.stop().await;
}

#[tokio::test]
async fn the_handshake_hands_out_the_page_ticket() {
    let daemon = Daemon::start(upstream().await).await;
    let client = Arc::clone(&daemon.fixture.server).connect_in_process();
    let hello = client.handshake().await.expect("handshake");
    assert_eq!(hello.page_ticket, Some(page_ticket(&daemon.token)));
    let pages = daemon
        .fixture
        .engine
        .plugin_statuses()
        .into_iter()
        .map(|plugin| (plugin.name, plugin.ui_path))
        .collect::<BTreeMap<_, _>>();
    assert_eq!(pages["pages"].as_deref(), Some("plugins/pages/"));
    assert_eq!(pages["plain"], None);
    daemon.stop().await;
}
