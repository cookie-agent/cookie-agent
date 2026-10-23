use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use cookie_agent_protocol::{EventOrigin, EventPayload, RunId, SessionId, ToolCallId};

use cookie_agent_runtime_api::ToolCollision;

use crate::{
    EngineError, PreparedTool, SessionToolContext, ToolCall, ToolError, ToolLayer, ToolOrigin,
    ToolPreparationContext, ToolProvider, ToolSpec, TurnAgentContext,
    policy::FrozenRunPolicy,
    runtime::{ToolCallFailureCode, ToolFailure},
};

use super::support::*;

fn probe_tools(collision: ToolCollision) -> FakeRuntimeTools {
    Arc::new(move |_| {
        vec![Arc::new(OriginProbe::new("test.fake_write", collision)) as Arc<dyn ToolProvider>]
    })
}

fn published_names(fixture: &Fixture, session: SessionId, policy: &FrozenRunPolicy) -> Vec<String> {
    fixture
        .engine
        .published_tool_definitions(session, policy)
        .expect("composed tools")
        .definitions
        .into_iter()
        .map(|definition| definition.name)
        .collect()
}

/// `policy` without the default rules that publish the `global` runtime's
/// memory tools, `list_runtimes`, and the cron tools, so they stay out of
/// tests about other tools.
fn without_memory(mut policy: FrozenRunPolicy) -> FrozenRunPolicy {
    policy.agent.permissions.retain(|rule| {
        rule.action != cookie_agent_protocol::PermissionAction::MEMORY
            && rule.action != cookie_agent_protocol::PermissionAction::MEMORY_WRITE
            && rule.action != cookie_agent_protocol::PermissionAction::LIST_RUNTIMES
            && rule.action != cookie_agent_protocol::PermissionAction::CRON
    });
    policy
}

fn test_turn_context(policy: &FrozenRunPolicy) -> Arc<TurnAgentContext> {
    let binding = policy
        .selected_suffix
        .first()
        .expect("frozen binding")
        .clone();
    Arc::new(TurnAgentContext {
        agent: policy.agent.agent.clone(),
        model: binding.selection.model.clone(),
        adapter: crate::policy::wire_adapter(binding.descriptor.adapter_id.as_str()),
        input_media: crate::policy::tool_input_media(
            binding.descriptor.adapter_id.as_str(),
            &policy
                .model_capabilities(&binding)
                .expect("model capabilities"),
        ),
    })
}

fn origin() -> EventOrigin {
    EventOrigin::new("client:test").expect("event origin")
}

/// A directory named `name` inside `parent`, created.
fn workspace_dir(parent: &std::path::Path, name: &str) -> std::path::PathBuf {
    let path = parent.join(name);
    create_private_test_dir(&path);
    path
}

/// An always-allowed `write` tool that declares `collision` and records the
/// origin it was prepared under.
struct OriginProbe {
    id: &'static str,
    collision: ToolCollision,
    /// Publishes the probe's tool under this name instead of `write`.
    name: Option<&'static str>,
    inner: TestUnconditionalProvider,
    prepared: Arc<Mutex<Vec<ToolOrigin>>>,
}

impl OriginProbe {
    fn new(id: &'static str, collision: ToolCollision) -> Self {
        Self {
            id,
            collision,
            name: None,
            inner: TestUnconditionalProvider(TestWriteProvider {
                executed: Arc::new(TestFlag::default()),
            }),
            prepared: Arc::default(),
        }
    }
}

#[async_trait]
impl ToolProvider for OriginProbe {
    fn provider_id(&self) -> &'static str {
        self.id
    }

    fn tools_for_session(&self, ctx: &SessionToolContext) -> Result<Vec<ToolSpec>, ToolError> {
        let mut tools = self.inner.tools_for_session(ctx)?;
        for tool in &mut tools {
            tool.collision = self.collision;
            if let Some(name) = self.name {
                tool.name = name.to_owned();
            }
        }
        Ok(tools)
    }

    fn get_permission_name(tool_name: &str) -> Result<Option<&'static str>, ToolError> {
        TestUnconditionalProvider::get_permission_name(tool_name)
    }

    fn get_permission_resource(
        &self,
        name: &str,
        arguments: &serde_json::Value,
    ) -> Result<(Option<&'static str>, Option<String>), ToolError> {
        self.inner.get_permission_resource(name, arguments)
    }

    fn get_display_argument(
        &self,
        name: &str,
        arguments: &serde_json::Value,
    ) -> Result<String, ToolError> {
        self.inner.get_display_argument(name, arguments)
    }

    async fn prepare(
        &self,
        ctx: ToolPreparationContext,
        call: ToolCall,
    ) -> Result<PreparedTool, ToolError> {
        self.prepared
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(ctx.origin.clone());
        self.inner.prepare(ctx, call).await
    }
}

impl OriginProbe {
    fn renamed(mut self, name: &'static str) -> Self {
        self.name = Some(name);
        self
    }
}

fn runtime_id(origin: &ToolOrigin) -> Option<&str> {
    origin
        .runtime()
        .map(cookie_agent_runtime_api::RuntimeInfo::id)
}

#[tokio::test]
async fn global_fallback_is_published_alone_and_hidden_behind_the_local_tool() {
    let (fixture, selection) = custom_fixture();
    let policy = without_memory(frozen_root_policy(&fixture, &selection));
    let session = fixture
        .engine
        .create_session(selection)
        .expect("session")
        .session_id;
    let global = Arc::new(OriginProbe::new(
        "test.global_write",
        ToolCollision::Fallback,
    ));
    fixture
        .engine
        .register_tool_provider(ToolLayer::Global, global.clone());

    let alone = fixture
        .engine
        .published_tool_definitions(session, &policy)
        .expect("fallback alone");
    assert_eq!(
        alone
            .definitions
            .iter()
            .map(|definition| definition.name.as_str())
            .collect::<Vec<_>>(),
        ["write"]
    );
    let published = &alone.tools["write"];
    assert_eq!(runtime_id(&published.source.origin), Some("global"));
    assert!(published.source.origin.workspace().is_none());

    let local = Arc::new(OriginProbe::new("test.local_write", ToolCollision::Prefix));
    fixture
        .engine
        .register_tool_provider(ToolLayer::Local, local.clone());
    let composed = fixture
        .engine
        .published_tool_definitions(session, &policy)
        .expect("fallback hidden");
    assert_eq!(composed.definitions.len(), 1);
    let published = &composed.tools["write"];
    assert_eq!(runtime_id(&published.source.origin), Some("local"));
    let local_runtime = published.source.origin.runtime().expect("local runtime");
    assert_eq!(local_runtime.kind().as_str(), "coding");
    assert_eq!(
        local_runtime.workspace().expect("local workspace").root(),
        fixture.engine.inner.runtimes.local_workspace_root()
    );

    let binding = policy
        .selected_suffix
        .first()
        .expect("frozen binding")
        .clone();
    let turn_context = Arc::new(TurnAgentContext {
        agent: policy.agent.agent.clone(),
        model: binding.selection.model.clone(),
        adapter: crate::policy::wire_adapter(binding.descriptor.adapter_id.as_str()),
        input_media: crate::policy::tool_input_media(
            binding.descriptor.adapter_id.as_str(),
            &policy
                .model_capabilities(&binding)
                .expect("model capabilities"),
        ),
    });
    let prepared = fixture
        .engine
        .prepare_published_tool_calls(
            session,
            RunId::new_v7(),
            vec![(
                ToolCall {
                    id: ToolCallId::new_v7(),
                    name: "write".into(),
                    arguments: serde_json::json!({"filePath":"notes.txt","content":"x"}),
                },
                Some(published),
            )],
            &policy,
            turn_context,
        )
        .await;
    assert!(prepared[0].prepared.is_ok(), "the local tool prepares");
    assert_eq!(
        prepared[0].origin().and_then(runtime_id),
        Some("local"),
        "the prepared call is traced to the local runtime"
    );
    assert!(global.prepared.lock().unwrap().is_empty());
    let local_prepared = local.prepared.lock().unwrap().clone();
    assert_eq!(local_prepared.len(), 1);
    assert_eq!(
        local_prepared[0].workspace_root(),
        Some(fixture.engine.inner.runtimes.local_workspace_root())
    );
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn the_base_layer_is_registered_once_and_has_no_workspace() {
    let (fixture, selection) = custom_fixture();
    let policy = frozen_root_policy(&fixture, &selection);
    let session = fixture
        .engine
        .create_session(selection)
        .expect("session")
        .session_id;
    fixture
        .engine
        .register_base_layer()
        .expect("register the base layer");
    let published = fixture
        .engine
        .published_tool_definitions(session, &policy)
        .expect("base layer tools");
    let read_artifact = &published.tools["read_artifact"];
    assert!(matches!(read_artifact.source.origin, ToolOrigin::Base));
    assert!(read_artifact.source.origin.workspace().is_none());
    assert!(
        fixture.engine.register_base_layer().is_err(),
        "base provider IDs are reserved once"
    );
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn colliding_coding_tools_are_prefixed_until_one_attachment_detaches() {
    let (fixture, selection) = custom_fixture();
    let policy = without_memory(frozen_root_policy(&fixture, &selection));
    let root = fixture._directory.path();
    let session = fixture
        .engine
        .create_session(selection)
        .expect("session")
        .session_id;
    fixture.engine.register_tool_provider(
        ToolLayer::Local,
        Arc::new(OriginProbe::new("test.local_write", ToolCollision::Prefix)),
    );
    let labpc = FakeRuntimeTemplate::with_workspace("labpc", probe_tools(ToolCollision::Prefix));
    fixture
        .engine
        .register_runtime_template(labpc.clone())
        .expect("register labpc");
    assert_eq!(published_names(&fixture, session, &policy), ["write"]);
    let description = |name: &str| {
        fixture
            .engine
            .published_tool_definitions(session, &policy)
            .expect("composed tools")
            .definitions
            .into_iter()
            .find(|definition| definition.name == name)
            .expect("published tool")
            .description
    };
    // One attachment besides `global`: its tools say nothing about it.
    assert!(!description("write").contains("Belongs to attachment"));

    let api = workspace_dir(root, "api");
    let attached = fixture
        .engine
        .attach_runtime(session, "labpc", None, Some(api.clone()), origin())
        .await
        .expect("attach labpc");
    assert_eq!(attached.name.as_str(), "labpc_api");
    let local_name = fixture
        .engine
        .inner
        .runtimes
        .local()
        .info()
        .attachment_name()
        .to_owned();
    let mut expected = vec![format!("{local_name}_write"), "labpc_api_write".to_owned()];
    expected.sort();
    assert_eq!(published_names(&fixture, session, &policy), expected);
    // Several: each attachment's tools say where they run. The fake labpc
    // runtime reports a machine of its own, named after it.
    let daemon = fixture.engine.runtime_machine("local").expect("machine");
    let labpc_description = description("labpc_api_write");
    assert!(
        labpc_description.ends_with(
            "\n\nBelongs to attachment `labpc_api` (runtime `labpc` on machine `labpc`)."
        ),
        "{labpc_description}"
    );
    assert!(
        description(&format!("{local_name}_write")).ends_with(&format!(
            "Belongs to attachment `{local_name}` (runtime `local` on machine `{daemon}`)."
        )),
    );
    // And the system prompt lists both by machine, without `global`.
    let mut composed = policy.clone();
    fixture
        .engine
        .compose_runtimes_prompt(&mut composed, session);
    let prompt = &composed.agent.composed_prompt;
    assert!(
        prompt.contains("This session has several attachments."),
        "{prompt}"
    );
    assert!(prompt.contains(&format!(
        "<machine name=\"{daemon}\" platform=\"{}\">\n<runtime name=\"local\">\n\
         <attachment name=\"{local_name}\" kind=\"coding\"",
        std::env::consts::OS
    )));
    assert!(
        prompt.contains(&format!(
            "<machine name=\"labpc\" platform=\"{}\">\n<runtime name=\"labpc\">\n\
             <attachment name=\"labpc_api\" kind=\"coding\"",
            std::env::consts::OS
        )),
        "{prompt}"
    );
    assert!(!prompt.contains("name=\"global\""));
    // Renaming the machine renames it in the next composition.
    fixture
        .engine
        .rename_machine(
            cookie_agent_protocol::MachineName::new("labpc").unwrap(),
            cookie_agent_protocol::MachineName::new("lab-pc").unwrap(),
        )
        .await
        .expect("rename labpc");
    let mut renamed = policy.clone();
    fixture
        .engine
        .compose_runtimes_prompt(&mut renamed, session);
    assert!(
        renamed
            .agent
            .composed_prompt
            .contains("<machine name=\"lab-pc\"")
    );
    assert!(description("labpc_api_write").ends_with("(runtime `labpc` on machine `lab-pc`)."));

    // A prefixed call runs on its own runtime under the name it knows.
    let published = fixture
        .engine
        .published_tool_definitions(session, &policy)
        .expect("composed tools");
    let labpc_write = &published.tools["labpc_api_write"];
    let prepared = fixture
        .engine
        .prepare_published_tool_calls(
            session,
            RunId::new_v7(),
            vec![(
                ToolCall {
                    id: ToolCallId::new_v7(),
                    name: "labpc_api_write".into(),
                    arguments: serde_json::json!({"value":"x"}),
                },
                Some(labpc_write),
            )],
            &policy,
            test_turn_context(&policy),
        )
        .await;
    assert!(prepared[0].prepared.is_ok(), "the labpc tool prepares");
    assert_eq!(prepared[0].call.name, "write");
    let runtime_tool = cookie_agent_protocol::ToolIdentity {
        name: "write".into(),
        published_name: "labpc_api_write".into(),
        source: Some(cookie_agent_protocol::Open::Known(
            cookie_agent_protocol::ToolSource::Runtime {
                runtime_id: cookie_agent_protocol::RuntimeId::new("labpc").unwrap(),
                attachment: cookie_agent_protocol::AttachmentName::new("labpc_api").unwrap(),
                capability: cookie_agent_protocol::CapabilityName::new("coding").unwrap(),
            },
        )),
    };
    assert_eq!(prepared[0].presentation.tool, runtime_tool);
    assert_eq!(
        prepared[0].attachment().map(|name| name.as_str()),
        Some("labpc_api")
    );
    assert_eq!(
        prepared[0].origin().and_then(ToolOrigin::workspace_root),
        Some(
            cookie_agent_protocol::paths::canonicalize(&api)
                .unwrap()
                .as_path()
        )
    );

    fixture
        .engine
        .detach_runtime(session, attached.name.clone(), origin())
        .await
        .expect("detach labpc");
    assert_eq!(published_names(&fixture, session, &policy), ["write"]);

    // What the model was offered before the detach no longer exists.
    let stale = fixture
        .engine
        .prepare_published_tool_calls(
            session,
            RunId::new_v7(),
            vec![(
                ToolCall {
                    id: ToolCallId::new_v7(),
                    name: "labpc_api_write".into(),
                    arguments: serde_json::json!({"value":"x"}),
                },
                Some(labpc_write),
            )],
            &policy,
            test_turn_context(&policy),
        )
        .await;
    assert!(matches!(
        stale[0].prepared,
        Err(ToolFailure {
            code: ToolCallFailureCode::OperationChanged,
            ..
        })
    ));
    // The failure still names the tool the model was offered.
    assert_eq!(stale[0].presentation.title.as_str(), "labpc_api_write");
    assert_eq!(stale[0].presentation.tool, runtime_tool);
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn a_runtime_tool_with_a_base_name_is_prefixed_and_the_base_tool_keeps_it() {
    let (fixture, selection) = custom_fixture();
    let policy = frozen_root_policy(&fixture, &selection);
    let session = fixture
        .engine
        .create_session(selection)
        .expect("session")
        .session_id;
    fixture
        .engine
        .register_base_layer()
        .expect("register the base layer");
    let shadow =
        OriginProbe::new("test.read_artifact", ToolCollision::Prefix).renamed("read_artifact");
    fixture
        .engine
        .register_tool_provider(ToolLayer::Global, Arc::new(shadow));
    let published = fixture
        .engine
        .published_tool_definitions(session, &policy)
        .expect("composed tools");
    assert!(matches!(
        published.tools["read_artifact"].source.origin,
        ToolOrigin::Base
    ));
    assert_eq!(
        published.tools["global_read_artifact"]
            .source
            .attachment
            .as_ref()
            .map(|name| name.as_str()),
        Some("global")
    );
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn sessions_attach_runtimes_under_unique_names() {
    let (fixture, _selection) = custom_fixture();
    let root = fixture._directory.path();
    let session = fixture
        .engine
        .create_session(_selection)
        .expect("session")
        .session_id;
    let labpc = FakeRuntimeTemplate::with_workspace("labpc", probe_tools(ToolCollision::Prefix));
    fixture
        .engine
        .register_runtime_template(labpc.clone())
        .expect("register labpc");

    let first = workspace_dir(&workspace_dir(root, "one"), "api");
    let second = workspace_dir(&workspace_dir(root, "two"), "api");
    let names = [first.clone(), second]
        .into_iter()
        .map(|workspace| {
            let engine = fixture.engine.clone();
            async move {
                engine
                    .attach_runtime(session, "labpc", None, Some(workspace), origin())
                    .await
                    .expect("attach")
                    .name
                    .into_string()
            }
        })
        .collect::<Vec<_>>();
    let mut attached = Vec::new();
    for name in names {
        attached.push(name.await);
    }
    assert_eq!(attached, ["labpc_api", "labpc_api_2"]);

    // One instance per (runtime ID, workspace), shared and never attached twice.
    let again = fixture
        .engine
        .attach_runtime(session, "labpc", None, Some(first), origin())
        .await;
    assert!(
        matches!(&again, Err(EngineError::Runtime(message)) if message.contains("already attached as `labpc_api`")),
        "{again:?}"
    );
    assert_eq!(labpc.starts.load(std::sync::atomic::Ordering::SeqCst), 2);

    for (runtime, workspace, expected) in [
        ("nowhere", None, "unknown runtime `nowhere`"),
        (
            "labpc",
            None,
            "capability `coding` of runtime `labpc` requires a workspace",
        ),
        (
            "local",
            Some(root.to_owned()),
            "capability `coding` of runtime `local` is already attached",
        ),
    ] {
        let error = fixture
            .engine
            .attach_runtime(session, runtime, None, workspace, origin())
            .await
            .expect_err("rejected attach");
        assert!(error.to_string().contains(expected), "{error}");
    }
    let runtimes = fixture
        .engine
        .session_runtimes(session)
        .expect("attachments")
        .into_iter()
        .map(|attachment| attachment.name.into_string())
        .collect::<Vec<_>>();
    let local_name = fixture
        .engine
        .inner
        .runtimes
        .local()
        .info()
        .attachment_name()
        .to_owned();
    assert_eq!(
        runtimes,
        [
            "global".to_owned(),
            local_name,
            "labpc_api".into(),
            "labpc_api_2".into()
        ]
    );
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn attach_and_detach_are_rejected_while_the_session_runs() {
    let response = "data: {\"choices\":[{\"delta\":{\"content\":\"done\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n".to_owned();
    let (endpoint, _captured, reached, release) =
        scripted_server_with_delayed_response(vec![response], 0).await;
    let (fixture, selection) = custom_fixture_with_endpoint(&endpoint);
    let labpc = FakeRuntimeTemplate::fixed(
        "labpc",
        fixture._directory.path(),
        probe_tools(ToolCollision::Prefix),
    );
    fixture
        .engine
        .register_runtime_template(labpc)
        .expect("register labpc");
    let session = fixture
        .engine
        .create_session(selection.clone())
        .expect("session")
        .session_id;
    fixture
        .engine
        .start_run(
            cookie_agent_protocol::RunStartParams {
                reset_fallback: false,
                session_id: session,
                client_run_id: cookie_agent_protocol::ClientRunId::new("attach-run").unwrap(),
                selection,
                content: "hold".into(),
            },
            origin(),
        )
        .await
        .expect("start run");
    reached.await.expect("request reached");
    assert!(matches!(
        fixture.engine.attach_runtime(session, "labpc", None, None, origin()).await,
        Err(EngineError::SessionRunning(id)) if id == session
    ));
    assert!(matches!(
        fixture
            .engine
            .detach_runtime(
                session,
                cookie_agent_protocol::AttachmentName::new("global").unwrap(),
                origin()
            )
            .await,
        Err(EngineError::SessionRunning(id)) if id == session
    ));
    release.notify_one();
    wait_for_session_not_running(&fixture.engine, session).await;
    fixture
        .engine
        .attach_runtime(session, "labpc", None, None, origin())
        .await
        .expect("attach once idle");
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn reopening_a_session_starts_its_attached_instances() {
    let (fixture, selection) = custom_fixture();
    let api = workspace_dir(fixture._directory.path(), "api");
    let template =
        || FakeRuntimeTemplate::with_workspace("labpc", probe_tools(ToolCollision::Prefix));
    fixture
        .engine
        .register_runtime_template(template())
        .expect("register labpc");
    let session = fixture
        .engine
        .create_session(selection.clone())
        .expect("session")
        .session_id;
    fixture
        .engine
        .attach_runtime(session, "labpc", None, Some(api), origin())
        .await
        .expect("attach labpc");
    // Persist the session so another engine can open it.
    fixture
        .engine
        .inner
        .store
        .persist_buffered_session(session)
        .expect("persist");
    fixture.engine.shutdown().await;

    let reopened = reopen_engine(&fixture);
    let labpc = template();
    reopened
        .register_runtime_template(labpc.clone())
        .expect("register labpc again");
    assert_eq!(labpc.starts.load(std::sync::atomic::Ordering::SeqCst), 0);
    reopened.resume(session).await.expect("resume");
    assert_eq!(labpc.starts.load(std::sync::atomic::Ordering::SeqCst), 1);
    let policy = without_memory(frozen_root_policy_for(&reopened, &selection));
    let names = reopened
        .published_tool_definitions(session, &policy)
        .expect("composed")
        .definitions
        .into_iter()
        .map(|definition| definition.name)
        .collect::<Vec<_>>();
    assert_eq!(names, ["write"]);
    reopened.shutdown().await;
}

/// A `labpc` template whose instances never start.
struct UnreachableTemplate;

#[async_trait]
impl crate::RuntimeTemplate for UnreachableTemplate {
    fn runtime_id(&self) -> &str {
        "labpc"
    }

    async fn capabilities(
        &self,
    ) -> Result<Vec<cookie_agent_protocol::RuntimeCapabilityInfo>, String> {
        Err("ssh: connect to host labpc: Connection refused".into())
    }

    fn listing(&self) -> crate::RuntimeListingState {
        crate::RuntimeListingState::default()
    }

    async fn start(
        &self,
        _capability: &cookie_agent_protocol::CapabilityName,
        _workspace: Option<&std::path::Path>,
    ) -> Result<crate::StartedRuntime, crate::StartError> {
        Err(crate::StartError::Connect(
            "ssh: connect to host labpc: Connection refused".into(),
        ))
    }
}

#[tokio::test]
async fn an_instance_that_fails_to_start_on_open_is_reported() {
    let (fixture, selection) = custom_fixture();
    let api = workspace_dir(fixture._directory.path(), "api");
    fixture
        .engine
        .register_runtime_template(FakeRuntimeTemplate::with_workspace(
            "labpc",
            probe_tools(ToolCollision::Prefix),
        ))
        .expect("register labpc");
    let session = fixture
        .engine
        .create_session(selection)
        .expect("session")
        .session_id;
    fixture
        .engine
        .attach_runtime(session, "labpc", None, Some(api.clone()), origin())
        .await
        .expect("attach labpc");
    fixture
        .engine
        .inner
        .store
        .persist_buffered_session(session)
        .expect("persist");
    fixture.engine.shutdown().await;

    let reopened = reopen_engine(&fixture);
    reopened
        .register_runtime_template(Arc::new(UnreachableTemplate))
        .expect("register labpc again");
    let mut failures = reopened.subscribe_runtime_failures();
    reopened.resume(session).await.expect("resume");
    let failure = failures.try_recv().expect("start failure reported");
    assert_eq!(failure.runtime_id, "labpc");
    assert_eq!(
        failure.workspace.as_deref().map(std::path::Path::new),
        Some(
            cookie_agent_protocol::paths::canonicalize(&api)
                .expect("canonical api")
                .as_path()
        )
    );
    assert!(
        failure.reason.contains("Connection refused"),
        "{}",
        failure.reason
    );
    let instance = reopened
        .runtime_instances()
        .into_iter()
        .find(|instance| instance.runtime_id == "labpc")
        .expect("labpc instance");
    assert_eq!(instance.status, crate::RuntimeInstanceStatus::Failed);
    assert_eq!(instance.message.as_deref(), Some(failure.reason.as_str()));
    reopened.shutdown().await;
}

#[tokio::test]
async fn each_coding_attachment_loads_its_own_agent_md() {
    let (endpoint, captured) = scripted_model_server().await;
    let (fixture, selection) = custom_fixture_with_endpoint(&endpoint);
    let root = fixture._directory.path();
    write_private_test_file(&root.join("AGENTS.md"), "local rules");
    let api = workspace_dir(root, "api");
    write_private_test_file(&api.join("AGENTS.md"), "labpc rules");
    fixture
        .engine
        .register_runtime_template(FakeRuntimeTemplate::with_workspace(
            "labpc",
            probe_tools(ToolCollision::Prefix),
        ))
        .expect("register labpc");
    let session = fixture
        .engine
        .create_session(selection.clone())
        .expect("session")
        .session_id;
    fixture
        .engine
        .attach_runtime(session, "labpc", None, Some(api), origin())
        .await
        .expect("attach labpc");
    fixture
        .engine
        .start_run(
            cookie_agent_protocol::RunStartParams {
                reset_fallback: false,
                session_id: session,
                client_run_id: cookie_agent_protocol::ClientRunId::new("agent-md").unwrap(),
                selection,
                content: "read the rules".into(),
            },
            origin(),
        )
        .await
        .expect("start run");
    wait_for_session_not_running(&fixture.engine, session).await;
    let local = fixture
        .engine
        .inner
        .runtimes
        .local()
        .info()
        .attachment_name()
        .to_owned();
    let loaded = fixture
        .engine
        .inner
        .store
        .get(session)
        .expect("session")
        .log
        .event_snapshot()
        .iter()
        .filter_map(|event| match &event.payload {
            EventPayload::AgentMdLoaded {
                entries,
                attachment,
                ..
            } => Some((attachment.to_string(), entries[0].content.clone())),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        loaded,
        [
            (local.clone(), "local rules".to_owned()),
            ("labpc_api".to_owned(), "labpc rules".to_owned()),
        ]
    );
    let history = serde_json::to_string(
        &fixture
            .engine
            .get_history(session, crate::EngineHistoryView::Assembled)
            .await
            .expect("history"),
    )
    .expect("history JSON");
    assert!(
        history.contains(&format!("runtime=\\\"{local}\\\"")),
        "{history}"
    );
    assert!(history.contains("runtime=\\\"labpc_api\\\""), "{history}");
    with_watchdog("captured request", captured)
        .await
        .expect("captured request");
    fixture.engine.shutdown().await;
}

fn write_skill(root: &std::path::Path, name: &str, body: &str) {
    let directory = root.join(".cookie-agent/skills").join(name);
    std::fs::create_dir_all(&directory).expect("skill directory");
    write_private_test_file(
        &directory.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: {body}\n---\n{body}\n"),
    );
}

#[tokio::test]
async fn same_named_project_skills_of_two_attachments_are_prefixed() {
    let (mut fixture, selection) = custom_fixture();
    let root = fixture._directory.path().to_owned();
    write_skill(&root, "deploy", "Local deploy.");
    let api = workspace_dir(&root, "api");
    write_skill(&api, "deploy", "Labpc deploy.");
    let user = root.join("user-skills");
    create_private_test_dir(&user);
    for (name, body) in [("deploy", "User deploy."), ("solo", "User solo.")] {
        let directory = user.join(name);
        std::fs::create_dir_all(&directory).expect("user skill directory");
        write_private_test_file(
            &directory.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: {body}\n---\n{body}\n"),
        );
    }
    fixture.config.skills =
        cookie_agent_config::load_skill_roots(None, Some(&user), &[]).expect("user skills");
    fixture.engine.shutdown().await;
    fixture.engine = reopen_engine(&fixture);
    fixture
        .engine
        .register_runtime_template(FakeRuntimeTemplate::with_workspace(
            "labpc",
            probe_tools(ToolCollision::Prefix),
        ))
        .expect("register labpc");
    let session = fixture
        .engine
        .create_session(selection)
        .expect("session")
        .session_id;
    let listed = |engine: &crate::Engine| {
        engine
            .list_skills(session)
            .expect("skills")
            .skills
            .into_iter()
            .map(|skill| (skill.name, skill.precedence_winner))
            .collect::<Vec<_>>()
    };
    // A project skill shadows the user skill of its name.
    assert_eq!(
        listed(&fixture.engine),
        [
            ("deploy".to_owned(), true),
            ("deploy".to_owned(), false),
            ("solo".to_owned(), true)
        ]
    );

    let attached = fixture
        .engine
        .attach_runtime(session, "labpc", None, Some(api), origin())
        .await
        .expect("attach labpc");
    let local = fixture
        .engine
        .inner
        .runtimes
        .local()
        .info()
        .attachment_name()
        .to_owned();
    // The user `deploy` stays shadowed and no session skill is named
    // `deploy` any more, so it is not listed.
    let mut expected = vec![
        ("labpc_api_deploy".to_owned(), true),
        (format!("{local}_deploy"), true),
        ("solo".to_owned(), true),
    ];
    expected.sort();
    assert_eq!(listed(&fixture.engine), expected);
    fixture
        .engine
        .set_session_permission(
            session,
            &cookie_agent_protocol::PermissionAction::SKILL,
            None,
            cookie_agent_protocol::WildcardPattern::new("*").unwrap(),
            cookie_agent_protocol::PermissionEffect::Allow,
            origin(),
        )
        .await
        .expect("allow skills");
    let rendered = fixture
        .engine
        .get_skill(session, "labpc_api_deploy", "")
        .expect("prefixed skill")
        .rendered;
    assert_eq!(rendered.trim(), "Labpc deploy.");

    fixture
        .engine
        .detach_runtime(session, attached.name, origin())
        .await
        .expect("detach labpc");
    assert_eq!(
        fixture
            .engine
            .get_skill(session, "deploy", "")
            .expect("bare again")
            .rendered
            .trim(),
        "Local deploy."
    );
    fixture.engine.shutdown().await;
}

/// A runtime that offers `coding`, which takes a workspace, and `notes`,
/// which takes none.
struct TwoCapabilityTemplate;

impl TwoCapabilityTemplate {
    fn offered() -> Vec<cookie_agent_protocol::RuntimeCapabilityInfo> {
        let capability =
            |name: &str, takes_workspace| cookie_agent_protocol::RuntimeCapabilityInfo {
                name: cookie_agent_protocol::CapabilityName::new(name).unwrap(),
                kind: cookie_agent_protocol::RuntimeKind::new(name).unwrap(),
                takes_workspace,
                description: String::new(),
            };
        vec![capability("coding", true), capability("notes", false)]
    }
}

#[async_trait]
impl crate::RuntimeTemplate for TwoCapabilityTemplate {
    fn runtime_id(&self) -> &str {
        "devbox"
    }

    async fn capabilities(
        &self,
    ) -> Result<Vec<cookie_agent_protocol::RuntimeCapabilityInfo>, String> {
        Ok(Self::offered())
    }

    fn listing(&self) -> crate::RuntimeListingState {
        crate::RuntimeListingState {
            connected: true,
            capabilities: Self::offered(),
            ..crate::RuntimeListingState::default()
        }
    }

    async fn start(
        &self,
        capability: &cookie_agent_protocol::CapabilityName,
        workspace: Option<&std::path::Path>,
    ) -> Result<crate::StartedRuntime, crate::StartError> {
        let info = match workspace {
            Some(path) => cookie_agent_runtime_api::RuntimeInfo::coding(
                "devbox",
                crate::WorkspaceContext::new(path)
                    .map_err(|error| crate::StartError::Open(error.to_string()))?,
            ),
            None => cookie_agent_runtime_api::RuntimeInfo::new(
                "devbox",
                capability.clone(),
                cookie_agent_protocol::RuntimeKind::new(capability.as_str()).unwrap(),
                &format!("devbox_{capability}"),
                None,
            ),
        };
        Ok(crate::StartedRuntime {
            info,
            providers: Vec::new(),
            skills: cookie_agent_config::SkillRegistry::default(),
            connection: None,
            shared: None,
        })
    }
}

#[tokio::test]
async fn a_session_attaches_each_capability_of_a_runtime_separately() {
    let (fixture, selection) = custom_fixture();
    let api = workspace_dir(fixture._directory.path(), "api");
    fixture
        .engine
        .register_runtime_template(Arc::new(TwoCapabilityTemplate))
        .expect("register devbox");
    let session = fixture
        .engine
        .create_session(selection)
        .expect("session")
        .session_id;
    let attach = |capability: Option<&'static str>, workspace: Option<std::path::PathBuf>| {
        fixture
            .engine
            .attach_runtime(session, "devbox", capability, workspace, origin())
    };

    let error = attach(None, Some(api.clone()))
        .await
        .expect_err("ambiguous");
    assert!(
        error
            .to_string()
            .contains("offers several capabilities; choose one of: coding, notes"),
        "{error}"
    );
    let error = attach(Some("browse"), None).await.expect_err("not offered");
    assert!(
        error
            .to_string()
            .contains("runtime `devbox` offers no capability `browse`"),
        "{error}"
    );
    let error = attach(Some("notes"), Some(api.clone()))
        .await
        .expect_err("notes takes no workspace");
    assert!(error.to_string().contains("takes no workspace"), "{error}");

    let coding = attach(Some("coding"), Some(api)).await.expect("coding");
    let notes = attach(Some("notes"), None).await.expect("notes");
    assert_eq!(coding.name.as_str(), "devbox_api");
    assert_eq!(coding.capability.as_str(), "coding");
    assert_eq!(
        (notes.name.as_str(), notes.capability.as_str()),
        ("devbox_notes", "notes")
    );
    let error = attach(Some("notes"), None)
        .await
        .expect_err("attached once");
    assert!(error.to_string().contains("already attached"), "{error}");

    // The attach events record the capability.
    let recorded = fixture
        .engine
        .inner
        .store
        .get(session)
        .unwrap()
        .log
        .events()
        .iter()
        .filter_map(|event| match &event.payload {
            EventPayload::RuntimeAttached {
                runtime_id,
                capability,
                ..
            } if runtime_id.as_str() == "devbox" => Some(capability.clone()),
            _ => None,
        })
        .map(|capability| capability.into_string())
        .collect::<Vec<_>>();
    assert_eq!(recorded, ["coding", "notes"]);
    fixture.engine.shutdown().await;
}

/// A session's directory is its first workspace as the runtime reported
/// it, never the client's text: a web client's `~/x` and a TUI's absolute
/// path attaching one workspace give one `cwd_identity`, and the session
/// list's directory filter returns both.
#[tokio::test]
async fn a_session_s_directory_is_the_workspace_its_runtime_reported() {
    let (fixture, selection) = custom_fixture();
    let home = workspace_dir(fixture._directory.path(), "home-ada");
    let home = cookie_agent_protocol::paths::canonicalize(&home).unwrap();
    let project = workspace_dir(&home, "x");
    fixture
        .engine
        .register_runtime_template(FakeRuntimeTemplate::with_home(
            "labpc",
            &home,
            probe_tools(ToolCollision::Prefix),
        ))
        .expect("register labpc");
    let create = |spelling: &str| {
        let engine = fixture.engine.clone();
        let selection = selection.clone();
        let spelling = std::path::PathBuf::from(spelling);
        async move {
            engine
                .create_session_with_permissions(
                    selection,
                    None,
                    Vec::new(),
                    crate::SessionRuntimes {
                        runtimes: Some(vec![crate::SessionRuntimeRequest {
                            runtime_id: "labpc".into(),
                            capability: None,
                            workspace: Some(spelling.clone()),
                        }]),
                        global: true,
                        cwd: Some(spelling),
                    },
                )
                .await
                .expect("session")
        }
    };
    let web = create("~/x").await;
    let tui = create(&project.to_string_lossy()).await;
    assert_eq!(web.cwd_identity.as_str(), project.to_string_lossy());
    assert_eq!(tui.cwd_identity, web.cwd_identity);
    for session in [&web, &tui] {
        let workspaces = session
            .attachments
            .iter()
            .map(|attachment| {
                (
                    attachment.runtime_id.as_str(),
                    attachment.workspace.as_deref(),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            workspaces,
            [
                ("global", None),
                ("labpc", Some(project.to_string_lossy().as_ref()))
            ]
        );
    }
    let mut listed = fixture
        .engine
        .list_sessions()
        .into_iter()
        .filter(|session| session.cwd_identity == web.cwd_identity)
        .map(|session| session.session_id)
        .collect::<Vec<_>>();
    listed.sort();
    let mut expected = vec![web.session_id, tui.session_id];
    expected.sort();
    assert_eq!(listed, expected);

    // A session without a workspace keeps the client's directory: the raw
    // text when it does not resolve on the daemon's machine.
    let bare = fixture
        .engine
        .create_session_with_permissions(
            selection,
            None,
            Vec::new(),
            crate::SessionRuntimes {
                runtimes: Some(Vec::new()),
                global: true,
                cwd: Some("~/nowhere".into()),
            },
        )
        .await
        .expect("session without a workspace");
    assert_eq!(bare.cwd_identity.as_str(), "~/nowhere");
    fixture.engine.shutdown().await;
}

/// The session's attachments in its metadata follow attach and detach.
#[tokio::test]
async fn session_metadata_lists_the_attachments_as_they_change() {
    let (fixture, selection) = custom_fixture();
    let session = fixture.engine.create_session(selection).expect("session");
    let names = |meta: &cookie_agent_protocol::SessionMeta| {
        meta.attachments
            .iter()
            .map(|attachment| attachment.runtime_id.as_str().to_owned())
            .collect::<Vec<_>>()
    };
    assert_eq!(names(&session), ["global", "local"]);
    fixture
        .engine
        .register_runtime_template(FakeRuntimeTemplate::with_workspace(
            "labpc",
            probe_tools(ToolCollision::Prefix),
        ))
        .expect("register labpc");
    let api = workspace_dir(fixture._directory.path(), "api");
    let attached = fixture
        .engine
        .attach_runtime(session.session_id, "labpc", None, Some(api), origin())
        .await
        .expect("attach labpc");
    let meta = fixture.engine.get_session(session.session_id).unwrap();
    assert_eq!(names(&meta), ["global", "local", "labpc"]);
    assert_eq!(
        meta.attachments,
        fixture.engine.session_runtimes(session.session_id).unwrap()
    );
    fixture
        .engine
        .detach_runtime(session.session_id, attached.name, origin())
        .await
        .expect("detach labpc");
    let meta = fixture.engine.get_session(session.session_id).unwrap();
    assert_eq!(names(&meta), ["global", "local"]);
    fixture.engine.shutdown().await;
}

/// A daemon serves sessions of any directory: a session created for the
/// client's directory records it and works there with `local`'s `coding`.
#[tokio::test]
async fn a_session_created_for_a_client_directory_works_there() {
    let (fixture, selection) = custom_fixture();
    let client_directory = workspace_dir(fixture._directory.path(), "client-repo");
    let canonical = cookie_agent_protocol::paths::canonicalize(&client_directory).unwrap();
    let session = fixture
        .engine
        .create_session_with_permissions(
            selection.clone(),
            None,
            Vec::new(),
            crate::SessionRuntimes {
                cwd: Some(client_directory.clone()),
                ..crate::SessionRuntimes::default()
            },
        )
        .await
        .expect("session");
    assert_eq!(session.cwd_identity.as_str(), canonical.to_string_lossy());
    let attachments = fixture
        .engine
        .session_runtimes(session.session_id)
        .expect("attachments");
    let coding = attachments
        .iter()
        .find(|attachment| attachment.runtime_id.as_str() == "local")
        .expect("local coding");
    assert_eq!(coding.capability.as_str(), "coding");
    assert_eq!(
        coding.workspace.as_deref(),
        Some(canonical.to_string_lossy().as_ref())
    );
    assert_eq!(coding.name.as_str(), "local_client_repo");

    // Without a directory the daemon's own is used, as before.
    let default = fixture
        .engine
        .create_session_with_permissions(
            selection,
            None,
            Vec::new(),
            crate::SessionRuntimes::default(),
        )
        .await
        .expect("default session");
    assert_ne!(default.cwd_identity, session.cwd_identity);
    fixture.engine.shutdown().await;
}

/// A provider that publishes `tools` as MCP tools do: each checked under
/// `mcp`, whose resource is the tool's own name.
struct FakeMcpTools {
    tools: &'static [&'static str],
}

#[async_trait]
impl ToolProvider for FakeMcpTools {
    fn provider_id(&self) -> &'static str {
        "test.fake_mcp"
    }

    fn tools_for_session(&self, _ctx: &SessionToolContext) -> Result<Vec<ToolSpec>, ToolError> {
        Ok(self
            .tools
            .iter()
            .map(|&name| ToolSpec {
                name: name.to_owned(),
                collision: ToolCollision::default(),
                permission_name: Some("mcp".into()),
                description: format!("[fake] {name}"),
                parameters: serde_json::json!({"type": "object", "properties": {}}),
                concurrency: Default::default(),
                result_truncation: Default::default(),
                output: Default::default(),
                background: false,
                background_default: false,
            })
            .collect())
    }

    fn get_permission_name(_tool_name: &str) -> Result<Option<&'static str>, ToolError> {
        Ok(Some("mcp"))
    }

    fn get_permission_resource(
        &self,
        name: &str,
        _arguments: &serde_json::Value,
    ) -> Result<(Option<&'static str>, Option<String>), ToolError> {
        Ok((Some("mcp"), Some(name.to_owned())))
    }

    fn get_display_argument(
        &self,
        name: &str,
        _arguments: &serde_json::Value,
    ) -> Result<String, ToolError> {
        Ok(name.to_owned())
    }

    async fn prepare(
        &self,
        _ctx: ToolPreparationContext,
        call: ToolCall,
    ) -> Result<PreparedTool, ToolError> {
        Err(ToolError::execution(format!("prepared `{}`", call.name)))
    }
}

/// `policy` whose `mcp` rules are `rules`, without the defaults that
/// publish the `global` runtime's own tools.
fn with_mcp_rules(
    policy: FrozenRunPolicy,
    rules: &[(&str, cookie_agent_protocol::PermissionEffect)],
) -> FrozenRunPolicy {
    use cookie_agent_protocol::{PermissionAction, PermissionRule, WildcardPattern};
    let mut policy = without_memory(policy);
    policy
        .agent
        .permissions
        .retain(|rule| rule.action != PermissionAction::MCP);
    policy
        .agent
        .permissions
        .extend(rules.iter().map(|&(pattern, effect)| PermissionRule {
            runtime: None,
            action: PermissionAction::MCP,
            resource: WildcardPattern::new(pattern).expect("pattern"),
            effect,
        }));
    policy
}

fn mcp_names(fixture: &Fixture, session: SessionId, policy: &FrozenRunPolicy) -> Vec<String> {
    let mut names = published_names(fixture, session, policy)
        .into_iter()
        .filter(|name| name.starts_with("my_chrome_") || name.starts_with("github_"))
        .collect::<Vec<_>>();
    names.sort();
    names
}

#[tokio::test]
async fn mcp_tools_are_offered_by_the_merged_rules_for_their_names() {
    use cookie_agent_protocol::{PermissionAction, PermissionEffect, WildcardPattern};
    let (fixture, selection) = custom_fixture();
    let base = frozen_root_policy(&fixture, &selection);
    let session = fixture
        .engine
        .create_session(selection)
        .expect("session")
        .session_id;
    fixture.engine.register_tool_provider(
        ToolLayer::Global,
        Arc::new(FakeMcpTools {
            tools: &[
                "my_chrome_click",
                "my_chrome_browser_install",
                "github_search",
            ],
        }),
    );

    // A narrow allow offers only the tools it matches.
    let narrow = with_mcp_rules(base.clone(), &[("my_chrome_*", PermissionEffect::Allow)]);
    assert_eq!(
        mcp_names(&fixture, session, &narrow),
        ["my_chrome_browser_install", "my_chrome_click"]
    );
    // `ask` offers every tool it covers.
    let asks = with_mcp_rules(base.clone(), &[("*", PermissionEffect::Ask)]);
    assert_eq!(
        mcp_names(&fixture, session, &asks),
        [
            "github_search",
            "my_chrome_browser_install",
            "my_chrome_click"
        ]
    );
    // A deny pattern hides one tool of an allowed server.
    let one_denied = with_mcp_rules(
        base.clone(),
        &[
            ("my_chrome_*", PermissionEffect::Allow),
            ("my_chrome_browser_install", PermissionEffect::Deny),
        ],
    );
    assert_eq!(
        mcp_names(&fixture, session, &one_denied),
        ["my_chrome_click"]
    );

    // A call to a hidden tool is refused before its provider prepares it;
    // an offered one reaches the provider.
    let turn = test_turn_context(&one_denied);
    let prepare = |name: &str| {
        fixture.engine.prepare_tool_call(
            session,
            RunId::new_v7(),
            ToolCall {
                id: ToolCallId::new_v7(),
                name: name.into(),
                arguments: serde_json::json!({}),
            },
            &one_denied,
            Arc::clone(&turn),
        )
    };
    let refused = prepare("my_chrome_browser_install").await;
    assert_eq!(
        refused.prepared.err().map(|failure| failure.message),
        Some("tool `my_chrome_browser_install` is not enabled for this session".into())
    );
    let offered = prepare("my_chrome_click").await;
    assert_eq!(
        offered.prepared.err().map(|failure| failure.message),
        Some("prepared `my_chrome_click`".into())
    );

    // A session override changes the list at the next composition.
    fixture
        .engine
        .set_session_permission(
            session,
            &PermissionAction::MCP,
            None,
            WildcardPattern::new("github_*").expect("pattern"),
            PermissionEffect::Deny,
            origin(),
        )
        .await
        .expect("session override");
    assert_eq!(
        mcp_names(&fixture, session, &asks),
        ["my_chrome_browser_install", "my_chrome_click"]
    );
    fixture.engine.shutdown().await;
}
