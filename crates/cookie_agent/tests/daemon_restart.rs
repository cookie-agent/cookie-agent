//! The real `cookie daemon` restarts itself through `daemon.restart`
//! (`docs/site/specs/engine-configuration.md` section 11): it closes the
//! connection saying it is restarting, comes back on the same port with
//! the same token, and a client reconnects. On Unix the process ID stays the
//! same; on Windows the daemon is a new process, which the test stops by the
//! ID its listening line names.
//!
//! The daemon delays every close frame (`COOKIE_AGENT_TEST_CLOSE_DELAY_MS`),
//! so a daemon that relaunched before its close frames left would cut the
//! connection every time instead of now and then.

use std::{
    io::{BufRead as _, BufReader},
    path::{Path, PathBuf},
    process::{Child, ChildStdout, Command, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use cookie_agent_protocol::{Client, ClientDelivery, ClientIdentity, ClientKind, RestartWhen};

/// A private scratch home in the system temporary directory.
struct ScratchHome(PathBuf);

impl ScratchHome {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "cookie_daemon_restart_{}_{timestamp}_{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).expect("create the scratch home");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))
                .expect("make the scratch home private");
        }
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for ScratchHome {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A port nothing listens on right now.
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind an ephemeral port")
        .local_addr()
        .expect("its address")
        .port()
}

/// The daemon, stopped when the test ends however it ends.
struct Daemon {
    child: Child,
    stdout: BufReader<ChildStdout>,
    /// The process now serving, which differs from the child's on Windows
    /// after a restart.
    pid: u32,
}

/// What the daemon printed when it started serving.
struct Ready {
    url: String,
    token: String,
    pid: u32,
}

impl Daemon {
    fn start(home: &Path, port: u16) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_cookie"));
        command
            .args(["daemon", "--port", &port.to_string()])
            .current_dir(home)
            .env_clear()
            .env("HOME", home)
            .env("USERPROFILE", home)
            .env("COOKIE_AGENT_TEST_BUNDLED_CATALOG", "1")
            // Well within the second a closing connection gets to finish.
            .env("COOKIE_AGENT_TEST_CLOSE_DELAY_MS", "300")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        if let Some(root) = std::env::var_os("SystemRoot") {
            command.env("SystemRoot", root);
        }
        let mut child = command.spawn().expect("spawn the daemon");
        let stdout = BufReader::new(child.stdout.take().expect("stdout"));
        let pid = child.id();
        Self { child, stdout, pid }
    }

    /// Reads up to the next listening line.
    fn ready(&mut self) -> Ready {
        let mut endpoint = None;
        loop {
            let mut line = String::new();
            let read = self.stdout.read_line(&mut line).expect("read the daemon");
            assert!(read > 0, "the daemon closed its output before serving");
            if let Some(frame) = line.strip_prefix("daemon-ready ") {
                let frame: serde_json::Value =
                    serde_json::from_str(frame.trim()).expect("the ready frame");
                endpoint = Some((
                    frame["url"].as_str().expect("url").to_owned(),
                    frame["token"].as_str().expect("token").to_owned(),
                ));
            } else if let Some(rest) = line.trim().strip_prefix("cookie daemon listening on ") {
                let pid = rest
                    .rsplit_once("process ")
                    .and_then(|(_, pid)| pid.trim_end_matches(')').parse().ok())
                    .expect("the listening line names the process");
                let (url, token) = endpoint.expect("the ready line comes first");
                self.pid = pid;
                return Ready { url, token, pid };
            }
        }
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        stop(self.pid);
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            if self.child.try_wait().ok().flatten().is_some() && !running(self.pid) {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = self.child.kill();
    }
}

#[cfg(unix)]
fn stop(pid: u32) {
    let pid = libc::pid_t::try_from(pid).expect("pid fits");
    // SAFETY: plain signal delivery to the daemon this test started.
    unsafe {
        libc::kill(pid, libc::SIGTERM);
    }
}

#[cfg(unix)]
fn running(pid: u32) -> bool {
    let pid = libc::pid_t::try_from(pid).expect("pid fits");
    // SAFETY: signal 0 only checks that the process exists.
    unsafe { libc::kill(pid, 0) == 0 }
}

#[cfg(windows)]
fn stop(pid: u32) {
    let _ = Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

#[cfg(windows)]
fn running(pid: u32) -> bool {
    Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH"])
        .output()
        .is_ok_and(|output| String::from_utf8_lossy(&output.stdout).contains(&pid.to_string()))
}

fn identity() -> ClientIdentity {
    ClientIdentity::generate(ClientKind::new("test").expect("client kind"))
}

async fn connect(ready: &Ready) -> Client {
    let client = Client::connect_websocket_with_token(&ready.url, &ready.token)
        .await
        .expect("connect to the daemon");
    client
        .handshake_as(identity())
        .await
        .expect("handshake with the daemon");
    client
}

#[tokio::test(flavor = "multi_thread")]
async fn the_daemon_restarts_through_the_rpc_and_clients_reconnect() {
    let home = ScratchHome::new();
    let port = free_port();
    let mut daemon = Daemon::start(home.path(), port);
    let first = tokio::task::block_in_place(|| daemon.ready());
    let client = connect(&first).await;
    let mut deliveries = client.subscribe_deliveries().expect("deliveries");

    let scheduled = client
        .restart_daemon(RestartWhen::Idle)
        .await
        .expect("the daemon schedules its restart");
    assert!(scheduled.scheduled);
    assert_eq!(scheduled.active_runs, 0);

    // The daemon says it restarts now, then closes the connection saying
    // why.
    let mut told = false;
    let closed = tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            match deliveries.recv().await {
                Some(ClientDelivery::DaemonRestarting(restarting)) => {
                    assert_eq!(restarting.active_runs, 0);
                    told = true;
                }
                Some(ClientDelivery::Disconnected { error }) => break Some(error),
                Some(_) => {}
                None => break None,
            }
        }
    })
    .await
    .expect("the daemon closes the connection");
    assert!(told, "no daemon.restarting before the close");
    let closed = closed.expect("the connection ends with an error saying why");
    assert!(closed.ends_with("the daemon is restarting"), "{closed}");

    let second = tokio::task::block_in_place(|| daemon.ready());
    assert_eq!(second.url, first.url, "the same port");
    assert_eq!(second.token, first.token, "the same token");
    if cfg!(unix) {
        assert_eq!(second.pid, first.pid, "exec keeps the process");
        assert!(
            daemon.child.try_wait().expect("daemon status").is_none(),
            "the daemon process kept running"
        );
    } else {
        assert_ne!(second.pid, first.pid, "a new process serves");
    }
    let again = connect(&second).await;
    assert!(again.runtime_snapshot().await.is_ok());
    again.shutdown();
}
