//! Deferred tools end to end (`docs/site/specs/deferred-tools-cookie.md`):
//! a model that loads deferred tools natively gets an MCP tool deferred,
//! announced, loaded with `tool_load`, and then called; a checkpoint folds
//! the loaded tool into the next round.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use cookie_agent_protocol::{ClientRunId, EventPayload, RunSelection, RunStartParams};
use cookie_agent_runtime_api::{
    PreparedTool, ProvidedToolSource, SessionToolContext, ToolCall, ToolError,
    ToolPreparationContext, ToolProvider, ToolSpec,
};

use super::support::*;

/// `TestWriteProvider`'s `write`, published as an MCP server's tool, which
/// the deferral policy defers, while `present` holds.
struct McpWrite(TestWriteProvider, Arc<AtomicBool>);

#[async_trait]
impl ToolProvider for McpWrite {
    fn provider_id(&self) -> &'static str {
        "test.mcp_write"
    }

    fn tools_for_session(&self, ctx: &SessionToolContext) -> Result<Vec<ToolSpec>, ToolError> {
        if !self.1.load(Ordering::SeqCst) {
            return Ok(Vec::new());
        }
        self.0.tools_for_session(ctx)
    }

    fn prompt_sections(
        &self,
        _ctx: &SessionToolContext,
    ) -> Result<Vec<cookie_agent_runtime_api::PromptSection>, ToolError> {
        Ok(vec![cookie_agent_runtime_api::PromptSection {
            title: "Chrome".into(),
            body: "Snapshot the page before writing.".into(),
        }])
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

fn deferral_fixture(endpoint: &str) -> (Fixture, RunSelection, Arc<TestFlag>, Arc<AtomicBool>) {
    let (fixture, selection) = custom_fixture_with_capabilities_and_variants(
        endpoint,
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
    let present = Arc::new(AtomicBool::new(true));
    fixture.engine.register_tool_provider(
        crate::ToolLayer::Local,
        Arc::new(McpWrite(
            TestWriteProvider {
                executed: Arc::clone(&executed),
            },
            Arc::clone(&present),
        )),
    );
    (fixture, selection, executed, present)
}

async fn run(
    fixture: &Fixture,
    session: cookie_agent_protocol::SessionId,
    selection: &RunSelection,
    id: &str,
    content: &str,
) {
    fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: session,
                client_run_id: ClientRunId::new(id).unwrap(),
                selection: selection.clone(),
                content: content.into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("deferral run");
    wait_for_session_not_running(&fixture.engine, session).await;
}

fn load_then_write(responses: &tokio::sync::mpsc::UnboundedSender<MatchedScriptedResponse>) {
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
            "Snapshot the page before writing.",
            scripted_tool_body("write-1", "write", serde_json::json!({"value": "x"})),
        ),
        ("tool", scripted_text_body("done")),
    ] {
        let response = if role.contains(' ') {
            MatchedScriptedResponse::last_message_contains(role, reply)
        } else {
            MatchedScriptedResponse::last_message_role(role, reply)
        };
        responses.send(response).expect("scripted reply");
    }
}

#[tokio::test]
async fn an_mcp_tool_is_announced_loaded_and_called() {
    let (endpoint, responses, captured) = scripted_channel_server(3).await;
    load_then_write(&responses);
    let (fixture, selection, executed, _) = deferral_fixture(&endpoint);
    let session = fixture
        .engine
        .create_session(selection.clone())
        .expect("deferral session");
    run(
        &fixture,
        session.session_id,
        &selection,
        "deferral-1",
        "write x",
    )
    .await;
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

    // The provider's instructions are not in the system prompt: every tool
    // of it is deferred.
    let system = first[0]["content"].as_str().expect("system prompt");
    assert!(!system.contains("Snapshot the page"), "{system}");

    // Kimi's dialect loads the tool with a system message right after the
    // load result, and the provider's instructions follow.
    let second = requests[1]["messages"].as_array().expect("messages");
    let load_index = second
        .iter()
        .position(|message| message["role"] == "tool")
        .expect("load result");
    assert_eq!(second[load_index]["content"], "Loaded tools: write.");
    let loaded = &second[load_index + 1];
    assert_eq!(loaded["role"], "system");
    assert!(loaded.get("content").is_none());
    assert_eq!(loaded["tools"][0]["function"]["name"], "write");
    let instructions = second[load_index + 2]["content"]
        .as_str()
        .expect("instructions turn");
    assert!(
        instructions.contains(
            "<tool_instructions provider=\"test.mcp_write\">\nSnapshot the page before writing."
        ),
        "{instructions}"
    );

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

/// A first run, a run that loads and calls `write`, a compaction that keeps
/// the second run raw, then a third run whose request it returns, with
/// `write` still published or not.
async fn after_a_compaction(keep_write: bool) -> serde_json::Value {
    let (endpoint, responses, captured) = scripted_channel_server(6).await;
    let (fixture, selection, _, present) = deferral_fixture(&endpoint);
    let session = fixture
        .engine
        .create_session(selection.clone())
        .expect("deferral session");
    let reply = |text: &str| {
        responses
            .send(MatchedScriptedResponse::last_message_role(
                "user",
                scripted_text_body(text),
            ))
            .expect("scripted reply");
    };
    // Long enough that its summary is shorter.
    reply(&"hello ".repeat(600));
    run(&fixture, session.session_id, &selection, "deferral-1", "hi").await;
    load_then_write(&responses);
    run(
        &fixture,
        session.session_id,
        &selection,
        "deferral-2",
        "write x",
    )
    .await;
    reply("the summary");
    assert!(
        fixture
            .engine
            .compact_session(
                session.session_id,
                None,
                cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
            )
            .await
            .expect("compaction")
    );
    let events = fixture
        .engine
        .inner
        .store
        .get(session.session_id)
        .unwrap()
        .log
        .events();
    let commit = events
        .iter()
        .find_map(|event| match &event.payload {
            EventPayload::ContextCheckpointCommitted { commit } => Some(commit.clone()),
            _ => None,
        })
        .expect("checkpoint");
    assert_eq!(commit.folded_tools(), ["write"]);
    assert!(
        commit.boundaries.recent_from_seq.is_some(),
        "the second run stays raw"
    );
    present.store(keep_write, Ordering::SeqCst);
    reply("again");
    run(
        &fixture,
        session.session_id,
        &selection,
        "deferral-3",
        "go on",
    )
    .await;
    let requests = with_watchdog("captured deferral requests", captured)
        .await
        .expect("captured requests");
    fixture.engine.shutdown().await;
    body(&requests[5])
}

#[tokio::test]
async fn a_checkpoint_folds_a_loaded_tool_into_the_next_round() {
    let request = after_a_compaction(true).await;
    // Up front now, with nothing left to load.
    let names = tool_names(&request);
    assert!(names.contains(&"write".to_owned()), "{names:?}");
    assert!(
        !names.iter().any(|name| name.starts_with("tool_")),
        "{names:?}"
    );
    let messages = serde_json::to_string(&request["messages"]).expect("messages");
    // The call to it stays, as a call to a declared tool; the load and its
    // announcement go.
    assert!(messages.contains("write-1"), "{messages}");
    assert!(!messages.contains("load-1"), "{messages}");
    assert!(!messages.contains("[cookie: deferred tools]"), "{messages}");
    assert!(
        !messages.contains("\"tools\""),
        "no load message: {messages}"
    );
    // Its provider's instructions are back in the system prompt, once.
    assert_eq!(
        messages.matches("Snapshot the page").count(),
        1,
        "{messages}"
    );
    assert!(
        request["messages"][0]["content"]
            .as_str()
            .expect("system prompt")
            .contains("Snapshot the page")
    );
}

#[tokio::test]
async fn a_checkpoint_drops_the_calls_of_a_tool_that_left() {
    let request = after_a_compaction(false).await;
    let names = tool_names(&request);
    assert!(
        !names
            .iter()
            .any(|name| name == "write" || name.starts_with("tool_")),
        "{names:?}"
    );
    let messages = serde_json::to_string(&request["messages"]).expect("messages");
    assert!(!messages.contains("write-1"), "{messages}");
    assert!(!messages.contains("load-1"), "{messages}");
    assert!(!messages.contains("[cookie: deferred tools]"), "{messages}");
    assert!(
        messages.contains("write x"),
        "the run's input stays: {messages}"
    );
}
