use cookie_agent_protocol::ProviderId;

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

#[test]
fn error_details_is_a_flag_of_every_command() {
    assert!(
        !Cli::try_parse_from(["cookie", "config", "status"])
            .unwrap()
            .error_details
    );
    for arguments in [
        &["cookie", "--error-details", "config", "status"][..],
        &["cookie", "config", "status", "--error-details"][..],
    ] {
        assert!(
            Cli::try_parse_from(arguments).unwrap().error_details,
            "{arguments:?}"
        );
    }
}

#[test]
fn cli_commands_and_removed_flags_are_exact() {
    assert_eq!(Cli::try_parse_from(["cookie"]).unwrap().command, None);
    assert!(!Cli::try_parse_from(["cookie"]).unwrap().standalone);
    let standalone = Cli::try_parse_from(["cookie", "--standalone"]).unwrap();
    assert!(standalone.standalone);
    assert_eq!(standalone.command, None);
    // Only plain `cookie` takes it; `main_result` refuses it before a
    // subcommand.
    assert!(Cli::try_parse_from(["cookie", "run", "--standalone", "hello"]).is_err());
    let daemon = |port| Command::Daemon {
        command: None,
        port,
        listen: None,
        tls_cert: None,
        tls_key: None,
    };
    assert_eq!(
        Cli::try_parse_from(["cookie", "daemon"]).unwrap().command,
        Some(daemon(None))
    );
    assert_eq!(
        Cli::try_parse_from(["cookie", "daemon", "--port", "0"])
            .unwrap()
            .command,
        Some(daemon(Some(0)))
    );
    assert_eq!(
        Cli::try_parse_from([
            "cookie",
            "daemon",
            "--listen",
            "0.0.0.0:7419",
            "--tls-cert",
            "cert.pem",
            "--tls-key",
            "key.pem"
        ])
        .unwrap()
        .command,
        Some(Command::Daemon {
            command: None,
            port: None,
            listen: Some("0.0.0.0:7419".parse().unwrap()),
            tls_cert: Some("cert.pem".into()),
            tls_key: Some("key.pem".into()),
        })
    );
    let restart = |now, url: Option<&str>, token: Option<&str>| Command::Daemon {
        command: Some(daemon_restart::DaemonCommand::Restart {
            now,
            url: url.map(str::to_owned),
            token: token.map(str::to_owned),
        }),
        port: None,
        listen: None,
        tls_cert: None,
        tls_key: None,
    };
    assert_eq!(
        Cli::try_parse_from(["cookie", "daemon", "restart"])
            .unwrap()
            .command,
        Some(restart(false, None, None))
    );
    assert_eq!(
        Cli::try_parse_from([
            "cookie",
            "daemon",
            "restart",
            "--now",
            "--url",
            "ws://127.0.0.1:7419/ws",
            "--token",
            "secret"
        ])
        .unwrap()
        .command,
        Some(restart(
            true,
            Some("ws://127.0.0.1:7419/ws"),
            Some("secret")
        ))
    );
    // Serving options belong to the daemon itself, not to its restart.
    assert!(Cli::try_parse_from(["cookie", "daemon", "--port", "0", "restart"]).is_err());
    assert!(Cli::try_parse_from(["cookie", "daemon", "restart", "--port", "0"]).is_err());
    assert_eq!(
        Cli::try_parse_from(["cookie", "sessions", "upgrade"])
            .unwrap()
            .command,
        Some(Command::Sessions {
            command: SessionsCommand::Upgrade { all: false },
        })
    );
    assert_eq!(
        Cli::try_parse_from(["cookie", "sessions", "upgrade", "--all"])
            .unwrap()
            .command,
        Some(Command::Sessions {
            command: SessionsCommand::Upgrade { all: true },
        })
    );
    for arguments in [
        vec!["cookie", "sessions"],
        vec![
            "cookie",
            "daemon",
            "--port",
            "1",
            "--listen",
            "0.0.0.0:7419",
        ],
        vec!["cookie", "daemon", "--tls-cert", "cert.pem"],
        vec!["cookie", "daemon", "--tls-key", "key.pem"],
        vec!["cookie", "daemon", "--listen", "not-an-address"],
    ] {
        assert!(Cli::try_parse_from(&arguments).is_err(), "{arguments:?}");
    }
    assert_eq!(
        Cli::try_parse_from([
            "cookie",
            "runtime",
            "serve",
            "--stdio",
            "--root",
            "~/my repo",
            "--root",
            "/srv"
        ])
        .unwrap()
        .command,
        Some(Command::Runtime {
            command: RuntimeCommand::Serve {
                stdio: true,
                daemon: None,
                id: None,
                token: None,
                roots: vec!["~/my repo".into(), "/srv".into()],
            }
        })
    );
    assert_eq!(
        Cli::try_parse_from([
            "cookie",
            "runtime",
            "serve",
            "--daemon",
            "wss://daemon.example.com:7419/ws",
            "--id",
            "labpc"
        ])
        .unwrap()
        .command,
        Some(Command::Runtime {
            command: RuntimeCommand::Serve {
                stdio: false,
                daemon: Some(Some("wss://daemon.example.com:7419/ws".into())),
                id: Some("labpc".into()),
                token: None,
                roots: Vec::new(),
            }
        })
    );
    // `--daemon` alone dials in to the configured daemon as this host.
    assert_eq!(
        Cli::try_parse_from(["cookie", "runtime", "serve", "--daemon"])
            .unwrap()
            .command,
        Some(Command::Runtime {
            command: RuntimeCommand::Serve {
                stdio: false,
                daemon: Some(None),
                id: None,
                token: None,
                roots: Vec::new(),
            }
        })
    );
    for arguments in [
        vec!["cookie", "runtime", "serve"],
        vec!["cookie", "runtime", "serve", "--root", "/srv"],
        vec!["cookie", "runtime", "serve", "--stdio", "--id", "labpc"],
        vec![
            "cookie",
            "runtime",
            "serve",
            "--stdio",
            "--daemon",
            "ws://127.0.0.1:7419/ws",
            "--id",
            "labpc",
        ],
        vec!["cookie", "runtime", "coding", "--stdio"],
        vec![
            "cookie",
            "runtime",
            "serve",
            "--stdio",
            "--workspace",
            "/srv",
        ],
    ] {
        assert!(Cli::try_parse_from(&arguments).is_err(), "{arguments:?}");
    }
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
            url: None,
            token: Some(TEST_TOKEN.into()),
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
                allowed_tools: vec![
                    run::AllowedTool::Action(cookie_agent_protocol::PermissionAction::READ),
                    run::AllowedTool::Action(cookie_agent_protocol::PermissionAction::BASH)
                ],
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
            run::AllowedTool::Action(cookie_agent_protocol::PermissionAction::READ),
            run::AllowedTool::Action(cookie_agent_protocol::PermissionAction::BASH),
            run::AllowedTool::Action(cookie_agent_protocol::PermissionAction::MCP),
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
fn every_token_flag_takes_a_token_that_starts_with_a_hyphen() {
    // Tokens written before generated ones avoided a leading `-` stay valid.
    let token = format!("-e{}", &TEST_TOKEN[2..]);
    assert_eq!(
        Cli::try_parse_from(["cookie", "attach", "--token", &token])
            .unwrap()
            .command,
        Some(Command::Attach {
            url: None,
            token: Some(token.clone()),
        })
    );
    for arguments in [
        &["cookie", "web", "--token"][..],
        &["cookie", "connect", "--token"][..],
        &["cookie", "disconnect", "--token"][..],
        &["cookie", "mcp", "--token"][..],
        &["cookie", "config", "status", "--token"][..],
        &["cookie", "machine", "list", "--token"][..],
        &["cookie", "memory", "list", "--token"][..],
        &[
            "cookie",
            "runtime",
            "serve",
            "--daemon",
            "ws://127.0.0.1:7419/ws",
            "--id",
            "devbox",
            "--token",
        ][..],
    ] {
        let mut arguments = arguments.to_vec();
        arguments.push(&token);
        if arguments[1] == "mcp" {
            // `mcp`'s flags come before its subcommand.
            arguments.push("list");
        }
        assert!(
            Cli::try_parse_from(&arguments).is_ok(),
            "{arguments:?}: {:?}",
            Cli::try_parse_from(&arguments).err()
        );
    }
}

#[test]
fn attach_connect_and_disconnect_are_cwd_independent() {
    for command in [
        Command::Attach {
            url: None,
            token: Some(TEST_TOKEN.into()),
        },
        Command::Web {
            url: None,
            token: Some(TEST_TOKEN.into()),
            open: false,
        },
        Command::Connect {
            provider_id: None,
            url: None,
            token: Some(TEST_TOKEN.into()),
        },
        Command::Disconnect {
            provider_id: None,
            url: None,
            token: Some(TEST_TOKEN.into()),
        },
        Command::Mcp {
            command: McpCommand::List,
            url: None,
            token: Some(TEST_TOKEN.into()),
        },
        Command::Runtime {
            command: RuntimeCommand::Serve {
                stdio: true,
                daemon: None,
                id: None,
                token: None,
                roots: vec!["~/repo".into()],
            },
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
fn websocket_subcommands_take_the_daemon_from_flags_or_the_configuration() {
    for arguments in [
        vec!["cookie", "attach"],
        vec!["cookie", "attach", "--token", TEST_TOKEN],
        vec!["cookie", "connect"],
        vec!["cookie", "disconnect"],
        vec!["cookie", "mcp", "list"],
        vec!["cookie", "mcp", "--token", TEST_TOKEN, "list"],
    ] {
        assert!(
            Cli::try_parse_from(&arguments).is_ok(),
            "rejected {arguments:?}"
        );
    }
    let server = |url: Option<&str>, token: Option<&str>| cookie_agent_config::ServerConfig {
        url: url.map(str::to_owned),
        token: token.map(str::to_owned),
        ..Default::default()
    };
    // Flags win and leave the configuration unread.
    let explicit = resolve_daemon_target(
        Some("wss://daemon.example:7419/ws".into()),
        Some("from-flag".into()),
        || panic!("the configuration is not read when both are given"),
    )
    .unwrap();
    assert_eq!(explicit.url, "wss://daemon.example:7419/ws");
    assert_eq!(explicit.token.as_str(), "from-flag");
    // Each missing one comes from `[server]`: its url, else the local daemon.
    let local = resolve_daemon_target(None, None, || Ok(server(None, Some(TEST_TOKEN)))).unwrap();
    assert_eq!(local.url, "ws://127.0.0.1:7419/ws");
    assert_eq!(local.token.as_str(), TEST_TOKEN);
    let remote = resolve_daemon_target(None, Some("from-flag".into()), || {
        Ok(server(Some("ws://laptop:7419/ws"), Some(TEST_TOKEN)))
    })
    .unwrap();
    assert_eq!(remote.url, "ws://laptop:7419/ws");
    assert_eq!(remote.token.as_str(), "from-flag");
    let error = resolve_daemon_target(None, None, || Ok(server(None, None)))
        .err()
        .expect("no token anywhere");
    assert!(error.to_string().contains("[server]"), "{error}");
    assert!(
        resolve_daemon_target(Some("ws://laptop/other".into()), Some("x".into()), || {
            panic!("unread")
        })
        .is_err()
    );
}

#[test]
fn websocket_endpoints_name_the_exact_client_path() {
    for url in [
        "ws://127.0.0.1:7419/ws",
        "wss://localhost:7419/ws",
        "ws://[::1]:7419/ws",
        "wss://daemon.example.com:7419/ws",
    ] {
        validate_websocket_url(url).unwrap();
    }
    for url in [
        "http://127.0.0.1:7419/ws",
        "ws://user:pass@127.0.0.1:7419/ws",
        "ws://127.0.0.1:7419/other",
        "ws://127.0.0.1:7419/ws?id=x",
    ] {
        assert!(validate_websocket_url(url).is_err(), "{url}");
    }
    assert_eq!(
        cookie_agent_protocol::runtime_dial_in_url("wss://daemon.example.com:7419/ws", "labpc")
            .unwrap(),
        "wss://daemon.example.com:7419/runtime?id=labpc"
    );
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

fn unusable_provider() -> ProviderDescriptor {
    let mut provider = provider(ProviderSupportState::Supported);
    provider.configuration = ProviderConfigurationState::Stored;
    provider.documentation_url = Some(SafeDisplayText::new("https://example.test/docs").unwrap());
    provider.environment = vec![
        cookie_agent_protocol::CredentialFieldName::new("OPENAI_ORG").unwrap(),
        cookie_agent_protocol::CredentialFieldName::new("OPENAI_API_KEY").unwrap(),
    ];
    provider.model_counts = cookie_agent_protocol::ProviderModelCounts {
        quarantined: 1,
        unsupported: 1,
        ..Default::default()
    };
    provider.unavailable_models = serde_json::from_value(serde_json::json!([
        {"id": "gpt-q", "display_name": "gpt-q", "kind": "quarantined", "reason": "invalid_catalog_model_record"},
        {"id": "gpt-u", "display_name": "GPT U", "kind": "unsupported", "reason": "unsupported_model_capabilities"}
    ]))
    .unwrap();
    provider
}

#[test]
fn provider_listing_and_details_explain_unavailable_models() {
    let providers = vec![unusable_provider()];
    let mut io = ScriptedConnectIo {
        public: VecDeque::from(["1".to_owned()]),
        ..Default::default()
    };
    let chosen = choose_provider(&providers, None, &mut io).unwrap();
    print_provider_details(chosen, "sha256:catalog", &mut io).unwrap();
    for line in [
        "  1. OpenAI (openai) — supported · models: 0 usable, 1 quarantined, 1 unsupported",
        "Models: 0 usable, 1 quarantined, 1 unsupported",
        "  gpt-q — quarantined: invalid_catalog_model_record",
        "  gpt-u — unsupported: unsupported_model_capabilities",
        "Docs: https://example.test/docs",
        "Config alternative: [providers.openai] source = \"models_dev\", api_key = \"${env:OPENAI_API_KEY}\"",
    ] {
        assert!(
            io.output.iter().any(|output| output == line),
            "missing {line:?}: {:#?}",
            io.output
        );
    }
}

#[test]
fn headless_model_errors_name_the_unavailable_reason() {
    let providers = vec![unusable_provider()];
    assert_eq!(
        cookie_agent::availability::model_unavailable_reason(
            &providers,
            &"openai/gpt-q".parse().unwrap()
        )
        .as_deref(),
        Some("quarantined: invalid_catalog_model_record")
    );
    assert_eq!(
        cookie_agent::availability::model_unavailable_reason(
            &providers,
            &"openai/other".parse().unwrap()
        )
        .as_deref(),
        Some("provider `openai` has no usable models (0 usable, 1 quarantined, 1 unsupported)")
    );
    assert_eq!(
        cookie_agent::availability::unusable_configured_providers(&providers).as_deref(),
        Some(
            "configured providers without usable models: openai (0 usable, 1 quarantined, 1 unsupported)"
        )
    );
    assert!(
        cookie_agent::availability::model_unavailable_reason(
            &providers,
            &"elsewhere/model".parse().unwrap()
        )
        .is_none()
    );
}

#[test]
fn typed_credentials_become_protocol_credentials_and_the_source_is_wiped() {
    let before = SECRET_VALUES_WIPED.load(TestOrdering::SeqCst);
    let credentials = SecretValues(BTreeMap::from([(
        "api_key".into(),
        "invented-test-placeholder".into(),
    )]))
    .into_credentials()
    .expect("credentials");
    assert_eq!(
        credentials.get(&cookie_agent_protocol::AuthFieldName::new("api_key").unwrap()),
        Some("invented-test-placeholder")
    );
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
    // The catalog's one provider and the built-in ChatGPT sign-in.
    assert_eq!(snapshot.snapshot.providers.len(), 2);
    assert!(
        snapshot
            .snapshot
            .providers
            .iter()
            .any(|provider| provider.id.as_str() == "chatgpt"
                && !provider.sign_in_methods.is_empty())
    );
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
        "expected_revision": snapshot.revision,
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
        expected_revision: connected.runtime.revision,
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

/// A catalog refresh between reading the snapshot and connecting is retried
/// against the new revision, unless the provider's form changed with it.
#[tokio::test]
async fn connect_retries_a_catalog_conflict_when_the_provider_form_is_unchanged() {
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
    let stale = runtime.engine.runtime_snapshot().unwrap().snapshot;
    let provider = stale
        .providers
        .iter()
        .find(|provider| provider.id.as_str() == "openai")
        .unwrap()
        .clone();
    // The same provider under a new catalog revision: a model was renamed.
    let renamed = String::from_utf8(openai_catalog().to_vec())
        .unwrap()
        .replace(r#""name":"GPT Test""#, r#""name":"GPT Renamed""#);
    let refreshed = CatalogManager::in_directory(
        OfflineTransport {
            fetches: Arc::new(AtomicUsize::new(0)),
            body: Arc::new(renamed.into_bytes()),
        },
        &cache_anchor,
        "refreshed-catalog",
    )
    .refresh()
    .await
    .unwrap();
    runtime.engine.refresh_catalog(Arc::new(refreshed)).unwrap();
    assert_ne!(
        runtime.engine.runtime_snapshot().unwrap().snapshot.revision,
        stale.revision
    );

    let (client_stream, server_stream) = cookie_agent_protocol::in_process_pair(32);
    let server_task = tokio::spawn(runtime.server.clone().serve_stream(
        server_stream,
        cookie_agent_protocol::Authentication::Authenticated,
    ));
    let client = Client::connect_stream(client_stream);
    client.handshake().await.unwrap();
    let params = || cookie_agent_protocol::ProviderConnectParams {
        provider_id: provider.id.clone(),
        expected_revision: stale.revision.clone(),
        setup_values: BTreeMap::new(),
        auth_method: cookie_agent_protocol::AuthMethodId::new("bearer-api-key-v1").unwrap(),
        auth_values: serde_json::from_value(serde_json::json!({"api_key": "placeholder"})).unwrap(),
        client_connect_id: ClientConnectId::new(Uuid::now_v7().to_string()).unwrap(),
    };
    // A form that no longer matches the provider is not retried.
    let mut changed = provider.clone();
    changed.auth_methods.clear();
    let error = client
        .connect_provider_retrying(params(), &changed)
        .await
        .expect_err("a changed form is not retried");
    assert_eq!(error.error_code(), Some("catalog_revision_conflict"));
    let connected = client
        .connect_provider_retrying(params(), &provider)
        .await
        .expect("retried against the new catalog");
    assert_eq!(connected.durable_connection.provider_id, provider.id);
    drop(client);
    server_task.await.unwrap().unwrap();
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
                "expected_revision": initial.revision,
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
    let (client_stream, server_stream) = cookie_agent_protocol::in_process_pair(32);
    let server_task = tokio::spawn(runtime.server.clone().serve_stream(
        server_stream,
        cookie_agent_protocol::Authentication::Authenticated,
    ));
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
        let (client_stream, server_stream) = cookie_agent_protocol::in_process_pair(32);
        let server_task = tokio::spawn(runtime.server.clone().serve_stream(
            server_stream,
            cookie_agent_protocol::Authentication::Authenticated,
        ));
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
    catalog_refresh_settled().await;
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
    catalog_refresh_settled().await;

    tokio::time::advance(CATALOG_REFRESH_INTERVAL).await;
    tokio::task::yield_now().await;
    let changed = changes.recv().await.unwrap();
    assert_eq!(
        changed.reasons,
        vec![cookie_agent_protocol::RuntimeChangeReason::CatalogRefreshed]
    );
    catalog_refresh_settled().await;

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

#[test]
fn a_tui_offers_its_machine_under_a_sanitized_host_name() {
    for (host, expected) in [
        ("laptop", "client_laptop"),
        ("My-MacBook.local", "client_my_macbook_local"),
        ("--edge__", "client_edge"),
        ("", "client"),
        (
            "a-very-long-host-name-that-goes-on-and-on",
            "client_a_very_long_host_name_tha",
        ),
    ] {
        let id = client_runtime_id(host);
        assert_eq!(id, expected, "{host}");
        cookie_agent_protocol::RuntimeId::new(id.as_str()).expect("a valid runtime ID");
    }
}

#[test]
fn the_daemon_keeps_a_fresh_token_in_its_config_and_reuses_a_configured_one() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    std::fs::write(&path, "# mine\n").unwrap();
    let created = daemon_token(None, Some(&path)).expect("create a token");
    let reloaded = cookie_agent_config::load_from_user_root(Some(directory.path()))
        .expect("reload the configuration");
    assert_eq!(
        reloaded.runtime.server.token.as_deref(),
        Some(created.as_str())
    );
    assert!(std::fs::read_to_string(&path).unwrap().contains("# mine"));
    let reused = daemon_token(Some(TEST_TOKEN.into()), Some(&path)).unwrap();
    assert_eq!(reused.as_str(), TEST_TOKEN);
    assert!(daemon_token(None, None).is_err());
}

#[test]
fn a_dial_in_runtime_defaults_to_its_host_name() {
    for (host, expected) in [
        ("labpc", "labpc"),
        ("LabPC.local", "labpc_local"),
        ("1box", "host_1box"),
        ("", "host"),
    ] {
        let id = host_runtime_id("", host);
        assert_eq!(id, expected, "{host}");
        cookie_agent_protocol::RuntimeId::new(id.as_str()).expect("a valid runtime ID");
    }
}

struct ScriptedConfigIo {
    edits: VecDeque<String>,
    confirmations: VecDeque<bool>,
    seen: Vec<String>,
    output: Vec<String>,
}

impl config_command::ConfigIo for ScriptedConfigIo {
    fn write_line(&mut self, line: &str) -> anyhow::Result<()> {
        self.output.push(line.to_owned());
        Ok(())
    }

    fn edit(&mut self, _path: &str, text: &str) -> anyhow::Result<String> {
        self.seen.push(text.to_owned());
        self.edits.pop_front().context("missing scripted edit")
    }

    fn confirm(&mut self, _prompt: &str) -> anyhow::Result<bool> {
        self.confirmations
            .pop_front()
            .context("missing scripted confirmation")
    }
}

/// `cookie config` edits, sets, removes, and reloads the configuration of
/// a running daemon; an edit that does not validate is offered again with
/// the text as it was left.
#[tokio::test]
async fn cookie_config_edits_and_sets_the_daemon_configuration() {
    use config_command::{ConfigCommand, run_config_with};
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
    let (client_stream, server_stream) = cookie_agent_protocol::in_process_pair(32);
    let server_task = tokio::spawn(runtime.server.clone().serve_stream(
        server_stream,
        cookie_agent_protocol::Authentication::Authenticated,
    ));
    let client = Client::connect_stream(client_stream);
    client.handshake().await.unwrap();
    let mut io = ScriptedConfigIo {
        edits: VecDeque::from([
            "[providers]\n[unknown]\n".to_owned(),
            "[providers]\n\n[loop_warning]\nenabled = false\n".to_owned(),
        ]),
        confirmations: VecDeque::from([true]),
        seen: Vec::new(),
        output: Vec::new(),
    };
    run_config_with(&client, ConfigCommand::Edit { path: None }, &mut io)
        .await
        .expect("edit");
    // The second round starts from the rejected text.
    assert_eq!(io.seen, ["[providers]\n", "[providers]\n[unknown]\n"]);
    assert!(io.output.iter().any(|line| line.starts_with("Not applied")));
    assert!(!runtime.engine.configuration().runtime.loop_warning.enabled);

    run_config_with(
        &client,
        ConfigCommand::Set {
            key: "model_retry.standard_retries".into(),
            value: "1".into(),
        },
        &mut io,
    )
    .await
    .expect("set");
    assert_eq!(
        runtime
            .engine
            .configuration()
            .runtime
            .model_retry
            .standard_retries,
        1
    );
    run_config_with(
        &client,
        ConfigCommand::Unset {
            key: "loop_warning".into(),
        },
        &mut io,
    )
    .await
    .expect("unset");
    assert!(runtime.engine.configuration().runtime.loop_warning.enabled);
    assert!(
        run_config_with(
            &client,
            ConfigCommand::Set {
                key: "loop_warning.enabled".into(),
                value: "\"not a boolean\"".into(),
            },
            &mut io,
        )
        .await
        .is_err()
    );

    // A new agent document starts from a template and is removed again.
    let agent = "---\ndescription: Reviewer\nmode: subagent\nenabled: true\nmodels: [{ model: \"openai/gpt-test\" }]\npermissions: {}\n---\nReview.\n";
    io.edits.push_back(agent.to_owned());
    run_config_with(
        &client,
        ConfigCommand::Edit {
            path: Some("agents/reviewer.md".into()),
        },
        &mut io,
    )
    .await
    .expect("create an agent");
    assert!(io.seen.last().unwrap().starts_with("---\ndescription:"));
    assert!(workspace.join(".cookie-agent/agents/reviewer.md").exists());
    run_config_with(
        &client,
        ConfigCommand::Remove {
            path: "agents/reviewer.md".into(),
        },
        &mut io,
    )
    .await
    .expect("remove");
    assert!(!workspace.join(".cookie-agent/agents/reviewer.md").exists());

    // A delegation target no runtime has reported yet applies, with a note.
    let lead = "---\ndescription: Lead\nmode: primary\nenabled: true\nmodels: [{ model: \"openai/gpt-test\" }]\npermissions:\n  delegate: { codex: allow }\n---\nLead.\n";
    io.edits.push_back(lead.to_owned());
    io.output.clear();
    run_config_with(
        &client,
        ConfigCommand::Edit {
            path: Some("agents/lead.md".into()),
        },
        &mut io,
    )
    .await
    .expect("an unknown delegation target applies");
    assert!(
        io.output.iter().any(|line| line
            .starts_with("Note: agent `lead` delegates to `codex`, which is not known yet")),
        "{:?}",
        io.output
    );
    run_config_with(
        &client,
        ConfigCommand::Remove {
            path: "agents/lead.md".into(),
        },
        &mut io,
    )
    .await
    .expect("remove");

    // A hand edit shows in the status until it is reloaded.
    std::fs::write(
        workspace.join(".cookie-agent/config.toml"),
        "[providers]\n[server]\nport = 7500\n",
    )
    .unwrap();
    io.output.clear();
    run_config_with(&client, ConfigCommand::Status, &mut io)
        .await
        .expect("status");
    assert!(
        io.output
            .iter()
            .any(|line| line.contains("cookie config reload")),
        "{:?}",
        io.output
    );
    io.output.clear();
    run_config_with(&client, ConfigCommand::Reload, &mut io)
        .await
        .expect("reload");
    assert!(
        io.output
            .iter()
            .any(|line| line == "Restart the daemon to apply: server."),
        "{:?}",
        io.output
    );
    drop(client);
    server_task.await.unwrap().unwrap();
    runtime.server.shutdown();
    runtime.stop_catalog_refresh().await;
    runtime.engine.shutdown().await;
}

#[test]
fn machine_commands_take_valid_names_and_runtime_ids() {
    use machine_command::MachineCommand;
    let name = |value: &str| cookie_agent_protocol::MachineName::new(value).unwrap();
    assert_eq!(
        Cli::try_parse_from(["cookie", "machine", "rename", "laptop", "desk-2"])
            .unwrap()
            .command,
        Some(Command::Machine {
            command: MachineCommand::Rename {
                machine: name("laptop"),
                name: name("desk-2"),
            },
            url: None,
            token: None,
        })
    );
    assert_eq!(
        Cli::try_parse_from(["cookie", "machine", "assign", "box", "labpc"])
            .unwrap()
            .command,
        Some(Command::Machine {
            command: MachineCommand::Assign {
                runtime: cookie_agent_protocol::RuntimeId::new("box").unwrap(),
                machine: name("labpc"),
            },
            url: None,
            token: None,
        })
    );
    for arguments in [
        &["cookie", "machine", "rename", "laptop", "Desk"][..],
        &["cookie", "machine", "assign", "my-box", "labpc"][..],
        &["cookie", "machine", "forget"][..],
        &["cookie", "machine", "merge", "a"][..],
    ] {
        assert!(Cli::try_parse_from(arguments).is_err(), "{arguments:?}");
    }
}

/// `cookie machine` lists the daemon's machines and regroups them through
/// the `machines.*` methods; a refusal is the daemon's error.
#[tokio::test]
async fn cookie_machine_lists_and_regroups_the_daemon_machines() {
    use machine_command::{MachineCommand, run_machine_with};
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
    let (client_stream, server_stream) = cookie_agent_protocol::in_process_pair(32);
    let server_task = tokio::spawn(runtime.server.clone().serve_stream(
        server_stream,
        cookie_agent_protocol::Authentication::Authenticated,
    ));
    let client = Client::connect_stream(client_stream);
    client.handshake().await.unwrap();
    let name = |value: &str| cookie_agent_protocol::MachineName::new(value).unwrap();
    let daemon = runtime.engine.runtime_machine("local").expect("machine");

    let listed = run_machine_with(&client, MachineCommand::List)
        .await
        .unwrap();
    assert!(
        listed[0].starts_with(&format!("{daemon} ({})", std::env::consts::OS)),
        "{listed:?}"
    );
    assert!(listed[0].ends_with(", 0 memories"), "{listed:?}");
    assert_eq!(
        listed[1],
        "  connected: global (by machine ID), local (by machine ID)"
    );

    assert_eq!(
        run_machine_with(
            &client,
            MachineCommand::Rename {
                machine: daemon.clone(),
                name: name("workstation"),
            },
        )
        .await
        .unwrap(),
        [format!("Renamed machine {daemon} to workstation.")]
    );
    run_machine_with(
        &client,
        MachineCommand::Assign {
            runtime: cookie_agent_protocol::RuntimeId::local(),
            machine: name("sandbox"),
        },
    )
    .await
    .unwrap();
    let listed = run_machine_with(&client, MachineCommand::List)
        .await
        .unwrap();
    assert_eq!(
        listed
            .iter()
            .filter(|line| !line.starts_with(' '))
            .map(|line| line.split([' ', ',']).next().unwrap())
            .collect::<Vec<_>>(),
        ["sandbox", "workstation"],
        "{listed:?}"
    );
    assert!(listed.contains(&"  connected: local (pinned)".to_owned()));
    assert!(listed.contains(&"  pinned: local".to_owned()));

    let refused = run_machine_with(
        &client,
        MachineCommand::Forget {
            machine: name("sandbox"),
        },
    )
    .await
    .unwrap_err();
    assert!(
        refused
            .chain()
            .filter_map(|cause| cause.downcast_ref::<cookie_agent_protocol::ClientError>())
            .any(|error| error.is(&cookie_agent_protocol::errors::MACHINE_IN_USE)),
        "{refused:#}"
    );
    run_machine_with(
        &client,
        MachineCommand::Merge {
            from: name("sandbox"),
            into: name("workstation"),
        },
    )
    .await
    .unwrap();
    run_machine_with(
        &client,
        MachineCommand::Unassign {
            runtime: cookie_agent_protocol::RuntimeId::local(),
        },
    )
    .await
    .unwrap();
    let listed = run_machine_with(&client, MachineCommand::List)
        .await
        .unwrap();
    assert!(listed[0].starts_with("workstation"), "{listed:?}");
    assert_eq!(
        listed.iter().filter(|line| !line.starts_with(' ')).count(),
        1
    );

    drop(client);
    server_task.await.unwrap().unwrap();
    runtime.server.shutdown();
    runtime.stop_catalog_refresh().await;
    runtime.engine.shutdown().await;
}

#[test]
fn memory_commands_take_a_layer_a_url_and_a_confirmation_switch() {
    use cookie_agent_protocol::MemoryLayer;
    use memory_command::MemoryCommand;
    let parsed = |arguments: &[&str]| match Cli::try_parse_from(arguments).unwrap().command {
        Some(Command::Memory { command, .. }) => command,
        other => panic!("not a memory command: {other:?}"),
    };
    assert_eq!(
        parsed(&["cookie", "memory", "list"]),
        MemoryCommand::List { layer: None }
    );
    assert_eq!(
        parsed(&["cookie", "memory", "list", "project"]),
        MemoryCommand::List {
            layer: Some(MemoryLayer::Project)
        }
    );
    assert_eq!(
        parsed(&["cookie", "memory", "show", "project://web/build"]),
        MemoryCommand::Show {
            url: "project://web/build".into()
        }
    );
    assert_eq!(
        parsed(&["cookie", "memory", "edit", "project://web/build"]),
        MemoryCommand::Edit {
            url: "project://web/build".into()
        }
    );
    assert_eq!(
        parsed(&["cookie", "memory", "rm", "user://hardware", "--yes"]),
        MemoryCommand::Rm {
            url: "user://hardware".into(),
            yes: true
        }
    );
    assert_eq!(
        parsed(&["cookie", "memory", "rm", "user://hardware"]),
        MemoryCommand::Rm {
            url: "user://hardware".into(),
            yes: false
        }
    );
    for arguments in [
        &["cookie", "memory", "list", "projects"][..],
        &["cookie", "memory", "edit"][..],
        &["cookie", "memory", "show"][..],
        &["cookie", "memory", "rm"][..],
    ] {
        assert!(Cli::try_parse_from(arguments).is_err(), "{arguments:?}");
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

impl memory_command::MemoryIo for ScriptedMemoryIo {
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

/// `cookie memory` lists, creates, edits, and deletes a running daemon's
/// memories through `memory.*`: invalid text is offered again as it was
/// left, a memory changed meanwhile is reloaded, and `rm` asks first.
#[tokio::test]
async fn cookie_memory_lists_edits_and_deletes_the_daemon_memories() {
    use memory_command::{MemoryCommand, NEW_MEMORY, run_memory_with};
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
    let memory = temporary.path().join("data/memory");
    let (client_stream, server_stream) = cookie_agent_protocol::in_process_pair(32);
    let server_task = tokio::spawn(runtime.server.clone().serve_stream(
        server_stream,
        cookie_agent_protocol::Authentication::Authenticated,
    ));
    let client = Client::connect_stream(client_stream);
    client.handshake().await.unwrap();
    // A project memory needs its project.
    client
        .create_project(cookie_agent_protocol::ProjectCreateParams {
            name: cookie_agent_protocol::ProjectName::new("web").unwrap(),
            emoji: None,
            defaults: None,
        })
        .await
        .unwrap();
    let url = "project://web/build";
    let valid = "---\ndescription: Build conventions\n---\nRun the gates.\n";

    // A new memory starts from the template; invalid text is offered again
    // as it was left.
    let mut io = ScriptedMemoryIo::new(&["---\ndescription: \n---\n", valid], &[true]);
    run_memory_with(&client, MemoryCommand::Edit { url: url.into() }, &mut io)
        .await
        .expect("create");
    assert_eq!(io.seen, [NEW_MEMORY, "---\ndescription: \n---\n"]);
    assert!(
        io.output[0].starts_with("Not saved: The text is not a valid memory"),
        "{:?}",
        io.output
    );
    assert_eq!(io.output.last().unwrap(), "Created project://web/build.");
    assert_eq!(
        unstamped(&std::fs::read_to_string(memory.join("projects/web/build.md")).unwrap()),
        valid
    );

    // Leaving a new memory's template as it is creates nothing.
    let mut io = ScriptedMemoryIo::new(&[NEW_MEMORY], &[]);
    run_memory_with(
        &client,
        MemoryCommand::Edit {
            url: "project://web/empty".into(),
        },
        &mut io,
    )
    .await
    .unwrap();
    assert_eq!(
        io.output,
        ["No changes; project://web/empty was not created."]
    );

    // A memory changed while it was open is reloaded and edited again.
    let changed = "---\ndescription: Build conventions\n---\nRun every gate.\n";
    let mine = "---\ndescription: Build conventions\n---\nMine.\n";
    let mut io = ScriptedMemoryIo::new(&[mine, mine], &[true]);
    io.meanwhile = Some((memory.join("projects/web/build.md"), changed.to_owned()));
    run_memory_with(&client, MemoryCommand::Edit { url: url.into() }, &mut io)
        .await
        .expect("edit");
    assert_eq!(
        io.seen
            .iter()
            .map(|text| unstamped(text))
            .collect::<Vec<_>>(),
        [valid, changed]
    );
    assert_eq!(
        io.output,
        [
            "Not saved: project://web/build changed since it was opened.",
            "Saved project://web/build."
        ]
    );
    assert_eq!(
        unstamped(&std::fs::read_to_string(memory.join("projects/web/build.md")).unwrap()),
        mine
    );

    // The listing groups by layer and project, gives each memory's stamps
    // and its size against its limit, and names what is not a memory.
    std::fs::write(memory.join("projects/web/notes.txt"), "stray").unwrap();
    // Sizes count the whole file, the daemon's stamps included.
    let size = |path: &str| std::fs::metadata(memory.join(path)).unwrap().len();
    let mut io = ScriptedMemoryIo::new(&[], &[]);
    run_memory_with(
        &client,
        MemoryCommand::List {
            layer: Some(cookie_agent_protocol::MemoryLayer::Project),
        },
        &mut io,
    )
    .await
    .unwrap();
    // The limit is the configuration's; only its place is checked.
    let stamps = |line: &str, path: &str| {
        line.starts_with(&format!(
            "      created less than a minute ago · modified less than a minute ago · {} bytes of ",
            size(path)
        )) && line.ends_with(" KB")
    };
    assert_eq!(
        io.output[..3],
        [
            "Projects",
            "  web",
            "    project://web/build  Build conventions"
        ]
    );
    assert!(
        stamps(&io.output[3], "projects/web/build.md"),
        "{:?}",
        io.output
    );
    assert_eq!(
        io.output[4],
        "    project://web/overview  Overview of the `web` project, loaded in full into its sessions"
    );
    assert!(
        stamps(&io.output[5], "projects/web/overview.md"),
        "{:?}",
        io.output
    );
    assert_eq!(
        io.output[6..],
        [
            "Not memories (ignored)",
            "  projects/web/notes.txt: not a `.md` file"
        ]
    );

    // `show` gives the stamps with their exact times, the size against the
    // limit, and the text without the stamp lines.
    let mut io = ScriptedMemoryIo::new(&[], &[]);
    run_memory_with(&client, MemoryCommand::Show { url: url.into() }, &mut io)
        .await
        .unwrap();
    assert_eq!(io.output[0], url);
    assert!(
        io.output[1].starts_with("created: ")
            && io.output[1].ends_with(" UTC (less than a minute ago)"),
        "{:?}",
        io.output
    );
    assert!(io.output[2].starts_with("modified: "), "{:?}", io.output);
    assert!(
        io.output[3].starts_with(&format!(
            "size: {} bytes of ",
            size("projects/web/build.md")
        )),
        "{:?}",
        io.output
    );
    assert!(io.output[4].starts_with("revision: "), "{:?}", io.output);
    assert_eq!(io.output[5..], ["", mine.trim_end()]);
    let mut io = ScriptedMemoryIo::new(&[], &[]);
    run_memory_with(&client, MemoryCommand::List { layer: None }, &mut io)
        .await
        .unwrap();
    assert_eq!(io.output[0], "User");
    assert!(io.output[1].starts_with("  user://feedback  Corrections"));

    // `rm` asks first; declining keeps the memory.
    let mut io = ScriptedMemoryIo::new(&[], &[false]);
    run_memory_with(
        &client,
        MemoryCommand::Rm {
            url: url.into(),
            yes: false,
        },
        &mut io,
    )
    .await
    .unwrap();
    assert_eq!(io.output, ["project://web/build was not deleted."]);
    assert!(memory.join("projects/web/build.md").exists());
    let mut io = ScriptedMemoryIo::new(&[], &[]);
    run_memory_with(
        &client,
        MemoryCommand::Rm {
            url: url.into(),
            yes: true,
        },
        &mut io,
    )
    .await
    .unwrap();
    assert_eq!(io.output, ["Deleted project://web/build."]);
    assert!(!memory.join("projects/web/build.md").exists());

    // A refusal is the daemon's error.
    let refused = run_memory_with(
        &client,
        MemoryCommand::Rm {
            url: url.into(),
            yes: true,
        },
        &mut ScriptedMemoryIo::new(&[], &[]),
    )
    .await
    .unwrap_err();
    assert!(
        refused
            .chain()
            .filter_map(|cause| cause.downcast_ref::<cookie_agent_protocol::ClientError>())
            .any(|error| error.is(&cookie_agent_protocol::errors::MEMORY_NOT_FOUND)),
        "{refused:#}"
    );
    // It reads as the message and its hint, the hint once.
    let shown = format!("{refused:#}");
    let hint = cookie_agent_protocol::errors::MEMORY_NOT_FOUND
        .hint
        .expect("memory_not_found has a hint");
    assert_eq!(shown.matches(hint).count(), 1, "{shown}");
    assert!(shown.starts_with("There is no memory at "), "{shown}");

    drop(client);
    server_task.await.unwrap().unwrap();
    runtime.server.shutdown();
    runtime.stop_catalog_refresh().await;
    runtime.engine.shutdown().await;
}

#[test]
fn project_commands_take_names_a_session_and_a_confirmation_switch() {
    use project_command::ProjectCommand;
    let name = |value: &str| cookie_agent_protocol::ProjectName::new(value).unwrap();
    let parsed = |arguments: &[&str]| match Cli::try_parse_from(arguments).unwrap().command {
        Some(Command::Project { command, .. }) => command,
        other => panic!("not a project command: {other:?}"),
    };
    assert_eq!(parsed(&["cookie", "project", "list"]), ProjectCommand::List);
    assert_eq!(
        parsed(&["cookie", "project", "create", "web", "--emoji", "🍪"]),
        ProjectCommand::Create {
            name: name("web"),
            emoji: Some("🍪".into())
        }
    );
    assert_eq!(
        parsed(&["cookie", "project", "rename", "web", "shop"]),
        ProjectCommand::Rename {
            old: name("web"),
            new: name("shop")
        }
    );
    assert_eq!(
        parsed(&["cookie", "project", "delete", "web", "--yes"]),
        ProjectCommand::Delete {
            name: name("web"),
            yes: true
        }
    );
    assert_eq!(
        parsed(&["cookie", "project", "edit", "web"]),
        ProjectCommand::Edit { name: name("web") }
    );
    let session = cookie_agent_protocol::SessionId::new_v7();
    assert_eq!(
        parsed(&[
            "cookie",
            "project",
            "defaults",
            "web",
            "--from-session",
            &session.to_string()
        ]),
        ProjectCommand::Defaults {
            name: name("web"),
            from_session: session
        }
    );
    for arguments in [
        &["cookie", "project", "create", "Web"][..],
        &["cookie", "project", "defaults", "web"][..],
        &["cookie", "project", "rename", "web"][..],
    ] {
        assert!(Cli::try_parse_from(arguments).is_err(), "{arguments:?}");
    }
    // `--project` names a project for plain `cookie`.
    let cli = Cli::try_parse_from(["cookie", "--project", "web"]).unwrap();
    assert_eq!(cli.project, Some(name("web")));
    assert!(cli.command.is_none());
    assert!(Cli::try_parse_from(["cookie", "--project", "Not Valid"]).is_err());
}

/// `cookie project` creates, lists, renames, edits, sets the defaults of,
/// and deletes a running daemon's projects through `project.*`.
#[tokio::test]
async fn cookie_project_manages_the_daemon_projects() {
    use project_command::{ProjectCommand, run_project_with};
    let temporary = tempfile::tempdir().unwrap();
    private_directory(temporary.path());
    let workspace = temporary.path().join("workspace");
    let cache_anchor = temporary.path().join("cache-anchor");
    write_session_config(&workspace);
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
    let (client_stream, server_stream) = cookie_agent_protocol::in_process_pair(32);
    let server_task = tokio::spawn(runtime.server.clone().serve_stream(
        server_stream,
        cookie_agent_protocol::Authentication::Authenticated,
    ));
    let client = Client::connect_stream(client_stream);
    client.handshake().await.unwrap();
    let name = |value: &str| cookie_agent_protocol::ProjectName::new(value).unwrap();

    let mut io = ScriptedMemoryIo::new(&[], &[]);
    run_project_with(&client, ProjectCommand::List, &mut io)
        .await
        .unwrap();
    run_project_with(
        &client,
        ProjectCommand::Create {
            name: name("web"),
            emoji: Some("🍪".into()),
        },
        &mut io,
    )
    .await
    .unwrap();
    run_project_with(&client, ProjectCommand::List, &mut io)
        .await
        .unwrap();
    assert_eq!(
        io.output,
        [
            "No projects.",
            "Created project web.",
            "🍪 web  0 sessions, 1 memory"
        ]
    );

    // Edit writes `AGENTS.md`; an emptied file removes it.
    let mut io = ScriptedMemoryIo::new(&["Use tabs.\n"], &[]);
    run_project_with(&client, ProjectCommand::Edit { name: name("web") }, &mut io)
        .await
        .unwrap();
    assert_eq!(io.seen, [""]);
    assert_eq!(io.output, ["Saved the AGENTS.md of project web."]);
    let mut io = ScriptedMemoryIo::new(&[""], &[]);
    run_project_with(&client, ProjectCommand::Edit { name: name("web") }, &mut io)
        .await
        .unwrap();
    assert_eq!(io.seen, ["Use tabs.\n"]);
    assert_eq!(io.output, ["Removed the AGENTS.md of project web."]);

    // Defaults come from a session's attachments and agent.
    let selection = cookie_agent_protocol::RunSelection {
        agent: cookie_agent_protocol::AgentId::new("primary").unwrap(),
        model: cookie_agent_protocol::ModelSelection {
            model: "custom.local/test".parse().unwrap(),
            variant: None,
        },
        preset: None,
    };
    let session = client
        .create_session(cookie_agent_protocol::SessionCreateParams {
            runtimes: Some(Vec::new()),
            ..cookie_agent_protocol::SessionCreateParams::new(selection)
        })
        .await
        .unwrap()
        .session
        .session_id;
    let mut io = ScriptedMemoryIo::new(&[], &[]);
    run_project_with(
        &client,
        ProjectCommand::Defaults {
            name: name("web"),
            from_session: session,
        },
        &mut io,
    )
    .await
    .unwrap();
    assert_eq!(
        io.output,
        [
            "Set the defaults of project web.",
            "🍪 web  0 sessions, 1 memory  [agent primary]"
        ]
    );

    run_project_with(
        &client,
        ProjectCommand::Rename {
            old: name("web"),
            new: name("shop"),
        },
        &mut io,
    )
    .await
    .unwrap();
    assert!(
        io.output
            .last()
            .unwrap()
            .starts_with("Renamed project web to shop"),
        "{:?}",
        io.output
    );
    // Delete asks first.
    let mut io = ScriptedMemoryIo::new(&[], &[false]);
    run_project_with(
        &client,
        ProjectCommand::Delete {
            name: name("shop"),
            yes: false,
        },
        &mut io,
    )
    .await
    .unwrap();
    assert_eq!(io.output, ["Project shop was not deleted."]);
    run_project_with(
        &client,
        ProjectCommand::Delete {
            name: name("shop"),
            yes: true,
        },
        &mut io,
    )
    .await
    .unwrap();
    assert_eq!(
        io.output.last().unwrap(),
        "Deleted project shop, 0 sessions and 1 memory."
    );
    let missing = run_project_with(
        &client,
        ProjectCommand::Edit { name: name("shop") },
        &mut io,
    )
    .await
    .unwrap_err();
    assert!(format!("{missing:#}").contains("there is no project named shop"));

    drop(client);
    server_task.await.unwrap().unwrap();
    runtime.server.shutdown();
    runtime.stop_catalog_refresh().await;
    runtime.engine.shutdown().await;
}

#[test]
fn cron_commands_take_ids_targets_and_options() {
    use cron_command::{CronCommand, parse_job_id};
    let parsed = |arguments: &[&str]| match Cli::try_parse_from(arguments).unwrap().command {
        Some(Command::Cron { command, .. }) => command,
        other => panic!("not a cron command: {other:?}"),
    };
    let id = parse_job_id("k3v9q2xa").unwrap();
    assert_eq!(parsed(&["cookie", "cron", "list"]), CronCommand::List);
    assert_eq!(
        parsed(&["cookie", "cron", "show", "k3v9q2xa"]),
        CronCommand::Show { id: id.clone() }
    );
    assert_eq!(
        parsed(&["cookie", "cron", "rm", "k3v9q2xa", "-y"]),
        CronCommand::Rm {
            id: id.clone(),
            yes: true
        }
    );
    assert_eq!(
        parsed(&["cookie", "cron", "run", "k3v9q2xa", "--url", "ws://h:1/ws"]),
        CronCommand::Run { id: id.clone() }
    );
    assert!(matches!(
        parsed(&[
            "cookie",
            "cron",
            "create",
            "--prompt",
            "p",
            "--schedule",
            "in 5m",
            "--agent",
            "primary",
            "--permission-mode",
            "ask",
            "--model",
            "a/b"
        ]),
        CronCommand::Create(_)
    ));
    assert!(matches!(
        parsed(&["cookie", "cron", "edit", "k3v9q2xa", "--name", ""]),
        CronCommand::Edit { .. }
    ));
    for arguments in [
        // A job id is eight lowercase letters and digits.
        &["cookie", "cron", "show", "K3V9Q2XA"][..],
        // A target is required, and only one.
        &[
            "cookie",
            "cron",
            "create",
            "--prompt",
            "p",
            "--schedule",
            "in 5m",
        ][..],
        &[
            "cookie",
            "cron",
            "create",
            "--prompt",
            "p",
            "--schedule",
            "in 5m",
            "--agent",
            "a",
            "--session",
            "0190a5b2-0000-7000-8000-000000000001",
        ][..],
        // A new-session option needs a new-session job, delivery a session.
        &[
            "cookie",
            "cron",
            "create",
            "--prompt",
            "p",
            "--schedule",
            "in 5m",
            "--session",
            "0190a5b2-0000-7000-8000-000000000001",
            "--permission-mode",
            "ask",
        ][..],
        &[
            "cookie",
            "cron",
            "create",
            "--prompt",
            "p",
            "--schedule",
            "in 5m",
            "--agent",
            "a",
            "--delivery",
            "steer",
        ][..],
    ] {
        assert!(Cli::try_parse_from(arguments).is_err(), "{arguments:?}");
    }
}

/// `cookie cron` creates, lists, shows, edits, disables, enables, runs, and
/// deletes a running daemon's cron jobs through `crons.*`.
#[tokio::test]
async fn cookie_cron_manages_the_daemon_jobs() {
    use clap::Parser as _;
    use cron_command::{CronCommand, parse_job_id, run_cron_with};
    let temporary = tempfile::tempdir().unwrap();
    private_directory(temporary.path());
    let workspace = temporary.path().join("workspace");
    let cache_anchor = temporary.path().join("cache-anchor");
    write_session_config(&workspace);
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
    let (client_stream, server_stream) = cookie_agent_protocol::in_process_pair(32);
    let server_task = tokio::spawn(runtime.server.clone().serve_stream(
        server_stream,
        cookie_agent_protocol::Authentication::Authenticated,
    ));
    let client = Client::connect_stream(client_stream);
    client.handshake().await.unwrap();
    let command = |arguments: &[&str]| {
        let mut full = vec!["cookie", "cron"];
        full.extend_from_slice(arguments);
        match Cli::try_parse_from(full).unwrap().command {
            Some(Command::Cron { command, .. }) => command,
            other => panic!("not a cron command: {other:?}"),
        }
    };

    let mut io = ScriptedMemoryIo::new(&[], &[]);
    run_cron_with(&client, CronCommand::List, &mut io)
        .await
        .unwrap();
    assert_eq!(io.output, ["No cron jobs."]);

    // A new-session job with a mode only a client sets.
    let mut io = ScriptedMemoryIo::new(&[], &[]);
    run_cron_with(
        &client,
        command(&[
            "create",
            "--name",
            "Nightly",
            "--prompt",
            "Check the dependencies.",
            "--schedule",
            "0 3 * * *",
            "--max-runs",
            "-1",
            "--agent",
            "primary",
            "--permission-mode",
            "ask",
        ]),
        &mut io,
    )
    .await
    .unwrap();
    let created = io.output[0].clone();
    assert!(
        created.starts_with("Created job ") && created.ends_with(" (Nightly)."),
        "{created}"
    );
    assert!(io.output[1].starts_with("Next fire: "), "{:?}", io.output);
    let jobs = client.list_crons().await.unwrap().jobs;
    assert_eq!(jobs.len(), 1);
    let id = jobs[0].id.clone();
    assert_eq!(
        jobs[0].permission_mode,
        Some(cookie_agent_protocol::PermissionMode::Ask)
    );

    let mut io = ScriptedMemoryIo::new(&[], &[]);
    run_cron_with(&client, CronCommand::List, &mut io)
        .await
        .unwrap();
    assert_eq!(io.output[0], format!("{id}  Nightly"));
    assert!(
        io.output[1].starts_with("    every day at 03:00 · next ")
            && io.output[1].ends_with(" · new session · primary · last never"),
        "{:?}",
        io.output
    );

    // An in-session job, edited: only what changed is sent.
    let selection = cookie_agent_protocol::RunSelection {
        agent: cookie_agent_protocol::AgentId::new("primary").unwrap(),
        model: cookie_agent_protocol::ModelSelection {
            model: "custom.local/test".parse().unwrap(),
            variant: None,
        },
        preset: None,
    };
    let session = client
        .create_session(cookie_agent_protocol::SessionCreateParams {
            runtimes: Some(Vec::new()),
            ..cookie_agent_protocol::SessionCreateParams::new(selection)
        })
        .await
        .unwrap()
        .session
        .session_id;
    let session_text = session.to_string();
    let mut io = ScriptedMemoryIo::new(&[], &[]);
    run_cron_with(
        &client,
        command(&[
            "create",
            "--prompt",
            "Summarize the build log.",
            "--schedule",
            "in 2h",
            "--session",
            &session_text,
            "--delivery",
            "steer",
        ]),
        &mut io,
    )
    .await
    .unwrap();
    let jobs = client.list_crons().await.unwrap().jobs;
    let second = jobs[1].id.clone();
    assert_eq!(
        jobs[1].target,
        cookie_agent_protocol::CronTarget::Session(session)
    );
    assert_eq!(
        jobs[1].delivery,
        Some(cookie_agent_protocol::ProducerDeliveryMode::Steer)
    );
    let second_text = second.to_string();
    let mut io = ScriptedMemoryIo::new(&[], &[]);
    run_cron_with(
        &client,
        command(&[
            "edit",
            &second_text,
            "--name",
            "Build log",
            "--delivery",
            "queue",
        ]),
        &mut io,
    )
    .await
    .unwrap();
    assert_eq!(io.output[0], format!("Updated job {second} (Build log)."));
    let mut io = ScriptedMemoryIo::new(&[], &[]);
    run_cron_with(&client, command(&["edit", &second_text]), &mut io)
        .await
        .unwrap();
    assert_eq!(io.output, [format!("Job {second} is unchanged.")]);
    let refused = run_cron_with(
        &client,
        command(&["edit", &second_text, "--permission-mode", "yolo"]),
        &mut io,
    )
    .await
    .unwrap_err();
    assert!(
        format!("{refused:#}").contains("--permission-mode is for a job that starts a new session"),
        "{refused:#}"
    );

    // Disable, show, enable.
    let id_text = id.to_string();
    let mut io = ScriptedMemoryIo::new(&[], &[]);
    run_cron_with(&client, command(&["disable", &id_text]), &mut io)
        .await
        .unwrap();
    assert_eq!(
        io.output,
        [
            format!("Disabled job {id} (Nightly)."),
            "No next fire: disabled by you.".to_owned()
        ]
    );
    let mut io = ScriptedMemoryIo::new(&[], &[]);
    run_cron_with(&client, command(&["show", &id_text]), &mut io)
        .await
        .unwrap();
    for expected in [
        "Nightly",
        "schedule: 0 3 * * *",
        "reads: every day at 03:00",
        "target: new session · primary",
        "state: disabled by you",
        "runs: 0, no limit",
        "mode: ask",
        "Check the dependencies.",
        "No fires yet.",
    ] {
        assert!(
            io.output.iter().any(|line| line == expected),
            "{expected}: {:?}",
            io.output
        );
    }
    let mut io = ScriptedMemoryIo::new(&[], &[]);
    run_cron_with(&client, command(&["enable", &id_text]), &mut io)
        .await
        .unwrap();
    assert_eq!(io.output[0], format!("Enabled job {id} (Nightly)."));
    assert!(io.output[1].starts_with("Next fire: "), "{:?}", io.output);

    // Run now records a run now in the history.
    let mut io = ScriptedMemoryIo::new(&[], &[]);
    run_cron_with(&client, command(&["run", &second_text]), &mut io)
        .await
        .unwrap();
    assert!(
        io.output[0].starts_with(&format!("Ran job {second}: run now · ")),
        "{:?}",
        io.output
    );

    // Delete asks first.
    let mut io = ScriptedMemoryIo::new(&[], &[false]);
    run_cron_with(&client, command(&["rm", &id_text]), &mut io)
        .await
        .unwrap();
    assert_eq!(io.output, [format!("Job {id} was not deleted.")]);
    run_cron_with(&client, command(&["rm", &id_text, "--yes"]), &mut io)
        .await
        .unwrap();
    assert_eq!(
        io.output.last().unwrap(),
        &format!("Deleted job {id} (Nightly).")
    );
    let missing = run_cron_with(
        &client,
        CronCommand::Show {
            id: parse_job_id(&id_text).unwrap(),
        },
        &mut io,
    )
    .await
    .unwrap_err();
    assert!(format!("{missing:#}").contains(&format!("there is no cron job {id}")));

    drop(client);
    server_task.await.unwrap().unwrap();
    runtime.server.shutdown();
    runtime.stop_catalog_refresh().await;
    runtime.engine.shutdown().await;
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

#[tokio::test]
async fn plain_cookie_connects_to_the_daemon_server_describes_and_creates_its_session_there() {
    let temporary = tempfile::tempdir().unwrap();
    private_directory(temporary.path());
    let daemon = ListeningDaemon::start(temporary.path()).await;
    // `[server] url` names another daemon; the probe never reads it.
    let server = scratch_server(
        &temporary.path().join("home"),
        &format!(
            "port = {}\ntoken = \"{TEST_TOKEN}\"\nurl = \"ws://192.0.2.1:7419/ws\"\n",
            daemon.port()
        ),
    );
    let local_daemon::Probe::Connected(found) =
        local_daemon::probe(&server, probe_identity()).await
    else {
        panic!("the listening daemon was not found");
    };
    assert_eq!(found.url, format!("ws://127.0.0.1:{}/ws", daemon.port()));
    assert_eq!(found.token.as_str(), TEST_TOKEN);

    // The daemon runs on this machine: its `local` serves the directory.
    let choice = local_daemon::runtime_for(
        &found.client,
        &found.hello,
        paths::machine_id().as_deref(),
        "client_test",
    )
    .await;
    assert_eq!(choice, local_daemon::RuntimeChoice::Local);
    let created = found
        .client
        .create_session(cookie_agent_protocol::SessionCreateParams {
            cwd: Some(daemon.workspace.to_string_lossy().into_owned()),
            ..cookie_agent_protocol::SessionCreateParams::new(session_selection())
        })
        .await
        .unwrap();
    let listed = found
        .client
        .list_sessions(cookie_agent_protocol::SessionListParams {
            project: None,
            cwd_identity: None,
        })
        .await
        .unwrap();
    assert!(
        listed
            .sessions
            .iter()
            .any(|session| session.session_id == created.session.session_id),
        "{listed:?}"
    );
    drop(found);
    daemon.stop().await;
}

#[tokio::test]
async fn plain_cookie_stays_in_process_silently_without_a_token_or_a_listener() {
    let temporary = tempfile::tempdir().unwrap();
    private_directory(temporary.path());
    let daemon = ListeningDaemon::start(temporary.path()).await;
    // No token: nothing is dialed, even with a daemon at the port.
    let without_token = scratch_server(
        &temporary.path().join("without-token"),
        &format!("port = {}\n", daemon.port()),
    );
    let probe = local_daemon::probe(&without_token, probe_identity()).await;
    assert!(matches!(probe, local_daemon::Probe::NoToken));
    assert_eq!(probe.notice(), None);
    daemon.stop().await;

    // A port nothing listens on refuses the connection at once.
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = closed.local_addr().unwrap().port();
    drop(closed);
    let nothing = scratch_server(
        &temporary.path().join("nothing"),
        &format!("port = {port}\ntoken = \"{TEST_TOKEN}\"\n"),
    );
    let started = std::time::Instant::now();
    let probe = local_daemon::probe(&nothing, probe_identity()).await;
    assert!(matches!(probe, local_daemon::Probe::NothingListens));
    assert!(started.elapsed() < local_daemon::CONNECT_DEADLINE);
    assert_eq!(probe.notice(), None);
}

#[tokio::test]
async fn a_daemon_refusing_the_token_leaves_plain_cookie_in_process_with_a_notice() {
    let temporary = tempfile::tempdir().unwrap();
    private_directory(temporary.path());
    let daemon = ListeningDaemon::start(temporary.path()).await;
    let server = scratch_server(
        &temporary.path().join("home"),
        &format!("port = {}\ntoken = \"{OTHER_TOKEN}\"\n", daemon.port()),
    );
    let probe = local_daemon::probe(&server, probe_identity()).await;
    let local_daemon::Probe::Unusable { url, reason } = &probe else {
        panic!("a refused token must leave the daemon unusable");
    };
    assert_eq!(url, &format!("ws://127.0.0.1:{}/ws", daemon.port()));
    assert_eq!(reason, "HTTP error: 401 Unauthorized");
    let notice = probe.notice().unwrap();
    assert!(notice.contains(url.as_str()), "{notice}");
    assert!(notice.contains("401"), "{notice}");
    daemon.stop().await;
}

#[tokio::test]
async fn a_listener_that_is_no_cookie_daemon_leaves_a_notice_within_the_deadline() {
    let temporary = tempfile::tempdir().unwrap();
    private_directory(temporary.path());
    // Accepts and closes at once, as a TLS listener does with plain HTTP.
    let closing = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let closing_port = closing.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((stream, _)) = closing.accept().await {
            drop(stream);
        }
    });
    let server = scratch_server(
        &temporary.path().join("closing"),
        &format!("port = {closing_port}\ntoken = \"{TEST_TOKEN}\"\n"),
    );
    let probe = local_daemon::probe(&server, probe_identity()).await;
    assert!(
        matches!(probe, local_daemon::Probe::Unusable { .. }),
        "a listener that closes is something that answered"
    );
    assert!(probe.notice().is_some());

    // Accepts and never answers: the handshake deadline ends the wait.
    let silent = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let silent_port = silent.local_addr().unwrap().port();
    let held = Arc::new(Mutex::new(Vec::new()));
    let holder = Arc::clone(&held);
    tokio::spawn(async move {
        while let Ok((stream, _)) = silent.accept().await {
            holder.lock().unwrap().push(stream);
        }
    });
    let server = scratch_server(
        &temporary.path().join("silent"),
        &format!("port = {silent_port}\ntoken = \"{TEST_TOKEN}\"\n"),
    );
    let deadline = Duration::from_millis(200);
    let started = std::time::Instant::now();
    let probe = local_daemon::probe_within(
        &server,
        probe_identity(),
        local_daemon::CONNECT_DEADLINE,
        deadline,
    )
    .await;
    assert!(started.elapsed() >= deadline);
    let local_daemon::Probe::Unusable { reason, .. } = &probe else {
        panic!("a silent listener must leave the daemon unusable");
    };
    assert_eq!(reason, "no handshake within 200 ms");
}

#[tokio::test]
async fn a_daemon_on_another_machine_gets_a_dial_in_runtime_for_the_directory() {
    let temporary = tempfile::tempdir().unwrap();
    private_directory(temporary.path());
    let daemon = ListeningDaemon::start(temporary.path()).await;
    let server = scratch_server(
        &temporary.path().join("home"),
        &format!("port = {}\ntoken = \"{TEST_TOKEN}\"\n", daemon.port()),
    );
    let local_daemon::Probe::Connected(found) =
        local_daemon::probe(&server, probe_identity()).await
    else {
        panic!("the listening daemon was not found");
    };
    // This client's machine is another than the daemon's, and no runtime of
    // the daemon runs on it.
    let own = "client_elsewhere";
    let choice =
        local_daemon::runtime_for(&found.client, &found.hello, Some("another-machine"), own).await;
    assert_eq!(choice, local_daemon::RuntimeChoice::DialIn);

    let directory = temporary.path().join("client-directory");
    private_directory(&directory);
    let directory = directory.canonicalize().unwrap();
    let directory = directory.to_string_lossy().into_owned();
    start_dial_in(&found.url, &found.token, own, Some(&directory)).unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        let runtimes = found.client.list_runtimes().await.unwrap();
        if runtimes
            .runtimes
            .iter()
            .any(|runtime| runtime.runtime_id.as_str() == own && runtime.connected)
        {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the dial-in never connected: {runtimes:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let created = found
        .client
        .create_session(cookie_agent_protocol::SessionCreateParams {
            cwd: Some(directory.clone()),
            runtimes: Some(vec![cookie_agent_protocol::RuntimeAttachRequest {
                runtime_id: cookie_agent_protocol::RuntimeId::new(own).unwrap(),
                capability: Some(cookie_agent_protocol::CapabilityName::coding()),
                workspace: Some(directory.clone()),
            }]),
            ..cookie_agent_protocol::SessionCreateParams::new(session_selection())
        })
        .await
        .unwrap();
    assert!(
        created
            .runtimes
            .iter()
            .any(|attachment| attachment.runtime_id.as_str() == own
                && attachment.workspace.as_deref() == Some(directory.as_str())),
        "{:?}",
        created.runtimes
    );
    drop(found);
    daemon.stop().await;
}

#[test]
fn cookie_web_prints_the_page_with_the_token_in_its_fragment() {
    assert_eq!(
        Cli::try_parse_from([
            "cookie",
            "web",
            "--open",
            "--url",
            "wss://lab.example/cookie/ws"
        ])
        .unwrap()
        .command,
        Some(Command::Web {
            url: Some("wss://lab.example/cookie/ws".into()),
            token: None,
            open: true,
        })
    );
    assert_eq!(
        web_link("ws://127.0.0.1:7419/ws", TEST_TOKEN)
            .unwrap()
            .as_str(),
        format!("http://127.0.0.1:7419/#token={TEST_TOKEN}")
    );
    assert_eq!(
        web_link("wss://lab.example/cookie/ws", TEST_TOKEN)
            .unwrap()
            .as_str(),
        format!("https://lab.example/cookie/#token={TEST_TOKEN}")
    );
    assert!(web_link("ws://127.0.0.1:7419/runtime", TEST_TOKEN).is_err());
}

#[test]
fn the_daemon_names_its_page() {
    assert_eq!(
        web_page_line("ws://127.0.0.1:7419/ws").as_deref(),
        Some("web page at http://127.0.0.1:7419/ (`cookie web` prints it with the token)")
    );
    let line = web_page_line("wss://lab.example:8443/ws").expect("a page");
    assert!(
        line.starts_with("web page at https://lab.example:8443/ "),
        "{line}"
    );
    // The line carries no token.
    assert!(!line.contains("token="), "{line}");
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
        cookie_agent_protocol::Authentication::Authenticated,
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

/// `cookie onboard` asks for the models, preselects their variants, skips
/// the optional ones on Enter, and sets them through the daemon.
#[tokio::test]
async fn cookie_onboard_sets_the_models() {
    use onboard_command::{OnboardArgs, run_onboard_with};
    let temporary = tempfile::tempdir().unwrap();
    private_directory(temporary.path());
    let (client, root) = onboard_daemon(temporary.path()).await;

    // --yes without the default model refuses rather than guess.
    let refused = run_onboard_with(
        &client,
        OnboardArgs {
            yes: true,
            ..OnboardArgs::default()
        },
        &mut ScriptedConnectIo::default(),
    )
    .await
    .expect_err("refused");
    assert!(
        format!("{refused:#}").contains("--default-model"),
        "{refused:#}"
    );

    // Interactively: keep the provider, pick A Model (its default variant
    // preselected), skip the small model, then search for Z Model as the
    // utility model.
    let mut io = ScriptedConnectIo {
        public: VecDeque::from(
            ["n", "1", "", "", "/z-model", "1"]
                .map(str::to_owned)
                .to_vec(),
        ),
        ..ScriptedConnectIo::default()
    };
    run_onboard_with(&client, OnboardArgs::default(), &mut io)
        .await
        .expect("onboard");
    assert!(
        io.output
            .iter()
            .any(|line| line.contains("1. high (Enter)")),
        "the default model preselects its default variant: {:?}",
        io.output
    );
    let config = std::fs::read_to_string(root.join("config.toml")).unwrap();
    assert!(
        config.contains("default_model = \"custom.test/a-model\""),
        "{config}"
    );
    assert!(
        config.contains("utility_model = \"custom.test/z-model\""),
        "{config}"
    );
    assert!(!config.contains("small_model"), "{config}");

    // Flags answer the questions; the small model's variant is given.
    let mut io = ScriptedConnectIo::default();
    run_onboard_with(
        &client,
        OnboardArgs {
            yes: true,
            small_model: Some("custom.test/a-model:low".into()),
            ..OnboardArgs::default()
        },
        &mut io,
    )
    .await
    .expect("flags");
    assert!(
        io.output
            .iter()
            .any(|line| line == "Set small_model to custom.test/a-model (low)"),
        "{:?}",
        io.output
    );
}

fn write_check_skill(dir: &std::path::Path, name: &str, body: &str) -> String {
    std::fs::create_dir_all(dir.join(name)).unwrap();
    let text = format!("---\nname: {name}\ndescription: A skill to check.\n---\n{body}\n");
    std::fs::write(dir.join(name).join("SKILL.md"), &text).unwrap();
    format!(
        "{:x}",
        <sha2::Sha256 as sha2::Digest>::digest(text.as_bytes())
    )
}

/// `cookie skills check` passes skills that parse, name real commands, and
/// link real pages with a matching index, and names each problem otherwise.
#[tokio::test]
async fn cookie_skills_check_finds_what_a_skill_gets_wrong() {
    let docs = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/site");
    let good = tempfile::tempdir().unwrap();
    let sha = write_check_skill(
        good.path(),
        "tidy",
        "Run `cookie config set <key> <value>`, then `cookie daemon restart`.\nSee https://cookie-agent.github.io/doc/dev/engine/configuration/ and https://cookie-agent.github.io/doc/dev/extend/skills/#official-skills.",
    );
    std::fs::write(
        good.path().join("index.json"),
        serde_json::to_vec(&serde_json::json!({"version": 1, "skills": [
            {"name": "tidy", "version": "1.0.0", "requires_cookie": ">=0.4.0-alpha.0",
             "files": [{"path": "SKILL.md", "sha256": sha}]}
        ]}))
        .unwrap(),
    )
    .unwrap();
    let problems = skills_command::check(good.path(), Some(&docs))
        .await
        .unwrap();
    assert!(problems.is_empty(), "{problems:?}");

    let bad = tempfile::tempdir().unwrap();
    write_check_skill(
        bad.path(),
        "broken",
        "Run `cookie teleport now` and `cookie config frobnicate`.\nSee https://cookie-agent.github.io/doc/dev/no/such/page/.",
    );
    std::fs::create_dir_all(bad.path().join("unparsable")).unwrap();
    std::fs::write(bad.path().join("unparsable/SKILL.md"), "no frontmatter").unwrap();
    std::fs::write(
        bad.path().join("index.json"),
        serde_json::to_vec(&serde_json::json!({"version": 1, "skills": [
            {"name": "broken", "version": "1.0.0", "requires_cookie": "~1",
             "files": [{"path": "SKILL.md", "sha256": "0".repeat(64)}]}
        ]}))
        .unwrap(),
    )
    .unwrap();
    let problems = skills_command::check(bad.path(), Some(&docs))
        .await
        .unwrap();
    let all = problems.join("\n");
    for expected in [
        "`teleport` is not a subcommand of `cookie`",
        "`frobnicate` is not a subcommand of `config`",
        "no/such/page names no documentation page",
        "unparsable/SKILL.md",
        "is not a version range",
        "the index says",
        "`unparsable` is not listed",
    ] {
        assert!(all.contains(expected), "{expected:?} in {all}");
    }
}

/// `cookie skills list` shows the daemon's skills with their origin.
#[tokio::test]
async fn cookie_skills_list_shows_where_each_skill_comes_from() {
    let temporary = tempfile::tempdir().unwrap();
    private_directory(temporary.path());
    let (client, _) = onboard_daemon(temporary.path()).await;
    let lines = skills_command::skills_with(&client, skills_command::SkillsCommand::List)
        .await
        .expect("list");
    assert!(!lines.is_empty());
}
