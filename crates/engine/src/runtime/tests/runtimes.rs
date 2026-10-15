use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use cookie_agent_protocol::{RunId, ToolCallId};

use cookie_agent_runtime_api::ToolCollision;

use crate::{
    PreparedTool, SessionToolContext, ToolCall, ToolError, ToolLayer, ToolOrigin,
    ToolPreparationContext, ToolProvider, ToolSpec, TurnAgentContext,
};

use super::support::*;

/// An always-allowed `write` tool that declares `collision` and records the
/// origin it was prepared under.
struct OriginProbe {
    id: &'static str,
    collision: ToolCollision,
    inner: TestUnconditionalProvider,
    prepared: Arc<Mutex<Vec<ToolOrigin>>>,
}

impl OriginProbe {
    fn new(id: &'static str, collision: ToolCollision) -> Self {
        Self {
            id,
            collision,
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
    assert_eq!(runtime_id(&published.origin), Some("global"));
    assert!(published.origin.workspace().is_none());

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
    assert_eq!(runtime_id(&published.origin), Some("local"));
    let local_runtime = published.origin.runtime().expect("local runtime");
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
async fn two_regular_tools_with_one_name_stay_a_composition_error() {
    let (fixture, selection) = custom_fixture();
    let policy = frozen_root_policy(&fixture, &selection);
    let session = fixture
        .engine
        .create_session(selection)
        .expect("session")
        .session_id;
    fixture.engine.register_tool_provider(
        ToolLayer::Global,
        Arc::new(OriginProbe::new("test.global_write", ToolCollision::Prefix)),
    );
    fixture.engine.register_tool_provider(
        ToolLayer::Local,
        Arc::new(OriginProbe::new("test.local_write", ToolCollision::Prefix)),
    );
    let Err(error) = fixture.engine.published_tool_definitions(session, &policy) else {
        panic!("duplicate regular tools must not compose");
    };
    assert!(
        error
            .to_string()
            .contains("tool name `write` is published by more than one provider"),
        "{error}"
    );
    fixture.engine.shutdown().await;
}
