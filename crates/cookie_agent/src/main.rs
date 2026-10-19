use std::{
    collections::BTreeMap,
    env, fmt,
    io::{self, IsTerminal, Write},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::Context as _;
use clap::{Parser, Subcommand};
use cookie_agent::{availability, run};
use cookie_agent_engine::{Engine, EngineOptions};
use cookie_agent_models::{
    ModelManager,
    catalog::{CatalogManager, CatalogTransport, HttpCatalogTransport},
    provider_store::ProviderStore,
};
use cookie_agent_protocol::oauth_callback::OAuthCallbackListener;
use cookie_agent_protocol::{
    AuthMethodDescriptor, Client, ClientConnectId, ClientRequestId, EffectiveAuthState,
    McpAuthBeginParams, McpAuthCancelParams, McpAuthCompleteParams, McpServerState,
    ProviderConfigurationState, ProviderConnectResult, ProviderDescriptor,
    ProviderDisconnectParams, ProviderDisconnectResult, ProviderSupportState,
    RuntimeSnapshotResult, SafeCode, SafeSetupValue, parse_setup_value, paths, setup_value_text,
    validate_websocket_url,
};
use cookie_agent_server::{Server, generate_token, ready_line};
use serde::Serialize;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use zeroize::{Zeroize, Zeroizing};

#[cfg(all(test, unix))]
use cookie_agent_protocol::{SetupFieldDescriptor, SetupFieldType};
#[cfg(all(test, unix))]
use std::sync::atomic::{AtomicUsize as TestAtomicUsize, Ordering as TestOrdering};

const CATALOG_REFRESH_INTERVAL: Duration = Duration::from_secs(60 * 60);

enum RunCatalogTransport {
    Http(HttpCatalogTransport),
    #[cfg(debug_assertions)]
    Bundled,
}

impl CatalogTransport for RunCatalogTransport {
    fn fetch(
        &self,
        etag: Option<String>,
    ) -> cookie_agent_models::catalog::CatalogTransportFuture<'_> {
        match self {
            Self::Http(transport) => transport.fetch(etag),
            #[cfg(debug_assertions)]
            Self::Bundled => Box::pin(async {
                Ok(
                    cookie_agent_models::catalog::CatalogTransportResponse::from_bytes(
                        200,
                        cookie_agent_models::catalog::MODELS_DEV_BOOTSTRAP.to_vec(),
                    ),
                )
            }),
        }
    }
}

fn run_catalog_transport() -> anyhow::Result<RunCatalogTransport> {
    #[cfg(debug_assertions)]
    if matches!(
        env::var("COOKIE_AGENT_TEST_BUNDLED_CATALOG").as_deref(),
        Ok("1")
    ) {
        return Ok(RunCatalogTransport::Bundled);
    }
    HttpCatalogTransport::new()
        .context("construct fixed catalog transport")
        .map(RunCatalogTransport::Http)
}

#[cfg(all(test, unix))]
static SECRET_VALUES_WIPED: TestAtomicUsize = TestAtomicUsize::new(0);

#[derive(Debug, Parser)]
#[command(name = "cookie", version = env!("COOKIE_VERSION"))]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
    /// When the daemon refuses a request, also print the technical detail of
    /// its error, such as the upstream cause or a path.
    #[arg(long, global = true)]
    error_details: bool,
}

#[derive(Debug, PartialEq, Eq, Subcommand)]
enum Command {
    /// Run one prompt without the interactive TUI.
    Run {
        #[command(flatten)]
        args: Box<run::RunArgs>,
    },
    /// Serve the cookie-agent client protocol 1.3 JSON-RPC WebSocket daemon on localhost.
    Daemon {
        /// Localhost port to bind. `0` selects an ephemeral port; the ready line
        /// reports the real one.
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
        #[arg(long, env = "COOKIE_DAEMON_TOKEN", hide_env_values = true)]
        token: Option<String>,
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
        #[arg(long, env = "COOKIE_DAEMON_TOKEN", hide_env_values = true)]
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
        #[arg(long, env = "COOKIE_DAEMON_TOKEN", hide_env_values = true)]
        token: Option<String>,
    },
    /// Serve a runtime to a daemon over the runtime protocol, as a
    /// `[runtimes.<id>]` template runs it. Needs no provider configuration.
    Runtime {
        #[command(subcommand)]
        command: RuntimeCommand,
    },
    /// Read and change a daemon's configuration: `config.toml` and the
    /// agent documents.
    Config {
        #[command(subcommand)]
        command: config_command::ConfigCommand,
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
            hide_env_values = true
        )]
        token: Option<String>,
    },
    /// List the machines a daemon's runtimes run on, and rename, regroup,
    /// merge, or forget them.
    Machine {
        #[command(subcommand)]
        command: machine_command::MachineCommand,
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
            hide_env_values = true
        )]
        token: Option<String>,
    },
    /// Manage the session history this machine stores.
    Sessions {
        #[command(subcommand)]
        command: SessionsCommand,
    },
    /// List MCP servers or start remote-server OAuth.
    Mcp {
        #[command(subcommand)]
        command: McpCommand,
        /// The daemon's client URL; `[server] url` in this machine's
        /// `config.toml`, or the local daemon, when omitted.
        #[arg(long)]
        url: Option<String>,
        /// The daemon's bearer token; `[server] token` in this machine's
        /// `config.toml` when omitted.
        #[arg(long, env = "COOKIE_DAEMON_TOKEN", hide_env_values = true)]
        token: Option<String>,
    },
}

#[derive(Debug, PartialEq, Eq, Subcommand)]
enum RuntimeCommand {
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
        #[arg(long, env = "COOKIE_DAEMON_TOKEN", hide_env_values = true)]
        token: Option<String>,
        /// Limit `coding` to workspaces under this directory; repeat for
        /// several. A leading `~` is expanded. Any workspace when omitted.
        #[arg(long = "root", value_name = "DIR")]
        roots: Vec<PathBuf>,
    },
}

#[derive(Debug, PartialEq, Eq, Subcommand)]
enum SessionsCommand {
    /// Convert the sessions of this directory to this build's history
    /// format now, as opening each one would. The originals stay beside
    /// them as `events.jsonl.v<N>.bak`. A session another cookie process
    /// holds is left for it to convert.
    Upgrade {
        /// Convert the sessions of every directory, not only this one's.
        #[arg(long)]
        all: bool,
    },
}

#[derive(Debug, PartialEq, Eq, Subcommand)]
enum McpCommand {
    /// List configured MCP servers.
    List,
    /// Start OAuth authorization for a remote MCP server.
    Auth { server: String },
}

struct Runtime {
    engine: Engine,
    server: Arc<Server>,
    port: u16,
    /// `[server] token`, when the configuration has one.
    token: Option<String>,
    /// `[server] allowed_origins`.
    allowed_origins: cookie_agent_server::AllowedOrigins,
    /// The user `config.toml`, where the daemon keeps a token it creates.
    config_path: Option<PathBuf>,
    catalog_refresh_shutdown: CancellationToken,
    catalog_refresh_task: Option<tokio::task::JoinHandle<()>>,
}

impl Runtime {
    async fn stop_catalog_refresh(&mut self) {
        self.catalog_refresh_shutdown.cancel();
        if let Some(task) = self.catalog_refresh_task.take() {
            let _ = task.await;
        }
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        self.catalog_refresh_shutdown.cancel();
    }
}

#[derive(Default, Serialize)]
#[serde(transparent)]
struct SecretValues(BTreeMap<String, String>);

impl fmt::Debug for SecretValues {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SecretValues(<redacted>)")
    }
}

impl Drop for SecretValues {
    fn drop(&mut self) {
        for value in self.0.values_mut() {
            value.zeroize();
        }
        #[cfg(all(test, unix))]
        SECRET_VALUES_WIPED.fetch_add(1, TestOrdering::SeqCst);
    }
}

impl SecretValues {
    /// The protocol's credential map, built without leaving a plaintext
    /// copy behind: the JSON in between is zeroized.
    fn into_credentials(self) -> anyhow::Result<cookie_agent_protocol::ProviderCredentialValues> {
        let serialized = Zeroizing::new(serde_json::to_vec(&self)?);
        serde_json::from_slice(&serialized).context("credentials")
    }
}

trait ConnectIo {
    fn write_line(&mut self, line: &str) -> anyhow::Result<()>;
    fn read_public(&mut self, prompt: &str) -> anyhow::Result<String>;
    fn read_secret(&mut self, prompt: &str) -> anyhow::Result<Zeroizing<String>>;
}

struct StdioConnectIo;

impl ConnectIo for StdioConnectIo {
    fn write_line(&mut self, line: &str) -> anyhow::Result<()> {
        println!("{line}");
        Ok(())
    }

    fn read_public(&mut self, prompt: &str) -> anyhow::Result<String> {
        read_public_line(prompt)
    }

    fn read_secret(&mut self, prompt: &str) -> anyhow::Result<Zeroizing<String>> {
        read_secret_line(prompt)
    }
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let error_details = cli.error_details;
    if let Err(error) = main_result(cli).await {
        eprintln!(
            "cookie: {}",
            cookie_agent_protocol::diagnostics::detail(&format!("{error:#}"))
        );
        // A daemon error shows its message and hint above; its technical
        // detail only on request.
        if let Some(detail) = error
            .chain()
            .find_map(|cause| cause.downcast_ref::<cookie_agent_protocol::ClientError>())
            .and_then(cookie_agent_protocol::ClientError::detail)
        {
            if error_details {
                eprintln!(
                    "details: {}",
                    cookie_agent_protocol::diagnostics::detail(&detail)
                );
            } else {
                eprintln!("(run again with --error-details for the technical detail)");
            }
        }
        std::process::exit(1);
    }
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

async fn main_result(cli: Cli) -> anyhow::Result<()> {
    init_logging();
    let Cli { command, .. } = cli;
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
        Some(Command::Run { args }) => {
            let workspace = match env::current_dir().context("determine current run workspace") {
                Ok(path) => path,
                Err(error) => {
                    eprintln!(
                        "cookie run: {}",
                        cookie_agent_protocol::diagnostics::detail(&format!("{error:#}"))
                    );
                    std::process::exit(run::EXIT_ENVIRONMENT);
                }
            };
            let data_dir = match args.data_dir.clone().map_or_else(data_dir, Ok) {
                Ok(path) => path,
                Err(error) => {
                    eprintln!(
                        "cookie run: {}",
                        cookie_agent_protocol::diagnostics::detail(&format!("{error:#}"))
                    );
                    std::process::exit(run::EXIT_ENVIRONMENT);
                }
            };
            let mut runtime = match compose_with(
                &workspace,
                run_catalog_transport,
                CatalogManager::standard,
                ProviderStore::standard,
                || Ok(data_dir),
            )
            .await
            {
                Ok(runtime) => runtime,
                Err(error) => {
                    eprintln!(
                        "cookie run: {}",
                        cookie_agent_protocol::diagnostics::detail(&format!("{error:#}"))
                    );
                    std::process::exit(run::EXIT_ENVIRONMENT);
                }
            };
            let exit_code = run::execute(&runtime.engine, *args).await;
            runtime.stop_catalog_refresh().await;
            runtime.engine.shutdown().await;
            if exit_code != 0 {
                std::process::exit(exit_code);
            }
            Ok(())
        }
        Some(Command::Daemon {
            port,
            listen,
            tls_cert,
            tls_key,
        }) => {
            let tls = match tls_cert.zip(tls_key) {
                Some((cert, key)) => {
                    Some(cookie_agent_server::load_tls(&cert, &key).context("load TLS identity")?)
                }
                None => None,
            };
            let runtime = compose(workspace.as_deref().expect("daemon workspace")).await?;
            let token = daemon_token(runtime.token.clone(), runtime.config_path.as_deref())?;
            run_daemon(
                runtime,
                DaemonListen {
                    port,
                    listen,
                    tls,
                    token,
                },
            )
            .await
        }
        Some(Command::Connect {
            provider_id,
            url,
            token,
        }) => {
            let target = daemon_target(url, token)?;
            run_connect(&target.url, &target.token, provider_id).await
        }
        Some(Command::Disconnect {
            provider_id,
            url,
            token,
        }) => {
            let target = daemon_target(url, token)?;
            run_disconnect(&target.url, &target.token, provider_id).await
        }
        Some(Command::Mcp {
            command,
            url,
            token,
        }) => {
            let target = daemon_target(url, token)?;
            run_mcp(&target.url, &target.token, command).await
        }
        Some(Command::Runtime { command }) => run_runtime(command).await,
        Some(Command::Sessions { command }) => {
            run_sessions(command, workspace.as_deref().expect("sessions workspace"))
        }
        Some(Command::Config {
            command,
            url,
            token,
        }) => {
            let target = daemon_target(url, token)?;
            config_command::run_config(&target.url, &target.token, command).await
        }
        Some(Command::Machine {
            command,
            url,
            token,
        }) => {
            let target = daemon_target(url, token)?;
            machine_command::run_machine(&target.url, &target.token, command).await
        }
        Some(Command::Attach { url, token }) => {
            #[cfg(feature = "tui")]
            cookie_agent_tui::markdown::warm_up_syntax_assets();
            let target = daemon_target(url, token)?;
            run_attached_tui(&target.url, &target.token).await
        }
        None => {
            // Syntax highlighting assets load while the runtime composes.
            #[cfg(feature = "tui")]
            cookie_agent_tui::markdown::warm_up_syntax_assets();
            run_local_frontend(compose(workspace.as_deref().expect("local workspace")).await?).await
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
                | Command::Connect { .. }
                | Command::Disconnect { .. }
                | Command::Mcp { .. }
                | Command::Config { .. }
                | Command::Machine { .. }
                | Command::Runtime { .. },
        )
    ) {
        Ok(None)
    } else {
        current_dir()
            .context("determine current workspace")
            .map(Some)
    }
}

/// `cookie sessions upgrade [--all]`: converts session trees on disk and
/// reports each one that needed it, or could not be converted.
fn run_sessions(command: SessionsCommand, workspace: &Path) -> anyhow::Result<()> {
    let SessionsCommand::Upgrade { all } = command;
    let data_root = data_dir()?;
    let upgrades =
        cookie_agent_engine::session::upgrade_sessions(&data_root, (!all).then_some(workspace))
            .context("upgrade sessions")?;
    let (mut current, mut converted, mut in_use, mut failed) = (0, 0, 0, 0);
    for (_, root, upgrade) in &upgrades {
        match upgrade {
            cookie_agent_engine::session::TreeUpgrade::Current => current += 1,
            cookie_agent_engine::session::TreeUpgrade::Converted(conversion) => {
                if !conversion.converted.is_empty() {
                    converted += 1;
                    let formats = conversion
                        .converted
                        .iter()
                        .map(|(_, format)| *format)
                        .collect::<std::collections::BTreeSet<_>>()
                        .into_iter()
                        .map(|format| format.to_string())
                        .collect::<Vec<_>>()
                        .join(", ");
                    println!(
                        "{root}: converted {} log(s) from history format {formats} to {}",
                        conversion.converted.len(),
                        cookie_agent_protocol::HISTORY_FORMAT
                    );
                }
                for (session, format) in &conversion.left {
                    failed += 1;
                    match format {
                        None => println!(
                            "{root}: {session} was written before history formats and is not \
                             converted"
                        ),
                        Some(format) => println!(
                            "{root}: {session} has history format {format}, which this build \
                             cannot convert"
                        ),
                    }
                }
            }
            cookie_agent_engine::session::TreeUpgrade::InUse => {
                in_use += 1;
                println!("{root}: skipped, another cookie process holds it");
            }
            cookie_agent_engine::session::TreeUpgrade::Failed(error) => {
                failed += 1;
                println!(
                    "{root}: not converted: {}",
                    cookie_agent_protocol::diagnostics::detail(&error.to_string())
                );
            }
        }
    }
    println!(
        "{} session tree(s): {converted} converted, {current} already current, {in_use} in use; \
         {failed} session(s) not converted",
        upgrades.len()
    );
    anyhow::ensure!(failed == 0, "{failed} session(s) could not be converted");
    Ok(())
}

/// `cookie runtime coding --stdio`: serves one coding runtime for the
/// workspace over this process's stdio until the daemon sends
/// `runtime/shutdown` or closes standard input. Standard output carries only
/// protocol messages; errors go to standard error.
async fn run_runtime(command: RuntimeCommand) -> anyhow::Result<()> {
    let RuntimeCommand::Serve {
        stdio,
        daemon,
        id,
        token,
        roots,
    } = command;
    let roots = roots
        .iter()
        .map(|root| {
            let expanded = cookie_agent_coding_runtime::server::expand_home(root);
            let root = expanded
                .canonicalize()
                .with_context(|| format!("root {} is unavailable", expanded.display()))?;
            anyhow::ensure!(root.is_dir(), "root {} is not a directory", root.display());
            Ok(root)
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    if !stdio && let Some(daemon) = daemon {
        let DaemonTarget { url: daemon, token } = daemon_target(daemon, token)?;
        let id = id.unwrap_or_else(|| host_runtime_id("", &host_name()));
        cookie_agent_protocol::RuntimeId::new(id.as_str())
            .map_err(|_| anyhow::anyhow!("invalid runtime ID `{id}`"))?;
        cookie_agent_protocol::runtime_dial_in_url(&daemon, &id)?;
        // A runtime that dials in hosts its own machine's MCP servers: those
        // of `[runtimes.local.mcp]` in its configuration.
        let mcp_servers = local_runtime_mcp_servers()?;
        let stop = CancellationToken::new();
        stop_on_signal(stop.clone())?;
        dial_in(
            &daemon,
            &token,
            &id,
            cookie_agent_coding_runtime::server::ServeOptions {
                roots,
                mcp_servers,
                stop,
                ..Default::default()
            },
        )
        .await;
        return Ok(());
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

async fn compose(workspace: &Path) -> anyhow::Result<Runtime> {
    compose_with(
        workspace,
        || HttpCatalogTransport::new().context("construct fixed catalog transport"),
        CatalogManager::standard,
        ProviderStore::standard,
        data_dir,
    )
    .await
}

async fn compose_with<T: CatalogTransport + 'static>(
    workspace: &Path,
    open_transport: impl FnOnce() -> anyhow::Result<T>,
    open_catalog: impl FnOnce(T) -> CatalogManager<T>,
    open_provider_store: impl FnOnce() -> Result<
        ProviderStore,
        cookie_agent_models::provider_store::ProviderStoreError,
    >,
    open_data_dir: impl FnOnce() -> anyhow::Result<PathBuf>,
) -> anyhow::Result<Runtime> {
    let configuration =
        cookie_agent_config::load().context("load user configuration, agents, and skills")?;
    compose_with_configuration(
        workspace,
        configuration,
        open_transport,
        open_catalog,
        open_provider_store,
        open_data_dir,
    )
    .await
}

async fn compose_with_configuration<T: CatalogTransport + 'static>(
    workspace: &Path,
    configuration: cookie_agent_config::LoadedConfiguration,
    open_transport: impl FnOnce() -> anyhow::Result<T>,
    open_catalog: impl FnOnce(T) -> CatalogManager<T>,
    open_provider_store: impl FnOnce() -> Result<
        ProviderStore,
        cookie_agent_models::provider_store::ProviderStoreError,
    >,
    open_data_dir: impl FnOnce() -> anyhow::Result<PathBuf>,
) -> anyhow::Result<Runtime> {
    if configuration.runtime.server.host != "127.0.0.1" {
        anyhow::bail!("server.host must be exactly 127.0.0.1");
    }
    let port = configuration.runtime.server.port;
    let token = configuration.runtime.server.token.clone();
    let allowed_origins = cookie_agent_server::AllowedOrigins::from_config(
        &configuration.runtime.server.allowed_origins,
    );
    let config_path = configuration.config_paths.user.clone();

    // Startup never waits on the network: it serves the validated cache (or
    // the bundled catalog) and the refresh loop checks models.dev right away.
    let catalog_manager = open_catalog(open_transport()?);
    let catalog = Arc::new(
        catalog_manager
            .load_cached()
            .context("load cached models.dev catalog")?,
    );
    let provider_store = open_provider_store().context("open provider store 3")?;
    let model_manager = Arc::new(
        ModelManager::new_with_headers(
            configuration.runtime.providers.clone(),
            configuration.runtime.headers.clone(),
            catalog,
            provider_store,
        )
        .context("compose effective providers and current model runtime")?,
    );
    let engine = Engine::open_async(EngineOptions {
        data_dir: open_data_dir()?,
        cwd: workspace.to_owned(),
        config: configuration,
        model_manager,
        runtimes: cookie_agent_coding_runtime::runtime_options(workspace),
    })
    .await
    .context("open manifests, rehydrate project state, and reconcile engine")?;
    let server = Arc::new(Server::new(engine.clone()));
    let catalog_refresh_shutdown = CancellationToken::new();
    let catalog_refresh_task = tokio::spawn(run_catalog_refresh_loop(
        catalog_manager,
        engine.clone(),
        catalog_refresh_shutdown.clone(),
        CATALOG_REFRESH_INTERVAL,
    ));
    Ok(Runtime {
        engine,
        server,
        port,
        token,
        allowed_origins,
        config_path,
        catalog_refresh_shutdown,
        catalog_refresh_task: Some(catalog_refresh_task),
    })
}

async fn run_catalog_refresh_loop<T: CatalogTransport + 'static>(
    catalog_manager: CatalogManager<T>,
    engine: Engine,
    shutdown: CancellationToken,
    cadence: Duration,
) {
    // The first check runs immediately, since startup served the cache.
    let mut first = true;
    loop {
        if !std::mem::take(&mut first) {
            tokio::select! {
                () = shutdown.cancelled() => return,
                () = tokio::time::sleep(cadence) => {}
            }
        }
        let refreshed = tokio::select! {
            () = shutdown.cancelled() => return,
            refreshed = catalog_manager.refresh() => refreshed,
        };
        let Ok(catalog) = refreshed else {
            continue;
        };
        if !catalog_publication_changed(&engine, &catalog) {
            continue;
        }
        if engine.refresh_catalog(Arc::new(catalog)).is_err() {
            continue;
        }
    }
}

/// Republishing recompiles every model, so it happens only when the models
/// could differ: a new catalog revision or a change in availability. A server
/// confirming the cached body (source `cache` becoming `network`) is not one.
fn catalog_publication_changed(
    engine: &Engine,
    catalog: &cookie_agent_models::catalog::CatalogSnapshot,
) -> bool {
    let current = engine.current_runtime();
    let current = current.models.catalog();
    current.revision != catalog.revision || current.state.availability != catalog.state.availability
}

/// A fresh identity for one client process connecting to the daemon.
fn client_identity(kind: &'static str) -> cookie_agent_protocol::ClientIdentity {
    cookie_agent_protocol::ClientIdentity::generate(
        cookie_agent_protocol::ClientKind::new(kind).expect("static client kind is valid"),
    )
}

fn data_dir() -> anyhow::Result<PathBuf> {
    paths::user_data_root().context("determine home directory for cookie agent data")
}

/// This machine's host name; empty when it is unavailable.
fn host_name() -> String {
    paths::host_name()
}

/// The ID a TUI offers its machine's runtime under: `client_` and the host
/// name, sanitized as [`host_runtime_id`] does.
#[cfg(feature = "tui")]
fn client_runtime_id(host: &str) -> String {
    host_runtime_id("client", host)
}

/// A runtime ID for this machine: `<prefix>_<host>`, or the host name alone
/// without a prefix, with the host name lowercased, every character outside
/// `[a-z0-9_]` replaced by `_`, repeats and edges trimmed, at most 32 bytes.
/// A bare host name that does not start with a letter gets `host_` in front;
/// no host name at all gives the prefix, or `host`.
fn host_runtime_id(prefix: &str, host: &str) -> String {
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

/// This process's canonical working directory, which the TUI sends as the
/// directory of the sessions it creates; `None` when it is unavailable.
#[cfg(feature = "tui")]
fn client_directory() -> Option<String> {
    let cwd = std::env::current_dir().ok()?;
    let cwd = cwd.canonicalize().unwrap_or(cwd);
    Some(cwd.to_string_lossy().into_owned())
}

#[cfg(feature = "tui")]
async fn run_local_frontend(mut runtime: Runtime) -> anyhow::Result<()> {
    let (client_stream, server_stream) = cookie_agent_protocol::in_process_pair(128);
    let server_task = tokio::spawn(runtime.server.clone().serve_stream(
        server_stream,
        cookie_agent_protocol::Authentication::Authenticated,
    ));
    let result = async {
        let client = Client::connect_stream(client_stream);
        client
            .handshake_as(client_identity("tui"))
            .await
            .context("handshake with daemon")?;
        cookie_agent_tui::run_with_new_session(
            client,
            cookie_agent_tui::SessionPlacement {
                cwd: client_directory(),
                runtime: None,
            },
        )
        .await
    }
    .await;
    runtime.server.shutdown();
    runtime.stop_catalog_refresh().await;
    let server_result = server_task
        .await
        .context("join in-process server task")?
        .context("run in-process server task");
    runtime.engine.shutdown().await;
    result.and(server_result)
}

#[cfg(not(feature = "tui"))]
async fn run_local_frontend(mut runtime: Runtime) -> anyhow::Result<()> {
    runtime.server.shutdown();
    runtime.stop_catalog_refresh().await;
    runtime.engine.shutdown().await;
    anyhow::bail!(
        "cookie was built without TUI support; use `cookie daemon`, `cookie connect`, or `cookie disconnect`"
    )
}

#[cfg(feature = "tui")]
async fn run_attached_tui(url: &str, token: &str) -> anyhow::Result<()> {
    validate_websocket_url(url)?;
    // One identity for the whole session: after a reconnect the daemon keeps
    // stamping the same origin, so this TUI still recognizes its own inputs.
    let identity = client_identity("tui");
    let client = Client::connect_websocket_with_token(url, token)
        .await
        .context("connect to daemon WebSocket")?;
    let hello = client
        .handshake_as(identity.clone())
        .await
        .context("handshake with daemon")?;
    let cwd = client_directory();
    // A daemon on another machine cannot see this one: offer it this
    // machine's directory as a runtime that lives as long as the TUI.
    let runtime = match (
        &hello.machine_id,
        cookie_agent_protocol::paths::machine_id(),
    ) {
        (Some(daemon), Some(local)) if *daemon != local => {
            let id = client_runtime_id(&host_name());
            let options = cookie_agent_coding_runtime::server::ServeOptions {
                roots: cwd.iter().map(PathBuf::from).collect(),
                mcp_servers: local_runtime_mcp_servers()?,
                ..Default::default()
            };
            let (url, token, dial_id) = (url.to_owned(), token.to_owned(), id.clone());
            tokio::spawn(async move { dial_in(&url, &token, &dial_id, options).await });
            Some(id)
        }
        _ => None,
    };
    let (url, token) = (url.to_owned(), token.to_owned());
    let reconnector: cookie_agent_tui::Reconnector = std::sync::Arc::new(move || {
        let (url, token, identity) = (url.clone(), token.clone(), identity.clone());
        Box::pin(async move {
            let client = Client::connect_websocket_with_token(&url, &token)
                .await
                .map_err(|error| error.to_string())?;
            client
                .handshake_as(identity)
                .await
                .map_err(|error| error.to_string())?;
            Ok(client)
        })
    });
    cookie_agent_tui::run_attached(
        client,
        reconnector,
        cookie_agent_tui::SessionPlacement { cwd, runtime },
    )
    .await
}

#[cfg(not(feature = "tui"))]
async fn run_attached_tui(url: &str, _token: &str) -> anyhow::Result<()> {
    validate_websocket_url(url)?;
    anyhow::bail!("cookie was built without TUI support")
}

async fn runtime_snapshot(client: &Client) -> anyhow::Result<RuntimeSnapshotResult> {
    client
        .runtime_snapshot()
        .await
        .context("runtime.snapshot.get failed")
}

async fn run_connect(url: &str, token: &str, provider_id: Option<String>) -> anyhow::Result<()> {
    require_interactive_tty(
        io::stdin().is_terminal(),
        io::stdout().is_terminal(),
        io::stderr().is_terminal(),
        "connect",
    )?;
    let client = Client::connect_websocket_with_token(url, token).await?;
    let mut io = StdioConnectIo;
    run_connect_with(&client, provider_id, &mut io).await
}

async fn run_connect_with<I: ConnectIo>(
    client: &Client,
    provider_id: Option<String>,
    io: &mut I,
) -> anyhow::Result<()> {
    client
        .handshake_as(client_identity("cli"))
        .await
        .context("handshake with daemon")?;
    let runtime = runtime_snapshot(client).await?;
    let provider = choose_provider(&runtime.snapshot.providers, provider_id, io)?;
    print_provider_details(provider, runtime.snapshot.revision.as_str(), io)?;
    ensure_supported(provider)?;
    io.write_line("Scope: durable, global (available to all workspaces).")?;
    if !matches!(
        io.read_public(connect_confirmation_prompt(provider))?
            .to_ascii_lowercase()
            .as_str(),
        "y" | "yes"
    ) {
        anyhow::bail!("provider connection cancelled");
    }

    // The TUI's order: the auth method, its setup values, then credentials.
    let auth_method = choose_auth_method(provider, io)?;
    let setup_values = collect_setup_values(provider, |prompt| io.read_public(prompt))?;
    let auth_values =
        collect_auth_values(auth_method, |prompt| io.read_secret(prompt))?.into_credentials()?;
    let result: ProviderConnectResult = client
        .connect_provider_retrying(
            cookie_agent_protocol::ProviderConnectParams {
                provider_id: provider.id.clone(),
                expected_revision: runtime.snapshot.revision,
                setup_values,
                auth_method: auth_method.id.clone(),
                auth_values,
                client_connect_id: ClientConnectId::new(Uuid::now_v7().to_string())
                    .expect("UUID is a valid connect ID"),
            },
            provider,
        )
        .await
        .context("provider.connect failed")?;
    io.write_line(&format!(
        "provider.connect succeeded for {} at catalog revision {}{}.",
        result.durable_connection.provider_id,
        result.runtime.revision,
        if result.replayed { " (replayed)" } else { "" }
    ))?;
    Ok(())
}

async fn run_disconnect(url: &str, token: &str, provider_id: Option<String>) -> anyhow::Result<()> {
    require_interactive_tty(
        io::stdin().is_terminal(),
        io::stdout().is_terminal(),
        io::stderr().is_terminal(),
        "disconnect",
    )?;
    let client = Client::connect_websocket_with_token(url, token).await?;
    let mut io = StdioConnectIo;
    client
        .handshake_as(client_identity("cli"))
        .await
        .context("handshake with daemon")?;
    let runtime = runtime_snapshot(&client).await?;
    let provider = choose_provider(&runtime.snapshot.providers, provider_id, &mut io)?;
    print_provider_details(provider, runtime.snapshot.revision.as_str(), &mut io)?;
    println!("Scope: durable, global (available to all workspaces).");
    if !prompt_confirmation("Disconnect this managed provider? [y/N] ")? {
        anyhow::bail!("provider disconnection cancelled");
    }
    let result: ProviderDisconnectResult = client
        .disconnect_provider(ProviderDisconnectParams {
            provider_id: provider.id.clone(),
            expected_revision: runtime.snapshot.revision,
            expected_connection_generation: provider
                .durable_connection
                .as_ref()
                .map(|connection| connection.connection_generation),
            client_request_id: ClientRequestId::new(Uuid::now_v7().to_string())
                .expect("UUID is a valid request ID"),
        })
        .await
        .context("provider.disconnect failed")?;
    println!(
        "provider.disconnect succeeded for {} at catalog revision {}{}.",
        result.provider_id,
        result.runtime.snapshot.revision,
        if result.replayed { " (replayed)" } else { "" }
    );
    Ok(())
}

async fn run_mcp(url: &str, token: &str, command: McpCommand) -> anyhow::Result<()> {
    let client = Client::connect_websocket_with_token(url, token).await?;
    client
        .handshake_as(client_identity("cli"))
        .await
        .context("handshake with daemon")?;
    match command {
        McpCommand::List => {
            let servers = client
                .list_mcp_servers()
                .await
                .context("mcp.server.list failed")?
                .servers;
            if servers.is_empty() {
                println!("No MCP servers are configured.");
            }
            for server in servers {
                let runtime = match &server.workspace {
                    Some(workspace) => format!("{} at {workspace}", server.runtime_id),
                    None => server.runtime_id.to_string(),
                };
                println!(
                    "{}\t{} tools\t{:?}\t{runtime}",
                    server.name, server.tool_count, server.state
                );
            }
            Ok(())
        }
        McpCommand::Auth { server } => {
            let status = client
                .list_mcp_servers()
                .await
                .context("mcp.server.list failed")?
                .servers
                .into_iter()
                .find(|candidate| {
                    candidate.name == server
                        && candidate.runtime_id.as_str() == cookie_agent_protocol::RuntimeId::GLOBAL
                })
                .with_context(|| format!("unknown MCP server `{server}`"))?;
            if status.state != McpServerState::NeedsAuth {
                anyhow::bail!(
                    "MCP server `{server}` does not currently require OAuth authorization"
                );
            }
            authorize_mcp_server(&client, server).await
        }
    }
}

/// Waits for the browser's redirect on this machine's loopback, or for the
/// redirect URL pasted on stdin when the browser runs elsewhere, and hands it
/// to the daemon, which exchanges the code.
async fn authorize_mcp_server(client: &Client, server: String) -> anyhow::Result<()> {
    use tokio::io::AsyncBufReadExt as _;

    let listener = OAuthCallbackListener::bind()
        .await
        .context("listen for the OAuth callback")?;
    let redirect_uri = listener.redirect_uri();
    let result = client
        .begin_mcp_auth(McpAuthBeginParams {
            server: server.clone(),
            redirect_uri: redirect_uri.clone(),
        })
        .await
        .context("mcp.auth.begin failed")?;
    println!("Open this URL to authenticate MCP server `{server}`:");
    println!("{}", result.authorization_url);
    println!();
    println!("Waiting for the browser to return to {redirect_uri}.");
    println!("If the browser runs on another machine, paste the URL it was redirected to.");

    let mut stdin = tokio::io::BufReader::new(tokio::io::stdin()).lines();
    let mut stdin_open = true;
    let mut status_poll = tokio::time::interval(Duration::from_secs(2));
    let mut signals =
        cookie_agent_protocol::StopSignals::new().context("listen for stop signals")?;
    loop {
        let completed = tokio::select! {
            callback = listener.accept() => {
                let callback = callback.context("receive the OAuth callback")?;
                let completed = complete_mcp_auth(client, &server, callback.url()).await;
                let _ = callback.respond(completed.is_ok()).await;
                completed
            }
            line = stdin.next_line(), if stdin_open => match line.context("read stdin")? {
                Some(line) if line.trim().is_empty() => continue,
                Some(line) => complete_mcp_auth(client, &server, &line).await,
                None => {
                    stdin_open = false;
                    continue;
                }
            },
            _ = status_poll.tick() => {
                if let Some(message) = ended_mcp_auth(client, &server).await? {
                    anyhow::bail!("MCP authorization for `{server}` ended: {message}");
                }
                continue;
            }
            () = signals.recv() => {
                let _ = client
                    .cancel_mcp_auth(McpAuthCancelParams { server: server.clone() })
                    .await;
                anyhow::bail!("MCP authorization for `{server}` cancelled");
            }
        };
        match completed {
            Ok(()) => {
                println!("MCP server `{server}` authorized.");
                return Ok(());
            }
            // A URL that is not this flow's callback leaves it waiting.
            Err(error) => match ended_mcp_auth(client, &server).await? {
                Some(_) => return Err(error),
                None => eprintln!("{error:#}"),
            },
        }
    }
}

async fn complete_mcp_auth(
    client: &Client,
    server: &str,
    callback_url: &str,
) -> anyhow::Result<()> {
    client
        .complete_mcp_auth(McpAuthCompleteParams {
            server: server.to_owned(),
            callback_url: callback_url.trim().to_owned(),
        })
        .await
        .context("mcp.auth.complete failed")?;
    Ok(())
}

/// The server's status message once its authorization is no longer waiting.
async fn ended_mcp_auth(client: &Client, server: &str) -> anyhow::Result<Option<String>> {
    let status = client
        .list_mcp_servers()
        .await
        .context("mcp.server.list failed")?
        .servers
        .into_iter()
        .find(|candidate| candidate.name == server)
        .with_context(|| format!("unknown MCP server `{server}`"))?;
    Ok((status.auth_in_progress != Some(true)).then(|| {
        status
            .message
            .unwrap_or_else(|| "no longer in progress".into())
    }))
}

fn ensure_supported(provider: &ProviderDescriptor) -> anyhow::Result<()> {
    if provider.support.state != ProviderSupportState::Supported {
        let reason = provider
            .support
            .reason
            .as_ref()
            .map_or("unspecified", SafeCode::as_str);
        anyhow::bail!("provider is not connectable: {reason}");
    }
    Ok(())
}

fn connect_confirmation_prompt(provider: &ProviderDescriptor) -> &'static str {
    if matches!(
        provider.configuration,
        ProviderConfigurationState::Authored | ProviderConfigurationState::AuthoredAndStored
    ) && provider.effective_auth_state == EffectiveAuthState::Unavailable
    {
        "Complete setup and authentication for this authored provider? [y/N] "
    } else if provider.durable_connection.is_some() {
        "Reconnect or update this provider? [y/N] "
    } else {
        "Connect this provider? [y/N] "
    }
}

fn collect_setup_values(
    provider: &ProviderDescriptor,
    mut read: impl FnMut(&str) -> anyhow::Result<String>,
) -> anyhow::Result<BTreeMap<cookie_agent_protocol::SetupFieldId, SafeSetupValue>> {
    let stored = provider
        .durable_connection
        .as_ref()
        .map(|connection| &connection.setup_values);
    provider
        .setup_fields
        .iter()
        .filter_map(|field| {
            let existing = stored.and_then(|values| values.get(&field.id));
            let fallback = existing.or(field.default.as_ref());
            let prompt = if let Some(value) = fallback {
                format!("{} [{}]: ", field.display_name, setup_value_text(value))
            } else {
                format!("{}: ", field.display_name)
            };
            let answer = match read(&prompt) {
                Ok(answer) => answer,
                Err(error) => return Some(Err(error)),
            };
            if answer.is_empty() {
                return fallback
                    .cloned()
                    .map(|value| Ok((field.id.clone(), value)))
                    .or_else(|| {
                        field.required.then(|| {
                            Err(anyhow::anyhow!(
                                "required setup field `{}` was blank",
                                field.id
                            ))
                        })
                    });
            }
            Some(
                parse_setup_value(field, &answer)
                    .map_err(anyhow::Error::from)
                    .map(|value| (field.id.clone(), value)),
            )
        })
        .collect()
}

fn choose_auth_method<'a>(
    provider: &'a ProviderDescriptor,
    io: &mut impl ConnectIo,
) -> anyhow::Result<&'a AuthMethodDescriptor> {
    if provider.auth_methods.is_empty() {
        anyhow::bail!("provider has no supported authentication method");
    }
    if provider.auth_methods.len() == 1 {
        return Ok(&provider.auth_methods[0]);
    }
    io.write_line("Authentication methods:")?;
    for (index, method) in provider.auth_methods.iter().enumerate() {
        io.write_line(&format!(
            "  {}. {} ({})",
            index + 1,
            method.display_name,
            method.id
        ))?;
    }
    let answer = io.read_public("Authentication method number or ID: ")?;
    if let Ok(index) = answer.parse::<usize>()
        && let Some(method) = index
            .checked_sub(1)
            .and_then(|index| provider.auth_methods.get(index))
    {
        return Ok(method);
    }
    provider
        .auth_methods
        .iter()
        .find(|method| method.id.as_str() == answer)
        .with_context(|| format!("authentication method `{answer}` was not found"))
}

fn collect_auth_values(
    method: &AuthMethodDescriptor,
    mut read: impl FnMut(&str) -> anyhow::Result<Zeroizing<String>>,
) -> anyhow::Result<SecretValues> {
    let mut values = SecretValues::default();
    for field in &method.credentials {
        let mut value = read(&format!(
            "{} (secret, blank does not retain): ",
            field.display_name
        ))?;
        if value.is_empty() {
            if field.required {
                anyhow::bail!("required credential field `{}` was blank", field.id);
            }
        } else {
            values
                .0
                .insert(field.id.as_str().to_owned(), std::mem::take(&mut *value));
        }
    }
    Ok(values)
}

fn require_interactive_tty(
    stdin: bool,
    stdout: bool,
    stderr: bool,
    command: &str,
) -> anyhow::Result<()> {
    if !(stdin && stdout && stderr) {
        anyhow::bail!(
            "cookie {command} requires an interactive TTY; credentials are never accepted as command-line arguments"
        );
    }
    Ok(())
}

fn choose_provider<'a>(
    providers: &'a [ProviderDescriptor],
    requested: Option<String>,
    io: &mut impl ConnectIo,
) -> anyhow::Result<&'a ProviderDescriptor> {
    if let Some(requested) = requested {
        return providers
            .iter()
            .find(|provider| provider.id.as_str() == requested)
            .with_context(|| format!("runtime provider `{requested}` was not found"));
    }
    if providers.is_empty() {
        anyhow::bail!("the runtime has no providers");
    }
    io.write_line("Runtime providers:")?;
    for (index, provider) in providers.iter().enumerate() {
        let support = provider.support.reason.as_ref().map_or_else(
            || format!("{:?}", provider.support.state).to_ascii_lowercase(),
            |reason| format!("{:?}: {}", provider.support.state, reason).to_ascii_lowercase(),
        );
        io.write_line(&format!(
            "  {}. {} ({}) — {} · models: {}",
            index + 1,
            provider.display_name,
            provider.id,
            support,
            availability::model_counts_text(&provider.model_counts)
        ))?;
    }
    let answer = io.read_public("Provider number or ID: ")?;
    if let Ok(index) = answer.parse::<usize>()
        && let Some(provider) = index.checked_sub(1).and_then(|index| providers.get(index))
    {
        return Ok(provider);
    }
    providers
        .iter()
        .find(|provider| provider.id.as_str() == answer)
        .with_context(|| format!("runtime provider `{answer}` was not found"))
}

fn print_provider_details(
    provider: &ProviderDescriptor,
    revision: &str,
    io: &mut impl ConnectIo,
) -> anyhow::Result<()> {
    io.write_line(&format!("Provider ID: {}", provider.id))?;
    io.write_line(&format!("Name: {}", provider.display_name))?;
    io.write_line(&format!("Catalog revision: {revision}"))?;
    io.write_line(&format!("Presence: {:?}", provider.presence))?;
    io.write_line(&format!("Support: {:?}", provider.support.state))?;
    if let Some(reason) = &provider.support.reason {
        io.write_line(&format!("Support reason: {reason}"))?;
    }
    io.write_line(&format!("Configuration: {:?}", provider.configuration))?;
    io.write_line(&format!(
        "Models: {}",
        availability::model_counts_text(&provider.model_counts)
    ))?;
    for model in &provider.unavailable_models {
        io.write_line(&format!(
            "  {} — {}",
            model.id,
            availability::unavailable_model_text(model)
        ))?;
    }
    if let Some(url) = &provider.documentation_url {
        io.write_line(&format!("Docs: {url}"))?;
    }
    if let Some(hint) = availability::env_config_hint(provider) {
        io.write_line(&format!("Config alternative: {hint}"))?;
    }
    Ok(())
}

fn read_public_line(prompt: &str) -> anyhow::Result<String> {
    print!("{prompt}");
    io::stdout().flush().context("flush prompt")?;
    let mut answer = String::new();
    io::stdin().read_line(&mut answer).context("read input")?;
    Ok(answer.trim().to_owned())
}

fn prompt_confirmation(prompt: &str) -> anyhow::Result<bool> {
    Ok(matches!(
        read_public_line(prompt)?.to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

#[cfg(unix)]
fn read_secret_line(prompt: &str) -> anyhow::Result<Zeroizing<String>> {
    struct EchoGuard(libc::termios);
    impl Drop for EchoGuard {
        fn drop(&mut self) {
            // SAFETY: stdin remains a valid process file descriptor.
            unsafe {
                libc::tcsetattr(libc::STDIN_FILENO, libc::TCSAFLUSH, &self.0);
            }
        }
    }

    print!("{prompt}");
    io::stdout().flush().context("flush credential prompt")?;
    // SAFETY: initialized by tcgetattr before use.
    let mut original = unsafe { std::mem::zeroed::<libc::termios>() };
    // SAFETY: connect verifies stdin is a TTY.
    if unsafe { libc::tcgetattr(libc::STDIN_FILENO, &mut original) } != 0 {
        return Err(io::Error::last_os_error()).context("disable credential echo");
    }
    let guard = EchoGuard(original);
    let mut hidden = original;
    hidden.c_lflag &= !libc::ECHO;
    // SAFETY: both termios values are initialized.
    if unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSAFLUSH, &hidden) } != 0 {
        return Err(io::Error::last_os_error()).context("disable credential echo");
    }
    let mut value = String::new();
    let read = io::stdin().read_line(&mut value).context("read credential");
    drop(guard);
    println!();
    read?;
    while matches!(value.as_bytes().last(), Some(b'\n' | b'\r')) {
        value.pop();
    }
    Ok(Zeroizing::new(value))
}

#[cfg(windows)]
fn read_secret_line(prompt: &str) -> anyhow::Result<Zeroizing<String>> {
    use windows_sys::Win32::{
        Foundation::{HANDLE, INVALID_HANDLE_VALUE},
        System::Console::{
            ENABLE_ECHO_INPUT, GetConsoleMode, GetStdHandle, STD_INPUT_HANDLE, SetConsoleMode,
        },
    };

    struct ConsoleModeGuard {
        handle: HANDLE,
        mode: u32,
    }
    impl Drop for ConsoleModeGuard {
        fn drop(&mut self) {
            // SAFETY: the console handle and original mode remain valid for this process.
            unsafe {
                SetConsoleMode(self.handle, self.mode);
            }
        }
    }

    print!("{prompt}");
    io::stdout().flush().context("flush credential prompt")?;
    // SAFETY: GetStdHandle requires no caller-owned pointers.
    let handle = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error()).context("open credential console");
    }
    let mut original = 0;
    // SAFETY: original points to writable mode storage.
    if unsafe { GetConsoleMode(handle, &mut original) } == 0 {
        return Err(io::Error::last_os_error()).context("read credential console mode");
    }
    // SAFETY: handle is a console input handle and the mode only disables echo.
    if unsafe { SetConsoleMode(handle, original & !ENABLE_ECHO_INPUT) } == 0 {
        return Err(io::Error::last_os_error()).context("disable credential echo");
    }
    let guard = ConsoleModeGuard {
        handle,
        mode: original,
    };
    let mut value = String::new();
    let read = io::stdin().read_line(&mut value).context("read credential");
    drop(guard);
    println!();
    read?;
    while matches!(value.as_bytes().last(), Some(b'\n' | b'\r')) {
        value.pop();
    }
    Ok(Zeroizing::new(value))
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

/// The daemon's token: `[server] token` from its configuration, or a fresh
/// one written there when there is none, so it stays the same across
/// restarts.
fn daemon_token(
    configured: Option<String>,
    config_path: Option<&Path>,
) -> anyhow::Result<Zeroizing<String>> {
    if let Some(token) = configured {
        return Ok(Zeroizing::new(token));
    }
    let token = generate_token().context("generate daemon authentication token")?;
    let path = config_path.context("no user config.toml to keep the daemon token in")?;
    cookie_agent_config::write_server_token(path, &token)
        .with_context(|| format!("keep the daemon token in {}", path.display()))?;
    Ok(token)
}

/// `[runtimes.local.mcp]` of this machine's configuration, as a runtime
/// that dials in hosts them.
fn local_runtime_mcp_servers()
-> anyhow::Result<Vec<cookie_agent_runtime_api::protocol::McpServerSettings>> {
    let config = cookie_agent_config::load().context("load configuration")?;
    Ok(config
        .runtimes
        .iter()
        .find(|(id, _)| id.as_str() == cookie_agent_protocol::RuntimeId::LOCAL)
        .map(|(_, template)| {
            template
                .mcp_servers
                .iter()
                .map(
                    |(name, server)| cookie_agent_runtime_api::protocol::McpServerSettings {
                        name: name.clone(),
                        config: serde_json::to_value(server)
                            .expect("MCP server configuration serializes"),
                    },
                )
                .collect()
        })
        .unwrap_or_default())
}

/// Dials in to the daemon at `daemon` as runtime `id` and serves it,
/// connecting again after every drop with a growing wait, until stopped.
async fn dial_in(
    daemon: &str,
    token: &str,
    id: &str,
    options: cookie_agent_coding_runtime::server::ServeOptions,
) {
    const LONGEST_WAIT: Duration = Duration::from_secs(30);
    let stop = options.stop.clone();
    let mut wait = Duration::from_secs(1);
    loop {
        let connected = tokio::select! {
            () = stop.cancelled() => return,
            connected = cookie_agent_protocol::WebSocketTransport::connect_runtime(daemon, token, id) => connected,
        };
        match connected {
            Ok(transport) => {
                eprintln!("cookie runtime: connected to {daemon} as `{id}`");
                let started = std::time::Instant::now();
                cookie_agent_coding_runtime::server::serve(Box::new(transport), options.clone())
                    .await;
                if stop.is_cancelled() {
                    return;
                }
                eprintln!("cookie runtime: disconnected from {daemon}");
                if started.elapsed() > LONGEST_WAIT {
                    wait = Duration::from_secs(1);
                }
            }
            Err(error) => eprintln!("cookie runtime: cannot reach {daemon}: {error}"),
        }
        tokio::select! {
            () = tokio::time::sleep(wait) => {}
            () = stop.cancelled() => return,
        }
        wait = (wait * 2).min(LONGEST_WAIT);
    }
}

/// Where and how `cookie daemon` listens.
struct DaemonListen {
    port: Option<u16>,
    listen: Option<std::net::SocketAddr>,
    tls: Option<cookie_agent_server::TlsConfig>,
    token: Zeroizing<String>,
}

async fn run_daemon(mut runtime: Runtime, options: DaemonListen) -> anyhow::Result<()> {
    let DaemonListen {
        port,
        listen,
        tls,
        token,
    } = options;
    let address = listen.unwrap_or_else(|| {
        std::net::SocketAddr::from(([127, 0, 0, 1], port.unwrap_or(runtime.port)))
    });
    let scheme = if tls.is_some() { "wss" } else { "ws" };
    let started = match cookie_agent_protocol::StopSignals::new() {
        Ok(signals) => runtime
            .server
            .clone()
            .serve_on(address, token.clone(), tls, runtime.allowed_origins.clone())
            .await
            .map(|listener| (signals, listener))
            .map_err(|error| anyhow::Error::new(error).context("start WebSocket daemon")),
        Err(error) => Err(anyhow::Error::new(error).context("listen for stop signals")),
    };
    let (mut signals, listener) = match started {
        Ok(started) => started,
        Err(error) => {
            runtime.server.shutdown();
            runtime.stop_catalog_refresh().await;
            runtime.engine.shutdown().await;
            return Err(error);
        }
    };
    let url = format!("{scheme}://{}/ws", listener.address());
    println!("{}", ready_line(&url, token.as_str()));
    drop(token);
    println!(
        "cookie daemon listening on {url} (client protocol {})",
        cookie_agent_protocol::CLIENT_PROTOCOL_VERSION
    );
    signals.recv().await;
    exit_on_next_stop(signals);
    runtime.server.shutdown();
    runtime.stop_catalog_refresh().await;
    listener.wait().await;
    runtime.engine.shutdown().await;
    Ok(())
}

/// Exits at once on the next stop signal, for a graceful stop that hangs.
fn exit_on_next_stop(mut signals: cookie_agent_protocol::StopSignals) {
    tokio::spawn(async move {
        signals.recv().await;
        eprintln!("cookie: stopping at once");
        std::process::exit(1);
    });
}

/// Cancels `stop` on the first stop signal; see [`exit_on_next_stop`] for
/// the second.
fn stop_on_signal(stop: CancellationToken) -> anyhow::Result<()> {
    let mut signals =
        cookie_agent_protocol::StopSignals::new().context("listen for stop signals")?;
    tokio::spawn(async move {
        signals.recv().await;
        stop.cancel();
        exit_on_next_stop(signals);
    });
    Ok(())
}

mod config_command;
mod machine_command;

#[cfg(test)]
#[path = "../../../test-support/config_harness.rs"]
mod config_harness;

#[cfg(all(test, unix))]
mod tests;
