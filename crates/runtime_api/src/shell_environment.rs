//! The user's shell environment for the processes a runtime starts
//! (`docs/site/specs/shell-environment.md`).
//!
//! A service manager starts the daemon, and sshd a runtime, with a minimal
//! environment, while the user's setup lives in their shell's rc files. A
//! runtime runs the user's shell once, has it print its environment between
//! two random markers, and starts every process with that environment in
//! place of its own: the `bash` tool's commands, MCP servers, ACP agents,
//! plugins, and the commands of runtime templates. With a bash shell it also
//! writes a snapshot of the shell's options, aliases, and functions, which
//! the `bash` tool sources before each command.
//!
//! There is one shell environment per process: every runtime a process
//! hosts runs on the same machine as the same user. [`configure`] sets how
//! it is captured, from the settings the daemon sends a runtime;
//! [`environment`] is what a process start uses: `None`, the process's own
//! environment, until configured, while capture is off, and after a failed
//! capture. The process's own environment is never changed.

// Only Unix captures; elsewhere the parsing and snapshot helpers go unused.
#![cfg_attr(not(unix), allow(dead_code))]

use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
};

use tokio::sync::watch;

use crate::protocol::{ShellCapture, ShellEnvironmentReport, ShellEnvironmentSettings};

/// Set in the environment of the shell a capture runs, so an rc file can
/// skip what must not run there, such as `exec tmux`.
pub const CAPTURE_VARIABLE: &str = "COOKIE_SHELL_CAPTURE";

/// A captured shell environment.
#[derive(Debug)]
pub struct ShellEnvironment {
    variables: Vec<(OsString, OsString)>,
    snapshot: Option<Snapshot>,
}

/// The snapshot file, in a private directory removed with it.
///
/// The process's environment lives in a static, which is never dropped, so
/// the directory outlives its process. It is named after the process's ID
/// ([`SNAPSHOT_PREFIX`]`<pid>-`), and each capture removes those of the
/// user's processes that are gone ([`remove_stale_snapshots`]).
#[derive(Debug)]
struct Snapshot {
    path: PathBuf,
    _directory: tempfile::TempDir,
}

impl ShellEnvironment {
    /// The captured variables, without those that describe the capturing
    /// shell rather than the user's setup.
    #[must_use]
    pub fn variables(&self) -> &[(OsString, OsString)] {
        &self.variables
    }

    /// The bash snapshot of the shell's options, aliases, and functions,
    /// when the shell was bash. It exists while `self` does.
    #[must_use]
    pub fn snapshot(&self) -> Option<&Path> {
        self.snapshot
            .as_ref()
            .map(|snapshot| snapshot.path.as_path())
    }

    /// Makes `command` start with this environment in place of the
    /// process's own, keeping the process's values of the variables a
    /// capture leaves out. Variables `command` sets afterwards still apply
    /// over it, so call this before setting a process's configured `env`.
    pub fn apply(&self, command: &mut std::process::Command) {
        command.env_clear();
        command.envs(self.variables.iter().map(|(name, value)| (name, value)));
        command.envs(std::env::vars_os().filter(|(name, _)| !captured_name(name)));
    }
}

/// Applies `environment` to `command`, if there is one.
pub fn apply(environment: Option<&ShellEnvironment>, command: &mut tokio::process::Command) {
    if let Some(environment) = environment {
        environment.apply(command.as_std_mut());
    }
}

/// Whether a capture keeps variable `name`: everything but the variables
/// that describe the capturing shell, and cookie's own, which always come
/// from the process.
fn captured_name(name: &std::ffi::OsStr) -> bool {
    const SHELL_OWN: &[&str] = &[
        "PWD",
        "OLDPWD",
        "SHLVL",
        "_",
        "PS1",
        "PS2",
        "PS3",
        "PS4",
        "PROMPT_COMMAND",
        "COLUMNS",
        "LINES",
        "HISTFILE",
        "TERM",
    ];
    let name = name.to_string_lossy();
    !SHELL_OWN.contains(&name.as_ref()) && !name.starts_with("COOKIE_")
}

/// How the latest capture went.
#[derive(Debug)]
struct Outcome {
    environment: Option<Arc<ShellEnvironment>>,
    report: ShellEnvironmentReport,
}

#[derive(Clone, Debug)]
enum Status {
    /// Nothing configured capture: the process's own environment.
    Unconfigured,
    /// A capture is running; process starts wait for it.
    Capturing,
    Done(Arc<Outcome>),
}

#[derive(Default)]
struct Configuration {
    /// `None` once configured without settings, as by an older daemon.
    settings: Option<Option<ShellEnvironmentSettings>>,
    /// Counts captures, so a superseded one does not land.
    generation: u64,
}

struct Global {
    configuration: Mutex<Configuration>,
    status: watch::Sender<Status>,
}

fn global() -> &'static Global {
    static GLOBAL: OnceLock<Global> = OnceLock::new();
    GLOBAL.get_or_init(|| Global {
        configuration: Mutex::default(),
        status: watch::Sender::new(Status::Unconfigured),
    })
}

fn lock_configuration() -> std::sync::MutexGuard<'static, Configuration> {
    global()
        .configuration
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Sets how the process captures the user's shell environment, capturing
/// anew when that changed. `None`, as from a daemon older than runtime
/// protocol 1.12.0, keeps the process's own environment. Called from a
/// Tokio runtime, which runs the capture.
pub fn configure(settings: Option<&ShellEnvironmentSettings>) {
    let mut configuration = lock_configuration();
    if configuration.settings.as_ref() == Some(&settings.cloned()) {
        return;
    }
    configuration.settings = Some(settings.cloned());
    start(&mut configuration);
}

/// Captures again with the settings [`configure`] set, as after the user
/// edited their rc files. Does nothing before [`configure`] or while
/// capture is off.
pub fn refresh() {
    let mut configuration = lock_configuration();
    let enabled = configuration
        .settings
        .as_ref()
        .and_then(Option::as_ref)
        .is_some_and(|settings| settings.capture != ShellCapture::Off);
    if enabled {
        start(&mut configuration);
    }
}

fn start(configuration: &mut Configuration) {
    configuration.generation += 1;
    let generation = configuration.generation;
    let settings = configuration.settings.clone().flatten();
    let status = &global().status;
    let Some(settings) =
        settings.filter(|settings| settings.capture != ShellCapture::Off && cfg!(unix))
    else {
        status.send_replace(Status::Done(Arc::new(Outcome {
            environment: None,
            report: ShellEnvironmentReport {
                captured: false,
                shell: String::new(),
                variables: 0,
                snapshot: false,
                duration_ms: 0,
                error: None,
            },
        })));
        return;
    };
    status.send_replace(Status::Capturing);
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        status.send_replace(Status::Done(Arc::new(failed(
            &settings.shell,
            std::time::Duration::ZERO,
            "no async runtime to capture on".to_owned(),
        ))));
        return;
    };
    handle.spawn(async move {
        let outcome = capture(&settings).await;
        log(&outcome.report);
        // Another configuration or refresh started a newer capture.
        if lock_configuration().generation == generation {
            global()
                .status
                .send_replace(Status::Done(Arc::new(outcome)));
        }
    });
}

/// Logs a capture, never its values. A failure is also reported to the
/// daemon, whose configuration status shows it.
fn log(report: &ShellEnvironmentReport) {
    match &report.error {
        None => tracing::info!(
            shell = %report.shell,
            variables = report.variables,
            snapshot = report.snapshot,
            duration_ms = report.duration_ms,
            "captured the shell environment"
        ),
        Some(error) => tracing::warn!(
            shell = %report.shell,
            %error,
            "shell environment capture failed; using the process's own environment"
        ),
    }
}

/// The environment process starts use now: waits for a capture that is
/// running, and is `None` (the process's own environment) when nothing
/// configured capture, capture is off, or it failed.
pub async fn environment() -> Option<Arc<ShellEnvironment>> {
    let mut status = global().status.subscribe();
    loop {
        match &*status.borrow_and_update() {
            Status::Unconfigured => return None,
            Status::Done(outcome) => return outcome.environment.clone(),
            Status::Capturing => {}
        }
        // The sender lives in a static, so it never closes.
        if status.changed().await.is_err() {
            return None;
        }
    }
}

/// The reports of finished captures, for a runtime to send the daemon.
pub struct Reports {
    status: watch::Receiver<Status>,
    current: bool,
}

/// Follows the captures from now on, starting with the latest finished one.
#[must_use]
pub fn reports() -> Reports {
    Reports {
        status: global().status.subscribe(),
        current: true,
    }
}

impl Reports {
    /// The report of the latest finished capture the first time, and of
    /// each one that finishes after that.
    pub async fn next(&mut self) -> ShellEnvironmentReport {
        if std::mem::take(&mut self.current)
            && let Status::Done(outcome) = &*self.status.borrow_and_update()
        {
            return outcome.report.clone();
        }
        loop {
            // The sender lives in a static, so it never closes.
            if self.status.changed().await.is_err() {
                std::future::pending::<()>().await;
            }
            if let Status::Done(outcome) = &*self.status.borrow_and_update() {
                return outcome.report.clone();
            }
        }
    }
}

fn failed(shell: &str, elapsed: std::time::Duration, error: String) -> Outcome {
    Outcome {
        environment: None,
        report: ShellEnvironmentReport {
            captured: false,
            shell: shell.to_owned(),
            variables: 0,
            snapshot: false,
            duration_ms: u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
            error: Some(error),
        },
    }
}

/// The shell `settings` names, else `$SHELL`, else `/bin/sh`.
fn shell(settings: &ShellEnvironmentSettings) -> String {
    if !settings.shell.is_empty() {
        return settings.shell.clone();
    }
    std::env::var("SHELL")
        .ok()
        .filter(|shell| !shell.is_empty())
        .unwrap_or_else(|| "/bin/sh".to_owned())
}

/// The most output a capture reads before it gives up.
const MAX_OUTPUT_BYTES: usize = 16 * 1024 * 1024;

#[cfg(not(unix))]
async fn capture(settings: &ShellEnvironmentSettings) -> Outcome {
    failed(
        &shell(settings),
        std::time::Duration::ZERO,
        "this platform does not capture the shell environment".to_owned(),
    )
}

#[cfg(unix)]
async fn capture(settings: &ShellEnvironmentSettings) -> Outcome {
    let started = std::time::Instant::now();
    let shell = shell(settings);
    let home = std::env::var_os("HOME");
    let outcome = within(
        settings.timeout_ms,
        run(&shell, settings.capture, home.as_deref()),
    )
    .await;
    match outcome {
        Ok(environment) => Outcome {
            report: ShellEnvironmentReport {
                captured: true,
                shell,
                variables: environment.variables.len() as u64,
                snapshot: environment.snapshot.is_some(),
                duration_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
                error: None,
            },
            environment: Some(Arc::new(environment)),
        },
        Err(error) => failed(&shell, started.elapsed(), error),
    }
}

/// `capture`'s result, or a timeout error after `timeout_ms`; 0 or less
/// waits indefinitely.
#[cfg(unix)]
async fn within(
    timeout_ms: i64,
    capture: impl Future<Output = Result<ShellEnvironment, String>>,
) -> Result<ShellEnvironment, String> {
    match u64::try_from(timeout_ms) {
        Ok(timeout_ms) if timeout_ms > 0 => {
            tokio::time::timeout(std::time::Duration::from_millis(timeout_ms), capture)
                .await
                .unwrap_or_else(|_| Err(format!("timed out after {timeout_ms} ms")))
        }
        _ => capture.await,
    }
}

/// Whether `shell` is bash, whose functions and aliases the `bash` tool
/// can use.
fn is_bash(shell: &str) -> bool {
    Path::new(shell)
        .file_name()
        .is_some_and(|name| name == "bash")
}

/// What the capturing shell runs. It writes the bash snapshot to `$1`
/// first: shell options before the functions, since functions such as
/// bash-completion's use extended globs that only parse once `extglob` is
/// on, leaving out bash-completion's own functions, which only a prompt
/// uses. Then it prints the environment between two `marker`s, so what rc
/// files print around it is never read as variables. Only `printf`, `env`,
/// and `;` outside the bash part, so sh, zsh, and fish run it unchanged.
fn script(marker: &str, bash: bool) -> String {
    let environment = format!("printf '%s' {marker}; env -0; printf '%s' {marker}");
    if !bash {
        return environment;
    }
    format!(
        r#"__cookie_snapshot() {{
  local line name source
  while IFS= read -r line; do
    case $line in *' login_shell') ;; *) printf '%s\n' "$line" ;; esac
  done < <(shopt -p)
  alias -p
  shopt -s extdebug
  for name in $(compgen -A function); do
    [ "$name" = __cookie_snapshot ] && continue
    source=$(declare -F "$name")
    source=${{source#"$name" * }}
    case $source in
      /usr/share/bash-completion/*|/etc/bash_completion*) ;;
      *) declare -f "$name" ;;
    esac
  done
  shopt -u extdebug
}}
__cookie_snapshot > "$1"
unset -f __cookie_snapshot
{environment}"#
    )
}

/// The start of a snapshot directory's name, before its process's ID.
const SNAPSHOT_PREFIX: &str = "cookie-shell-";

/// Removes the snapshot directories in `directory` that the user's
/// processes other than this one left: those whose process is gone. One
/// that cannot be removed is left for a later capture.
#[cfg(unix)]
fn remove_stale_snapshots(directory: &Path) {
    use std::os::unix::fs::MetadataExt as _;

    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    // SAFETY: `getuid` has no preconditions and touches no memory.
    let user = unsafe { libc::getuid() };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name
            .to_str()
            .and_then(|name| name.strip_prefix(SNAPSHOT_PREFIX))
            .and_then(|rest| rest.split_once('-'))
            .and_then(|(pid, _)| pid.parse::<libc::pid_t>().ok())
        else {
            continue;
        };
        if pid <= 0 || pid == std::process::id() as libc::pid_t || process_exists(pid) {
            continue;
        }
        // Not followed: a link is not a directory a capture made.
        if entry
            .metadata()
            .is_ok_and(|metadata| metadata.is_dir() && metadata.uid() == user)
        {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

/// Whether process `pid` exists, as `kill(pid, 0)` tells: one of another
/// user's that refuses the signal exists.
#[cfg(unix)]
fn process_exists(pid: libc::pid_t) -> bool {
    // SAFETY: signal 0 only checks that `pid` can be signalled.
    let signalled = unsafe { libc::kill(pid, 0) } == 0;
    signalled || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

#[cfg(unix)]
async fn run(
    shell: &str,
    capture: ShellCapture,
    home: Option<&std::ffi::OsStr>,
) -> Result<ShellEnvironment, String> {
    use std::{os::unix::fs::OpenOptionsExt as _, process::Stdio};
    use tokio::io::AsyncReadExt as _;

    let mut bytes = [0_u8; 16];
    getrandom::getrandom(&mut bytes).map_err(|error| error.to_string())?;
    let marker = format!(
        "__cookie_env_{}__",
        bytes
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    );
    let snapshot = if is_bash(shell) {
        remove_stale_snapshots(&std::env::temp_dir());
        let directory = tempfile::Builder::new()
            .prefix(&format!("{SNAPSHOT_PREFIX}{}-", std::process::id()))
            .tempdir()
            .map_err(|error| format!("no directory for the bash snapshot: {error}"))?;
        let path = directory.path().join("snapshot.sh");
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .map_err(|error| format!("no bash snapshot file: {error}"))?;
        Some(Snapshot {
            path,
            _directory: directory,
        })
    } else {
        None
    };
    let mut command = tokio::process::Command::new(shell);
    command.arg("-l");
    if capture == ShellCapture::Interactive {
        command.arg("-i");
    }
    command
        .arg("-c")
        .arg(script(&marker, snapshot.is_some()))
        .arg("cookie-shell-capture");
    if let Some(snapshot) = &snapshot {
        command.arg(&snapshot.path);
    }
    command
        .env(CAPTURE_VARIABLE, "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    if let Some(home) = home {
        command.env("HOME", home);
        if Path::new(home).is_dir() {
            command.current_dir(home);
        }
    }
    // Its own session: no controlling terminal for an interactive shell to
    // take over or read from, and one process group to kill.
    // SAFETY: `setsid` is async-signal-safe and touches no memory.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
    let mut child = command
        .spawn()
        .map_err(|error| format!("`{shell}` did not start: {error}"))?;
    let group = child.id().and_then(|id| libc::pid_t::try_from(id).ok());
    // Kills what the shell left running when the capture ends, timed out
    // and dropped included.
    let _group = KillGroup(group);
    let mut stdout = child.stdout.take().ok_or("the shell's output is missing")?;
    let mut output = Vec::new();
    let mut chunk = [0_u8; 64 * 1024];
    // Read until the second marker rather than to the end: a process an rc
    // file starts in the background may hold the output open.
    let entries = loop {
        if let Some(entries) = between_markers(&output, marker.as_bytes()) {
            break entries.to_vec();
        }
        let read = stdout
            .read(&mut chunk)
            .await
            .map_err(|error| format!("the shell's output was not readable: {error}"))?;
        if read == 0 {
            let status = child.wait().await.ok();
            return Err(match status.and_then(|status| status.code()) {
                Some(code) => {
                    format!("the shell exited with status {code} before printing its environment")
                }
                None => "the shell exited before printing its environment".to_owned(),
            });
        }
        output.extend_from_slice(&chunk[..read]);
        if output.len() > MAX_OUTPUT_BYTES {
            return Err("the shell printed too much".to_owned());
        }
    };
    drop(stdout);
    Ok(ShellEnvironment {
        variables: parse_variables(&entries),
        snapshot,
    })
}

#[cfg(unix)]
struct KillGroup(Option<libc::pid_t>);

#[cfg(unix)]
impl Drop for KillGroup {
    fn drop(&mut self) {
        if let Some(group) = self.0 {
            // SAFETY: signals a process group this capture created; a
            // group already gone is an ignored error.
            unsafe {
                libc::killpg(group, libc::SIGKILL);
            }
        }
    }
}

/// What `output` holds between the first two `marker`s, once both arrived.
fn between_markers<'a>(output: &'a [u8], marker: &[u8]) -> Option<&'a [u8]> {
    let start = memchr::memmem::find(output, marker)? + marker.len();
    let length = memchr::memmem::find(&output[start..], marker)?;
    Some(&output[start..start + length])
}

/// The variables of `env -0` output, without those
/// [`captured_name`] leaves out and entries that are not `NAME=value`.
fn parse_variables(entries: &[u8]) -> Vec<(OsString, OsString)> {
    entries
        .split(|byte| *byte == 0)
        .filter_map(|entry| {
            let equals = entry.iter().position(|byte| *byte == b'=')?;
            let (name, value) = (&entry[..equals], &entry[equals + 1..]);
            if name.is_empty() {
                return None;
            }
            let name = os_string(name);
            captured_name(&name).then(|| (name, os_string(value)))
        })
        .collect()
}

#[cfg(unix)]
fn os_string(bytes: &[u8]) -> OsString {
    use std::os::unix::ffi::OsStringExt as _;
    OsString::from_vec(bytes.to_vec())
}

#[cfg(not(unix))]
fn os_string(bytes: &[u8]) -> OsString {
    OsString::from(String::from_utf8_lossy(bytes).into_owned())
}

#[cfg(test)]
#[path = "shell_environment/tests.rs"]
mod tests;
