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
use cookie_agent_engine::{Engine, EngineOptions};
use cookie_agent_models::{
    ModelManager,
    catalog::{CatalogManager, CatalogTransport, HttpCatalogTransport},
    provider_store::ProviderStore,
};
use cookie_agent_protocol::{
    AuthMethodDescriptor, ClientConnectId, ClientRequestId, EffectiveAuthState,
    McpApprovalDecision, McpApprovalRespondParams, ProviderConfigurationState,
    ProviderConnectResult, ProviderDescriptor, ProviderDisconnectParams, ProviderDisconnectResult,
    ProviderId, ProviderSupportState, RuntimeSnapshotResult, SafeCode, SafeSetupValue,
    parse_setup_value, setup_value_text,
};
#[cfg(feature = "tui")]
use cookie_agent_server::in_process_pair;
use cookie_agent_server::{Client, ClientProtocol, Server, validate_websocket_url};
use cookie_agent_tools::{BuiltinTools, delegate::DelegateToolProvider};
use serde::Serialize;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use zeroize::{Zeroize, Zeroizing};

#[cfg(test)]
use cookie_agent_protocol::{SetupFieldDescriptor, SetupFieldType};
#[cfg(test)]
use std::sync::atomic::{AtomicUsize as TestAtomicUsize, Ordering as TestOrdering};

const DEFAULT_WEBSOCKET_URL: &str = "ws://127.0.0.1:7419/ws";
const CATALOG_REFRESH_INTERVAL: Duration = Duration::from_secs(60 * 60);

#[cfg(test)]
static SECRET_VALUES_WIPED: TestAtomicUsize = TestAtomicUsize::new(0);

#[derive(Debug, Parser)]
#[command(name = "cookie")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, PartialEq, Eq, Subcommand)]
enum Command {
    /// Serve the protocol-v9 JSON-RPC WebSocket daemon on localhost.
    Daemon,
    /// Attach the TUI to an existing daemon.
    Attach {
        #[arg(long, default_value = DEFAULT_WEBSOCKET_URL)]
        url: String,
    },
    /// Securely create or update a durable global managed-provider connection.
    Connect {
        /// Exact runtime provider ID. Omit to choose interactively.
        provider_id: Option<String>,
        #[arg(long, default_value = DEFAULT_WEBSOCKET_URL)]
        url: String,
    },
    /// Remove a durable global managed-provider connection.
    Disconnect {
        /// Exact runtime provider ID. Omit to choose interactively.
        provider_id: Option<String>,
        #[arg(long, default_value = DEFAULT_WEBSOCKET_URL)]
        url: String,
    },
    /// List or respond to pending project MCP server approvals.
    Mcp {
        #[command(subcommand)]
        command: McpCommand,
        #[arg(long, default_value = DEFAULT_WEBSOCKET_URL)]
        url: String,
    },
}

#[derive(Debug, PartialEq, Eq, Subcommand)]
enum McpCommand {
    /// List project MCP servers waiting for approval.
    List,
    /// Approve one pending project MCP server.
    Approve { server: String },
    /// Reject one pending project MCP server for this daemon lifetime.
    Reject { server: String },
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
        #[cfg(test)]
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
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    let Cli { command } = Cli::parse();
    let workspace = local_workspace(&command, env::current_dir)?;
    match command {
        Some(Command::Daemon) => {
            run_daemon(compose(workspace.as_deref().expect("daemon workspace")).await?).await
        }
        Some(Command::Connect { provider_id, url }) => run_connect(&url, provider_id).await,
        Some(Command::Disconnect { provider_id, url }) => run_disconnect(&url, provider_id).await,
        Some(Command::Mcp { command, url }) => run_mcp(&url, command).await,
        Some(Command::Attach { url }) => run_attached_tui(&url).await,
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
    if configuration.runtime.server.host != "127.0.0.1" {
        anyhow::bail!("server.host must be exactly 127.0.0.1");
    }
    let port = configuration.runtime.server.port;

    let catalog_manager = open_catalog(open_transport()?);
    let catalog = Arc::new(
        catalog_manager
            .refresh()
            .await
            .context("refresh fixed models.dev catalog")?,
    );
    let provider_store = open_provider_store().context("open provider store 3")?;
    let model_manager = Arc::new(
        ModelManager::new(
            configuration.runtime.providers.clone(),
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
    engine
        .try_register_tool_provider(Arc::new(DelegateToolProvider::new(engine.clone())))
        .context("register delegate tools")?;
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
    loop {
        tokio::select! {
            () = shutdown.cancelled() => return,
            () = tokio::time::sleep(cadence) => {}
        }
        let Ok(catalog) = catalog_manager.refresh().await else {
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

fn catalog_publication_changed(
    engine: &Engine,
    catalog: &cookie_agent_models::catalog::CatalogSnapshot,
) -> bool {
    let current = engine.current_runtime();
    let current = current.models.catalog();
    current.revision != catalog.revision
        || current.source != catalog.source
        || current.state.availability != catalog.state.availability
}

fn data_dir() -> anyhow::Result<PathBuf> {
    let home = env::var_os("HOME").context("determine home directory for cookie agent data")?;
    Ok(PathBuf::from(home).join(".local/share/cookie_agent"))
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
async fn run_attached_tui(url: &str) -> anyhow::Result<()> {
    validate_websocket_url(url)?;
    let client = cookie_agent_tui::Client::connect_websocket(url)
        .await
        .context("connect to daemon WebSocket")?;
    client.handshake().await.context("handshake with daemon")?;
    cookie_agent_tui::run_with_client(client).await
}

#[cfg(not(feature = "tui"))]
async fn run_attached_tui(url: &str) -> anyhow::Result<()> {
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

async fn run_connect(url: &str, provider_id: Option<String>) -> anyhow::Result<()> {
    require_interactive_tty(
        io::stdin().is_terminal(),
        io::stdout().is_terminal(),
        io::stderr().is_terminal(),
        "connect",
    )?;
    let client = Client::connect_websocket(url).await?;
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

async fn run_disconnect(url: &str, provider_id: Option<String>) -> anyhow::Result<()> {
    require_interactive_tty(
        io::stdin().is_terminal(),
        io::stdout().is_terminal(),
        io::stderr().is_terminal(),
        "disconnect",
    )?;
    let client = Client::connect_websocket(url).await?;
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

async fn run_mcp(url: &str, command: McpCommand) -> anyhow::Result<()> {
    let client = Client::connect_websocket(url).await?;
    client.handshake().await.context("handshake with daemon")?;
    let pending = client
        .list_mcp_approvals()
        .await
        .context("mcp.approval.list failed")?
        .approvals;
    match command {
        McpCommand::List => {
            if pending.is_empty() {
                println!("No project MCP servers are pending approval.");
            }
            for approval in pending {
                println!(
                    "{}\n  connection:\n{}",
                    approval.server,
                    approval
                        .connection
                        .lines()
                        .map(|line| format!("    {line}"))
                        .collect::<Vec<_>>()
                        .join("\n")
                );
            }
            Ok(())
        }
        McpCommand::Approve { server } => {
            respond_mcp_cli(&client, pending, server, McpApprovalDecision::Approve).await
        }
        McpCommand::Reject { server } => {
            respond_mcp_cli(&client, pending, server, McpApprovalDecision::Reject).await
        }
    }
}

async fn respond_mcp_cli(
    client: &Client,
    pending: Vec<cookie_agent_protocol::McpPendingApproval>,
    server: String,
    decision: McpApprovalDecision,
) -> anyhow::Result<()> {
    let approval = pending
        .into_iter()
        .find(|approval| approval.server == server)
        .with_context(|| format!("MCP server `{server}` is not pending approval"))?;
    println!("Server: {}", approval.server);
    println!("Connection:\n{}", approval.connection);
    let prompt = match decision {
        McpApprovalDecision::Approve => "Approve this project MCP server? [y/N] ",
        McpApprovalDecision::Reject => "Reject this project MCP server? [y/N] ",
    };
    if !prompt_confirmation(prompt)? {
        anyhow::bail!("MCP approval response cancelled");
    }
    let result = client
        .respond_mcp_approval(McpApprovalRespondParams { server, decision })
        .await
        .context("mcp.approval.respond failed")?;
    println!(
        "MCP server `{}` was {}.",
        result.server,
        match result.decision {
            McpApprovalDecision::Approve => "approved",
            McpApprovalDecision::Reject => "rejected",
        }
    );
    Ok(())
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

#[cfg(not(unix))]
fn read_secret_line(_: &str) -> anyhow::Result<Zeroizing<String>> {
    anyhow::bail!("secure no-echo credential input is unavailable on this platform")
}

async fn run_daemon(mut runtime: Runtime) -> anyhow::Result<()> {
    let listener = match runtime.server.clone().serve(runtime.port).await {
        Ok(listener) => listener,
        Err(error) => {
            runtime.server.shutdown();
            runtime.stop_catalog_refresh().await;
            runtime.engine.shutdown().await;
            return Err(anyhow::Error::new(error).context("start WebSocket daemon"));
        }
    };
    println!(
        "cookie daemon listening on ws://{}/ws (protocol 9)",
        listener.address()
    );
    let signal = tokio::signal::ctrl_c().await;
    runtime.server.shutdown();
    runtime.stop_catalog_refresh().await;
    listener.wait().await;
    runtime.engine.shutdown().await;
    signal.context("wait for daemon shutdown signal")
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        future::Future,
        pin::Pin,
        sync::{
            Arc, Mutex,
            atomic::{AtomicUsize, Ordering},
        },
    };

    use cookie_agent_protocol::{
        AuthCredentialDescriptor, AuthFieldName, CredentialFieldType, EffectiveAuthState,
        ProviderConfigurationState, ProviderPresence, ProviderSupport, SafeDisplayText,
        SetupFieldId, SetupFieldValidation,
    };

    use super::*;

    #[derive(Clone)]
    struct OfflineTransport {
        fetches: Arc<AtomicUsize>,
        body: Arc<Vec<u8>>,
    }

    impl CatalogTransport for OfflineTransport {
        fn fetch(
            &self,
            _: cookie_agent_models::catalog::CatalogRequest,
        ) -> Pin<
            Box<
                dyn Future<
                        Output = Result<
                            cookie_agent_models::catalog::CatalogTransportResponse,
                            cookie_agent_models::catalog::CatalogTransportError,
                        >,
                    > + Send
                    + '_,
            >,
        > {
            self.fetches.fetch_add(1, Ordering::SeqCst);
            let body = Arc::clone(&self.body);
            Box::pin(async move {
                Ok(
                    cookie_agent_models::catalog::CatalogTransportResponse::from_bytes(
                        200,
                        body.as_ref().clone(),
                    ),
                )
            })
        }
    }

    enum CatalogStep {
        Body(Arc<Vec<u8>>),
        NotModified,
        Fail,
    }

    #[derive(Clone)]
    struct ScriptedTransport {
        fetches: Arc<AtomicUsize>,
        steps: Arc<Mutex<VecDeque<CatalogStep>>>,
    }

    #[derive(Default)]
    struct ScriptedConnectIo {
        public: VecDeque<String>,
        secrets: VecDeque<Zeroizing<String>>,
        output: Vec<String>,
    }

    impl ConnectIo for ScriptedConnectIo {
        fn write_line(&mut self, line: &str) -> anyhow::Result<()> {
            self.output.push(line.to_owned());
            Ok(())
        }

        fn read_public(&mut self, prompt: &str) -> anyhow::Result<String> {
            self.output.push(prompt.to_owned());
            self.public
                .pop_front()
                .context("missing scripted public CLI input")
        }

        fn read_secret(&mut self, prompt: &str) -> anyhow::Result<Zeroizing<String>> {
            self.output.push(prompt.to_owned());
            self.secrets
                .pop_front()
                .context("missing scripted secret CLI input")
        }
    }

    impl CatalogTransport for ScriptedTransport {
        fn fetch(
            &self,
            _: cookie_agent_models::catalog::CatalogRequest,
        ) -> Pin<
            Box<
                dyn Future<
                        Output = Result<
                            cookie_agent_models::catalog::CatalogTransportResponse,
                            cookie_agent_models::catalog::CatalogTransportError,
                        >,
                    > + Send
                    + '_,
            >,
        > {
            self.fetches.fetch_add(1, Ordering::SeqCst);
            let step = self.steps.lock().unwrap().pop_front().unwrap();
            Box::pin(async move {
                match step {
                    CatalogStep::Body(body) => Ok(
                        cookie_agent_models::catalog::CatalogTransportResponse::from_bytes(
                            200,
                            body.as_ref().clone(),
                        ),
                    ),
                    CatalogStep::NotModified => {
                        Ok(cookie_agent_models::catalog::CatalogTransportResponse::not_modified())
                    }
                    CatalogStep::Fail => {
                        Err(cookie_agent_models::catalog::CatalogTransportError::RequestFailed)
                    }
                }
            })
        }
    }

    fn unsupported_catalog() -> Arc<Vec<u8>> {
        Arc::new(br#"{"providers":{"test":{"id":"test","env":["TEST_API_KEY"],"npm":"@ai-sdk/openai-compatible","api":"https://example.invalid/v1","name":"Test","doc":"https://example.invalid/docs","models":{"group/model":{"id":"group/model","name":"Group Model","description":"test model","attachment":false,"reasoning":false,"tool_call":false,"temperature":true,"structured_output":false,"open_weights":false,"release_date":"2026-08-01","last_updated":"2026-08-02","modalities":{"input":["text"],"output":["text"]},"limit":{"context":8192,"output":1024}}}}},"models":{"group/model":{"id":"group/model","name":"Canonical","description":"metadata only","attachment":false,"reasoning":false,"tool_call":false,"temperature":false,"open_weights":false,"release_date":"2026-08-01","last_updated":"2026-08-02","modalities":{"input":["text"],"output":["text"]},"limit":{"context":8192,"output":1024}}}}"#.to_vec())
    }

    fn bedrock_catalog() -> Arc<Vec<u8>> {
        Arc::new(br#"{"providers":{"amazon-bedrock":{"id":"amazon-bedrock","env":["AWS_ACCESS_KEY_ID","AWS_BEARER_TOKEN_BEDROCK","AWS_REGION","AWS_SECRET_ACCESS_KEY"],"npm":"@ai-sdk/amazon-bedrock","name":"Amazon Bedrock","doc":"https://example.invalid/bedrock","models":{"bedrock-test":{"id":"bedrock-test","name":"Bedrock Test","description":"test model","attachment":false,"reasoning":false,"tool_call":true,"temperature":true,"structured_output":false,"open_weights":false,"release_date":"2026-08-01","last_updated":"2026-08-02","modalities":{"input":["text"],"output":["text"]},"limit":{"context":8192,"output":1024}}}}},"models":{"bedrock-test":{"id":"bedrock-test","name":"Bedrock Test","description":"test model","attachment":false,"reasoning":false,"tool_call":true,"temperature":true,"structured_output":false,"open_weights":false,"release_date":"2026-08-01","last_updated":"2026-08-02","modalities":{"input":["text"],"output":["text"]},"limit":{"context":8192,"output":1024}}}}"#.to_vec())
    }

    fn openai_catalog() -> Arc<Vec<u8>> {
        Arc::new(br#"{"providers":{"openai":{"id":"openai","env":["OPENAI_API_KEY"],"npm":"@ai-sdk/openai","name":"OpenAI","doc":"https://example.invalid/openai","models":{"gpt-test":{"id":"gpt-test","name":"GPT Test","description":"test model","attachment":false,"reasoning":false,"tool_call":true,"temperature":true,"structured_output":true,"open_weights":false,"release_date":"2026-08-01","last_updated":"2026-08-02","modalities":{"input":["text"],"output":["text"]},"limit":{"context":8192,"output":1024}}}}},"models":{"gpt-test":{"id":"gpt-test","name":"GPT Test","description":"test model","attachment":false,"reasoning":false,"tool_call":true,"temperature":true,"structured_output":true,"open_weights":false,"release_date":"2026-08-01","last_updated":"2026-08-02","modalities":{"input":["text"],"output":["text"]},"limit":{"context":8192,"output":1024}}}}"#.to_vec())
    }

    fn removed_openai_catalog() -> Arc<Vec<u8>> {
        Arc::new(br#"{"providers":{"test":{"id":"test","env":["TEST_API_KEY"],"npm":"@ai-sdk/openai-compatible","api":"https://example.invalid/v1","name":"Test","doc":"https://example.invalid/docs","models":{"group/model":{"id":"group/model","name":"Group Model","description":"test model","attachment":false,"reasoning":false,"tool_call":false,"temperature":true,"structured_output":false,"open_weights":false,"release_date":"2026-08-01","last_updated":"2026-08-02","modalities":{"input":["text"],"output":["text"]},"limit":{"context":8192,"output":1024}}}},"broken":{"id":"broken","env":["BROKEN_KEY"],"npm":"x","name":"Broken","doc":"https://example.invalid/broken","models":{},"unknown":true}},"models":{"group/model":{"id":"group/model","name":"Canonical","description":"metadata only","attachment":false,"reasoning":false,"tool_call":false,"temperature":false,"open_weights":false,"release_date":"2026-08-01","last_updated":"2026-08-02","modalities":{"input":["text"],"output":["text"]},"limit":{"context":8192,"output":1024}}}}"#.to_vec())
    }

    fn private_directory(path: &Path) {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::create_dir_all(path).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }

    fn write_empty_config(workspace: &Path) {
        use std::os::unix::fs::PermissionsExt as _;
        private_directory(workspace);
        private_directory(&workspace.join(".cookie-agent"));
        let config = workspace.join(".cookie-agent/config.toml");
        std::fs::write(&config, "[providers]\n").unwrap();
        std::fs::set_permissions(config, std::fs::Permissions::from_mode(0o600)).unwrap();
    }

    fn write_config_without_providers(workspace: &Path) {
        use std::os::unix::fs::PermissionsExt as _;
        private_directory(workspace);
        private_directory(&workspace.join(".cookie-agent"));
        let config = workspace.join(".cookie-agent/config.toml");
        std::fs::write(&config, "").unwrap();
        std::fs::set_permissions(config, std::fs::Permissions::from_mode(0o600)).unwrap();
    }

    fn provider(state: ProviderSupportState) -> ProviderDescriptor {
        ProviderDescriptor {
            id: ProviderId::new("openai").unwrap(),
            display_name: SafeDisplayText::new("OpenAI").unwrap(),
            presence: ProviderPresence::Current,
            support: ProviderSupport {
                state,
                reason: (state != ProviderSupportState::Supported)
                    .then(|| SafeCode::new("unsupported_recipe").unwrap()),
            },
            setup_fields: vec![SetupFieldDescriptor {
                id: SetupFieldId::new("region").unwrap(),
                display_name: SafeDisplayText::new("Region").unwrap(),
                help: SafeDisplayText::new("Public region").unwrap(),
                required: true,
                default: Some(SafeSetupValue::Code(SafeCode::new("us-test-1").unwrap())),
                validation: SetupFieldValidation {
                    value_type: SetupFieldType::Code,
                    min_length: Some(1),
                    max_length: Some(64),
                    minimum: None,
                    maximum: None,
                },
                safe_to_project: true,
            }],
            auth_methods: vec![AuthMethodDescriptor {
                id: cookie_agent_protocol::AuthMethodId::new("bearer-api-key-v1").unwrap(),
                display_name: SafeDisplayText::new("API key").unwrap(),
                credentials: vec![AuthCredentialDescriptor {
                    id: AuthFieldName::new("api_key").unwrap(),
                    display_name: SafeDisplayText::new("API key").unwrap(),
                    help: SafeDisplayText::new("Secret").unwrap(),
                    required: true,
                    credential_type: CredentialFieldType::ApiKey,
                }],
            }],
            configuration: ProviderConfigurationState::Unconfigured,
            effective_auth_state: EffectiveAuthState::Unavailable,
            durable_connection: None,
            quarantine: None,
        }
    }

    #[test]
    fn cli_commands_and_removed_flags_are_exact() {
        assert_eq!(Cli::try_parse_from(["cookie"]).unwrap().command, None);
        assert_eq!(
            Cli::try_parse_from(["cookie", "daemon"]).unwrap().command,
            Some(Command::Daemon)
        );
        assert!(Cli::try_parse_from(["cookie", "--trust-workspace", "daemon"]).is_err());
        assert!(
            Cli::try_parse_from(["cookie", "connect", "openai", "--api-key", "sentinel"]).is_err()
        );
        assert_eq!(
            Cli::try_parse_from(["cookie", "mcp", "approve", "github"])
                .unwrap()
                .command,
            Some(Command::Mcp {
                command: McpCommand::Approve {
                    server: "github".into()
                },
                url: DEFAULT_WEBSOCKET_URL.into(),
            })
        );
    }

    #[test]
    fn attach_connect_and_disconnect_are_cwd_independent() {
        for command in [
            Command::Attach {
                url: DEFAULT_WEBSOCKET_URL.into(),
            },
            Command::Connect {
                provider_id: None,
                url: DEFAULT_WEBSOCKET_URL.into(),
            },
            Command::Disconnect {
                provider_id: None,
                url: DEFAULT_WEBSOCKET_URL.into(),
            },
            Command::Mcp {
                command: McpCommand::List,
                url: DEFAULT_WEBSOCKET_URL.into(),
            },
        ] {
            assert!(
                local_workspace(&Some(command), || panic!("cwd inspected"))
                    .unwrap()
                    .is_none()
            );
        }
    }

    #[test]
    fn websocket_endpoints_are_loopback_only() {
        for url in [
            "ws://127.0.0.1:7419/ws",
            "wss://localhost:7419/ws",
            "ws://[::1]:7419/ws",
        ] {
            validate_websocket_url(url).unwrap();
        }
        for url in [
            "http://127.0.0.1:7419/ws",
            "ws://example.com:7419/ws",
            "ws://user:pass@127.0.0.1:7419/ws",
            "ws://127.0.0.1:7419/other",
        ] {
            assert!(validate_websocket_url(url).is_err(), "{url}");
        }
    }

    #[test]
    fn provider_connect_requires_support_before_submission() {
        ensure_supported(&provider(ProviderSupportState::Supported)).unwrap();
        assert!(ensure_supported(&provider(ProviderSupportState::Unsupported)).is_err());
        assert!(ensure_supported(&provider(ProviderSupportState::Quarantined)).is_err());

        let mut removed = provider(ProviderSupportState::Supported);
        removed.presence = ProviderPresence::Removed;
        ensure_supported(&removed).unwrap();

        removed.support = ProviderSupport {
            state: ProviderSupportState::Unsupported,
            reason: Some(SafeCode::new("removed_without_retained_recipe_match").unwrap()),
        };
        assert!(
            ensure_supported(&removed)
                .unwrap_err()
                .to_string()
                .contains("removed_without_retained_recipe_match")
        );
    }

    #[test]
    fn authored_configuration_is_not_treated_as_connected_when_auth_is_incomplete() {
        let mut authored = provider(ProviderSupportState::Supported);
        authored.configuration = ProviderConfigurationState::Authored;
        authored.effective_auth_state = EffectiveAuthState::Unavailable;
        assert_eq!(
            connect_confirmation_prompt(&authored),
            "Complete setup and authentication for this authored provider? [y/N] "
        );
    }

    #[test]
    fn authored_complete_provider_is_offered_global_store_connect_not_reconnect() {
        let mut authored = provider(ProviderSupportState::Supported);
        authored.configuration = ProviderConfigurationState::Authored;
        authored.effective_auth_state = EffectiveAuthState::AuthoredApiKey;
        authored.durable_connection = None;
        assert_eq!(
            connect_confirmation_prompt(&authored),
            "Connect this provider? [y/N] "
        );
    }

    #[test]
    fn setup_and_auth_prompts_are_separate_and_blank_secret_is_rejected() {
        let provider = provider(ProviderSupportState::Supported);
        let mut public_prompts = Vec::new();
        let setup = collect_setup_values(&provider, |prompt| {
            public_prompts.push(prompt.to_owned());
            Ok(String::new())
        })
        .unwrap();
        assert_eq!(setup.len(), 1);
        assert!(public_prompts[0].contains("Region"));

        let mut secret_prompts = Vec::new();
        let error = collect_auth_values(&provider.auth_methods[0], |prompt| {
            secret_prompts.push(prompt.to_owned());
            Ok(Zeroizing::new(String::new()))
        })
        .unwrap_err();
        assert!(error.to_string().contains("was blank"));
        assert!(secret_prompts[0].contains("secret"));
    }

    #[test]
    fn sensitive_connect_serialization_has_current_contract_and_drop_wipes_source() {
        let before = SECRET_VALUES_WIPED.load(TestOrdering::SeqCst);
        let mut params = SensitiveProviderConnectParams {
            provider_id: ProviderId::new("openai").unwrap(),
            expected_catalog_revision: cookie_agent_protocol::CatalogRevision::new(format!(
                "sha256:{}",
                "0".repeat(64)
            ))
            .unwrap(),
            setup_values: BTreeMap::new(),
            auth_method: cookie_agent_protocol::AuthMethodId::new("bearer-api-key-v1").unwrap(),
            auth_values: SecretValues(BTreeMap::from([(
                "api_key".into(),
                "invented-test-placeholder".into(),
            )])),
            client_connect_id: ClientConnectId::new("connect-1").unwrap(),
        };
        let encoded = serde_json::to_value(&params).unwrap();
        assert!(encoded.get("expected_catalog_revision").is_some());
        assert!(encoded.get("setup_values").is_some());
        assert!(encoded.get("auth_values").is_some());
        assert!(encoded.get("credentials").is_none());
        for value in params.auth_values.0.values_mut() {
            value.zeroize();
            assert!(value.is_empty());
        }
        drop(params);
        assert!(SECRET_VALUES_WIPED.load(TestOrdering::SeqCst) > before);
    }

    #[tokio::test]
    async fn empty_startup_uses_injected_offline_catalog_and_composes_before_frontend() {
        let temporary = tempfile::tempdir().unwrap();
        private_directory(temporary.path());
        let workspace = temporary.path().join("workspace");
        let cache_anchor = temporary.path().join("cache-anchor");
        let provider_store = temporary.path().join("provider-store");
        let data = temporary.path().join("data");
        write_empty_config(&workspace);
        private_directory(&cache_anchor);
        let fetches = Arc::new(AtomicUsize::new(0));
        let mut runtime = compose_with(
            &workspace,
            || {
                Ok(OfflineTransport {
                    fetches: Arc::clone(&fetches),
                    body: unsupported_catalog(),
                })
            },
            |transport| CatalogManager::in_directory(transport, &cache_anchor, "catalog"),
            || ProviderStore::open(&provider_store),
            || Ok(data),
        )
        .await
        .unwrap();
        assert_eq!(fetches.load(Ordering::SeqCst), 1);
        let snapshot = runtime.engine.runtime_snapshot().unwrap();
        assert!(snapshot.snapshot.models.is_empty());
        assert_eq!(snapshot.snapshot.providers.len(), 1);
        runtime.server.shutdown();
        runtime.stop_catalog_refresh().await;
        runtime.engine.shutdown().await;
    }

    #[tokio::test]
    async fn leftover_schema_field_fails_before_catalog_or_provider_store_open() {
        let temporary = tempfile::tempdir().unwrap();
        private_directory(temporary.path());
        let workspace = temporary.path().join("workspace");
        write_empty_config(&workspace);
        std::fs::write(
            workspace.join(".cookie-agent/config.toml"),
            "schema_version = 10\n",
        )
        .unwrap();
        let fetches = Arc::new(AtomicUsize::new(0));
        let catalog_opens = Arc::new(AtomicUsize::new(0));
        let provider_opens = Arc::new(AtomicUsize::new(0));
        let catalog_anchor = temporary.path().to_owned();
        let provider_path = temporary.path().join("providers");
        let data_path = temporary.path().join("data");
        let result = compose_with(
            &workspace,
            || {
                Ok(OfflineTransport {
                    fetches: Arc::clone(&fetches),
                    body: unsupported_catalog(),
                })
            },
            {
                let catalog_opens = Arc::clone(&catalog_opens);
                move |transport| {
                    catalog_opens.fetch_add(1, Ordering::SeqCst);
                    CatalogManager::in_directory(transport, catalog_anchor, "catalog")
                }
            },
            {
                let provider_opens = Arc::clone(&provider_opens);
                move || {
                    provider_opens.fetch_add(1, Ordering::SeqCst);
                    ProviderStore::open(provider_path)
                }
            },
            || Ok(data_path),
        )
        .await;
        assert!(result.is_err());
        assert_eq!(catalog_opens.load(Ordering::SeqCst), 0);
        assert_eq!(provider_opens.load(Ordering::SeqCst), 0);
        assert_eq!(fetches.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn global_setup_and_auth_provider_recomposes_across_two_workspaces() {
        let temporary = tempfile::tempdir().unwrap();
        private_directory(temporary.path());
        let workspace_a = temporary.path().join("workspace-a");
        let workspace_b = temporary.path().join("workspace-b");
        let cache_anchor = temporary.path().join("cache-anchor");
        let provider_path = temporary.path().join("provider-store");
        let data_path = temporary.path().join("data");
        write_config_without_providers(&workspace_a);
        write_config_without_providers(&workspace_b);
        private_directory(&cache_anchor);
        let fetches = Arc::new(AtomicUsize::new(0));

        let seed_catalog = Arc::new(
            CatalogManager::in_directory(
                OfflineTransport {
                    fetches: Arc::clone(&fetches),
                    body: bedrock_catalog(),
                },
                &cache_anchor,
                "catalog",
            )
            .refresh()
            .await
            .unwrap(),
        );
        let provider_id = ProviderId::new("amazon-bedrock").unwrap();
        let authored = cookie_agent_models::ProviderDefinition::ModelsDev(
            cookie_agent_models::authoring::ModelsDevProvider {
                base_url: None,
                setup: BTreeMap::from([(
                    SetupFieldId::new("region").unwrap(),
                    cookie_agent_models::SafeSetupValue::String(
                        cookie_agent_models::BoundedSetupString::new("us-test-1").unwrap(),
                    ),
                )]),
                api_key: None,
                auth_override: None,
                shape: None,
                model_overrides: BTreeMap::new(),
            },
        );
        let seed_manager = ModelManager::new(
            BTreeMap::from([(provider_id.clone(), authored)]),
            Arc::clone(&seed_catalog),
            ProviderStore::open(&provider_path).unwrap(),
        )
        .unwrap();
        let seed_request = cookie_agent_models::ProviderConnectRequest {
            provider_id: provider_id.clone(),
            expected_catalog_revision: seed_catalog.revision.clone(),
            setup_values: BTreeMap::from([(
                SetupFieldId::new("region").unwrap(),
                cookie_agent_models::SafeSetupValue::String(
                    cookie_agent_models::BoundedSetupString::new("us-test-1").unwrap(),
                ),
            )]),
            auth_method: cookie_agent_protocol::AuthMethodId::new("aws-sigv4-credentials-v1")
                .unwrap(),
            auth_values: cookie_agent_models::provider_store::ProviderAuthValues::new(
                BTreeMap::from([
                    (
                        cookie_agent_protocol::AuthFieldName::new("access_key_id").unwrap(),
                        "invented-access-placeholder".into(),
                    ),
                    (
                        cookie_agent_protocol::AuthFieldName::new("secret_access_key").unwrap(),
                        "invented-secret-placeholder".into(),
                    ),
                ]),
            )
            .unwrap(),
            client_connect_id: cookie_agent_models::provider_store::ClientConnectId::new(
                "store-backed-connect-1",
            )
            .unwrap(),
        };
        let seeded = seed_manager
            .connect(seed_request.clone(), |_, _| Ok(()))
            .unwrap();
        assert!(!seeded.replayed);
        assert!(
            seed_manager
                .connect(seed_request, |_, _| Ok(()))
                .unwrap()
                .replayed
        );
        drop(seed_manager);

        let mut runtime = compose_with(
            &workspace_a,
            || {
                Ok(OfflineTransport {
                    fetches: Arc::clone(&fetches),
                    body: bedrock_catalog(),
                })
            },
            |transport| CatalogManager::in_directory(transport, &cache_anchor, "catalog"),
            || ProviderStore::open(&provider_path),
            || Ok(data_path.clone()),
        )
        .await
        .unwrap();
        let snapshot_a = runtime.engine.runtime_snapshot().unwrap().snapshot;
        let provider_a = snapshot_a
            .providers
            .iter()
            .find(|provider| provider.id == provider_id)
            .unwrap();
        assert_eq!(
            provider_a
                .durable_connection
                .as_ref()
                .unwrap()
                .setup_values
                .get(&SetupFieldId::new("region").unwrap())
                .map(setup_value_text),
            Some("us-test-1".into())
        );
        assert_eq!(
            provider_a
                .durable_connection
                .as_ref()
                .unwrap()
                .credential_fields
                .len(),
            2
        );
        runtime.server.shutdown();
        runtime.stop_catalog_refresh().await;
        runtime.engine.shutdown().await;

        let mut recomposed = compose_with(
            &workspace_b,
            || {
                Ok(OfflineTransport {
                    fetches,
                    body: bedrock_catalog(),
                })
            },
            |transport| CatalogManager::in_directory(transport, &cache_anchor, "catalog"),
            || ProviderStore::open(&provider_path),
            || Ok(data_path),
        )
        .await
        .unwrap();
        let snapshot = recomposed.engine.runtime_snapshot().unwrap().snapshot;
        let provider = snapshot
            .providers
            .iter()
            .find(|provider| provider.id.as_str() == "amazon-bedrock")
            .unwrap();
        assert!(provider.durable_connection.is_some());
        assert_eq!(provider.configuration, ProviderConfigurationState::Stored);
        assert_eq!(
            provider
                .durable_connection
                .as_ref()
                .unwrap()
                .credential_fields
                .len(),
            2
        );
        recomposed.server.shutdown();
        recomposed.stop_catalog_refresh().await;
        recomposed.engine.shutdown().await;
    }

    #[tokio::test]
    async fn connect_reconnect_and_disconnect_are_idempotent() {
        let temporary = tempfile::tempdir().unwrap();
        private_directory(temporary.path());
        let workspace = temporary.path().join("workspace");
        let cache_anchor = temporary.path().join("cache-anchor");
        write_empty_config(&workspace);
        private_directory(&cache_anchor);
        let mut runtime = compose_with(
            &workspace,
            || {
                Ok(OfflineTransport {
                    fetches: Arc::new(AtomicUsize::new(0)),
                    body: openai_catalog(),
                })
            },
            |transport| CatalogManager::in_directory(transport, &cache_anchor, "catalog"),
            || ProviderStore::open(temporary.path().join("providers")),
            || Ok(temporary.path().join("data")),
        )
        .await
        .unwrap();
        let snapshot = runtime.engine.runtime_snapshot().unwrap().snapshot;
        let connect_json = serde_json::json!({
            "provider_id": "openai",
            "expected_catalog_revision": snapshot.catalog_revision,
            "setup_values": {},
            "auth_method": "bearer-api-key-v1",
            "auth_values": {"api_key": "invented-test-placeholder"},
            "client_connect_id": "connect-replay-1"
        });
        let connected = runtime
            .engine
            .connect_provider(serde_json::from_value(connect_json.clone()).unwrap())
            .unwrap();
        assert!(!connected.replayed);
        assert!(
            runtime
                .engine
                .connect_provider(serde_json::from_value(connect_json).unwrap())
                .unwrap()
                .replayed
        );
        let disconnect = ProviderDisconnectParams {
            provider_id: ProviderId::new("openai").unwrap(),
            expected_runtime_revision: connected.runtime.runtime_revision,
            expected_provider_state_revision: connected.runtime.provider_state_revision,
            expected_connection_generation: Some(
                connected.durable_connection.connection_generation,
            ),
            client_request_id: ClientRequestId::new("disconnect-replay-1").unwrap(),
        };
        assert!(
            !runtime
                .engine
                .disconnect_provider(disconnect.clone())
                .unwrap()
                .replayed
        );
        assert!(
            runtime
                .engine
                .disconnect_provider(disconnect)
                .unwrap()
                .replayed
        );
        runtime.server.shutdown();
        runtime.stop_catalog_refresh().await;
        runtime.engine.shutdown().await;
    }

    #[tokio::test]
    async fn cli_reconnects_supported_removed_provider_through_real_server() {
        let temporary = tempfile::tempdir().unwrap();
        private_directory(temporary.path());
        let workspace = temporary.path().join("workspace");
        let cache_anchor = temporary.path().join("cache-anchor");
        write_empty_config(&workspace);
        private_directory(&cache_anchor);
        let mut runtime = compose_with(
            &workspace,
            || {
                Ok(OfflineTransport {
                    fetches: Arc::new(AtomicUsize::new(0)),
                    body: openai_catalog(),
                })
            },
            |transport| CatalogManager::in_directory(transport, &cache_anchor, "catalog"),
            || ProviderStore::open(temporary.path().join("providers")),
            || Ok(temporary.path().join("data")),
        )
        .await
        .unwrap();
        let initial = runtime.engine.runtime_snapshot().unwrap().snapshot;
        let connected = runtime
            .engine
            .connect_provider(
                serde_json::from_value(serde_json::json!({
                    "provider_id": "openai",
                    "expected_catalog_revision": initial.catalog_revision,
                    "setup_values": {},
                    "auth_method": "bearer-api-key-v1",
                    "auth_values": {"api_key": "invented-initial-placeholder"},
                    "client_connect_id": "removed-provider-initial-connect"
                }))
                .unwrap(),
            )
            .unwrap();
        let initial_generation = connected.durable_connection.connection_generation;

        let removed = CatalogManager::in_directory(
            OfflineTransport {
                fetches: Arc::new(AtomicUsize::new(0)),
                body: removed_openai_catalog(),
            },
            &cache_anchor,
            "removed-catalog",
        )
        .refresh()
        .await
        .unwrap();
        runtime.engine.refresh_catalog(Arc::new(removed)).unwrap();
        let snapshot = runtime.engine.runtime_snapshot().unwrap().snapshot;
        let removed_provider = snapshot
            .providers
            .iter()
            .find(|provider| provider.id.as_str() == "openai")
            .unwrap();
        assert_eq!(removed_provider.presence, ProviderPresence::Removed);
        assert_eq!(
            removed_provider.support.state,
            ProviderSupportState::Supported
        );

        let before_wipe = SECRET_VALUES_WIPED.load(TestOrdering::SeqCst);
        let (client_stream, server_stream) = cookie_agent_server::in_process_pair(32);
        let server_task = tokio::spawn(runtime.server.clone().serve_stream(server_stream));
        let client = Client::connect_stream(client_stream);
        let mut io = ScriptedConnectIo {
            public: VecDeque::from(["openai".into(), "yes".into()]),
            secrets: VecDeque::from([Zeroizing::new("invented-reconnect-placeholder".into())]),
            output: Vec::new(),
        };
        run_connect_with(&client, None, &mut io).await.unwrap();
        let output = io.output.join("\n");
        assert!(output.contains("openai"));
        assert!(output.contains("Presence: Removed"));
        assert!(output.contains("provider.connect succeeded"));
        assert!(SECRET_VALUES_WIPED.load(TestOrdering::SeqCst) > before_wipe);
        drop(client);
        server_task.await.unwrap().unwrap();
        let reconnected = runtime
            .engine
            .runtime_snapshot()
            .unwrap()
            .snapshot
            .providers
            .into_iter()
            .find(|provider| provider.id.as_str() == "openai")
            .unwrap()
            .durable_connection
            .unwrap();
        assert!(reconnected.connection_generation > initial_generation);

        let blocked = runtime
            .engine
            .runtime_snapshot()
            .unwrap()
            .snapshot
            .providers;
        assert_eq!(
            blocked
                .iter()
                .find(|provider| provider.id.as_str() == "test")
                .unwrap()
                .support
                .state,
            ProviderSupportState::Supported
        );
        for (provider_id, state) in [("broken", ProviderSupportState::Quarantined)] {
            let descriptor = blocked
                .iter()
                .find(|provider| provider.id.as_str() == provider_id)
                .unwrap();
            assert_eq!(descriptor.support.state, state);
            let reason = descriptor
                .support
                .reason
                .as_ref()
                .unwrap()
                .as_str()
                .to_owned();
            let (client_stream, server_stream) = cookie_agent_server::in_process_pair(32);
            let server_task = tokio::spawn(runtime.server.clone().serve_stream(server_stream));
            let client = Client::connect_stream(client_stream);
            let mut io = ScriptedConnectIo::default();
            let error = run_connect_with(&client, Some(provider_id.into()), &mut io)
                .await
                .unwrap_err();
            assert!(error.to_string().contains(&reason), "{error:#}");
            assert!(
                io.output
                    .iter()
                    .any(|line| line.contains(&format!("Support: {state:?}")))
            );
            assert!(io.secrets.is_empty());
            drop(client);
            server_task.await.unwrap().unwrap();
        }

        runtime.server.shutdown();
        runtime.stop_catalog_refresh().await;
        runtime.engine.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn hourly_catalog_refresh_skips_noops_publishes_fallback_once_and_cancels() {
        let temporary = tempfile::tempdir().unwrap();
        private_directory(temporary.path());
        let workspace = temporary.path().join("workspace");
        let cache_anchor = temporary.path().join("cache-anchor");
        write_empty_config(&workspace);
        private_directory(&cache_anchor);
        let fetches = Arc::new(AtomicUsize::new(0));
        let steps = Arc::new(Mutex::new(VecDeque::from([
            CatalogStep::Body(unsupported_catalog()),
            CatalogStep::NotModified,
            CatalogStep::Fail,
            CatalogStep::NotModified,
            CatalogStep::NotModified,
        ])));
        let mut runtime = compose_with(
            &workspace,
            || {
                Ok(ScriptedTransport {
                    fetches: Arc::clone(&fetches),
                    steps: Arc::clone(&steps),
                })
            },
            |transport| CatalogManager::in_directory(transport, &cache_anchor, "catalog"),
            || ProviderStore::open(temporary.path().join("providers")),
            || Ok(temporary.path().join("data")),
        )
        .await
        .unwrap();
        let mut changes = runtime.engine.subscribe_runtime_changes();
        tokio::task::yield_now().await;

        tokio::time::advance(CATALOG_REFRESH_INTERVAL).await;
        tokio::task::yield_now().await;
        assert_eq!(fetches.load(Ordering::SeqCst), 2);
        assert!(matches!(
            changes.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ));

        tokio::time::advance(CATALOG_REFRESH_INTERVAL).await;
        tokio::task::yield_now().await;
        let changed = changes.recv().await.unwrap();
        assert_eq!(
            changed.reasons,
            vec![cookie_agent_protocol::RuntimeChangeReason::CatalogFallback]
        );

        tokio::time::advance(CATALOG_REFRESH_INTERVAL).await;
        tokio::task::yield_now().await;
        let changed = changes.recv().await.unwrap();
        assert_eq!(
            changed.reasons,
            vec![cookie_agent_protocol::RuntimeChangeReason::CatalogRefreshed]
        );

        tokio::time::advance(CATALOG_REFRESH_INTERVAL).await;
        tokio::task::yield_now().await;
        assert!(matches!(
            changes.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ));
        let before_shutdown = fetches.load(Ordering::SeqCst);
        runtime.stop_catalog_refresh().await;
        tokio::time::advance(CATALOG_REFRESH_INTERVAL).await;
        tokio::task::yield_now().await;
        assert_eq!(fetches.load(Ordering::SeqCst), before_shutdown);
        runtime.server.shutdown();
        runtime.engine.shutdown().await;
    }
}
