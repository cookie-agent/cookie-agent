#![cfg(unix)]

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

/// A syntactically valid 43-character base64url daemon token for CLI parsing.
const TEST_TOKEN: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";

struct TestHome(PathBuf);

impl TestHome {
    fn new() -> Self {
        use std::os::unix::fs::PermissionsExt as _;

        static NEXT: AtomicU64 = AtomicU64::new(0);
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "cookie_startup_environment_{}_{timestamp}_{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).expect("create process test home");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700))
            .expect("make process test home private");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestHome {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn run_cookie(arguments: &[&str], extra_environment: &[(&str, &str)]) -> Output {
    let home = TestHome::new();
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut command = Command::new(env!("CARGO_BIN_EXE_cookie"));
    command
        .args(arguments)
        .current_dir(workspace)
        .env_clear()
        .env("HOME", home.path())
        .env("COOKIE_TEST_API_KEY", "expected-process-credential");
    for (key, value) in extra_environment {
        command.env(key, value);
    }
    command.output().expect("run cookie process")
}

#[test]
fn checked_fixture_rejects_ambient_config_overrides_before_cli_secret_input() {
    let output = run_cookie(
        &["connect", "--token", TEST_TOKEN],
        &[
            ("COOKIE_AGENT_THEME", "ignored-theme"),
            ("COOKIE_AGENT_CONFIG__SERVER__PORT", "ignored-port"),
            ("COOKIE_AGENT_FOO", "ignored-runtime-value"),
        ],
    );
    assert!(!output.status.success(), "non-TTY connect must fail");
    let report = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(report.contains("interactive TTY"), "{report}");
    assert!(!report.contains("load user configuration"), "{report}");
    for secret in [
        "expected-process-credential",
        "ignored-theme",
        "ignored-port",
        "ignored-runtime-value",
    ] {
        assert!(
            !report.contains(secret),
            "secret or ambient value leaked: {report}"
        );
    }
}

#[test]
fn disconnect_is_cwd_independent_and_rejects_non_tty() {
    let output = run_cookie(&["disconnect", "openai", "--token", TEST_TOKEN], &[]);
    assert!(!output.status.success());
    let report = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(report.contains("interactive TTY"), "{report}");
    assert!(!report.contains("load user configuration"), "{report}");
}

#[test]
#[cfg(feature = "tui")]
fn attach_uses_the_tui_entry_point_without_a_workspace() {
    let output = run_cookie(&["attach", "--token", TEST_TOKEN], &[]);
    assert!(!output.status.success());
    let report = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(report.contains("connect to daemon WebSocket"), "{report}");
    assert!(!report.contains("load user configuration"), "{report}");
    assert!(!report.contains("built without TUI support"), "{report}");
}

#[test]
fn connect_is_cwd_independent_and_rejects_non_tty_before_credentials() {
    let output = run_cookie(&["connect", "openai", "--token", TEST_TOKEN], &[]);
    assert!(!output.status.success());
    let report = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(report.contains("interactive TTY"), "{report}");
    assert!(!report.contains("load user configuration"), "{report}");
    assert!(!report.contains("expected-process-credential"), "{report}");
}

/// The daemon shuts down and exits cleanly on each signal that asks it to
/// stop, as `kill`, systemd, or a closed terminal sends.
#[test]
fn the_daemon_exits_cleanly_on_each_stop_signal() {
    use std::io::BufRead as _;

    for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
        let home = TestHome::new();
        let mut daemon = Command::new(env!("CARGO_BIN_EXE_cookie"))
            .args(["daemon", "--listen", "127.0.0.1:0"])
            .current_dir(home.path())
            .env_clear()
            .env("HOME", home.path())
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn the daemon");
        // Kept open until the daemon exits, so its later lines still write.
        let mut stdout = std::io::BufReader::new(daemon.stdout.take().expect("stdout"));
        let mut ready = String::new();
        stdout.read_line(&mut ready).expect("read the ready line");
        assert!(ready.starts_with("daemon-ready "), "{ready:?}");
        let pid = i32::try_from(daemon.id()).expect("pid fits");
        // SAFETY: plain signal delivery to a child process this test started.
        assert_eq!(unsafe { libc::kill(pid, signal) }, 0, "signal {pid}");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        let status = loop {
            if let Some(status) = daemon.try_wait().expect("daemon status") {
                break status;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the daemon did not stop"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        };
        assert!(status.success(), "signal {signal}: {status}");
        drop(stdout);
    }
}
