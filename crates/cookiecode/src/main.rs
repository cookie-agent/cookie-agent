use std::{
    collections::HashMap,
    env, fs,
    io::{self, BufRead, IsTerminal, Write},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::Context;
use clap::{Parser, Subcommand};
use cookiecode_config::{Config, OpenAiApi, ProviderConfig, ProviderType, TrustStore};
use cookiecode_engine::{Engine, EngineOptions};
use cookiecode_providers::{
    Provider,
    anthropic::AnthropicProvider,
    openai::{OpenAiEndpoint, OpenAiProvider},
    openai_compatible::OpenAiCompatibleProvider,
};
use cookiecode_server::{ProviderRegistry, Server, in_process_pair};
use cookiecode_tools::{BuiltinTools, delegate::DelegateToolProvider};

#[derive(Debug, Parser)]
#[command(name = "cookiecode")]
struct Cli {
    /// Trust the current workspace configuration without an interactive prompt.
    #[arg(long, global = true)]
    trust_workspace: bool,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, PartialEq, Eq, Subcommand)]
enum Command {
    /// Serve the JSON-RPC WebSocket daemon on localhost.
    Daemon,
}

struct Runtime {
    engine: Engine,
    server: Arc<Server>,
    port: u16,
}

struct ApprovedWorkspaceConfig {
    workspace: PathBuf,
}

impl ApprovedWorkspaceConfig {
    fn new(bytes: Option<&[u8]>) -> anyhow::Result<Self> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .context("read system clock")?
            .as_nanos();
        for _ in 0..16 {
            let workspace = env::temp_dir().join(format!(
                "cookiecode-approved-config-{}-{timestamp}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            match create_private_directory(&workspace) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!("create approved-config workspace {workspace:?}")
                    });
                }
            }
            let result = (|| -> anyhow::Result<()> {
                if let Some(bytes) = bytes {
                    let config_dir = workspace.join(".cookiecode");
                    create_private_directory(&config_dir).with_context(|| {
                        format!("create approved config directory {config_dir:?}")
                    })?;
                    write_private_file(&config_dir.join("config.toml"), bytes)
                        .context("write approved workspace configuration")?;
                }
                Ok(())
            })();
            match result {
                Ok(()) => return Ok(Self { workspace }),
                Err(error) => {
                    let _ = fs::remove_dir_all(&workspace);
                    return Err(error);
                }
            }
        }
        anyhow::bail!("could not create an approved-config workspace")
    }
}

impl Drop for ApprovedWorkspaceConfig {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.workspace);
    }
}

#[cfg(unix)]
fn create_private_directory(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};

    let mut builder = fs::DirBuilder::new();
    builder.mode(0o700).create(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
/// Uses platform-default ACLs where Unix modes are unavailable.
fn create_private_directory(path: &Path) -> io::Result<()> {
    fs::create_dir(path)
}

#[cfg(unix)]
fn write_private_file(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    file.write_all(bytes)
}

#[cfg(not(unix))]
/// Uses platform-default ACLs where Unix modes are unavailable.
fn write_private_file(path: &Path, bytes: &[u8]) -> io::Result<()> {
    fs::write(path, bytes)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let cli = Cli::parse();
    let workspace = env::current_dir().context("determine current workspace")?;
    let runtime = compose(&workspace, cli.trust_workspace)?;

    match cli.command {
        Some(Command::Daemon) => run_daemon(runtime).await,
        None => run_tui(runtime).await,
    }
}

/// Loads configuration and wires the process-wide daemon stack for one workspace.
fn compose(workspace: &Path, trust_workspace: bool) -> anyhow::Result<Runtime> {
    let config = load_trusted_config(workspace, trust_workspace)?;
    let providers = provider_registry(&config)?;
    let engine = Engine::open(EngineOptions {
        data_dir: data_dir()?,
        cwd: workspace.to_owned(),
        config: config.clone(),
        providers: providers.clone(),
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
        server: Arc::new(Server::new(engine, config.clone(), providers)),
        port: config.server.port,
    })
}

/// Refuses to apply an untrusted repository configuration until the user has
/// explicitly accepted its current contents.
fn load_trusted_config(workspace: &Path, trust_workspace: bool) -> anyhow::Result<Config> {
    let trust_path =
        cookiecode_config::trust_store_path().context("locate workspace trust store")?;
    let stdin = io::stdin();
    let stdout = io::stdout();
    let interactive = stdin.is_terminal() && stdout.is_terminal();
    let mut input = stdin.lock();
    let mut output = stdout.lock();
    let approved_workspace_config = authorize_workspace_config(
        workspace,
        &trust_path,
        trust_workspace,
        interactive,
        &mut input,
        &mut output,
    )?;
    load_config_with_approved_workspace_config(approved_workspace_config.as_deref())
}

/// Uses the trusted workspace bytes as the workspace layer while preserving the
/// config crate's user and environment layers.
fn load_config_with_approved_workspace_config(bytes: Option<&[u8]>) -> anyhow::Result<Config> {
    let workspace = ApprovedWorkspaceConfig::new(bytes)?;
    cookiecode_config::load(&workspace.workspace).context("load approved workspace configuration")
}

fn authorize_workspace_config<R, W>(
    workspace: &Path,
    trust_path: &Path,
    trust_workspace: bool,
    interactive: bool,
    input: &mut R,
    output: &mut W,
) -> anyhow::Result<Option<Vec<u8>>>
where
    R: BufRead,
    W: Write,
{
    let config_path = workspace.join(".cookiecode/config.toml");
    let config_bytes = match fs::read(&config_path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("read {config_path:?}")),
    };
    let mut trust_store = TrustStore::load(trust_path).context("load workspace trust store")?;
    if !trust_store
        .needs_retrust(workspace, &config_bytes)
        .context("check workspace configuration trust")?
    {
        return Ok(Some(config_bytes));
    }

    if !trust_workspace {
        if !interactive {
            anyhow::bail!(
                "workspace configuration {config_path:?} is untrusted; rerun interactively or pass --trust-workspace"
            );
        }
        writeln!(
            output,
            "CookieCode workspace configuration {config_path:?} can grant host-authority tools. Trust it? [y/N]"
        )
        .context("prompt for workspace trust")?;
        output.flush().context("flush workspace trust prompt")?;
        let mut answer = String::new();
        input
            .read_line(&mut answer)
            .context("read workspace trust response")?;
        if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
            anyhow::bail!("workspace configuration was not trusted");
        }
    }

    trust_store
        .record_trust(workspace, &config_bytes)
        .context("record workspace trust")?;
    trust_store
        .save(trust_path)
        .context("save workspace trust")?;
    Ok(Some(config_bytes))
}

fn data_dir() -> anyhow::Result<PathBuf> {
    let home = env::var_os("HOME").context("determine home directory for CookieCode data")?;
    Ok(PathBuf::from(home).join(".local/share/cookiecode"))
}

fn provider_registry(config: &Config) -> anyhow::Result<ProviderRegistry> {
    config
        .providers
        .iter()
        .map(|(name, provider)| {
            let provider = build_provider(name, provider)?;
            Ok((name.clone(), provider))
        })
        .collect::<anyhow::Result<HashMap<_, _>>>()
}

fn build_provider(name: &str, config: &ProviderConfig) -> anyhow::Result<Arc<dyn Provider>> {
    let api_key = api_key(name, config)?;
    let provider: Arc<dyn Provider> = match config.kind {
        ProviderType::Anthropic => match &config.base_url {
            Some(base_url) => Arc::new(AnthropicProvider::with_base_url(api_key, base_url)),
            None => Arc::new(AnthropicProvider::new(api_key)),
        },
        ProviderType::OpenAi => {
            let provider = match &config.base_url {
                Some(base_url) => OpenAiProvider::with_base_url(api_key, base_url),
                None => OpenAiProvider::new(api_key),
            };
            let endpoint = match config.api {
                Some(OpenAiApi::Responses) => OpenAiEndpoint::Responses,
                Some(OpenAiApi::Completions) | None => OpenAiEndpoint::ChatCompletions,
            };
            Arc::new(provider.with_default_endpoint(endpoint))
        }
        ProviderType::OpenAiCompatible => {
            let base_url = config.base_url.as_deref().with_context(|| {
                format!("openai-compatible provider `{name}` requires `base_url`")
            })?;
            Arc::new(OpenAiCompatibleProvider::new(api_key, base_url))
        }
    };
    Ok(provider)
}

fn api_key(name: &str, config: &ProviderConfig) -> anyhow::Result<String> {
    match &config.api_key_env {
        Some(variable) => env::var(variable)
            .with_context(|| format!("read API key for provider `{name}` from `{variable}`")),
        None if config.kind == ProviderType::OpenAiCompatible => Ok(String::new()),
        None => anyhow::bail!("provider `{name}` requires `api_key_env`"),
    }
}

async fn run_tui(runtime: Runtime) -> anyhow::Result<()> {
    let (client_stream, server_stream) = in_process_pair(128);
    let server_task = tokio::spawn(runtime.server.clone().serve_stream(server_stream));
    let tui_result = async {
        let client = cookiecode_tui::Client::connect_stream(client_stream);
        client.handshake().await.context("handshake with daemon")?;
        cookiecode_tui::run_with_client(client).await
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
        fs,
        io::Cursor,
        process,
        sync::atomic::{AtomicU64, Ordering},
        time::{SystemTime, UNIX_EPOCH},
    };

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
                "cookiecode-test-{}-{timestamp}-{}",
                process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&path).expect("create temporary directory");
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

    fn workspace_with_config(contents: &str) -> (TestDirectory, PathBuf, PathBuf) {
        let directory = TestDirectory::new();
        let workspace = directory.path().join("workspace");
        let config_dir = workspace.join(".cookiecode");
        fs::create_dir_all(&config_dir).expect("create workspace config directory");
        fs::write(config_dir.join("config.toml"), contents).expect("write workspace config");
        let trust_path = directory.path().join("trust.json");
        (directory, workspace, trust_path)
    }

    #[test]
    fn cli_defaults_to_the_in_process_tui() {
        let cli = Cli::try_parse_from(["cookiecode"]).expect("parse CLI");
        assert_eq!(cli.command, None);
    }

    #[test]
    fn cli_parses_daemon() {
        let cli = Cli::try_parse_from(["cookiecode", "daemon"]).expect("parse CLI");
        assert_eq!(cli.command, Some(Command::Daemon));
    }

    #[test]
    fn local_provider_can_be_wired_without_a_secret() {
        let mut config = Config::default();
        config.providers.insert(
            "local".into(),
            ProviderConfig {
                kind: ProviderType::OpenAiCompatible,
                api_key_env: None,
                base_url: Some("http://127.0.0.1:11434/v1".into()),
                api: None,
            },
        );

        assert!(
            provider_registry(&config)
                .expect("wire provider registry")
                .contains_key("local")
        );
    }

    #[test]
    fn untrusted_workspace_config_is_refused_when_prompt_is_rejected() {
        let (_directory, workspace, trust_path) = workspace_with_config("[server]\nport = 8123\n");
        let mut input = Cursor::new(b"no\n");
        let mut output = Vec::new();

        let error = authorize_workspace_config(
            &workspace,
            &trust_path,
            false,
            true,
            &mut input,
            &mut output,
        )
        .expect_err("rejected trust prompt");

        assert!(error.to_string().contains("not trusted"));
        assert!(
            String::from_utf8(output)
                .expect("prompt text")
                .contains("Trust it?")
        );
        let bytes = fs::read(workspace.join(".cookiecode/config.toml")).expect("config bytes");
        assert!(
            TrustStore::load(&trust_path)
                .expect("trust store")
                .needs_retrust(&workspace, &bytes)
                .expect("trust state")
        );
    }

    #[test]
    fn trusted_workspace_config_is_applied() {
        let (_directory, workspace, trust_path) = workspace_with_config("[server]\nport = 8123\n");
        let mut input = Cursor::new(b"yes\n");
        let mut output = Vec::new();

        let _ = authorize_workspace_config(
            &workspace,
            &trust_path,
            false,
            true,
            &mut input,
            &mut output,
        )
        .expect("accept trust prompt");

        let config =
            cookiecode_config::load_layered(None, Some(&workspace.join(".cookiecode/config.toml")))
                .expect("load trusted workspace configuration");
        assert_eq!(config.server.port, 8123);
    }

    #[test]
    fn untrusted_workspace_config_is_refused_without_a_tty() {
        let (_directory, workspace, trust_path) = workspace_with_config("[server]\nport = 8123\n");
        let mut input = Cursor::new(Vec::new());
        let mut output = Vec::new();

        let error = authorize_workspace_config(
            &workspace,
            &trust_path,
            false,
            false,
            &mut input,
            &mut output,
        )
        .expect_err("non-interactive trust is refused");

        assert!(error.to_string().contains("--trust-workspace"));
        assert!(output.is_empty());
    }

    #[test]
    fn explicit_trust_allows_workspace_config_without_a_tty() {
        let (_directory, workspace, trust_path) = workspace_with_config("[server]\nport = 8123\n");
        let mut input = Cursor::new(Vec::new());
        let mut output = Vec::new();

        let _ = authorize_workspace_config(
            &workspace,
            &trust_path,
            true,
            false,
            &mut input,
            &mut output,
        )
        .expect("explicit workspace trust");

        let bytes = fs::read(workspace.join(".cookiecode/config.toml")).expect("config bytes");
        assert!(
            !TrustStore::load(&trust_path)
                .expect("trust store")
                .needs_retrust(&workspace, &bytes)
                .expect("trust state")
        );
    }

    #[test]
    fn approved_workspace_bytes_are_loaded_after_source_changes() {
        let approved = r#"
[providers.approved]
type = "openai-compatible"
base_url = "http://127.0.0.1:11434/v1"

[agents.primary]
type = "primary"
models = [{ provider = "approved", model = "safe" }]
tools = ["read"]
"#;
        let malicious = r#"
[providers.malicious]
type = "openai-compatible"
base_url = "http://127.0.0.1:11434/v1"

[agents.primary]
type = "primary"
models = [{ provider = "malicious", model = "unsafe" }]
tools = ["bash"]
"#;
        let (_directory, workspace, trust_path) = workspace_with_config(approved);
        let mut input = Cursor::new(b"yes\n");
        let mut output = Vec::new();
        let approved_bytes = authorize_workspace_config(
            &workspace,
            &trust_path,
            false,
            true,
            &mut input,
            &mut output,
        )
        .expect("approve workspace configuration")
        .expect("workspace configuration exists");

        fs::write(workspace.join(".cookiecode/config.toml"), malicious)
            .expect("replace workspace configuration");
        let config = load_config_with_approved_workspace_config(Some(&approved_bytes))
            .expect("load approved workspace configuration");

        assert_eq!(config.agents["primary"].tools, ["read"]);
        let providers = provider_registry(&config).expect("construct providers");
        assert!(providers.contains_key("approved"));
        assert!(!providers.contains_key("malicious"));
    }

    #[cfg(unix)]
    #[test]
    fn approved_workspace_staging_is_private() {
        use std::os::unix::fs::PermissionsExt;

        let workspace = ApprovedWorkspaceConfig::new(Some(b"[server]\nport = 8123\n"))
            .expect("create approved workspace");
        assert_eq!(
            fs::metadata(&workspace.workspace)
                .expect("workspace metadata")
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(workspace.workspace.join(".cookiecode"))
                .expect("config directory metadata")
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(workspace.workspace.join(".cookiecode/config.toml"))
                .expect("config metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[tokio::test]
    async fn in_process_composition_shuts_down_cleanly() {
        let directory = TestDirectory::new();
        let config = Config::default();
        let engine = Engine::open(EngineOptions {
            data_dir: directory.path().join("data"),
            cwd: directory.path().to_owned(),
            config: config.clone(),
            providers: HashMap::new(),
            tools: Vec::new(),
        })
        .expect("open engine");
        engine.register_tool_provider(Arc::new(BuiltinTools::new(directory.path())));
        engine.register_tool_provider(Arc::new(DelegateToolProvider::new(
            engine.client(),
            &config,
        )));
        let server = Arc::new(Server::new(engine.clone(), config, HashMap::new()));
        let (client_stream, server_stream) = in_process_pair(8);
        let server_task = tokio::spawn(server.clone().serve_stream(server_stream));
        let client = cookiecode_tui::Client::connect_stream(client_stream);

        client.handshake().await.expect("handshake");
        server.shutdown();
        server_task
            .await
            .expect("join server task")
            .expect("serve stream");
        engine.shutdown().await;
    }
}
