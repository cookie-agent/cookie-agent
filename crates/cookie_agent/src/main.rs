use std::{
    collections::BTreeMap,
    env,
    io::{self, IsTerminal, Write},
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::Context;
use clap::{Parser, Subcommand};
use cookie_agent_engine::{Engine, EngineOptions};
use cookie_agent_models::{Catalog, CredentialStore, ModelSetManager};
use cookie_agent_protocol::{
    AgentDescriptor, AgentListParams, CatalogProvider, CatalogProviderListParams, ModelListParams,
    ProviderConnectParams, ProviderCredentials,
};
use cookie_agent_server::{Server, in_process_pair};
use cookie_agent_tools::{BuiltinTools, delegate::DelegateToolProvider};
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
    /// Serve the JSON-RPC WebSocket daemon on localhost.
    Daemon,
    /// Attach the TUI to an existing JSON-RPC WebSocket daemon.
    Attach {
        /// WebSocket endpoint exposed by `cookie daemon`.
        #[arg(long, default_value = DEFAULT_WEBSOCKET_URL)]
        url: String,
    },
    /// Securely connect a catalog provider to the running daemon.
    Connect {
        /// Exact catalog provider ID. Omit to choose interactively.
        provider_id: Option<String>,
        /// WebSocket endpoint exposed by `cookie daemon`.
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
    values: BTreeMap<String, String>,
}

impl CredentialBuffers {
    fn insert(&mut self, field: String, value: String) {
        self.values.insert(field, value);
    }

    fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    fn take(&mut self) -> BTreeMap<String, String> {
        std::mem::take(&mut self.values)
    }
}

impl Drop for CredentialBuffers {
    fn drop(&mut self) {
        for value in self.values.values_mut() {
            value.zeroize();
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let Cli { command } = Cli::parse();
    validate_tui_startup_config(&command, None)?;
    let workspace = local_workspace(&command, env::current_dir)?;

    match command {
        Some(Command::Attach { url }) => run_attached_tui(&url).await,
        Some(Command::Connect { provider_id, url }) => run_connect(&url, provider_id).await,
        command => {
            let runtime = compose(workspace.as_deref().expect("local command has workspace"))?;
            match command {
                Some(Command::Daemon) => run_daemon(runtime).await,
                None => run_tui(runtime).await,
                Some(Command::Attach { .. }) => unreachable!("attach handled above"),
                Some(Command::Connect { .. }) => unreachable!("connect handled above"),
            }
        }
    }
}

fn validate_tui_startup_config(
    command: &Option<Command>,
    override_path: Option<&Path>,
) -> anyhow::Result<()> {
    if command.is_none() || matches!(command, Some(Command::Attach { .. })) {
        cookie_agent_tui::config::load(override_path)
            .map(|_| ())
            .map_err(anyhow::Error::new)
            .context("load TUI configuration")?;
    }
    Ok(())
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

/// Loads configuration and wires the process-wide daemon stack for one workspace.
fn compose(workspace: &Path) -> anyhow::Result<Runtime> {
    let config = cookie_agent_config::load(workspace).context("load workspace configuration")?;
    let catalog = Arc::new(Catalog::embedded().context("load vendored models.dev catalog")?);
    let model_manager = Arc::new(
        ModelSetManager::new(
            config.models.clone(),
            Arc::clone(&catalog),
            CredentialStore::standard().context("open secure provider credential store")?,
        )
        .context("compose runtime model manager")?,
    );
    let engine = Engine::open(EngineOptions {
        data_dir: data_dir()?,
        cwd: workspace.to_owned(),
        config: config.clone(),
        model_manager: Arc::clone(&model_manager),
        tools: Vec::new(),
    })
    .context("open engine")?;

    engine.register_tool_provider(Arc::new(BuiltinTools::new(workspace)));
    engine.register_tool_provider(Arc::new(DelegateToolProvider::new(
        engine.client(),
        &config,
    )));

    Ok(Runtime {
        engine: engine.clone(),
        server: Arc::new(Server::new(engine, model_manager, catalog)),
        port: config.server.port,
    })
}

fn data_dir() -> anyhow::Result<PathBuf> {
    let home = env::var_os("HOME").context("determine home directory for cookie agent data")?;
    Ok(PathBuf::from(home).join(".local/share/cookie_agent"))
}

async fn run_tui(runtime: Runtime) -> anyhow::Result<()> {
    let (client_stream, server_stream) = in_process_pair(128);
    let server_task = tokio::spawn(runtime.server.clone().serve_stream(server_stream));
    let tui_result = async {
        let client = cookie_agent_tui::Client::connect_stream(client_stream);
        client.handshake().await.context("handshake with daemon")?;
        cookie_agent_tui::run_with_new_session(client).await
    }
    .await;

    runtime.server.shutdown();
    let server_result = match server_task.await {
        Ok(result) => result.context("run in-process server task"),
        Err(error) => Err(anyhow::Error::new(error).context("join in-process server task")),
    };
    runtime.engine.shutdown().await;
    tui_result.and(server_result)
}

async fn run_attached_tui(url: &str) -> anyhow::Result<()> {
    cookie_agent_tui::validate_websocket_url(url)
        .map_err(anyhow::Error::new)
        .context("validate daemon WebSocket URL")?;
    let client = cookie_agent_tui::Client::connect_websocket(url)
        .await
        .context("connect to daemon WebSocket")?;
    client.handshake().await.context("handshake with daemon")?;
    cookie_agent_tui::run_with_client(client).await
}

async fn run_connect(url: &str, provider_id: Option<String>) -> anyhow::Result<()> {
    require_connect_tty(
        io::stdin().is_terminal(),
        io::stdout().is_terminal(),
        io::stderr().is_terminal(),
    )?;
    cookie_agent_tui::validate_websocket_url(url)
        .map_err(anyhow::Error::new)
        .context("validate daemon WebSocket URL")?;
    let client = cookie_agent_tui::Client::connect_websocket(url)
        .await
        .context("connect to authenticated daemon WebSocket")?;
    client.handshake().await.context("handshake with daemon")?;
    let catalog = client
        .list_catalog_providers(CatalogProviderListParams {})
        .await
        .context("list catalog providers")?;
    let provider = choose_provider(&catalog.providers, provider_id)?;
    print_provider_details(provider, &catalog.snapshot.revision);
    if !prompt_confirmation("Connect this provider? [y/N] ")? {
        anyhow::bail!("provider connection cancelled");
    }

    let mut credentials = CredentialBuffers::default();
    for field in &provider.credential_fields {
        let mut value = read_secret_line(&format!("{field}: "))?;
        if !value.is_empty() {
            credentials.insert(field.clone(), std::mem::take(&mut *value));
        }
    }
    if credentials.is_empty() {
        anyhow::bail!("no credentials were provided");
    }
    let result = client
        .connect_provider(ProviderConnectParams {
            client_connect_id: Uuid::now_v7().to_string(),
            provider_id: provider.id.clone(),
            catalog_revision: catalog.snapshot.revision,
            credentials: ProviderCredentials {
                values: credentials.take(),
            },
        })
        .await
        .context("provider.connect failed")?;
    println!(
        "provider.connect succeeded for {} at model revision {}.",
        result.connection.provider_id, result.model_revision
    );
    let models = client
        .list_models(ModelListParams {})
        .await
        .context("provider connected, but model.list refresh failed")?;
    println!(
        "model.list refreshed revision {} ({} models).",
        models.revision,
        models.models.len()
    );
    let agents = client
        .list_agents(AgentListParams::default())
        .await
        .context("provider and model refresh succeeded, but agent.list failed")?;
    print_agent_report(&agents.agents);
    Ok(())
}

fn agent_report_lines(agents: &[AgentDescriptor]) -> Vec<String> {
    if agents.is_empty() {
        return vec!["agent.list: no user-selectable profiles are configured.".into()];
    }
    let mut lines = vec![format!("agent.list: {} profile(s):", agents.len())];
    lines.extend(agents.iter().map(|agent| {
        let status = if agent.enabled {
            "runnable"
        } else {
            "disabled or unresolved"
        };
        let models = if agent.models.is_empty() {
            "no resolved models".into()
        } else {
            agent
                .models
                .iter()
                .map(|model| model.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        };
        format!(
            "  {} ({:?}): {status}; {models}",
            agent.name, agent.agent_type
        )
    }));
    lines
}

fn print_agent_report(agents: &[AgentDescriptor]) {
    for line in agent_report_lines(agents) {
        println!("{line}");
    }
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
            .find(|provider| provider.id == requested)
            .with_context(|| format!("catalog provider `{requested}` was not found"));
    }
    if providers.is_empty() {
        anyhow::bail!("the daemon catalog contains no providers");
    }
    println!("Available providers:");
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
        .find(|provider| provider.id == answer)
        .with_context(|| format!("catalog provider `{answer}` was not found"))
}

fn print_provider_details(provider: &CatalogProvider, revision: &str) {
    println!("Provider ID: {}", provider.id);
    println!("Name: {}", provider.name);
    println!(
        "Endpoint: {}",
        provider.api.as_deref().unwrap_or("catalog default")
    );
    println!(
        "Documentation: {}",
        provider
            .documentation_url
            .as_deref()
            .unwrap_or("not advertised")
    );
    println!("Catalog revision: {revision}");
    println!(
        "Credential fields: {}",
        provider.credential_fields.join(", ")
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
    // SAFETY: `termios` is initialized by `tcgetattr` before it is read.
    let mut original = unsafe { std::mem::zeroed::<libc::termios>() };
    // SAFETY: stdin is a TTY, checked before this function is called.
    if unsafe { libc::tcgetattr(libc::STDIN_FILENO, &mut original) } != 0 {
        return Err(io::Error::last_os_error()).context("disable credential echo");
    }
    let guard = EchoGuard(original);
    let mut hidden = original;
    hidden.c_lflag &= !libc::ECHO;
    // SAFETY: both pointers refer to initialized termios values.
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
fn read_secret_line(_prompt: &str) -> anyhow::Result<Zeroizing<String>> {
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

    let signal = tokio::signal::ctrl_c().await;
    runtime.server.shutdown();
    listener.wait().await;
    runtime.engine.shutdown().await;
    signal.context("wait for daemon shutdown signal")
}

#[cfg(test)]
mod tests {
    use std::{
        fs, process,
        sync::{
            Mutex, MutexGuard, OnceLock,
            atomic::{AtomicU64, Ordering},
        },
        time::{Duration, SystemTime, UNIX_EPOCH},
    };

    use cookie_agent_config::Config;

    use super::*;

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let timestamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock after epoch")
                .as_nanos();
            let path = env::temp_dir().join(format!(
                "cookie_agent_test_{}_{timestamp}_{}",
                process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&path).expect("create temporary directory");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                fs::set_permissions(&path, fs::Permissions::from_mode(0o700))
                    .expect("secure temporary directory");
            }
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn workspace_with_config(contents: &str) -> (TestDirectory, PathBuf) {
        let directory = TestDirectory::new();
        let workspace = directory.path().join("workspace");
        let config_dir = workspace.join(".cookie_agent");
        fs::create_dir_all(&config_dir).expect("create workspace config directory");
        fs::write(config_dir.join("config.toml"), contents).expect("write workspace config");
        (directory, workspace)
    }

    struct HomeEnvironment {
        _guard: MutexGuard<'static, ()>,
        prior_home: Option<std::ffi::OsString>,
    }

    impl HomeEnvironment {
        fn new(home: &Path) -> Self {
            static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
            let guard = LOCK
                .get_or_init(|| Mutex::new(()))
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            let prior_home = env::var_os("HOME");
            unsafe { env::set_var("HOME", home) };
            Self {
                _guard: guard,
                prior_home,
            }
        }
    }

    impl Drop for HomeEnvironment {
        fn drop(&mut self) {
            unsafe {
                match self.prior_home.take() {
                    Some(home) => env::set_var("HOME", home),
                    None => env::remove_var("HOME"),
                }
            }
        }
    }

    async fn compose_with_timeout(label: &str, workspace: PathBuf) -> Runtime {
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            tokio::task::spawn_blocking(move || compose(&workspace)),
        )
        .await
        .expect("startup composition must not block")
        .expect("join startup composition");
        result.unwrap_or_else(|error| panic!("{label}: compose startup runtime: {error:#}"))
    }

    async fn shutdown(runtime: Runtime) {
        runtime.server.shutdown();
        runtime.engine.shutdown().await;
    }

    #[cfg(unix)]
    fn create_private_data_directory(home: &Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt as _;

        let local = home.join(".local");
        let share = local.join("share");
        let data = share.join("cookie_agent");
        fs::create_dir_all(&data).expect("create stale data directory");
        for directory in [&local, &share, &data] {
            fs::set_permissions(directory, fs::Permissions::from_mode(0o700))
                .expect("make data directory private");
        }
        data
    }

    fn explicit_model_config(
        alias: &str,
        provider_id: &str,
        model_id: &str,
        tools: &str,
    ) -> String {
        format!(
            r#"
[models.{alias}]
provider_id = "{provider_id}"
model_id = "{model_id}"
endpoint = "https://example.test/v1"
adaptor = "openai-responses"

[models.{alias}.auth]
type = "openai"
api_key = "test-secret"

[models.{alias}.capabilities]
features = ["max_output_tokens", "tool_calling"]
cancellation = "local_only"
compaction = "unsupported"

[models.{alias}.capabilities.limits]
context = 4096

[models.{alias}.capabilities.modalities]
input = ["text"]
output = ["text"]

[models.{alias}.capabilities.media]
input = {{}}

[models.{alias}.capabilities.replay]
policy = "never"
capability = "unsupported"
reasoning = false

[models.{alias}.settings]

[agents.primary]
type = "primary"
models = ["{alias}"]
tools = ["{tools}"]
"#
        )
    }

    #[test]
    fn cli_defaults_to_the_in_process_tui() {
        let cli = Cli::try_parse_from(["cookie"]).expect("parse CLI");
        assert_eq!(cli.command, None);
    }

    #[test]
    fn in_process_and_attached_tui_propagate_the_same_config_errors() {
        let directory = TestDirectory::new();
        let missing = directory.path().join("missing-tui.toml");
        let attached = Some(Command::Attach {
            url: DEFAULT_WEBSOCKET_URL.into(),
        });
        for command in [&None, &attached] {
            validate_tui_startup_config(command, Some(&missing)).expect("missing uses defaults");
        }

        let malformed = directory.path().join("malformed-tui.toml");
        fs::write(&malformed, "theme = \"secret-invalid-theme\"\n").expect("write malformed");
        let reports = [&None, &attached]
            .into_iter()
            .map(|command| {
                format!(
                    "{:#}",
                    validate_tui_startup_config(command, Some(&malformed))
                        .expect_err("malformed config must fail startup")
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(reports[0], reports[1]);
        assert!(reports[0].contains(&malformed.display().to_string()));
        assert!(reports[0].contains("theme"));
        assert!(!reports[0].contains("secret-invalid-theme"));

        let unknown = directory.path().join("unknown-tui.toml");
        fs::write(&unknown, "unknown_setting = \"secret-value\"\n").expect("write unknown");
        let report = format!(
            "{:#}",
            validate_tui_startup_config(&None, Some(&unknown))
                .expect_err("unknown key must fail startup")
        );
        assert!(report.contains("unknown_setting"));
        assert!(!report.contains("secret-value"));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;

            let unreadable = directory.path().join("unreadable-tui.toml");
            fs::write(&unreadable, "theme = \"mono\"\n").expect("write unreadable");
            fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o000))
                .expect("remove read permissions");
            let report = format!(
                "{:#}",
                validate_tui_startup_config(&attached, Some(&unreadable))
                    .expect_err("permission error must fail startup")
            );
            assert!(report.contains(&unreadable.display().to_string()));
        }
    }

    #[test]
    fn cli_parses_daemon() {
        let cli = Cli::try_parse_from(["cookie", "daemon"]).expect("parse CLI");
        assert_eq!(cli.command, Some(Command::Daemon));
    }

    #[test]
    fn removed_workspace_flag_is_rejected_before_and_after_daemon() {
        for arguments in [
            ["cookie", "--trust-workspace", "daemon"],
            ["cookie", "daemon", "--trust-workspace"],
        ] {
            let error = Cli::try_parse_from(arguments).expect_err("removed flag must be unknown");
            assert_eq!(error.kind(), clap::error::ErrorKind::UnknownArgument);
        }
    }

    #[test]
    fn cli_parses_websocket_attach_with_default_url() {
        let cli = Cli::try_parse_from(["cookie", "attach"]).expect("parse CLI");
        assert_eq!(
            cli.command,
            Some(Command::Attach {
                url: DEFAULT_WEBSOCKET_URL.into()
            })
        );
    }

    #[test]
    fn cli_parses_websocket_attach_url_override() {
        let cli = Cli::try_parse_from(["cookie", "attach", "--url", "ws://127.0.0.1:8123/ws"])
            .expect("parse CLI");
        assert_eq!(
            cli.command,
            Some(Command::Attach {
                url: "ws://127.0.0.1:8123/ws".into()
            })
        );
    }

    #[test]
    fn cli_parses_connect_with_optional_provider_and_url() {
        let default = Cli::try_parse_from(["cookie", "connect"]).expect("parse connect");
        assert_eq!(
            default.command,
            Some(Command::Connect {
                provider_id: None,
                url: DEFAULT_WEBSOCKET_URL.into(),
            })
        );
        let explicit = Cli::try_parse_from([
            "cookie",
            "connect",
            "openai",
            "--url",
            "ws://127.0.0.1:8123/ws",
        ])
        .expect("parse explicit connect");
        assert_eq!(
            explicit.command,
            Some(Command::Connect {
                provider_id: Some("openai".into()),
                url: "ws://127.0.0.1:8123/ws".into(),
            })
        );
    }

    #[test]
    fn cli_connect_has_no_secret_argument_surface() {
        for arguments in [
            vec![
                "cookie",
                "connect",
                "openai",
                "--api-key",
                "sentinel-secret",
            ],
            vec!["cookie", "connect", "openai", "sentinel-secret"],
        ] {
            assert!(Cli::try_parse_from(arguments).is_err());
        }
    }

    #[test]
    fn connect_rejects_any_non_tty_stream() {
        for terminals in [
            (false, true, true),
            (true, false, true),
            (true, true, false),
            (false, false, false),
        ] {
            let error = require_connect_tty(terminals.0, terminals.1, terminals.2)
                .expect_err("non-TTY connect must fail");
            let message = error.to_string();
            assert!(message.contains("interactive TTY"));
            assert!(message.contains("never accepted"));
            assert!(!message.contains("sentinel-secret"));
        }
        require_connect_tty(true, true, true).expect("interactive connect");
    }

    #[test]
    fn cli_reports_agent_list_runnable_state_and_resolved_models() {
        let lines = agent_report_lines(&[
            AgentDescriptor {
                name: "primary".into(),
                agent_type: cookie_agent_protocol::AgentType::Primary,
                enabled: true,
                models: vec![cookie_agent_protocol::ModelRef {
                    name: "openai/gpt-test".into(),
                    provider_id: "openai".into(),
                    model_id: "gpt-test".into(),
                    adapter_id: "openai-responses".into(),
                }],
            },
            AgentDescriptor {
                name: "disabled".into(),
                agent_type: cookie_agent_protocol::AgentType::All,
                enabled: false,
                models: Vec::new(),
            },
        ]);
        let report = lines.join("\n");
        assert!(report.contains("primary (Primary): runnable"));
        assert!(report.contains("openai/gpt-test"));
        assert!(report.contains("disabled (All): disabled or unresolved"));
    }

    #[test]
    fn cli_attach_accepts_only_loopback_websocket_endpoints() {
        for url in [
            "ws://127.0.0.1:7419/ws",
            "wss://localhost:7419/ws",
            "ws://[::1]:7419/ws",
        ] {
            cookie_agent_tui::validate_websocket_url(url)
                .unwrap_or_else(|error| panic!("{url} should be accepted: {error}"));
        }

        for (url, reason) in [
            ("http://127.0.0.1:7419/ws", "scheme"),
            ("tcp://127.0.0.1:7419/ws", "scheme"),
            ("ws://example.com:7419/ws", "loopback"),
            ("ws://user:password@127.0.0.1:7419/ws", "credentials"),
        ] {
            let error = cookie_agent_tui::validate_websocket_url(url)
                .expect_err("remote or unsafe attach URL");
            assert!(error.to_string().contains(reason), "{url}: {error}");
        }
    }

    #[test]
    fn attach_and_connect_do_not_acquire_a_workspace() {
        for command in [
            Command::Attach {
                url: DEFAULT_WEBSOCKET_URL.into(),
            },
            Command::Connect {
                provider_id: None,
                url: DEFAULT_WEBSOCKET_URL.into(),
            },
        ] {
            let workspace = local_workspace(&Some(command), || {
                panic!("attach/connect must not inspect the current directory")
            })
            .expect("workspace-independent command");
            assert!(workspace.is_none());
        }
    }

    #[test]
    fn explicit_oven_models_compose_into_an_immutable_set() {
        let (_directory, workspace) =
            workspace_with_config(&explicit_model_config("local", "local", "scripted", "read"));
        let config = cookie_agent_config::load_layered(
            None,
            Some(&workspace.join(".cookie_agent/config.toml")),
        )
        .expect("load explicit model config");
        let models = config.build_model_set().expect("compose models");
        assert_eq!(models.aliases().collect::<Vec<_>>(), ["local"]);
    }

    #[test]
    fn workspace_config_loads_directly_without_interaction() {
        let (directory, workspace) = workspace_with_config("[server]\nport = 8123\n");
        let _environment = HomeEnvironment::new(directory.path());
        let config = cookie_agent_config::load(&workspace).expect("load workspace configuration");
        assert_eq!(config.server.port, 8123);
    }

    #[test]
    fn invalid_workspace_toml_reports_its_logical_path() {
        let (directory, workspace) = workspace_with_config("[server\nport = 8123\n");
        let _environment = HomeEnvironment::new(directory.path());
        let error = cookie_agent_config::load(&workspace).expect_err("invalid workspace TOML");
        let message = format!("{error:#}");
        let path = workspace.join(".cookie_agent/config.toml");
        assert!(message.contains(&path.display().to_string()), "{message}");
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn startup_ignores_and_preserves_stale_trust_objects_without_blocking() {
        use std::{
            ffi::CString,
            os::unix::{ffi::OsStrExt as _, fs::FileTypeExt as _},
        };

        {
            let (directory, workspace) = workspace_with_config("");
            let _environment = HomeEnvironment::new(directory.path());
            let data = create_private_data_directory(directory.path());
            let stale = data.join("trust.json");
            fs::write(&stale, b"{malformed").expect("write stale malformed object");

            shutdown(compose_with_timeout("malformed", workspace).await).await;

            assert_eq!(fs::read(stale).expect("read stale object"), b"{malformed");
        }

        {
            let (directory, workspace) = workspace_with_config("");
            let _environment = HomeEnvironment::new(directory.path());
            let data = create_private_data_directory(directory.path());
            let target = directory.path().join("stale-target");
            fs::write(&target, b"unchanged").expect("write symlink target");
            let stale = data.join("trust.json");
            std::os::unix::fs::symlink(&target, &stale).expect("create stale symlink");

            shutdown(compose_with_timeout("symlink", workspace).await).await;

            assert!(
                fs::symlink_metadata(&stale)
                    .unwrap()
                    .file_type()
                    .is_symlink()
            );
            assert_eq!(fs::read(target).expect("read symlink target"), b"unchanged");
        }

        {
            let (directory, workspace) = workspace_with_config("");
            let _environment = HomeEnvironment::new(directory.path());
            let data = create_private_data_directory(directory.path());
            let stale = data.join("trust.json");
            let stale_c = CString::new(stale.as_os_str().as_bytes()).expect("FIFO path");
            assert_eq!(unsafe { libc::mkfifo(stale_c.as_ptr(), 0o600) }, 0);

            shutdown(compose_with_timeout("FIFO", workspace).await).await;

            assert!(fs::symlink_metadata(stale).unwrap().file_type().is_fifo());
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn startup_creates_no_trust_artifact() {
        let (directory, workspace) = workspace_with_config("");
        let _environment = HomeEnvironment::new(directory.path());
        let stale = directory
            .path()
            .join(".local/share/cookie_agent/trust.json");

        shutdown(compose_with_timeout("absent", workspace).await).await;

        assert!(!stale.exists());
    }

    #[tokio::test]
    async fn in_process_composition_shuts_down_cleanly() {
        let directory = TestDirectory::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700))
                .expect("private test root");
        }
        let config = Config::default();
        let catalog = Arc::new(Catalog::embedded().expect("embedded catalog"));
        let model_manager = Arc::new(
            ModelSetManager::new(
                config.models.clone(),
                Arc::clone(&catalog),
                CredentialStore::new(directory.path().join("credentials")),
            )
            .expect("compose empty model manager"),
        );
        let engine = Engine::open(EngineOptions {
            data_dir: directory.path().join("data"),
            cwd: directory.path().to_owned(),
            config: config.clone(),
            model_manager: Arc::clone(&model_manager),
            tools: Vec::new(),
        })
        .expect("open engine");
        engine.register_tool_provider(Arc::new(BuiltinTools::new(directory.path())));
        engine.register_tool_provider(Arc::new(DelegateToolProvider::new(
            engine.client(),
            &config,
        )));
        let server = Arc::new(Server::new(engine.clone(), model_manager, catalog));
        let (client_stream, server_stream) = in_process_pair(8);
        let server_task = tokio::spawn(server.clone().serve_stream(server_stream));
        let client = cookie_agent_tui::Client::connect_stream(client_stream);

        client.handshake().await.expect("handshake");
        server.shutdown();
        server_task
            .await
            .expect("join server task")
            .expect("serve stream");
        engine.shutdown().await;
    }
}
