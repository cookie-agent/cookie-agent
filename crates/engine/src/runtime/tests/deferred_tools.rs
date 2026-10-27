//! Deferred tools end to end (`docs/site/specs/deferred-tools-cookie.md`):
//! a model that loads deferred tools natively gets an MCP tool deferred,
//! announced, loaded with `tool_load`, and then called.

use std::sync::Arc;

use async_trait::async_trait;
use cookie_agent_protocol::{ClientRunId, EventPayload, RunStartParams};
use cookie_agent_runtime_api::{
    PreparedTool, ProvidedToolSource, SessionToolContext, ToolCall, ToolError,
    ToolPreparationContext, ToolProvider, ToolSpec,
};

use super::support::*;

/// `TestWriteProvider`'s `write`, published as an MCP server's tool, which
/// the deferral policy defers.
struct McpWrite(TestWriteProvider);

#[async_trait]
impl ToolProvider for McpWrite {
    fn provider_id(&self) -> &'static str {
        "test.mcp_write"
    }

    fn tools_for_session(&self, ctx: &SessionToolContext) -> Result<Vec<ToolSpec>, ToolError> {
        self.0.tools_for_session(ctx)
    }

    fn tool_source(&self, _tool_name: &str) -> Option<ProvidedToolSource> {
        Some(ProvidedToolSource::Mcp {
            server: "chrome".into(),
        })
    }

    fn get_permission_name(tool_name: &str) -> Result<Option<&'static str>, ToolError> {
        TestWriteProvider::get_permission_name(tool_name)
    }

    fn get_permission_resource(
        &self,
        name: &str,
        arguments: &serde_json::Value,
    ) -> Result<(Option<&'static str>, Option<String>), ToolError> {
        self.0.get_permission_resource(name, arguments)
    }

    fn get_display_argument(
        &self,
        name: &str,
        arguments: &serde_json::Value,
    ) -> Result<String, ToolError> {
        self.0.get_display_argument(name, arguments)
    }

    async fn prepare(
        &self,
        ctx: ToolPreparationContext,
        call: ToolCall,
    ) -> Result<PreparedTool, ToolError> {
        self.0.prepare(ctx, call).await
    }
}

const NATIVE_CAPABILITIES: &str = "input = [\"text\"]\noutput = [\"text\"]\ncontext_tokens = 64000\noutput_tokens = 1024\ntool_calling = true\nparallel_tool_calls = true\nstructured_output = false\nreasoning = false\ntemperature = true\ntop_p = true\nseed = true\nnative_replay = \"unsupported\"\ndeferred_tools = \"native\"\nmedia = {}";

fn tool_names(request: &serde_json::Value) -> Vec<String> {
    request["tools"]
        .as_array()
        .map(|tools| {
            tools
                .iter()
                .filter_map(|tool| tool["function"]["name"].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

fn body(request: &str) -> serde_json::Value {
    let (_, body) = request.split_once("\r\n\r\n").expect("request body");
    serde_json::from_str(body).expect("request JSON")
}

#[tokio::test]
async fn an_mcp_tool_is_announced_loaded_and_called() {
    let (endpoint, responses, captured) = scripted_channel_server(3).await;
    for (role, reply) in [
        (
            "user",
            scripted_tool_body(
                "load-1",
                "tool_load",
                serde_json::json!({"names": ["write"]}),
            ),
        ),
        (
            "system",
            scripted_tool_body("write-1", "write", serde_json::json!({"value": "x"})),
        ),
        ("tool", scripted_text_body("done")),
    ] {
        responses
            .send(MatchedScriptedResponse::last_message_role(role, reply))
            .expect("scripted reply");
    }
    let (fixture, selection) = custom_fixture_with_capabilities_and_variants(
        &endpoint,
        "---\ndescription: Deferral test\nmode: primary\nenabled: true\nmodels: [{ model: \"custom.test/group/model\", variant: base }]\npermissions:\n  write: allow\n---\nUse the tools.\n",
        None,
        None,
        false,
        None,
        None,
        64_000,
        None,
        "openai-compatible",
        Some(NATIVE_CAPABILITIES),
        None,
    );
    fixture.engine.register_base_layer().expect("base layer");
    let executed = Arc::new(TestFlag::default());
    fixture.engine.register_tool_provider(
        crate::ToolLayer::Local,
        Arc::new(McpWrite(TestWriteProvider {
            executed: Arc::clone(&executed),
        })),
    );
    let session = fixture
        .engine
        .create_session(selection.clone())
        .expect("deferral session");
    fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: session.session_id,
                client_run_id: ClientRunId::new("deferral-1").unwrap(),
                selection,
                content: "write x".into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("deferral run");
    wait_for_session_not_running(&fixture.engine, session.session_id).await;
    let requests = with_watchdog("captured deferral requests", captured)
        .await
        .expect("captured requests")
        .iter()
        .map(|request| body(request))
        .collect::<Vec<_>>();
    assert_eq!(requests.len(), 3);

    // The deferred tool is never declared up front; the loaders are.
    for request in &requests {
        let names = tool_names(request);
        assert!(names.contains(&"tool_load".to_owned()), "{names:?}");
        assert!(names.contains(&"tool_search".to_owned()), "{names:?}");
        assert!(!names.contains(&"write".to_owned()), "{names:?}");
    }
    let first = requests[0]["messages"].as_array().expect("messages");
    let announcement = first
        .iter()
        .filter_map(|message| message["content"].as_str())
        .find(|content| content.starts_with("[cookie: deferred tools]"))
        .expect("announcement turn");
    assert!(
        announcement.contains("<group name=\"chrome\" source=\"mcp\">\nwrite\n</group>"),
        "{announcement}"
    );

    // Kimi's dialect loads the tool with a system message right after the
    // load result.
    let second = requests[1]["messages"].as_array().expect("messages");
    let loaded = second.last().expect("tools message");
    assert_eq!(loaded["role"], "system");
    assert!(loaded.get("content").is_none());
    assert_eq!(loaded["tools"][0]["function"]["name"], "write");
    let load_result = &second[second.len() - 2];
    assert_eq!(load_result["role"], "tool");
    assert_eq!(load_result["content"], "Loaded tools: write.");

    assert!(executed.is_set(), "the loaded tool ran");

    let events = fixture
        .engine
        .inner
        .store
        .get(session.session_id)
        .unwrap()
        .log
        .events();
    let rounds = events
        .iter()
        .filter(|event| matches!(event.payload, EventPayload::ToolRoundStarted { .. }))
        .count();
    let announcements = events
        .iter()
        .filter(|event| matches!(event.payload, EventPayload::DeferredToolsAnnounced { .. }))
        .count();
    assert_eq!(
        (rounds, announcements),
        (1, 1),
        "recorded once, then reused"
    );
}
