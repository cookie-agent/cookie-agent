use std::{collections::BTreeMap, fs};

use cookie_agent_config::{McpServerSource, PluginConfig};

use cookie_agent_protocol::{
    ClientRunId, EventPayload, PermissionAction, PermissionEffect, RunSelection, RunStartParams,
    ToolTerminationOutcome, WildcardPattern,
};

use super::support::*;

#[tokio::test]
async fn plugin_publication_streams_bus_but_rejects_unregistered_model_emission() {
    let (mut fixture, selection) = custom_fixture();
    let session = fixture.engine.create_session(selection).expect("session");
    fixture
        .engine
        .set_session_permission(
            session.session_id,
            PermissionAction::Read,
            WildcardPattern::new("*").expect("wildcard"),
            PermissionEffect::Allow,
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("persist session");
    fixture.engine.shutdown().await;

    let event_file = fixture._directory.path().join("plugin-events.jsonl");
    let result_file = fixture._directory.path().join("plugin-results.jsonl");
    fixture.config.plugins.insert(
        "fixture".into(),
        PluginConfig {
            command: Some(python_command().into()),
            args: vec![PLUGIN_FIXTURE.into()],
            env: BTreeMap::from([
                ("FIXTURE_NAME".into(), "fixture".into()),
                (
                    "FIXTURE_CAPABILITIES".into(),
                    r#"{"producer_messaging":false,"tools":false,"resources":false,"subscribe_events":true,"subscribe_bus":true,"publish_bus":true,"publish_session_events":true,"intercept":[]}"#.into(),
                ),
                (
                    "FIXTURE_EMIT_ON_EVENT".into(),
                    r#"{"name":"fixture_notice","payload":{"value":7}}"#.into(),
                ),
                (
                    "FIXTURE_EVENT_FILE".into(),
                    event_file.display().to_string(),
                ),
                (
                    "FIXTURE_EMIT_RESULT_FILE".into(),
                    result_file.display().to_string(),
                ),
            ]),
            cwd: None,
            enabled: true,
            producer_messaging: false,
            interception_timeout_ms: 2_000,
            startup_timeout_ms: 10_000,
            shutdown_grace_ms: 3_000,
            tool_timeout_ms: 30_000,
        },
    );
    let mut other = fixture.config.plugins["fixture"].clone();
    other.enabled = false;
    other.env.insert("FIXTURE_NAME".into(), "other".into());
    fixture.config.plugins.insert("other".into(), other);
    drop(fixture.engine);
    let engine = reopen_engine_parts(&fixture._directory, &fixture.config, &fixture.manager);
    engine.inner.plugins.await_eager_ready().await;

    let mut bus = engine.subscribe_engine_events();
    engine
        .append(
            session.session_id,
            None,
            cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
            EventPayload::PluginDiagnostic {
                plugin: "engine".into(),
                kind: cookie_agent_protocol::PluginDiagnosticKind::HookBlocked,
                message: "trigger".into(),
                count: 1,
            },
        )
        .await
        .expect("trigger event");
    let event = tokio::time::timeout(std::time::Duration::from_secs(3), bus.recv())
        .await
        .expect("bus timeout")
        .expect("bus event");
    assert!(matches!(
        event,
        crate::EngineEvent::PluginEvent { ref plugin, ref name, ref payload, .. }
            if plugin == "fixture" && name == "fixture_notice" && payload["value"] == 7
    ));

    let receipt = tokio::time::timeout(test_timeout(3), async {
        loop {
            if let Ok(contents) = fs::read_to_string(&result_file)
                && let Some(receipt) = contents
                    .lines()
                    .find_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            {
                break receipt;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("plugin emit receipt");
    assert_eq!(receipt["durable"], "rejected");
    assert!(
        engine
            .inner
            .store
            .get(session.session_id)
            .unwrap()
            .log
            .events()
            .iter()
            .all(|event| !matches!(event.payload, EventPayload::PluginEventAdded { .. }))
    );
    assert!(
        engine
            .session_producers(cookie_agent_protocol::SessionProducersParams {
                session_id: session.session_id
            })
            .await
            .unwrap()
            .producers
            .is_empty()
    );
    let streamed = fs::read_to_string(&event_file).expect("streamed events");
    assert_eq!(
        streamed.lines().count(),
        1,
        "published event echoed to source"
    );

    let oversized = engine
        .publish_plugin_emit(crate::plugin::PluginEmitRequest {
            plugin: "fixture".into(),
            session_id: session.session_id,
            context: crate::plugin::PluginEmitContext::Granted,
            name: "oversized".into(),
            payload: serde_json::Value::String("x".repeat(256 * 1024 + 1)),
            publish_bus: true,
            publish_session_events: true,
        })
        .await;
    assert_eq!(
        oversized.bus,
        cookie_agent_protocol::ExtensionEmitStatus::Dropped
    );
    assert_eq!(
        oversized.durable,
        cookie_agent_protocol::ExtensionEmitStatus::Rejected
    );
    let mismatched = engine
        .publish_plugin_emit(crate::plugin::PluginEmitRequest {
            plugin: "fixture".into(),
            session_id: session.session_id,
            context: crate::plugin::PluginEmitContext::Rejected {
                diagnostic_session_id: Some(session.session_id),
                reason: "test mismatch".into(),
            },
            name: "mismatched".into(),
            payload: serde_json::json!({}),
            publish_bus: true,
            publish_session_events: true,
        })
        .await;
    assert_eq!(
        mismatched.durable,
        cookie_agent_protocol::ExtensionEmitStatus::Rejected
    );
    assert!(
        mismatched
            .reason
            .as_deref()
            .is_some_and(|reason| reason.contains("test mismatch"))
    );

    let mut throttled = None;
    for ordinal in 0..=crate::plugin::PLUGIN_EVENTS_PER_SECOND {
        let outcome = engine
            .publish_plugin_emit(crate::plugin::PluginEmitRequest {
                plugin: "fixture".into(),
                session_id: session.session_id,
                context: crate::plugin::PluginEmitContext::Granted,
                name: format!("spam_{ordinal}"),
                payload: serde_json::json!({"ordinal": ordinal}),
                publish_bus: true,
                publish_session_events: false,
            })
            .await;
        if outcome.bus == cookie_agent_protocol::ExtensionEmitStatus::Dropped {
            throttled = Some(outcome);
            break;
        }
    }
    assert!(
        throttled
            .and_then(|outcome| outcome.reason)
            .is_some_and(|reason| reason.contains("40 events per second"))
    );
    let unaffected = engine
        .publish_plugin_emit(crate::plugin::PluginEmitRequest {
            plugin: "other".into(),
            session_id: session.session_id,
            context: crate::plugin::PluginEmitContext::Granted,
            name: "other_notice".into(),
            payload: serde_json::json!({"ok": true}),
            publish_bus: true,
            publish_session_events: false,
        })
        .await;
    assert_eq!(
        unaffected.bus,
        cookie_agent_protocol::ExtensionEmitStatus::Published
    );
    await_projection(
        &engine,
        session.session_id,
        "aggregated quota diagnostic",
        |projection| {
            let events = projection.log.events();
            let rate_limited = events.iter().any(|event| {
                matches!(
                    event.payload,
                    EventPayload::PluginDiagnostic {
                        kind: cookie_agent_protocol::PluginDiagnosticKind::RateLimited,
                        ..
                    }
                )
            });
            let context_mismatch = events.iter().any(|event| {
                matches!(
                    event.payload,
                    EventPayload::PluginDiagnostic {
                        kind: cookie_agent_protocol::PluginDiagnosticKind::ContextMismatch,
                        ..
                    }
                )
            });
            rate_limited && context_mismatch
        },
    )
    .await;
    engine.runtime_snapshot().expect("engine remains usable");
    engine.shutdown().await;

    drop(engine);
    let reopened = reopen_engine_parts(&fixture._directory, &fixture.config, &fixture.manager);
    assert!(
        reopened
            .inner
            .store
            .get(session.session_id)
            .expect("reopened session")
            .log
            .events()
            .iter()
            .all(|event| !matches!(event.payload, EventPayload::PluginEventAdded { .. }))
    );
    reopened.shutdown().await;
}

#[tokio::test]
async fn interleaved_plugin_emit_uses_its_correlated_session_context() {
    let (mut fixture, selection) = custom_fixture();
    let session_a = fixture
        .engine
        .create_session(selection.clone())
        .expect("session A");
    let session_b = fixture.engine.create_session(selection).expect("session B");
    for session_id in [session_a.session_id, session_b.session_id] {
        fixture
            .engine
            .set_session_permission(
                session_id,
                PermissionAction::Read,
                WildcardPattern::new("*").expect("wildcard"),
                PermissionEffect::Allow,
                cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
            )
            .await
            .expect("persist session");
    }
    fixture.engine.shutdown().await;
    fixture.config.plugins.insert(
        "fixture".into(),
        PluginConfig {
            command: Some(python_command().into()),
            args: vec![PLUGIN_FIXTURE.into()],
            env: BTreeMap::from([
                ("FIXTURE_NAME".into(), "fixture".into()),
                (
                    "FIXTURE_CAPABILITIES".into(),
                    r#"{"producer_messaging":false,"tools":false,"resources":false,"subscribe_events":true,"subscribe_bus":false,"publish_bus":true,"publish_session_events":true,"intercept":[]}"#.into(),
                ),
                (
                    "FIXTURE_EMIT_ON_EVENT".into(),
                    r#"{"name":"delayed_a","payload":{"source":"a"}}"#.into(),
                ),
                ("FIXTURE_EMIT_FIRST_AFTER_SECOND".into(), "1".into()),
                ("FIXTURE_EMIT_COUNT".into(), "2".into()),
            ]),
            cwd: None,
            enabled: true,
            producer_messaging: false,
            interception_timeout_ms: 2_000,
            startup_timeout_ms: 10_000,
            shutdown_grace_ms: 3_000,
            tool_timeout_ms: 30_000,
        },
    );
    drop(fixture.engine);
    let engine = reopen_engine_parts(&fixture._directory, &fixture.config, &fixture.manager);
    engine.inner.plugins.await_eager_ready().await;
    let mut bus = engine.subscribe_engine_events();
    for (session_id, message) in [
        (session_a.session_id, "trigger A"),
        (session_b.session_id, "trigger B"),
    ] {
        engine
            .append(
                session_id,
                None,
                cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
                EventPayload::PluginDiagnostic {
                    plugin: "engine".into(),
                    kind: cookie_agent_protocol::PluginDiagnosticKind::HookBlocked,
                    message: message.into(),
                    count: 1,
                },
            )
            .await
            .expect("trigger event");
    }
    let delivered = tokio::time::timeout(test_timeout(3), bus.recv())
        .await
        .expect("delayed bus emit")
        .expect("bus event");
    assert!(
        matches!(delivered, crate::EngineEvent::PluginEvent { session_id, name, .. } if session_id == session_a.session_id && name == "delayed_a")
    );
    await_projection(
        &engine,
        session_a.session_id,
        "delayed session A emit",
        |projection| {
            let events = projection.log.events();
            let published = events
                .iter()
                .filter(|event| {
                    matches!(
                        &event.payload,
                        EventPayload::PluginEventAdded { name, .. } if name == "delayed_a"
                    )
                })
                .count();
            let replay_diagnosed = events.iter().any(|event| {
                matches!(
                    &event.payload,
                    EventPayload::PluginDiagnostic {
                        kind: cookie_agent_protocol::PluginDiagnosticKind::ContextMismatch,
                        ..
                    }
                )
            });
            published == 0 && replay_diagnosed
        },
    )
    .await;
    assert!(
        engine
            .inner
            .store
            .get(session_b.session_id)
            .expect("session B")
            .log
            .events()
            .iter()
            .all(|event| !matches!(
                &event.payload,
                EventPayload::PluginEventAdded { name, .. } if name == "delayed_a"
            ))
    );
    let unknown = engine
        .publish_plugin_emit(crate::plugin::PluginEmitRequest {
            plugin: "fixture".into(),
            session_id: session_b.session_id,
            context: crate::plugin::PluginEmitContext::Rejected {
                diagnostic_session_id: None,
                reason: "unknown replay token".into(),
            },
            name: "unknown".into(),
            payload: serde_json::json!({}),
            publish_bus: false,
            publish_session_events: true,
        })
        .await;
    assert_eq!(
        unknown.durable,
        cookie_agent_protocol::ExtensionEmitStatus::Rejected
    );
    // This negative assertion covers the plugin replay-token expiry window.
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    assert!(
        engine
            .inner
            .store
            .get(session_b.session_id)
            .expect("session B")
            .log
            .events()
            .iter()
            .all(|event| !matches!(
                event.payload,
                EventPayload::PluginDiagnostic {
                    kind: cookie_agent_protocol::PluginDiagnosticKind::ContextMismatch,
                    ..
                }
            )),
        "unknown token routed a diagnostic to the plugin-supplied session"
    );
    assert!(
        engine.plugin_statuses().iter().any(|status| {
            status.plugin == "fixture"
                && status
                    .reason
                    .as_deref()
                    .is_some_and(|reason| reason.contains("unknown replay token"))
        }),
        "unknown token was not diagnosed against the offender"
    );
    engine.shutdown().await;
}

#[tokio::test]
async fn plugin_diagnostic_coalescing_is_exact_and_shutdown_drains() {
    const DROP_COUNT: u64 = 5_000;
    const DISTINCT_COUNT: u64 = 5_000;

    let (fixture, selection) = custom_fixture();
    let session = fixture.engine.create_session(selection).expect("session");
    fixture
        .engine
        .set_session_permission(
            session.session_id,
            PermissionAction::Read,
            WildcardPattern::new("*").expect("wildcard"),
            PermissionEffect::Allow,
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("persist session");
    for _ in 0..DROP_COUNT {
        fixture.engine.record_plugin_diagnostic(
            session.session_id,
            "lagging".into(),
            cookie_agent_protocol::PluginDiagnosticKind::EventDrop,
            "buffer overflow".into(),
        );
    }
    for ordinal in 0..DISTINCT_COUNT {
        fixture.engine.record_plugin_diagnostic(
            session.session_id,
            "fixture".into(),
            cookie_agent_protocol::PluginDiagnosticKind::InvalidModification,
            format!("distinct diagnostic {ordinal}"),
        );
    }
    assert!(fixture.engine.pending_plugin_diagnostic_keys_for_test() <= 257);
    await_projection(
        &fixture.engine,
        session.session_id,
        "all coalesced plugin diagnostics",
        |projection| {
            let (dropped, distinct) =
                projection
                    .log
                    .events()
                    .iter()
                    .fold((0, 0), |(dropped, distinct), event| match &event.payload {
                        EventPayload::PluginDiagnostic {
                            plugin,
                            kind: cookie_agent_protocol::PluginDiagnosticKind::EventDrop,
                            message,
                            count,
                        } if plugin == "lagging" && message == "buffer overflow" => {
                            (dropped + count, distinct)
                        }
                        EventPayload::PluginDiagnostic {
                            plugin,
                            kind: cookie_agent_protocol::PluginDiagnosticKind::InvalidModification,
                            count,
                            ..
                        } if plugin == "fixture" => (dropped, distinct + count),
                        _ => (dropped, distinct),
                    });
            dropped == DROP_COUNT && distinct == DISTINCT_COUNT
        },
    )
    .await;
    fixture.engine.shutdown().await;

    let events = fixture
        .engine
        .inner
        .store
        .get(session.session_id)
        .expect("session after shutdown")
        .log
        .all_events();
    let dropped = events
        .iter()
        .filter_map(|event| match &event.payload {
            EventPayload::PluginDiagnostic {
                plugin,
                kind: cookie_agent_protocol::PluginDiagnosticKind::EventDrop,
                message,
                count,
            } if plugin == "lagging" && message == "buffer overflow" => Some(*count),
            _ => None,
        })
        .sum::<u64>();
    let distinct = events
        .iter()
        .filter_map(|event| match &event.payload {
            EventPayload::PluginDiagnostic {
                plugin,
                kind: cookie_agent_protocol::PluginDiagnosticKind::InvalidModification,
                message: _,
                count,
            } if plugin == "fixture" => Some(*count),
            _ => None,
        })
        .sum::<u64>();
    assert_eq!(dropped, DROP_COUNT);
    assert_eq!(distinct, DISTINCT_COUNT);
}

#[tokio::test]
async fn plugin_diagnostic_wedge_does_not_block_shutdown() {
    let (mut fixture, selection) = custom_fixture();
    let session = fixture
        .engine
        .create_session(selection)
        .expect("diagnostic session");
    fixture
        .engine
        .set_session_permission(
            session.session_id,
            PermissionAction::Read,
            WildcardPattern::new("*").expect("wildcard"),
            PermissionEffect::Allow,
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("persist session");
    fixture.engine.shutdown().await;
    let mut plugin = interception_plugin("fixture", &[]);
    plugin.enabled = false;
    fixture.config.plugins.insert("fixture".into(), plugin);
    fixture.engine = reopen_engine(&fixture);
    fixture.engine.block_plugin_diagnostic_appends_for_test();
    fixture.engine.record_plugin_diagnostic(
        session.session_id,
        "fixture".into(),
        cookie_agent_protocol::PluginDiagnosticKind::EventDrop,
        "wedged diagnostic".into(),
    );
    tokio::time::timeout(std::time::Duration::from_secs(1), fixture.engine.shutdown())
        .await
        .expect("diagnostic wedge blocked shutdown");
    assert!(fixture.engine.plugin_statuses().iter().any(|status| {
        status.plugin == "fixture"
            && status
                .reason
                .as_deref()
                .is_some_and(|reason| reason.contains("drain incomplete"))
    }));
}

#[tokio::test]
async fn lazy_mcp_preemption_rejects_the_plugin_tool_published_to_the_model() {
    const PLUGIN_FIXTURE: &str =
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/fake_plugin.py");
    const MCP_FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/mcp_server.py");

    let first = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"plugin-call\",\"type\":\"function\",\"function\":{\"name\":\"fixture_echo_text\",\"arguments\":\"{\\\"plugin_arg\\\":\\\"published-schema\\\"}\"}}]},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n".to_owned();
    let second = "data: {\"choices\":[{\"delta\":{\"content\":\"done\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n".to_owned();
    let (endpoint, server, reached, release) =
        scripted_server_with_delayed_response(vec![first, second], 0).await;
    let markers = tempfile::tempdir().expect("call markers");
    let plugin_call = markers.path().join("plugin-call.json");
    let mcp_call = markers.path().join("mcp-call.json");
    let declaration = serde_json::json!([{
        "name": "fixture_echo_text",
        "description": "Plugin schema",
        "parameters": {
            "type": "object",
            "properties": {"plugin_arg": {"type": "string"}},
            "required": ["plugin_arg"]
        },
        "permission_name": "issue_read",
        "primary_resource_param": "plugin_arg"
    }]);
    let toml_string = |value: &str| toml::Value::String(value.to_owned()).to_string();
    let extra_config = format!(
        r#"
[plugins.collision]
command = {}
args = [{}]
env = {{ FIXTURE_NAME = "collision", FIXTURE_TOOLS = {}, FIXTURE_TOOL_CALL_FILE = {} }}

[mcp.servers.fixture]
command = {}
args = [{}]
env = {{ MCP_FIXTURE_CALL_FILE = {} }}
lazy = true
"#,
        toml_string(python_command()),
        toml_string(PLUGIN_FIXTURE),
        toml_string(&declaration.to_string()),
        toml_string(&plugin_call.display().to_string()),
        toml_string(python_command()),
        toml_string(MCP_FIXTURE),
        toml_string(&mcp_call.display().to_string()),
    );
    let agent = "---\ndescription: Plugin preemption test\nmode: primary\nenabled: true\nmodels: [{ model: \"custom.test/a-model\", variant: null }]\npermissions:\n  plugin:\n    \"issue_read *\": allow\n---\nTest plugin ownership pinning.\n";
    let fixture = synthetic_default_fixture_with_config(Some(agent), &endpoint, &extra_config)
        .expect("engine");
    let snapshot = fixture.engine.runtime_snapshot().expect("runtime").snapshot;
    let agent = snapshot
        .agents
        .iter()
        .find(|agent| agent.id.as_str() == "primary")
        .expect("primary agent");
    let selection = RunSelection {
        agent: agent.id.clone(),
        model: agent.resolved_fallback[0].clone(),
        preset: None,
    };
    let session = fixture
        .engine
        .create_session(selection.clone())
        .expect("session");
    fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: session.session_id,
                client_run_id: ClientRunId::new("plugin-mcp-preemption").expect("run ID"),
                selection,
                input: "use the plugin".into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("run accepted");
    tokio::time::timeout(test_timeout(3), reached)
        .await
        .expect("model request reached")
        .expect("model reach signal");

    fixture
        .engine
        .reconnect_mcp_server("fixture".into())
        .await
        .expect("connect lazy MCP");
    // Plugin lifecycle changes are not session events; this is the only
    // observation API available for asynchronous MCP/plugin preemption.
    tokio::time::timeout(test_timeout(3), async {
        loop {
            if fixture.engine.plugin_statuses().iter().any(|status| {
                status.plugin == "collision" && status.state == crate::PluginState::Failed
            }) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("plugin preempted");
    release.notify_one();

    wait_for_session_not_running(&fixture.engine, session.session_id).await;
    let requests = with_watchdog("server fixture completion", server)
        .await
        .expect("model server");
    assert_eq!(requests.len(), 2);
    let projection = fixture
        .engine
        .inner
        .store
        .get(session.session_id)
        .expect("session projection");
    let events = projection.log.events();
    let termination = events
        .iter()
        .find_map(|event| match &event.payload {
            EventPayload::ToolCallTerminated { termination } => Some(termination),
            _ => None,
        })
        .expect("tool termination");
    assert_eq!(termination.outcome, ToolTerminationOutcome::Failed);
    let error = termination.error.as_ref().expect("tool error");
    assert_eq!(error.code.as_str(), "operation_changed");
    assert!(error.message.as_str().contains("tool definition changed"));
    assert!(!plugin_call.exists(), "preempted plugin must not execute");
    assert!(!mcp_call.exists(), "replacement MCP tool must not execute");
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn immediate_first_run_waits_for_complete_eager_mcp_listing() {
    let (endpoint, captured) = scripted_model_server().await;
    let (fixture, selection) = custom_fixture_with_endpoint_primary_internal_and_concurrency(
        &endpoint,
        "---\ndescription: MCP readiness agent\nmode: primary\nenabled: true\nmodels: [{ model: \"custom.test/group/model\", variant: base }]\npermissions:\n  mcp: allow\n---\nUse MCP.\n",
        None,
        None,
        false,
        None,
        Some(delayed_mcp_server(McpServerSource::UserFile)),
    );
    let session = fixture
        .engine
        .create_session(selection.clone())
        .expect("immediate session");
    fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: session.session_id,
                client_run_id: ClientRunId::new("immediate-mcp-run").expect("run ID"),
                selection,
                input: "use the available tools".into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("first run after engine open");
    let status = fixture.engine.mcp_statuses().remove(0);
    assert_eq!(status.state, crate::McpServerState::Connected);
    assert_eq!(status.tools.len(), 2);
    captured.abort();
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn immediate_first_run_waits_for_project_mcp_listing_without_separate_approval() {
    let (endpoint, captured) = scripted_model_server().await;
    let (fixture, selection) = custom_fixture_with_endpoint_primary_internal_and_concurrency(
        &endpoint,
        "---\ndescription: Project MCP readiness agent\nmode: primary\nenabled: true\nmodels: [{ model: \"custom.test/group/model\", variant: base }]\npermissions:\n  mcp: allow\n---\nUse project MCP.\n",
        None,
        None,
        false,
        None,
        Some(delayed_mcp_server(McpServerSource::WorkspaceFile)),
    );
    let session = fixture
        .engine
        .create_session(selection.clone())
        .expect("immediate project MCP session");
    fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: session.session_id,
                client_run_id: ClientRunId::new("project-mcp-run").expect("run ID"),
                selection,
                input: "use the project tools".into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("first run after engine open");
    let status = fixture.engine.mcp_statuses().remove(0);
    assert_eq!(status.state, crate::McpServerState::Connected);
    assert_eq!(status.tools.len(), 2);
    captured.abort();
    fixture.engine.shutdown().await;
}
