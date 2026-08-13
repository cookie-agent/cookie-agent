use std::{
    collections::BTreeMap,
    fs,
    os::unix::fs::PermissionsExt as _,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use async_trait::async_trait;
use cookie_agent_config::{
    ApprovalConfig, ConfigSchemaVersion, ContextCompactionConfig, LoadedConfiguration,
    RuntimeConfig, ServerConfig, SessionTitleConfig, ToolOutputConfig, load_from_roots,
};
use cookie_agent_models::{
    ModelManager,
    catalog::{
        CatalogAgeState, CatalogAvailability, CatalogLimits, CatalogModalities, CatalogModelEntry,
        CatalogModelRecord, CatalogModelStatus, CatalogProviderEntry, CatalogProviderRecord,
        CatalogQuarantineEntry, CatalogQuarantineReason, CatalogRuntimeState, CatalogSnapshot,
        CatalogSource,
    },
    provider_store::{ClientRequestId as StoreClientRequestId, ProviderStore},
};
use cookie_agent_protocol::{
    AgentId, ApprovalBoundary, ApprovalCapability, ApprovalDecisionSource, ApprovalFinalOutcome,
    ApprovalInternalDecisionKind, ApprovalReasonCode, ApprovalResourceSource,
    ApprovalRespondParams, ApprovalStatus, ApprovalUserDecision, CatalogRevision, ClientConnectId,
    ClientRenameId, ClientRequestId, ClientResponseId, ClientRunId, EventPayload,
    InternalAgentKind, InvocationId, ModelSelection, PermissionAction, PermissionMode,
    PreparedApprovalResource, PreparedBindingLifetime, PreparedCapabilityOperation,
    PreparedOperationIdentity, PreparedResourceDigest, PreparedResourceIdentity,
    ProviderConnectParams, ProviderCredentialValues, ProviderDisconnectParams, ProviderId,
    ProviderModelId, RunSelection, RunStartParams, RuntimeChangeReason, SessionId, SessionStatus,
    SessionTitle, SessionTitleChange, SetupFieldId, Sha256Digest, ToolCallId,
    ToolTerminationOutcome,
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

    fn get_primary_argument(
        &self,
        name: &str,
        arguments: &serde_json::Value,
    ) -> Result<String, ToolError> {
        if name != "delegate" {
            return Err(ToolError::execution(
                "delegate provider received another tool",
            ));
        }
        let args: TestDelegateArgs = serde_json::from_value(arguments.clone())
            .map_err(|error| ToolError::execution(error.to_string()))?;
        Ok(args.agent.to_string())
    }

    fn get_simplified_argument(
        &self,
        name: &str,
        arguments: &serde_json::Value,
    ) -> Result<String, ToolError> {
        self.get_primary_argument(name, arguments)
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
            serde_json::to_value(&args).map_err(|error| ToolError::execution(error.to_string()))?,
            None,
            Box::new(TestDelegateExecutor {
                engine: self.engine.clone(),
                call_id: call.id,
                args,
            }),
        )?
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

#[derive(Clone)]
struct TestWriteProvider {
    executed: Arc<AtomicBool>,
}

struct TestWriteExecutor {
    executed: Arc<AtomicBool>,
}

#[async_trait]
impl ToolProvider for TestWriteProvider {
    fn tools_for_session(&self, _ctx: &SessionToolContext) -> Result<Vec<ToolSpec>, ToolError> {
        Ok(vec![ToolSpec {
            name: "write".to_owned(),
            description: "Write a test value".to_owned(),
            parameters: serde_json::json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {}
            }),
        }])
    }

    fn get_primary_argument(
        &self,
        name: &str,
        _arguments: &serde_json::Value,
    ) -> Result<String, ToolError> {
        if name != "write" {
            return Err(ToolError::execution("write provider received another tool"));
        }
        Ok("approval-test.txt".into())
    }

    fn get_simplified_argument(
        &self,
        name: &str,
        arguments: &serde_json::Value,
    ) -> Result<String, ToolError> {
        self.get_primary_argument(name, arguments)
    }

    async fn prepare(
        &self,
        _ctx: ToolPreparationContext,
        _call: ToolCall,
    ) -> Result<PreparedTool, ToolError> {
        let label = "approval-test.txt";
        let operation = PreparedOperationIdentity::new(
            Sha256Digest::of_bytes(b"approval test write arguments"),
            vec![ApprovalCapability {
                action: PermissionAction::Write,
                operation: PreparedCapabilityOperation::new("write:file")
                    .map_err(|error| ToolError::execution(error.to_string()))?,
            }],
            vec![PreparedApprovalResource {
                capability: PermissionAction::Write,
                canonical: PreparedResourceIdentity::new(label)
                    .map_err(|error| ToolError::execution(error.to_string()))?,
                binding_digest: PreparedResourceDigest::from_canonical_binding_bytes(
                    label.as_bytes(),
                ),
                binding_lifetime: PreparedBindingLifetime::RestartStable,
                boundary: ApprovalBoundary::Exact,
                source: ApprovalResourceSource::PrimaryOperation,
            }],
            Sha256Digest::of_bytes(b"approval test execution context"),
        )
        .map_err(|error| ToolError::execution(error.to_string()))?;
        PreparedTool::new(
            operation,
            serde_json::json!({}),
            None,
            Box::new(TestWriteExecutor {
                executed: Arc::clone(&self.executed),
            }),
        )
    }
}

#[async_trait]
impl PreparedExecutor for TestWriteExecutor {
    async fn revalidate(&self) -> Result<(), ToolError> {
        Ok(())
    }

    async fn execute(
        self: Box<Self>,
        _context: ToolExecutionContext,
    ) -> Result<cookie_agent_protocol::PersistedToolResult, ToolError> {
        self.executed.store(true, Ordering::Release);
        Ok(cookie_agent_protocol::PersistedToolResult {
            title: cookie_agent_protocol::SafeDisplayText::new("approval test write")
                .expect("result title"),
            output: "executed".to_owned(),
            metadata: serde_json::Value::Null,
            truncation: None,
            attachments: Vec::new(),
        })
    }
}

#[derive(Clone)]
struct TestRehydrationReadProvider {
    executed: Arc<AtomicBool>,
    swap_after_prepare: bool,
}

struct TestRehydrationReadExecutor {
    executed: Arc<AtomicBool>,
    path: std::path::PathBuf,
    expected: Option<std::path::PathBuf>,
}

#[async_trait]
impl ToolProvider for TestRehydrationReadProvider {
    fn tools_for_session(&self, _ctx: &SessionToolContext) -> Result<Vec<ToolSpec>, ToolError> {
        Ok(vec![ToolSpec {
            name: "read".into(),
            description: "Test capability-bound read".into(),
            parameters: serde_json::json!({
                "type":"object",
                "additionalProperties":false,
                "properties":{"filePath":{"type":"string"}},
                "required":["filePath"]
            }),
        }])
    }

    fn get_primary_argument(
        &self,
        name: &str,
        arguments: &serde_json::Value,
    ) -> Result<String, ToolError> {
        if name != "read" {
            return Err(ToolError::execution("read provider received another tool"));
        }
        arguments
            .get("filePath")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| ToolError::execution("missing filePath"))
    }

    fn get_simplified_argument(
        &self,
        name: &str,
        arguments: &serde_json::Value,
    ) -> Result<String, ToolError> {
        self.get_primary_argument(name, arguments)
    }

    async fn prepare(
        &self,
        ctx: ToolPreparationContext,
        call: ToolCall,
    ) -> Result<PreparedTool, ToolError> {
        let display = call
            .arguments
            .get("filePath")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| ToolError::execution("missing filePath"))?
            .to_owned();
        let path = if std::path::Path::new(&display).is_absolute() {
            std::path::PathBuf::from(&display)
        } else {
            ctx.cwd.join(&display)
        };
        let expected = std::fs::read_link(&path).ok();
        if self.swap_after_prepare && expected.is_some() {
            std::fs::remove_file(&path).map_err(|error| ToolError::execution(error.to_string()))?;
            std::os::unix::fs::symlink("denied.txt", &path)
                .map_err(|error| ToolError::execution(error.to_string()))?;
        }
        let operation = PreparedOperationIdentity::new(
            Sha256Digest::of_bytes(display.as_bytes()),
            vec![ApprovalCapability {
                action: PermissionAction::Read,
                operation: PreparedCapabilityOperation::new("read:file")
                    .map_err(|error| ToolError::execution(error.to_string()))?,
            }],
            vec![PreparedApprovalResource {
                capability: PermissionAction::Read,
                canonical: PreparedResourceIdentity::new(display.clone())
                    .map_err(|error| ToolError::execution(error.to_string()))?,
                binding_digest: PreparedResourceDigest::from_canonical_binding_bytes(
                    display.as_bytes(),
                ),
                binding_lifetime: PreparedBindingLifetime::ProcessLocal,
                boundary: ApprovalBoundary::Exact,
                source: ApprovalResourceSource::PrimaryOperation,
            }],
            Sha256Digest::of_bytes(b"rehydration read context"),
        )
        .map_err(|error| ToolError::execution(error.to_string()))?;
        PreparedTool::new(
            operation,
            serde_json::json!({"filePath": path}),
            None,
            Box::new(TestRehydrationReadExecutor {
                executed: Arc::clone(&self.executed),
                path,
                expected,
            }),
        )
    }
}

#[async_trait]
impl PreparedExecutor for TestRehydrationReadExecutor {
    async fn revalidate(&self) -> Result<(), ToolError> {
        Ok(())
    }

    async fn execute(
        self: Box<Self>,
        _context: ToolExecutionContext,
    ) -> Result<cookie_agent_protocol::PersistedToolResult, ToolError> {
        if self.expected.is_some() && std::fs::read_link(&self.path).ok() != self.expected {
            return Err(ToolError::operation_changed(
                "read symlink changed after capability preparation",
            ));
        }
        self.executed.store(true, Ordering::Release);
        let output = std::fs::read_to_string(&self.path)
            .map_err(|error| ToolError::execution(error.to_string()))?;
        Ok(cookie_agent_protocol::PersistedToolResult {
            title: cookie_agent_protocol::SafeDisplayText::new("rehydrated read").unwrap(),
            output,
            metadata: serde_json::Value::Null,
            truncation: None,
            attachments: Vec::new(),
        })
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
            delegation: cookie_agent_config::DelegationConfig::default(),
            providers: BTreeMap::new(),
        },
        agents: BTreeMap::new(),
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
        interleaved: None,
        canonical_provenance: None,
    };
    let record = CatalogProviderRecord {
        id: provider_id.clone(),
        name: "Amazon Bedrock".to_owned(),
        environment: environment.clone(),
        npm: "@ai-sdk/amazon-bedrock".to_owned(),
        api: None,
        shape: None,
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
    fs::write(project.join("config.toml"), "schema_version = 10\n").expect("empty provider config");
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
        "---\nschema: 4\ndescription: Bedrock test agent\nmode: primary\nenabled: true\nmodel_fallback: [{ model: \"amazon-bedrock/anthropic.claude-3-5-sonnet-20241022-v2:0\", variant: base }]\ntools: []\npermissions: {}\n---\nUse Bedrock.\n",
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

#[tokio::test]
async fn session_metadata_tracks_log_tail_for_create_get_list_tree_and_append() {
    let (fixture, selection) = custom_fixture();
    let created = fixture
        .engine
        .create_session(selection.clone())
        .expect("create session");
    let creation_event = fixture
        .engine
        .inner
        .store
        .get(created.session_id)
        .expect("created projection")
        .log
        .last_event()
        .expect("creation event");
    assert_eq!(created.last_activity, creation_event.timestamp);
    assert_eq!(
        fixture
            .engine
            .get_session(created.session_id)
            .expect("get session")
            .last_activity,
        creation_event.timestamp
    );
    assert_eq!(
        fixture
            .engine
            .list_sessions()
            .into_iter()
            .find(|session| session.session_id == created.session_id)
            .expect("listed session")
            .last_activity,
        creation_event.timestamp
    );
    assert_eq!(
        fixture
            .engine
            .tree(created.session_id)
            .expect("session tree")
            .session
            .last_activity,
        creation_event.timestamp
    );

    fixture
        .engine
        .append_direct(
            created.session_id,
            None,
            EventPayload::SessionTitleCommitted {
                input_through_seq: creation_event.seq,
                change: SessionTitleChange::UserSet {
                    title: SessionTitle::new("Latest activity").expect("title"),
                    client_rename_id: ClientRenameId::new("latest-activity").expect("rename ID"),
                },
            },
        )
        .expect("append event");
    fixture
        .engine
        .start_run(RunStartParams {
            session_id: created.session_id,
            client_run_id: ClientRunId::new("metadata-persist").expect("client run ID"),
            selection,
            input: "persist session".into(),
        })
        .await
        .expect("persist session");
    for _ in 0..200 {
        if fixture
            .engine
            .get_session(created.session_id)
            .is_ok_and(|meta| meta.status != cookie_agent_protocol::SessionStatus::Running)
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let latest_event = fixture
        .engine
        .inner
        .store
        .get(created.session_id)
        .expect("updated projection")
        .log
        .last_event()
        .expect("latest event");
    assert_eq!(
        fixture
            .engine
            .get_session(created.session_id)
            .expect("updated session")
            .last_activity,
        latest_event.timestamp
    );
    assert_eq!(
        fixture
            .engine
            .list_sessions()
            .into_iter()
            .find(|session| session.session_id == created.session_id)
            .expect("updated listed session")
            .last_activity,
        latest_event.timestamp
    );
    assert_eq!(
        fixture
            .engine
            .tree(created.session_id)
            .expect("updated tree")
            .session
            .last_activity,
        latest_event.timestamp
    );

    let reopened = reopen_engine(&fixture);
    assert_eq!(
        reopened
            .get_session(created.session_id)
            .expect("replayed session")
            .last_activity,
        latest_event.timestamp
    );
}

#[tokio::test]
async fn session_metadata_cache_version_eight_is_rejected() {
    let (fixture, selection) = custom_fixture();
    let session = fixture
        .engine
        .create_session(selection.clone())
        .expect("create session");
    fixture
        .engine
        .start_run(RunStartParams {
            session_id: session.session_id,
            client_run_id: ClientRunId::new("metadata-cache-persist").expect("client run ID"),
            selection,
            input: "persist session".into(),
        })
        .await
        .expect("persist session");
    for _ in 0..200 {
        if fixture
            .engine
            .get_session(session.session_id)
            .is_ok_and(|meta| meta.status != cookie_agent_protocol::SessionStatus::Running)
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let path = fixture
        .engine
        .inner
        .store
        .session_dir(session.session_id)
        .join("meta.json");
    let mut persisted: serde_json::Value =
        serde_json::from_slice(&fs::read(&path).expect("read metadata cache"))
            .expect("parse metadata cache");
    assert!(persisted.get("last_activity").is_none());
    persisted["meta_schema_version"] = serde_json::json!(8);
    fs::write(
        &path,
        serde_json::to_vec_pretty(&persisted).expect("encode old metadata cache"),
    )
    .expect("write old metadata cache");

    let error = crate::session::SessionStore::open(
        &fixture._directory.path().join("data"),
        fixture._directory.path(),
    )
    .expect_err("schema 8 metadata cache must be rejected");
    assert!(error.to_string().contains("expected 9"));
}

#[test]
fn empty_session_is_live_only_and_disappears_on_restart_without_artifacts() {
    let (fixture, selection) = custom_fixture();
    let session = fixture
        .engine
        .create_session(selection)
        .expect("create session");
    let session_dir = fixture.engine.inner.store.session_dir(session.session_id);

    assert!(!session_dir.exists());
    assert!(
        fixture
            .engine
            .list_sessions()
            .iter()
            .any(|listed| listed.session_id == session.session_id)
    );
    fixture
        .engine
        .set_permission_mode(session.session_id, PermissionMode::Ask)
        .expect("set memory-only permission mode");
    fixture
        .engine
        .append_direct(
            session.session_id,
            None,
            EventPayload::SessionTitleCommitted {
                input_through_seq: 1,
                change: SessionTitleChange::UserSet {
                    title: SessionTitle::new("Memory-only title").expect("title"),
                    client_rename_id: ClientRenameId::new("memory-only-title").expect("rename ID"),
                },
            },
        )
        .expect("append memory-only title");
    assert!(!session_dir.exists());

    let reopened = reopen_engine(&fixture);
    assert!(
        !reopened
            .list_sessions()
            .iter()
            .any(|listed| listed.session_id == session.session_id)
    );
    assert!(reopened.get_session(session.session_id).is_err());
    assert!(!session_dir.exists());
}

#[tokio::test]
async fn first_user_message_flushes_complete_ordered_buffer_and_replays_exactly() {
    let (fixture, selection) = custom_fixture();
    let session = fixture
        .engine
        .create_session(selection.clone())
        .expect("create session");
    let session_dir = fixture.engine.inner.store.session_dir(session.session_id);
    assert!(!session_dir.exists());

    fixture
        .engine
        .start_run(RunStartParams {
            session_id: session.session_id,
            client_run_id: ClientRunId::new("first-persist").expect("client run ID"),
            selection,
            input: "first user message".into(),
        })
        .await
        .expect("start run");

    assert!(session_dir.join("meta.json").is_file());
    assert!(session_dir.join("events.jsonl").is_file());
    for _ in 0..200 {
        if fixture
            .engine
            .get_session(session.session_id)
            .is_ok_and(|meta| meta.status != cookie_agent_protocol::SessionStatus::Running)
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    let memory_events = fixture
        .engine
        .inner
        .store
        .get(session.session_id)
        .expect("live session")
        .log
        .events();
    let disk_events = crate::events::load_jsonl::<cookie_agent_protocol::StoredEvent>(
        &session_dir.join("events.jsonl"),
    )
    .expect("disk events");
    assert_eq!(disk_events, memory_events);
    assert!(matches!(
        disk_events[0].payload,
        EventPayload::SessionCreated { .. }
    ));
    assert!(matches!(
        disk_events[1].payload,
        EventPayload::RunStarted { .. }
    ));
    assert!(matches!(
        disk_events[2].payload,
        EventPayload::UserInputSubmitted { .. }
    ));
    assert!(
        disk_events
            .iter()
            .enumerate()
            .all(|(index, event)| event.seq == index as u64 + 1)
    );

    let reopened = reopen_engine(&fixture);
    assert_eq!(
        reopened
            .inner
            .store
            .get(session.session_id)
            .expect("replayed session")
            .log
            .events(),
        memory_events
    );
}

fn custom_fixture_with_endpoint(endpoint: &str) -> (Fixture, RunSelection) {
    custom_fixture_with_endpoint_and_primary_agent(
        endpoint,
        "---\nschema: 4\ndescription: Primary test agent\nmode: primary\nenabled: true\nmodel_fallback: [{ model: \"custom.test/group/model\", variant: base }]\ntools: []\npermissions:\n  delegate:\n    worker: allow\n---\nTest prompt.\n",
    )
}

fn approval_fixture_with_endpoint(endpoint: &str) -> (Fixture, RunSelection) {
    custom_fixture_with_endpoint_and_primary_agent(
        endpoint,
        "---\nschema: 4\ndescription: Approval test agent\nmode: primary\nenabled: true\nmodel_fallback: [{ model: \"custom.test/group/model\", variant: base }]\ntools: [write]\npermissions:\n  write: ask\n---\nTest approval flow.\n",
    )
}

fn denied_approval_fixture_with_endpoint(endpoint: &str) -> (Fixture, RunSelection) {
    custom_fixture_with_endpoint_and_primary_agent(
        endpoint,
        "---\nschema: 4\ndescription: Denied approval test agent\nmode: primary\nenabled: true\nmodel_fallback: [{ model: \"custom.test/group/model\", variant: base }]\ntools: [write]\npermissions:\n  write: deny\n---\nTest denied approval flow.\n",
    )
}

fn custom_fixture_with_endpoint_and_primary_agent(
    endpoint: &str,
    primary_agent: &str,
) -> (Fixture, RunSelection) {
    custom_fixture_with_endpoint_primary_and_internal(endpoint, primary_agent, None, None)
}

fn custom_fixture_with_endpoint_primary_and_internal(
    endpoint: &str,
    primary_agent: &str,
    internal: Option<(&str, &str)>,
    compaction_buffer_tokens: Option<u64>,
) -> (Fixture, RunSelection) {
    let directory = TempDir::new().expect("temp directory");
    fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700))
        .expect("private temp directory");
    let project = directory.path().join(".cookie-agent");
    fs::create_dir(&project).expect("project directory");
    fs::set_permissions(&project, fs::Permissions::from_mode(0o700)).expect("private project");
    let config_text = r#"schema_version = 10

[delegation]
max_depth = 1

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
    fs::write(agents.join("primary.md"), primary_agent).expect("agent");
    fs::set_permissions(agents.join("primary.md"), fs::Permissions::from_mode(0o600))
        .expect("private agent");
    fs::write(
        agents.join("worker.md"),
        "---\nschema: 4\ndescription: Worker test agent\nmode: subagent\nenabled: true\nmodel_fallback: [{ model: \"custom.test/group/model\", variant: base }]\ntools: []\npermissions: {}\n---\nWorker prompt.\n",
    )
    .expect("worker agent");
    fs::set_permissions(agents.join("worker.md"), fs::Permissions::from_mode(0o600))
        .expect("private worker agent");
    if let Some((name, document)) = internal {
        fs::write(agents.join(name), document).expect("internal agent");
        fs::set_permissions(agents.join(name), fs::Permissions::from_mode(0o600))
            .expect("private internal agent");
    }
    let mut config = load_from_roots(None, Some(&project)).expect("loaded config");
    config.runtime.session_title.generate_on_first_turn = false;
    if let Some(buffer_tokens) = compaction_buffer_tokens {
        config.runtime.context_compaction.buffer_tokens = buffer_tokens;
    }
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

fn frozen_root_policy(
    fixture: &Fixture,
    selection: &RunSelection,
) -> crate::policy::FrozenRunPolicy {
    let runtime = fixture.engine.current_runtime();
    let registry = Arc::clone(&runtime.agents);
    let agent = crate::policy::resolve_agent(&registry, &selection.agent).expect("resolved agent");
    crate::policy::freeze_root_agent_policy(
        agent,
        Arc::clone(&registry),
        runtime,
        &selection.model,
        3,
        crate::policy::ResultLimits {
            tool_output_max_lines: 2_000,
            tool_output_max_bytes: 50 * 1024,
        },
    )
    .expect("frozen root policy")
}

fn completed_read_events(
    session: SessionId,
    run: cookie_agent_protocol::RunId,
    path: &str,
) -> Vec<cookie_agent_protocol::StoredEvent> {
    let model_call_id = cookie_agent_protocol::ModelCallId::new("rehydration-read").unwrap();
    let tool_call_id = ToolCallId::new_v7();
    let owner = cookie_agent_protocol::AssistantToolCallRef {
        model_turn_seq: 1,
        content_index: 0,
        model_call_id: model_call_id.clone(),
        provider_item_id: None,
    };
    let envelope = |seq, payload| cookie_agent_protocol::StoredEvent {
        event_schema_version: cookie_agent_protocol::EventSchemaVersion::current(),
        session_id: session,
        run_id: Some(run),
        seq,
        timestamp: Timestamp::now(),
        payload,
    };
    vec![
        envelope(
            1,
            EventPayload::ModelTurnCommitted {
                attempt_id: cookie_agent_protocol::AttemptId::new_v7(),
                model_turn_seq: 1,
                resolved_model: crate::policy::wire_resolved(&crate::test_support::model_binding()),
                input_through_seq: 1,
                turn: cookie_agent_protocol::PersistedModelTurn {
                    content: vec![cookie_agent_protocol::PersistedAssistantPart::ToolCall {
                        id: model_call_id,
                        provider_item_id: None,
                        name: cookie_agent_protocol::SafeCode::new("read").unwrap(),
                        input: serde_json::json!({"filePath": path}),
                        raw_input: None,
                        metadata: None,
                    }],
                    provider_options: BTreeMap::new(),
                    finish_reason: cookie_agent_protocol::ModelFinishReason::ToolCalls,
                    usage: cookie_agent_protocol::Usage::default(),
                    response_metadata: BTreeMap::new(),
                    provider_metadata: BTreeMap::new(),
                    native_replay: None,
                },
                warnings: Vec::new(),
            },
        ),
        envelope(
            2,
            EventPayload::ToolCallStarted {
                start: cookie_agent_protocol::ToolCallStart {
                    tool_call_id,
                    owner: owner.clone(),
                    presentation: cookie_agent_protocol::ToolCallPresentation {
                        title: cookie_agent_protocol::SafeDisplayText::new("Read").unwrap(),
                        primary_argument: None,
                    },
                    operation_fingerprint: serde_json::from_value(serde_json::json!({
                        "digest": Sha256Digest::of_bytes(path.as_bytes())
                    }))
                    .unwrap(),
                },
            },
        ),
        envelope(
            3,
            EventPayload::ToolCallTerminated {
                termination: cookie_agent_protocol::ToolCallTermination {
                    tool_call_id,
                    owner,
                    outcome: ToolTerminationOutcome::Completed,
                    result: Some(cookie_agent_protocol::PersistedToolResult {
                        title: cookie_agent_protocol::SafeDisplayText::new("Read").unwrap(),
                        output: "historical output".into(),
                        metadata: serde_json::Value::Null,
                        truncation: None,
                        attachments: Vec::new(),
                    }),
                    error: None,
                },
            },
        ),
    ]
}

#[tokio::test]
async fn rehydration_skips_reads_denied_by_the_frozen_permission_pipeline() {
    let (fixture, selection) = custom_fixture_with_endpoint_and_primary_agent(
        "http://127.0.0.1:9/v1",
        "---\nschema: 4\ndescription: Rehydration deny test\nmode: primary\nenabled: true\nmodel_fallback: [{ model: \"custom.test/group/model\", variant: base }]\ntools: [read]\npermissions:\n  read: deny\n---\nTest denied rehydration.\n",
    );
    let executed = Arc::new(AtomicBool::new(false));
    fixture
        .engine
        .register_tool_provider(Arc::new(TestRehydrationReadProvider {
            executed: Arc::clone(&executed),
            swap_after_prepare: false,
        }));
    let session = fixture
        .engine
        .create_session(selection.clone())
        .expect("rehydration session");
    let run = cookie_agent_protocol::RunId::new_v7();
    let owner = frozen_root_policy(&fixture, &selection);
    let files = fixture
        .engine
        .rehydrated_files_for_test(
            session.session_id,
            run,
            &owner,
            &completed_read_events(session.session_id, run, "denied.txt"),
        )
        .await;
    assert!(files.is_empty());
    assert!(!executed.load(Ordering::Acquire));
}

#[tokio::test]
async fn rehydration_skips_a_symlink_swapped_after_capability_preparation() {
    let (fixture, selection) = custom_fixture_with_endpoint_and_primary_agent(
        "http://127.0.0.1:9/v1",
        "---\nschema: 4\ndescription: Rehydration swap test\nmode: primary\nenabled: true\nmodel_fallback: [{ model: \"custom.test/group/model\", variant: base }]\ntools: [read]\npermissions:\n  read: allow\n---\nTest swapped rehydration.\n",
    );
    fs::write(fixture._directory.path().join("allowed.txt"), "allowed").expect("allowed file");
    fs::write(fixture._directory.path().join("denied.txt"), "denied").expect("denied file");
    std::os::unix::fs::symlink("allowed.txt", fixture._directory.path().join("link.txt"))
        .expect("read symlink");
    let executed = Arc::new(AtomicBool::new(false));
    fixture
        .engine
        .register_tool_provider(Arc::new(TestRehydrationReadProvider {
            executed: Arc::clone(&executed),
            swap_after_prepare: true,
        }));
    let session = fixture
        .engine
        .create_session(selection.clone())
        .expect("rehydration session");
    let run = cookie_agent_protocol::RunId::new_v7();
    let owner = frozen_root_policy(&fixture, &selection);
    let files = fixture
        .engine
        .rehydrated_files_for_test(
            session.session_id,
            run,
            &owner,
            &completed_read_events(session.session_id, run, "link.txt"),
        )
        .await;
    assert!(files.is_empty());
    assert!(!executed.load(Ordering::Acquire));
}

#[test]
fn parent_model_resolves_exact_binding_skips_parentless_and_replays_historically() {
    let fixture = synthetic_default_fixture(None);
    let descriptor = fixture
        .engine
        .runtime_snapshot()
        .expect("runtime")
        .snapshot
        .agents
        .into_iter()
        .find(|agent| agent.id.as_str() == "default")
        .expect("default agent");
    let selection = RunSelection {
        agent: descriptor.id,
        model: descriptor.resolved_fallback[0].clone(),
    };
    let owner = frozen_root_policy(&fixture, &selection);
    let parent = owner.selected_suffix[0].clone();
    assert_eq!(
        parent
            .selection
            .variant
            .as_ref()
            .map(|variant| variant.as_str()),
        Some("precise")
    );

    let policy = fixture
        .engine
        .internal_agent_policy(InternalAgentKind::ContextCompaction, &owner, Some(&parent))
        .expect("internal policy");
    assert_eq!(policy.models, vec![parent.clone()]);

    let parentless = fixture
        .engine
        .internal_agent_policy(InternalAgentKind::ContextCompaction, &owner, None)
        .expect("parentless policy");
    assert!(parentless.models.is_empty());

    let replayed_owner = crate::policy::policy_from_snapshot(
        owner.agent.clone(),
        owner.selected_suffix.clone(),
        Arc::clone(&owner.registry),
        Arc::clone(&owner.runtime),
        owner.result_limits.tool_output_max_lines,
        owner.result_limits.tool_output_max_bytes,
    )
    .expect("replayed owner policy");
    let replayed = fixture
        .engine
        .internal_agent_policy(
            InternalAgentKind::ContextCompaction,
            &replayed_owner,
            replayed_owner.selected_suffix.first(),
        )
        .expect("replayed internal policy");
    assert_eq!(replayed.models, vec![parent]);
}

#[test]
fn manual_compaction_resolves_parent_model_from_nonzero_active_fallback() {
    let fixture = synthetic_default_fixture(None);
    let descriptor = fixture
        .engine
        .runtime_snapshot()
        .expect("runtime")
        .snapshot
        .agents
        .into_iter()
        .find(|agent| agent.id.as_str() == "default")
        .expect("default agent");
    let selection = RunSelection {
        agent: descriptor.id,
        model: descriptor.resolved_fallback[0].clone(),
    };
    let mut owner = frozen_root_policy(&fixture, &selection);
    let fallback = crate::test_support::model_binding_named("fallback-one");
    owner.selected_suffix.push(fallback.clone());
    owner.selected_suffix_wire.push(fallback.clone());
    let run = cookie_agent_protocol::RunId::new_v7();
    let events = vec![cookie_agent_protocol::StoredEvent {
        event_schema_version: cookie_agent_protocol::EventSchemaVersion::current(),
        session_id: SessionId::new_v7(),
        run_id: Some(run),
        seq: 1,
        timestamp: Timestamp::now(),
        payload: EventPayload::ModelAttemptStarted {
            attempt_id: cookie_agent_protocol::AttemptId::new_v7(),
            attempt_ordinal: 2,
            fallback_index: 1,
            retry_ordinal: 0,
            resolved_model: crate::policy::wire_resolved(&fallback),
            prompt_fingerprint: Sha256Digest::of_bytes(b"fallback prompt"),
        },
    }];
    let binding = crate::runtime::compaction::active_compaction_binding(&owner, &events, run)
        .expect("active compaction binding");
    assert_eq!(binding.selection, fallback.selection);
    let internal = fixture
        .engine
        .internal_agent_policy(InternalAgentKind::ContextCompaction, &owner, Some(binding))
        .expect("compaction policy");
    assert_eq!(internal.models, vec![fallback]);
}

#[test]
fn workspace_internal_agent_replaces_builtin_document_and_limits() {
    let (fixture, selection) = custom_fixture_with_endpoint_primary_and_internal(
        "http://127.0.0.1:9/v1",
        "---\nschema: 4\ndescription: Primary test agent\nmode: primary\nenabled: true\nmodel_fallback: [{ model: \"custom.test/group/model\", variant: base }]\ntools: []\npermissions: {}\n---\nPrimary.\n",
        Some((
            "approval.md",
            "---\nschema: 4\ndescription: Workspace approval\nmode: internal\nenabled: true\nmodel_fallback: [{ model: \"${parent_model}\" }]\nlimits: { timeout_ms: 1234, max_input_tokens: 2345, max_output_tokens: 345 }\ntools: [bash]\npermissions: {}\n---\nWorkspace approval prompt.\n",
        )),
        None,
    );
    let owner = frozen_root_policy(&fixture, &selection);
    let policy = fixture
        .engine
        .internal_agent_policy(
            InternalAgentKind::Approval,
            &owner,
            owner.selected_suffix.first(),
        )
        .expect("workspace approval policy");

    assert_eq!(
        policy.agent.document_source,
        cookie_agent_protocol::AgentDocumentSource::Workspace
    );
    assert_eq!(policy.agent.composed_prompt, "Workspace approval prompt.\n");
    assert_eq!(policy.limits.timeout_ms, 1234);
    assert_eq!(policy.limits.max_input_tokens, 2345);
    assert_eq!(policy.limits.max_output_tokens, 345);
    assert!(policy.agent.tools.is_empty());
}

fn synthetic_default_fixture(authored_agent: Option<&str>) -> Fixture {
    let directory = TempDir::new().expect("temp directory");
    fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700))
        .expect("private temp directory");
    let project = directory.path().join(".cookie-agent");
    fs::create_dir(&project).expect("project directory");
    fs::set_permissions(&project, fs::Permissions::from_mode(0o700)).expect("private project");
    fs::write(
        project.join("config.toml"),
        r#"schema_version = 10

[providers."custom.test"]
source = "custom"
endpoint = "http://127.0.0.1:9/v1"
adaptor = "openai-compatible"
auth = { method = "no-auth-v1", values = {} }

[providers."custom.test".models."z-model"]
display_name = "Z Model"
capabilities = { input = ["text"], output = ["text"], context_tokens = 4096, output_tokens = 1024, tool_calling = true, parallel_tool_calls = true, structured_output = false, reasoning = false, temperature = true, top_p = true, seed = true, native_replay = "unsupported", cancellation = "local_only", media = {} }

[providers."custom.test".models."a-model"]
display_name = "A Model"
capabilities = { input = ["text"], output = ["text"], context_tokens = 4096, output_tokens = 1024, tool_calling = true, parallel_tool_calls = true, structured_output = false, reasoning = false, temperature = true, top_p = true, seed = true, native_replay = "unsupported", cancellation = "local_only", media = {} }
variants = { zeta = { operation = "add" }, alpha = { operation = "add" }, precise = { operation = "add", defaults = { temperature = 0.25 } } }
default_variant = "precise"
"#,
    )
    .expect("config");
    fs::set_permissions(
        project.join("config.toml"),
        fs::Permissions::from_mode(0o600),
    )
    .expect("private config");
    if let Some(agent) = authored_agent {
        let agents = project.join("agents");
        fs::create_dir(&agents).expect("agents directory");
        fs::set_permissions(&agents, fs::Permissions::from_mode(0o700)).expect("private agents");
        fs::write(agents.join("primary.md"), agent).expect("agent");
        fs::set_permissions(agents.join("primary.md"), fs::Permissions::from_mode(0o600))
            .expect("private agent");
    }
    let config = load_from_roots(None, Some(&project)).expect("loaded config");
    let provider_store = directory.path().join("provider-store");
    fs::create_dir(&provider_store).expect("provider store directory");
    fs::set_permissions(&provider_store, fs::Permissions::from_mode(0o700))
        .expect("private provider store");
    let now = Timestamp::now();
    let catalog = Arc::new(CatalogSnapshot {
        revision: CatalogRevision::new(format!("sha256:{}", "2".repeat(64)))
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
    .expect("engine");
    Fixture {
        _directory: directory,
        engine,
        config,
        manager,
    }
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

async fn scripted_zero_resource_tool_server() -> (String, tokio::task::JoinHandle<Vec<String>>) {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("zero-resource listener");
    let address = listener.local_addr().expect("listener address");
    let bodies = [
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"zero-resource-write\",\"type\":\"function\",\"function\":{\"name\":\"write\",\"arguments\":\"{}\"}}]},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"content\":\"resource-free write rejected\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
    ];
    let task = tokio::spawn(async move {
        let mut requests = Vec::new();
        for body in bodies {
            let (mut socket, _) = listener.accept().await.expect("zero-resource accept");
            let mut request = Vec::new();
            let mut buffer = [0_u8; 8192];
            loop {
                let read = socket.read(&mut buffer).await.expect("zero-resource read");
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
                .expect("zero-resource response");
            requests.push(String::from_utf8(request).expect("UTF-8 request"));
        }
        requests
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

async fn scripted_approval_server(
    internal_output: &str,
) -> (String, tokio::task::JoinHandle<Vec<String>>) {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("approval listener");
    let address = listener.local_addr().expect("listener address");
    let internal_delta = serde_json::json!({
        "choices": [{
            "delta": {"content": internal_output},
            "finish_reason": null
        }]
    });
    let bodies = [
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"write-call\",\"type\":\"function\",\"function\":{\"name\":\"write\",\"arguments\":\"{}\"}}]},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n"
            .to_owned(),
        format!(
            "data: {internal_delta}\n\ndata: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"stop\"}}]}}\n\n"
        ),
        "data: {\"choices\":[{\"delta\":{\"content\":\"approval flow complete\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n"
            .to_owned(),
    ];
    let task = tokio::spawn(async move {
        let mut requests = Vec::new();
        for body in bodies {
            let (mut socket, _) = listener.accept().await.expect("approval accept");
            let mut request = Vec::new();
            let mut buffer = [0_u8; 8192];
            loop {
                let read = socket.read(&mut buffer).await.expect("approval read");
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
                .expect("approval response");
            requests.push(String::from_utf8(request).expect("UTF-8 request"));
        }
        requests
    });
    (format!("http://{address}/v1"), task)
}

async fn scripted_server_with_delayed_response(
    bodies: Vec<String>,
    delayed_index: usize,
) -> (
    String,
    tokio::task::JoinHandle<Vec<String>>,
    tokio::sync::oneshot::Receiver<()>,
    Arc<tokio::sync::Notify>,
) {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("delayed listener");
    let address = listener.local_addr().expect("listener address");
    let (reached_tx, reached_rx) = tokio::sync::oneshot::channel();
    let release = Arc::new(tokio::sync::Notify::new());
    let task_release = Arc::clone(&release);
    let task = tokio::spawn(async move {
        let mut reached_tx = Some(reached_tx);
        let mut requests = Vec::new();
        for (index, body) in bodies.into_iter().enumerate() {
            let (mut socket, _) = listener.accept().await.expect("delayed accept");
            let mut request = Vec::new();
            let mut buffer = [0_u8; 8192];
            let expected_len = loop {
                let read = socket.read(&mut buffer).await.expect("delayed read");
                if read == 0 {
                    break request.len();
                }
                request.extend_from_slice(&buffer[..read]);
                let Some(header_end) = request.windows(4).position(|window| window == b"\r\n\r\n")
                else {
                    continue;
                };
                let header_end = header_end + 4;
                let headers = String::from_utf8_lossy(&request[..header_end]);
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        line.strip_prefix("content-length: ")
                            .or_else(|| line.strip_prefix("Content-Length: "))
                    })
                    .and_then(|value| value.trim().parse::<usize>().ok())
                    .unwrap_or(0);
                break header_end + content_length;
            };
            while request.len() < expected_len {
                let read = socket.read(&mut buffer).await.expect("delayed body read");
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..read]);
            }
            if index == delayed_index {
                if let Some(reached_tx) = reached_tx.take() {
                    let _ = reached_tx.send(());
                }
                task_release.notified().await;
            }
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = socket.write_all(response.as_bytes()).await;
            requests.push(String::from_utf8(request).expect("UTF-8 request"));
        }
        requests
    });
    (format!("http://{address}/v1"), task, reached_rx, release)
}

async fn scripted_repeated_write_server(
    tool_calls: usize,
) -> (String, tokio::task::JoinHandle<Vec<String>>) {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("repeated write listener");
    let address = listener.local_addr().expect("listener address");
    let mut bodies = (0..tool_calls)
        .map(|index| {
            format!(
                "data: {{\"choices\":[{{\"delta\":{{\"tool_calls\":[{{\"index\":0,\"id\":\"write-call-{index}\",\"type\":\"function\",\"function\":{{\"name\":\"write\",\"arguments\":\"{{}}\"}}}}]}},\"finish_reason\":null}}]}}\n\ndata: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\n"
            )
        })
        .collect::<Vec<_>>();
    bodies.push(
        "data: {\"choices\":[{\"delta\":{\"content\":\"permission sequence complete\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n"
            .to_owned(),
    );
    let task = tokio::spawn(async move {
        let mut requests = Vec::new();
        for body in bodies {
            let (mut socket, _) = listener.accept().await.expect("repeated write accept");
            let mut request = Vec::new();
            let mut buffer = [0_u8; 8192];
            loop {
                let read = socket.read(&mut buffer).await.expect("repeated write read");
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
                .expect("repeated write response");
            requests.push(String::from_utf8(request).expect("UTF-8 request"));
        }
        requests
    });
    (format!("http://{address}/v1"), task)
}

async fn scripted_two_approved_writes_server() -> (String, tokio::task::JoinHandle<Vec<String>>) {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("persistent approval listener");
    let address = listener.local_addr().expect("listener address");
    let tool_call = |index| {
        format!(
            "data: {{\"choices\":[{{\"delta\":{{\"tool_calls\":[{{\"index\":0,\"id\":\"persistent-write-{index}\",\"type\":\"function\",\"function\":{{\"name\":\"write\",\"arguments\":\"{{}}\"}}}}]}},\"finish_reason\":null}}]}}\n\ndata: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\n"
        )
    };
    let approval = "data: {\"choices\":[{\"delta\":{\"content\":\"{\\\"decision\\\":\\\"allow\\\"}\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n".to_owned();
    let bodies = [
        tool_call(1),
        approval.clone(),
        tool_call(2),
        approval,
        "data: {\"choices\":[{\"delta\":{\"content\":\"persistent approvals complete\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n".to_owned(),
    ];
    let task = tokio::spawn(async move {
        let mut requests = Vec::new();
        for body in bodies {
            let (mut socket, _) = listener.accept().await.expect("persistent approval accept");
            let mut request = Vec::new();
            let mut buffer = [0_u8; 8192];
            loop {
                let read = socket
                    .read(&mut buffer)
                    .await
                    .expect("persistent approval read");
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
                .expect("persistent approval response");
            requests.push(String::from_utf8(request).expect("UTF-8 request"));
        }
        requests
    });
    (format!("http://{address}/v1"), task)
}

async fn wait_for_escalated_approval(
    engine: &Engine,
    session_id: SessionId,
) -> cookie_agent_protocol::ApprovalRecord {
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let mut approvals = engine
                .list_approvals(session_id, Some(ApprovalStatus::Escalated))
                .approvals;
            if let Some(approval) = approvals.pop()
                && engine
                    .inner
                    .pending_approvals
                    .lock()
                    .expect("pending approvals lock")
                    .contains_key(&(session_id, approval.request.approval_id()))
            {
                return approval;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("user-visible escalated approval")
}

async fn approve_once(
    engine: &Engine,
    approval: &cookie_agent_protocol::ApprovalRecord,
    client_response_id: &str,
) -> cookie_agent_protocol::ApprovalRespondResult {
    let request_revision = serde_json::to_value(&approval.request)
        .expect("approval request JSON")
        .get("revision")
        .and_then(serde_json::Value::as_u64)
        .expect("approval request revision");
    engine
        .approval_respond(ApprovalRespondParams {
            session_id: approval.session_id,
            approval_id: approval.request.approval_id(),
            request_revision,
            operation_fingerprint: approval.request.operation_fingerprint().clone(),
            client_response_id: ClientResponseId::new(client_response_id)
                .expect("client response ID"),
            decision: ApprovalUserDecision::ApproveOnce,
            feedback: None,
        })
        .await
        .expect("approve once")
}

async fn wait_for_tool_execution(executed: &AtomicBool) {
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while !executed.load(Ordering::Acquire) {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("approved tool execution");
}

async fn wait_for_session_not_running(engine: &Engine, session_id: SessionId) {
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if engine
                .get_session(session_id)
                .is_ok_and(|meta| meta.status != SessionStatus::Running)
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("session completion");
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
    assert_eq!(
        snapshot
            .agents
            .iter()
            .filter(|agent| agent.mode == cookie_agent_protocol::AgentMode::Internal)
            .count(),
        3
    );
    assert!(!snapshot.agents.iter().any(|agent| agent.runnable_as_root));
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
fn available_models_synthesize_default_agent_and_admit_sessions() {
    let fixture = synthetic_default_fixture(None);
    let snapshot = fixture.engine.runtime_snapshot().expect("runtime").snapshot;
    assert_eq!(snapshot.models.len(), 2);
    assert_eq!(snapshot.agents.len(), 4);
    let agent = snapshot
        .agents
        .iter()
        .find(|agent| agent.id.as_str() == "default")
        .expect("default agent");
    assert_eq!(agent.id.as_str(), "default");
    assert!(agent.runnable_as_root);
    assert_eq!(agent.resolved_fallback.len(), 1);
    assert_eq!(
        agent.resolved_fallback[0].model.to_string(),
        "custom.test/a-model"
    );
    assert_eq!(
        agent.resolved_fallback[0]
            .variant
            .as_ref()
            .map(|variant| variant.as_str()),
        Some("precise")
    );
    let session = fixture
        .engine
        .create_session(RunSelection {
            agent: agent.id.clone(),
            model: agent.resolved_fallback[0].clone(),
        })
        .expect("synthetic-agent session");
    let frozen = fixture
        .engine
        .inner
        .store
        .get(session.session_id)
        .expect("stored session")
        .creation_agent;
    assert_eq!(
        frozen.document_source,
        cookie_agent_protocol::AgentDocumentSource::BuiltIn
    );
    assert_eq!(
        frozen.tools,
        vec![
            cookie_agent_protocol::ToolName::Read,
            cookie_agent_protocol::ToolName::Write,
            cookie_agent_protocol::ToolName::Edit,
            cookie_agent_protocol::ToolName::Bash,
        ]
    );
    assert!(frozen.delegation.is_none());
    assert!(frozen.permissions.iter().any(|rule| {
        rule.action == PermissionAction::Read
            && rule.resource.as_str() == "store-v3.json"
            && rule.effect == cookie_agent_protocol::PermissionEffect::Deny
    }));
    assert!(frozen.permissions.iter().any(|rule| {
        rule.action == PermissionAction::Write
            && rule.resource.as_str() == "*"
            && rule.effect == cookie_agent_protocol::PermissionEffect::Ask
    }));
    for (action, resource, expected) in [
        (
            PermissionAction::Read,
            ".env",
            cookie_agent_protocol::PermissionEffect::Deny,
        ),
        (
            PermissionAction::Read,
            "nested/.env.local",
            cookie_agent_protocol::PermissionEffect::Deny,
        ),
        (
            PermissionAction::Read,
            ".env.example",
            cookie_agent_protocol::PermissionEffect::Allow,
        ),
        (
            PermissionAction::Read,
            "nested/.env.example",
            cookie_agent_protocol::PermissionEffect::Allow,
        ),
        (
            PermissionAction::Read,
            "store-v3.json",
            cookie_agent_protocol::PermissionEffect::Deny,
        ),
        (
            PermissionAction::Read,
            "nested/token-v1",
            cookie_agent_protocol::PermissionEffect::Deny,
        ),
        (
            PermissionAction::Read,
            "id_ed25519",
            cookie_agent_protocol::PermissionEffect::Deny,
        ),
        (
            PermissionAction::Read,
            ".netrc",
            cookie_agent_protocol::PermissionEffect::Deny,
        ),
        (
            PermissionAction::Read,
            "application_default_credentials.json",
            cookie_agent_protocol::PermissionEffect::Deny,
        ),
        (
            PermissionAction::Read,
            "src/lib.rs",
            cookie_agent_protocol::PermissionEffect::Allow,
        ),
        (
            PermissionAction::Write,
            "src/lib.rs",
            cookie_agent_protocol::PermissionEffect::Ask,
        ),
        (
            PermissionAction::Bash,
            "cargo test",
            cookie_agent_protocol::PermissionEffect::Ask,
        ),
        (
            PermissionAction::Delegate,
            "worker",
            cookie_agent_protocol::PermissionEffect::Ask,
        ),
    ] {
        assert_eq!(
            crate::permissions::effective_permission(
                &frozen,
                action,
                resource,
                fixture.engine.inner.store.cwd(),
            )
            .0,
            expected,
            "{action:?} {resource}"
        );
    }
}

#[test]
fn runtime_snapshot_model_descriptor_preserves_compiled_variant_order() {
    let fixture = synthetic_default_fixture(None);
    let snapshot = fixture.engine.runtime_snapshot().expect("runtime").snapshot;
    let descriptor = snapshot
        .models
        .iter()
        .find(|model| model.key.to_string() == "custom.test/a-model")
        .expect("runtime model descriptor");
    let runtime = fixture.manager.current();
    let compiled = runtime
        .models()
        .get(&descriptor.key)
        .expect("compiled runtime model");

    assert_eq!(descriptor.variant_order, compiled.model.variant_order);
}

#[test]
fn synthetic_default_replaces_no_authored_agent_and_unrunnable_authored_agents_only() {
    let unrunnable = synthetic_default_fixture(Some(
        "---\nschema: 4\ndescription: Unrunnable primary\nmode: primary\nenabled: true\nmodel_fallback: [{ model: \"custom.test/missing\", variant: base }]\ntools: []\npermissions: {}\n---\nUnrunnable prompt.\n",
    ));
    let snapshot = unrunnable
        .engine
        .runtime_snapshot()
        .expect("runtime")
        .snapshot;
    assert_eq!(snapshot.agents.len(), 5);
    assert!(
        snapshot
            .agents
            .iter()
            .any(|agent| agent.id.as_str() == "default" && agent.runnable_as_root)
    );
    assert!(
        snapshot
            .agents
            .iter()
            .any(|agent| agent.id.as_str() == "primary" && !agent.runnable_as_root)
    );

    let (runnable, _) = custom_fixture();
    let snapshot = runnable
        .engine
        .runtime_snapshot()
        .expect("runtime")
        .snapshot;
    assert!(
        snapshot
            .agents
            .iter()
            .any(|agent| agent.id.as_str() == "primary")
    );
    assert!(
        !snapshot
            .agents
            .iter()
            .any(|agent| agent.id.as_str() == "default")
    );
}

#[test]
fn shared_project_cwd_creates_and_reopens_model_manifests() {
    let fixture = fixture();
    let workspace = fixture._directory.path().join("shared-workspace");
    fs::create_dir(&workspace).expect("shared workspace");
    fs::set_permissions(&workspace, fs::Permissions::from_mode(0o775))
        .expect("shared workspace mode");
    let data_dir = fixture._directory.path().join("shared-data");

    let engine = Engine::open(EngineOptions {
        data_dir: data_dir.clone(),
        cwd: workspace.clone(),
        config: fixture.config.clone(),
        model_manager: Arc::clone(&fixture.manager),
        tools: Vec::new(),
    })
    .expect("engine in shared workspace");
    let revision = engine
        .runtime_snapshot()
        .expect("runtime snapshot")
        .snapshot
        .model_revision;
    drop(engine);

    let snapshots = workspace.join(".cookie-agent/model-snapshots");
    assert_eq!(
        fs::metadata(&snapshots).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert!(fs::read_dir(&snapshots).unwrap().any(|entry| {
        entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .ends_with(".json")
    }));

    let reopened = Engine::open(EngineOptions {
        data_dir,
        cwd: workspace,
        config: fixture.config.clone(),
        model_manager: Arc::clone(&fixture.manager),
        tools: Vec::new(),
    })
    .expect("reopened engine in shared workspace");
    assert_eq!(
        reopened
            .runtime_snapshot()
            .expect("reopened runtime snapshot")
            .snapshot
            .model_revision,
        revision
    );
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
    assert!(
        connected
            .runtime
            .agents
            .iter()
            .any(|agent| agent.runnable_as_root)
    );

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
    let drifted = fixture
        .engine
        .refresh_catalog(Arc::new(drifted))
        .expect("provider drift refresh")
        .snapshot;
    assert_eq!(drifted.catalog_state.provider_quarantine_count, 0);
    assert_eq!(drifted.catalog_state.model_quarantine_count, 0);
    assert_eq!(
        drifted.providers[0].support.state,
        cookie_agent_protocol::ProviderSupportState::Supported
    );
    assert_eq!(
        drifted.providers[0]
            .support
            .reason
            .as_ref()
            .map(cookie_agent_protocol::SafeCode::as_str),
        None
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
    record.environment.clear();
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
    assert_eq!(snapshot.catalog_state.model_quarantine_count, 0);
    assert!(snapshot.models.is_empty());
}

#[test]
fn nested_endpoint_placeholders_project_setup_and_secret_classification() {
    let fixture = fixture();
    let mut catalog = (*bedrock_catalog()).clone();
    let provider = catalog
        .providers
        .values_mut()
        .next()
        .unwrap()
        .record
        .as_mut()
        .unwrap();
    provider.models.values_mut().next().unwrap().record.as_mut().unwrap().provider = Some(
        cookie_agent_models::catalog::CatalogModelProviderMetadata {
            npm: Some("@ai-sdk/anthropic".to_owned()),
            api: Some("https://${AZURE_COGNITIVE_SERVICES_RESOURCE_NAME}.example/${SERVICE_TOKEN}/anthropic/v1".to_owned()),
            shape: None,
        },
    );
    let snapshot = fixture
        .engine
        .refresh_catalog(Arc::new(catalog))
        .expect("nested placeholder refresh")
        .snapshot;
    let fields = &snapshot.providers[0].setup_fields;
    assert!(fields.iter().any(|field| {
        field.id.as_str() == "azure_cognitive_services_resource_name" && field.safe_to_project
    }));
    assert!(
        fields
            .iter()
            .any(|field| { field.id.as_str() == "service_token" && !field.safe_to_project })
    );
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
    assert_eq!(first.catalog_state.model_quarantine_count, 2);

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
fn event_schema_thirteen_persistence_fails_deserialization() {
    let directory = TempDir::new().expect("temp directory");
    let path = directory.path().join("events.jsonl");
    fs::write(
        &path,
        b"{\"event_schema_version\":13,\"payload\":{\"type\":\"session_created\"}}\n",
    )
    .expect("legacy event");
    let error = EventLog::open(path, SessionId::new_v7()).expect_err("version 13 rejected");
    assert!(matches!(error, EventLogError::Json { .. }));
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
async fn internal_agent_ask_transaction_persists_escalation_and_pending_approval() {
    let (endpoint, captured) = scripted_approval_server(r#"{"decision":"ask"}"#).await;
    let (fixture, selection) = approval_fixture_with_endpoint(&endpoint);
    let executed = Arc::new(AtomicBool::new(false));
    fixture
        .engine
        .register_tool_provider(Arc::new(TestWriteProvider {
            executed: Arc::clone(&executed),
        }));
    let session = fixture
        .engine
        .create_session(selection.clone())
        .expect("approval session");
    fixture
        .engine
        .start_run(RunStartParams {
            session_id: session.session_id,
            client_run_id: cookie_agent_protocol::ClientRunId::new("ask-transaction")
                .expect("run ID"),
            selection,
            input: "request the write tool".to_owned(),
        })
        .await
        .expect("accepted approval run");

    let approval = wait_for_escalated_approval(&fixture.engine, session.session_id).await;
    let approval_id = approval.request.approval_id();
    let events = fixture
        .engine
        .inner
        .store
        .get(session.session_id)
        .expect("approval projection")
        .log
        .events();
    let lifecycle = events
        .iter()
        .filter(|event| match &event.payload {
            EventPayload::ApprovalRequested { request } => request.approval_id() == approval_id,
            EventPayload::ApprovalEvaluated {
                approval_id: event_approval_id,
                ..
            }
            | EventPayload::ApprovalEscalated {
                approval_id: event_approval_id,
                ..
            } => *event_approval_id == approval_id,
            _ => false,
        })
        .collect::<Vec<_>>();
    assert_eq!(lifecycle.len(), 3);
    assert!(matches!(
        lifecycle[0].payload,
        EventPayload::ApprovalRequested { .. }
    ));
    assert!(matches!(
        &lifecycle[1].payload,
        EventPayload::ApprovalEvaluated {
            decision,
            ..
        }
            if decision.decision == ApprovalInternalDecisionKind::Escalate
                && decision.source == ApprovalDecisionSource::InternalAgent
                && decision.reason_code == ApprovalReasonCode::Escalated
    ));
    assert!(matches!(
        lifecycle[2].payload,
        EventPayload::ApprovalEscalated { .. }
    ));
    assert!(
        fixture
            .engine
            .inner
            .pending_approvals
            .lock()
            .expect("pending approvals lock")
            .contains_key(&(session.session_id, approval_id))
    );
    assert_eq!(
        fixture
            .engine
            .list_approvals(session.session_id, Some(ApprovalStatus::Escalated))
            .approvals
            .len(),
        1
    );
    assert!(!events.iter().any(|event| matches!(
        &event.payload,
        EventPayload::ToolCallTerminated { termination }
            if termination.outcome == ToolTerminationOutcome::Failed
    )));

    approve_once(&fixture.engine, &approval, "ask-transaction-approval").await;
    wait_for_tool_execution(&executed).await;
    captured.abort();
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn pending_steering_promotes_after_tools_and_compaction_in_admission_order() {
    let bodies = vec![
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"write-call\",\"type\":\"function\",\"function\":{\"name\":\"write\",\"arguments\":\"{}\"}}]},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}],\"usage\":{\"prompt_tokens\":4000,\"completion_tokens\":1,\"total_tokens\":4001}}\n\n".to_owned(),
        "data: {\"choices\":[{\"delta\":{\"content\":\"{\\\"decision\\\":\\\"ask\\\"}\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n".to_owned(),
        "data: {\"choices\":[{\"delta\":{\"content\":\"compacted before steering\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n".to_owned(),
        "data: {\"choices\":[{\"delta\":{\"content\":\"continued after steering\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n".to_owned(),
    ];
    let (endpoint, captured, compaction_reached, release_compaction) =
        scripted_server_with_delayed_response(bodies, 2).await;
    let (fixture, selection) = custom_fixture_with_endpoint_primary_and_internal(
        &endpoint,
        "---\nschema: 4\ndescription: Steering compaction test\nmode: primary\nenabled: true\nmodel_fallback: [{ model: \"custom.test/group/model\", variant: base }]\ntools: [write]\npermissions:\n  write: ask\n---\nTest steering compaction.\n",
        None,
        Some(500),
    );
    let executed = Arc::new(AtomicBool::new(false));
    fixture
        .engine
        .register_tool_provider(Arc::new(TestWriteProvider {
            executed: Arc::clone(&executed),
        }));
    let session = fixture
        .engine
        .create_session(selection.clone())
        .expect("steering session");
    let run = fixture
        .engine
        .start_run(RunStartParams {
            session_id: session.session_id,
            client_run_id: cookie_agent_protocol::ClientRunId::new("steering-compaction")
                .expect("run ID"),
            selection,
            input: "begin".into(),
        })
        .await
        .expect("started steering run");
    let approval = wait_for_escalated_approval(&fixture.engine, session.session_id).await;
    assert!(
        fixture
            .engine
            .steer(run.run_id, "recall me".into())
            .await
            .expect("first admission")
            .accepted
    );
    assert_eq!(
        fixture
            .engine
            .recall_steer(run.run_id)
            .await
            .expect("recall pending input")
            .recalled
            .as_deref(),
        Some("recall me")
    );
    assert_eq!(
        fixture
            .engine
            .recall_steer(run.run_id)
            .await
            .expect("empty recall")
            .recalled,
        None
    );
    let first_pending = "first pending input with enough additional text to cross the learned predictive compaction threshold";
    for input in [first_pending, "second pending", "third pending"] {
        assert!(
            fixture
                .engine
                .steer(run.run_id, input.into())
                .await
                .expect("admission")
                .accepted
        );
    }
    assert_eq!(
        fixture
            .engine
            .recall_steer(run.run_id)
            .await
            .expect("LIFO recall")
            .recalled
            .as_deref(),
        Some("third pending")
    );
    assert!(
        fixture
            .engine
            .steer(run.run_id, "third pending".into())
            .await
            .expect("replacement admission")
            .accepted
    );
    let before_boundary = fixture
        .engine
        .inner
        .store
        .get(session.session_id)
        .expect("pending projection")
        .log
        .events();
    assert!(before_boundary.iter().any(|event| matches!(
        &event.payload,
        EventPayload::UserInputAdmitted { input } if input == first_pending
    )));
    assert!(!before_boundary.iter().any(|event| matches!(
        &event.payload,
        EventPayload::UserInputSubmitted { input } if input != "begin"
    )));
    approve_once(&fixture.engine, &approval, "steering-race-approval").await;
    wait_for_tool_execution(&executed).await;
    compaction_reached
        .await
        .expect("promotion compaction started");
    let during_reservation = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        fixture.engine.steer(run.run_id, "fourth pending".into()),
    )
    .await
    .expect("steer is not blocked by compaction")
    .expect("steer during compaction");
    assert!(during_reservation.accepted);
    release_compaction.notify_one();
    let requests = captured.await.expect("steering server task");
    assert_eq!(requests.len(), 4);
    assert!(!requests[0].contains("first pending"));
    for input in [first_pending, "third pending", "fourth pending"] {
        assert!(requests[3].contains(input));
    }
    assert!(!requests[3].contains("recall me"));
    let events = fixture
        .engine
        .inner
        .store
        .get(session.session_id)
        .expect("steering projection")
        .log
        .events();
    let checkpoint_seq = events
        .iter()
        .find_map(|event| {
            matches!(
                event.payload,
                EventPayload::ContextCheckpointCommitted { .. }
            )
            .then_some(event.seq)
        })
        .expect("predictive checkpoint");
    let tool_result_seq = events
        .iter()
        .find_map(|event| {
            matches!(event.payload, EventPayload::ToolCallTerminated { .. }).then_some(event.seq)
        })
        .expect("tool result");
    let submitted = events
        .iter()
        .filter_map(|event| match &event.payload {
            EventPayload::UserInputSubmitted { input } if input != "begin" => {
                Some((event.seq, input.as_str()))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        submitted
            .iter()
            .map(|(_, input)| *input)
            .collect::<Vec<_>>(),
        vec![
            first_pending,
            "second pending",
            "third pending",
            "fourth pending"
        ]
    );
    let first_steering_seq = submitted[0].0;
    let next_attempt_seq = events
        .iter()
        .find_map(|event| {
            (event.seq > first_steering_seq
                && matches!(event.payload, EventPayload::ModelAttemptStarted { .. }))
            .then_some(event.seq)
        })
        .expect("next model request");
    assert!(
        tool_result_seq < checkpoint_seq
            && checkpoint_seq < first_steering_seq
            && submitted.last().expect("submitted inputs").0 < next_attempt_seq
    );
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn cancel_during_start_prediction_aborts_compaction_without_appending_input() {
    let bodies = vec![
        "data: {\"choices\":[{\"delta\":{\"content\":\"first run complete\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":4000,\"completion_tokens\":1,\"total_tokens\":4001}}\n\n".to_owned(),
        "data: {\"choices\":[{\"delta\":{\"content\":\"late summary\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n".to_owned(),
    ];
    let (endpoint, captured, compaction_reached, release_compaction) =
        scripted_server_with_delayed_response(bodies, 1).await;
    let (fixture, selection) = custom_fixture_with_endpoint_primary_and_internal(
        &endpoint,
        "---\nschema: 4\ndescription: Start cancellation test\nmode: primary\nenabled: true\nmodel_fallback: [{ model: \"custom.test/group/model\", variant: base }]\ntools: []\npermissions: {}\n---\nTest start cancellation.\n",
        None,
        Some(500),
    );
    let session = fixture
        .engine
        .create_session(selection.clone())
        .expect("cancellation session");
    fixture
        .engine
        .start_run(RunStartParams {
            session_id: session.session_id,
            client_run_id: ClientRunId::new("prime-predictor").expect("client run ID"),
            selection: selection.clone(),
            input: "prime predictor".into(),
        })
        .await
        .expect("first run started");
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if fixture
                .engine
                .inner
                .store
                .get(session.session_id)
                .expect("first run projection")
                .status
                != SessionStatus::Running
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("first run completed");

    let start_engine = fixture.engine.clone();
    let second_selection = selection.clone();
    let start = tokio::spawn(async move {
        start_engine
            .start_run(RunStartParams {
                session_id: session.session_id,
                client_run_id: ClientRunId::new("cancel-prediction").expect("client run ID"),
                selection: second_selection,
                input: "must never be appended".into(),
            })
            .await
    });
    compaction_reached.await.expect("start compaction reached");
    let run = fixture
        .engine
        .inner
        .store
        .get(session.session_id)
        .expect("start projection")
        .log
        .events()
        .iter()
        .rev()
        .find_map(|event| {
            matches!(event.payload, EventPayload::RunStarted { .. }).then_some(event.run_id)
        })
        .flatten()
        .expect("second run ID");
    assert!(fixture.engine.run_active_for_test(run));
    assert!(
        fixture
            .engine
            .compaction_reserved_for_test(session.session_id)
    );
    fixture
        .engine
        .cancel_run(run)
        .await
        .expect("cancel during prediction");
    assert_eq!(
        start
            .await
            .expect("start task")
            .expect("cancelled start result")
            .run_id,
        run
    );
    release_compaction.notify_one();
    let events = fixture
        .engine
        .inner
        .store
        .get(session.session_id)
        .expect("cancelled projection")
        .log
        .events();
    assert!(events.iter().any(|event| {
        event.run_id == Some(run)
            && matches!(event.payload, EventPayload::InternalAgentCancelled { .. })
    }));
    assert!(events.iter().any(|event| {
        event.run_id == Some(run) && matches!(event.payload, EventPayload::RunCancelled { .. })
    }));
    assert!(!events.iter().any(|event| {
        event.run_id == Some(run)
            && matches!(
                &event.payload,
                EventPayload::UserInputSubmitted { input } if input == "must never be appended"
            )
    }));
    assert!(!fixture.engine.run_active_for_test(run));
    assert!(
        !fixture
            .engine
            .compaction_reserved_for_test(session.session_id)
    );
    assert_eq!(captured.await.expect("cancel server task").len(), 2);
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn steer_during_start_prediction_survives_initial_submission_and_reaches_model() {
    let bodies = vec![
        "data: {\"choices\":[{\"delta\":{\"content\":\"prime complete\"},\"finish_reason\":null}],\"usage\":{\"prompt_tokens\":4000,\"completion_tokens\":1,\"total_tokens\":4001}}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n".to_owned(),
        "data: {\"choices\":[{\"delta\":{\"content\":\"start-time summary\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n".to_owned(),
        "data: {\"choices\":[{\"delta\":{\"content\":\"initial turn\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n".to_owned(),
        "data: {\"choices\":[{\"delta\":{\"content\":\"steered turn\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n".to_owned(),
    ];
    let (endpoint, captured, compaction_reached, release_compaction) =
        scripted_server_with_delayed_response(bodies, 1).await;
    let (fixture, selection) = custom_fixture_with_endpoint_primary_and_internal(
        &endpoint,
        "---\nschema: 4\ndescription: Start steering race test\nmode: primary\nenabled: true\nmodel_fallback: [{ model: \"custom.test/group/model\", variant: base }]\ntools: []\npermissions: {}\n---\nTest start steering.\n",
        None,
        Some(500),
    );
    let session = fixture
        .engine
        .create_session(selection.clone())
        .expect("steering race session");
    fixture
        .engine
        .start_run(RunStartParams {
            session_id: session.session_id,
            client_run_id: ClientRunId::new("prime-start-steer").expect("client run ID"),
            selection: selection.clone(),
            input: "prime predictor".into(),
        })
        .await
        .expect("prime run");
    wait_for_session_not_running(&fixture.engine, session.session_id).await;

    let start_engine = fixture.engine.clone();
    let start = tokio::spawn(async move {
        start_engine
            .start_run(RunStartParams {
                session_id: session.session_id,
                client_run_id: ClientRunId::new("start-steer-race").expect("client run ID"),
                selection,
                input: "initial second-run input".into(),
            })
            .await
    });
    compaction_reached.await.expect("start compaction reached");
    let run = fixture
        .engine
        .inner
        .store
        .get(session.session_id)
        .expect("start projection")
        .log
        .events()
        .iter()
        .rev()
        .find_map(|event| {
            matches!(event.payload, EventPayload::RunStarted { .. }).then_some(event.run_id)
        })
        .flatten()
        .expect("second run ID");
    let steering = "steer admitted before initial submission";
    assert!(
        fixture
            .engine
            .steer(run, steering.into())
            .await
            .expect("steer during start compaction")
            .accepted
    );
    let during_compaction = fixture
        .engine
        .inner
        .store
        .get(session.session_id)
        .expect("admitted projection")
        .log
        .events();
    assert!(during_compaction.iter().any(|event| matches!(
        &event.payload,
        EventPayload::UserInputAdmitted { input } if input == steering
    )));
    assert!(!during_compaction.iter().any(|event| {
        event.run_id == Some(run)
            && matches!(event.payload, EventPayload::UserInputSubmitted { .. })
    }));
    release_compaction.notify_one();
    assert_eq!(
        start
            .await
            .expect("start task")
            .expect("started run")
            .run_id,
        run
    );
    wait_for_session_not_running(&fixture.engine, session.session_id).await;

    let requests = captured.await.expect("scripted requests");
    assert_eq!(requests.len(), 4);
    assert!(requests[2].contains("initial second-run input"));
    assert!(!requests[2].contains(steering));
    assert!(requests[3].contains("initial second-run input"));
    assert!(requests[3].contains(steering));
    let events = fixture
        .engine
        .inner
        .store
        .get(session.session_id)
        .expect("completed projection")
        .log
        .events();
    let submissions = events
        .iter()
        .filter_map(|event| match &event.payload {
            EventPayload::UserInputSubmitted { input } if event.run_id == Some(run) => {
                Some(input.as_str())
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(submissions, vec!["initial second-run input", steering]);
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn repeated_approvals_remain_stateless_and_reuse_the_user_request_prefix() {
    let (endpoint, captured) = scripted_two_approved_writes_server().await;
    let (fixture, selection) = custom_fixture_with_endpoint_primary_and_internal(
        &endpoint,
        "---\nschema: 4\ndescription: Approval test agent\nmode: primary\nenabled: true\nmodel_fallback: [{ model: \"custom.test/group/model\", variant: base }]\ntools: [write]\npermissions:\n  write: ask\n---\nTest approval flow.\n",
        Some((
            "approval.md",
            "---\nschema: 4\ndescription: Persistent approval evaluator\nmode: internal\nenabled: true\nmodel_fallback: [{ model: \"${parent_model}\" }]\nlimits: { timeout_ms: 30000, max_input_tokens: 4096, max_output_tokens: 128 }\ntools: []\npermissions: {}\n---\nEvaluate approval requests conservatively.\n",
        )),
        None,
    );
    let executed = Arc::new(AtomicBool::new(false));
    fixture
        .engine
        .register_tool_provider(Arc::new(TestWriteProvider {
            executed: Arc::clone(&executed),
        }));
    let session = fixture
        .engine
        .create_session(selection.clone())
        .expect("persistent approval session");
    fixture
        .engine
        .start_run(RunStartParams {
            session_id: session.session_id,
            client_run_id: cookie_agent_protocol::ClientRunId::new("persistent-approval")
                .expect("run ID"),
            selection,
            input: "request two writes".to_owned(),
        })
        .await
        .expect("accepted persistent approval run");

    let completed = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if fixture
                .engine
                .inner
                .store
                .get(session.session_id)
                .expect("projection")
                .status
                == SessionStatus::Completed
            {
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
            .get(session.session_id)
            .expect("timed-out projection");
        panic!(
            "stateless approval completion timed out with status {:?} and events {:#?}",
            projection.status,
            projection.log.events()
        );
    }
    assert!(executed.load(Ordering::Acquire));

    let events = fixture
        .engine
        .inner
        .store
        .get(session.session_id)
        .expect("projection")
        .log
        .events();
    let evaluations = events
        .iter()
        .filter(|event| matches!(event.payload, EventPayload::ApprovalEvaluated { .. }))
        .count();
    assert_eq!(evaluations, 2);
    let requests = captured.await.expect("persistent approval server task");
    assert_eq!(requests.len(), 5);
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn ask_permission_mode_escalates_without_starting_internal_approval_agent() {
    let (endpoint, captured) = scripted_approval_server(r#"{"decision":"allow"}"#).await;
    let (fixture, selection) = approval_fixture_with_endpoint(&endpoint);
    let executed = Arc::new(AtomicBool::new(false));
    fixture
        .engine
        .register_tool_provider(Arc::new(TestWriteProvider {
            executed: Arc::clone(&executed),
        }));
    let session = fixture
        .engine
        .create_session(selection.clone())
        .expect("session");
    fixture
        .engine
        .set_permission_mode(session.session_id, PermissionMode::Ask)
        .expect("ask mode");
    fixture
        .engine
        .start_run(RunStartParams {
            session_id: session.session_id,
            client_run_id: cookie_agent_protocol::ClientRunId::new("ask-mode").expect("run ID"),
            selection,
            input: "request the write tool".into(),
        })
        .await
        .expect("run");

    let approval = wait_for_escalated_approval(&fixture.engine, session.session_id).await;
    let events = fixture
        .engine
        .inner
        .store
        .get(session.session_id)
        .expect("projection")
        .log
        .events();
    assert!(!events.iter().any(|event| matches!(
        event.payload,
        EventPayload::InternalAgentStarted {
            kind: InternalAgentKind::Approval,
            ..
        }
    )));
    assert!(
        events
            .iter()
            .any(|event| matches!(event.payload, EventPayload::ApprovalEscalated { .. }))
    );
    approve_once(&fixture.engine, &approval, "ask-mode-approval").await;
    wait_for_tool_execution(&executed).await;
    captured.abort();
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn yolo_permission_mode_durably_approves_and_executes_without_escalation() {
    let (endpoint, captured) = scripted_approval_server(r#"{"decision":"deny"}"#).await;
    let (fixture, selection) = approval_fixture_with_endpoint(&endpoint);
    let executed = Arc::new(AtomicBool::new(false));
    fixture
        .engine
        .register_tool_provider(Arc::new(TestWriteProvider {
            executed: Arc::clone(&executed),
        }));
    let session = fixture
        .engine
        .create_session(selection.clone())
        .expect("session");
    fixture
        .engine
        .set_permission_mode(session.session_id, PermissionMode::Yolo)
        .expect("yolo mode");
    fixture
        .engine
        .start_run(RunStartParams {
            session_id: session.session_id,
            client_run_id: cookie_agent_protocol::ClientRunId::new("yolo-mode").expect("run ID"),
            selection,
            input: "request the write tool".into(),
        })
        .await
        .expect("run");
    wait_for_tool_execution(&executed).await;

    let events = fixture
        .engine
        .inner
        .store
        .get(session.session_id)
        .expect("projection")
        .log
        .events();
    let approval_lifecycle = events
        .iter()
        .filter(|event| {
            matches!(
                event.payload,
                EventPayload::ApprovalRequested { .. }
                    | EventPayload::ApprovalEvaluated { .. }
                    | EventPayload::ApprovalFinalized { .. }
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(approval_lifecycle.len(), 3);
    assert!(matches!(
        &approval_lifecycle[1].payload,
        EventPayload::ApprovalEvaluated {
            decision,
            ..
        }
            if decision.decision == ApprovalInternalDecisionKind::Allow
                && decision.source == ApprovalDecisionSource::Policy
                && decision.reason_code == ApprovalReasonCode::YoloApproved
    ));
    assert!(matches!(
        &approval_lifecycle[2].payload,
        EventPayload::ApprovalFinalized { decision, .. }
            if decision.outcome == ApprovalFinalOutcome::Approved
                && decision.source == ApprovalDecisionSource::Policy
                && decision.reason_code == ApprovalReasonCode::YoloApproved
    ));
    assert!(!events.iter().any(|event| matches!(
        event.payload,
        EventPayload::ApprovalEscalated { .. }
            | EventPayload::InternalAgentStarted {
                kind: InternalAgentKind::Approval,
                ..
            }
    )));
    captured.abort();
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn yolo_permission_mode_does_not_override_hard_deny_rules() {
    let (endpoint, captured) = scripted_approval_server(r#"{"decision":"allow"}"#).await;
    let (fixture, selection) = denied_approval_fixture_with_endpoint(&endpoint);
    let executed = Arc::new(AtomicBool::new(false));
    fixture
        .engine
        .register_tool_provider(Arc::new(TestWriteProvider {
            executed: Arc::clone(&executed),
        }));
    let session = fixture
        .engine
        .create_session(selection.clone())
        .expect("session");
    fixture
        .engine
        .set_permission_mode(session.session_id, PermissionMode::Yolo)
        .expect("yolo mode");
    fixture
        .engine
        .start_run(RunStartParams {
            session_id: session.session_id,
            client_run_id: cookie_agent_protocol::ClientRunId::new("yolo-deny").expect("run ID"),
            selection,
            input: "request the denied write tool".into(),
        })
        .await
        .expect("run");
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let events = fixture
        .engine
        .inner
        .store
        .get(session.session_id)
        .expect("projection")
        .log
        .events();
    assert!(!executed.load(Ordering::Acquire));
    assert!(!events.iter().any(|event| matches!(
        event.payload,
        EventPayload::ApprovalRequested { .. }
            | EventPayload::ApprovalEvaluated { .. }
            | EventPayload::ApprovalFinalized { .. }
            | EventPayload::ApprovalEscalated { .. }
    )));
    captured.abort();
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn yolo_permission_mode_still_triggers_the_doom_loop_guard() {
    let (endpoint, captured) = scripted_repeated_write_server(4).await;
    let (fixture, selection) = approval_fixture_with_endpoint(&endpoint);
    fixture
        .engine
        .register_tool_provider(Arc::new(TestWriteProvider {
            executed: Arc::new(AtomicBool::new(false)),
        }));
    let session = fixture
        .engine
        .create_session(selection.clone())
        .expect("session");
    fixture
        .engine
        .set_permission_mode(session.session_id, PermissionMode::Yolo)
        .expect("yolo mode");
    fixture
        .engine
        .start_run(RunStartParams {
            session_id: session.session_id,
            client_run_id: cookie_agent_protocol::ClientRunId::new("yolo-doom-loop")
                .expect("run ID"),
            selection,
            input: "repeat the same write".into(),
        })
        .await
        .expect("run");
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let events = fixture
                .engine
                .inner
                .store
                .get(session.session_id)
                .expect("projection")
                .log
                .events();
            if events.iter().any(|event| {
                matches!(
                    &event.payload,
                    EventPayload::ApprovalFinalized { decision, .. }
                        if decision.outcome == ApprovalFinalOutcome::Rejected
                            && decision.reason_code == ApprovalReasonCode::DoomLoopDetected
                )
            }) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("doom-loop rejection");
    let events = fixture
        .engine
        .inner
        .store
        .get(session.session_id)
        .expect("projection")
        .log
        .events();
    assert!(events.iter().any(|event| matches!(
        &event.payload,
        EventPayload::ApprovalFinalized { decision, .. }
            if decision.outcome == ApprovalFinalOutcome::Rejected
                && decision.reason_code == ApprovalReasonCode::DoomLoopDetected
    )));
    captured.abort();
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn permission_mode_change_applies_to_the_next_operation_only() {
    let (endpoint, captured) = scripted_repeated_write_server(2).await;
    let (fixture, selection) = approval_fixture_with_endpoint(&endpoint);
    let executed = Arc::new(AtomicBool::new(false));
    fixture
        .engine
        .register_tool_provider(Arc::new(TestWriteProvider {
            executed: Arc::clone(&executed),
        }));
    let session = fixture
        .engine
        .create_session(selection.clone())
        .expect("session");
    fixture
        .engine
        .set_permission_mode(session.session_id, PermissionMode::Ask)
        .expect("ask mode");
    fixture
        .engine
        .start_run(RunStartParams {
            session_id: session.session_id,
            client_run_id: cookie_agent_protocol::ClientRunId::new("live-mode-change")
                .expect("run ID"),
            selection,
            input: "perform two writes".into(),
        })
        .await
        .expect("run");

    let first = wait_for_escalated_approval(&fixture.engine, session.session_id).await;
    fixture
        .engine
        .set_permission_mode(session.session_id, PermissionMode::Yolo)
        .expect("yolo mode");
    assert_eq!(
        fixture
            .engine
            .list_approvals(session.session_id, Some(ApprovalStatus::Escalated))
            .approvals
            .len(),
        1
    );
    approve_once(&fixture.engine, &first, "live-mode-first").await;
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let events = fixture
                .engine
                .inner
                .store
                .get(session.session_id)
                .expect("projection")
                .log
                .events();
            if events.iter().any(|event| {
                matches!(
                    &event.payload,
                    EventPayload::ApprovalFinalized { decision, .. }
                        if decision.reason_code == ApprovalReasonCode::YoloApproved
                )
            }) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("next operation uses yolo");
    assert!(executed.load(Ordering::Acquire));
    let events = fixture
        .engine
        .inner
        .store
        .get(session.session_id)
        .expect("projection")
        .log
        .events();
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event.payload, EventPayload::ApprovalEscalated { .. }))
            .count(),
        1
    );
    captured.abort();
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn malformed_internal_approval_output_falls_back_to_escalation_transaction() {
    let (endpoint, captured) = scripted_approval_server("not-json").await;
    let (fixture, selection) = approval_fixture_with_endpoint(&endpoint);
    let executed = Arc::new(AtomicBool::new(false));
    fixture
        .engine
        .register_tool_provider(Arc::new(TestWriteProvider {
            executed: Arc::clone(&executed),
        }));
    let session = fixture
        .engine
        .create_session(selection.clone())
        .expect("approval session");
    fixture
        .engine
        .start_run(RunStartParams {
            session_id: session.session_id,
            client_run_id: cookie_agent_protocol::ClientRunId::new("malformed-approval")
                .expect("run ID"),
            selection,
            input: "request the write tool".to_owned(),
        })
        .await
        .expect("accepted approval run");

    let approval = wait_for_escalated_approval(&fixture.engine, session.session_id).await;
    let events = fixture
        .engine
        .inner
        .store
        .get(session.session_id)
        .expect("approval projection")
        .log
        .events();
    assert!(events.iter().any(|event| matches!(
        &event.payload,
        EventPayload::ApprovalEvaluated {
            approval_id,
            decision,
            ..
        }
            if *approval_id == approval.request.approval_id()
                && decision.decision == ApprovalInternalDecisionKind::Escalate
                && decision.source == ApprovalDecisionSource::InternalAgent
                && decision.reason_code == ApprovalReasonCode::Escalated
    )));

    approve_once(&fixture.engine, &approval, "malformed-approval-response").await;
    wait_for_tool_execution(&executed).await;
    captured.abort();
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn internal_agent_ask_escalates_to_user_approval_then_executes_tool() {
    let (endpoint, captured) = scripted_approval_server(r#"{"decision":"ask"}"#).await;
    let (fixture, selection) = approval_fixture_with_endpoint(&endpoint);
    let executed = Arc::new(AtomicBool::new(false));
    fixture
        .engine
        .register_tool_provider(Arc::new(TestWriteProvider {
            executed: Arc::clone(&executed),
        }));
    let session = fixture
        .engine
        .create_session(selection.clone())
        .expect("approval session");
    fixture
        .engine
        .start_run(RunStartParams {
            session_id: session.session_id,
            client_run_id: cookie_agent_protocol::ClientRunId::new("approval-e2e").expect("run ID"),
            selection,
            input: "request the write tool".to_owned(),
        })
        .await
        .expect("accepted approval run");

    let approval = wait_for_escalated_approval(&fixture.engine, session.session_id).await;
    let response = approve_once(&fixture.engine, &approval, "approval-e2e-response").await;
    assert_eq!(response.approval.status, ApprovalStatus::Approved);
    wait_for_tool_execution(&executed).await;

    let events = fixture
        .engine
        .inner
        .store
        .get(session.session_id)
        .expect("completed approval projection")
        .log
        .events();
    let approval_id = approval.request.approval_id();
    assert!(events.iter().any(|event| matches!(
        &event.payload,
        EventPayload::ApprovalUserDecisionRecorded { approval_id: event_id, decision, .. }
            if *event_id == approval_id && *decision == ApprovalUserDecision::ApproveOnce
    )));
    assert!(events.iter().any(|event| matches!(
        &event.payload,
        EventPayload::ApprovalFinalized { approval_id: event_id, decision }
            if *event_id == approval_id
                && decision.outcome == cookie_agent_protocol::ApprovalFinalOutcome::Approved
                && decision.source == ApprovalDecisionSource::User
                && decision.reason_code == ApprovalReasonCode::UserApprovedOnce
    )));
    assert!(events.iter().any(|event| matches!(
        &event.payload,
        EventPayload::ToolCallTerminated { termination }
            if termination.outcome == ToolTerminationOutcome::Completed
    )));
    captured.abort();
    fixture.engine.shutdown().await;
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
async fn revert_and_fork_preserve_prefix_context_replay_and_independence() {
    let response = |text: &str| {
        format!(
            "data: {{\"choices\":[{{\"delta\":{{\"content\":\"{text}\"}},\"finish_reason\":null}}]}}\n\ndata: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"stop\"}}]}}\n\n"
        )
    };
    let (endpoint, captured, second_request_reached, release_second) =
        scripted_server_with_delayed_response(
            vec![
                response("first answer"),
                response("second answer"),
                response("branch answer"),
            ],
            1,
        )
        .await;
    let (fixture, selection) = custom_fixture_with_endpoint(&endpoint);
    let session = fixture
        .engine
        .create_session(selection.clone())
        .expect("source session");

    fixture
        .engine
        .start_run(RunStartParams {
            session_id: session.session_id,
            client_run_id: ClientRunId::new("revert-first").expect("client run ID"),
            selection: selection.clone(),
            input: "first input".into(),
        })
        .await
        .expect("first run");
    wait_for_session_not_running(&fixture.engine, session.session_id).await;
    let through_seq = fixture
        .engine
        .inner
        .store
        .get(session.session_id)
        .expect("first projection")
        .log
        .all_events()
        .last()
        .expect("first tip")
        .seq;

    fixture
        .engine
        .start_run(RunStartParams {
            session_id: session.session_id,
            client_run_id: ClientRunId::new("revert-second").expect("client run ID"),
            selection: selection.clone(),
            input: "second input must disappear".into(),
        })
        .await
        .expect("second run");
    second_request_reached
        .await
        .expect("second request reached");
    assert!(matches!(
        fixture
            .engine
            .revert_session(session.session_id, through_seq)
            .await,
        Err(EngineError::SessionRunning(id)) if id == session.session_id
    ));
    let fork = fixture
        .engine
        .fork_session(session.session_id, through_seq)
        .await
        .expect("fork active source");
    let (artifact, digest) = fixture
        .engine
        .inner
        .artifacts
        .retain(b"fork-shared-artifact")
        .expect("retain shared artifact");
    assert_eq!(artifact.uri, format!("artifact://sha256/{digest}"));
    assert!(
        fixture
            .engine
            .inner
            .artifacts
            .open_existing(&digest)
            .expect("resolve shared artifact")
            .is_some()
    );
    let source_prefix = fixture
        .engine
        .inner
        .store
        .get(session.session_id)
        .expect("source prefix")
        .log
        .all_events()
        .into_iter()
        .filter(|event| event.seq <= through_seq)
        .collect::<Vec<_>>();
    let fork_physical = fixture
        .engine
        .inner
        .store
        .get(fork.session_id)
        .expect("fork projection")
        .log
        .all_events();
    assert_eq!(fork_physical.len(), source_prefix.len() + 2);
    for (source_event, fork_event) in source_prefix.iter().zip(&fork_physical) {
        assert_eq!(fork_event.session_id, fork.session_id);
        assert_eq!(
            fork_event.event_schema_version,
            source_event.event_schema_version
        );
        assert_eq!(fork_event.run_id, source_event.run_id);
        assert_eq!(fork_event.seq, source_event.seq);
        assert_eq!(fork_event.timestamp, source_event.timestamp);
        assert_eq!(fork_event.payload, source_event.payload);
    }
    assert!(matches!(
        fork_physical[source_prefix.len()].payload,
        EventPayload::SessionReverted { through_seq: target } if target == through_seq
    ));
    assert!(matches!(
        fork_physical[source_prefix.len() + 1].payload,
        EventPayload::SessionTitleCommitted { .. }
    ));
    release_second.notify_one();
    wait_for_session_not_running(&fixture.engine, session.session_id).await;

    let reverted = fixture
        .engine
        .revert_session(session.session_id, through_seq)
        .await
        .expect("revert completed source");
    let first_revert_event = fixture
        .engine
        .inner
        .store
        .get(session.session_id)
        .expect("reverted source")
        .log
        .all_events()
        .last()
        .expect("revert tip")
        .clone();
    assert!(matches!(
        first_revert_event.payload,
        EventPayload::SessionReverted { through_seq: target } if target == through_seq
    ));
    assert_eq!(reverted.session.last_event_seq, first_revert_event.seq);
    assert_eq!(reverted.session.last_activity, first_revert_event.timestamp);
    let first_revert_tip = first_revert_event.seq;
    fixture
        .engine
        .rename_session(cookie_agent_protocol::SessionRenameParams {
            session_id: session.session_id,
            client_rename_id: ClientRenameId::new("branch-title").expect("rename ID"),
            change: cookie_agent_protocol::SessionRenameChange::Set {
                title: SessionTitle::new("temporary branch").expect("title"),
            },
        })
        .await
        .expect("branch title");
    fixture
        .engine
        .revert_session(session.session_id, first_revert_tip)
        .await
        .expect("stacked revert");
    let fork_after_revert = fixture
        .engine
        .fork_session(session.session_id, through_seq)
        .await
        .expect("fork reverted source at original boundary");
    let first_fork_prefix = fixture
        .engine
        .inner
        .store
        .get(fork.session_id)
        .expect("first fork")
        .log
        .all_events()
        .into_iter()
        .filter(|event| event.seq <= through_seq)
        .collect::<Vec<_>>();
    let reverted_fork_prefix = fixture
        .engine
        .inner
        .store
        .get(fork_after_revert.session_id)
        .expect("fork after revert")
        .log
        .all_events()
        .into_iter()
        .filter(|event| event.seq <= through_seq)
        .collect::<Vec<_>>();
    assert_eq!(first_fork_prefix.len(), reverted_fork_prefix.len());
    for (first, second) in first_fork_prefix.iter().zip(&reverted_fork_prefix) {
        assert_eq!(first.event_schema_version, second.event_schema_version);
        assert_eq!(first.run_id, second.run_id);
        assert_eq!(first.seq, second.seq);
        assert_eq!(first.timestamp, second.timestamp);
        assert_eq!(first.payload, second.payload);
    }
    let visible = fixture
        .engine
        .inner
        .store
        .get(session.session_id)
        .expect("stacked projection")
        .log
        .events();
    assert!(visible.iter().all(|event| !matches!(
        &event.payload,
        EventPayload::UserInputSubmitted { input } if input == "second input must disappear"
    )));

    fixture
        .engine
        .start_run(RunStartParams {
            session_id: session.session_id,
            client_run_id: ClientRunId::new("revert-branch").expect("client run ID"),
            selection,
            input: "branch input".into(),
        })
        .await
        .expect("branch run");
    wait_for_session_not_running(&fixture.engine, session.session_id).await;
    let requests = captured.await.expect("scripted requests");
    assert_eq!(requests.len(), 3);
    assert!(requests[2].contains("first input"));
    assert!(requests[2].contains("branch input"));
    assert!(!requests[2].contains("second input must disappear"));

    let source_tip_before_fork_rename = fixture
        .engine
        .inner
        .store
        .get(session.session_id)
        .expect("source")
        .log
        .all_events()
        .len();
    let fork_meta = fixture
        .engine
        .get_session(fork.session_id)
        .expect("fork meta");
    assert!(
        fork_meta
            .title
            .is_some_and(|title| title.as_str().ends_with(" (fork)"))
    );
    fixture
        .engine
        .rename_session(cookie_agent_protocol::SessionRenameParams {
            session_id: fork.session_id,
            client_rename_id: ClientRenameId::new("fork-independent").expect("rename ID"),
            change: cookie_agent_protocol::SessionRenameChange::Set {
                title: SessionTitle::new("independent fork").expect("title"),
            },
        })
        .await
        .expect("rename fork");
    assert_eq!(
        fixture
            .engine
            .inner
            .store
            .get(session.session_id)
            .expect("unchanged source")
            .log
            .all_events()
            .len(),
        source_tip_before_fork_rename
    );

    fixture.engine.shutdown().await;
    let reopened = reopen_engine(&fixture);
    assert!(
        reopened
            .inner
            .artifacts
            .open_existing(&digest)
            .expect("resolve shared artifact after restart")
            .is_some()
    );
    let reopened_visible = reopened
        .inner
        .store
        .get(session.session_id)
        .expect("reopened source")
        .log
        .events();
    let reopened_physical_tip = reopened
        .inner
        .store
        .get(session.session_id)
        .expect("reopened physical source")
        .log
        .all_events()
        .last()
        .expect("reopened physical tip")
        .clone();
    let reopened_meta = reopened
        .get_session(session.session_id)
        .expect("reopened source metadata");
    assert_eq!(reopened_meta.last_event_seq, reopened_physical_tip.seq);
    assert_eq!(reopened_meta.last_activity, reopened_physical_tip.timestamp);
    assert!(reopened_visible.iter().any(|event| matches!(
        &event.payload,
        EventPayload::UserInputSubmitted { input } if input == "branch input"
    )));
    assert!(reopened_visible.iter().all(|event| !matches!(
        &event.payload,
        EventPayload::UserInputSubmitted { input } if input == "second input must disappear"
    )));
    assert_eq!(
        reopened
            .get_session(fork.session_id)
            .expect("reopened fork")
            .title
            .expect("fork title")
            .as_str(),
        "independent fork"
    );
    reopened.shutdown().await;
}

#[tokio::test]
async fn registered_external_tool_must_declare_resource_and_cannot_bypass_deny() {
    let (endpoint, captured) = scripted_zero_resource_tool_server().await;
    let (fixture, selection) = custom_fixture_with_endpoint_and_primary_agent(
        &endpoint,
        "---\nschema: 4\ndescription: Resource-bound test agent\nmode: primary\nenabled: true\nmodel_fallback: [{ model: \"custom.test/group/model\", variant: base }]\ntools: [write]\npermissions:\n  write: deny\n---\nReject denied tools.\n",
    );
    let executed = Arc::new(AtomicBool::new(false));
    fixture
        .engine
        .register_tool_provider(Arc::new(TestWriteProvider {
            executed: Arc::clone(&executed),
        }));
    let session = fixture
        .engine
        .create_session(selection.clone())
        .expect("zero-resource session");
    fixture
        .engine
        .start_run(RunStartParams {
            session_id: session.session_id,
            client_run_id: ClientRunId::new("zero-resource-run").expect("run ID"),
            selection,
            input: "attempt the write tool".into(),
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
                .expect("resource-bound projection");
            if projection.status == SessionStatus::Completed {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("zero-resource run completion");

    assert!(!executed.load(Ordering::Acquire));
    let events = fixture
        .engine
        .inner
        .store
        .get(session.session_id)
        .expect("completed resource-bound projection")
        .log
        .events();
    assert!(events.iter().any(|event| matches!(
        &event.payload,
        EventPayload::ToolCallTerminated { termination }
            if termination.outcome == ToolTerminationOutcome::Failed
                && termination.error.as_ref().is_some_and(|error| {
                    error.code.as_str() == "execution_failed"
                })
    )));
    assert_eq!(captured.await.expect("resource-bound server").len(), 2);
    fixture.engine.shutdown().await;
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
async fn root_run_and_schema_ten_delegation_reservation_reopen_exactly() {
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
        10
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

#[test]
fn test_providers_expose_mandatory_primary_arguments() {
    let write = TestWriteProvider {
        executed: Arc::new(AtomicBool::new(false)),
    };
    assert_eq!(
        write
            .get_primary_argument("write", &serde_json::json!({}))
            .expect("write primary"),
        "approval-test.txt"
    );
    let read = TestRehydrationReadProvider {
        executed: Arc::new(AtomicBool::new(false)),
        swap_after_prepare: false,
    };
    assert_eq!(
        read.get_primary_argument("read", &serde_json::json!({"filePath":"src/lib.rs"}))
            .expect("read primary"),
        "src/lib.rs"
    );
    assert!(
        read.get_primary_argument("read", &serde_json::json!({}))
            .is_err()
    );
    assert_eq!(
        write
            .get_simplified_argument("write", &serde_json::json!({}))
            .expect("write simplified"),
        "approval-test.txt"
    );
    assert_eq!(
        read.get_simplified_argument("read", &serde_json::json!({"filePath":"src/lib.rs"}))
            .expect("read simplified"),
        "src/lib.rs"
    );
}

struct DivergentReadProvider;

struct DivergentReadExecutor;

#[async_trait]
impl ToolProvider for DivergentReadProvider {
    fn tools_for_session(&self, _ctx: &SessionToolContext) -> Result<Vec<ToolSpec>, ToolError> {
        Ok(vec![ToolSpec {
            name: "read".into(),
            description: "Divergent prepared-label test".into(),
            parameters: serde_json::json!({
                "type":"object",
                "additionalProperties":false,
                "properties":{"filePath":{"type":"string"}},
                "required":["filePath"]
            }),
        }])
    }

    fn get_primary_argument(
        &self,
        name: &str,
        arguments: &serde_json::Value,
    ) -> Result<String, ToolError> {
        if name != "read" {
            return Err(ToolError::execution("read provider received another tool"));
        }
        arguments
            .get("filePath")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| ToolError::execution("missing filePath"))
    }

    fn get_simplified_argument(
        &self,
        name: &str,
        arguments: &serde_json::Value,
    ) -> Result<String, ToolError> {
        self.get_primary_argument(name, arguments)
    }

    async fn prepare(
        &self,
        _ctx: ToolPreparationContext,
        call: ToolCall,
    ) -> Result<PreparedTool, ToolError> {
        let raw = call
            .arguments
            .get("filePath")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| ToolError::execution("missing filePath"))?;
        let prepared_path = format!("canonical/{raw}");
        let operation = PreparedOperationIdentity::new(
            Sha256Digest::of_bytes(prepared_path.as_bytes()),
            vec![ApprovalCapability {
                action: PermissionAction::Read,
                operation: PreparedCapabilityOperation::new("read:file")
                    .map_err(|error| ToolError::execution(error.to_string()))?,
            }],
            vec![PreparedApprovalResource {
                capability: PermissionAction::Read,
                canonical: PreparedResourceIdentity::new(format!(
                    "file:{}",
                    Sha256Digest::of_bytes(b"divergent-raw")
                ))
                .map_err(|error| ToolError::execution(error.to_string()))?,
                binding_digest: PreparedResourceDigest::from_canonical_binding_bytes(b"divergent"),
                binding_lifetime: PreparedBindingLifetime::ProcessLocal,
                boundary: ApprovalBoundary::Exact,
                source: ApprovalResourceSource::PrimaryOperation,
            }],
            Sha256Digest::of_bytes(b"divergent context"),
        )
        .map_err(|error| ToolError::execution(error.to_string()))?;
        PreparedTool::new(
            operation,
            serde_json::json!({"filePath": prepared_path}),
            None,
            Box::new(DivergentReadExecutor),
        )?
        .with_policy_labels(vec!["divergent-raw".into()])
    }
}

#[async_trait]
impl PreparedExecutor for DivergentReadExecutor {
    async fn revalidate(&self) -> Result<(), ToolError> {
        Ok(())
    }

    async fn execute(
        self: Box<Self>,
        _context: ToolExecutionContext,
    ) -> Result<cookie_agent_protocol::PersistedToolResult, ToolError> {
        unreachable!("divergence test never executes")
    }
}

#[tokio::test]
async fn permission_labels_come_from_get_primary_argument_on_prepared_arguments() {
    let provider = DivergentReadProvider;
    let raw = serde_json::json!({"filePath":"src/lib.rs"});
    assert_eq!(
        provider
            .get_primary_argument("read", &raw)
            .expect("raw primary"),
        "src/lib.rs"
    );
    let prepared = provider
        .prepare(
            ToolPreparationContext {
                session: SessionId::new_v7(),
                run: cookie_agent_protocol::RunId::new_v7(),
                cwd: "/tmp".into(),
                workspace_root: "/tmp".into(),
            },
            ToolCall {
                id: ToolCallId::new_v7(),
                name: "read".into(),
                arguments: raw,
            },
        )
        .await
        .expect("prepare");
    assert_eq!(prepared.policy_labels(), ["divergent-raw"]);
    let labeled =
        crate::runtime::tool_execution::apply_primary_argument_labels(&provider, "read", prepared)
            .expect("overwrite");
    assert_eq!(
        provider
            .get_primary_argument("read", labeled.normalized_arguments())
            .expect("prepared primary"),
        "canonical/src/lib.rs"
    );
    assert_eq!(labeled.policy_labels(), ["canonical/src/lib.rs"]);
}
