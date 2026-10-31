#![cfg(unix)]
//! Which commands of the real `cookie` binary look for a daemon at the
//! address `[server]` of a scratch home describes: plain `cookie` does, and
//! `cookie --standalone` and `cookie run` never do. A listener that counts
//! its connections stands in for the daemon; the probe and the runtime
//! choice against a real daemon are tested beside `local_daemon` in the
//! binary.

use std::{
    fs,
    os::unix::{fs::PermissionsExt as _, process::CommandExt as _},
    path::Path,
    process::{Command, Output, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

/// A syntactically valid 43-character base64url daemon token.
const TEST_TOKEN: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";

/// Accepts connections on loopback, counts them, and closes each at once.
struct CountingListener {
    port: u16,
    accepted: Arc<AtomicUsize>,
}

impl CountingListener {
    fn start() -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind listener");
        let port = listener.local_addr().expect("listener address").port();
        let accepted = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&accepted);
        thread::spawn(move || {
            for stream in listener.incoming() {
                counter.fetch_add(1, Ordering::SeqCst);
                drop(stream);
            }
        });
        Self { port, accepted }
    }

    fn accepted(&self) -> usize {
        self.accepted.load(Ordering::SeqCst)
    }
}

fn make_private(path: &Path) {
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).expect("private directory");
}

/// A scratch home whose `config.toml` has `server` under `[server]`, and a
/// workspace to run in.
struct Home {
    root: tempfile::TempDir,
}

impl Home {
    fn new(server: &str) -> Self {
        let root = tempfile::tempdir().expect("home root");
        make_private(root.path());
        let config_root = root.path().join("home/.cookie-agent");
        fs::create_dir_all(&config_root).expect("config root");
        make_private(&root.path().join("home"));
        make_private(&config_root);
        fs::create_dir(root.path().join("workspace")).expect("workspace");
        let config = config_root.join("config.toml");
        fs::write(&config, format!("[server]\n{server}")).expect("config");
        fs::set_permissions(&config, fs::Permissions::from_mode(0o600)).expect("private config");
        Self { root }
    }

    /// Runs `cookie` with `arguments` in a session of its own, with no
    /// terminal: a TUI it starts fails at once instead of taking the test's
    /// terminal.
    fn run(&self, arguments: &[&str]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_cookie"));
        command
            .args(arguments)
            .current_dir(self.root.path().join("workspace"))
            .env_clear()
            .env("HOME", self.root.path().join("home"))
            .env("COOKIE_AGENT_TEST_BUNDLED_CATALOG", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // SAFETY: setsid is async-signal-safe and touches no state of this
        // process.
        unsafe {
            command.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
        let child = command.spawn().expect("run cookie");
        // A cookie that does not exit is killed, failing the test below.
        let pid = libc::pid_t::try_from(child.id()).expect("pid");
        let exited = Arc::new(AtomicBool::new(false));
        let watched = Arc::clone(&exited);
        thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(60);
            while Instant::now() < deadline {
                if watched.load(Ordering::SeqCst) {
                    return;
                }
                thread::sleep(Duration::from_millis(50));
            }
            // SAFETY: kill only sends a signal to the child's process ID.
            unsafe {
                libc::kill(pid, libc::SIGKILL);
            }
        });
        let output = child.wait_with_output().expect("wait for cookie");
        exited.store(true, Ordering::SeqCst);
        assert!(
            output.status.code().is_some(),
            "cookie {arguments:?} did not exit: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }
}

fn configured(listener: &CountingListener) -> Home {
    Home::new(&format!(
        "port = {}\ntoken = \"{TEST_TOKEN}\"\n",
        listener.port
    ))
}

#[test]
fn plain_cookie_looks_for_the_daemon_server_describes() {
    let listener = CountingListener::start();
    let home = configured(&listener);
    let output = home.run(&[]);
    assert_eq!(
        listener.accepted(),
        1,
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn plain_cookie_without_a_token_dials_nothing() {
    let listener = CountingListener::start();
    let home = Home::new(&format!("port = {}\n", listener.port));
    let output = home.run(&[]);
    assert_eq!(
        listener.accepted(),
        0,
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn standalone_and_headless_runs_never_look_for_a_daemon() {
    let listener = CountingListener::start();
    let home = configured(&listener);
    for arguments in [&["--standalone"][..], &["run", "hello"][..]] {
        let output = home.run(arguments);
        assert_eq!(
            listener.accepted(),
            0,
            "{arguments:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn standalone_with_a_subcommand_is_a_usage_error() {
    let listener = CountingListener::start();
    let home = configured(&listener);
    for arguments in [
        &["--standalone", "run", "hello"][..],
        &["--standalone", "attach"][..],
    ] {
        let output = home.run(arguments);
        assert_eq!(output.status.code(), Some(2), "{arguments:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("--standalone applies to plain `cookie` only"),
            "{stderr}"
        );
    }
    // A subcommand does not take it after its name.
    let output = home.run(&["run", "--standalone", "hello"]);
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(listener.accepted(), 0);
}
