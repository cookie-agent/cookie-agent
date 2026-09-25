//! Shared fixtures, helpers and doubles for the runtime tests.

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt as _;

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    fs,
    ops::Deref,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

use async_trait::async_trait;

use cookie_agent_config::{
    ApprovalConfig, ContextCompactionConfig, EngineConfig, LoadedConfiguration, LoadedMcpServer,
    McpServerConfig, McpServerSource, ModelRetryConfig, PluginConfig, ServerConfig,
    SessionTitleConfig, ToolOutputConfig, load_from_roots,
};

use cookie_agent_models::{
    ModelManager, ProviderDefinition,
    catalog::{
        CatalogAgeState, CatalogAvailability, CatalogLimits, CatalogModalities, CatalogModelEntry,
        CatalogModelRecord, CatalogModelStatus, CatalogProviderEntry, CatalogProviderRecord,
        CatalogRuntimeState, CatalogSnapshot, CatalogSource,
    },
    provider_store::ProviderStore,
};

use cookie_agent_protocol::{
    AgentId, ApprovalBoundary, ApprovalCapability, ApprovalId, ApprovalResourceSource,
    ApprovalRespondParams, ApprovalStatus, ApprovalUserDecision, CatalogRevision, ClientResponseId,
    ClientRunId, EventPayload, EventSubscriptionMessage, InvocationId, ModelSelection,
    PermissionAction, PermissionEffect, PreparedApprovalResource, PreparedBindingLifetime,
    PreparedCapabilityOperation, PreparedOperationIdentity, PreparedResourceDigest,
    PreparedResourceIdentity, ProviderId, ProviderModelId, RunSelection, RunStartParams, SessionId,
    SessionStatus, Sha256Digest, ToolCallId, ToolTerminationOutcome, WildcardPattern,
};

use jiff::Timestamp;

use tempfile::TempDir;

use crate::{
    DelegateInvocation, Engine, EngineError, EngineOptions, PreparedExecutor,
    PreparedSerializationKey, PreparedTool, PromptSection, SessionToolContext, ToolCall,
    ToolConcurrency, ToolError, ToolExecutionContext, ToolPreparationContext, ToolProgress,
    ToolProvider, ToolSpec, TurnAgentContext, runtime::ModelRetrySleepMode,
};

pub(crate) mod providers;
pub(crate) mod scripted;

pub(crate) use providers::*;
pub(crate) use scripted::*;

pub(crate) const PLUGIN_FIXTURE: &str =
    concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/fake_plugin.py");

pub(crate) fn private_tempdir() -> PanicResistantTempDir {
    let directory = TempDir::new().expect("temp directory");
    #[cfg(unix)]
    fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700))
        .expect("private temp directory");
    #[cfg(windows)]
    {
        fs::remove_dir(directory.path()).expect("remove ordinary temp directory");
        cookie_agent_models::secure_store::SecureDirectory::open(directory.path())
            .expect("private temp directory");
    }
    PanicResistantTempDir(Some(directory))
}

pub(crate) fn create_private_test_dir(path: &std::path::Path) {
    #[cfg(unix)]
    {
        fs::create_dir(path).expect("private test directory");
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .expect("private test directory");
    }
    #[cfg(windows)]
    cookie_agent_models::secure_store::SecureDirectory::open(path).expect("private test directory");
}

pub(crate) fn write_private_test_file(path: &std::path::Path, contents: impl AsRef<[u8]>) {
    #[cfg(unix)]
    {
        use std::{fs::OpenOptions, io::Write as _, os::unix::fs::OpenOptionsExt as _};

        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(path)
            .expect("private test file");
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .expect("private test file permissions");
        file.set_len(0).expect("truncate private test file");
        file.write_all(contents.as_ref())
            .expect("write private test file");
    }
    #[cfg(windows)]
    {
        use std::io::Write as _;

        let mut file = match fs::OpenOptions::new().write(true).truncate(true).open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                cookie_agent_models::secure_store::create_windows_private_file(path)
                    .expect("private test file")
            }
            Err(error) => panic!("private test file: {error}"),
        };
        file.write_all(contents.as_ref())
            .expect("write private test file");
    }
}

pub(crate) fn copy_private_test_tree(source: &std::path::Path, target: &std::path::Path) {
    create_private_test_dir(target);
    for entry in fs::read_dir(source).expect("snapshot source") {
        let entry = entry.expect("snapshot entry");
        if crate::ownership::is_owner_lock_path(&entry.path()) {
            continue;
        }
        // Unpublished captures are live, locked scratch files, not recovery state.
        if source.file_name().is_some_and(|name| name == "artifacts")
            && entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with(".capture-") && name.ends_with(".tmp"))
        {
            continue;
        }
        let destination = target.join(entry.file_name());
        if entry.file_type().expect("snapshot type").is_dir() {
            copy_private_test_tree(&entry.path(), &destination);
        } else {
            write_private_test_file(&destination, fs::read(entry.path()).expect("snapshot file"));
        }
    }
}

pub(crate) fn python_command() -> &'static str {
    if cfg!(windows) { "python" } else { "python3" }
}

/// Upper bound for a test's wait on something that should happen. Every
/// caller waits for progress and panics on expiry, so a passing test never
/// waits it out; the floor keeps loaded CI runners (Windows especially) from
/// failing a healthy test on a tight bound. `seconds` still records the
/// expected scale at the call site.
pub(crate) fn test_timeout(seconds: u64) -> std::time::Duration {
    std::time::Duration::from_secs(seconds.max(MIN_TEST_TIMEOUT_SECONDS))
}

const MIN_TEST_TIMEOUT_SECONDS: u64 = 60;

pub(crate) const EVENT_WATCHDOG_SECONDS: u64 = 60;

pub(crate) async fn await_session_change<T>(
    engine: &Engine,
    session_id: SessionId,
    description: &str,
    mut check: impl FnMut() -> Option<T>,
) -> T {
    let mut last_seen = VecDeque::with_capacity(20);
    let wait = async {
        let mut cursor = None;
        loop {
            let (snapshot, mut live) = engine
                .subscribe(session_id, cursor)
                .await
                .expect("test event subscription");
            for event in snapshot.events {
                if last_seen.len() == 20 {
                    last_seen.pop_front();
                }
                last_seen.push_back(event);
            }
            if let Some(result) = check() {
                return result;
            }
            loop {
                match live.recv().await {
                    Some(EventSubscriptionMessage::Event { event }) => {
                        if last_seen.len() == 20 {
                            last_seen.pop_front();
                        }
                        last_seen.push_back(*event);
                        if let Some(result) = check() {
                            return result;
                        }
                    }
                    Some(EventSubscriptionMessage::Gap {
                        last_delivered_seq, ..
                    }) => {
                        cursor = Some(last_delivered_seq);
                        break;
                    }
                    None => panic!("event subscription closed while waiting for {description}"),
                }
            }
        }
    };
    match tokio::time::timeout(test_timeout(EVENT_WATCHDOG_SECONDS), wait).await {
        Ok(result) => result,
        Err(_) => {
            let projection = engine.inner.store.get(session_id).ok();
            panic!(
                "timed out waiting for {description}: status={:?}, last_seen={:#?}",
                projection.as_ref().map(|projection| projection.status),
                last_seen
            );
        }
    }
}

pub(crate) async fn with_watchdog<T>(
    description: &str,
    future: impl std::future::Future<Output = T>,
) -> T {
    tokio::time::timeout(test_timeout(EVENT_WATCHDOG_SECONDS), future)
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {description}"))
}

pub(crate) async fn await_event(
    engine: &Engine,
    session_id: SessionId,
    description: &str,
    mut predicate: impl FnMut(&cookie_agent_protocol::StoredEvent) -> bool,
) -> cookie_agent_protocol::StoredEvent {
    await_session_change(engine, session_id, description, || {
        engine
            .inner
            .store
            .get(session_id)
            .ok()?
            .log
            .events()
            .iter()
            .find(|event| predicate(event))
            .cloned()
    })
    .await
}

pub(crate) async fn await_projection(
    engine: &Engine,
    session_id: SessionId,
    description: &str,
    predicate: impl Fn(&crate::session::SessionProjection) -> bool,
) -> crate::session::SessionProjection {
    await_session_change(engine, session_id, description, || {
        engine.inner.store.get(session_id).ok().filter(&predicate)
    })
    .await
}

/// Polls the background-delegation slot count under a root until it reaches
/// `expected`.
///
/// Adoption resolves recovered delegations on its own before `resume` returns,
/// so a settled count is what a test should see immediately. This helper is the
/// defensive form for assertions that only care about the settled value: it
/// keeps an unrelated scheduling hiccup from turning into a bare count
/// mismatch, and reports the last observed count when the bound expires.
pub(crate) async fn await_running_background_delegations(
    engine: &Engine,
    root: SessionId,
    expected: usize,
    label: &str,
) {
    let mut last = engine.running_background_delegations_for_test(root);
    let timed_out = {
        let last = &mut last;
        tokio::time::timeout(test_timeout(EVENT_WATCHDOG_SECONDS), async move {
            while *last != expected {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                *last = engine.running_background_delegations_for_test(root);
            }
        })
        .await
        .is_err()
    };
    assert!(
        !timed_out,
        "timed out waiting for {label}: expected {expected}, last observed {last}"
    );
}

pub(crate) async fn await_child(
    engine: &Engine,
    parent_session_id: SessionId,
    description: &str,
    predicate: impl Fn(&cookie_agent_protocol::ChildSummary) -> bool,
) -> cookie_agent_protocol::ChildSummary {
    await_session_change(engine, parent_session_id, description, || {
        engine
            .children(parent_session_id)
            .expect("children")
            .into_iter()
            .find(&predicate)
    })
    .await
}

#[derive(Debug, Default)]
pub(crate) struct TestFlag {
    pub(crate) set: AtomicBool,
    pub(crate) changed: tokio::sync::Notify,
}

impl TestFlag {
    pub(crate) fn set(&self) {
        self.set.store(true, Ordering::Release);
        self.changed.notify_waiters();
    }

    pub(crate) fn is_set(&self) -> bool {
        self.set.load(Ordering::Acquire)
    }

    pub(crate) async fn wait(&self) {
        tokio::time::timeout(test_timeout(EVENT_WATCHDOG_SECONDS), async {
            loop {
                let changed = self.changed.notified();
                if self.is_set() {
                    break;
                }
                changed.await;
            }
        })
        .await
        .expect("test flag notification");
    }
}

pub(crate) struct PanicResistantTempDir(pub(crate) Option<TempDir>);

impl Deref for PanicResistantTempDir {
    type Target = TempDir;

    fn deref(&self) -> &Self::Target {
        self.0.as_ref().expect("test directory")
    }
}

impl Drop for PanicResistantTempDir {
    fn drop(&mut self) {
        if std::thread::panicking()
            && let Some(directory) = self.0.take()
        {
            std::mem::forget(directory);
        }
    }
}

pub(crate) struct Fixture {
    pub(crate) _directory: PanicResistantTempDir,
    pub(crate) engine: Engine,
    pub(crate) config: LoadedConfiguration,
    pub(crate) manager: Arc<ModelManager>,
}

pub(crate) fn fixture() -> Fixture {
    let directory = private_tempdir();
    let project = directory.path().join(".cookie-agent");
    create_private_test_dir(&project);
    let provider_store = directory.path().join("provider-store");
    create_private_test_dir(&provider_store);
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
        runtime: EngineConfig {
            server: ServerConfig::default(),
            tool_output: ToolOutputConfig::default(),
            agent_md: cookie_agent_config::AgentMdConfig::default(),
            approval: ApprovalConfig::default(),
            model_retry: cookie_agent_config::ModelRetryConfig::default(),
            context_compaction: ContextCompactionConfig::default(),
            session_title: SessionTitleConfig::default(),
            delegation: cookie_agent_config::DelegationConfig::default(),
            messaging: cookie_agent_config::MessagingConfig::default(),
            pricing: cookie_agent_config::PricingConfig::default(),
            headers: BTreeMap::new(),
            providers: BTreeMap::new(),
        },
        agents: BTreeMap::new(),
        agent_presets: BTreeMap::new(),
        mcp_servers: BTreeMap::new(),
        user_mcp_servers: BTreeMap::new(),
        workspace_mcp_servers: BTreeMap::new(),
        plugins: Default::default(),
        config_paths: cookie_agent_config::ConfigLayerPaths::default(),
        skills: cookie_agent_config::SkillRegistry::default(),
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

pub(crate) fn bedrock_catalog() -> Arc<CatalogSnapshot> {
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
        cost: None,
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

pub(crate) fn empty_provider_workspace(path: &std::path::Path) -> LoadedConfiguration {
    create_private_test_dir(path);
    let project = path.join(".cookie-agent");
    create_private_test_dir(&project);
    write_private_test_file(&project.join("config.toml"), "");
    let agents = project.join("agents");
    create_private_test_dir(&agents);
    write_private_test_file(
        &agents.join("primary.md"),
        "---\ndescription: Bedrock test agent\nmode: primary\nenabled: true\nmodels: [{ model: \"amazon-bedrock/anthropic.claude-3-5-sonnet-20241022-v2:0\", variant: base }]\npermissions: {}\n---\nUse Bedrock.\n",
    );
    load_from_roots(None, Some(&project)).expect("workspace config")
}

pub(crate) fn open_workspace_engine(
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
    fs::create_dir_all(data.join("model-snapshots")).expect("workspace manifest directory");
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

pub(crate) fn custom_fixture() -> (Fixture, RunSelection) {
    custom_fixture_with_endpoint("http://127.0.0.1:9/v1")
}

pub(crate) fn managed_openai_compaction_fixture(endpoint: &str) -> (Fixture, RunSelection) {
    let directory = private_tempdir();
    let project = directory.path().join(".cookie-agent");
    create_private_test_dir(&project);
    write_private_test_file(
        &project.join("config.toml"),
        r#"
[providers.openai]
source = "models_dev"
api_key = "test-secret"

[providers.openai.models."gpt-test"]
model_id = "wire-native"
adaptor_options = { request_endpoint = "responses" }
compaction = "openai-responses-compact"
"#,
    );
    let agents = project.join("agents");
    create_private_test_dir(&agents);
    write_private_test_file(
        &agents.join("primary.md"),
        "---\ndescription: Native compaction test\nmode: primary\nenabled: true\nmodels: [{ model: \"openai/gpt-test\", variant: base }]\npermissions: {}\n---\nTest native compaction.\n",
    );
    let mut config = load_from_roots(None, Some(&project)).expect("loaded config");
    config.runtime.session_title.generate_on_first_turn = false;
    let provider_id = ProviderId::new("openai").expect("provider ID");
    let model_id = ProviderModelId::new("gpt-test").expect("model ID");
    let model = CatalogModelRecord {
        id: model_id.clone(),
        name: "GPT Test".into(),
        description: "native compaction test".into(),
        family: None,
        attachment: false,
        reasoning: false,
        tool_call: true,
        structured_output: Some(false),
        temperature: Some(true),
        open_weights: false,
        status: CatalogModelStatus::Stable,
        release_date: "2026-01-01".into(),
        last_updated: "2026-01-01".into(),
        modalities: CatalogModalities {
            input: vec!["text".into()],
            output: vec!["text".into()],
        },
        limits: CatalogLimits {
            context: 4096,
            input: None,
            output: 1024,
        },
        shape: None,
        provider: None,
        reasoning_options: Vec::new(),
        cost: None,
        interleaved: None,
        canonical_provenance: None,
    };
    let record = CatalogProviderRecord {
        id: provider_id.clone(),
        name: "OpenAI".into(),
        environment: vec!["OPENAI_API_KEY".into()],
        npm: "@ai-sdk/openai".into(),
        api: Some(endpoint.into()),
        shape: None,
        documentation_url: "https://example.test/openai".into(),
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
    let catalog = Arc::new(CatalogSnapshot {
        revision: CatalogRevision::new(format!("sha256:{}", "c".repeat(64))).unwrap(),
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
    });
    let provider_store = directory.path().join("provider-store");
    create_private_test_dir(&provider_store);
    let manager = Arc::new(
        ModelManager::new(
            config.runtime.providers.clone(),
            catalog,
            ProviderStore::open(provider_store).expect("provider store"),
        )
        .expect("managed manager"),
    );
    let engine = Engine::open(EngineOptions {
        data_dir: directory.path().join("data"),
        cwd: directory.path().to_owned(),
        config: config.clone(),
        model_manager: Arc::clone(&manager),
        tools: Vec::new(),
    })
    .expect("managed engine");
    (
        Fixture {
            _directory: directory,
            engine,
            config,
            manager,
        },
        RunSelection {
            agent: AgentId::new("primary").unwrap(),
            model: ModelSelection {
                model: "openai/gpt-test".parse().unwrap(),
                variant: None,
            },
            preset: None,
        },
    )
}

pub(crate) fn create_buffered_delegated_child(engine: &Engine, parent: SessionId) -> SessionId {
    let parent_projection = engine.inner.store.get(parent).expect("parent projection");
    let EventPayload::SessionCreated {
        origin: _,
        short_id: _,
        cwd_identity,
        creation_selection,
        creation_agent,
        runtime_revision,
        catalog_revision,
        provider_state_revision,
        model_revision,
        agent_revision,
        recipe_registry_revision,
        manifest_revision,
    } = parent_projection
        .log
        .event_snapshot()
        .first()
        .expect("parent creation event")
        .payload
        .clone()
    else {
        panic!("expected a session creation event");
    };
    let child = SessionId::new_v7();
    engine
        .inner
        .store
        .create(
            child,
            cookie_agent_protocol::EventOrigin::new("engine:test").expect("event origin"),
            EventPayload::SessionCreated {
                short_id: None,
                origin: cookie_agent_protocol::SessionOrigin::Delegated {
                    root_session_id: parent,
                    parent_session_id: parent,
                    parent_run_id: cookie_agent_protocol::RunId::new_v7(),
                    parent_tool_call_id: ToolCallId::new_v7(),
                    invocation_id: InvocationId::new_v7(),
                    depth: 1,
                },
                cwd_identity,
                creation_selection,
                creation_agent,
                runtime_revision,
                catalog_revision,
                provider_state_revision,
                model_revision,
                agent_revision,
                recipe_registry_revision,
                manifest_revision,
            },
        )
        .expect("create delegated child");
    child
}

pub(crate) fn custom_fixture_with_endpoint(endpoint: &str) -> (Fixture, RunSelection) {
    custom_fixture_with_endpoint_and_primary_agent(
        endpoint,
        "---\ndescription: Primary test agent\nmode: primary\nenabled: true\nmodels: [{ model: \"custom.test/group/model\", variant: base }]\npermissions:\n  delegate:\n    worker: allow\n---\nTest prompt.\n",
    )
}

pub(crate) async fn retry_fixture_with_endpoint(
    endpoint: &str,
    model_retry: ModelRetryConfig,
) -> (Fixture, RunSelection) {
    let primary = "---\ndescription: Retry test agent\nmode: primary\nenabled: true\nmodels:\n  - { model: \"custom.test/group/model\", variant: base }\n  - { model: \"custom.test/group/fallback\", variant: base }\npermissions: {}\n---\nTest retry policy.\n";
    let (mut fixture, selection) =
        custom_fixture_with_endpoint_and_primary_agent(endpoint, primary);
    fixture.engine.shutdown().await;
    fixture.config.runtime.model_retry = model_retry;

    let provider_id = ProviderId::new("custom.test").expect("retry provider ID");
    let ProviderDefinition::Custom(provider) = fixture
        .config
        .runtime
        .providers
        .get_mut(&provider_id)
        .expect("retry provider")
    else {
        panic!("custom retry provider");
    };
    let source_id = ProviderModelId::new("group/model").expect("source model ID");
    let fallback_id = ProviderModelId::new("group/fallback").expect("fallback model ID");
    let fallback = provider.models[&source_id].clone();
    provider.models.insert(fallback_id, fallback);

    let current = fixture.manager.current();
    let manager = Arc::new(
        ModelManager::new(
            fixture.config.runtime.providers.clone(),
            Arc::clone(current.catalog()),
            ProviderStore::open(fixture._directory.path().join("provider-store"))
                .expect("retry provider store"),
        )
        .expect("retry model manager"),
    );
    fixture.engine = Engine::open(EngineOptions {
        data_dir: fixture._directory.path().join("data"),
        cwd: fixture._directory.path().to_owned(),
        config: fixture.config.clone(),
        model_manager: Arc::clone(&manager),
        tools: Vec::new(),
    })
    .expect("retry engine");
    fixture.manager = manager;
    (fixture, selection)
}

pub(crate) async fn reopen_fixture_with_residency(
    fixture: &mut Fixture,
    max_resident_subagents: usize,
    idle_eviction_after: std::time::Duration,
) {
    fixture.engine.shutdown().await;
    fixture.config.runtime.delegation.max_resident_subagents = max_resident_subagents;
    fixture.config.runtime.delegation.idle_eviction_after = idle_eviction_after;
    fixture.engine = Engine::open(EngineOptions {
        data_dir: fixture._directory.path().join("data"),
        cwd: fixture._directory.path().to_owned(),
        config: fixture.config.clone(),
        model_manager: Arc::clone(&fixture.manager),
        tools: Vec::new(),
    })
    .expect("reopen fixture with subagent residency settings");
}

pub(crate) fn approval_fixture_with_endpoint(endpoint: &str) -> (Fixture, RunSelection) {
    custom_fixture_with_endpoint_and_primary_agent(
        endpoint,
        "---\ndescription: Approval test agent\nmode: primary\nenabled: true\nmodels: [{ model: \"custom.test/group/model\", variant: base }]\npermissions:\n  write: ask\n---\nTest approval flow.\n",
    )
}

pub(crate) fn denied_approval_fixture_with_endpoint(endpoint: &str) -> (Fixture, RunSelection) {
    custom_fixture_with_endpoint_and_primary_agent(
        endpoint,
        "---\ndescription: Denied approval test agent\nmode: primary\nenabled: true\nmodels: [{ model: \"custom.test/group/model\", variant: base }]\npermissions:\n  write: deny\n---\nTest denied approval flow.\n",
    )
}

pub(crate) fn custom_fixture_with_endpoint_and_primary_agent(
    endpoint: &str,
    primary_agent: &str,
) -> (Fixture, RunSelection) {
    custom_fixture_with_endpoint_primary_and_internal(endpoint, primary_agent, None, None, false)
}

pub(crate) fn custom_fixture_with_endpoint_primary_and_internal(
    endpoint: &str,
    primary_agent: &str,
    internal: Option<(&str, &str)>,
    compaction_buffer_tokens: Option<u64>,
    generate_titles: bool,
) -> (Fixture, RunSelection) {
    custom_fixture_with_endpoint_primary_internal_and_concurrency(
        endpoint,
        primary_agent,
        internal,
        compaction_buffer_tokens,
        generate_titles,
        None,
        None,
    )
}

pub(crate) fn custom_fixture_with_endpoint_primary_internal_and_concurrency(
    endpoint: &str,
    primary_agent: &str,
    internal: Option<(&str, &str)>,
    compaction_buffer_tokens: Option<u64>,
    generate_titles: bool,
    max_concurrency: Option<u32>,
    mcp_server: Option<LoadedMcpServer>,
) -> (Fixture, RunSelection) {
    custom_fixture_with_endpoint_primary_internal_concurrency_and_context(
        endpoint,
        primary_agent,
        internal,
        compaction_buffer_tokens,
        generate_titles,
        max_concurrency,
        mcp_server,
        4_096,
        None,
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn custom_fixture_with_endpoint_primary_internal_concurrency_and_context(
    endpoint: &str,
    primary_agent: &str,
    internal: Option<(&str, &str)>,
    compaction_buffer_tokens: Option<u64>,
    generate_titles: bool,
    max_concurrency: Option<u32>,
    mcp_server: Option<LoadedMcpServer>,
    context_tokens: u64,
    worker_agent: Option<&str>,
) -> (Fixture, RunSelection) {
    custom_fixture_with_endpoint_primary_internal_concurrency_context_and_adaptor(
        endpoint,
        primary_agent,
        internal,
        compaction_buffer_tokens,
        generate_titles,
        max_concurrency,
        mcp_server,
        context_tokens,
        worker_agent,
        "openai-compatible",
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn custom_fixture_with_endpoint_primary_internal_concurrency_context_and_adaptor(
    endpoint: &str,
    primary_agent: &str,
    internal: Option<(&str, &str)>,
    compaction_buffer_tokens: Option<u64>,
    generate_titles: bool,
    max_concurrency: Option<u32>,
    mcp_server: Option<LoadedMcpServer>,
    context_tokens: u64,
    worker_agent: Option<&str>,
    adaptor: &str,
) -> (Fixture, RunSelection) {
    custom_fixture_with_capabilities(
        endpoint,
        primary_agent,
        internal,
        compaction_buffer_tokens,
        generate_titles,
        max_concurrency,
        mcp_server,
        context_tokens,
        worker_agent,
        adaptor,
        None,
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn custom_fixture_with_capabilities(
    endpoint: &str,
    primary_agent: &str,
    internal: Option<(&str, &str)>,
    compaction_buffer_tokens: Option<u64>,
    generate_titles: bool,
    max_concurrency: Option<u32>,
    mcp_server: Option<LoadedMcpServer>,
    context_tokens: u64,
    worker_agent: Option<&str>,
    adaptor: &str,
    capabilities_override: Option<&str>,
) -> (Fixture, RunSelection) {
    custom_fixture_with_capabilities_and_worker_name(
        endpoint,
        primary_agent,
        internal,
        compaction_buffer_tokens,
        generate_titles,
        max_concurrency,
        mcp_server,
        context_tokens,
        worker_agent,
        adaptor,
        capabilities_override,
        None,
        None,
        "worker",
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn custom_fixture_with_capabilities_and_variants(
    endpoint: &str,
    primary_agent: &str,
    internal: Option<(&str, &str)>,
    compaction_buffer_tokens: Option<u64>,
    generate_titles: bool,
    max_concurrency: Option<u32>,
    mcp_server: Option<LoadedMcpServer>,
    context_tokens: u64,
    worker_agent: Option<&str>,
    adaptor: &str,
    capabilities_override: Option<&str>,
    model_variants: Option<&str>,
) -> (Fixture, RunSelection) {
    custom_fixture_with_capabilities_and_worker_name(
        endpoint,
        primary_agent,
        internal,
        compaction_buffer_tokens,
        generate_titles,
        max_concurrency,
        mcp_server,
        context_tokens,
        worker_agent,
        adaptor,
        capabilities_override,
        model_variants,
        None,
        "worker",
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn custom_fixture_with_capabilities_and_worker_name(
    endpoint: &str,
    primary_agent: &str,
    internal: Option<(&str, &str)>,
    compaction_buffer_tokens: Option<u64>,
    generate_titles: bool,
    max_concurrency: Option<u32>,
    mcp_server: Option<LoadedMcpServer>,
    context_tokens: u64,
    worker_agent: Option<&str>,
    adaptor: &str,
    capabilities_override: Option<&str>,
    model_variants: Option<&str>,
    provider_cache: Option<&str>,
    worker_name: &str,
) -> (Fixture, RunSelection) {
    let directory = private_tempdir();
    let project = directory.path().join(".cookie-agent");
    create_private_test_dir(&project);
    let config_text = r#"
[delegation]
max_depth = 1

[providers."custom.test"]
source = "custom"
endpoint = "http://127.0.0.1:9/v1"
adaptor = "openai-compatible"
auth = { method = "no-auth-v1", values = {} }
__PROVIDER_CACHE__

[providers."custom.test".models."group/model"]
display_name = "Model"
__MODEL_VARIANTS__

[providers."custom.test".models."group/model".capabilities]
__MODEL_CAPABILITIES__

[providers."custom.test".models."group/fallback"]
display_name = "Fallback model"

[providers."custom.test".models."group/fallback".capabilities]
__MODEL_CAPABILITIES__
"#
    .replace("http://127.0.0.1:9/v1", endpoint)
    .replace(
        "adaptor = \"openai-compatible\"",
        &format!("adaptor = \"{adaptor}\""),
    )
    .replace(
        "__MODEL_CAPABILITIES__",
        capabilities_override.map_or_else(
            || {
                format!(
                    "input = [\"text\"]\noutput = [\"text\"]\ncontext_tokens = {context_tokens}\noutput_tokens = 1024\ntool_calling = true\nparallel_tool_calls = true\nstructured_output = false\nreasoning = false\ntemperature = true\ntop_p = true\nseed = true\nnative_replay = \"unsupported\"\nmedia = {{}}"
                )
            },
            str::to_owned,
        )
        .as_str(),
    )
    .replace("__MODEL_VARIANTS__", model_variants.unwrap_or_default());
    let config_text = config_text.replace(
        "__PROVIDER_CACHE__",
        &provider_cache.map_or_else(String::new, |cache| {
            format!("\n[providers.\"custom.test\".cache]\n{cache}")
        }),
    );
    let config_text = if adaptor.starts_with("anthropic") {
        config_text.replace("seed = true", "seed = false").replace(
            "auth = { method = \"no-auth-v1\", values = {} }",
            "auth = { method = \"anthropic-api-key-v1\", values = { api_key = \"test-key\" } }",
        )
    } else {
        config_text
    };
    let config_text = max_concurrency.map_or(config_text.clone(), |max_concurrency| {
        config_text.replace(
            "[delegation]\nmax_depth = 1",
            &format!("[delegation]\nmax_depth = 1\nmax_concurrency = {max_concurrency}"),
        )
    });
    write_private_test_file(&project.join("config.toml"), config_text);
    let agents = project.join("agents");
    create_private_test_dir(&agents);
    write_private_test_file(&agents.join("primary.md"), primary_agent);
    write_private_test_file(
        &agents.join(format!("{worker_name}.md")),
        worker_agent.unwrap_or(
            "---\ndescription: Worker test agent\nmode: subagent\nenabled: true\nmodels: [{ model: \"custom.test/group/model\", variant: base }]\npermissions: {}\n---\nWorker prompt.\n",
        ),
    );
    if let Some((name, document)) = internal {
        write_private_test_file(&agents.join(name), document);
    }
    let mut config = load_from_roots(None, Some(&project)).expect("loaded config");
    if let Some(server) = mcp_server {
        config.mcp_servers.insert("fixture".into(), server);
    }
    config.runtime.session_title.generate_on_first_turn = generate_titles;
    if let Some(buffer_tokens) = compaction_buffer_tokens {
        config.runtime.context_compaction.trigger =
            cookie_agent_config::ContextCompactionTrigger::BufferTokens { buffer_tokens };
    }
    let provider_store = directory.path().join("provider-store");
    create_private_test_dir(&provider_store);
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
        preset: None,
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

pub(crate) fn frozen_root_policy(
    fixture: &Fixture,
    selection: &RunSelection,
) -> crate::policy::FrozenRunPolicy {
    try_frozen_root_policy(fixture, selection).expect("frozen root policy")
}

pub(crate) fn try_frozen_root_policy(
    fixture: &Fixture,
    selection: &RunSelection,
) -> Result<crate::policy::FrozenRunPolicy, EngineError> {
    let runtime = fixture.engine.current_runtime();
    let registry = runtime
        .agents_for_preset(selection.preset.as_deref())
        .expect("selected agent preset");
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
        fixture.config.runtime.model_retry,
    )
}

pub(crate) fn synthetic_default_fixture(authored_agent: Option<&str>) -> Fixture {
    synthetic_default_fixture_with_config(authored_agent, "http://127.0.0.1:9/v1", "")
        .expect("engine")
}

pub(crate) fn synthetic_default_fixture_with_config(
    authored_agent: Option<&str>,
    endpoint: &str,
    extra_config: &str,
) -> Result<Fixture, EngineError> {
    let directory = private_tempdir();
    let project = directory.path().join(".cookie-agent");
    create_private_test_dir(&project);
    let base_config = r#"
[providers."custom.test"]
source = "custom"
endpoint = "http://127.0.0.1:9/v1"
adaptor = "openai-compatible"
auth = { method = "no-auth-v1", values = {} }

[providers."custom.test".models."z-model"]
display_name = "Z Model"
capabilities = { input = ["text"], output = ["text"], context_tokens = 4096, output_tokens = 1024, tool_calling = true, parallel_tool_calls = true, structured_output = false, reasoning = false, temperature = true, top_p = true, seed = true, native_replay = "unsupported", media = {} }

[providers."custom.test".models."a-model"]
display_name = "A Model"
capabilities = { input = ["text"], output = ["text"], context_tokens = 4096, output_tokens = 1024, tool_calling = true, parallel_tool_calls = true, structured_output = false, reasoning = false, temperature = true, top_p = true, seed = true, native_replay = "unsupported", media = {} }
variants = { zeta = { }, alpha = { }, precise = { generation_options = { temperature = 0.25 } } }
default_variant = "precise"
"#;
    let mut config_text = base_config.replace("http://127.0.0.1:9/v1", endpoint);
    config_text.push_str(extra_config);
    write_private_test_file(&project.join("config.toml"), config_text);
    if let Some(agent) = authored_agent {
        let agents = project.join("agents");
        create_private_test_dir(&agents);
        write_private_test_file(&agents.join("primary.md"), agent);
    }
    let config = load_from_roots(None, Some(&project)).expect("loaded config");
    let provider_store = directory.path().join("provider-store");
    create_private_test_dir(&provider_store);
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
    })?;
    Ok(Fixture {
        _directory: directory,
        engine,
        config,
        manager,
    })
}

pub(crate) async fn accept_scripted_planned_request(
    listener: &tokio::net::TcpListener,
    context: &str,
) -> (tokio::net::TcpStream, Vec<u8>) {
    loop {
        let (mut socket, _) = listener.accept().await.expect(context);
        let request = read_scripted_http_request(&mut socket).await;
        if scripted_is_auxiliary_subagent_notification(&request) {
            write_scripted_sse(
                &mut socket,
                &scripted_text_body("auxiliary subagent notification accepted"),
            )
            .await;
            continue;
        }
        return (socket, request);
    }
}

pub(crate) fn spawn_scripted_auxiliary_tail(listener: tokio::net::TcpListener) {
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let request = read_scripted_http_request(&mut socket).await;
            if scripted_is_auxiliary_subagent_notification(&request) {
                write_scripted_sse(
                    &mut socket,
                    &scripted_text_body("auxiliary subagent notification accepted"),
                )
                .await;
            }
        }
    });
}

pub(crate) async fn wait_for_escalated_approval(
    engine: &Engine,
    session_id: SessionId,
) -> cookie_agent_protocol::ApprovalRecord {
    let approval = await_session_change(
        engine,
        session_id,
        "user-visible escalated approval",
        || {
            let mut approvals = engine
                .list_approvals(session_id, Some(ApprovalStatus::Escalated))
                .approvals;
            approvals.pop()
        },
    )
    .await;
    tokio::time::timeout(test_timeout(EVENT_WATCHDOG_SECONDS), async {
        loop {
            let ready = engine.inner.test_hooks.pending_approval_ready.notified();
            if engine
                .inner
                .approvals
                .pending
                .lock()
                .expect("pending approvals lock")
                .contains_key(&(session_id, approval.request.approval_id()))
            {
                break;
            }
            ready.await;
        }
    })
    .await
    .expect("escalated approval responder readiness");
    approval
}

pub(crate) async fn wait_for_tree_escalated_approval(
    engine: &Engine,
    root_session_id: SessionId,
    child_session_id: SessionId,
) -> cookie_agent_protocol::ApprovalRecord {
    await_event(
        engine,
        child_session_id,
        "delegated child approval escalation",
        |event| matches!(event.payload, EventPayload::ApprovalEscalated { .. }),
    )
    .await;
    let approval = engine
        .list_approvals(root_session_id, Some(ApprovalStatus::Escalated))
        .approvals
        .into_iter()
        .find(|approval| approval.session_id == child_session_id)
        .expect("delegated child escalated approval");
    tokio::time::timeout(test_timeout(EVENT_WATCHDOG_SECONDS), async {
        loop {
            let ready = engine.inner.test_hooks.pending_approval_ready.notified();
            if engine
                .inner
                .approvals
                .pending
                .lock()
                .expect("pending approvals lock")
                .contains_key(&(child_session_id, approval.request.approval_id()))
            {
                break;
            }
            ready.await;
        }
    })
    .await
    .expect("delegated child approval responder readiness");
    approval
}

pub(crate) async fn approve_once(
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
        .approval_respond(
            ApprovalRespondParams {
                session_id: approval.session_id,
                approval_id: approval.request.approval_id(),
                request_revision,
                operation_fingerprint: approval.request.operation_fingerprint().clone(),
                client_response_id: ClientResponseId::new(client_response_id)
                    .expect("client response ID"),
                decision: ApprovalUserDecision::ApproveOnce,
                feedback: None,
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("approve once")
}

pub(crate) async fn wait_for_tool_execution(
    engine: &Engine,
    session_id: SessionId,
    executed: &TestFlag,
) {
    executed.wait().await;
    await_event(engine, session_id, "completed tool execution", |event| {
        matches!(
            &event.payload,
            EventPayload::ToolCallTerminated { termination }
                if termination.outcome == ToolTerminationOutcome::Completed
        )
    })
    .await;
}

pub(crate) async fn wait_for_session_not_running(engine: &Engine, session_id: SessionId) {
    await_projection(engine, session_id, "session completion", |projection| {
        projection.status != SessionStatus::Running
    })
    .await;
}

/// Start a run, waiting out a start latch the previous run has not yet dropped.
///
/// A session projection reports a terminal status as soon as the terminal
/// event is appended, but the actor clears the latch that guards
/// `SessionCommand::Start` only once it finishes tearing the run down. A test
/// that starts a second run right after observing completion therefore races
/// that release under load and sees `SessionRunning`. Nothing was started when
/// the latch rejects, so retrying is the whole fix.
pub(crate) async fn start_run_when_idle(
    engine: &Engine,
    params: RunStartParams,
    origin: cookie_agent_protocol::EventOrigin,
) -> Result<cookie_agent_protocol::RunStartResult, EngineError> {
    let deadline = tokio::time::Instant::now() + test_timeout(EVENT_WATCHDOG_SECONDS);
    loop {
        match engine.start_run(params.clone(), origin.clone()).await {
            Err(EngineError::SessionRunning(session)) if tokio::time::Instant::now() < deadline => {
                debug_assert_eq!(session, params.session_id);
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
            other => return other,
        }
    }
}

pub(crate) async fn wait_for_run_inactive(engine: &Engine, run_id: cookie_agent_protocol::RunId) {
    tokio::time::timeout(test_timeout(EVENT_WATCHDOG_SECONDS), async {
        while engine.run_active_for_test(run_id) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("run task termination");
}

/// Drain every command already queued on a session's actor, including the
/// post-reply producer reconcile that follows producer commands, so a test may
/// mutate that session's log directly without racing the actor.
pub(crate) async fn settle_session_actor(engine: &Engine, session: SessionId) {
    let actor = {
        engine
            .inner
            .sessions
            .actors
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&session)
            .cloned()
    };
    let Some(actor) = actor else {
        return;
    };
    let (reply, receiver) = tokio::sync::oneshot::channel();
    actor
        .send(crate::runtime::SessionCommand::EvictionBarrier { reply })
        .await
        .expect("session actor accepts the barrier");
    receiver
        .await
        .expect("session actor replies to the barrier")
        .expect("session actor barrier succeeds");
}

pub(crate) fn interception_plugin(name: &str, extra_env: &[(&str, String)]) -> PluginConfig {
    let mut env = BTreeMap::from([
        ("FIXTURE_NAME".into(), name.to_owned()),
        ("FIXTURE_TOOLS".into(), "[]".into()),
        (
            "FIXTURE_CAPABILITIES".into(),
            r#"{"producer_messaging":false,"tools":false,"resources":false,"subscribe_events":false,"subscribe_bus":false,"publish_bus":false,"publish_session_events":false,"intercept":["tool_before_call"]}"#.into(),
        ),
    ]);
    env.extend(
        extra_env
            .iter()
            .cloned()
            .map(|(key, value)| (key.into(), value)),
    );
    PluginConfig {
        command: Some(python_command().into()),
        args: vec![PLUGIN_FIXTURE.into()],
        env,
        cwd: None,
        enabled: true,
        producer_messaging: false,
        interception_timeout_ms: 2_000,
        startup_timeout_ms: 10_000,
        shutdown_grace_ms: 3_000,
        tool_timeout_ms: 30_000,
    }
}

pub(crate) async fn reopen_with_interception_plugins(
    fixture: &mut Fixture,
    plugins: Vec<(String, PluginConfig)>,
) {
    fixture.engine.shutdown().await;
    fixture.config.plugins = plugins.into_iter().collect();
    fixture.engine = reopen_engine(fixture);
    fixture.engine.inner.plugins.await_eager_ready().await;
}

pub(crate) async fn reject_approval(
    engine: &Engine,
    approval: &cookie_agent_protocol::ApprovalRecord,
    client_response_id: &str,
) {
    let request_revision = serde_json::to_value(&approval.request)
        .expect("approval request JSON")
        .get("revision")
        .and_then(serde_json::Value::as_u64)
        .expect("approval request revision");
    engine
        .approval_respond(
            ApprovalRespondParams {
                session_id: approval.session_id,
                approval_id: approval.request.approval_id(),
                request_revision,
                operation_fingerprint: approval.request.operation_fingerprint().clone(),
                client_response_id: ClientResponseId::new(client_response_id)
                    .expect("client response ID"),
                decision: ApprovalUserDecision::Reject,
                feedback: None,
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("reject approval");
}

pub(crate) fn append_compaction_tool_history(
    fixture: &Fixture,
    session: SessionId,
    run: cookie_agent_protocol::RunId,
    binding: &cookie_agent_protocol::FrozenModelBinding,
    result: cookie_agent_protocol::PersistedToolResult,
    latest_usage: u64,
) -> ToolCallId {
    append_named_compaction_tool_history(
        fixture,
        session,
        run,
        binding,
        result,
        (
            "bash",
            serde_json::json!({"command": "produce historical output"}),
        ),
        Some(latest_usage),
    )
}

pub(crate) fn append_named_compaction_tool_history(
    fixture: &Fixture,
    session: SessionId,
    run: cookie_agent_protocol::RunId,
    binding: &cookie_agent_protocol::FrozenModelBinding,
    result: cookie_agent_protocol::PersistedToolResult,
    tool: (&str, serde_json::Value),
    latest_usage: Option<u64>,
) -> ToolCallId {
    let (name, input) = tool;
    let tool_call_id = ToolCallId::new_v7();
    let model_call_id =
        cookie_agent_protocol::ModelCallId::new(format!("compaction-history-tool-{tool_call_id}"))
            .expect("model call ID");
    let prior_events = fixture
        .engine
        .inner
        .store
        .get(session)
        .expect("history projection")
        .log
        .events();
    let model_turn_seq = prior_events
        .iter()
        .filter_map(|event| match event.payload {
            EventPayload::ModelTurnCommitted { model_turn_seq, .. } => Some(model_turn_seq),
            _ => None,
        })
        .max()
        .unwrap_or(0)
        + 1;
    let owner = cookie_agent_protocol::AssistantToolCallRef {
        model_turn_seq,
        content_index: 0,
        model_call_id: model_call_id.clone(),
        provider_item_id: None,
    };
    let resolved_model = crate::policy::wire_resolved(binding);
    let first_attempt_ordinal = prior_events
        .iter()
        .filter(|event| {
            event.run_id == Some(run)
                && matches!(event.payload, EventPayload::ModelAttemptStarted { .. })
        })
        .count() as u32
        + 1;
    let prompt_fingerprint = prior_events
        .iter()
        .find_map(|event| match &event.payload {
            EventPayload::RunStarted { agent, .. } if event.run_id == Some(run) => {
                Some(agent.prompt_fingerprint.clone())
            }
            _ => None,
        })
        .expect("run prompt fingerprint");
    let append_model_turn = |model_turn_seq, attempt_ordinal, content, finish_reason, usage| {
        let attempt_id = cookie_agent_protocol::AttemptId::new_v7();
        fixture
            .engine
            .append_direct(
                session,
                Some(run),
                cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
                EventPayload::ModelAttemptStarted {
                    attempt_id,
                    attempt_ordinal,
                    fallback_index: 0,
                    retry_ordinal: 0,
                    resolved_model: resolved_model.clone(),
                    prompt_fingerprint: prompt_fingerprint.clone(),
                },
            )
            .expect("append model attempt");
        fixture
            .engine
            .append_direct(
                session,
                Some(run),
                cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
                EventPayload::ModelTurnCommitted {
                    attempt_id,
                    model_turn_seq,
                    resolved_model: resolved_model.clone(),
                    input_through_seq: 1,
                    turn: cookie_agent_protocol::PersistedModelTurn {
                        content,
                        provider_options: BTreeMap::new(),
                        finish_reason,
                        usage,
                        response_metadata: BTreeMap::new(),
                        provider_metadata: BTreeMap::new(),
                        native_replay: None,
                    },
                    warnings: Vec::new(),
                },
            )
            .expect("append model turn");
    };
    append_model_turn(
        model_turn_seq,
        first_attempt_ordinal,
        vec![cookie_agent_protocol::PersistedAssistantPart::ToolCall {
            id: model_call_id,
            provider_item_id: None,
            name: cookie_agent_protocol::SafeCode::new(name).unwrap(),
            input,
            raw_input: None,
            metadata: None,
        }],
        cookie_agent_protocol::ModelFinishReason::ToolCalls,
        cookie_agent_protocol::Usage::default(),
    );
    fixture
        .engine
        .append_direct(
            session,
            Some(run),
            cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
            EventPayload::ToolCallStarted {
                start: cookie_agent_protocol::ToolCallStart {
                    output: Default::default(),
                    tool_call_id,
                    owner: owner.clone(),
                    presentation: cookie_agent_protocol::ToolCallPresentation {
                        title: cookie_agent_protocol::SafeDisplayText::new("Historical output")
                            .unwrap(),
                        primary_argument: None,
                    },
                    operation_fingerprint: serde_json::from_value(serde_json::json!({
                        "digest": Sha256Digest::of_bytes(b"compaction-history-tool")
                    }))
                    .unwrap(),
                },
            },
        )
        .expect("append tool start");
    fixture
        .engine
        .append_direct(
            session,
            Some(run),
            cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
            EventPayload::ToolCallTerminated {
                termination: cookie_agent_protocol::ToolCallTermination {
                    tool_call_id,
                    owner,
                    outcome: ToolTerminationOutcome::Completed,
                    result: Some(result),
                    error: None,
                },
            },
        )
        .expect("append tool result");
    for (model_turn_seq, attempt_ordinal, usage) in [
        (
            model_turn_seq + 1,
            first_attempt_ordinal + 1,
            cookie_agent_protocol::Usage::default(),
        ),
        (
            model_turn_seq + 2,
            first_attempt_ordinal + 2,
            cookie_agent_protocol::Usage {
                input_tokens: latest_usage,
                ..cookie_agent_protocol::Usage::default()
            },
        ),
    ]
    .into_iter()
    .take(if latest_usage.is_some() { 2 } else { 0 })
    {
        append_model_turn(
            model_turn_seq,
            attempt_ordinal,
            vec![cookie_agent_protocol::PersistedAssistantPart::Text {
                text: format!("recent turn {model_turn_seq}"),
                metadata: None,
            }],
            cookie_agent_protocol::ModelFinishReason::Stop,
            usage,
        );
    }
    tool_call_id
}

pub(crate) fn reopen_engine(fixture: &Fixture) -> Engine {
    reopen_engine_parts(&fixture._directory, &fixture.config, &fixture.manager)
}

pub(crate) fn reopen_engine_parts(
    directory: &PanicResistantTempDir,
    config: &LoadedConfiguration,
    source_manager: &Arc<ModelManager>,
) -> Engine {
    let current = source_manager.current();
    let manager = Arc::new(
        ModelManager::new(
            current.authored().clone(),
            Arc::clone(current.catalog()),
            ProviderStore::open(directory.path().join("provider-store"))
                .expect("reopened provider store"),
        )
        .expect("reopened manager"),
    );
    Engine::open(EngineOptions {
        data_dir: directory.path().join("data"),
        cwd: directory.path().to_owned(),
        config: config.clone(),
        model_manager: manager,
        tools: Vec::new(),
    })
    .expect("reopened engine")
}

pub(crate) fn copy_test_tree(source: &std::path::Path, target: &std::path::Path) {
    fs::create_dir_all(target).expect("create copied directory");
    let Ok(entries) = fs::read_dir(source) else {
        return;
    };
    for entry in entries {
        let Ok(entry) = entry else { continue };
        if crate::ownership::is_owner_lock_path(&entry.path()) {
            continue;
        }
        let destination = target.join(entry.file_name());
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            copy_test_tree(&entry.path(), &destination);
        } else if let Err(error) = fs::copy(entry.path(), destination)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            panic!("copy test file: {error}");
        }
    }
}

pub(crate) async fn start_streaming_bash_test_run(
    command: &str,
    interactive: bool,
) -> (
    Fixture,
    SessionId,
    cookie_agent_protocol::RunId,
    ToolCallId,
    Arc<tokio::sync::Notify>,
    Arc<tokio::sync::Notify>,
    tokio::task::JoinHandle<Vec<String>>,
) {
    let (endpoint, responses, captured) = scripted_channel_server(1).await;
    responses
        .send(MatchedScriptedResponse::last_message_role(
            "user",
            scripted_tool_body(
                "streaming-test-call",
                "bash",
                serde_json::json!({"command":command, "interactive":interactive}),
            ),
        ))
        .expect("scripted tool response");
    let (fixture, selection) = custom_fixture_with_endpoint_and_primary_agent(
        &endpoint,
        "---\ndescription: Streaming timeout test\nmode: primary\nenabled: true\nmodels: [{ model: \"custom.test/group/model\", variant: base }]\npermissions:\n  bash: allow\n---\nTest streaming timeout ordering.\n",
    );
    let output_started = Arc::new(tokio::sync::Notify::new());
    let stdin_received = Arc::new(tokio::sync::Notify::new());
    let cleanup_progress_sent = Arc::new(tokio::sync::Notify::new());
    fixture
        .engine
        .register_tool_provider(Arc::new(TestStreamingBashProvider {
            output_started: Arc::clone(&output_started),
            stdin_received: Arc::clone(&stdin_received),
            cleanup_progress_sent: Arc::clone(&cleanup_progress_sent),
        }));
    let session_id = fixture
        .engine
        .create_session(selection.clone())
        .expect("streaming session")
        .session_id;
    let run_id = fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id,
                client_run_id: ClientRunId::new(format!("streaming-{command}"))
                    .expect("client run id"),
                selection,
                input: "start streaming test".into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("run started")
        .run_id;
    tokio::time::timeout(test_timeout(2), output_started.notified())
        .await
        .expect("streaming output started");
    let call_id = fixture
        .engine
        .inner
        .store
        .get(session_id)
        .expect("streaming projection")
        .log
        .events()
        .iter()
        .find_map(|event| match &event.payload {
            EventPayload::ToolCallStarted { start } if event.run_id == Some(run_id) => {
                Some(start.tool_call_id)
            }
            _ => None,
        })
        .expect("started tool call");
    (
        fixture,
        session_id,
        run_id,
        call_id,
        stdin_received,
        cleanup_progress_sent,
        captured,
    )
}

pub(crate) async fn run_replay_test_turn(
    fixture: &Fixture,
    session_id: SessionId,
    selection: &RunSelection,
    id: &str,
) {
    fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id,
                client_run_id: ClientRunId::new(id).unwrap(),
                selection: selection.clone(),
                input: id.into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .unwrap();
    wait_for_session_not_running(&fixture.engine, session_id).await;
}

pub(crate) async fn anthropic_replay_fallback_fixture(endpoint: &str) -> (Fixture, RunSelection) {
    let primary = "---\ndescription: Replay fallback test\nmode: primary\nenabled: true\nmodels:\n  - { model: \"custom.test/group/model\", variant: base }\n  - { model: \"custom.test/group/fallback\", variant: base }\npermissions: {}\n---\nReplay fallback.\n";
    let (mut fixture, selection) = custom_fixture_with_capabilities(
        endpoint,
        primary,
        None,
        None,
        false,
        None,
        None,
        4_096,
        None,
        "anthropic-compatible",
        Some(ANTHROPIC_REPLAY_CAPABILITIES),
    );
    fixture.engine.shutdown().await;
    let provider_id = ProviderId::new("custom.test").expect("replay provider ID");
    let ProviderDefinition::Custom(provider) = fixture
        .config
        .runtime
        .providers
        .get_mut(&provider_id)
        .expect("replay provider")
    else {
        panic!("custom replay provider");
    };
    let source_id = ProviderModelId::new("group/model").expect("replay source model ID");
    let fallback_id = ProviderModelId::new("group/fallback").expect("replay fallback model ID");
    provider
        .models
        .insert(fallback_id, provider.models[&source_id].clone());
    let current = fixture.manager.current();
    let manager = Arc::new(
        ModelManager::new(
            fixture.config.runtime.providers.clone(),
            Arc::clone(current.catalog()),
            ProviderStore::open(fixture._directory.path().join("provider-store"))
                .expect("replay provider store"),
        )
        .expect("replay model manager"),
    );
    fixture.engine = Engine::open(EngineOptions {
        data_dir: fixture._directory.path().join("data"),
        cwd: fixture._directory.path().to_owned(),
        config: fixture.config.clone(),
        model_manager: Arc::clone(&manager),
        tools: Vec::new(),
    })
    .expect("replay engine");
    fixture.manager = manager;
    (fixture, selection)
}

pub(crate) fn rejected_unsigned_replay_recovery_count(
    events: &[cookie_agent_protocol::StoredEvent],
) -> usize {
    let marked_attempts = events
        .iter()
        .filter_map(|event| {
            match &event.payload {
            EventPayload::ModelReplayEvaluated {
                attempt_id,
                ordered_decisions,
                ..
            } if ordered_decisions.iter().any(|decision| matches!(
                &decision.disposition,
                cookie_agent_protocol::ReplayDisposition::DiscardedInvalidPayload { reason }
                    if reason.as_str().starts_with("rejected unsigned Anthropic replay artifact ")
            )) => Some(*attempt_id),
            _ => None,
        }
        })
        .collect::<Vec<_>>();
    events
        .iter()
        .filter(|event| {
            matches!(
                event.payload,
                EventPayload::AttemptAbandoned { attempt_id, .. }
                    if marked_attempts.contains(&attempt_id)
            )
        })
        .count()
}

pub(crate) fn delayed_mcp_server(source: McpServerSource) -> LoadedMcpServer {
    let mut env = BTreeMap::new();
    env.insert("MCP_FIXTURE_LIST_DELAY_MS".into(), "100".into());
    LoadedMcpServer {
        source,
        config: McpServerConfig {
            command: Some(python_command().into()),
            args: vec![format!(
                "{}/tests/fixtures/mcp_server.py",
                env!("CARGO_MANIFEST_DIR")
            )],
            env,
            cwd: None,
            url: None,
            headers: BTreeMap::new(),
            oauth: Default::default(),
            enabled: true,
            lazy: false,
            timeout_ms: Some(5_000),
        },
    }
}

pub(crate) fn resume_sweep_test_request() -> cookie_agent_protocol::ApprovalRequest {
    let binding = cookie_agent_protocol::PreparedResourceDigest::from_canonical_binding_bytes(
        b"resume-sweep-binding",
    );
    let resource = PreparedApprovalResource {
        capability: PermissionAction::Bash,
        canonical: PreparedResourceIdentity::new("command:test")
            .expect("prepared resource identity"),
        binding_digest: binding.clone(),
        binding_lifetime: PreparedBindingLifetime::RestartStable,
        boundary: ApprovalBoundary::Exact,
        source: ApprovalResourceSource::PrimaryOperation,
    };
    let operation = PreparedOperationIdentity::new(
        Sha256Digest::of_bytes(b"resume-sweep-args"),
        vec![ApprovalCapability {
            action: PermissionAction::Bash,
            operation: PreparedCapabilityOperation::new("bash:execute").expect("capability"),
        }],
        vec![resource],
        Sha256Digest::of_bytes(b"resume-sweep-context"),
    )
    .expect("prepared operation");
    cookie_agent_protocol::ApprovalRequest::new(
        ApprovalId::new_v7(),
        1,
        cookie_agent_protocol::ApprovalTrigger::PermissionPolicy,
        operation,
        vec![cookie_agent_protocol::ApprovalEvaluation {
            resource_digest: binding,
            effect: PermissionEffect::Ask,
            trace: cookie_agent_protocol::DecisionTrace {
                action: PermissionAction::Bash,
                normalized_resource: "command:test".into(),
                candidates: Vec::new(),
                effect: PermissionEffect::Ask,
                precedence_reason: "resume-sweep-test".into(),
            },
        }],
        cookie_agent_protocol::ApprovalConstraints {
            allow_once: true,
            allow_tree_grant: true,
            cancellable: true,
            expires_at: None,
        },
    )
    .expect("approval request")
}
