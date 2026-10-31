//! The `cookie` command line: its arguments, and the dispatch to the
//! module of each subcommand.

use std::{
    env,
    io::{self, IsTerminal},
    path::PathBuf,
};

use anyhow::Context as _;
use clap::{Parser, Subcommand};
use cookie_agent::run;
use cookie_agent_protocol::paths;
use cookie_agent_protocol_transport::validate_websocket_url;
use zeroize::Zeroizing;

use crate::{Launch, daemon_restart};

mod agents;
mod compose;
mod config;
mod connect;
mod cron;
mod daemon;
mod frontend;
mod headless;
mod machine;
mod mcp;
mod memory;
mod onboard;
mod project;
mod runtime;
mod sessions;
mod skills;
pub(crate) mod update;
mod web;

#[cfg(all(test, unix))]
mod tests;

#[derive(Debug, Parser)]
#[command(name = "cookie", version = cookie_agent_engine::release::long_version())]
pub(crate) struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
    /// When the daemon refuses a request, also print the technical detail of
    /// its error, such as the upstream cause or a path.
    #[arg(long, global = true)]
    pub(crate) error_details: bool,
    /// Run the daemon inside this process even when one is already running
    /// on this machine, without looking for it. Plain `cookie` only.
    #[arg(long)]
    standalone: bool,
    /// Start a new session in this project: it takes the project's default
    /// agent and runtimes. Plain `cookie` only; nothing is picked from the
    /// current directory.
    #[arg(long, value_name = "NAME", value_parser = project::parse_project_name)]
    project: Option<cookie_agent_protocol::ProjectName>,
}

#[derive(Debug, PartialEq, Eq, Subcommand)]
enum Command {
    /// Run one prompt without the interactive TUI.
    Run {
        #[command(flatten)]
        args: Box<run::RunArgs>,
    },
    /// Run the daemon: serve the client protocol, dial-in runtimes, and the
    /// web page on loopback at `[server] port`, or on `--listen`.
    #[command(args_conflicts_with_subcommands = true)]
    Daemon {
        #[command(subcommand)]
        command: Option<daemon_restart::DaemonCommand>,
        /// Loopback port to bind instead of `[server] port`. `0` selects an
        /// ephemeral port; the ready line reports the real one.
        #[arg(long, conflicts_with = "listen")]
        port: Option<u16>,
        /// Address and port to bind, such as `0.0.0.0:7419`, for clients and
        /// runtimes on other machines. Without TLS the token crosses the
        /// network in the clear: use it on a private network.
        #[arg(long, value_name = "ADDR:PORT")]
        listen: Option<std::net::SocketAddr>,
        /// PEM certificate chain to serve `wss` with; needs `--tls-key`.
        #[arg(long, value_name = "PATH", requires = "tls_key")]
        tls_cert: Option<PathBuf>,
        /// PEM private key of `--tls-cert`.
        #[arg(long, value_name = "PATH", requires = "tls_cert")]
        tls_key: Option<PathBuf>,
    },
    /// Attach the TUI to an existing daemon.
    Attach {
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
    /// Print the daemon's web page with its token, `#token=<token>`, for a
    /// browser to open.
    Web {
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
        /// Also open the page in the system browser.
        #[arg(long)]
        open: bool,
    },
    /// Securely create or update a durable global managed-provider connection.
    Connect {
        /// Exact runtime provider ID. Omit to choose interactively.
        provider_id: Option<String>,
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
    /// Remove a durable global managed-provider connection.
    Disconnect {
        /// Exact runtime provider ID. Omit to choose interactively.
        provider_id: Option<String>,
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
    /// Serve a runtime to a daemon over the runtime protocol, as a
    /// `[runtimes.<id>]` template runs it. Needs no provider configuration.
    Runtime {
        #[command(subcommand)]
        command: runtime::RuntimeCommand,
    },
    /// Set up a daemon: connect a provider and choose the default, small,
    /// and utility models. The daemon writes everything, on its own
    /// machine, and fetches its official agents.
    Onboard {
        #[command(flatten)]
        args: onboard::OnboardArgs,
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
    /// Read and change a daemon's configuration: `config.toml` and the
    /// agent documents.
    Config {
        #[command(subcommand)]
        command: config::ConfigCommand,
        /// The daemon's client URL; `[server] url` in this machine's
        /// `config.toml`, or the local daemon, when omitted.
        #[arg(long, global = true)]
        url: Option<String>,
        /// The daemon's bearer token; `[server] token` in this machine's
        /// `config.toml` when omitted.
        #[arg(
            long,
            global = true,
            env = "COOKIE_DAEMON_TOKEN",
            hide_env_values = true,
            allow_hyphen_values = true
        )]
        token: Option<String>,
    },
    /// List the machines a daemon's runtimes run on, and rename, regroup,
    /// merge, or forget them.
    Machine {
        #[command(subcommand)]
        command: machine::MachineCommand,
        /// The daemon's client URL; `[server] url` in this machine's
        /// `config.toml`, or the local daemon, when omitted.
        #[arg(long, global = true)]
        url: Option<String>,
        /// The daemon's bearer token; `[server] token` in this machine's
        /// `config.toml` when omitted.
        #[arg(
            long,
            global = true,
            env = "COOKIE_DAEMON_TOKEN",
            hide_env_values = true,
            allow_hyphen_values = true
        )]
        token: Option<String>,
    },
    /// List, create, rename, delete, and edit a daemon's projects: named
    /// bodies of work that sessions belong to.
    Project {
        #[command(subcommand)]
        command: project::ProjectCommand,
        /// The daemon's client URL; `[server] url` in this machine's
        /// `config.toml`, or the local daemon, when omitted.
        #[arg(long, global = true)]
        url: Option<String>,
        /// The daemon's bearer token; `[server] token` in this machine's
        /// `config.toml` when omitted.
        #[arg(
            long,
            global = true,
            env = "COOKIE_DAEMON_TOKEN",
            hide_env_values = true,
            allow_hyphen_values = true
        )]
        token: Option<String>,
    },
    /// List, edit, and delete a daemon's memories: what agents and the user
    /// keep about the user, agents, machines, and projects.
    Memory {
        #[command(subcommand)]
        command: memory::MemoryCommand,
        /// The daemon's client URL; `[server] url` in this machine's
        /// `config.toml`, or the local daemon, when omitted.
        #[arg(long, global = true)]
        url: Option<String>,
        /// The daemon's bearer token; `[server] token` in this machine's
        /// `config.toml` when omitted.
        #[arg(
            long,
            global = true,
            env = "COOKIE_DAEMON_TOKEN",
            hide_env_values = true,
            allow_hyphen_values = true
        )]
        token: Option<String>,
    },
    /// List, create, edit, run, and delete a daemon's cron jobs: prompts
    /// it fires on a schedule into a session or a new session.
    Cron {
        #[command(subcommand)]
        command: cron::CronCommand,
        /// The daemon's client URL; `[server] url` in this machine's
        /// `config.toml`, or the local daemon, when omitted.
        #[arg(long, global = true)]
        url: Option<String>,
        /// The daemon's bearer token; `[server] token` in this machine's
        /// `config.toml` when omitted.
        #[arg(
            long,
            global = true,
            env = "COOKIE_DAEMON_TOKEN",
            hide_env_values = true,
            allow_hyphen_values = true
        )]
        token: Option<String>,
    },
    /// List a daemon's skills, refresh its official skills, or check
    /// skills before publishing them.
    Skills {
        #[command(subcommand)]
        command: skills::SkillsCommand,
        /// The daemon's client URL; `[server] url` in this machine's
        /// `config.toml`, or the local daemon, when omitted.
        #[arg(long, global = true)]
        url: Option<String>,
        /// The daemon's bearer token; `[server] token` in this machine's
        /// `config.toml` when omitted.
        #[arg(
            long,
            global = true,
            env = "COOKIE_DAEMON_TOKEN",
            hide_env_values = true,
            allow_hyphen_values = true
        )]
        token: Option<String>,
    },
    /// List a daemon's agents, refresh its official agents, or check agent
    /// documents before publishing them.
    Agents {
        #[command(subcommand)]
        command: agents::AgentsCommand,
        /// The daemon's client URL; `[server] url` in this machine's
        /// `config.toml`, or the local daemon, when omitted.
        #[arg(long, global = true)]
        url: Option<String>,
        /// The daemon's bearer token; `[server] token` in this machine's
        /// `config.toml` when omitted.
        #[arg(
            long,
            global = true,
            env = "COOKIE_DAEMON_TOKEN",
            hide_env_values = true,
            allow_hyphen_values = true
        )]
        token: Option<String>,
    },
    /// Replace this binary with the newest release on its channel, stable
    /// or nightly.
    #[command(visible_alias = "upgrade")]
    Update(update::UpdateArgs),
    /// Manage the session history this machine stores.
    Sessions {
        #[command(subcommand)]
        command: sessions::SessionsCommand,
    },
    /// List MCP servers or start remote-server OAuth.
    Mcp {
        #[command(subcommand)]
        command: mcp::McpCommand,
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

/// Installs a tracing subscriber only when `RUST_LOG` is set, writing to
/// stderr: stdout carries `cookie run` output and the daemon ready line, and
/// the TUI owns the terminal.
fn init_logging() {
    if env::var_os(tracing_subscriber::EnvFilter::DEFAULT_ENV).is_none() {
        return;
    }
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(io::stderr)
        .with_ansi(io::stderr().is_terminal())
        .init();
}

pub(crate) async fn dispatch(cli: Cli, launch: &mut Launch) -> anyhow::Result<()> {
    init_logging();
    let Cli {
        command,
        standalone,
        project,
        ..
    } = cli;
    if standalone && command.is_some() {
        clap::Error::raw(
            clap::error::ErrorKind::ArgumentConflict,
            "--standalone applies to plain `cookie` only, not to a subcommand\n",
        )
        .exit();
    }
    if project.is_some() && command.is_some() {
        clap::Error::raw(
            clap::error::ErrorKind::ArgumentConflict,
            "--project applies to plain `cookie` only, not to a subcommand\n",
        )
        .exit();
    }
    if let Some(Command::Run { args }) = &command
        && let Err(message) = args.validate_cli()
    {
        clap::Error::raw(clap::error::ErrorKind::ArgumentConflict, message).exit();
    }
    let workspace = if matches!(&command, Some(Command::Run { .. })) {
        None
    } else {
        local_workspace(&command, env::current_dir)?
    };
    match command {
        Some(Command::Run { args }) => headless::run_headless(*args).await,
        Some(Command::Daemon {
            command: Some(daemon_restart::DaemonCommand::Restart { now, url, token }),
            ..
        }) => {
            let target = daemon_target(url, token)?;
            daemon_restart::run_restart(&target.url, &target.token, now).await
        }
        Some(Command::Daemon {
            command: None,
            port,
            listen,
            tls_cert,
            tls_key,
        }) => {
            let workspace = workspace.expect("daemon workspace");
            daemon::serve(&workspace, port, listen, tls_cert.zip(tls_key), launch).await
        }
        Some(Command::Connect {
            provider_id,
            url,
            token,
        }) => {
            let target = daemon_target(url, token)?;
            connect::run_connect(&target.url, &target.token, provider_id).await
        }
        Some(Command::Disconnect {
            provider_id,
            url,
            token,
        }) => {
            let target = daemon_target(url, token)?;
            connect::run_disconnect(&target.url, &target.token, provider_id).await
        }
        Some(Command::Web { url, token, open }) => web::run_web(url, token, open),
        Some(Command::Mcp {
            command,
            url,
            token,
        }) => {
            let target = daemon_target(url, token)?;
            mcp::run_mcp(&target.url, &target.token, command).await
        }
        Some(Command::Runtime { command }) => runtime::run_runtime(command).await,
        Some(Command::Update(args)) => update::run_update(args).await,
        Some(Command::Sessions { command }) => {
            sessions::run_sessions(command, workspace.as_deref().expect("sessions workspace"))
        }
        Some(Command::Onboard { args, url, token }) => {
            let target = daemon_target(url, token)?;
            onboard::run_onboard(&target.url, &target.token, args).await
        }
        Some(Command::Config {
            command,
            url,
            token,
        }) => {
            let target = daemon_target(url, token)?;
            config::run_config(&target.url, &target.token, command).await
        }
        Some(Command::Machine {
            command,
            url,
            token,
        }) => {
            let target = daemon_target(url, token)?;
            machine::run_machine(&target.url, &target.token, command).await
        }
        Some(Command::Memory {
            command,
            url,
            token,
        }) => {
            let target = daemon_target(url, token)?;
            memory::run_memory(&target.url, &target.token, command).await
        }
        Some(Command::Project {
            command,
            url,
            token,
        }) => {
            let target = daemon_target(url, token)?;
            project::run_project(&target.url, &target.token, command).await
        }
        Some(Command::Agents {
            command: agents::AgentsCommand::Check { dir },
            ..
        }) => agents::run_check(&dir),
        Some(Command::Agents {
            command,
            url,
            token,
        }) => {
            let target = daemon_target(url, token)?;
            agents::run_agents(&target.url, &target.token, command).await
        }
        Some(Command::Skills {
            command: skills::SkillsCommand::Check { dir, docs },
            ..
        }) => skills::run_check(&dir, docs.as_deref()).await,
        Some(Command::Skills {
            command,
            url,
            token,
        }) => {
            let target = daemon_target(url, token)?;
            skills::run_skills(&target.url, &target.token, command).await
        }
        Some(Command::Cron {
            command,
            url,
            token,
        }) => {
            let target = daemon_target(url, token)?;
            cron::run_cron(&target.url, &target.token, command).await
        }
        Some(Command::Attach { url, token }) => {
            #[cfg(feature = "tui")]
            cookie_agent_tui::markdown::warm_up_syntax_assets();
            let target = daemon_target(url, token)?;
            frontend::run_attached_tui(&target.url, &target.token).await
        }
        None => {
            frontend::run_plain(&workspace.expect("local workspace"), standalone, project).await
        }
    }
}

fn local_workspace(
    command: &Option<Command>,
    current_dir: impl FnOnce() -> io::Result<PathBuf>,
) -> anyhow::Result<Option<PathBuf>> {
    if matches!(
        command,
        Some(
            Command::Attach { .. }
                | Command::Web { .. }
                | Command::Connect { .. }
                | Command::Disconnect { .. }
                | Command::Mcp { .. }
                | Command::Config { .. }
                | Command::Onboard { .. }
                | Command::Skills { .. }
                | Command::Agents { .. }
                | Command::Machine { .. }
                | Command::Memory { .. }
                | Command::Project { .. }
                | Command::Cron { .. }
                | Command::Runtime { .. }
                | Command::Update(_),
        )
    ) {
        Ok(None)
    } else {
        current_dir()
            .context("determine current workspace")
            .map(Some)
    }
}

/// A fresh identity for one client process connecting to the daemon.
pub(crate) fn client_identity(kind: &'static str) -> cookie_agent_protocol::ClientIdentity {
    cookie_agent_protocol::ClientIdentity::generate(
        cookie_agent_protocol::ClientKind::new(kind).expect("static client kind is valid"),
    )
}

fn data_dir() -> anyhow::Result<PathBuf> {
    paths::user_data_root().context("determine home directory for Cookie Agent data")
}

/// This machine's host name; empty when it is unavailable.
fn host_name() -> String {
    paths::host_name()
}

/// The daemon a client or dial-in runtime connects to, and the token it
/// presents.
struct DaemonTarget {
    url: String,
    token: Zeroizing<String>,
}

/// The daemon to connect to: `--url` and `--token` (or
/// `COOKIE_DAEMON_TOKEN`), each falling back to this machine's `config.toml`:
/// `[server] url`, else the local daemon it describes, and `[server] token`.
fn daemon_target(url: Option<String>, token: Option<String>) -> anyhow::Result<DaemonTarget> {
    resolve_daemon_target(url, token, || {
        Ok(cookie_agent_config::load()
            .context("load configuration")?
            .runtime
            .server)
    })
}

fn resolve_daemon_target(
    url: Option<String>,
    token: Option<String>,
    configured: impl FnOnce() -> anyhow::Result<cookie_agent_config::ServerConfig>,
) -> anyhow::Result<DaemonTarget> {
    let (url, token) = match (url, token) {
        (Some(url), Some(token)) => (url, token),
        (url, token) => {
            let server = configured()?;
            let token = token.or_else(|| server.token.clone()).ok_or_else(|| {
                anyhow::anyhow!(
                    "no daemon token: pass --token, set COOKIE_DAEMON_TOKEN, or set `token` \
                     under [server] in config.toml"
                )
            })?;
            (url.unwrap_or_else(|| server.client_url()), token)
        }
    };
    validate_websocket_url(&url)?;
    Ok(DaemonTarget {
        url,
        token: Zeroizing::new(token),
    })
}
