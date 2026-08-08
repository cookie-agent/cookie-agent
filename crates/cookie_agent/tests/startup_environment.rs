#![cfg(unix)]

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

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
fn checked_schema7_fixture_rejects_ambient_config_overrides_before_cli_secret_input() {
    let output = run_cookie(
        &["connect"],
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
    assert!(
        !report.contains("load schema-9 workspace configuration"),
        "{report}"
    );
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
    let output = run_cookie(&["disconnect", "openai"], &[]);
    assert!(!output.status.success());
    let report = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(report.contains("interactive TTY"), "{report}");
    assert!(!report.contains("workspace configuration"), "{report}");
}

#[test]
#[cfg(feature = "tui")]
fn attach_uses_the_tui_entry_point_without_a_workspace() {
    let output = run_cookie(&["attach"], &[]);
    assert!(!output.status.success());
    let report = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(report.contains("connect to daemon WebSocket"), "{report}");
    assert!(!report.contains("workspace configuration"), "{report}");
    assert!(!report.contains("built without TUI support"), "{report}");
}

#[test]
fn connect_is_cwd_independent_and_rejects_non_tty_before_credentials() {
    let output = run_cookie(&["connect", "openai"], &[]);
    assert!(!output.status.success());
    let report = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(report.contains("interactive TTY"), "{report}");
    assert!(!report.contains("workspace configuration"), "{report}");
    assert!(!report.contains("expected-process-credential"), "{report}");
}
