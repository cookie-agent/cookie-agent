use std::{
    collections::BTreeMap,
    env,
    io::{self, IsTerminal, Write},
    net::IpAddr,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::Context as _;
use clap::{Parser, Subcommand};
use cookie_agent_engine::{Engine, EngineOptions};
use cookie_agent_models::{Catalog, CredentialStore, ModelSetManager};
use cookie_agent_protocol::{
    AgentDescriptor, AgentListParams, AgentListResult, CatalogProvider, CatalogProviderListParams,
    CatalogProviderListResult, ClientConnectId, ClientHello, CredentialFieldName, JsonRpcId,
    ModelListParams, ModelListResult, Notification, ProtocolVersion, ProviderConnectParams,
    ProviderConnectResult, ProviderCredentials, Request, Response,
};
#[cfg(feature = "tui")]
use cookie_agent_server::in_process_pair;
use cookie_agent_server::{Server, load_auth_token};
use cookie_agent_tools::{BuiltinTools, delegate::DelegateToolProvider};
use futures_util::{SinkExt as _, StreamExt as _};
use serde::{Serialize, de::DeserializeOwned};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async,
    tungstenite::{Message, client::IntoClientRequest as _},
};
use url::{Host, Url};
use uuid::Uuid;
use zeroize::{Zeroize, Zeroizing};

const DEFAULT_WEBSOCKET_URL: &str = "ws://127.0.0.1:7419/ws";

#[derive(Debug, Parser)]
#[command(name = "cookie")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, PartialEq, Eq, Subcommand)]
enum Command {
    /// Serve the protocol-v7 JSON-RPC WebSocket daemon on localhost.
    Daemon,
    /// Attach the TUI to an existing daemon.
    Attach {
        #[arg(long, default_value = DEFAULT_WEBSOCKET_URL)]
        url: String,
    },
    /// Securely connect a configured credential-store provider.
    Connect {
        /// Exact configured provider ID. Omit to choose interactively.
        provider_id: Option<String>,
        #[arg(long, default_value = DEFAULT_WEBSOCKET_URL)]
        url: String,
    },
}

struct Runtime {
    engine: Engine,
    server: Arc<Server>,
    port: u16,
}

#[derive(Default)]
struct CredentialBuffers {
    values: BTreeMap<CredentialFieldName, String>,
}

impl Drop for CredentialBuffers {
    fn drop(&mut self) {
        for value in self.values.values_mut() {
            value.zeroize();
        }
    }
}

type Socket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

struct ProviderConnectGuard(ProviderConnectParams);

impl Drop for ProviderConnectGuard {
    fn drop(&mut self) {
        for value in self.0.credentials.values.values_mut() {
            value.zeroize();
        }
    }
}

#[derive(Serialize)]
struct SensitiveRequest<'a, P> {
    jsonrpc: &'static str,
    id: i64,
    method: &'static str,
    params: &'a P,
}

struct RpcClient {
    socket: Socket,
    next_id: i64,
}

impl RpcClient {
    async fn connect(url: &str) -> anyhow::Result<Self> {
        validate_websocket_url(url)?;
        let token = Zeroizing::new(load_auth_token().context("load daemon authentication token")?);
        let mut request = url
            .into_client_request()
            .context("construct daemon WebSocket request")?;
        let authorization = Zeroizing::new(format!("Bearer {}", token.as_str()));
        request.headers_mut().insert(
            "authorization",
            authorization
                .as_str()
                .parse()
                .context("construct daemon authorization header")?,
        );
        let (socket, _) = connect_async(request)
            .await
            .context("connect to authenticated daemon WebSocket")?;
        Ok(Self { socket, next_id: 1 })
    }

    async fn handshake(&mut self) -> anyhow::Result<()> {
        let hello: cookie_agent_protocol::ServerHello = self
            .call(
                "handshake",
                &ClientHello {
                    protocol_version: ProtocolVersion::current(),
                },
            )
            .await?;
        if hello.protocol_version != ProtocolVersion::current() {
            anyhow::bail!("daemon protocol version is not 7");
        }
        Ok(())
    }

    async fn call<P, R>(&mut self, method: &str, params: &P) -> anyhow::Result<R>
    where
        P: Serialize + ?Sized,
        R: DeserializeOwned,
    {
        let id = self.next_id;
        self.next_id += 1;
        let request = Request::new(
            JsonRpcId::Number(id),
            method,
            Some(serde_json::to_value(params).context("encode RPC params")?),
        );
        self.socket
            .send(Message::Text(
                serde_json::to_string(&request)
                    .context("encode RPC request")?
                    .into(),
            ))
            .await
            .context("send RPC request")?;
        self.receive_response(id).await
    }

    async fn call_provider_connect(
        &mut self,
        params: ProviderConnectParams,
    ) -> anyhow::Result<ProviderConnectResult> {
        let id = self.next_id;
        self.next_id += 1;
        let params = ProviderConnectGuard(params);
        let request = SensitiveRequest {
            jsonrpc: "2.0",
            id,
            method: "provider.connect",
            params: &params.0,
        };
        let mut encoded = Zeroizing::new(
            serde_json::to_string(&request).context("encode sensitive RPC request")?,
        );
        // Until dispatch, cancellation drops and wipes both the structured
        // credentials and serialized frame. Once moved into tungstenite, any
        // WebSocket, TLS, socket, or kernel copies are transport-owned and
        // cannot honestly be guaranteed to be zeroized by this process.
        let outbound = std::mem::take(&mut *encoded);
        self.socket
            .send(Message::Text(outbound.into()))
            .await
            .context("send sensitive RPC request")?;
        drop(encoded);
        drop(params);
        self.receive_response(id).await
    }

    async fn receive_response<R>(&mut self, id: i64) -> anyhow::Result<R>
    where
        R: DeserializeOwned,
    {
        while let Some(message) = self.socket.next().await {
            let message = message.context("read RPC response")?;
            let Message::Text(text) = message else {
                continue;
            };
            let value: serde_json::Value =
                serde_json::from_str(&text).context("decode RPC response")?;
            if value.get("id") != Some(&serde_json::json!(id)) {
                let _: Result<Notification, _> = serde_json::from_value(value);
                continue;
            }
            return match serde_json::from_value::<Response>(value)
                .context("validate RPC response")?
            {
                Response::Success(response) => {
                    serde_json::from_value(response.result).context("decode RPC result")
                }
                Response::Error(response) => anyhow::bail!(
                    "{} ({}){}",
                    response.error.message,
                    response.error.code,
                    response
                        .error
                        .data
                        .map(|data| format!(": {data}"))
                        .unwrap_or_default()
                ),
            };
        }
        anyhow::bail!("daemon closed the WebSocket before replying")
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    let Cli { command } = Cli::parse();
    let workspace = local_workspace(&command, env::current_dir)?;
    match command {
        Some(Command::Daemon) => {
            run_daemon(compose(workspace.as_deref().expect("daemon workspace"))?).await
        }
        Some(Command::Connect { provider_id, url }) => run_connect(&url, provider_id).await,
        Some(Command::Attach { url }) => run_attached_tui(&url).await,
        None => run_local_frontend(compose(workspace.as_deref().expect("local workspace"))?).await,
    }
}

fn local_workspace(
    command: &Option<Command>,
    current_dir: impl FnOnce() -> io::Result<PathBuf>,
) -> anyhow::Result<Option<PathBuf>> {
    if matches!(
        command,
        Some(Command::Attach { .. } | Command::Connect { .. })
    ) {
        Ok(None)
    } else {
        current_dir()
            .context("determine current workspace")
            .map(Some)
    }
}

fn compose(workspace: &Path) -> anyhow::Result<Runtime> {
    let configuration = Arc::new(
        cookie_agent_config::load(workspace).context("load schema-6 workspace configuration")?,
    );
    if configuration.runtime.server.host != "127.0.0.1" {
        anyhow::bail!("server.host must be exactly 127.0.0.1");
    }
    let catalog = Arc::new(Catalog::embedded().context("load vendored models.dev catalog")?);
    let model_manager = Arc::new(
        ModelSetManager::new(
            configuration.runtime.providers.clone(),
            Arc::clone(&catalog),
            CredentialStore::standard().context("open secure provider credential store")?,
        )
        .context("compose provider/model snapshot manager")?,
    );
    configuration
        .resolve_agents(model_manager.current().model_set())
        .context("resolve Markdown agent registry")?;
    let engine = Engine::open(EngineOptions {
        data_dir: data_dir()?,
        cwd: workspace.to_owned(),
        config: (*configuration).clone(),
        model_manager: Arc::clone(&model_manager),
        tools: Vec::new(),
    })
    .context("open engine")?;
    engine.register_tool_provider(Arc::new(BuiltinTools::new(workspace)));
    engine.register_tool_provider(Arc::new(DelegateToolProvider::new(engine.client())));
    Ok(Runtime {
        engine: engine.clone(),
        server: Arc::new(Server::new(
            engine,
            model_manager,
            catalog,
            Arc::clone(&configuration),
        )),
        port: configuration.runtime.server.port,
    })
}

fn data_dir() -> anyhow::Result<PathBuf> {
    let home = env::var_os("HOME").context("determine home directory for cookie agent data")?;
    Ok(PathBuf::from(home).join(".local/share/cookie_agent"))
}

#[cfg(feature = "tui")]
async fn run_local_frontend(runtime: Runtime) -> anyhow::Result<()> {
    let (client_stream, server_stream) = in_process_pair(128);
    let server_task = tokio::spawn(runtime.server.clone().serve_stream(server_stream));
    let result = async {
        let client = cookie_agent_tui::Client::connect_stream(client_stream);
        client.handshake().await.context("handshake with daemon")?;
        cookie_agent_tui::run_with_new_session(client).await
    }
    .await;
    runtime.server.shutdown();
    let server_result = server_task
        .await
        .context("join in-process server task")?
        .context("run in-process server task");
    runtime.engine.shutdown().await;
    result.and(server_result)
}

#[cfg(not(feature = "tui"))]
async fn run_local_frontend(runtime: Runtime) -> anyhow::Result<()> {
    runtime.server.shutdown();
    runtime.engine.shutdown().await;
    anyhow::bail!("cookie was built without TUI support; use `cookie daemon` or `cookie connect`")
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

async fn run_connect(url: &str, provider_id: Option<String>) -> anyhow::Result<()> {
    require_connect_tty(
        io::stdin().is_terminal(),
        io::stdout().is_terminal(),
        io::stderr().is_terminal(),
    )?;
    let mut client = RpcClient::connect(url).await?;
    client.handshake().await.context("handshake with daemon")?;
    let catalog: CatalogProviderListResult = client
        .call("catalog.provider.list", &CatalogProviderListParams {})
        .await
        .context("list catalog providers")?;
    let provider = choose_provider(&catalog.providers, provider_id)?;
    print_provider_details(provider, catalog.snapshot.revision.as_str());
    if !prompt_confirmation("Connect this configured provider? [y/N] ")? {
        anyhow::bail!("provider connection cancelled");
    }

    let mut credentials = CredentialBuffers::default();
    for field in &provider.credential_fields {
        let mut value = read_secret_line(&format!("{field}: "))?;
        if !value.is_empty() {
            credentials
                .values
                .insert(field.clone(), std::mem::take(&mut *value));
        }
    }
    if credentials.values.is_empty() {
        anyhow::bail!("no credentials were provided");
    }
    let result = client
        .call_provider_connect(ProviderConnectParams {
            client_connect_id: ClientConnectId::new(Uuid::now_v7().to_string())
                .expect("UUID is a valid connect ID"),
            provider_id: provider
                .id
                .as_str()
                .parse()
                .context("catalog provider ID is not a configured provider ID")?,
            catalog_revision: catalog.snapshot.revision,
            credentials: ProviderCredentials {
                values: std::mem::take(&mut credentials.values),
            },
        })
        .await
        .context("provider.connect failed")?;
    println!(
        "provider.connect succeeded for {} at model revision {}.",
        result.connection.provider_id, result.model_revision
    );
    let models: ModelListResult = client
        .call("model.list", &ModelListParams {})
        .await
        .context("provider connected, but model.list refresh failed")?;
    println!(
        "model.list revision {}: {} model(s), {} named variant(s).",
        models.revision,
        models.models.len(),
        models
            .models
            .iter()
            .map(|model| model.variants.len())
            .sum::<usize>()
    );
    let agents: AgentListResult = client
        .call("agent.list", &AgentListParams {})
        .await
        .context("provider and model refresh succeeded, but agent.list failed")?;
    print_agent_report(&agents.agents);
    Ok(())
}

fn agent_report_lines(agents: &[AgentDescriptor]) -> Vec<String> {
    if agents.is_empty() {
        return vec!["agent.list: no agents are configured.".into()];
    }
    let mut lines = vec![format!("agent.list: {} agent(s):", agents.len())];
    lines.extend(agents.iter().map(|agent| {
        let state = if agent.runnable_as_root {
            "root-runnable"
        } else if agent.enabled {
            "enabled, not root-runnable"
        } else {
            "disabled"
        };
        let selections = if agent.resolved_fallback.is_empty() {
            "delegated inheritance".into()
        } else {
            agent
                .resolved_fallback
                .iter()
                .map(|selection| match &selection.variant {
                    Some(variant) => format!("{}@{}", selection.model, variant),
                    None => format!("{}@base", selection.model),
                })
                .collect::<Vec<_>>()
                .join(", ")
        };
        format!("  {} ({:?}): {state}; {selections}", agent.id, agent.mode)
    }));
    lines
}

fn print_agent_report(agents: &[AgentDescriptor]) {
    for line in agent_report_lines(agents) {
        println!("{line}");
    }
}

fn validate_websocket_url(value: &str) -> anyhow::Result<()> {
    let url = Url::parse(value).context("parse daemon WebSocket URL")?;
    if !matches!(url.scheme(), "ws" | "wss") {
        anyhow::bail!("daemon WebSocket URL scheme must be ws or wss");
    }
    if !url.username().is_empty() || url.password().is_some() {
        anyhow::bail!("daemon WebSocket URL must not contain credentials");
    }
    let loopback = match url.host().context("daemon WebSocket URL requires a host")? {
        Host::Domain(host) => host.eq_ignore_ascii_case("localhost"),
        Host::Ipv4(address) => IpAddr::V4(address).is_loopback(),
        Host::Ipv6(address) => IpAddr::V6(address).is_loopback(),
    };
    if !loopback {
        anyhow::bail!("daemon WebSocket URL host must be loopback");
    }
    if url.path() != "/ws" || url.query().is_some() || url.fragment().is_some() {
        anyhow::bail!("daemon WebSocket URL path must be exactly /ws without query or fragment");
    }
    Ok(())
}

fn require_connect_tty(stdin: bool, stdout: bool, stderr: bool) -> anyhow::Result<()> {
    if !(stdin && stdout && stderr) {
        anyhow::bail!(
            "cookie connect requires an interactive TTY; credentials are never accepted as command-line arguments"
        );
    }
    Ok(())
}

fn choose_provider(
    providers: &[CatalogProvider],
    requested: Option<String>,
) -> anyhow::Result<&CatalogProvider> {
    if let Some(requested) = requested {
        return providers
            .iter()
            .find(|provider| provider.id.as_str() == requested)
            .with_context(|| {
                format!("configured connectable provider `{requested}` was not found")
            });
    }
    if providers.is_empty() {
        anyhow::bail!("the daemon has no configured connectable providers");
    }
    println!("Configured connectable providers:");
    for (index, provider) in providers.iter().enumerate() {
        println!("  {}. {} ({})", index + 1, provider.name, provider.id);
    }
    print!("Provider number or ID: ");
    io::stdout().flush().context("flush provider prompt")?;
    let mut answer = String::new();
    io::stdin()
        .read_line(&mut answer)
        .context("read provider selection")?;
    let answer = answer.trim();
    if let Ok(index) = answer.parse::<usize>()
        && let Some(provider) = index.checked_sub(1).and_then(|index| providers.get(index))
    {
        return Ok(provider);
    }
    providers
        .iter()
        .find(|provider| provider.id.as_str() == answer)
        .with_context(|| format!("configured connectable provider `{answer}` was not found"))
}

fn print_provider_details(provider: &CatalogProvider, revision: &str) {
    println!("Provider ID: {}", provider.id);
    println!("Name: {}", provider.name);
    println!(
        "Endpoint: {}",
        provider
            .api
            .as_ref()
            .map_or("catalog default", |api| api.as_str())
    );
    println!("Documentation: {}", provider.documentation_url);
    println!("Catalog revision: {revision}");
    println!(
        "Credential fields: {}",
        provider
            .credential_fields
            .iter()
            .map(CredentialFieldName::as_str)
            .collect::<Vec<_>>()
            .join(", ")
    );
}

fn prompt_confirmation(prompt: &str) -> anyhow::Result<bool> {
    print!("{prompt}");
    io::stdout().flush().context("flush confirmation prompt")?;
    let mut answer = String::new();
    io::stdin()
        .read_line(&mut answer)
        .context("read confirmation")?;
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
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

async fn run_daemon(runtime: Runtime) -> anyhow::Result<()> {
    let listener = match runtime.server.clone().serve(runtime.port).await {
        Ok(listener) => listener,
        Err(error) => {
            runtime.server.shutdown();
            runtime.engine.shutdown().await;
            return Err(anyhow::Error::new(error).context("start WebSocket daemon"));
        }
    };
    println!(
        "cookie daemon listening on ws://{}/ws (protocol 7)",
        listener.address()
    );
    let signal = tokio::signal::ctrl_c().await;
    runtime.server.shutdown();
    listener.wait().await;
    runtime.engine.shutdown().await;
    signal.context("wait for daemon shutdown signal")
}

#[cfg(test)]
mod tests {
    use cookie_agent_protocol::{AgentId, AgentMode, ModelKey, ModelSelection, ToolName};

    use super::*;

    #[test]
    fn cli_commands_and_removed_secret_workspace_flags_are_exact() {
        assert_eq!(Cli::try_parse_from(["cookie"]).unwrap().command, None);
        assert_eq!(
            Cli::try_parse_from(["cookie", "daemon"]).unwrap().command,
            Some(Command::Daemon)
        );
        assert!(Cli::try_parse_from(["cookie", "--trust-workspace", "daemon"]).is_err());
        assert!(
            Cli::try_parse_from(["cookie", "connect", "openai", "--api-key", "sentinel"]).is_err()
        );
    }

    #[test]
    fn attach_and_connect_are_cwd_independent() {
        for command in [
            Command::Attach {
                url: DEFAULT_WEBSOCKET_URL.into(),
            },
            Command::Connect {
                provider_id: None,
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
    fn connect_requires_all_tty_streams() {
        for terminals in [
            (false, true, true),
            (true, false, true),
            (true, true, false),
        ] {
            assert!(require_connect_tty(terminals.0, terminals.1, terminals.2).is_err());
        }
        require_connect_tty(true, true, true).unwrap();
    }

    #[test]
    fn reporting_uses_agent_model_and_variant_terms() {
        let agent = AgentDescriptor {
            id: AgentId::new("primary").unwrap(),
            description: "Primary".into(),
            mode: AgentMode::Primary,
            enabled: true,
            runnable_as_root: true,
            resolved_fallback: vec![ModelSelection {
                model: "openai/gpt-5.6-sol".parse::<ModelKey>().unwrap(),
                variant: Some("high".parse().unwrap()),
            }],
            tools: vec![ToolName::Read],
            delegation_targets: Vec::new(),
        };
        let report = agent_report_lines(&[agent]).join("\n");
        assert!(report.contains("agent.list: 1 agent(s)"));
        assert!(report.contains("openai/gpt-5.6-sol@high"));
        assert!(!report.contains("profile"));
        assert!(!report.contains("alias"));
    }
}
