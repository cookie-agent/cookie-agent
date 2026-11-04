//! Connections to a daemon that is still starting
//! (`docs/site/specs/daemon-startup.md`).

use cookie_agent_protocol::{DaemonPhase, DaemonStatus, DaemonTask};

use super::*;

/// A starting daemon a test hands its server to and changes the status of.
struct Starting {
    server: Arc<std::sync::OnceLock<Arc<StubServer>>>,
    status: watch::Sender<DaemonStatus>,
}

impl Starting {
    fn new() -> Self {
        Self {
            server: Arc::new(std::sync::OnceLock::new()),
            status: watch::Sender::new(DaemonStatus {
                phase: DaemonPhase::Starting,
                tasks: vec![task("Moving sessions")],
                ..DaemonStatus::default()
            }),
        }
    }

    /// A connection, handshaken as a client of `version`; a client older
    /// than the daemon status has no answer yet.
    async fn connect(&self, version: cookie_agent_protocol::SemVer) -> Connection {
        let (requests, incoming) = mpsc::unbounded_channel();
        let (outgoing, frames) = mpsc::unbounded_channel();
        tokio::spawn(serve_starting(
            Startup {
                server: Arc::clone(&self.server),
                status: self.status.subscribe(),
                about: crate::server::About {
                    engine_version: "0.4.0".into(),
                    page_url: Some("https://cookie.example/".into()),
                    page_ticket: None,
                    share_url: None,
                },
                guard: None,
            },
            ChannelTransport { incoming, outgoing },
            CancellationToken::new(),
            Authentication::Authenticated,
        ));
        let connection = Connection { requests, frames };
        let mut hello = hello(None);
        hello["protocol_version"] = json!(version);
        connection.send(0, "handshake", hello);
        connection
    }

    /// Hands the server over, then says the daemon is ready.
    fn ready(&self) {
        assert!(self.server.set(Arc::new(StubServer::default())).is_ok());
        self.status.send_modify(|status| {
            status.phase = DaemonPhase::Ready;
            status.tasks.clear();
        });
    }
}

fn task(label: &str) -> DaemonTask {
    DaemonTask {
        id: "1".into(),
        label: label.into(),
        detail: None,
        progress: None,
        blocking: true,
        started_at: jiff::Timestamp::now(),
    }
}

const OLDER: cookie_agent_protocol::SemVer = cookie_agent_protocol::SemVer::new(1, 64, 0);

#[tokio::test]
async fn a_client_connects_while_the_daemon_starts_and_keeps_its_connection() {
    let starting = Starting::new();
    let mut connection = starting
        .connect(cookie_agent_protocol::CLIENT_PROTOCOL_VERSION)
        .await;
    let hello = connection.next().await;
    assert_eq!(hello["id"], 0);
    assert_eq!(hello["result"]["status"]["phase"], "starting");
    assert_eq!(
        hello["result"]["status"]["tasks"][0]["label"],
        "Moving sessions"
    );
    // The page's address is known before the daemon is ready.
    assert_eq!(hello["result"]["page_url"], "https://cookie.example/");

    connection.send(1, "session.list", json!({}));
    let refused = connection.next().await;
    assert_eq!(refused["id"], 1);
    assert_eq!(refused["error"]["data"]["code"], "daemon_starting");
    assert_eq!(refused["error"]["data"]["status"]["phase"], "starting");
    connection.send(2, cookie_agent_protocol::DAEMON_STATUS_METHOD, json!({}));
    let status = connection.next().await;
    assert_eq!(status["id"], 2);
    assert_eq!(status["result"]["phase"], "starting");

    starting.ready();
    let notification = connection.next().await;
    assert_eq!(
        notification["method"],
        cookie_agent_protocol::DAEMON_STATUS_METHOD
    );
    assert_eq!(notification["params"]["phase"], "ready");
    connection.send(3, "session.list", json!({}));
    let listed = connection.next().await;
    assert_eq!(listed["id"], 3);
    assert!(listed.get("result").is_some(), "served: {listed}");
}

#[tokio::test]
async fn an_older_client_is_answered_once_the_daemon_is_ready() {
    let starting = Starting::new();
    let mut connection = starting.connect(OLDER).await;
    assert!(
        tokio::time::timeout(Duration::from_millis(100), connection.frames.recv())
            .await
            .is_err(),
        "the handshake waits for the daemon"
    );
    starting.ready();
    let hello = connection.next().await;
    assert_eq!(hello["id"], 0);
    assert!(
        hello["result"].get("status").is_none(),
        "an older client is not sent the status: {hello}"
    );
    connection.send(1, "session.list", json!({}));
    let listed = connection.next().await;
    assert_eq!(listed["id"], 1);
    assert!(listed.get("result").is_some(), "served: {listed}");
}

#[tokio::test]
async fn a_failed_startup_reaches_every_waiting_client_and_closes_them() {
    let starting = Starting::new();
    let mut current = starting
        .connect(cookie_agent_protocol::CLIENT_PROTOCOL_VERSION)
        .await;
    assert_eq!(current.next().await["id"], 0);
    let mut older = starting.connect(OLDER).await;
    tokio::task::yield_now().await;
    let error = cookie_agent_protocol::errors::DAEMON_START_FAILED
        .error()
        .detail("the store is newer")
        .build();
    starting.status.send_modify(|status| {
        status.phase = DaemonPhase::Failed;
        status.error = Some(error);
    });

    let failed = current.next().await;
    assert_eq!(failed["params"]["phase"], "failed");
    assert_eq!(
        failed["params"]["error"]["data"]["detail"],
        "the store is newer"
    );
    assert!(current.closed().await);
    let refused = older.next().await;
    assert_eq!(refused["id"], 0);
    assert_eq!(refused["error"]["data"]["code"], "daemon_start_failed");
    assert!(older.closed().await);
}

#[tokio::test(start_paused = true)]
async fn progress_is_sent_at_most_once_an_interval_but_a_phase_change_at_once() {
    let starting = Starting::new();
    let mut connection = starting
        .connect(cookie_agent_protocol::CLIENT_PROTOCOL_VERSION)
        .await;
    assert_eq!(connection.next().await["id"], 0);
    for done in 1..=50 {
        starting.status.send_modify(|status| {
            status.tasks[0].progress = Some(cookie_agent_protocol::TaskProgress {
                done,
                total: Some(50),
                unit: None,
            });
        });
        tokio::task::yield_now().await;
    }
    let first = connection.next().await;
    let last = connection.next().await;
    assert_eq!(
        last["params"]["tasks"][0]["progress"]["done"], 50,
        "the latest progress goes once the interval passes: {first} then {last}"
    );
    assert!(
        tokio::time::timeout(STATUS_INTERVAL * 4, connection.frames.recv())
            .await
            .is_err(),
        "nothing more is sent without a change"
    );
    starting.ready();
    let ready = connection.next().await;
    assert_eq!(ready["params"]["phase"], "ready");
}
