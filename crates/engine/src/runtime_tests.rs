use std::{collections::BTreeMap, fs, os::unix::fs::PermissionsExt as _, sync::Arc};

use async_trait::async_trait;
use cookie_agent_config::{
    ApprovalConfig, ConfigSchemaVersion, ContextCompactionConfig, LoadedConfiguration,
    RuntimeConfig, ServerConfig, SessionTitleConfig, ToolOutputConfig, load_from_roots,
};
use cookie_agent_models::{
    ModelManager,
    catalog::{
        CatalogAgeState, CatalogAvailability, CatalogClaim, CatalogLimits, CatalogModalities,
        CatalogModelEntry, CatalogModelRecord, CatalogModelStatus, CatalogProviderClaims,
        CatalogProviderEntry, CatalogProviderRecord, CatalogQuarantineEntry,
        CatalogQuarantineReason, CatalogRuntimeState, CatalogSnapshot, CatalogSource,
    },
    provider_store::{ClientRequestId as StoreClientRequestId, ProviderStore},
};
use cookie_agent_protocol::{
    AgentId, ApprovalBoundary, ApprovalCapability, ApprovalResourceSource, CatalogRevision,
    ClientConnectId, ClientRequestId, EventPayload, InvocationId, ModelSelection, PermissionAction,
    PreparedApprovalResource, PreparedBindingLifetime, PreparedCapabilityOperation,
    PreparedOperationIdentity, PreparedResourceDigest, PreparedResourceIdentity,
    ProviderConnectParams, ProviderCredentialValues, ProviderDisconnectParams, ProviderId,
    ProviderModelId, RunSelection, RunStartParams, RuntimeChangeReason, SessionId, SetupFieldId,
    Sha256Digest, ToolCallId,
};
use jiff::Timestamp;
use tempfile::TempDir;

use crate::events::{EventLog, EventLogError};
use crate::{
    DelegateInvocation, Engine, EngineClient, EngineError, EngineOptions, PreparedExecutor,
    PreparedTool, SessionToolContext, ToolCall, ToolError, ToolExecutionContext,
    ToolPreparationContext, ToolProvider, ToolSpec,
};

#[derive(Clone)]
struct TestDelegateProvider {
    engine: EngineClient,
}

#[derive(serde::Deserialize, serde::Serialize)]
struct TestDelegateArgs {
    agent: AgentId,
    task: String,
}

struct TestDelegateExecutor {
    engine: EngineClient,
    call_id: ToolCallId,
    args: TestDelegateArgs,
}

#[async_trait]
impl ToolProvider for TestDelegateProvider {
    fn tools_for_session(&self, ctx: &SessionToolContext) -> Result<Vec<ToolSpec>, ToolError> {
        let targets = self
            .engine
            .delegate_targets(ctx.session)
            .map_err(|error| ToolError::execution(error.to_string()))?;
        Ok((!targets.is_empty())
            .then(|| ToolSpec {
                name: "delegate".to_owned(),
                description: "Delegate scripted work".to_owned(),
                parameters: serde_json::json!({
                    "type":"object",
                    "additionalProperties":false,
                    "properties":{
                        "agent":{"type":"string","enum":targets},
                        "task":{"type":"string"}
                    },
                    "required":["agent","task"]
                }),
            })
            .into_iter()
            .collect())
    }

    async fn prepare(
        &self,
        _ctx: ToolPreparationContext,
        call: ToolCall,
    ) -> Result<PreparedTool, ToolError> {
        let args: TestDelegateArgs = serde_json::from_value(call.arguments)
            .map_err(|error| ToolError::execution(error.to_string()))?;
        let label = args.agent.to_string();
        let label_digest = Sha256Digest::of_bytes(label.as_bytes());
        let operation = PreparedOperationIdentity::new(
            Sha256Digest::of_bytes(
                &serde_json::to_vec(&args)
                    .map_err(|error| ToolError::execution(error.to_string()))?,
            ),
            vec![ApprovalCapability {
                action: PermissionAction::Delegate,
                operation: PreparedCapabilityOperation::new("delegate:spawn")
                    .map_err(|error| ToolError::execution(error.to_string()))?,
            }],
            vec![PreparedApprovalResource {
                capability: PermissionAction::Delegate,
                canonical: PreparedResourceIdentity::new(format!("agent:{label_digest}"))
                    .map_err(|error| ToolError::execution(error.to_string()))?,
                binding_digest: PreparedResourceDigest::from_canonical_binding_bytes(
                    label.as_bytes(),
                ),
                binding_lifetime: PreparedBindingLifetime::RestartStable,
                boundary: ApprovalBoundary::Exact,
                source: ApprovalResourceSource::PrimaryOperation,
            }],
            Sha256Digest::of_bytes(b"scripted delegation context"),
        )
        .map_err(|error| ToolError::execution(error.to_string()))?;
        PreparedTool::new(
            operation,
            None,
            Box::new(TestDelegateExecutor {
                engine: self.engine.clone(),
                call_id: call.id,
                args,
            }),
        )
        .with_policy_labels(vec![label])
    }
}

#[async_trait]
impl PreparedExecutor for TestDelegateExecutor {
    async fn revalidate(&self) -> Result<(), ToolError> {
        Ok(())
    }

    async fn execute(
        self: Box<Self>,
        context: ToolExecutionContext,
    ) -> Result<cookie_agent_protocol::PersistedToolResult, ToolError> {
        let handle = self
            .engine
            .delegate_invoke(DelegateInvocation {
                parent_session_id: context.session,
                parent_run_id: context.run,
                parent_tool_call_id: self.call_id,
                agent: self.args.agent,
                task: self.args.task,
                context: Vec::new(),
                success_criteria: vec!["return a report".to_owned()],
                expected_output: serde_json::Value::Null,
            })
            .await
            .map_err(|error| ToolError::execution(error.to_string()))?;
        self.engine
            .await_delegate(handle)
            .await
            .map_err(|error| ToolError::execution(error.to_string()))
    }
}

struct Fixture {
    _directory: TempDir,
    engine: Engine,
    config: LoadedConfiguration,
    manager: Arc<ModelManager>,
}

fn fixture() -> Fixture {
    let directory = TempDir::new().expect("temp directory");
    fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700))
        .expect("private temp directory");
    let project = directory.path().join(".cookie-agent");
    fs::create_dir(&project).expect("project directory");
    fs::set_permissions(&project, fs::Permissions::from_mode(0o700)).expect("private project");
    let provider_store = directory.path().join("provider-store");
    fs::create_dir(&provider_store).expect("provider store directory");
    fs::set_permissions(&provider_store, fs::Permissions::from_mode(0o700))
        .expect("private provider store");
    let now = Timestamp::now();
    let catalog = Arc::new(CatalogSnapshot {
        revision: CatalogRevision::new(format!("sha256:{}", "0".repeat(64)))
            .expect("catalog revision"),
        source: CatalogSource::Bootstrap,
        state: CatalogRuntimeState {
            availability: CatalogAvailability::Bootstrap,
            age: CatalogAgeState::Current,
            last_error: None,
        },
        validated_at: now,
        last_checked_at: now,
        etag: None,
        providers: BTreeMap::new(),
        canonical_models: BTreeMap::new(),
        quarantine: Vec::new(),
    });
    let manager = Arc::new(
        ModelManager::new(
            BTreeMap::new(),
            catalog,
            ProviderStore::open(provider_store).expect("provider store"),
        )
        .expect("empty manager"),
    );
    let config = LoadedConfiguration {
        runtime: RuntimeConfig {
            schema_version: ConfigSchemaVersion,
            server: ServerConfig::default(),
            tool_output: ToolOutputConfig::default(),
            approval: ApprovalConfig::default(),
            context_compaction: ContextCompactionConfig::default(),
            session_title: SessionTitleConfig::default(),
            providers: BTreeMap::new(),
        },
        agents: BTreeMap::new(),
        provider_provenance: BTreeMap::new(),
    };
    let engine = Engine::open(EngineOptions {
        data_dir: directory.path().join("data"),
        cwd: directory.path().to_owned(),
        config: config.clone(),
        model_manager: Arc::clone(&manager),
        tools: Vec::new(),
    })
    .expect("empty engine");
    Fixture {
        _directory: directory,
        engine,
        config,
        manager,
    }
}

fn bedrock_catalog() -> Arc<CatalogSnapshot> {
    let provider_id = ProviderId::new("amazon-bedrock").expect("provider ID");
    let model_id =
        ProviderModelId::new("anthropic.claude-3-5-sonnet-20241022-v2:0").expect("model ID");
    let environment = [
        "AWS_ACCESS_KEY_ID",
        "AWS_BEARER_TOKEN_BEDROCK",
        "AWS_REGION",
        "AWS_SECRET_ACCESS_KEY",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect::<Vec<_>>();
    let model = CatalogModelRecord {
        id: model_id.clone(),
        name: "Bedrock Claude".to_owned(),
        description: "test".to_owned(),
        family: None,
        attachment: false,
        reasoning: false,
        tool_call: true,
        structured_output: Some(true),
        temperature: Some(true),
        open_weights: false,
        status: CatalogModelStatus::Stable,
        release_date: "2026-01-01".to_owned(),
        last_updated: "2026-01-01".to_owned(),
        modalities: CatalogModalities {
            input: vec!["text".to_owned()],
            output: vec!["text".to_owned()],
        },
        limits: CatalogLimits {
            context: 128_000,
            input: None,
            output: 16_384,
        },
        shape: None,
        provider: None,
        reasoning_options: Vec::new(),
        canonical_provenance: None,
    };
    let record = CatalogProviderRecord {
        id: provider_id.clone(),
        name: "Amazon Bedrock".to_owned(),
        environment: environment.clone(),
        npm: "@ai-sdk/amazon-bedrock".to_owned(),
        api: None,
        shape: None,
        claims: CatalogProviderClaims {
            environment: CatalogClaim::Present(environment),
            npm: CatalogClaim::Present("@ai-sdk/amazon-bedrock".to_owned()),
            api: CatalogClaim::Absent,
            shape: CatalogClaim::Absent,
        },
        documentation_url: "https://example.test/bedrock".to_owned(),
        models: BTreeMap::from([(
            model_id.clone(),
            CatalogModelEntry {
                id: model_id,
                record: Some(model),
                quarantine: None,
            },
        )]),
    };
    let now = Timestamp::now();
    Arc::new(CatalogSnapshot {
        revision: CatalogRevision::new(format!("sha256:{}", "b".repeat(64)))
            .expect("catalog revision"),
        source: CatalogSource::Network,
        state: CatalogRuntimeState {
            availability: CatalogAvailability::Ready,
            age: CatalogAgeState::Current,
            last_error: None,
        },
        validated_at: now,
        last_checked_at: now,
        etag: None,
        providers: BTreeMap::from([(
            provider_id.clone(),
            CatalogProviderEntry {
                id: provider_id,
                record: Some(record),
                quarantine: None,
            },
        )]),
        canonical_models: BTreeMap::new(),
        quarantine: Vec::new(),
    })
}

fn empty_provider_workspace(path: &std::path::Path) -> LoadedConfiguration {
    fs::create_dir(path).expect("workspace");
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).expect("private workspace");
    let project = path.join(".cookie-agent");
    fs::create_dir(&project).expect("project");
    fs::set_permissions(&project, fs::Permissions::from_mode(0o700)).expect("private project");
    fs::write(project.join("config.toml"), "schema_version = 7\n").expect("empty provider config");
    fs::set_permissions(
        project.join("config.toml"),
        fs::Permissions::from_mode(0o600),
    )
    .expect("private config");
    let agents = project.join("agents");
    fs::create_dir(&agents).expect("agents");
    fs::set_permissions(&agents, fs::Permissions::from_mode(0o700)).expect("private agents");
    fs::write(
        agents.join("primary.md"),
        "---\nschema: 1\ndescription: Bedrock test agent\nmode: primary\nenabled: true\nmodel_fallback: [{ model: \"amazon-bedrock/anthropic.claude-3-5-sonnet-20241022-v2:0\", variant: base }]\ntools: []\npermissions: []\n---\nUse Bedrock.\n",
    )
    .expect("agent");
    fs::set_permissions(agents.join("primary.md"), fs::Permissions::from_mode(0o600))
        .expect("private agent");
    load_from_roots(None, Some(&project)).expect("workspace config")
}

fn open_workspace_engine(
    workspace: &std::path::Path,
    data: &std::path::Path,
    provider_store: &std::path::Path,
    catalog: Arc<CatalogSnapshot>,
    config: LoadedConfiguration,
) -> (Engine, Arc<ModelManager>) {
    let manager = Arc::new(
        ModelManager::new(
            BTreeMap::new(),
            catalog,
            ProviderStore::open(provider_store).expect("shared provider store"),
        )
        .expect("workspace manager"),
    );
    let engine = Engine::open(EngineOptions {
        data_dir: data.to_owned(),
        cwd: workspace.to_owned(),
        config,
        model_manager: Arc::clone(&manager),
        tools: Vec::new(),
    })
    .expect("workspace engine");
    (engine, manager)
}

fn custom_fixture() -> (Fixture, RunSelection) {
    custom_fixture_with_endpoint("http://127.0.0.1:9/v1")
}

fn custom_fixture_with_endpoint(endpoint: &str) -> (Fixture, RunSelection) {
    let directory = TempDir::new().expect("temp directory");
    fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700))
        .expect("private temp directory");
    let project = directory.path().join(".cookie-agent");
    fs::create_dir(&project).expect("project directory");
    fs::set_permissions(&project, fs::Permissions::from_mode(0o700)).expect("private project");
    let config_text = r#"schema_version = 7

[providers."custom.test"]
source = "custom"
endpoint = "http://127.0.0.1:9/v1"
adaptor = "openai-compatible"
auth = { method = "no-auth-v1", values = {} }

[providers."custom.test".models."group/model"]
display_name = "Model"

[providers."custom.test".models."group/model".capabilities]
input = ["text"]
output = ["text"]
context_tokens = 4096
output_tokens = 1024
tool_calling = true
parallel_tool_calls = true
structured_output = false
reasoning = false
temperature = true
top_p = true
seed = true
native_replay = "unsupported"
native_compaction = "unsupported"
cancellation = "local_only"
media = {}
"#
    .replace("http://127.0.0.1:9/v1", endpoint);
    fs::write(project.join("config.toml"), config_text).expect("config");
    fs::set_permissions(
        project.join("config.toml"),
        fs::Permissions::from_mode(0o600),
    )
    .expect("private config");
    let agents = project.join("agents");
    fs::create_dir(&agents).expect("agents directory");
    fs::set_permissions(&agents, fs::Permissions::from_mode(0o700)).expect("private agents");
    fs::write(
        agents.join("primary.md"),
        "---\nschema: 1\ndescription: Primary test agent\nmode: primary\nenabled: true\nmodel_fallback: [{ model: \"custom.test/group/model\", variant: base }]\ntools: []\npermissions: [{ id: allow-delegate, action: delegate, resource: \"*\", effect: allow }]\ndelegation: { agents: [worker], max_depth: 1 }\n---\nTest prompt.\n",
    )
    .expect("agent");
    fs::set_permissions(agents.join("primary.md"), fs::Permissions::from_mode(0o600))
        .expect("private agent");
    fs::write(
        agents.join("worker.md"),
        "---\nschema: 1\ndescription: Worker test agent\nmode: subagent\nenabled: true\nmodel_fallback: [{ model: \"custom.test/group/model\", variant: base }]\ntools: []\npermissions: []\n---\nWorker prompt.\n",
    )
    .expect("worker agent");
    fs::set_permissions(agents.join("worker.md"), fs::Permissions::from_mode(0o600))
        .expect("private worker agent");
    let mut config = load_from_roots(None, Some(&project)).expect("loaded config");
    config.runtime.session_title.generate_on_first_turn = false;
    let provider_store = directory.path().join("provider-store");
    fs::create_dir(&provider_store).expect("provider store directory");
    fs::set_permissions(&provider_store, fs::Permissions::from_mode(0o700))
        .expect("private provider store");
    let now = Timestamp::now();
    let catalog = Arc::new(CatalogSnapshot {
        revision: CatalogRevision::new(format!("sha256:{}", "1".repeat(64)))
            .expect("catalog revision"),
        source: CatalogSource::Bootstrap,
        state: CatalogRuntimeState {
            availability: CatalogAvailability::Bootstrap,
            age: CatalogAgeState::Current,
            last_error: None,
        },
        validated_at: now,
        last_checked_at: now,
        etag: None,
        providers: BTreeMap::new(),
        canonical_models: BTreeMap::new(),
        quarantine: Vec::new(),
    });
    let manager = Arc::new(
        ModelManager::new(
            config.runtime.providers.clone(),
            catalog,
            ProviderStore::open(provider_store).expect("provider store"),
        )
        .expect("custom manager"),
    );
    let engine = Engine::open(EngineOptions {
        data_dir: directory.path().join("data"),
        cwd: directory.path().to_owned(),
        config: config.clone(),
        model_manager: Arc::clone(&manager),
        tools: Vec::new(),
    })
    .expect("custom engine");
    let selection = RunSelection {
        agent: AgentId::new("primary").expect("agent ID"),
        model: ModelSelection {
            model: "custom.test/group/model".parse().expect("model key"),
            variant: None,
        },
    };
    (
        Fixture {
            _directory: directory,
            engine,
            config,
            manager,
        },
        selection,
    )
}

async fn scripted_model_server() -> (String, tokio::task::JoinHandle<String>) {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("scripted listener");
    let address = listener.local_addr().expect("listener address");
    let task = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("scripted accept");
        let mut request = Vec::new();
        let mut buffer = [0_u8; 8192];
        loop {
            let read = socket.read(&mut buffer).await.expect("scripted read");
            if read == 0 {
                break;
            }
            request.extend_from_slice(&buffer[..read]);
            if request.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
        }
        let body = "data: {\"choices\":[{\"delta\":{\"content\":\"scripted root complete\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n";
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        socket
            .write_all(response.as_bytes())
            .await
            .expect("scripted response");
        String::from_utf8(request).expect("UTF-8 request")
    });
    (format!("http://{address}/v1"), task)
}

async fn scripted_delegation_server() -> (String, tokio::task::JoinHandle<Vec<String>>) {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("delegation listener");
    let address = listener.local_addr().expect("listener address");
    let task = tokio::spawn(async move {
        let bodies = [
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"delegate-call\",\"type\":\"function\",\"function\":{\"name\":\"delegate\",\"arguments\":\"{\\\"agent\\\":\\\"worker\\\",\\\"task\\\":\\\"write report\\\"}\"}}]},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"delegated child report\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"parent accepted child report\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        ];
        let mut requests = Vec::new();
        for body in bodies {
            let (mut socket, _) = listener.accept().await.expect("delegation accept");
            let mut request = Vec::new();
            let mut buffer = [0_u8; 8192];
            loop {
                let read = socket.read(&mut buffer).await.expect("delegation read");
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..read]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            socket
                .write_all(response.as_bytes())
                .await
                .expect("delegation response");
            requests.push(String::from_utf8(request).expect("UTF-8 request"));
        }
        requests
    });
    (format!("http://{address}/v1"), task)
}

fn reopen_engine(fixture: &Fixture) -> Engine {
    let current = fixture.manager.current();
    let manager = Arc::new(
        ModelManager::new(
            current.authored().clone(),
            Arc::clone(current.catalog()),
            ProviderStore::open(fixture._directory.path().join("provider-store"))
                .expect("reopened provider store"),
        )
        .expect("reopened manager"),
    );
    Engine::open(EngineOptions {
        data_dir: fixture._directory.path().join("data"),
        cwd: fixture._directory.path().to_owned(),
        config: fixture.config.clone(),
        model_manager: manager,
        tools: Vec::new(),
    })
    .expect("reopened engine")
}

#[test]
fn empty_startup_is_coherent_and_rejects_fabricated_sessions() {
    let fixture = fixture();
    let snapshot = fixture
        .engine
        .runtime_snapshot()
        .expect("runtime snapshot")
        .snapshot;
    assert!(snapshot.providers.is_empty());
    assert!(snapshot.models.is_empty());
    assert!(snapshot.agents.is_empty());
    let selection = RunSelection {
        agent: AgentId::new("primary").expect("agent ID"),
        model: ModelSelection {
            model: "openai/model".parse().expect("model key"),
            variant: None,
        },
    };
    assert!(matches!(
        fixture.engine.create_session(selection),
        Err(EngineError::NoRunnableModel)
    ));
}

#[test]
fn absent_disconnect_commits_once_and_replay_publishes_nothing() {
    let fixture = fixture();
    let initial = fixture
        .engine
        .runtime_snapshot()
        .expect("runtime snapshot")
        .snapshot;
    let mut notifications = fixture.engine.subscribe_runtime_changes();
    let request = ProviderDisconnectParams {
        provider_id: ProviderId::new("openai").expect("provider ID"),
        expected_runtime_revision: initial.runtime_revision.clone(),
        expected_provider_state_revision: initial.provider_state_revision.clone(),
        expected_connection_generation: None,
        client_request_id: ClientRequestId::new("absent-disconnect").expect("request ID"),
    };
    let first = fixture
        .engine
        .disconnect_provider(request.clone())
        .expect("first disconnect");
    assert!(!first.replayed);
    let changed = notifications.try_recv().expect("runtime notification");
    assert_eq!(
        changed.reasons,
        vec![RuntimeChangeReason::ProviderDisconnected]
    );
    assert_eq!(changed.previous_revision, Some(initial.runtime_revision));

    let replay = fixture
        .engine
        .disconnect_provider(request)
        .expect("disconnect replay");
    assert!(replay.replayed);
    assert!(notifications.try_recv().is_err());
    assert_eq!(
        replay.runtime.snapshot.runtime_revision,
        first.runtime.snapshot.runtime_revision
    );
}

#[test]
fn disconnect_replay_survives_a_clean_engine_restart() {
    let fixture = fixture();
    let initial = fixture.engine.runtime_snapshot().expect("runtime").snapshot;
    let request = ProviderDisconnectParams {
        provider_id: ProviderId::new("openai").expect("provider ID"),
        expected_runtime_revision: initial.runtime_revision,
        expected_provider_state_revision: initial.provider_state_revision,
        expected_connection_generation: None,
        client_request_id: ClientRequestId::new("restart-disconnect").expect("request ID"),
    };
    let first = fixture
        .engine
        .disconnect_provider(request.clone())
        .expect("first disconnect");
    let reopened = reopen_engine(&fixture);
    let mut notifications = reopened.subscribe_runtime_changes();
    let replay = reopened
        .disconnect_provider(request)
        .expect("restart replay");
    assert!(replay.replayed);
    assert_eq!(replay.durable_receipt, first.durable_receipt);
    assert!(notifications.try_recv().is_err());
}

#[tokio::test]
async fn global_bedrock_connection_executes_cross_workspace_and_disconnect_preserves_frozen_run() {
    let temporary = TempDir::new().expect("temporary root");
    fs::set_permissions(temporary.path(), fs::Permissions::from_mode(0o700)).expect("private root");
    let workspace_one = temporary.path().join("workspace-one");
    let workspace_two = temporary.path().join("workspace-two");
    let config_one = empty_provider_workspace(&workspace_one);
    let config_two = empty_provider_workspace(&workspace_two);
    assert!(config_one.runtime.providers.is_empty());
    assert!(config_two.runtime.providers.is_empty());
    let data = temporary.path().join("data");
    let provider_store = temporary.path().join("global-provider-store");
    let catalog = bedrock_catalog();
    let (engine_one, _) = open_workspace_engine(
        &workspace_one,
        &data,
        &provider_store,
        Arc::clone(&catalog),
        config_one.clone(),
    );
    let initial = engine_one
        .runtime_snapshot()
        .expect("initial runtime")
        .snapshot;
    assert!(initial.models.is_empty());
    let auth_values: ProviderCredentialValues = serde_json::from_value(serde_json::json!({
        "access_key_id":"bedrock-access",
        "secret_access_key":"bedrock-secret",
        "session_token":"bedrock-session"
    }))
    .expect("credential values");
    let connected = engine_one
        .connect_provider(ProviderConnectParams {
            provider_id: ProviderId::new("amazon-bedrock").expect("provider ID"),
            expected_catalog_revision: catalog.revision.clone(),
            setup_values: BTreeMap::from([(
                SetupFieldId::new("region").expect("setup field"),
                cookie_agent_protocol::SafeSetupValue::String(
                    cookie_agent_protocol::BoundedSetupString::new("us-east-1").expect("region"),
                ),
            )]),
            auth_method: cookie_agent_protocol::AuthMethodId::new("aws-sigv4-credentials-v1")
                .expect("auth method"),
            auth_values,
            client_connect_id: ClientConnectId::new("global-bedrock-connect").expect("connect ID"),
        })
        .expect("connect Bedrock");
    assert_eq!(
        connected.effective_auth_source,
        cookie_agent_protocol::EffectiveAuthSource::ProviderStore
    );
    assert_eq!(connected.runtime.models.len(), 1);
    assert!(connected.runtime.agents[0].runnable_as_root);

    let (engine_two, manager_two) = open_workspace_engine(
        &workspace_two,
        &data,
        &provider_store,
        Arc::clone(&catalog),
        config_two.clone(),
    );
    let second = engine_two
        .runtime_snapshot()
        .expect("second runtime")
        .snapshot;
    assert_eq!(second.models.len(), 1);
    assert_eq!(
        second.providers[0].effective_auth_state,
        cookie_agent_protocol::EffectiveAuthState::ProviderStore
    );
    let selection = RunSelection {
        agent: AgentId::new("primary").expect("agent ID"),
        model: ModelSelection {
            model: "amazon-bedrock/anthropic.claude-3-5-sonnet-20241022-v2:0"
                .parse()
                .expect("model key"),
            variant: None,
        },
    };
    manager_two
        .current()
        .resolve(&selection.model)
        .expect("cross-workspace executable constructor");
    let session = engine_two
        .create_session(selection.clone())
        .expect("session");
    let run = engine_two
        .start_run(RunStartParams {
            session_id: session.session_id,
            client_run_id: cookie_agent_protocol::ClientRunId::new("frozen-bedrock-run")
                .expect("run ID"),
            selection,
            input: "hold frozen Bedrock semantics".to_owned(),
        })
        .await
        .expect("accepted run");
    let frozen = engine_two
        .inner
        .store
        .get(session.session_id)
        .expect("session projection")
        .log
        .events()
        .into_iter()
        .find_map(|event| match event.payload {
            EventPayload::RunStarted {
                selected_suffix, ..
            } if event.run_id == Some(run.run_id) => Some(selected_suffix),
            _ => None,
        })
        .expect("frozen suffix");
    let connection = second.providers[0]
        .durable_connection
        .as_ref()
        .expect("durable connection");
    let disconnect_request = ProviderDisconnectParams {
        provider_id: ProviderId::new("amazon-bedrock").expect("provider ID"),
        expected_runtime_revision: second.runtime_revision,
        expected_provider_state_revision: second.provider_state_revision,
        expected_connection_generation: Some(connection.connection_generation),
        client_request_id: ClientRequestId::new("global-bedrock-disconnect")
            .expect("disconnect ID"),
    };
    let disconnected = engine_two
        .disconnect_provider(disconnect_request.clone())
        .expect("disconnect Bedrock");
    assert!(!disconnected.replayed);
    assert!(disconnected.runtime.snapshot.models.is_empty());
    assert_eq!(
        disconnected.runtime.snapshot.providers[0].effective_auth_state,
        cookie_agent_protocol::EffectiveAuthState::Unavailable
    );
    assert!(
        engine_one
            .runtime_snapshot()
            .expect("workspace one reload")
            .snapshot
            .models
            .is_empty()
    );
    let readable = engine_two
        .get_session(session.session_id)
        .expect("readable session");
    assert_eq!(readable.manifest_revision, frozen[0].manifest_revision);
    let still_frozen = engine_two
        .inner
        .store
        .get(session.session_id)
        .expect("session after disconnect")
        .log
        .events()
        .into_iter()
        .find_map(|event| match event.payload {
            EventPayload::RunStarted {
                selected_suffix, ..
            } if event.run_id == Some(run.run_id) => Some(selected_suffix),
            _ => None,
        })
        .expect("frozen suffix after disconnect");
    assert_eq!(still_frozen, frozen);
    engine_one.shutdown().await;
    engine_two.shutdown().await;

    let (reopened_two, _) =
        open_workspace_engine(&workspace_two, &data, &provider_store, catalog, config_two);
    let replay = reopened_two
        .disconnect_provider(disconnect_request)
        .expect("disconnect replay after restart");
    assert!(replay.replayed);
    assert!(reopened_two.get_session(session.session_id).is_ok());
    reopened_two.shutdown().await;
}

#[test]
fn catalog_refresh_publishes_one_coherent_reasoned_snapshot() {
    let fixture = fixture();
    let before = fixture.engine.current_runtime();
    let mut refreshed = (**before.models.catalog()).clone();
    refreshed.revision =
        CatalogRevision::new(format!("sha256:{}", "2".repeat(64))).expect("catalog revision");
    refreshed.source = CatalogSource::Network;
    refreshed.state.availability = CatalogAvailability::Ready;
    let mut notifications = fixture.engine.subscribe_runtime_changes();
    let result = fixture
        .engine
        .refresh_catalog(Arc::new(refreshed))
        .expect("catalog refresh");
    let changed = notifications.try_recv().expect("refresh notification");
    assert_eq!(changed.reasons, vec![RuntimeChangeReason::CatalogRefreshed]);
    assert_eq!(
        changed.previous_revision,
        Some(before.result.snapshot.runtime_revision.clone())
    );
    assert_eq!(changed.snapshot, result.snapshot);
    assert_eq!(fixture.engine.current_runtime().result, result);
}

#[test]
fn parser_quarantine_is_counted_and_changes_the_global_digest() {
    let fixture = fixture();
    let before = fixture.engine.runtime_snapshot().expect("runtime").snapshot;
    let mut catalog = (**fixture.manager.current().catalog()).clone();
    catalog.revision =
        CatalogRevision::new(format!("sha256:{}", "1".repeat(64))).expect("catalog revision");
    catalog.source = CatalogSource::Network;
    catalog.state.availability = CatalogAvailability::Ready;
    let provider_id = ProviderId::new("broken-provider").expect("provider ID");
    catalog.providers.insert(
        provider_id.clone(),
        CatalogProviderEntry {
            id: provider_id.clone(),
            record: None,
            quarantine: Some(CatalogQuarantineReason::InvalidCatalogProviderRecord),
        },
    );
    catalog.quarantine.push(CatalogQuarantineEntry {
        provider_id: Some(provider_id.to_string()),
        model_id: None,
        canonical_model_id: None,
        reason: CatalogQuarantineReason::InvalidCatalogProviderRecord,
    });

    let refreshed = fixture
        .engine
        .refresh_catalog(Arc::new(catalog))
        .expect("parser quarantine refresh")
        .snapshot;
    assert_eq!(refreshed.catalog_state.provider_quarantine_count, 1);
    assert_eq!(refreshed.catalog_state.model_quarantine_count, 0);
    assert_ne!(
        refreshed.catalog_state.quarantine_digest,
        before.catalog_state.quarantine_digest
    );
    let provider = refreshed
        .providers
        .iter()
        .find(|provider| provider.id == provider_id)
        .expect("quarantined provider descriptor");
    assert_eq!(
        provider.support.state,
        cookie_agent_protocol::ProviderSupportState::Quarantined
    );
    assert!(refreshed.catalog_state.provider_quarantine_count >= 1);
}

#[test]
fn registry_provider_drift_counts_but_unsupported_provider_does_not() {
    let fixture = fixture();
    let mut drifted = (*bedrock_catalog()).clone();
    drifted.revision =
        CatalogRevision::new(format!("sha256:{}", "2".repeat(64))).expect("catalog revision");
    let provider = drifted
        .providers
        .get_mut(&ProviderId::new("amazon-bedrock").expect("provider ID"))
        .expect("Bedrock provider")
        .record
        .as_mut()
        .expect("Bedrock record");
    provider.shape = Some("unexpected".to_owned());
    provider.claims.shape = CatalogClaim::Present("unexpected".to_owned());
    let drifted = fixture
        .engine
        .refresh_catalog(Arc::new(drifted))
        .expect("provider drift refresh")
        .snapshot;
    assert_eq!(drifted.catalog_state.provider_quarantine_count, 1);
    assert_eq!(drifted.catalog_state.model_quarantine_count, 0);
    assert_eq!(
        drifted.providers[0].support.state,
        cookie_agent_protocol::ProviderSupportState::Quarantined
    );
    assert_eq!(
        drifted.providers[0]
            .support
            .reason
            .as_ref()
            .map(cookie_agent_protocol::SafeCode::as_str),
        Some("catalog_provider_shape_drift")
    );

    let mut unsupported = (*bedrock_catalog()).clone();
    unsupported.revision =
        CatalogRevision::new(format!("sha256:{}", "3".repeat(64))).expect("catalog revision");
    let old_id = ProviderId::new("amazon-bedrock").expect("provider ID");
    let unknown_id = ProviderId::new("unknown-provider").expect("provider ID");
    let mut entry = unsupported
        .providers
        .remove(&old_id)
        .expect("provider entry");
    entry.id = unknown_id.clone();
    let record = entry.record.as_mut().expect("provider record");
    record.id = unknown_id.clone();
    record.npm = "@example/unknown-provider".to_owned();
    record.claims.npm = CatalogClaim::Present(record.npm.clone());
    record.environment.clear();
    record.claims.environment = CatalogClaim::Present(Vec::new());
    unsupported.providers.insert(unknown_id, entry);
    let unsupported = fixture
        .engine
        .refresh_catalog(Arc::new(unsupported))
        .expect("unsupported provider refresh")
        .snapshot;
    assert_eq!(unsupported.catalog_state.provider_quarantine_count, 0);
    assert_eq!(unsupported.catalog_state.model_quarantine_count, 0);
    assert_eq!(
        unsupported.providers[0].support.state,
        cookie_agent_protocol::ProviderSupportState::Unsupported
    );
}

#[test]
fn registry_model_shape_drift_is_counted_with_exact_model_identity() {
    let fixture = fixture();
    let mut catalog = (*bedrock_catalog()).clone();
    catalog.revision =
        CatalogRevision::new(format!("sha256:{}", "4".repeat(64))).expect("catalog revision");
    let provider = catalog
        .providers
        .get_mut(&ProviderId::new("amazon-bedrock").expect("provider ID"))
        .expect("provider")
        .record
        .as_mut()
        .expect("provider record");
    provider
        .models
        .values_mut()
        .next()
        .expect("model")
        .record
        .as_mut()
        .expect("model record")
        .shape = Some("unexpected".to_owned());
    let snapshot = fixture
        .engine
        .refresh_catalog(Arc::new(catalog))
        .expect("model drift refresh")
        .snapshot;
    assert_eq!(snapshot.catalog_state.provider_quarantine_count, 0);
    assert_eq!(snapshot.catalog_state.model_quarantine_count, 1);
    assert!(snapshot.models.is_empty());
}

#[test]
fn combined_quarantine_digest_is_order_independent_and_notifications_are_coherent() {
    let fixture = fixture();
    let mut catalog = (*bedrock_catalog()).clone();
    catalog.revision =
        CatalogRevision::new(format!("sha256:{}", "5".repeat(64))).expect("catalog revision");
    catalog
        .providers
        .get_mut(&ProviderId::new("amazon-bedrock").expect("provider ID"))
        .expect("provider")
        .record
        .as_mut()
        .expect("provider record")
        .models
        .values_mut()
        .next()
        .expect("model")
        .record
        .as_mut()
        .expect("model record")
        .shape = Some("unexpected".to_owned());
    let parser_provider = ProviderId::new("parser-broken").expect("provider ID");
    catalog.providers.insert(
        parser_provider.clone(),
        CatalogProviderEntry {
            id: parser_provider.clone(),
            record: None,
            quarantine: Some(CatalogQuarantineReason::InvalidCatalogProviderRecord),
        },
    );
    catalog.quarantine = vec![
        CatalogQuarantineEntry {
            provider_id: Some(parser_provider.to_string()),
            model_id: None,
            canonical_model_id: None,
            reason: CatalogQuarantineReason::InvalidCatalogProviderRecord,
        },
        CatalogQuarantineEntry {
            provider_id: Some("amazon-bedrock".to_owned()),
            model_id: Some("parser-model".to_owned()),
            canonical_model_id: None,
            reason: CatalogQuarantineReason::InvalidCatalogModelRecord,
        },
        CatalogQuarantineEntry {
            provider_id: None,
            model_id: None,
            canonical_model_id: Some("canonical-model".to_owned()),
            reason: CatalogQuarantineReason::InvalidCanonicalModelRecord,
        },
    ];
    let mut notifications = fixture.engine.subscribe_runtime_changes();
    let first = fixture
        .engine
        .refresh_catalog(Arc::new(catalog.clone()))
        .expect("combined refresh")
        .snapshot;
    let first_notification = notifications.try_recv().expect("first notification");
    assert_eq!(first_notification.snapshot, first);
    assert_eq!(first.catalog_state.provider_quarantine_count, 1);
    assert_eq!(first.catalog_state.model_quarantine_count, 3);

    catalog.revision =
        CatalogRevision::new(format!("sha256:{}", "6".repeat(64))).expect("catalog revision");
    catalog.quarantine.reverse();
    let reordered = fixture
        .engine
        .refresh_catalog(Arc::new(catalog.clone()))
        .expect("reordered refresh")
        .snapshot;
    let reordered_notification = notifications.try_recv().expect("reordered notification");
    assert_eq!(reordered_notification.snapshot, reordered);
    assert_eq!(
        reordered.catalog_state.quarantine_digest,
        first.catalog_state.quarantine_digest
    );
    assert_eq!(
        reordered.catalog_state.provider_quarantine_count,
        first.catalog_state.provider_quarantine_count
    );
    assert_eq!(
        reordered.catalog_state.model_quarantine_count,
        first.catalog_state.model_quarantine_count
    );

    catalog.revision =
        CatalogRevision::new(format!("sha256:{}", "7".repeat(64))).expect("catalog revision");
    catalog.quarantine.pop();
    let changed = fixture
        .engine
        .refresh_catalog(Arc::new(catalog))
        .expect("changed quarantine refresh")
        .snapshot;
    let changed_notification = notifications.try_recv().expect("changed notification");
    assert_eq!(changed_notification.snapshot, changed);
    assert_ne!(
        changed.catalog_state.quarantine_digest,
        reordered.catalog_state.quarantine_digest
    );
}

#[test]
fn failed_publication_preparation_commits_nothing_and_publishes_nothing() {
    use std::sync::atomic::Ordering;

    let fixture = fixture();
    let initial = fixture.engine.runtime_snapshot().expect("runtime").snapshot;
    let initial_generation = fixture.manager.current().store().generation();
    let mut notifications = fixture.engine.subscribe_runtime_changes();
    fixture
        .engine
        .inner
        .publication_failure
        .store(true, Ordering::Release);
    let result = fixture
        .engine
        .disconnect_provider(ProviderDisconnectParams {
            provider_id: ProviderId::new("openai").expect("provider ID"),
            expected_runtime_revision: initial.runtime_revision,
            expected_provider_state_revision: initial.provider_state_revision,
            expected_connection_generation: None,
            client_request_id: ClientRequestId::new("failed-publication").expect("request ID"),
        });
    assert!(matches!(result, Err(EngineError::ModelManager(_))));
    assert_eq!(
        fixture.manager.current().store().generation(),
        initial_generation
    );
    assert!(notifications.try_recv().is_err());
}

#[test]
fn corrupt_matching_manifest_rejects_reopen() {
    let fixture = fixture();
    let runtime = fixture.engine.current_runtime();
    let revision = runtime
        .current_manifest
        .revision
        .as_str()
        .strip_prefix("sha256:")
        .expect("manifest revision");
    let path = fixture
        ._directory
        .path()
        .join(".cookie-agent/model-snapshots")
        .join(format!("{revision}.json"));
    fs::write(&path, b"{\"schema_version\":1}\n").expect("corrupt manifest");
    let reopened = Engine::open(EngineOptions {
        data_dir: fixture._directory.path().join("other-data"),
        cwd: fixture._directory.path().to_owned(),
        config: fixture.config,
        model_manager: fixture.manager,
        tools: Vec::new(),
    });
    assert!(matches!(reopened, Err(EngineError::Manifest(_))));
}

#[test]
fn external_store_generation_is_reloaded_before_discovery() {
    let fixture = fixture();
    let current = fixture.manager.current();
    let external = ModelManager::new(
        current.authored().clone(),
        Arc::clone(current.catalog()),
        ProviderStore::open(fixture._directory.path().join("provider-store"))
            .expect("second provider store"),
    )
    .expect("second manager");
    let external_current = external.current();
    external
        .disconnect(
            cookie_agent_models::ProviderDisconnectRequest {
                provider_id: ProviderId::new("openai").expect("provider ID"),
                expected_runtime_revision: external_current.runtime_revision().clone(),
                expected_provider_state_revision: external_current.provider_state_revision(),
                expected_connection_generation: None,
                client_request_id: StoreClientRequestId::new("external-disconnect")
                    .expect("request ID"),
            },
            |_, _| Ok(()),
        )
        .expect("external mutation");
    let mut notifications = fixture.engine.subscribe_runtime_changes();
    let before = fixture
        .engine
        .current_runtime()
        .result
        .snapshot
        .runtime_revision
        .clone();
    let after = fixture
        .engine
        .runtime_snapshot()
        .expect("reloaded snapshot")
        .snapshot;
    assert_ne!(before, after.runtime_revision);
    let changed = notifications.try_recv().expect("reload notification");
    assert_eq!(
        changed.reasons,
        vec![
            RuntimeChangeReason::ProviderStoreChanged,
            RuntimeChangeReason::ProviderStoreReloaded,
        ]
    );
    assert_eq!(changed.previous_revision, Some(before));
    assert_eq!(changed.snapshot.runtime_revision, after.runtime_revision);
}

#[test]
fn protocol_seven_event_persistence_is_rejected_explicitly() {
    let directory = TempDir::new().expect("temp directory");
    let path = directory.path().join("events.jsonl");
    fs::write(
        &path,
        b"{\"event_schema_version\":7,\"payload\":{\"type\":\"session_created\"}}\n",
    )
    .expect("legacy event");
    let error = EventLog::open(path, SessionId::new_v7()).expect_err("version 7 rejected");
    assert!(matches!(
        error,
        EventLogError::UnsupportedSchemaVersion {
            found: 7,
            expected: 8,
            ..
        }
    ));
}

#[test]
fn engine_attempt_resolution_uses_the_published_executable_handle() {
    let (fixture, selection) = custom_fixture();
    let runtime = fixture.engine.current_runtime();
    let binding = crate::model_snapshots::binding_for_selection(
        &runtime.current_manifest,
        &runtime.models,
        &selection.model,
    )
    .expect("frozen binding");
    let expected = runtime
        .models
        .resolve(&selection.model)
        .expect("published executable");
    let resolved = crate::policy::resolve_model(&binding, &runtime).expect("engine resolution");
    assert!(Arc::ptr_eq(expected.model(), resolved.model()));
}

#[tokio::test]
async fn accepted_root_run_keeps_its_exact_manifest_binding_after_runtime_change() {
    let (fixture, selection) = custom_fixture();
    let session = fixture
        .engine
        .create_session(selection.clone())
        .expect("session");
    let run = fixture
        .engine
        .start_run(RunStartParams {
            session_id: session.session_id,
            client_run_id: cookie_agent_protocol::ClientRunId::new("immutable-run")
                .expect("run ID"),
            selection,
            input: "hello".to_owned(),
        })
        .await
        .expect("run");
    let (before, _) = fixture
        .engine
        .subscribe(session.session_id, None)
        .await
        .expect("events");
    let frozen = before
        .events
        .iter()
        .find_map(|event| match &event.payload {
            cookie_agent_protocol::EventPayload::RunStarted {
                selected_suffix, ..
            } if event.run_id == Some(run.run_id) => Some(selected_suffix.clone()),
            _ => None,
        })
        .expect("frozen suffix");
    let runtime = fixture.engine.runtime_snapshot().expect("runtime").snapshot;
    fixture
        .engine
        .disconnect_provider(ProviderDisconnectParams {
            provider_id: ProviderId::new("openai").expect("provider ID"),
            expected_runtime_revision: runtime.runtime_revision,
            expected_provider_state_revision: runtime.provider_state_revision,
            expected_connection_generation: None,
            client_request_id: ClientRequestId::new("immutability-change").expect("request ID"),
        })
        .expect("runtime mutation");
    let (after, _) = fixture
        .engine
        .subscribe(session.session_id, None)
        .await
        .expect("events after change");
    let still_frozen = after
        .events
        .iter()
        .find_map(|event| match &event.payload {
            cookie_agent_protocol::EventPayload::RunStarted {
                selected_suffix, ..
            } if event.run_id == Some(run.run_id) => Some(selected_suffix.clone()),
            _ => None,
        })
        .expect("frozen suffix after change");
    assert_eq!(frozen, still_frozen);
    assert_eq!(frozen[0].manifest_revision, session.manifest_revision);
}

#[tokio::test]
async fn scripted_root_run_completes_through_the_real_adapter_and_reopens() {
    let (endpoint, captured) = scripted_model_server().await;
    let (fixture, selection) = custom_fixture_with_endpoint(&endpoint);
    let session = fixture
        .engine
        .create_session(selection.clone())
        .expect("session");
    fixture
        .engine
        .start_run(RunStartParams {
            session_id: session.session_id,
            client_run_id: cookie_agent_protocol::ClientRunId::new("scripted-root")
                .expect("run ID"),
            selection,
            input: "hello scripted model".to_owned(),
        })
        .await
        .expect("accepted run");
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let projection = fixture
                .engine
                .inner
                .store
                .get(session.session_id)
                .expect("projection");
            if projection.status == cookie_agent_protocol::SessionStatus::Completed {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("scripted run completion");
    let request = captured.await.expect("scripted server task");
    assert!(request.starts_with("POST /v1/chat/completions? HTTP/1.1"));
    assert!(
        fixture
            .engine
            .inner
            .store
            .get(session.session_id)
            .expect("completed projection")
            .log
            .events()
            .iter()
            .any(|event| matches!(
                &event.payload,
                EventPayload::RunCompleted { final_text: Some(text) }
                    if text == "scripted root complete"
            ))
    );
    fixture.engine.shutdown().await;
    let reopened = reopen_engine(&fixture);
    assert_eq!(
        reopened
            .get_session(session.session_id)
            .expect("reopened scripted session")
            .status,
        cookie_agent_protocol::SessionStatus::Completed
    );
    reopened.shutdown().await;
}

#[tokio::test]
async fn scripted_parent_delegate_child_run_completes_and_reopens() {
    let (endpoint, captured) = scripted_delegation_server().await;
    let (fixture, selection) = custom_fixture_with_endpoint(&endpoint);
    fixture
        .engine
        .register_tool_provider(Arc::new(TestDelegateProvider {
            engine: fixture.engine.client(),
        }));
    let parent = fixture
        .engine
        .create_session(selection.clone())
        .expect("parent session");
    fixture
        .engine
        .start_run(RunStartParams {
            session_id: parent.session_id,
            client_run_id: cookie_agent_protocol::ClientRunId::new("scripted-delegation")
                .expect("run ID"),
            selection,
            input: "delegate this task".to_owned(),
        })
        .await
        .expect("accepted parent run");
    let completed = tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            let projection = fixture
                .engine
                .inner
                .store
                .get(parent.session_id)
                .expect("parent projection");
            if projection.status == cookie_agent_protocol::SessionStatus::Completed {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await;
    if completed.is_err() {
        let projection = fixture
            .engine
            .inner
            .store
            .get(parent.session_id)
            .expect("timed out parent projection");
        panic!(
            "delegation completion timed out: status={:?} events={:#?}",
            projection.status,
            projection.log.events()
        );
    }
    let requests = captured.await.expect("delegation server task");
    assert_eq!(requests.len(), 3);
    let children = fixture.engine.children(parent.session_id);
    assert_eq!(children.len(), 1);
    let child = fixture
        .engine
        .get_session(children[0].session_id)
        .expect("child session");
    assert_eq!(
        child.status,
        cookie_agent_protocol::SessionStatus::Completed
    );
    let entries = fixture.engine.inner.journal.entries();
    assert_eq!(entries.len(), 1);
    assert!(entries[0].linked);
    assert!(entries[0].child_run_id.is_some());
    fixture.engine.shutdown().await;

    let reopened = reopen_engine(&fixture);
    assert_eq!(
        reopened
            .get_session(parent.session_id)
            .expect("reopened parent")
            .status,
        cookie_agent_protocol::SessionStatus::Completed
    );
    assert_eq!(
        reopened
            .get_session(child.session_id)
            .expect("reopened child")
            .status,
        cookie_agent_protocol::SessionStatus::Completed
    );
    assert_eq!(reopened.inner.journal.entries().len(), 1);
    reopened.shutdown().await;
}

#[tokio::test]
async fn root_run_and_schema_eight_delegation_reservation_reopen_exactly() {
    let (fixture, selection) = custom_fixture();
    let session = fixture
        .engine
        .create_session(selection.clone())
        .expect("session");
    let run = fixture
        .engine
        .start_run(RunStartParams {
            session_id: session.session_id,
            client_run_id: cookie_agent_protocol::ClientRunId::new("reopen-root").expect("run ID"),
            selection,
            input: "scripted root input".to_owned(),
        })
        .await
        .expect("accepted root run");
    let projection = fixture
        .engine
        .inner
        .store
        .get(session.session_id)
        .expect("root projection");
    let agent = projection
        .log
        .events()
        .into_iter()
        .find_map(|event| match event.payload {
            EventPayload::SessionCreated { creation_agent, .. } => Some(*creation_agent),
            _ => None,
        })
        .expect("creation agent");
    let runtime = fixture.engine.current_runtime();
    let invocation_id = InvocationId::new_v7();
    let revisions = crate::journal::DelegationRuntimeRevisions {
        manifest_revision: agent.fallback_chain[0].manifest_revision.clone(),
        runtime_revision: runtime.result.snapshot.runtime_revision.clone(),
        catalog_revision: runtime.result.snapshot.catalog_revision.clone(),
        provider_state_revision: runtime.result.snapshot.provider_state_revision.clone(),
        model_revision: runtime.result.snapshot.model_revision.clone(),
        agent_revision: runtime.result.snapshot.agent_revision.clone(),
        recipe_registry_revision: runtime.result.snapshot.recipe_registry_revision.clone(),
    };
    fixture
        .engine
        .inner
        .journal
        .reserve(
            invocation_id,
            session.session_id,
            run.run_id,
            ToolCallId::new_v7(),
            agent.clone(),
            revisions.clone(),
            agent.fallback_chain.clone(),
            Sha256Digest::of_bytes(b"scripted delegation request"),
            crate::journal::DelegateRequestPayload {
                task: "scripted delegated task".to_owned(),
                context: Vec::new(),
                success_criteria: vec!["reopens exactly".to_owned()],
                expected_output: serde_json::Value::Null,
            },
        )
        .expect("delegation reservation");
    let persisted = fs::read_to_string(fixture.engine.inner.journal.path())
        .expect("persisted delegation journal");
    let line = persisted.lines().next().expect("started journal record");
    let raw: serde_json::Value = serde_json::from_str(line).expect("journal JSON");
    let generated: cookie_agent_protocol::StoredDelegationJournalRecord =
        serde_json::from_value(raw.clone()).expect("generated protocol journal type");
    assert_eq!(
        serde_json::to_value(&generated).expect("journal reserialization"),
        raw
    );
    let schema: serde_json::Value = serde_json::from_str(include_str!(
        "../../protocol/generated/json-schema/StoredDelegationJournalRecord.schema.json"
    ))
    .expect("generated delegation journal schema");
    assert_eq!(schema["title"], "StoredDelegationJournalRecord");
    assert_eq!(
        schema["properties"]["delegation_journal_schema_version"]["const"],
        8
    );
    assert_eq!(schema["additionalProperties"], false);
    let required_keys = |value: &serde_json::Value| {
        value
            .as_array()
            .expect("schema required array")
            .iter()
            .map(|key| key.as_str().expect("schema key").to_owned())
            .collect::<std::collections::BTreeSet<_>>()
    };
    assert_eq!(
        required_keys(&schema["required"]),
        raw.as_object()
            .expect("stored record object")
            .keys()
            .cloned()
            .collect()
    );
    let started_schema = &schema["$defs"]["DelegationJournalRecord"]["oneOf"][0];
    assert_eq!(started_schema["additionalProperties"], false);
    assert_eq!(
        required_keys(&started_schema["required"]),
        raw["record"]
            .as_object()
            .expect("started record object")
            .keys()
            .cloned()
            .collect()
    );
    let schema_text = serde_json::to_string(&schema).expect("schema text");
    for required in [
        "manifest_revision",
        "runtime_revision",
        "catalog_revision",
        "provider_state_revision",
        "model_revision",
        "agent_revision",
        "recipe_registry_revision",
    ] {
        assert!(schema_text.contains(required), "missing {required}");
    }
    fixture.engine.shutdown().await;

    let reopened = reopen_engine(&fixture);
    let reopened_session = reopened
        .get_session(session.session_id)
        .expect("reopened root");
    assert_eq!(
        reopened_session.manifest_revision,
        session.manifest_revision
    );
    let entry = reopened
        .inner
        .journal
        .get(invocation_id)
        .expect("reopened delegation reservation");
    assert_eq!(entry.revisions, revisions);
    assert_eq!(entry.selected_suffix, agent.fallback_chain);
    reopened.shutdown().await;

    let mut forged: serde_json::Value = serde_json::from_str(line).expect("journal value");
    forged["record"]["runtime_revision"] = serde_json::json!(format!("sha256:{}", "f".repeat(64)));
    fs::write(
        fixture.engine.inner.journal.path(),
        format!(
            "{}\n",
            serde_json::to_string(&forged).expect("forged journal")
        ),
    )
    .expect("write forged journal revision");
    let current = fixture.manager.current();
    let manager = Arc::new(
        ModelManager::new(
            current.authored().clone(),
            Arc::clone(current.catalog()),
            ProviderStore::open(fixture._directory.path().join("provider-store"))
                .expect("reopened provider store"),
        )
        .expect("reopened manager"),
    );
    let rejected = Engine::open(EngineOptions {
        data_dir: fixture._directory.path().join("data"),
        cwd: fixture._directory.path().to_owned(),
        config: fixture.config,
        model_manager: manager,
        tools: Vec::new(),
    });
    assert!(matches!(rejected, Err(EngineError::RuntimeCompileFailed)));
}
