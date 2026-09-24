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
use cookie_agent::run;
use cookie_agent_engine::{Engine, EngineOptions};
use cookie_agent_models::{
    ModelManager,
    catalog::{CatalogManager, CatalogTransport, HttpCatalogTransport},
    provider_store::ProviderStore,
};
use cookie_agent_protocol::{
    AuthMethodDescriptor, ClientConnectId, ClientRequestId, EffectiveAuthState, McpAuthBeginParams,
    McpServerState, ProviderConfigurationState, ProviderConnectResult, ProviderDescriptor,
    ProviderDisconnectParams, ProviderDisconnectResult, ProviderId, ProviderSupportState,
    RuntimeSnapshotResult, SafeCode, SafeSetupValue, parse_setup_value, paths, setup_value_text,
};
use cookie_agent_server::{
    Client, ClientProtocol, Server, generate_token, in_process_pair, ready_line,
    validate_websocket_url,
};
use cookie_agent_tools::{
    BuiltinTools, delegate::DelegateToolProvider, message::MessageToolProvider,
};
use serde::Serialize;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use zeroize::{Zeroize, Zeroizing};

#[cfg(all(test, unix))]
use cookie_agent_protocol::{SetupFieldDescriptor, SetupFieldType};
#[cfg(all(test, unix))]
use std::sync::atomic::{AtomicUsize as TestAtomicUsize, Ordering as TestOrdering};

const DEFAULT_WEBSOCKET_URL: &str = "ws://127.0.0.1:7419/ws";
const CATALOG_REFRESH_INTERVAL: Duration = Duration::from_secs(60 * 60);

enum RunCatalogTransport {
    Http(HttpCatalogTransport),
    #[cfg(debug_assertions)]
    Bundled,
}

impl CatalogTransport for RunCatalogTransport {
    fn fetch(
        &self,
        request: cookie_agent_models::catalog::CatalogRequest,
    ) -> cookie_agent_models::catalog::CatalogTransportFuture<'_> {
        match self {
            Self::Http(transport) => transport.fetch(request),
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
}

#[derive(Debug, PartialEq, Eq, Subcommand)]
enum Command {
    /// Run one prompt without the interactive TUI.
    Run {
        #[command(flatten)]
        args: Box<run::RunArgs>,
    },
    /// Serve the exact cookie-agent protocol 20 JSON-RPC WebSocket daemon on localhost.
    Daemon {
        /// Localhost port to bind. `0` selects an ephemeral port; the ready line
        /// reports the real one.
        #[arg(long)]
        port: Option<u16>,
    },
    /// Attach the TUI to an existing daemon.
    Attach {
        #[arg(long, default_value = DEFAULT_WEBSOCKET_URL)]
        url: String,
        #[arg(long, env = "COOKIE_DAEMON_TOKEN", hide_env_values = true)]
        token: String,
    },
    /// Securely create or update a durable global managed-provider connection.
    Connect {
        /// Exact runtime provider ID. Omit to choose interactively.
        provider_id: Option<String>,
        #[arg(long, default_value = DEFAULT_WEBSOCKET_URL)]
        url: String,
        #[arg(long, env = "COOKIE_DAEMON_TOKEN", hide_env_values = true)]
        token: String,
    },
    /// Remove a durable global managed-provider connection.
    Disconnect {
        /// Exact runtime provider ID. Omit to choose interactively.
        provider_id: Option<String>,
        #[arg(long, default_value = DEFAULT_WEBSOCKET_URL)]
        url: String,
        #[arg(long, env = "COOKIE_DAEMON_TOKEN", hide_env_values = true)]
        token: String,
    },
    /// List MCP servers or start remote-server OAuth.
    Mcp {
        #[command(subcommand)]
        command: McpCommand,
        #[arg(long, default_value = DEFAULT_WEBSOCKET_URL)]
        url: String,
        #[arg(long, env = "COOKIE_DAEMON_TOKEN", hide_env_values = true)]
        token: String,
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

#[derive(Serialize)]
struct SensitiveProviderConnectParams {
    provider_id: ProviderId,
    expected_catalog_revision: cookie_agent_protocol::CatalogRevision,
    setup_values: BTreeMap<cookie_agent_protocol::SetupFieldId, SafeSetupValue>,
    auth_method: cookie_agent_protocol::AuthMethodId,
    auth_values: SecretValues,
    client_connect_id: ClientConnectId,
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
    if let Err(error) = main_result().await {
        eprintln!(
            "cookie: {}",
            cookie_agent_protocol::diagnostics::detail(&format!("{error:#}"))
        );
        std::process::exit(1);
    }
}

async fn main_result() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    let Cli { command } = Cli::parse();
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
        Some(Command::Daemon { port }) => {
            run_daemon(
                compose(workspace.as_deref().expect("daemon workspace")).await?,
                port,
            )
            .await
        }
        Some(Command::Connect {
            provider_id,
            url,
            token,
        }) => run_connect(&url, &token, provider_id).await,
        Some(Command::Disconnect {
            provider_id,
            url,
            token,
        }) => run_disconnect(&url, &token, provider_id).await,
        Some(Command::Mcp {
            command,
            url,
            token,
        }) => run_mcp(&url, &token, command).await,
        Some(Command::Attach { url, token }) => run_attached_tui(&url, &token).await,
        None => {
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
                | Command::Mcp { .. },
        )
    ) {
        Ok(None)
    } else {
        current_dir()
            .context("determine current workspace")
            .map(Some)
    }
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
        cookie_agent_config::load(workspace).context("load workspace configuration and agents")?;
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
    let engine = Engine::open(EngineOptions {
        data_dir: open_data_dir()?,
        cwd: workspace.to_owned(),
        config: configuration,
        model_manager,
        tools: vec![Arc::new(BuiltinTools::new(workspace))],
    })
    .context("open manifests, rehydrate project state, and reconcile engine")?;
    if let Err(error) = engine
        .try_register_tool_provider(Arc::new(DelegateToolProvider::new(engine.clone())))
        .context("register delegate tools")
    {
        engine.shutdown().await;
        return Err(error);
    }
    if let Err(error) = engine
        .try_register_tool_provider(Arc::new(MessageToolProvider::new(engine.clone())))
        .context("register message tools")
    {
        engine.shutdown().await;
        return Err(error);
    }
    if let Err(error) = engine
        .try_register_tool_provider(Arc::new(cookie_agent_tools::skill::SkillTool::new(
            engine.clone(),
        )))
        .context("register skill tool")
    {
        engine.shutdown().await;
        return Err(error);
    }
    if let Err(error) = engine
        .try_register_tool_provider(Arc::new(cookie_agent_tools::goal::GoalTools::new(
            engine.clone(),
        )))
        .context("register goal tools")
    {
        engine.shutdown().await;
        return Err(error);
    }
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

fn data_dir() -> anyhow::Result<PathBuf> {
    paths::user_data_root().context("determine home directory for cookie agent data")
}

#[cfg(feature = "tui")]
async fn run_local_frontend(mut runtime: Runtime) -> anyhow::Result<()> {
    let (client_stream, server_stream) = in_process_pair(128);
    let server_task = tokio::spawn(runtime.server.clone().serve_stream(server_stream));
    let result = async {
        let client = cookie_agent_tui::Client::connect_stream(client_stream);
        client.handshake().await.context("handshake with daemon")?;
        cookie_agent_tui::run_with_new_session(client).await
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
    let client = cookie_agent_tui::Client::connect_websocket_with_token(url, token)
        .await
        .context("connect to daemon WebSocket")?;
    client.handshake().await.context("handshake with daemon")?;
    cookie_agent_tui::run_with_client(client).await
}

#[cfg(not(feature = "tui"))]
async fn run_attached_tui(url: &str, _token: &str) -> anyhow::Result<()> {
    validate_websocket_url(url)?;
    anyhow::bail!("cookie was built without TUI support")
}

async fn runtime_snapshot(
    client: &(impl ClientProtocol + ?Sized),
) -> anyhow::Result<RuntimeSnapshotResult> {
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
    client.handshake().await.context("handshake with daemon")?;
    let runtime = runtime_snapshot(client).await?;
    let provider = choose_provider(&runtime.snapshot.providers, provider_id, io)?;
    print_provider_details(provider, runtime.snapshot.catalog_revision.as_str(), io)?;
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

    let setup_values = collect_setup_values(provider, |prompt| io.read_public(prompt))?;
    let auth_method = choose_auth_method(provider, io)?;
    let auth_values = collect_auth_values(auth_method, |prompt| io.read_secret(prompt))?;
    let result: ProviderConnectResult = client
        .call_sensitive(
            "provider.connect",
            SensitiveProviderConnectParams {
                provider_id: provider.id.clone(),
                expected_catalog_revision: runtime.snapshot.catalog_revision,
                setup_values,
                auth_method: auth_method.id.clone(),
                auth_values,
                client_connect_id: ClientConnectId::new(Uuid::now_v7().to_string())
                    .expect("UUID is a valid connect ID"),
            },
        )
        .await
        .context("provider.connect failed")?;
    io.write_line(&format!(
        "provider.connect succeeded for {} at runtime revision {}{}.",
        result.durable_connection.provider_id,
        result.runtime.runtime_revision,
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
    client.handshake().await.context("handshake with daemon")?;
    let runtime = runtime_snapshot(&client).await?;
    let provider = choose_provider(&runtime.snapshot.providers, provider_id, &mut io)?;
    print_provider_details(
        provider,
        runtime.snapshot.catalog_revision.as_str(),
        &mut io,
    )?;
    println!("Scope: durable, global (available to all workspaces).");
    if !prompt_confirmation("Disconnect this managed provider? [y/N] ")? {
        anyhow::bail!("provider disconnection cancelled");
    }
    let result: ProviderDisconnectResult = client
        .disconnect_provider(ProviderDisconnectParams {
            provider_id: provider.id.clone(),
            expected_runtime_revision: runtime.snapshot.runtime_revision,
            expected_provider_state_revision: runtime.snapshot.provider_state_revision,
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
        "provider.disconnect succeeded for {} at runtime revision {}{}.",
        result.provider_id,
        result.runtime.snapshot.runtime_revision,
        if result.replayed { " (replayed)" } else { "" }
    );
    Ok(())
}

async fn run_mcp(url: &str, token: &str, command: McpCommand) -> anyhow::Result<()> {
    let client = Client::connect_websocket_with_token(url, token).await?;
    client.handshake().await.context("handshake with daemon")?;
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
                println!(
                    "{}\t{} tools\t{:?}",
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
                .find(|candidate| candidate.name == server)
                .with_context(|| format!("unknown MCP server `{server}`"))?;
            if status.state != McpServerState::NeedsAuth {
                anyhow::bail!(
                    "MCP server `{server}` does not currently require OAuth authorization"
                );
            }
            let result = client
                .begin_mcp_auth(McpAuthBeginParams { server })
                .await
                .context("mcp.auth.begin failed")?;
            println!(
                "Open this URL to authenticate MCP server `{}`:",
                result.server
            );
            println!("{}", result.authorization_url);
            Ok(())
        }
    }
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
            "  {}. {} ({}) — {}",
            index + 1,
            provider.display_name,
            provider.id,
            support
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

async fn run_daemon(mut runtime: Runtime, port_override: Option<u16>) -> anyhow::Result<()> {
    let token = generate_token().context("generate daemon authentication token")?;
    let port = port_override.unwrap_or(runtime.port);
    let listener = match runtime.server.clone().serve(port, token.clone()).await {
        Ok(listener) => listener,
        Err(error) => {
            runtime.server.shutdown();
            runtime.stop_catalog_refresh().await;
            runtime.engine.shutdown().await;
            return Err(anyhow::Error::new(error).context("start WebSocket daemon"));
        }
    };
    let url = format!("ws://{}/ws", listener.address());
    println!("{}", ready_line(&url, token.as_str()));
    drop(token);
    println!(
        "cookie daemon listening on {url} (protocol {})",
        cookie_agent_protocol::PROTOCOL_VERSION
    );
    let signal = tokio::signal::ctrl_c().await;
    runtime.server.shutdown();
    runtime.stop_catalog_refresh().await;
    listener.wait().await;
    runtime.engine.shutdown().await;
    signal.context("wait for daemon shutdown signal")
}

#[cfg(test)]
#[path = "../../../test-support/config_harness.rs"]
mod config_harness;

#[cfg(all(test, unix))]
mod tests;
