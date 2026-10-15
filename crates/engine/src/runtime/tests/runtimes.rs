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
        adapter_family: crate::policy::tool_adapter_family(binding.descriptor.adapter_id.as_str()),
        capabilities: policy
            .model_capabilities(&binding)
            .expect("model capabilities"),
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

    fn get_permission_name(tool_name: &str) -> Result<&'static str, ToolError> {
        TestUnconditionalProvider::get_permission_name(tool_name)
    }

    fn get_permission_resource(
        &self,
        name: &str,
        arguments: &serde_json::Value,
    ) -> Result<(&'static str, Option<String>), ToolError> {
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
    let policy = frozen_root_policy(&fixture, &selection);
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
    assert_eq!(
        local_runtime.kind(),
        cookie_agent_runtime_api::RuntimeKind::Coding
    );
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
        adapter_family: crate::policy::tool_adapter_family(binding.descriptor.adapter_id.as_str()),
        capabilities: policy
            .model_capabilities(&binding)
            .expect("model capabilities"),
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
    let policy = frozen_root_policy(&fixture, &selection);
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

    let api = workspace_dir(root, "api");
    let attached = fixture
        .engine
        .attach_runtime(session, "labpc", Some(api.clone()), origin())
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
    assert_eq!(
        prepared[0].attachment().map(|name| name.as_str()),
        Some("labpc_api")
    );
    assert_eq!(
        prepared[0].origin().and_then(ToolOrigin::workspace_root),
        Some(api.canonicalize().unwrap().as_path())
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
                    .attach_runtime(session, "labpc", Some(workspace), origin())
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
        .attach_runtime(session, "labpc", Some(first), origin())
        .await;
    assert!(
        matches!(&again, Err(EngineError::Runtime(message)) if message.contains("already attached as `labpc_api`")),
        "{again:?}"
    );
    assert_eq!(labpc.starts.load(std::sync::atomic::Ordering::SeqCst), 2);

    for (runtime, workspace, expected) in [
        ("nowhere", None, "unknown runtime `nowhere`"),
        ("labpc", None, "runtime `labpc` requires a workspace"),
        (
            "local",
            Some(root.to_owned()),
            "runtime `local` takes no workspace",
        ),
    ] {
        let error = fixture
            .engine
            .attach_runtime(session, runtime, workspace, origin())
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
async fn a_log_without_attachment_events_reads_as_global_and_local() {
    let (fixture, _selection) = custom_fixture();
    let session = create_legacy_session(&fixture.engine, _selection);
    let local_name = fixture
        .engine
        .inner
        .runtimes
        .local()
        .info()
        .attachment_name()
        .to_owned();
    let names = |engine: &crate::Engine| {
        engine
            .session_runtimes(session)
            .expect("attachments")
            .into_iter()
            .map(|attachment| attachment.name.into_string())
            .collect::<Vec<_>>()
    };
    assert_eq!(
        names(&fixture.engine),
        ["global".to_owned(), local_name.clone()]
    );

    // The first detach writes the implicit attachments out first.
    fixture
        .engine
        .detach_runtime(
            session,
            cookie_agent_protocol::AttachmentName::new("global").unwrap(),
            origin(),
        )
        .await
        .expect("detach global");
    assert_eq!(names(&fixture.engine), [local_name]);
    let events = fixture
        .engine
        .inner
        .store
        .get(session)
        .expect("session")
        .log
        .event_snapshot();
    let attachment_events = events
        .iter()
        .filter(|event| {
            matches!(
                event.payload,
                EventPayload::RuntimeAttached { .. } | EventPayload::RuntimeDetached { .. }
            )
        })
        .count();
    assert_eq!(attachment_events, 3);
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
                input: "hold".into(),
            },
            origin(),
        )
        .await
        .expect("start run");
    reached.await.expect("request reached");
    assert!(matches!(
        fixture.engine.attach_runtime(session, "labpc", None, origin()).await,
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
        .attach_runtime(session, "labpc", None, origin())
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
        .attach_runtime(session, "labpc", Some(api), origin())
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
    let policy = frozen_root_policy_for(&reopened, &selection);
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
