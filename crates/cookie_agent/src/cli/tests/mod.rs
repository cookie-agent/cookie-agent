//! Tests of the `cookie` command line, by subcommand, and what they share.

use std::{
    collections::{BTreeMap, VecDeque},
    env,
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use clap::Parser;
use cookie_agent::run;
use cookie_agent_models::{
    ModelManager,
    catalog::{CatalogManager, CatalogTransport},
    provider_store::ProviderStore,
};
use cookie_agent_protocol::{
    AuthCredentialDescriptor, AuthFieldName, AuthMethodDescriptor, ClientConnectId,
    ClientRequestId, CredentialFieldType, EffectiveAuthState, ProviderConfigurationState,
    ProviderDescriptor, ProviderDisconnectParams, ProviderId, ProviderPresence, ProviderSupport,
    ProviderSupportState, SafeCode, SafeDisplayText, SafeSetupValue, SetupFieldDescriptor,
    SetupFieldId, SetupFieldType, SetupFieldValidation, paths, setup_value_text,
};
use cookie_agent_protocol_transport::{Client, validate_websocket_url};
use uuid::Uuid;
use zeroize::Zeroizing;

use super::*;
use super::{
    compose::*, connect::*, daemon::*, frontend::*, mcp::McpCommand, runtime::*,
    sessions::SessionsCommand, web::*,
};
use crate::local_daemon;

/// A syntactically valid 43-character base64url daemon token.
const TEST_TOKEN: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";

/// `text` without the stamp lines the daemon writes into a memory's
/// frontmatter.
fn unstamped(text: &str) -> String {
    text.split_inclusive('\n')
        .filter(|line| !line.starts_with("created: ") && !line.starts_with("modified: "))
        .collect()
}

/// Compose with the workspace's `.cookie-agent` as the user layer root, isolating tests from any
/// real user-level configuration on the host.
async fn compose_isolated<T: CatalogTransport + 'static>(
    workspace: &std::path::Path,
    open_transport: impl FnOnce() -> anyhow::Result<T>,
    open_catalog: impl FnOnce(T) -> CatalogManager<T>,
    open_provider_store: impl FnOnce() -> Result<
        ProviderStore,
        cookie_agent_models::provider_store::ProviderStoreError,
    >,
    open_data_dir: impl FnOnce() -> anyhow::Result<std::path::PathBuf>,
) -> anyhow::Result<Runtime> {
    let configuration =
        cookie_agent_config::load_from_user_root(Some(&workspace.join(".cookie-agent")))
            .context("load isolated user configuration and agents")?;
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

/// Startup serves the cached or bundled catalog and refreshes from the
/// transport in the background; waits until that first refresh is published.
async fn await_initial_catalog_refresh(runtime: &Runtime) {
    let published = || {
        runtime.engine.current_runtime().models.catalog().source
            == cookie_agent_models::catalog::CatalogSource::Network
    };
    // The engine stores a runtime before it announces it, so a check after
    // subscribing sees a publication that came first.
    let mut changes = runtime.engine.subscribe_runtime_changes();
    // A plain thread keeps the deadline in wall-clock time: on a paused
    // clock a tokio timer would fire as soon as the runtime idled, while
    // the recompile still runs on the blocking pool.
    let (expire, mut expired) = tokio::sync::oneshot::channel::<()>();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(60));
        let _ = expire.send(());
    });
    while !published() {
        tokio::select! {
            change = changes.recv() => {
                if let Err(tokio::sync::broadcast::error::RecvError::Closed) = change {
                    panic!("the engine closed its runtime changes");
                }
            }
            _ = &mut expired => panic!("initial catalog refresh was not published"),
        }
    }
}

/// Waits, on a paused clock, until the catalog refresh loop sleeps again
/// after a publication: the recompile runs on the blocking pool, which keeps
/// the clock from advancing until it is done, so a timer of the test's own
/// fires only once the loop's sleep is set.
async fn catalog_refresh_settled() {
    tokio::time::sleep(Duration::from_millis(1)).await;
}

#[derive(Clone)]
struct OfflineTransport {
    fetches: Arc<AtomicUsize>,
    body: Arc<Vec<u8>>,
}

impl CatalogTransport for OfflineTransport {
    fn fetch(
        &self,
        _: Option<String>,
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
        _: Option<String>,
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
    Arc::new(br#"{"providers":{"test":{"id":"test","env":["TEST_API_KEY"],"npm":"@ai-sdk/openai-compatible","api":"https://example.invalid/v1","name":"Test","doc":"https://example.invalid/docs","models":{"group/model":{"id":"group/model","name":"Group Model","description":"test model","attachment":false,"reasoning":false,"tool_call":false,"temperature":true,"structured_output":false,"open_weights":false,"release_date":"2026-08-01","last_updated":"2026-08-02","modalities":{"input":["text"],"output":["text"]},"limit":{"context":8192,"output":1024}}}},"broken":{"id":"broken","env":["BROKEN_KEY"],"npm":"x","name":"Broken","doc":42,"models":{}}},"models":{"group/model":{"id":"group/model","name":"Canonical","description":"metadata only","attachment":false,"reasoning":false,"tool_call":false,"temperature":false,"open_weights":false,"release_date":"2026-08-01","last_updated":"2026-08-02","modalities":{"input":["text"],"output":["text"]},"limit":{"context":8192,"output":1024}}}}"#.to_vec())
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
        documentation_url: None,
        environment: Vec::new(),
        model_counts: cookie_agent_protocol::ProviderModelCounts::default(),
        unavailable_models: Vec::new(),
        sign_in_methods: Vec::new(),
        sign_in_account: None,
    }
}

struct ScriptedMemoryIo {
    edits: VecDeque<String>,
    confirmations: VecDeque<bool>,
    seen: Vec<String>,
    output: Vec<String>,
    /// Written to a file before the next edit returns, as another writer
    /// would.
    meanwhile: Option<(std::path::PathBuf, String)>,
}

impl ScriptedMemoryIo {
    fn new(edits: &[&str], confirmations: &[bool]) -> Self {
        Self {
            edits: edits.iter().map(|edit| (*edit).to_owned()).collect(),
            confirmations: confirmations.iter().copied().collect(),
            seen: Vec::new(),
            output: Vec::new(),
            meanwhile: None,
        }
    }
}

impl crate::cli::memory::MemoryIo for ScriptedMemoryIo {
    fn write_line(&mut self, line: &str) -> anyhow::Result<()> {
        self.output.push(line.to_owned());
        Ok(())
    }

    fn edit(&mut self, _url: &str, text: &str) -> anyhow::Result<String> {
        self.seen.push(text.to_owned());
        if let Some((path, text)) = self.meanwhile.take() {
            std::fs::write(path, text).unwrap();
        }
        self.edits.pop_front().context("missing scripted edit")
    }

    fn confirm(&mut self, _prompt: &str, _default: bool) -> anyhow::Result<bool> {
        self.confirmations
            .pop_front()
            .context("missing scripted confirmation")
    }
}

/// A token the test daemon does not accept.
const OTHER_TOKEN: &str = "BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB";

/// A workspace whose user configuration has a custom model and a primary
/// agent, so sessions can be created without a provider connection.
fn write_session_config(workspace: &Path) {
    use std::os::unix::fs::PermissionsExt as _;
    write_empty_config(workspace);
    let root = workspace.join(".cookie-agent");
    let config = root.join("config.toml");
    std::fs::write(
        &config,
        r#"[providers."custom.local"]
source = "custom"
endpoint = "http://127.0.0.1:9/v1"
adaptor = "openai-compatible"
auth = { method = "no-auth-v1", values = {} }

[providers."custom.local".models.test]
display_name = "Local Daemon Test"
capabilities = { input = ["text"], output = ["text"], context_tokens = 65536, output_tokens = 2048, tool_calling = true, parallel_tool_calls = false, structured_output = false, reasoning = false, temperature = true, top_p = true, seed = false, native_replay = "unsupported", media = {} }
"#,
    )
    .unwrap();
    std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o600)).unwrap();
    let agents = root.join("agents");
    private_directory(&agents);
    std::fs::write(
        agents.join("primary.md"),
        "---\ndescription: Local daemon agent\nmode: primary\nenabled: true\nmodels:\n  - { model: \"custom.local/test\", variant: null }\n---\nHelp.\n",
    )
    .unwrap();
}

fn session_selection() -> cookie_agent_protocol::RunSelection {
    cookie_agent_protocol::RunSelection {
        agent: "primary".parse().unwrap(),
        model: cookie_agent_protocol::ModelSelection {
            model: "custom.local/test".parse().unwrap(),
            variant: None,
        },
        preset: None,
    }
}

/// A daemon serving `/ws` on loopback with [`TEST_TOKEN`], as
/// `cookie daemon` does, over a workspace under `root`.
struct ListeningDaemon {
    runtime: Runtime,
    listener: cookie_agent_server::RunningServer,
    workspace: PathBuf,
}

impl ListeningDaemon {
    async fn start(root: &Path) -> Self {
        let workspace = root.join("daemon-workspace");
        let cache_anchor = root.join("daemon-cache");
        write_session_config(&workspace);
        private_directory(&cache_anchor);
        let runtime = compose_isolated(
            &workspace,
            || {
                Ok(OfflineTransport {
                    fetches: Arc::new(AtomicUsize::new(0)),
                    body: openai_catalog(),
                })
            },
            |transport| CatalogManager::in_directory(transport, &cache_anchor, "catalog"),
            || ProviderStore::open(root.join("daemon-providers")),
            || Ok(root.join("daemon-data")),
        )
        .await
        .unwrap();
        let listener = runtime
            .server
            .clone()
            .serve_on(
                std::net::SocketAddr::from(([127, 0, 0, 1], 0)),
                Zeroizing::new(TEST_TOKEN.to_owned()),
                None,
            )
            .await
            .unwrap();
        Self {
            runtime,
            listener,
            workspace,
        }
    }

    fn port(&self) -> u16 {
        self.listener.address().port()
    }

    async fn stop(mut self) {
        self.runtime.server.shutdown();
        self.runtime.stop_catalog_refresh().await;
        self.listener.wait().await;
        self.runtime.engine.shutdown().await;
    }
}

/// `[server]` of a scratch home whose `config.toml` holds `server`, as plain
/// `cookie` reads it on this machine.
fn scratch_server(home: &Path, server: &str) -> cookie_agent_config::ServerConfig {
    use std::os::unix::fs::PermissionsExt as _;
    private_directory(home);
    let config = home.join("config.toml");
    std::fs::write(&config, format!("[server]\n{server}")).unwrap();
    std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o600)).unwrap();
    cookie_agent_config::load_from_user_root(Some(home))
        .unwrap()
        .runtime
        .server
}

fn probe_identity() -> cookie_agent_protocol::ClientIdentity {
    client_identity("tui")
}

const ONBOARD_PROVIDER: &str = r#"
[providers."custom.test"]
source = "custom"
endpoint = "http://127.0.0.1:9/v1"
adaptor = "openai-compatible"
auth = { method = "no-auth-v1", values = {} }

[providers."custom.test".models."a-model"]
display_name = "A Model"
capabilities = { input = ["text"], output = ["text"], context_tokens = 4096, output_tokens = 1024, tool_calling = true, parallel_tool_calls = true, structured_output = false, reasoning = false, temperature = true, top_p = true, seed = true, native_replay = "unsupported", media = {} }
variants = { low = { }, high = { } }
default_variant = "high"

[providers."custom.test".models."z-model"]
display_name = "Z Model"
capabilities = { input = ["text"], output = ["text"], context_tokens = 4096, output_tokens = 1024, tool_calling = true, parallel_tool_calls = true, structured_output = false, reasoning = false, temperature = true, top_p = true, seed = true, native_replay = "unsupported", media = {} }
"#;

/// A daemon with the provider `custom.test` and its two models, served in
/// process, and a kit archive in the temporary directory.
async fn onboard_daemon(temporary: &std::path::Path) -> (Client, std::path::PathBuf) {
    use std::os::unix::fs::PermissionsExt as _;
    let workspace = temporary.join("workspace");
    let cache_anchor = temporary.join("cache-anchor");
    private_directory(&workspace);
    private_directory(&workspace.join(".cookie-agent"));
    private_directory(&cache_anchor);
    let config = workspace.join(".cookie-agent/config.toml");
    std::fs::write(&config, ONBOARD_PROVIDER).unwrap();
    std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o600)).unwrap();
    let runtime = compose_isolated(
        &workspace,
        || {
            Ok(OfflineTransport {
                fetches: Arc::new(AtomicUsize::new(0)),
                body: openai_catalog(),
            })
        },
        |transport| CatalogManager::in_directory(transport, &cache_anchor, "catalog"),
        || ProviderStore::open(temporary.join("providers")),
        || Ok(temporary.join("data")),
    )
    .await
    .unwrap();
    let (client_stream, server_stream) = cookie_agent_protocol::in_process_pair(32);
    tokio::spawn(runtime.server.clone().serve_stream(
        server_stream,
        cookie_agent_protocol_transport::Authentication::Authenticated,
    ));
    let client = Client::connect_stream(client_stream);
    client.handshake().await.unwrap();
    // The default agent is the user's own here, so the run fetches no
    // official agents.
    let root = workspace.join(".cookie-agent");
    std::fs::create_dir_all(root.join("agents")).unwrap();
    std::fs::write(
        root.join("agents/primary.md"),
        b"---\ndescription: Primary\nmode: primary\nmodels: [\"${default_model}\"]\n---\nWork.\n",
    )
    .unwrap();
    runtime
        .engine
        .reload_configuration()
        .await
        .expect("reload with the agent");
    // Keep the runtime's engine alive with the server task.
    std::mem::forget(runtime);
    (client, root)
}

mod compose;
mod config;
mod connect;
mod cron;
mod daemon;
mod frontend;
mod machine;
mod memory;
mod onboard;
mod parse;
mod project;
mod runtime;
mod skills;
mod web;
