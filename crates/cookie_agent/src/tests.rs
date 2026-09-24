/// A syntactically valid 43-character base64url daemon token.
const TEST_TOKEN: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";

/// Compose with configuration loaded from the workspace only, isolating tests from any
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
        cookie_agent_config::load_from_roots(None, Some(&workspace.join(".cookie-agent")))
            .context("load isolated workspace configuration and agents")?;
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
    for _ in 0..10_000 {
        if runtime.engine.current_runtime().models.catalog().source
            == cookie_agent_models::catalog::CatalogSource::Network
        {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("initial catalog refresh was not published");
}

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
    ProviderConfigurationState, ProviderPresence, ProviderSupport, SafeDisplayText, SetupFieldId,
    SetupFieldValidation,
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
        Some(Command::Daemon { port: None })
    );
    assert_eq!(
        Cli::try_parse_from(["cookie", "daemon", "--port", "0"])
            .unwrap()
            .command,
        Some(Command::Daemon { port: Some(0) })
    );
    assert!(Cli::try_parse_from(["cookie", "--trust-workspace", "daemon"]).is_err());
    assert!(Cli::try_parse_from(["cookie", "connect", "openai", "--api-key", "sentinel"]).is_err());
    assert!(Cli::try_parse_from(["cookie", "mcp", "approve", "github"]).is_err());
    assert!(Cli::try_parse_from(["cookie", "mcp", "reject", "github"]).is_err());
    assert_eq!(
        Cli::try_parse_from(["cookie", "mcp", "--token", TEST_TOKEN, "auth", "remote"])
            .unwrap()
            .command,
        Some(Command::Mcp {
            command: McpCommand::Auth {
                server: "remote".into()
            },
            url: DEFAULT_WEBSOCKET_URL.into(),
            token: TEST_TOKEN.into(),
        })
    );
    assert_eq!(
        Cli::try_parse_from([
            "cookie",
            "run",
            "check the build",
            "--permission-mode",
            "ask",
            "--allowed-tools",
            "read,bash",
            "--max-turns",
            "7",
            "--timeout",
            "30",
            "--output",
            "json",
            "--agent",
            "primary",
            "--preset",
            "python",
            "--model",
            "custom.local/test",
            "--variant",
            "base",
            "--output-file",
            "result.jsonl",
            "--verbose",
        ])
        .unwrap()
        .command,
        Some(Command::Run {
            args: Box::new(run::RunArgs {
                positional_prompt: Some("check the build".into()),
                prompt: None,
                prompt_file: None,
                agent: Some("primary".parse().unwrap()),
                preset: Some("python".into()),
                model: Some("custom.local/test".parse().unwrap()),
                variant: Some("base".into()),
                permission_mode: run::PermissionModeArg::Ask,
                allowed_tools: vec![run::AllowedTool::Read, run::AllowedTool::Bash],
                skill: None,
                skill_args: None,
                max_turns: 7,
                timeout: 30,
                resume_session: None,
                data_dir: None,
                output: Some(run::OutputMode::Json),
                output_file: Some("result.jsonl".into()),
                verbose: true,
                json: false,
            }),
        })
    );
    for prompt in [
        ["cookie", "run", "-p", "named"],
        ["cookie", "run", "--prompt", "-"],
        ["cookie", "run", "-f", "prompt.txt"],
    ] {
        assert!(Cli::try_parse_from(prompt).is_ok());
    }
    let Command::Run { args } = Cli::try_parse_from([
        "cookie",
        "run",
        "-p",
        "repeat tools",
        "--allowed-tools",
        "read",
        "--allowed-tools",
        "bash,mcp",
        "--json",
    ])
    .unwrap()
    .command
    .unwrap() else {
        panic!("run command");
    };
    assert_eq!(
        args.allowed_tools,
        [
            run::AllowedTool::Read,
            run::AllowedTool::Bash,
            run::AllowedTool::Mcp,
        ]
    );
    assert_eq!(args.output_mode(), run::OutputMode::Json);
    let Command::Run { args } = Cli::try_parse_from([
        "cookie",
        "run",
        "prompt",
        "--skill",
        "release-check",
        "--skill-args",
        "v1.2.0",
        "--allowed-tools",
        "skill:release-check",
    ])
    .unwrap()
    .command
    .unwrap() else {
        panic!("run command");
    };
    assert_eq!(args.skill.as_deref(), Some("release-check"));
    assert_eq!(args.skill_args.as_deref(), Some("v1.2.0"));
    assert_eq!(
        args.allowed_tools,
        [run::AllowedTool::Skill("release-check".into())]
    );
    assert!(Cli::try_parse_from(["cookie", "run", "prompt", "--skill-args", "orphan"]).is_err());
    assert!(Cli::try_parse_from(["cookie", "run"]).is_err());
    assert!(Cli::try_parse_from(["cookie", "run", "positional", "-p", "named"]).is_err());
    assert!(Cli::try_parse_from(["cookie", "run", "-p", "named", "-f", "prompt.txt"]).is_err());
    assert!(
        Cli::try_parse_from(["cookie", "run", "prompt", "--json", "--output", "text"]).is_err()
    );
    let Command::Run { args } = Cli::try_parse_from([
        "cookie",
        "run",
        "prompt",
        "--output",
        "none",
        "--output-file",
        "result.txt",
    ])
    .unwrap()
    .command
    .unwrap() else {
        panic!("run command");
    };
    assert!(args.validate_cli().is_err());
    assert!(Cli::try_parse_from(["cookie", "run", "prompt", "--max-turns", "0"]).is_err());
    assert!(Cli::try_parse_from(["cookie", "run", "prompt", "--timeout", "0"]).is_err());
}

#[test]
fn attach_connect_and_disconnect_are_cwd_independent() {
    for command in [
        Command::Attach {
            url: DEFAULT_WEBSOCKET_URL.into(),
            token: TEST_TOKEN.into(),
        },
        Command::Connect {
            provider_id: None,
            url: DEFAULT_WEBSOCKET_URL.into(),
            token: TEST_TOKEN.into(),
        },
        Command::Disconnect {
            provider_id: None,
            url: DEFAULT_WEBSOCKET_URL.into(),
            token: TEST_TOKEN.into(),
        },
        Command::Mcp {
            command: McpCommand::List,
            url: DEFAULT_WEBSOCKET_URL.into(),
            token: TEST_TOKEN.into(),
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
fn websocket_subcommands_require_an_explicit_token() {
    for arguments in [
        vec!["cookie", "attach"],
        vec!["cookie", "connect"],
        vec!["cookie", "disconnect"],
        vec!["cookie", "mcp", "list"],
    ] {
        assert!(
            Cli::try_parse_from(&arguments).is_err(),
            "missing token accepted for {arguments:?}"
        );
    }
    for arguments in [
        vec!["cookie", "attach", "--token", TEST_TOKEN],
        vec!["cookie", "connect", "--token", TEST_TOKEN],
        vec!["cookie", "disconnect", "--token", TEST_TOKEN],
        vec!["cookie", "mcp", "--token", TEST_TOKEN, "list"],
    ] {
        assert!(
            Cli::try_parse_from(&arguments).is_ok(),
            "token rejected for {arguments:?}"
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
    let mut runtime = compose_isolated(
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
    // Startup itself never waits on the network.
    assert_eq!(fetches.load(Ordering::SeqCst), 0);
    await_initial_catalog_refresh(&runtime).await;
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
    let result = compose_isolated(
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
            cache: None,
            headers: BTreeMap::new(),
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
        auth_method: cookie_agent_protocol::AuthMethodId::new("aws-sigv4-credentials-v1").unwrap(),
        auth_values: cookie_agent_models::provider_store::ProviderAuthValues::new(BTreeMap::from(
            [
                (
                    cookie_agent_protocol::AuthFieldName::new("access_key_id").unwrap(),
                    "invented-access-placeholder".into(),
                ),
                (
                    cookie_agent_protocol::AuthFieldName::new("secret_access_key").unwrap(),
                    "invented-secret-placeholder".into(),
                ),
            ],
        ))
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

    let mut runtime = compose_isolated(
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

    let mut recomposed = compose_isolated(
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
    let mut runtime = compose_isolated(
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
        expected_connection_generation: Some(connected.durable_connection.connection_generation),
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
    let mut runtime = compose_isolated(
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
    await_initial_catalog_refresh(&runtime).await;
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
    let mut runtime = compose_isolated(
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
    // The first refresh runs right after startup, replacing the bundled catalog.
    await_initial_catalog_refresh(&runtime).await;
    assert_eq!(fetches.load(Ordering::SeqCst), 1);
    let changed = changes.recv().await.unwrap();
    assert_eq!(
        changed.reasons,
        vec![cookie_agent_protocol::RuntimeChangeReason::CatalogRefreshed]
    );

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
