use std::{collections::BTreeMap, sync::Arc, time::Duration};

use cookie_agent_config::PluginConfig;
use cookie_agent_protocol::{
    AgentId, AssistantToolCallRef, CancellationCapability, EventPayload,
    ExtensionAgentBeforeStartParams, ExtensionBusEventParams, ExtensionSessionBeforeCompactParams,
    ExtensionToolAfterResultAction, ExtensionToolAfterResultParams, ExtensionToolBeforeCallAction,
    ExtensionToolBeforeCallParams, Modality, ModelCallId, ModelCapabilities, Notification,
    PersistedToolResult, PluginDiagnosticKind, ReplayCapability, RunId, SafeDisplayText, SessionId,
    StoredEvent, ToolCallId, ToolCallTermination, ToolTerminationOutcome,
};
use indexmap::IndexMap;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use super::{
    Control, PluginDeliveryClass, PluginRegistry, PluginState, plugin_context_id,
    plugin_event_origin,
};

#[test]
fn plugin_rpc_rejections_preserve_selected_diagnostics() {
    let response = || {
        serde_json::from_value(serde_json::json!({"jsonrpc":"2.0","id":1,"error":{"code":-32000,"message":"initialization failed","data":{"reason":"Executable missing","path":"/work/helper","token":"private-token"}}})).unwrap()
    };
    for error in [
        super::parse_initialize(response(), "fixture").unwrap_err(),
        super::parse_ping(response()).unwrap_err(),
        super::parse_tool_call(response()).unwrap_err(),
    ] {
        assert!(error.contains("Executable missing"));
        assert!(error.contains("/work/helper"));
        assert!(!error.contains("private-token"));
    }
}

#[test]
fn plugin_event_origins_preserve_valid_names_and_hash_legacy_names() {
    assert_eq!(plugin_event_origin("fixture").as_str(), "plugin:fixture");
    let legacy = plugin_event_origin("command_handler");
    assert_eq!(legacy.plugin_name().expect("plugin origin").len(), 64);
    assert_eq!(legacy, plugin_event_origin("command_handler"));
}
use crate::{
    events::OutputHub,
    tool_api::{
        ProgressSink, ToolCall, ToolExecutionContext, ToolPreparationContext, ToolProvider,
        TurnAgentContext,
    },
};

const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/fake_plugin.py");
const DECLARATION: &str = r#"[{"name":"fixture_echo","description":"Echo","parameters":{"type":"object","properties":{"text":{"type":"string"},"path":{"type":"string"}}},"permission_name":"fixture_echo","primary_resource_param":"path"}]"#;
const CAPABILITIES: &str = r#"{"producer_messaging":false,"tools":true,"resources":false,"subscribe_events":false,"subscribe_bus":false,"publish_bus":false,"publish_session_events":false,"intercept":[]}"#;
#[cfg(unix)]
const PYTHON: &str = "python3";
#[cfg(windows)]
const PYTHON: &str = "python";

#[cfg(unix)]
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
#[cfg(windows)]
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

struct Harness {
    directory: tempfile::TempDir,
    registry: PluginRegistry,
}

async fn harness(extra_env: &[(&str, &str)], timeout_ms: u64) -> Harness {
    let directory = tempfile::tempdir().expect("plugin test directory");
    let mcp = Arc::new(
        crate::McpRegistry::new(
            BTreeMap::new(),
            directory.path().join("private-oauth").join("oauth.json"),
        )
        .expect("MCP registry"),
    );
    let mut env = BTreeMap::from([
        ("FIXTURE_NAME".into(), "fixture".into()),
        ("FIXTURE_TOOLS".into(), DECLARATION.into()),
        ("FIXTURE_CAPABILITIES".into(), CAPABILITIES.into()),
    ]);
    #[cfg(windows)]
    for name in ["PATH", "PATHEXT", "SYSTEMROOT", "WINDIR", "TEMP", "TMP"] {
        if let Ok(value) = std::env::var(name) {
            env.insert(name.to_owned(), value);
        }
    }
    env.extend(
        extra_env
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned())),
    );
    let registry = PluginRegistry::new(
        BTreeMap::from([(
            "fixture".into(),
            PluginConfig {
                producer_messaging: false,
                command: Some(PYTHON.into()),
                args: vec![FIXTURE.into()],
                env,
                cwd: None,
                enabled: true,
                interception_timeout_ms: 2_000,
                startup_timeout_ms: 10_000,
                shutdown_grace_ms: 3_000,
                tool_timeout_ms: timeout_ms,
            },
        )])
        .into_iter()
        .collect(),
        mcp,
    );
    registry.start_eager(&tokio::runtime::Handle::current());
    let connected = tokio::time::timeout(CONNECT_TIMEOUT, async {
        loop {
            if registry
                .statuses()
                .iter()
                .any(|status| status.state == PluginState::Connected)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await;
    if connected.is_err() {
        panic!(
            "plugin failed to connect with interpreter `{PYTHON}` and fixture `{FIXTURE}`; statuses: {:?}",
            registry.statuses()
        );
    }
    Harness {
        directory,
        registry,
    }
}

async fn multi_harness(plugins: &[(&str, &[(&str, &str)])]) -> Harness {
    let directory = tempfile::tempdir().expect("plugin test directory");
    let mcp = Arc::new(
        crate::McpRegistry::new(
            BTreeMap::new(),
            directory.path().join("private-oauth").join("oauth.json"),
        )
        .expect("MCP registry"),
    );
    let plugins: IndexMap<String, PluginConfig> = plugins
        .iter()
        .map(|(name, extra_env)| {
            let mut env = BTreeMap::from([
                ("FIXTURE_NAME".into(), (*name).to_owned()),
                ("FIXTURE_TOOLS".into(), "[]".into()),
                ("FIXTURE_CAPABILITIES".into(), CAPABILITIES.into()),
            ]);
            env.extend(
                extra_env
                    .iter()
                    .map(|(key, value)| ((*key).to_owned(), (*value).to_owned())),
            );
            #[cfg(windows)]
            for name in ["PATH", "PATHEXT", "SYSTEMROOT", "WINDIR", "TEMP", "TMP"] {
                if let Ok(value) = std::env::var(name) {
                    env.insert(name.to_owned(), value);
                }
            }
            let interception_timeout_ms = env
                .remove("FIXTURE_HOST_INTERCEPTION_TIMEOUT_MS")
                .and_then(|value| value.parse().ok())
                .unwrap_or(2_000);
            (
                (*name).to_owned(),
                PluginConfig {
                    producer_messaging: false,
                    command: Some(PYTHON.into()),
                    args: vec![FIXTURE.into()],
                    env,
                    cwd: None,
                    enabled: true,
                    interception_timeout_ms,
                    startup_timeout_ms: 10_000,
                    shutdown_grace_ms: 3_000,
                    tool_timeout_ms: 1_000,
                },
            )
        })
        .collect();
    let registry = PluginRegistry::new(plugins, mcp);
    registry.start_eager(&tokio::runtime::Handle::current());
    registry.await_eager_ready().await;
    assert!(
        registry
            .statuses()
            .iter()
            .all(|status| status.state == PluginState::Connected)
    );
    Harness {
        directory,
        registry,
    }
}

fn turn_context() -> Arc<TurnAgentContext> {
    Arc::new(TurnAgentContext {
        agent: AgentId::new("test").expect("agent ID"),
        model: "test/model".parse().expect("model key"),
        adapter: cookie_agent_protocol::AdaptorId::OpenaiChat,
        adapter_family: cookie_agent_models::adapters::OvenAdapterFamily::OpenaiChat,
        capabilities: ModelCapabilities {
            input: [Modality::Text].into_iter().collect(),
            output: [Modality::Text].into_iter().collect(),
            context_tokens: 8_192,
            output_tokens: 2_048,
            tool_calling: true,
            parallel_tool_calls: true,
            structured_output: false,
            reasoning: false,
            temperature: true,
            top_p: true,
            seed: false,
            native_replay: ReplayCapability::Optional,
            cancellation: CancellationCapability::LocalOnly,
            media: BTreeMap::new(),
        },
    })
}

async fn prepared(harness: &Harness) -> crate::PreparedTool {
    harness
        .registry
        .prepare(
            ToolPreparationContext {
                session: SessionId::new_v7(),
                run: RunId::new_v7(),
                cwd: harness.directory.path().into(),
                workspace_root: harness.directory.path().into(),
                turn_context: turn_context(),
            },
            ToolCall {
                id: ToolCallId::new_v7(),
                name: "fixture_echo".into(),
                arguments: serde_json::json!({"text":"hello", "path":"src/lib.rs"}),
            },
        )
        .await
        .expect("prepare plugin call")
}

async fn execute(
    harness: &Harness,
    prepared: crate::PreparedTool,
    cancellation: CancellationToken,
) -> Result<cookie_agent_protocol::PersistedToolResult, crate::ToolError> {
    let call_id = ToolCallId::new_v7();
    let executor = prepared
        .executor
        .lock()
        .await
        .take()
        .expect("prepared executor");
    let (progress, _receiver) = tokio::sync::mpsc::channel(1);
    executor
        .execute(ToolExecutionContext {
            session: SessionId::new_v7(),
            run: RunId::new_v7(),
            progress: ProgressSink::new(progress, OutputHub::new(call_id, 1024)),
            cancellation,
            stdin: None,
            turn_context: turn_context(),
            artifacts: crate::ArtifactRouter::open_flat(harness.directory.path().join("artifacts"))
                .expect("artifact store"),
        })
        .await?
        .into_result_for_test()
}

#[tokio::test]
async fn plugin_executor_maps_success_and_error_results() {
    let success = harness(&[], 1_000).await;
    let arguments = serde_json::json!({"text":"hello", "path":"src/lib.rs"});
    assert_eq!(
        success
            .registry
            .get_display_argument("fixture_echo", &arguments)
            .expect("display argument"),
        "src/lib.rs"
    );
    assert_eq!(
        success
            .registry
            .get_permission_resource("fixture_echo", &arguments)
            .expect("permission resource"),
        ("plugin", Some("fixture_echo src/lib.rs".into()))
    );
    let result = execute(&success, prepared(&success).await, CancellationToken::new())
        .await
        .expect("plugin result");
    assert_eq!(result.output, "hello");
    assert_eq!(
        result.metadata,
        serde_json::json!({"plugin": {"is_error": false}})
    );
    success.registry.shutdown().await;

    let error = harness(&[("FIXTURE_TOOL_ERROR", "1")], 1_000).await;
    let result = execute(&error, prepared(&error).await, CancellationToken::new())
        .await
        .expect("plugin error result");
    assert_eq!(
        result.metadata,
        serde_json::json!({"plugin": {"is_error": true}})
    );
    error.registry.shutdown().await;
}

#[tokio::test]
async fn plugin_executor_maps_rpc_error_timeout_and_cancellation() {
    let rpc = harness(&[("FIXTURE_TOOL_RPC_ERROR", "1")], 1_000).await;
    let error = execute(&rpc, prepared(&rpc).await, CancellationToken::new())
        .await
        .expect_err("RPC error");
    assert!(error.to_string().contains("fixture tool RPC error"));
    rpc.registry.shutdown().await;

    let slow = harness(&[("FIXTURE_TOOL_DELAY_MS", "200")], 20).await;
    let error = execute(&slow, prepared(&slow).await, CancellationToken::new())
        .await
        .expect_err("timeout");
    assert!(error.to_string().contains("timed out"));
    slow.registry.shutdown().await;

    let cancelled = harness(&[("FIXTURE_TOOL_DELAY_MS", "200")], 1_000).await;
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let error = execute(&cancelled, prepared(&cancelled).await, cancellation)
        .await
        .expect_err("cancellation");
    assert!(error.to_string().contains("cancelled"));
    cancelled.registry.shutdown().await;
}

#[tokio::test]
async fn crash_during_call_invalidates_prepared_tool_and_listing() {
    let harness = harness(&[("FIXTURE_CRASH_DURING_TOOL", "1")], 1_000).await;
    let stale = prepared(&harness).await;
    let error = execute(&harness, prepared(&harness).await, CancellationToken::new())
        .await
        .expect_err("crash");
    assert!(error.to_string().contains("stopped during tool call"));
    tokio::time::timeout(Duration::from_secs(1), async {
        while harness.registry.statuses()[0].state != PluginState::Failed {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("failed state");
    assert!(
        harness
            .registry
            .tools_for_session(&crate::SessionToolContext::new(SessionId::new_v7()))
            .expect("tool listing")
            .is_empty()
    );
    let executor = stale.executor.lock().await.take().expect("stale executor");
    assert!(executor.revalidate().await.is_err());
    harness.registry.shutdown().await;
}

#[tokio::test]
async fn streams_ordered_events_and_bus_without_self_echo() {
    let marker = tempfile::tempdir().expect("marker directory");
    let event_file = marker.path().join("events.jsonl");
    let bus_file = marker.path().join("bus.jsonl");
    let capabilities = r#"{"producer_messaging":false,"tools":true,"resources":false,"subscribe_events":true,"subscribe_bus":true,"publish_bus":false,"publish_session_events":false,"intercept":[]}"#;
    let harness = harness(
        &[
            ("FIXTURE_CAPABILITIES", capabilities),
            (
                "FIXTURE_EVENT_FILE",
                event_file.to_str().expect("event path"),
            ),
            (
                "FIXTURE_BUS_EVENT_FILE",
                bus_file.to_str().expect("bus path"),
            ),
        ],
        1_000,
    )
    .await;
    let session_id = SessionId::new_v7();
    for seq in [2, 3] {
        let event = StoredEvent {
            engine_version: None,
            origin: None,
            session_id,
            run_id: None,
            seq,
            timestamp: jiff::Timestamp::now(),
            payload: if seq == 2 {
                EventPayload::ToolCallProgress {
                    tool_call_id: cookie_agent_protocol::ToolCallId::new_v7(),
                    message: cookie_agent_protocol::SafeDisplayText::new("bash stdout")
                        .expect("message"),
                    display: Some("partial output".into()),
                }
            } else {
                EventPayload::PluginDiagnostic {
                    plugin: "engine".into(),
                    kind: PluginDiagnosticKind::HookBlocked,
                    message: format!("event {seq}"),
                    count: 1,
                }
            },
        };
        assert!(
            harness
                .registry
                .stream_session_event(&event, None)
                .is_empty()
        );
    }
    assert!(
        harness
            .registry
            .stream_bus_event(
                &ExtensionBusEventParams {
                    session_id,
                    context_id: None,
                    plugin: "other".into(),
                    name: "notice".into(),
                    payload: serde_json::json!({"ok": true}),
                },
                None,
            )
            .is_empty()
    );
    tokio::time::timeout(Duration::from_secs(2), async {
        while !event_file.exists()
            || std::fs::read_to_string(&event_file).map_or(0, |contents| contents.lines().count())
                < 2
            || !bus_file.exists()
        {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("streamed notifications");
    let records = std::fs::read_to_string(&event_file)
        .expect("event file")
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("event JSON"))
        .collect::<Vec<_>>();
    let seqs = records
        .iter()
        .map(|record| record["seq"].clone())
        .collect::<Vec<_>>();
    assert_eq!(seqs, [serde_json::json!(2), serde_json::json!(3)]);
    assert_eq!(records[0]["event"]["display"], "partial output");

    let self_event = StoredEvent {
        engine_version: None,
        origin: Some(cookie_agent_protocol::EventOrigin::new("plugin:fixture").unwrap()),
        session_id,
        run_id: None,
        seq: 4,
        timestamp: jiff::Timestamp::now(),
        payload: EventPayload::PluginEventAdded {
            plugin: "fixture".into(),
            name: "self".into(),
            payload: Value::Null,
        },
    };
    harness
        .registry
        .stream_session_event(&self_event, self_event.origin.as_ref());
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert_eq!(
        std::fs::read_to_string(event_file)
            .expect("event file")
            .lines()
            .count(),
        2
    );
    harness.registry.shutdown().await;
}

#[tokio::test]
async fn dispatches_all_interception_hooks_and_fails_open_on_crash() {
    let capabilities = r#"{"producer_messaging":false,"tools":true,"resources":false,"subscribe_events":false,"subscribe_bus":false,"publish_bus":false,"publish_session_events":false,"intercept":["tool_before_call","tool_after_result","agent_before_start","session_before_compact"]}"#;
    let active = harness(
            &[
                ("FIXTURE_CAPABILITIES", capabilities),
                (
                    "FIXTURE_TOOL_BEFORE_RESULT",
                    r#"{"action":"allow","modified_arguments":{"text":"modified","path":"src/lib.rs"}}"#,
                ),
                (
                    "FIXTURE_TOOL_AFTER_RESULT",
                    r#"{"action":"replace","replacement_content":"replaced"}"#,
                ),
                (
                    "FIXTURE_AGENT_BEFORE_RESULT",
                    r#"{"addendum":"agent addendum"}"#,
                ),
                (
                    "FIXTURE_COMPACT_BEFORE_RESULT",
                    r#"{"addendum":"compact addendum"}"#,
                ),
            ],
            1_000,
        )
        .await;
    let session_id = SessionId::new_v7();
    let before = active
        .registry
        .intercept_tool_before_call(&ExtensionToolBeforeCallParams {
            session_id,
            context_id: plugin_context_id(),
            tool: "fixture_echo".into(),
            arguments: serde_json::json!({"text":"original","path":"src/lib.rs"}),
            permission_name: "fixture_echo".into(),
            resource: Some("src/lib.rs".into()),
        })
        .await;
    assert!(matches!(
        &before[0].1,
        Ok(result)
            if result.action == ExtensionToolBeforeCallAction::Allow
                && result.modified_arguments.as_ref().is_some_and(|value| value["text"] == "modified")
    ));
    let after = active
        .registry
        .intercept_tool_after_result(&ExtensionToolAfterResultParams {
            session_id,
            context_id: plugin_context_id(),
            tool: "fixture_echo".into(),
            arguments: Value::Null,
            result_content: "original".into(),
            is_error: false,
        })
        .await;
    assert!(matches!(
        &after[0].1,
        Ok(result)
            if result.action == ExtensionToolAfterResultAction::Replace
                && result.replacement_content.as_deref() == Some("replaced")
    ));
    assert_eq!(
        active
            .registry
            .intercept_agent_before_start(&ExtensionAgentBeforeStartParams {
                session_id,
                context_id: plugin_context_id(),
                agent_path: "primary".into(),
                prompt_context: Value::Null,
            })
            .await[0]
            .1
            .as_ref()
            .expect("agent interception")
            .addendum
            .as_deref(),
        Some("agent addendum")
    );
    assert_eq!(
        active
            .registry
            .intercept_session_before_compact(&ExtensionSessionBeforeCompactParams {
                session_id,
                context_id: plugin_context_id(),
                checkpoint_id: "checkpoint".into(),
                additions: Vec::new(),
                instructions: None,
            })
            .await[0]
            .1
            .as_ref()
            .expect("compaction interception")
            .addendum
            .as_deref(),
        Some("compact addendum")
    );
    active.registry.shutdown().await;

    let crashed = harness(
        &[
            ("FIXTURE_CAPABILITIES", capabilities),
            ("FIXTURE_CRASH_DURING_INTERCEPT", "1"),
        ],
        1_000,
    )
    .await;
    let result = crashed
        .registry
        .intercept_tool_before_call(&ExtensionToolBeforeCallParams {
            session_id,
            context_id: plugin_context_id(),
            tool: "fixture_echo".into(),
            arguments: serde_json::json!({}),
            permission_name: "fixture_echo".into(),
            resource: None,
        })
        .await;
    assert!(
        result[0]
            .1
            .as_ref()
            .is_err_and(|error| error.contains("crashed"))
    );
    crashed.registry.shutdown().await;
}

#[tokio::test]
async fn tool_before_interception_orders_and_block_short_circuits() {
    let marker = tempfile::tempdir().expect("marker directory");
    let second_file = marker.path().join("second.jsonl");
    let capabilities = r#"{"producer_messaging":false,"tools":false,"resources":false,"subscribe_events":false,"subscribe_bus":false,"publish_bus":false,"publish_session_events":false,"intercept":["tool_before_call"]}"#;
    let first_env = [
        ("FIXTURE_CAPABILITIES", capabilities),
        (
            "FIXTURE_TOOL_BEFORE_RESULT",
            r#"{"action":"allow","modified_arguments":{"step":"first"}}"#,
        ),
    ];
    let second_path = second_file.to_str().expect("second path");
    let second_env = [
        ("FIXTURE_CAPABILITIES", capabilities),
        ("FIXTURE_INTERCEPT_FILE", second_path),
    ];
    let harness = multi_harness(&[("zeta", &first_env), ("alpha", &second_env)]).await;
    let params = ExtensionToolBeforeCallParams {
        session_id: SessionId::new_v7(),
        context_id: plugin_context_id(),
        tool: "example".into(),
        arguments: serde_json::json!({"step":"original"}),
        permission_name: "read".into(),
        resource: None,
    };
    let results = harness.registry.intercept_tool_before_call(&params).await;
    assert_eq!(results.len(), 2);
    assert_eq!(results[0].0, "zeta");
    assert_eq!(results[1].0, "alpha");
    let second: Value = serde_json::from_str(
        std::fs::read_to_string(&second_file)
            .expect("second interception")
            .lines()
            .next()
            .expect("second line"),
    )
    .expect("second JSON");
    assert_eq!(second["params"]["arguments"]["step"], "original");
    harness.registry.shutdown().await;

    let blocked_file = marker.path().join("blocked-second.jsonl");
    let block_env = [
        ("FIXTURE_CAPABILITIES", capabilities),
        (
            "FIXTURE_TOOL_BEFORE_RESULT",
            r#"{"action":"block","reason":"blocked"}"#,
        ),
    ];
    let blocked_path = blocked_file.to_str().expect("blocked path");
    let untouched_env = [
        ("FIXTURE_CAPABILITIES", capabilities),
        ("FIXTURE_INTERCEPT_FILE", blocked_path),
    ];
    let blocked = multi_harness(&[("zeta", &block_env), ("alpha", &untouched_env)]).await;
    let results = blocked.registry.intercept_tool_before_call(&params).await;
    assert_eq!(results.len(), 1);
    assert!(matches!(
        &results[0].1,
        Ok(result) if result.action == ExtensionToolBeforeCallAction::Block
    ));
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert!(!blocked_file.exists());
    blocked.registry.shutdown().await;
}

#[tokio::test]
async fn interception_timeout_fails_open_and_remaining_hooks_continue() {
    let marker = tempfile::tempdir().expect("marker directory");
    let second_file = marker.path().join("second.jsonl");
    let capabilities = r#"{"producer_messaging":false,"tools":false,"resources":false,"subscribe_events":false,"subscribe_bus":false,"publish_bus":false,"publish_session_events":false,"intercept":["tool_before_call"]}"#;
    let slow_env = [
        ("FIXTURE_CAPABILITIES", capabilities),
        ("FIXTURE_INTERCEPT_DELAY_MS", "200"),
        ("FIXTURE_HOST_INTERCEPTION_TIMEOUT_MS", "30"),
    ];
    let second_path = second_file.to_str().expect("second path");
    let steady_env = [
        ("FIXTURE_CAPABILITIES", capabilities),
        ("FIXTURE_INTERCEPT_FILE", second_path),
    ];
    let harness = multi_harness(&[("first", &slow_env), ("second", &steady_env)]).await;
    let results = harness
        .registry
        .intercept_tool_before_call(&ExtensionToolBeforeCallParams {
            session_id: SessionId::new_v7(),
            context_id: plugin_context_id(),
            tool: "example".into(),
            arguments: serde_json::json!({}),
            permission_name: "read".into(),
            resource: None,
        })
        .await;
    assert_eq!(results.len(), 2);
    assert!(
        results[0]
            .1
            .as_ref()
            .is_err_and(|error| error.contains("timed out"))
    );
    assert!(results[1].1.is_ok());
    assert!(second_file.exists());
    harness.registry.shutdown().await;
}

#[tokio::test]
async fn result_agent_and_compaction_hooks_receive_accumulated_state() {
    let marker = tempfile::tempdir().expect("marker directory");
    let alpha_file = marker.path().join("alpha.jsonl");
    let capabilities = r#"{"producer_messaging":false,"tools":false,"resources":false,"subscribe_events":false,"subscribe_bus":false,"publish_bus":false,"publish_session_events":false,"intercept":["tool_after_result","agent_before_start","session_before_compact"]}"#;
    let zeta_env = [
        ("FIXTURE_CAPABILITIES", capabilities),
        (
            "FIXTURE_TOOL_AFTER_RESULT",
            r#"{"action":"replace","replacement_content":"zeta result"}"#,
        ),
        (
            "FIXTURE_AGENT_BEFORE_RESULT",
            r#"{"addendum":"zeta agent"}"#,
        ),
        (
            "FIXTURE_COMPACT_BEFORE_RESULT",
            r#"{"addendum":"zeta compact"}"#,
        ),
    ];
    let alpha_path = alpha_file.to_str().expect("alpha path");
    let alpha_env = [
        ("FIXTURE_CAPABILITIES", capabilities),
        ("FIXTURE_INTERCEPT_FILE", alpha_path),
    ];
    let harness = multi_harness(&[("zeta", &zeta_env), ("alpha", &alpha_env)]).await;
    let session_id = SessionId::new_v7();
    harness
        .registry
        .intercept_tool_after_result(&ExtensionToolAfterResultParams {
            session_id,
            context_id: plugin_context_id(),
            tool: "example".into(),
            arguments: serde_json::json!({}),
            result_content: "original".into(),
            is_error: false,
        })
        .await;
    harness
        .registry
        .intercept_agent_before_start(&ExtensionAgentBeforeStartParams {
            session_id,
            context_id: plugin_context_id(),
            agent_path: "primary".into(),
            prompt_context: serde_json::json!({"system_prompt":"original prompt"}),
        })
        .await;
    harness
        .registry
        .intercept_session_before_compact(&ExtensionSessionBeforeCompactParams {
            session_id,
            context_id: plugin_context_id(),
            checkpoint_id: "checkpoint".into(),
            additions: Vec::new(),
            instructions: None,
        })
        .await;
    let records = std::fs::read_to_string(alpha_file)
        .expect("alpha records")
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("record JSON"))
        .collect::<Vec<_>>();
    assert_eq!(records[0]["params"]["result_content"], "zeta result");
    assert_eq!(
        records[1]["params"]["prompt_context"]["system_prompt"],
        "original prompt\nzeta agent"
    );
    assert_eq!(records[2]["params"]["additions"][0], "zeta compact");
    harness.registry.shutdown().await;
}

#[tokio::test]
async fn full_plugin_buffer_drops_without_blocking_and_counts_loss() {
    let capabilities = r#"{"producer_messaging":false,"tools":true,"resources":false,"subscribe_events":true,"subscribe_bus":false,"publish_bus":false,"publish_session_events":false,"intercept":[]}"#;
    let harness = harness(&[("FIXTURE_CAPABILITIES", capabilities)], 1_000).await;
    let runtime = harness
        .registry
        .inner
        .plugins
        .get("fixture")
        .expect("fixture runtime");
    let original = runtime
        .notifications
        .lock()
        .expect("notification lock")
        .clone();
    *runtime.notifications.lock().expect("notification lock") =
        Arc::new(super::PluginNotificationQueue::new(1));
    let event = StoredEvent {
        engine_version: None,
        origin: None,
        session_id: SessionId::new_v7(),
        run_id: None,
        seq: 2,
        timestamp: jiff::Timestamp::now(),
        payload: EventPayload::ToolCallProgress {
            tool_call_id: cookie_agent_protocol::ToolCallId::new_v7(),
            message: cookie_agent_protocol::SafeDisplayText::new("bash stdout").expect("message"),
            display: Some("flood chunk".into()),
        },
    };
    assert!(
        harness
            .registry
            .stream_session_event(&event, None)
            .is_empty()
    );
    let started = std::time::Instant::now();
    let drops = harness.registry.stream_session_event(&event, None);
    assert!(started.elapsed() < Duration::from_millis(50));
    assert_eq!(drops.len(), 1);
    assert_eq!(drops[0].plugin, "fixture");
    assert_eq!(
        runtime.contexts.lock().expect("contexts lock").active.len(),
        0,
        "queued or failed delivery registered a context grant before host dequeue"
    );
    assert_eq!(harness.registry.statuses()[0].dropped_events, 1);
    *runtime.notifications.lock().expect("notification lock") = original;
    harness.registry.shutdown().await;
}

#[tokio::test]
async fn chunk_flood_evicts_by_priority_without_reordering_terminal() {
    let capabilities = r#"{"producer_messaging":false,"tools":true,"resources":false,"subscribe_events":true,"subscribe_bus":false,"publish_bus":false,"publish_session_events":false,"intercept":[]}"#;
    let harness = harness(&[("FIXTURE_CAPABILITIES", capabilities)], 1_000).await;
    let runtime = harness
        .registry
        .inner
        .plugins
        .get("fixture")
        .expect("fixture runtime");
    let original = runtime
        .notifications
        .lock()
        .expect("notification lock")
        .clone();
    let notifications = Arc::new(super::PluginNotificationQueue::new(17));
    *runtime.notifications.lock().expect("notification lock") = Arc::clone(&notifications);
    let session_id = SessionId::new_v7();
    let call_id = ToolCallId::new_v7();
    let owner = AssistantToolCallRef {
        model_turn_seq: 1,
        content_index: 0,
        model_call_id: ModelCallId::new("plugin-flood-call").expect("model call id"),
        provider_item_id: None,
    };
    let event = |seq, payload| StoredEvent {
        engine_version: None,
        origin: None,
        session_id,
        run_id: None,
        seq,
        timestamp: jiff::Timestamp::now(),
        payload,
    };

    let chunk = |seq| {
        event(
            seq,
            EventPayload::ToolCallProgress {
                tool_call_id: call_id,
                message: SafeDisplayText::new("bash stdout").expect("message"),
                display: Some(format!("chunk {seq}")),
            },
        )
    };
    assert!(
        harness
            .registry
            .stream_session_event(&chunk(1), None)
            .is_empty()
    );
    for seq in 2..=17 {
        assert!(
            harness
                .registry
                .stream_session_event(
                    &event(
                        seq,
                        EventPayload::PluginDiagnostic {
                            plugin: "engine".into(),
                            kind: PluginDiagnosticKind::HookBlocked,
                            message: format!("ordinary {seq}"),
                            count: 1,
                        },
                    ),
                    None,
                )
                .is_empty()
        );
    }
    let first_overflow = harness.registry.stream_session_event(
        &event(
            18,
            EventPayload::PluginDiagnostic {
                plugin: "engine".into(),
                kind: PluginDiagnosticKind::HookBlocked,
                message: "ordinary overflow".into(),
                count: 1,
            },
        ),
        None,
    );
    assert_eq!(first_overflow.len(), 1);
    assert_eq!(first_overflow[0].class, PluginDeliveryClass::Chunk);
    let second_overflow = harness.registry.stream_session_event(
        &event(
            19,
            EventPayload::PluginDiagnostic {
                plugin: "engine".into(),
                kind: PluginDiagnosticKind::HookBlocked,
                message: "second ordinary overflow".into(),
                count: 1,
            },
        ),
        None,
    );
    assert_eq!(second_overflow.len(), 1);
    assert_eq!(second_overflow[0].class, PluginDeliveryClass::Ordinary);
    let terminal_drops = harness.registry.stream_session_event(
        &event(
            20,
            EventPayload::ToolCallTerminated {
                termination: ToolCallTermination {
                    tool_call_id: call_id,
                    owner,
                    outcome: ToolTerminationOutcome::Completed,
                    result: Some(PersistedToolResult {
                        display: None,
                        retained_output: None,
                        title: SafeDisplayText::new("Bash").expect("title"),
                        output: "done".into(),
                        metadata: Value::Null,
                        truncation: None,
                        attachments: Vec::new(),
                        additional_messages: Vec::new(),
                    }),
                    error: None,
                },
            },
        ),
        None,
    );

    assert_eq!(terminal_drops.len(), 1);
    assert_eq!(terminal_drops[0].class, PluginDeliveryClass::Ordinary);
    assert_eq!(harness.registry.statuses()[0].dropped_events, 3);
    let mut delivered = Vec::new();
    while let Some(control) = notifications.pop() {
        let Control::Notify { notification, .. } = control else {
            panic!("event notification")
        };
        let params = notification.params.expect("params");
        delivered.push((
            params["seq"].as_u64().expect("sequence"),
            params["event"]["type"]
                .as_str()
                .expect("event type")
                .to_owned(),
        ));
    }
    assert!(delivered.windows(2).all(|pair| pair[0].0 < pair[1].0));
    assert_eq!(delivered.last(), Some(&(20, "tool_call_terminated".into())));

    *runtime.notifications.lock().expect("notification lock") = original;
    harness.registry.shutdown().await;
}

#[tokio::test]
async fn expired_context_is_spent_and_keeps_only_its_known_session() {
    let capabilities = r#"{"producer_messaging":false,"tools":true,"resources":false,"subscribe_events":false,"subscribe_bus":false,"publish_bus":false,"publish_session_events":false,"intercept":[]}"#;
    let harness = harness(&[("FIXTURE_CAPABILITIES", capabilities)], 1_000).await;
    let runtime = harness
        .registry
        .inner
        .plugins
        .get("fixture")
        .expect("fixture runtime");
    let session_id = SessionId::new_v7();
    runtime.register_context(
        "expiring-context",
        session_id,
        tokio::time::Instant::now() + Duration::from_millis(1),
    );
    tokio::time::sleep(Duration::from_millis(5)).await;
    assert!(matches!(
        runtime.consume_context("expiring-context", SessionId::new_v7()),
        super::PluginEmitContext::Rejected {
            diagnostic_session_id: Some(known_session),
            ..
        } if known_session == session_id
    ));
    harness.registry.shutdown().await;
}

#[tokio::test]
async fn context_lifetime_starts_at_delivery_and_cancelled_queue_entry_stays_spent() {
    let harness = harness(&[], 1_000).await;
    let runtime = harness
        .registry
        .inner
        .plugins
        .get("fixture")
        .expect("fixture runtime");
    let (sender, mut receiver) = tokio::sync::mpsc::channel(2);
    let session_id = SessionId::new_v7();
    sender
        .try_send(Control::Notify {
            notification: Notification::new("plugin/test", None),
            session_id,
            context_id: "delayed".into(),
            context_lifetime: Duration::from_millis(5),
        })
        .expect("queue delayed context");
    tokio::time::sleep(Duration::from_millis(10)).await;
    let Control::Notify {
        context_id,
        context_lifetime,
        ..
    } = receiver.recv().await.expect("delayed control")
    else {
        panic!("expected notification control");
    };
    runtime.register_context(
        &context_id,
        session_id,
        tokio::time::Instant::now() + context_lifetime,
    );
    assert!(matches!(
        runtime.consume_context(&context_id, session_id),
        super::PluginEmitContext::Granted
    ));

    sender
        .try_send(Control::Notify {
            notification: Notification::new("plugin/test", None),
            session_id,
            context_id: "cancelled".into(),
            context_lifetime: Duration::from_secs(1),
        })
        .expect("queue cancelled context");
    runtime.revoke_context("cancelled", session_id);
    let Control::Notify {
        context_id,
        context_lifetime,
        ..
    } = receiver.recv().await.expect("cancelled control")
    else {
        panic!("expected notification control");
    };
    runtime.register_context(
        &context_id,
        session_id,
        tokio::time::Instant::now() + context_lifetime,
    );
    assert!(matches!(
        runtime.consume_context(&context_id, session_id),
        super::PluginEmitContext::Rejected {
            diagnostic_session_id: Some(known_session),
            ..
        } if known_session == session_id
    ));
    harness.registry.shutdown().await;
}
