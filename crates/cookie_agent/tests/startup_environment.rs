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

fn run_cookie(extra_environment: &[(&str, &str)]) -> Output {
    let home = TestHome::new();
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut command = Command::new(env!("CARGO_BIN_EXE_cookie"));
    command
        .current_dir(workspace)
        .env_clear()
        .env("HOME", home.path())
        .env("COOKIE_TEST_API_KEY", "expected-process-credential");
    for (key, value) in extra_environment {
        command.env(key, value);
    }
    command.output().expect("run cookie process")
}

fn assert_reached_tui(output: Output, secrets: &[&str]) {
    assert!(!output.status.success(), "non-TTY startup must fail");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let report = format!("{stdout}\n{stderr}");
    assert!(report.contains("enable terminal raw mode"), "{report}");
    assert!(!report.contains("load workspace configuration"), "{report}");
    assert!(!report.contains("extraction failed"), "{report}");
    for secret in secrets {
        assert!(!report.contains(secret), "secret leaked in process error");
    }
}

#[test]
fn startup_with_expected_credential_and_cookie_agent_theme_reaches_tui() {
    let output = run_cookie(&[
        ("COOKIE_AGENT_THEME", "legacy-theme-value"),
        ("COOKIE_AGENT_FOO", "arbitrary-runtime-value"),
    ]);
    assert_reached_tui(
        output,
        &[
            "expected-process-credential",
            "legacy-theme-value",
            "arbitrary-runtime-value",
        ],
    );
}

#[test]
fn startup_with_expected_and_cookie_agent_provider_credentials_reaches_tui() {
    let output = run_cookie(&[
        ("COOKIE_AGENT_TEST_API_KEY", "provider-process-secret"),
        ("COOKIE_AGENT_FOO", "arbitrary-provider-value"),
        (
            "COOKIE_AGENT_CONFIG__SERVER__PORT",
            "must-not-be-a-config-override",
        ),
    ]);
    assert_reached_tui(
        output,
        &[
            "expected-process-credential",
            "provider-process-secret",
            "arbitrary-provider-value",
            "must-not-be-a-config-override",
        ],
    );
}
