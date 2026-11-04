//! Deferred tools end to end (`docs/site/specs/deferred-tools-cookie.md`):
//! a model that loads deferred tools natively gets an MCP tool deferred,
//! announced, loaded with `tool_search`, and then called; a checkpoint folds
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
/// the deferral policy defers unless the server always loads it, while
/// `present` holds.
struct McpWrite(TestWriteProvider, Arc<AtomicBool>, bool);

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

    fn source_summary(&self, _tool_name: &str) -> Option<String> {
        Some("Browser automation.".into())
    }

    fn always_load(&self, _tool_name: &str) -> bool {
        self.2
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
    always_load_fixture(endpoint, false)
}

fn always_load_fixture(
    endpoint: &str,
    always_load: bool,
) -> (Fixture, RunSelection, Arc<TestFlag>, Arc<AtomicBool>) {
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
    // The built-in groups go up front, so only the MCP tool is deferred.
    always_load_groups(&fixture, true);
    let executed = Arc::new(TestFlag::default());
    let present = Arc::new(AtomicBool::new(true));
    fixture.engine.register_tool_provider(
        crate::ToolLayer::Local,
        Arc::new(McpWrite(
            TestWriteProvider {
                executed: Arc::clone(&executed),
            },
            Arc::clone(&present),
            always_load,
        )),
    );
    (fixture, selection, executed, present)
}

/// Sends every deferred built-in group up front, or none.
fn always_load_groups(fixture: &Fixture, all: bool) {
    let mut config = (**fixture.engine.inner.config.load()).clone();
    config.runtime.deferred_tools.always_load = if all {
        cookie_agent_config::DEFERRED_BUILTIN_GROUPS
            .iter()
            .map(|group| (*group).to_owned())
            .collect()
    } else {
        Vec::new()
    };
    fixture.engine.inner.config.store(Arc::new(config));
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
                "tool_search",
                serde_json::json!({"query": "select:write"}),
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
        assert!(names.contains(&"tool_search".to_owned()), "{names:?}");
        assert!(!names.contains(&"tool_load".to_owned()), "{names:?}");
        assert!(!names.contains(&"write".to_owned()), "{names:?}");
    }
    let first = requests[0]["messages"].as_array().expect("messages");
    let announcement = first
        .iter()
        .filter_map(|message| message["content"].as_str())
        .find(|content| content.contains(cookie_agent_protocol::model_text::DEFERRED_TOOLS_ADDED))
        .expect("announcement turn");
    assert!(
        announcement.contains(
            "<group name=\"chrome\" source=\"mcp\" summary=\"Browser automation.\">\nwrite\n</group>"
        ),
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
    let output = second[load_index]["content"].as_str().expect("load output");
    assert!(
        output.starts_with("Loaded 1 tool:\n- write [chrome]"),
        "{output}"
    );
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
    // Clients see `write` searchable, then provided once loaded.
    let published = events
        .iter()
        .filter_map(|event| match &event.payload {
            EventPayload::ModelToolsPublished {
                deferred_tool_names,
                ..
            } => Some(deferred_tool_names.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(published, [vec!["write".to_owned()], Vec::new()]);
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
    assert!(
        !messages.contains("The following deferred tool groups"),
        "{messages}"
    );
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
    assert!(
        !messages.contains("The following deferred tool groups"),
        "{messages}"
    );
    assert!(
        messages.contains("write x"),
        "the run's input stays: {messages}"
    );
}

/// A custom Anthropic-compatible model: deferral is attempted, not verified.
const ATTEMPTED_CAPABILITIES: &str = "input = [\"text\"]\noutput = [\"text\"]\ncontext_tokens = 64000\noutput_tokens = 1024\ntool_calling = true\nparallel_tool_calls = true\nstructured_output = false\nreasoning = false\ntemperature = true\ntop_p = true\nseed = false\nnative_replay = \"unsupported\"\nmedia = {}";

fn deferred_names(request: &serde_json::Value) -> Vec<&str> {
    request["tools"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|tool| tool["defer_loading"] == true)
        .filter_map(|tool| tool["name"].as_str())
        .collect()
}

#[tokio::test]
async fn a_refused_deferral_is_retried_up_front_and_remembered() {
    let (endpoint, captured) = anthropic_replay_server(vec![
        AnthropicReplayResponse::Status400,
        AnthropicReplayResponse::Text("hello"),
        AnthropicReplayResponse::Text("again"),
    ])
    .await;
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
        "anthropic-compatible",
        Some(ATTEMPTED_CAPABILITIES),
        None,
    );
    fixture.engine.register_base_layer().expect("base layer");
    always_load_groups(&fixture, true);
    fixture.engine.register_tool_provider(
        crate::ToolLayer::Local,
        Arc::new(McpWrite(
            TestWriteProvider {
                executed: Arc::new(TestFlag::default()),
            },
            Arc::new(AtomicBool::new(true)),
            false,
        )),
    );
    let session = fixture
        .engine
        .create_session(selection.clone())
        .expect("deferral session");
    run(&fixture, session.session_id, &selection, "deferral-1", "hi").await;
    run(
        &fixture,
        session.session_id,
        &selection,
        "deferral-2",
        "more",
    )
    .await;
    let requests = with_watchdog("captured deferral requests", captured)
        .await
        .expect("captured requests")
        .iter()
        .map(|request| request_body(request))
        .collect::<Vec<_>>();
    assert_eq!(requests.len(), 3);
    assert_eq!(deferred_names(&requests[0]), ["write"], "attempted");
    assert!(
        deferred_names(&requests[1]).is_empty(),
        "the retry sends every tool up front"
    );
    let marks = fixture.engine.inner.deferred_tools_record.marks();
    assert_eq!(marks.len(), 1);
    assert!(marks[0].reason.contains("HTTP 400"), "{}", marks[0].reason);
    // The marked model is no longer sent deferred tools, nor the loaders.
    let names = requests[2]["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .collect::<Vec<_>>();
    assert!(deferred_names(&requests[2]).is_empty());
    assert!(names.contains(&"write"), "{names:?}");
    assert!(!names.contains(&"tool_search"), "{names:?}");
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn an_always_loaded_mcp_tool_is_sent_up_front() {
    let (endpoint, responses, captured) = scripted_channel_server(1).await;
    responses
        .send(MatchedScriptedResponse::last_message_role(
            "user",
            scripted_text_body("hello"),
        ))
        .expect("scripted reply");
    let (fixture, selection, _, _) = always_load_fixture(&endpoint, true);
    let session = fixture
        .engine
        .create_session(selection.clone())
        .expect("deferral session");
    run(&fixture, session.session_id, &selection, "deferral-1", "hi").await;
    let requests = with_watchdog("captured deferral requests", captured)
        .await
        .expect("captured requests");
    let request = body(&requests[0]);
    let names = tool_names(&request);
    assert!(names.contains(&"write".to_owned()), "{names:?}");
    assert!(
        !names.iter().any(|name| name.starts_with("tool_")),
        "nothing is deferred, so no loaders: {names:?}"
    );
    let messages = serde_json::to_string(&request["messages"]).expect("messages");
    assert!(
        !messages.contains("The following deferred tool groups"),
        "{messages}"
    );
    fixture.engine.shutdown().await;
}

/// The goal tools, a built-in group the policy defers, are announced under
/// their group rather than sent; `[deferred_tools] always_load` sends them
/// up front (stable prefix spec, section 3).
/// `attach_machine` lists an attached machine's MCP tool by its group while
/// the tool is deferred, and as callable once its server always loads it.
#[tokio::test]
async fn an_attached_machines_deferred_tool_is_listed_by_its_group() {
    for always_load in [false, true] {
        let (fixture, selection, _, present) = deferral_fixture("http://127.0.0.1:9/v1");
        // Only the attached machine offers `write`.
        present.store(false, Ordering::SeqCst);
        fixture
            .engine
            .register_runtime_template(FakeRuntimeTemplate::with_workspace(
                "labpc",
                Arc::new(move |_| {
                    vec![Arc::new(McpWrite(
                        TestWriteProvider {
                            executed: Arc::new(TestFlag::default()),
                        },
                        Arc::new(AtomicBool::new(true)),
                        always_load,
                    )) as Arc<dyn ToolProvider>]
                }),
            ))
            .expect("register labpc");
        let session = fixture
            .engine
            .create_session(selection.clone())
            .expect("deferral session")
            .session_id;
        let workspace = fixture._directory.path().join("labpc");
        create_private_test_dir(&workspace);
        let attached = fixture
            .engine
            .attach_runtime(
                session,
                "labpc",
                None,
                Some(workspace),
                cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
            )
            .await
            .expect("attach labpc");
        let policy = frozen_root_policy(&fixture, &selection);
        let write = fixture
            .engine
            .attachment_tools(session, &policy, &attached.name)
            .expect("attachment tools")
            .into_iter()
            .find(|tool| tool.name == "write")
            .expect("labpc's write");
        assert_eq!(write.machine.as_deref(), Some("labpc"));
        assert_eq!(
            write.deferred.as_deref(),
            (!always_load).then_some("chrome"),
            "always_load {always_load}"
        );
        fixture.engine.shutdown().await;
    }
}

#[tokio::test]
async fn a_deferred_built_in_group_is_announced_unless_always_loaded() {
    for always_load in [false, true] {
        let (endpoint, responses, captured) = scripted_channel_server(1).await;
        responses
            .send(MatchedScriptedResponse::last_message_role(
                "user",
                scripted_text_body("done"),
            ))
            .expect("scripted reply");
        let (fixture, selection, _, _) = deferral_fixture(&endpoint);
        always_load_groups(&fixture, always_load);
        let session = fixture
            .engine
            .create_session(selection.clone())
            .expect("deferral session");
        run(
            &fixture,
            session.session_id,
            &selection,
            "groups-1",
            "hello",
        )
        .await;
        let requests = with_watchdog("captured group requests", captured)
            .await
            .expect("captured requests");
        let request = body(&requests[0]);
        let names = tool_names(&request);
        let announcement = request["messages"]
            .as_array()
            .expect("messages")
            .iter()
            .filter_map(|message| message["content"].as_str())
            .find(|content| {
                content.contains(cookie_agent_protocol::model_text::DEFERRED_TOOLS_ADDED)
            })
            .expect("announcement turn")
            .to_owned();
        let goal_group = "<group name=\"goal\" source=\"builtin\" summary=\"The session's goal and the checklist that tracks it.\">\ngoal_get\ngoal_set\ngoal_update\n</group>";
        if always_load {
            assert!(names.contains(&"goal_set".to_owned()), "{names:?}");
            assert!(!announcement.contains("name=\"goal\""), "{announcement}");
        } else {
            assert!(!names.contains(&"goal_set".to_owned()), "{names:?}");
            assert!(announcement.contains(goal_group), "{announcement}");
        }
        fixture.engine.shutdown().await;
    }
}

/// A provider with no tools whose prompt section the test changes.
struct Section(Arc<std::sync::Mutex<String>>);

#[async_trait]
impl ToolProvider for Section {
    fn provider_id(&self) -> &'static str {
        "test.section"
    }

    fn tools_for_session(&self, _ctx: &SessionToolContext) -> Result<Vec<ToolSpec>, ToolError> {
        Ok(Vec::new())
    }

    fn prompt_sections(
        &self,
        _ctx: &SessionToolContext,
    ) -> Result<Vec<cookie_agent_runtime_api::PromptSection>, ToolError> {
        Ok(vec![cookie_agent_runtime_api::PromptSection {
            title: "Section".into(),
            body: self.0.lock().unwrap().clone(),
        }])
    }

    fn get_permission_name(_tool_name: &str) -> Result<Option<&'static str>, ToolError> {
        Err(ToolError::execution("the section provider has no tools"))
    }

    fn get_permission_resource(
        &self,
        _name: &str,
        _arguments: &serde_json::Value,
    ) -> Result<(Option<&'static str>, Option<String>), ToolError> {
        Err(ToolError::execution("the section provider has no tools"))
    }

    fn get_display_argument(
        &self,
        _name: &str,
        _arguments: &serde_json::Value,
    ) -> Result<String, ToolError> {
        Err(ToolError::execution("the section provider has no tools"))
    }

    async fn prepare(
        &self,
        _ctx: ToolPreparationContext,
        _call: ToolCall,
    ) -> Result<PreparedTool, ToolError> {
        Err(ToolError::execution("the section provider has no tools"))
    }
}

fn system_text(request: &serde_json::Value) -> String {
    request["messages"][0]["content"]
        .as_str()
        .expect("system prompt")
        .to_owned()
}

fn instructions_changed(request: &serde_json::Value) -> Option<String> {
    request["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .filter_map(|message| message["content"].as_str())
        .find(|content| content.starts_with("<system-reminder>\n<instructions_changed>"))
        .map(str::to_owned)
}

/// The system prompt keeps the run's prompt blocks and a change is
/// announced in place, whether the model defers tools or not (stable prefix
/// spec, section 6; frozen context spec, section 6.2).
#[tokio::test]
async fn a_changed_prompt_block_is_announced_whether_the_model_defers_or_not() {
    for defers in [true, false] {
        let (endpoint, responses, captured) = scripted_channel_server(2).await;
        for input in ["one", "two"] {
            responses
                .send(MatchedScriptedResponse::last_message_contains(
                    input,
                    scripted_text_body("done"),
                ))
                .expect("scripted reply");
        }
        let capabilities = if defers {
            NATIVE_CAPABILITIES.to_owned()
        } else {
            NATIVE_CAPABILITIES.replace(
                "deferred_tools = \"native\"",
                "deferred_tools = \"unsupported\"",
            )
        };
        let (fixture, selection) = custom_fixture_with_capabilities_and_variants(
            &endpoint,
            "---\ndescription: Prompt block test\nmode: primary\nenabled: true\nmodels: [{ model: \"custom.test/group/model\", variant: base }]\n---\nWork.\n",
            None,
            None,
            false,
            None,
            None,
            64_000,
            None,
            "openai-compatible",
            Some(&capabilities),
            None,
        );
        fixture.engine.register_base_layer().expect("base layer");
        always_load_groups(&fixture, true);
        let section = Arc::new(std::sync::Mutex::new("Section one.".to_owned()));
        fixture.engine.register_tool_provider(
            crate::ToolLayer::Local,
            Arc::new(Section(Arc::clone(&section))),
        );
        let session = fixture
            .engine
            .create_session(selection.clone())
            .expect("prompt block session");
        run(&fixture, session.session_id, &selection, "blocks-1", "one").await;
        *section.lock().unwrap() = "Section two.".to_owned();
        run(&fixture, session.session_id, &selection, "blocks-2", "two").await;
        let requests = with_watchdog("captured prompt block requests", captured)
            .await
            .expect("captured requests")
            .iter()
            .map(|request| body(request))
            .collect::<Vec<_>>();
        assert!(system_text(&requests[0]).contains("Section one."));
        assert!(instructions_changed(&requests[0]).is_none());
        // Every model keeps its system prompt until a refresh point
        // (frozen context spec, section 6.2).
        assert_eq!(system_text(&requests[1]), system_text(&requests[0]));
        let announcement = instructions_changed(&requests[1]).expect("announced");
        assert!(
            announcement.contains("<block name=\"tool_instructions\">\n"),
            "{announcement}"
        );
        assert!(announcement.contains("Section two."), "{announcement}");
        let recorded = fixture
            .engine
            .inner
            .store
            .get(session.session_id)
            .unwrap()
            .log
            .events()
            .iter()
            .filter_map(|event| match &event.payload {
                EventPayload::PromptBlocksRecorded { composed_live, .. } => Some(*composed_live),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(recorded, [true, false], "defers: {defers}");
        fixture.engine.shutdown().await;
    }
}

/// A deferred tool called before a result loaded it is refused, as the
/// announcement says, and never runs.
#[tokio::test]
async fn a_deferred_tool_called_before_it_is_loaded_is_refused() {
    let (endpoint, responses, captured) = scripted_channel_server(2).await;
    for (role, reply) in [
        (
            "user",
            scripted_tool_body("write-1", "write", serde_json::json!({"value": "x"})),
        ),
        ("tool", scripted_text_body("done")),
    ] {
        responses
            .send(MatchedScriptedResponse::last_message_role(role, reply))
            .expect("scripted reply");
    }
    let (fixture, selection, executed, _) = deferral_fixture(&endpoint);
    let session = fixture
        .engine
        .create_session(selection.clone())
        .expect("deferral session");
    run(
        &fixture,
        session.session_id,
        &selection,
        "unloaded-1",
        "write x",
    )
    .await;
    let requests = with_watchdog("captured deferral requests", captured)
        .await
        .expect("captured requests");
    let second = body(&requests[1]);
    let result = second["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .filter(|message| message["role"] == "tool")
        .filter_map(|message| message["content"].as_str())
        .next_back()
        .expect("tool result")
        .to_owned();
    assert!(
        result.contains("`write` is not loaded: load it with `tool_search`"),
        "{result}"
    );
    assert!(!executed.is_set(), "the unloaded tool never ran");
    fixture.engine.shutdown().await;
}

/// An eager tool whose description and parameters a test changes between
/// runs.
struct Shaped(Arc<std::sync::Mutex<(String, serde_json::Value)>>);

#[async_trait]
impl ToolProvider for Shaped {
    fn provider_id(&self) -> &'static str {
        "test.shaped"
    }

    fn tools_for_session(&self, _ctx: &SessionToolContext) -> Result<Vec<ToolSpec>, ToolError> {
        let (description, parameters) = self.0.lock().unwrap().clone();
        Ok(vec![ToolSpec {
            background: false,
            background_default: false,
            collision: Default::default(),
            output: Default::default(),
            concurrency: Default::default(),
            result_truncation: Default::default(),
            name: "shaped".into(),
            permission_name: Some("read".into()),
            description,
            parameters,
        }])
    }

    fn get_permission_name(_tool_name: &str) -> Result<Option<&'static str>, ToolError> {
        Ok(Some("read"))
    }

    fn get_permission_resource(
        &self,
        _name: &str,
        _arguments: &serde_json::Value,
    ) -> Result<(Option<&'static str>, Option<String>), ToolError> {
        Ok((None, None))
    }

    fn get_display_argument(
        &self,
        _name: &str,
        _arguments: &serde_json::Value,
    ) -> Result<String, ToolError> {
        Ok(String::new())
    }

    async fn prepare(
        &self,
        _ctx: ToolPreparationContext,
        _call: ToolCall,
    ) -> Result<PreparedTool, ToolError> {
        Err(ToolError::execution("the shaped tool is listing-only"))
    }
}

fn shaped_parameters(field: &str) -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "properties": { field: { "type": "string" } }
    })
}

fn tool<'a>(request: &'a serde_json::Value, name: &str) -> &'a serde_json::Value {
    request["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .find(|tool| tool["function"]["name"] == name)
        .map(|tool| &tool["function"])
        .expect("declared tool")
}

/// A changed description of an eager tool waits for the next refresh point;
/// a changed parameter schema is one: the run composes the context afresh,
/// starts a new tool round, and records why (frozen context spec, section
/// 6.1).
#[tokio::test]
async fn a_changed_parameter_schema_refreshes_and_a_changed_description_does_not() {
    for defers in [true, false] {
        let (endpoint, responses, captured) = scripted_channel_server(3).await;
        for input in ["one", "two", "three"] {
            responses
                .send(MatchedScriptedResponse::last_message_contains(
                    input,
                    scripted_text_body("done"),
                ))
                .expect("scripted reply");
        }
        let capabilities = if defers {
            NATIVE_CAPABILITIES.to_owned()
        } else {
            NATIVE_CAPABILITIES.replace(
                "deferred_tools = \"native\"",
                "deferred_tools = \"unsupported\"",
            )
        };
        let (fixture, selection) = custom_fixture_with_capabilities_and_variants(
            &endpoint,
            "---\ndescription: Frozen context test\nmode: primary\nenabled: true\nmodels: [{ model: \"custom.test/group/model\", variant: base }]\n---\nWork.\n",
            None,
            None,
            false,
            None,
            None,
            64_000,
            None,
            "openai-compatible",
            Some(&capabilities),
            None,
        );
        fixture.engine.register_base_layer().expect("base layer");
        let shape = Arc::new(std::sync::Mutex::new((
            "Shaped, first.".to_owned(),
            shaped_parameters("first"),
        )));
        fixture.engine.register_tool_provider(
            crate::ToolLayer::Local,
            Arc::new(Shaped(Arc::clone(&shape))),
        );
        let session = fixture
            .engine
            .create_session(selection.clone())
            .expect("frozen context session");
        run(&fixture, session.session_id, &selection, "frozen-1", "one").await;
        shape.lock().unwrap().0 = "Shaped, described again.".to_owned();
        run(&fixture, session.session_id, &selection, "frozen-2", "two").await;
        shape.lock().unwrap().1 = shaped_parameters("second");
        run(
            &fixture,
            session.session_id,
            &selection,
            "frozen-3",
            "three",
        )
        .await;
        let requests = with_watchdog("captured frozen context requests", captured)
            .await
            .expect("captured requests")
            .iter()
            .map(|request| body(request))
            .collect::<Vec<_>>();
        assert_eq!(
            tool(&requests[1], "shaped")["description"],
            "Shaped, first."
        );
        let field = |request, name: &str| {
            tool(request, "shaped")["parameters"]["properties"]
                .get(name)
                .is_some()
        };
        assert!(field(&requests[1], "first") && !field(&requests[1], "second"));
        assert!(field(&requests[2], "second") && !field(&requests[2], "first"));
        assert_eq!(
            tool(&requests[2], "shaped")["description"],
            "Shaped, described again."
        );
        let events = fixture
            .engine
            .inner
            .store
            .get(session.session_id)
            .unwrap()
            .log
            .events();
        let refreshes = events
            .iter()
            .filter_map(|event| match &event.payload {
                EventPayload::ContextRefreshed { reason, changed } => {
                    Some((*reason, changed.tools.changed.clone()))
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            refreshes,
            [(
                cookie_agent_protocol::RefreshReason::ToolsChanged,
                vec!["shaped".to_owned()]
            )],
            "defers: {defers}"
        );
        let rounds = events
            .iter()
            .filter(|event| matches!(event.payload, EventPayload::ToolRoundStarted { .. }))
            .count();
        assert_eq!(rounds, 2, "defers: {defers}");
        fixture.engine.shutdown().await;
    }
}

/// A machine a client attaches between runs adds its workspace's AGENTS.md
/// where it was attached, as a turn of its own; nothing before it moves and
/// the system prompt stays (frozen context spec, section 6.3).
#[tokio::test]
async fn a_machine_attached_between_runs_adds_its_agents_md_in_place() {
    let (endpoint, responses, captured) = scripted_channel_server(2).await;
    for input in ["one", "two"] {
        responses
            .send(MatchedScriptedResponse::last_message_contains(
                input,
                scripted_text_body("done"),
            ))
            .expect("scripted reply");
    }
    let (fixture, selection, _, _) = deferral_fixture(&endpoint);
    fixture
        .engine
        .register_runtime_template(FakeRuntimeTemplate::with_workspace(
            "labpc",
            Arc::new(|_| Vec::new()),
        ))
        .expect("register labpc");
    let session = fixture
        .engine
        .create_session(selection.clone())
        .expect("attach session")
        .session_id;
    run(&fixture, session, &selection, "attach-1", "one").await;
    let workspace = fixture._directory.path().join("labpc");
    create_private_test_dir(&workspace);
    write_private_test_file(&workspace.join("AGENTS.md"), "labpc workspace rules");
    fixture
        .engine
        .attach_runtime(
            session,
            "labpc",
            None,
            Some(workspace),
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("attach labpc");
    run(&fixture, session, &selection, "attach-2", "two").await;
    let requests = with_watchdog("captured attach requests", captured)
        .await
        .expect("captured requests")
        .iter()
        .map(|request| body(request))
        .collect::<Vec<_>>();
    assert_eq!(system_text(&requests[1]), system_text(&requests[0]));
    let messages = |request: &serde_json::Value| {
        request["messages"]
            .as_array()
            .expect("messages")
            .iter()
            .map(|message| message.to_string())
            .collect::<Vec<_>>()
    };
    let first = messages(&requests[0]);
    let second = messages(&requests[1]);
    // What the first request sent is a prefix of the second.
    assert_eq!(second[..first.len()], first[..]);
    let rules = second
        .iter()
        .position(|message| message.contains("labpc workspace rules"))
        .expect("attached AGENTS.md");
    assert!(rules >= first.len());
    let loaded = fixture
        .engine
        .inner
        .store
        .get(session)
        .unwrap()
        .log
        .events()
        .into_iter()
        .filter(|event| {
            matches!(
                event.payload,
                EventPayload::AgentMdLoaded { in_place: true, .. }
            )
        })
        .count();
    assert_eq!(loaded, 1);
    fixture.engine.shutdown().await;
}

/// An eager group a session turns off mid-round stays declared, is
/// announced once before the next request, and the model reads the notice
/// in its history (session tool groups spec, section 8.2).
#[tokio::test]
async fn an_eager_group_turned_off_mid_round_is_announced_once() {
    let (endpoint, responses, captured) = scripted_channel_server(3).await;
    for input in ["one", "two", "three"] {
        responses
            .send(MatchedScriptedResponse::last_message_contains(
                input,
                scripted_text_body("done"),
            ))
            .expect("scripted reply");
    }
    let capabilities = NATIVE_CAPABILITIES.replace(
        "deferred_tools = \"native\"",
        "deferred_tools = \"unsupported\"",
    );
    let (fixture, selection) = custom_fixture_with_capabilities_and_variants(
        &endpoint,
        "---\ndescription: Notice test\nmode: primary\nenabled: true\nmodels: [{ model: \"custom.test/group/model\", variant: base }]\n---\nWork.\n",
        None,
        None,
        false,
        None,
        None,
        64_000,
        None,
        "openai-compatible",
        Some(&capabilities),
        None,
    );
    fixture.engine.register_base_layer().expect("base layer");
    let session = fixture
        .engine
        .create_session(selection.clone())
        .expect("notice session")
        .session_id;
    run(&fixture, session, &selection, "notice-1", "one").await;
    fixture
        .engine
        .set_session_tools(
            session,
            cookie_agent_protocol::ToolExposure {
                tool_groups: vec![cookie_agent_protocol::ToolGroupRule {
                    selector: "memory".parse().unwrap(),
                    state: cookie_agent_protocol::ToolGroupState::Off,
                    from_project: false,
                }],
                skills: Vec::new(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .unwrap();
    run(&fixture, session, &selection, "notice-2", "two").await;
    run(&fixture, session, &selection, "notice-3", "three").await;
    let requests = with_watchdog("captured notice requests", captured)
        .await
        .expect("captured requests")
        .iter()
        .map(|request| body(request))
        .collect::<Vec<_>>();
    // The round keeps declaring the memory tools.
    assert!(tool_names(&requests[1]).contains(&"memory_read".to_owned()));
    let text = requests[1].to_string();
    assert!(
        text.contains("no longer available in this session")
            && text.contains("<group name=\\\"memory\\\""),
        "{text}"
    );
    let notices = fixture
        .engine
        .inner
        .store
        .get(session)
        .unwrap()
        .log
        .events()
        .iter()
        .filter(|event| {
            matches!(
                event.payload,
                EventPayload::ToolsAvailabilityAnnounced { .. }
            )
        })
        .count();
    assert_eq!(notices, 1, "announced once");
    fixture.engine.shutdown().await;
}
