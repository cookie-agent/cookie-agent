//! Restarting the daemon (`docs/site/specs/engine-configuration.md`
//! section 11): `cookie daemon restart`, and how `cookie daemon` relaunches
//! itself once a client asked it to.
//!
//! The daemon remembers how it was started — its executable, arguments,
//! environment, and directory — before it changes any of them. When a
//! scheduled restart is due it shuts down as on a stop signal and then
//! relaunches that command:
//!
//! - on Unix with an in-place `exec`, which keeps the process ID, so a
//!   service manager sees no change. Every descriptor the daemon opened is
//!   close-on-exec (Rust opens them so, and [`relaunch`] marks any other
//!   before the `exec`), so the new image starts with only standard input,
//!   output, and error, and binds the port the old one released;
//! - on Windows by starting the same command line and exiting. The new
//!   process finds [`RESTARTED_ENV`] and retries binding its port for up to
//!   [`RESTART_BIND_WAIT`] while the old one exits.
//!
//! The executable is the path the daemon resolved when it started, so a
//! restart after an upgrade runs the new build.

use std::{
    ffi::OsString,
    path::PathBuf,
    time::{Duration, Instant},
};

use anyhow::Context as _;
use clap::Subcommand;
use cookie_agent_client_state::restart::waiting_for;
use cookie_agent_protocol::{Client, ClientDelivery, DaemonRestartResult, RestartWhen};

/// Set in the environment of the process a restart starts, which removes it
/// at once: the daemon then retries binding its port while the process it
/// replaces lets go of it.
pub(crate) const RESTARTED_ENV: &str = "COOKIE_DAEMON_RESTARTED";

/// How long a restarted daemon retries binding a port still in use.
pub(crate) const RESTART_BIND_WAIT: Duration = Duration::from_secs(30);

/// How long `cookie daemon restart` waits for the daemon to come back once
/// it closed the connection.
const RECONNECT_WAIT: Duration = Duration::from_secs(60);

#[derive(Debug, PartialEq, Eq, Subcommand)]
pub(crate) enum DaemonCommand {
    /// Restart the daemon once no turn is running, or at once with `--now`.
    /// It relaunches its own executable with the same arguments and
    /// environment, applying the configuration that needs a restart and a
    /// binary upgraded on disk.
    Restart {
        /// Restart at once, interrupting running turns.
        #[arg(long)]
        now: bool,
        /// The daemon's client URL; `[server] url` in this machine's
        /// `config.toml`, or the local daemon, when omitted.
        #[arg(long)]
        url: Option<String>,
        /// The daemon's bearer token; `[server] token` in this machine's
        /// `config.toml` when omitted.
        #[arg(
            long,
            env = "COOKIE_DAEMON_TOKEN",
            hide_env_values = true,
            allow_hyphen_values = true
        )]
        token: Option<String>,
    },
}

/// How this process was started, to start it again.
#[derive(Debug)]
pub(crate) struct Relaunch {
    executable: PathBuf,
    arguments: Vec<OsString>,
    environment: Vec<(OsString, OsString)>,
    directory: PathBuf,
}

impl Relaunch {
    /// This process's command, before anything changes it; `None` when the
    /// executable or the directory cannot be found, and the daemon then
    /// refuses to restart. [`RESTARTED_ENV`] is left out of the environment.
    pub(crate) fn capture() -> Option<Self> {
        let executable = std::env::current_exe().ok()?;
        let directory = std::env::current_dir().ok()?;
        Some(Self {
            executable,
            arguments: std::env::args_os().collect(),
            environment: std::env::vars_os()
                .filter(|(name, _)| name != RESTARTED_ENV)
                .collect(),
            directory,
        })
    }

    fn command(&self) -> std::process::Command {
        let mut command = std::process::Command::new(&self.executable);
        command
            .args(self.arguments.iter().skip(1))
            .env_clear()
            .envs(self.environment.iter().map(|(name, value)| (name, value)))
            .env(RESTARTED_ENV, "1")
            .current_dir(&self.directory);
        command
    }
}

/// Starts the daemon again as `relaunch` says, replacing this process. Only
/// returns, with the reason, when that fails.
#[cfg(unix)]
pub(crate) fn relaunch(relaunch: &Relaunch) -> anyhow::Error {
    use std::os::unix::process::CommandExt as _;
    flush_standard_streams();
    mark_descriptors_close_on_exec();
    let mut command = relaunch.command();
    if let Some(name) = relaunch.arguments.first() {
        command.arg0(name);
    }
    let error = command.exec();
    anyhow::Error::new(error).context(format!("relaunch {}", relaunch.executable.display()))
}

/// Starts the daemon again as `relaunch` says and exits; only returns, with
/// the reason, when starting it fails.
#[cfg(windows)]
pub(crate) fn relaunch(relaunch: &Relaunch) -> anyhow::Error {
    flush_standard_streams();
    match relaunch.command().spawn() {
        Ok(_) => std::process::exit(0),
        Err(error) => {
            anyhow::Error::new(error).context(format!("relaunch {}", relaunch.executable.display()))
        }
    }
}

fn flush_standard_streams() {
    use std::io::Write as _;
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
}

/// Marks every descriptor above standard error close-on-exec, so nothing
/// the daemon held open, such as a socket a library opened without the
/// flag, survives into the relaunched image.
#[cfg(unix)]
fn mark_descriptors_close_on_exec() {
    let listing = if cfg!(target_os = "linux") {
        "/proc/self/fd"
    } else {
        "/dev/fd"
    };
    let Ok(entries) = std::fs::read_dir(listing) else {
        return;
    };
    let descriptors = entries
        .filter_map(|entry| {
            entry
                .ok()?
                .file_name()
                .to_str()?
                .parse::<libc::c_int>()
                .ok()
        })
        .filter(|descriptor| *descriptor > libc::STDERR_FILENO)
        .collect::<Vec<_>>();
    for descriptor in descriptors {
        // SAFETY: `F_GETFD` and `F_SETFD` read and set one descriptor's flags
        // and touch no memory; a descriptor closed meanwhile (the listing's
        // own) fails harmlessly.
        unsafe {
            let flags = libc::fcntl(descriptor, libc::F_GETFD);
            if flags >= 0 && flags & libc::FD_CLOEXEC == 0 {
                libc::fcntl(descriptor, libc::F_SETFD, flags | libc::FD_CLOEXEC);
            }
        }
    }
}

/// Binds with `bind` until it succeeds, an error other than the address
/// being in use, or `wait` passes.
pub(crate) async fn bind_retrying<T, F, Fut>(
    wait: Duration,
    mut bind: F,
) -> Result<T, cookie_agent_server::ServerError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, cookie_agent_server::ServerError>>,
{
    let deadline = Instant::now() + wait;
    loop {
        match bind().await {
            Err(cookie_agent_server::ServerError::Listen(error))
                if error.kind() == std::io::ErrorKind::AddrInUse && Instant::now() < deadline =>
            {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            result => return result,
        }
    }
}

/// What the daemon answered, as a line.
pub(crate) fn scheduled_line(when: RestartWhen, result: &DaemonRestartResult) -> String {
    let waiting = waiting_for(result.active_runs, result.background_calls);
    match (when, waiting.is_empty()) {
        (_, true) => "The daemon is restarting.".to_owned(),
        (RestartWhen::Now, false) => {
            format!("The daemon is restarting now, interrupting {waiting}.")
        }
        (_, false) => format!(
            "The daemon restarts once {waiting} finish{}. New turns are refused until then.",
            if result.active_runs + result.background_calls == 1 {
                "es"
            } else {
                ""
            }
        ),
    }
}

/// `cookie daemon restart`: asks the daemon to restart, then waits until it
/// is back.
pub(crate) async fn run_restart(url: &str, token: &str, now: bool) -> anyhow::Result<()> {
    let client = Client::connect_websocket_with_token(url, token).await?;
    let mut deliveries = client
        .subscribe_deliveries()
        .context("the connection's deliveries")?;
    let hello = client
        .handshake_as(crate::client_identity("cli"))
        .await
        .context("handshake with daemon")?;
    let when = if now {
        RestartWhen::Now
    } else {
        RestartWhen::Idle
    };
    let result = client
        .restart_daemon(when)
        .await
        .context("restart the daemon")?;
    println!("{}", scheduled_line(when, &result));
    let mut waiting_for_now = (result.active_runs, result.background_calls);
    if waiting_for_now != (0, 0) && when == RestartWhen::Idle {
        println!(
            "Waiting; Ctrl-C stops waiting, and the restart stays scheduled \
             (`cookie daemon restart --now` restarts at once)."
        );
    }
    while let Some(delivery) = deliveries.recv().await {
        match delivery {
            ClientDelivery::DaemonRestarting(restarting) => {
                let now = (restarting.active_runs, restarting.background_calls);
                if now != waiting_for_now && now != (0, 0) {
                    waiting_for_now = now;
                    println!("Waiting for {}.", waiting_for(now.0, now.1));
                }
            }
            ClientDelivery::Disconnected { .. } => break,
            _ => {}
        }
    }
    drop(client);
    let deadline = Instant::now() + RECONNECT_WAIT;
    loop {
        tokio::time::sleep(Duration::from_millis(250)).await;
        match reconnect(url, token).await {
            Ok(engine_version) => {
                if engine_version == hello.engine_version {
                    println!("The daemon is back.");
                } else {
                    println!(
                        "The daemon is back, now version {engine_version} (was {}).",
                        hello.engine_version
                    );
                }
                return Ok(());
            }
            Err(error) if Instant::now() >= deadline => {
                return Err(error.context(format!(
                    "the daemon did not come back within {} seconds",
                    RECONNECT_WAIT.as_secs()
                )));
            }
            Err(_) => {}
        }
    }
}

/// Connects again; the daemon's version.
async fn reconnect(url: &str, token: &str) -> anyhow::Result<String> {
    let client = Client::connect_websocket_with_token(url, token).await?;
    let hello = client.handshake_as(crate::client_identity("cli")).await?;
    client.shutdown();
    Ok(hello.engine_version)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use cookie_agent_protocol::{DaemonRestartResult, RestartWhen};

    use super::{bind_retrying, scheduled_line};

    #[test]
    fn the_answer_says_what_the_restart_waits_for() {
        let result = |active_runs| DaemonRestartResult {
            scheduled: true,
            active_runs,
            background_calls: 0,
        };
        assert_eq!(
            scheduled_line(RestartWhen::Idle, &result(0)),
            "The daemon is restarting."
        );
        assert_eq!(
            scheduled_line(RestartWhen::Idle, &result(1)),
            "The daemon restarts once 1 running turn finishes. New turns are refused until then."
        );
        assert_eq!(
            scheduled_line(RestartWhen::Idle, &result(3)),
            "The daemon restarts once 3 running turns finish. New turns are refused until then."
        );
        assert_eq!(
            scheduled_line(RestartWhen::Now, &result(2)),
            "The daemon is restarting now, interrupting 2 running turns."
        );
        let background = DaemonRestartResult {
            scheduled: true,
            active_runs: 1,
            background_calls: 2,
        };
        assert_eq!(
            scheduled_line(RestartWhen::Idle, &background),
            "The daemon restarts once 1 running turn, 2 background calls finish. New turns are \
             refused until then."
        );
        assert_eq!(
            scheduled_line(
                RestartWhen::Idle,
                &DaemonRestartResult {
                    scheduled: true,
                    active_runs: 0,
                    background_calls: 1,
                }
            ),
            "The daemon restarts once 1 background call finishes. New turns are refused until \
             then."
        );
    }

    async fn bind(port: u16) -> Result<tokio::net::TcpListener, cookie_agent_server::ServerError> {
        tokio::net::TcpListener::bind(("127.0.0.1", port))
            .await
            .map_err(cookie_agent_server::ServerError::Listen)
    }

    /// A restarted daemon waits for its port to free; a daemon started
    /// anew fails at once on a port in use.
    #[tokio::test]
    async fn binding_retries_a_port_in_use_only_while_waiting() {
        let held = std::net::TcpListener::bind("127.0.0.1:0").expect("hold a port");
        let port = held.local_addr().expect("address").port();
        let refused = bind_retrying(Duration::ZERO, || bind(port)).await;
        assert!(
            matches!(&refused, Err(cookie_agent_server::ServerError::Listen(error))
                if error.kind() == std::io::ErrorKind::AddrInUse),
            "{refused:?}"
        );
        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            drop(held);
        });
        bind_retrying(Duration::from_secs(10), || bind(port))
            .await
            .expect("bound once the port freed");
        release.join().expect("released");
    }
}
