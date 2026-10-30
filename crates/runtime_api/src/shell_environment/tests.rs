use super::*;

#[test]
fn reads_only_between_the_markers() {
    let output = b"banner MARK\0A=1\0B=2\0MARKfortune";
    assert_eq!(between_markers(output, b"MARK"), Some(&b"\0A=1\0B=2\0"[..]));
    assert_eq!(between_markers(b"banner MARK\0A=1\0", b"MARK"), None);
    assert_eq!(between_markers(b"no markers", b"MARK"), None);
}

#[test]
fn parses_values_with_newlines_and_equals_and_drops_the_shells_own() {
    let entries = b"MULTI=one\ntwo\0EQ=a=b\0PWD=/x\0SHLVL=2\0_=/usr/bin/env\0\
        COOKIE_SHELL_CAPTURE=1\0TERM=dumb\0PS1=$ \0no_equals\0=empty_name\0EMPTY=\0";
    let variables = parse_variables(entries);
    let names = variables
        .iter()
        .map(|(name, value)| (name.to_str().unwrap(), value.to_str().unwrap()))
        .collect::<Vec<_>>();
    assert_eq!(names, [("MULTI", "one\ntwo"), ("EQ", "a=b"), ("EMPTY", "")]);
}

#[test]
fn applied_environment_replaces_the_processs_and_yields_to_later_variables() {
    let environment = ShellEnvironment {
        variables: vec![
            ("CAPTURED".into(), "from-shell".into()),
            ("OVERRIDDEN".into(), "from-shell".into()),
            ("PATH".into(), "/usr/bin:/bin".into()),
        ],
        snapshot: None,
    };
    let mut command = std::process::Command::new("env");
    environment.apply(&mut command);
    command.env("OVERRIDDEN", "configured");
    let output = command.output().expect("run env");
    let output = String::from_utf8(output.stdout).expect("utf-8 env");
    let lines = output.lines().collect::<Vec<_>>();
    assert!(lines.contains(&"CAPTURED=from-shell"), "{output}");
    assert!(lines.contains(&"OVERRIDDEN=configured"), "{output}");
    assert!(!lines.contains(&"OVERRIDDEN=from-shell"), "{output}");
}

#[cfg(unix)]
mod capture {
    use super::*;

    /// A home whose rc files are Debian's shape: `.profile` sources
    /// `.bashrc`, which returns early unless interactive.
    struct Home(tempfile::TempDir);

    impl Home {
        fn new(bashrc_below_guard: &str) -> Self {
            let home = tempfile::tempdir().expect("scratch home");
            std::fs::write(
                home.path().join(".profile"),
                "[ -f \"$HOME/.bashrc\" ] && . \"$HOME/.bashrc\"\n",
            )
            .expect("write .profile");
            std::fs::write(
                home.path().join(".bashrc"),
                format!("case $- in *i*) ;; *) return;; esac\n{bashrc_below_guard}\n"),
            )
            .expect("write .bashrc");
            Self(home)
        }

        async fn capture(&self, capture: ShellCapture) -> Result<ShellEnvironment, String> {
            self.capture_within(capture, 30_000).await
        }

        async fn capture_within(
            &self,
            capture: ShellCapture,
            timeout_ms: i64,
        ) -> Result<ShellEnvironment, String> {
            within(
                timeout_ms,
                run(&bash(), capture, Some(self.0.path().as_os_str())),
            )
            .await
        }
    }

    fn bash() -> String {
        ["/bin/bash", "/usr/bin/bash", "/usr/local/bin/bash"]
            .into_iter()
            .find(|path| Path::new(path).exists())
            .expect("bash installed")
            .to_owned()
    }

    fn value(environment: &ShellEnvironment, name: &str) -> Option<String> {
        environment
            .variables()
            .iter()
            .find(|(variable, _)| variable == name)
            .map(|(_, value)| value.to_string_lossy().into_owned())
    }

    /// Runs `command` as the `bash` tool does: `environment` applied and
    /// its snapshot sourced first.
    fn run_like_the_bash_tool(environment: &ShellEnvironment, command: &str) -> String {
        let snapshot = environment.snapshot().expect("bash wrote a snapshot");
        let mut process = std::process::Command::new(bash());
        environment.apply(&mut process);
        let output = process
            .arg("-c")
            .arg(format!(
                "source '{}' 2>/dev/null\n{command}",
                snapshot.display()
            ))
            .output()
            .expect("run bash");
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    #[tokio::test]
    async fn interactive_capture_reaches_past_the_interactivity_guard() {
        let home = Home::new("export BELOW_GUARD=yes");
        let interactive = home.capture(ShellCapture::Interactive).await.unwrap();
        assert_eq!(value(&interactive, "BELOW_GUARD").as_deref(), Some("yes"));
        let login = home.capture(ShellCapture::Login).await.unwrap();
        assert_eq!(value(&login, "BELOW_GUARD"), None);
    }

    #[tokio::test]
    async fn output_around_the_environment_is_ignored() {
        let home = Home::new("echo welcome\nprintf 'A=B\\0'\nexport KEPT=1\ntrap 'echo bye' EXIT");
        let environment = home.capture(ShellCapture::Interactive).await.unwrap();
        assert_eq!(value(&environment, "KEPT").as_deref(), Some("1"));
        assert_eq!(value(&environment, "A"), None);
    }

    #[tokio::test]
    async fn rc_files_can_tell_a_capture_apart() {
        let home = Home::new("[ -n \"$COOKIE_SHELL_CAPTURE\" ] && export SAW_CAPTURE=1");
        let environment = home.capture(ShellCapture::Interactive).await.unwrap();
        assert_eq!(value(&environment, "SAW_CAPTURE").as_deref(), Some("1"));
        assert_eq!(value(&environment, CAPTURE_VARIABLE), None);
    }

    #[tokio::test]
    async fn the_snapshot_brings_functions_needing_extglob_and_aliases() {
        let home = Home::new(
            "shopt -s extglob\n\
             pick() { case $1 in +(a|b)) echo matched;; *) echo other;; esac; }\n\
             greet() { echo \"hello $1\"; }\n\
             alias hi='greet alias'",
        );
        let environment = home.capture(ShellCapture::Interactive).await.unwrap();
        let output = run_like_the_bash_tool(&environment, "pick ab; greet fn; hi");
        assert_eq!(output, "matched\nhello fn\nhello alias\n");
    }

    #[tokio::test]
    async fn the_snapshot_goes_with_the_environment() {
        let home = Home::new("");
        let environment = home.capture(ShellCapture::Interactive).await.unwrap();
        let snapshot = environment.snapshot().expect("snapshot").to_owned();
        assert!(snapshot.exists());
        drop(environment);
        assert!(!snapshot.exists());
    }

    #[test]
    fn a_capture_removes_the_snapshots_of_processes_that_are_gone() {
        let directory = tempfile::tempdir().expect("scratch directory");
        let mut exited = std::process::Command::new("true")
            .spawn()
            .expect("start a process");
        let gone = exited.id();
        exited.wait().expect("the process exits");
        let mine = std::process::id();
        let names = [
            format!("{SNAPSHOT_PREFIX}{gone}-AbCdEf"),
            format!("{SNAPSHOT_PREFIX}{mine}-AbCdEf"),
            format!("{SNAPSHOT_PREFIX}1-AbCdEf"),
            format!("{SNAPSHOT_PREFIX}AbCdEf"),
            format!("other-{gone}-AbCdEf"),
        ];
        for name in &names {
            std::fs::create_dir(directory.path().join(name)).expect("make a directory");
        }
        remove_stale_snapshots(directory.path());
        let left = names
            .iter()
            .filter(|name| directory.path().join(name).exists())
            .cloned()
            .collect::<Vec<_>>();
        // The exited process's goes; this process's, a live one's (init),
        // and names that are not a process's snapshot stay.
        assert_eq!(left, names[1..]);
    }

    #[tokio::test]
    async fn a_slow_rc_file_times_out() {
        let home = Home::new("sleep 30");
        let started = std::time::Instant::now();
        let error = home
            .capture_within(ShellCapture::Interactive, 300)
            .await
            .unwrap_err();
        assert_eq!(error, "timed out after 300 ms");
        assert!(started.elapsed() < std::time::Duration::from_secs(10));
    }

    #[tokio::test]
    async fn an_rc_file_that_replaces_the_shell_fails() {
        let home = Home::new("exec cat");
        let error = home.capture(ShellCapture::Interactive).await.unwrap_err();
        assert!(error.contains("before printing its environment"), "{error}");
    }

    #[tokio::test]
    async fn a_missing_shell_fails() {
        let error = within(1_000, run("/nonexistent/shell", ShellCapture::Login, None))
            .await
            .unwrap_err();
        assert!(
            error.starts_with("`/nonexistent/shell` did not start"),
            "{error}"
        );
    }
}

#[tokio::test]
async fn capture_off_keeps_the_processs_environment() {
    let mut reports = reports();
    configure(Some(&ShellEnvironmentSettings {
        capture: ShellCapture::Off,
        timeout_ms: 1_000,
        shell: String::new(),
    }));
    assert!(environment().await.is_none());
    let report = reports.next().await;
    assert!(!report.captured);
    assert_eq!(report.error, None);
}
