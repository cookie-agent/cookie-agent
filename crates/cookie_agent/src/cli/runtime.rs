//! `cookie runtime`: serving this machine's coding runtime over stdio or
//! dialed in to a daemon, and the MCP bridge ACP agents start.

use std::{path::PathBuf, time::Duration};

use anyhow::Context as _;
use clap::Subcommand;
use tokio_util::sync::CancellationToken;

use super::daemon::stop_on_signal;
use super::{DaemonTarget, daemon_target, host_name};

#[derive(Debug, PartialEq, Eq, Subcommand)]
pub(super) enum RuntimeCommand {
    /// Serve this machine's runtime: the `coding` capability (`read`,
    /// `write`, `edit`, `bash`, `webfetch`) for every workspace sessions
    /// attach.
    #[command(group(clap::ArgGroup::new("transport").required(true).args(["stdio", "daemon"])))]
    Serve {
        /// Speak the runtime protocol over standard input and output, one
        /// JSON-RPC message per line, as a template starts it.
        #[arg(long)]
        stdio: bool,
        /// Dial in to a daemon and stay connected, reconnecting after drops:
        /// the daemon at this client URL (`ws://` or `wss://` ending in
        /// `/ws`), or, without one, `[server] url` in this machine's
        /// `config.toml`, else the local daemon.
        #[arg(long, value_name = "URL", num_args = 0..=1)]
        daemon: Option<Option<String>>,
        /// The runtime ID to dial in as; this machine's host name when
        /// omitted.
        #[arg(long, value_name = "RUNTIME_ID", conflicts_with = "stdio")]
        id: Option<String>,
        /// The daemon's bearer token, for `--daemon`; `[server] token` in
        /// this machine's `config.toml` when omitted.
        #[arg(
            long,
            env = "COOKIE_DAEMON_TOKEN",
            hide_env_values = true,
            allow_hyphen_values = true
        )]
        token: Option<String>,
        /// Limit `coding` to workspaces under this directory; repeat for
        /// several. A leading `~` is expanded. Any workspace when omitted.
        #[arg(long = "root", value_name = "DIR")]
        roots: Vec<PathBuf>,
    },
    /// Connect an ACP agent's MCP client to cookie's tools: copy standard
    /// input to the runtime's bridge socket and the socket to standard
    /// output. The runtime hands this command to the agents it hosts; it
    /// is not meant to be run by hand.
    McpBridge {
        /// The bridge socket the runtime opened for the agent's session.
        #[arg(long, value_name = "PATH")]
        socket: PathBuf,
    },
}

/// `cookie runtime serve --stdio`: serves one coding runtime for the
/// workspace over this process's stdio until the daemon sends
/// `runtime/shutdown` or closes standard input. Standard output carries only
/// protocol messages; errors go to standard error.
pub(super) async fn run_runtime(command: RuntimeCommand) -> anyhow::Result<()> {
    let (stdio, daemon, id, token, roots) = match command {
        RuntimeCommand::Serve {
            stdio,
            daemon,
            id,
            token,
            roots,
        } => (stdio, daemon, id, token, roots),
        RuntimeCommand::McpBridge { socket } => {
            return cookie_agent_coding_runtime::acp_bridge::run_bridge(&socket)
                .await
                .with_context(|| format!("MCP bridge at {}", socket.display()));
        }
    };
    let roots = roots
        .iter()
        .map(|root| {
            let expanded = cookie_agent_coding_runtime::server::expand_home(root);
            let root = cookie_agent_protocol::paths::canonicalize(&expanded)
                .with_context(|| format!("root {} is unavailable", expanded.display()))?;
            anyhow::ensure!(root.is_dir(), "root {} is not a directory", root.display());
            Ok(root)
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    if !stdio && let Some(daemon) = daemon {
        let DaemonTarget { url: daemon, token } = daemon_target(daemon, token)?;
        let id = id.unwrap_or_else(|| host_runtime_id("", &host_name()));
        dial_in_id(&id)?;
        cookie_agent_protocol::runtime_dial_in_url(&daemon, &id)?;
        // A runtime that dials in hosts the MCP servers and ACP agents the
        // daemon's `[runtimes.<id>]` gives it, never this machine's own.
        warn_local_runtime_settings(&id);
        let stop = CancellationToken::new();
        stop_on_signal(stop.clone())?;
        return dial_in(
            &daemon,
            &token,
            &id,
            cookie_agent_coding_runtime::server::ServeOptions {
                roots,
                stop,
                ..Default::default()
            },
        )
        .await;
    }
    let stop = CancellationToken::new();
    stop_on_signal(stop.clone())?;
    cookie_agent_coding_runtime::server::serve_stdio(
        cookie_agent_coding_runtime::server::ServeOptions {
            roots,
            stop,
            ..Default::default()
        },
    )
    .await;
    // The blocking standard-input reader may still wait for a line that
    // never comes; exit without waiting for it.
    std::process::exit(0)
}

/// A runtime ID for this machine: `<prefix>_<host>`, or the host name alone
/// without a prefix, with the host name lowercased, every character outside
/// `[a-z0-9_]` replaced by `_`, repeats and edges trimmed, at most 32 bytes.
/// A bare host name that does not start with a letter gets `host_` in front;
/// no host name at all gives the prefix, or `host`.
pub(super) fn host_runtime_id(prefix: &str, host: &str) -> String {
    let mut name = String::new();
    for character in host.chars() {
        let character = character.to_ascii_lowercase();
        let character = if character.is_ascii_lowercase() || character.is_ascii_digit() {
            character
        } else {
            '_'
        };
        if character == '_' && (name.is_empty() || name.ends_with('_')) {
            continue;
        }
        name.push(character);
    }
    let name = name.trim_end_matches('_');
    let mut id = match (prefix, name) {
        ("", "") => "host".to_owned(),
        (prefix, "") => prefix.to_owned(),
        ("", name) if name.starts_with(|character: char| character.is_ascii_lowercase()) => {
            name.to_owned()
        }
        ("", name) => format!("host_{name}"),
        (prefix, name) => format!("{prefix}_{name}"),
    };
    id.truncate(32);
    id.truncate(id.trim_end_matches('_').len());
    id
}

/// Says once that this machine's `[runtimes.local]` MCP servers and ACP
/// agents, which only its own daemon's built-in runtime hosts, are not the
/// ones runtime `id` dials in with
/// (`docs/site/specs/runtime-management.md` section 3.2).
fn warn_local_runtime_settings(id: &str) {
    let Ok(config) = cookie_agent_config::load() else {
        return;
    };
    let local = config
        .runtimes
        .iter()
        .find(|(runtime, _)| runtime.as_str() == cookie_agent_protocol::RuntimeId::LOCAL)
        .map(|(_, template)| template);
    if local.is_some_and(|local| !local.mcp_servers.is_empty() || !local.acp_agents.is_empty()) {
        eprintln!(
            "cookie runtime: this machine's [runtimes.local] MCP servers and ACP agents are its \
             own daemon's; runtime `{id}` hosts the ones [runtimes.{id}] gives it in the \
             configuration of the daemon it dials in to"
        );
    }
}

/// Checks `id` as a runtime ID a runtime may dial in under: well formed
/// and not a built-in runtime's, which the daemon refuses.
fn dial_in_id(id: &str) -> anyhow::Result<()> {
    let runtime_id = cookie_agent_protocol::RuntimeId::new(id).map_err(|_| {
        anyhow::anyhow!("invalid runtime ID `{id}`: it must match [a-z][a-z0-9_]{{0,31}}")
    })?;
    anyhow::ensure!(
        !runtime_id.is_reserved(),
        "runtime ID `{id}` is reserved for the daemon's built-in runtime; choose another with --id"
    );
    Ok(())
}

/// Dials in to the daemon at `daemon` as runtime `id` and serves it,
/// connecting again after every drop or refusal for now with a growing
/// wait, until stopped. A connection that has carried nothing, not even a
/// pong to its keepalive pings, for `DIAL_IN_DEAD_AFTER` counts as a drop,
/// so one that died without a close is noticed, and so does a connect that
/// the daemon has not answered within `CONNECT_TIMEOUT` (a peer that takes
/// the TCP connection and never upgrades it). A refusal for good, such as
/// a configured or reserved ID or a wrong token, ends it with the daemon's
/// reason.
pub(super) async fn dial_in(
    daemon: &str,
    token: &str,
    id: &str,
    options: cookie_agent_coding_runtime::server::ServeOptions,
) -> anyhow::Result<()> {
    const LONGEST_WAIT: Duration = Duration::from_secs(30);
    let stop = options.stop.clone();
    let mut waits =
        cookie_agent_identity::Backoff::new(Duration::from_secs(1), LONGEST_WAIT).delays();
    let mut wait = waits.next_delay();
    loop {
        let connected = tokio::select! {
            () = stop.cancelled() => return Ok(()),
            connected = cookie_agent_protocol::WebSocketTransport::connect_runtime(daemon, token, id) => connected,
        };
        match connected {
            Ok(transport) => {
                eprintln!("cookie runtime: connected to {daemon} as `{id}`");
                let started = std::time::Instant::now();
                let reason = cookie_agent_coding_runtime::server::serve(
                    Box::new(transport),
                    options.clone(),
                )
                .await;
                if stop.is_cancelled() {
                    return Ok(());
                }
                eprintln!("cookie runtime: disconnected from {daemon}: {reason}");
                if started.elapsed() > LONGEST_WAIT {
                    waits.reset();
                    wait = waits.next_delay();
                }
            }
            Err(error) if error.is_permanent() => {
                anyhow::bail!("cannot dial in to {daemon} as `{id}`: {error}");
            }
            Err(error) => eprintln!(
                "cookie runtime: cannot dial in to {daemon} as `{id}`: {error}; retrying in {} s",
                wait.as_secs()
            ),
        }
        tokio::select! {
            () = tokio::time::sleep(wait) => {}
            () = stop.cancelled() => return Ok(()),
        }
        wait = waits.next_delay();
    }
}
