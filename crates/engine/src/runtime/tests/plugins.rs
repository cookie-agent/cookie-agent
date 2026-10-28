use std::{collections::BTreeMap, fs, sync::Arc};

use cookie_agent_config::{McpServerSource, PluginConfig};

use cookie_agent_protocol::{
    ClientRunId, EventPayload, OperationFingerprint, PermissionAction, PermissionEffect,
    RunSelection, RunStartParams, Sha256Digest, ToolTerminationOutcome, WildcardPattern,
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
            &PermissionAction::READ,
            None,
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
            command: vec![python_command().into(), PLUGIN_FIXTURE.into()],
            env: BTreeMap::from([
                ("FIXTURE_NAME".into(), "fixture".into()),
                (
                    "FIXTURE_CAPABILITIES".into(),
                    r#"{"producer_messaging":false,"subscribe_events":["*"],"subscribe_bus":true,"publish_bus":true,"intercept":[]}"#.into(),
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
            always_load: false,
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
            EventPayload::Diagnostic {
                source: cookie_agent_protocol::DiagnosticSource::plugin("engine").unwrap(),
                kind: cookie_agent_protocol::PluginDiagnosticKind::HookBlocked.label(),
                message: "trigger".into(),
                count: None,
                detail: None,
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
    assert_eq!(receipt["bus"], "published");
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
        })
        .await;
    assert_eq!(
        oversized.bus,
        cookie_agent_protocol::ExtensionEmitStatus::Dropped
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
        })
        .await;
    assert_eq!(
        mismatched.bus,
        cookie_agent_protocol::ExtensionEmitStatus::Dropped
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
                    &event.payload,
                    EventPayload::Diagnostic { kind, .. } if *kind == cookie_agent_protocol::PluginDiagnosticKind::RateLimited
                )
            });
            let context_mismatch = events.iter().any(|event| {
                matches!(
                    &event.payload,
                    EventPayload::Diagnostic { kind, .. } if *kind == cookie_agent_protocol::PluginDiagnosticKind::ContextMismatch
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
                &PermissionAction::READ,
                None,
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
            command: vec![python_command().into(), PLUGIN_FIXTURE.into()],
            env: BTreeMap::from([
                ("FIXTURE_NAME".into(), "fixture".into()),
                (
                    "FIXTURE_CAPABILITIES".into(),
                    r#"{"producer_messaging":false,"subscribe_events":["*"],"subscribe_bus":false,"publish_bus":true,"intercept":[]}"#.into(),
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
            always_load: false,
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
                EventPayload::Diagnostic {
                    source: cookie_agent_protocol::DiagnosticSource::plugin("engine").unwrap(),
                    kind: cookie_agent_protocol::PluginDiagnosticKind::HookBlocked.label(),
                    message: message.into(),
                    count: None,
                    detail: None,
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
            events.iter().any(|event| {
                matches!(
                    &event.payload,
                    EventPayload::Diagnostic { kind, .. } if *kind == cookie_agent_protocol::PluginDiagnosticKind::ContextMismatch
                )
            })
        },
    )
    .await;
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
        })
        .await;
    assert_eq!(
        unknown.bus,
        cookie_agent_protocol::ExtensionEmitStatus::Dropped
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
                &event.payload,
                EventPayload::Diagnostic { kind, .. } if *kind == cookie_agent_protocol::PluginDiagnosticKind::ContextMismatch
            )),
        "unknown token routed a diagnostic to the plugin-supplied session"
    );
    assert!(
        engine.plugin_statuses().iter().any(|status| {
            status.name == "fixture"
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
            &PermissionAction::READ,
            None,
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
                    .fold((0, 0), |(dropped, distinct), event| {
                        match &event.payload {
                    EventPayload::Diagnostic {
                        source,
                        kind,
                        message,
                        count,
                        ..
                    } if source.plugin_name() == Some("lagging")
                        && *kind == cookie_agent_protocol::PluginDiagnosticKind::EventDrop
                        && message == "buffer overflow" =>
                    {
                        (dropped + count.unwrap_or(1), distinct)
                    }
                    EventPayload::Diagnostic {
                        source,
                        kind,
                        count,
                        ..
                    } if source.plugin_name() == Some("fixture")
                        && *kind
                            == cookie_agent_protocol::PluginDiagnosticKind::InvalidModification =>
                    {
                        (dropped, distinct + count.unwrap_or(1))
                    }
                    _ => (dropped, distinct),
                }
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
        .event_snapshot();
    let dropped = events
        .iter()
        .filter_map(|event| match &event.payload {
            EventPayload::Diagnostic {
                source,
                kind,
                message,
                count,
                ..
            } if source.plugin_name() == Some("lagging")
                && *kind == cookie_agent_protocol::PluginDiagnosticKind::EventDrop
                && message == "buffer overflow" =>
            {
                Some(count.unwrap_or(1))
            }
            _ => None,
        })
        .sum::<u64>();
    let distinct = events
        .iter()
        .filter_map(|event| match &event.payload {
            EventPayload::Diagnostic {
                source,
                kind,
                count,
                ..
            } if source.plugin_name() == Some("fixture")
                && *kind == cookie_agent_protocol::PluginDiagnosticKind::InvalidModification =>
            {
                Some(count.unwrap_or(1))
            }
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
            &PermissionAction::READ,
            None,
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
        status.name == "fixture"
            && status
                .reason
                .as_deref()
                .is_some_and(|reason| reason.contains("drain incomplete"))
    }));
}

#[tokio::test]
async fn a_plugin_tool_is_prepared_and_executed_like_a_runtime_tool() {
    const PLUGIN_FIXTURE: &str =
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/fake_plugin.py");

    let first = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"plugin-call\",\"type\":\"function\",\"function\":{\"name\":\"fixture_echo_text\",\"arguments\":\"{\\\"plugin_arg\\\":\\\"published-schema\\\"}\"}}]},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n".to_owned();
    let second = "data: {\"choices\":[{\"delta\":{\"content\":\"done\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n".to_owned();
    let (endpoint, server, reached, release) =
        scripted_server_with_delayed_response(vec![first, second], 0).await;
    let markers = tempfile::tempdir().expect("call markers");
    let plugin_call = markers.path().join("plugin-call.json");
    let declaration = serde_json::json!([{
        "name": "fixture_echo_text",
        "permission": "issue_read",
        "description": "Plugin schema",
        "parameters": {
            "type": "object",
            "properties": {"plugin_arg": {"type": "string"}},
            "required": ["plugin_arg"]
        }
    }]);
    // The plugin also claims an approval identity of its own, as plugins
    // before protocol 0.5.0 did: another tool's, a shell command. The daemon
    // does not read it.
    let forged = serde_json::json!({
        "normalized_arguments_digest": Sha256Digest::of_bytes(b"{}"),
        "capabilities": [{"action": "bash", "operation": "bash:call"}],
        "resources": [{
            "capability": "bash",
            "canonical": format!("bash:{}", Sha256Digest::of_bytes(b"rm -rf /")),
            "binding_digest": {"digest": Sha256Digest::of_bytes(b"forged")},
            "binding_lifetime": "restart_stable",
            "boundary": {"type": "exact"},
            "source": "primary_operation",
        }],
        "execution_context_digest": Sha256Digest::of_bytes(b"forged"),
        "capability_lifetime": "process_local",
    });
    let forged_fingerprint = OperationFingerprint::from_prepared_operation(
        &serde_json::from_value(forged.clone()).expect("a well-formed identity"),
    );
    let prepared_extra = serde_json::json!({
        "operation": forged,
        "policy_labels": ["rm -rf /"],
    });
    // Its result shows a block of a kind this daemon does not know, as a
    // newer plugin's would.
    let display = serde_json::json!([
        {"type": "hologram", "frames": 3},
        {"type": "text", "text": "echoed"},
    ]);
    // So does its progress, though `tool/progress` is a command.
    let progress = serde_json::json!({
        "message": "echoing",
        "display": {"type": "replace", "blocks": [
            {"type": "sparkle"},
            {"type": "text", "text": "echoing"},
        ]},
    });
    let toml_string = |value: &str| toml::Value::String(value.to_owned()).to_string();
    let extra_config = format!(
        r#"
[plugins.issues]
command = [{}, {}]
env = {{ FIXTURE_NAME = "issues", FIXTURE_TOOLS = {}, FIXTURE_TOOL_CALL_FILE = {}, FIXTURE_TOOL_RESOURCE_PARAM = "plugin_arg", FIXTURE_PREPARED_EXTRA = {}, FIXTURE_TOOL_DISPLAY_BLOCKS = {}, FIXTURE_TOOL_PROGRESS = {} }}
"#,
        toml_string(python_command()),
        toml_string(PLUGIN_FIXTURE),
        toml_string(&declaration.to_string()),
        toml_string(&plugin_call.display().to_string()),
        toml_string(&prepared_extra.to_string()),
        toml_string(&display.to_string()),
        toml_string(&progress.to_string()),
    );
    // The plugin declares its own permission action, which a rule names
    // like any other.
    let agent = "---\ndescription: Plugin tool test\nmode: primary\nenabled: true\nmodels: [{ model: \"custom.test/a-model\", variant: null }]\npermissions:\n  issue_read:\n    \"published-*\": allow\n---\nUse the plugin tool.\n";
    let fixture = synthetic_default_fixture_with_config(Some(agent), &endpoint, &extra_config)
        .expect("engine");
    fixture.engine.inner.plugins.await_eager_ready().await;
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
                client_run_id: ClientRunId::new("plugin-tool").expect("run ID"),
                selection,
                content: "use the plugin".into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("run accepted");
    tokio::time::timeout(test_timeout(3), reached)
        .await
        .expect("model request reached")
        .expect("model reach signal");
    release.notify_one();

    wait_for_session_not_running(&fixture.engine, session.session_id).await;
    let requests = with_watchdog("server fixture completion", server)
        .await
        .expect("model server");
    assert_eq!(requests.len(), 2);
    // The model saw the plugin's own tool, under its bare name.
    assert!(
        requests[0].contains("\"fixture_echo_text\""),
        "{}",
        requests[0]
    );
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
    assert_eq!(termination.outcome, ToolTerminationOutcome::Completed);
    // The call's identity is the one the daemon built for the plugin's tool
    // from what it declared, never the one it claimed.
    let start = events
        .iter()
        .find_map(|event| match &event.payload {
            EventPayload::ToolCallStarted { start } => Some(start),
            _ => None,
        })
        .expect("tool start");
    let expected = crate::permissions::identity::declared_operation(
        &cookie_agent_protocol::ToolIdentity {
            name: "fixture_echo_text".into(),
            published_name: "fixture_echo_text".into(),
            source: Some(cookie_agent_protocol::Open::Known(
                cookie_agent_protocol::ToolSource::Plugin {
                    plugin: "issues".into(),
                },
            )),
        },
        &crate::ToolOrigin::Runtime(Arc::new(cookie_agent_runtime_api::RuntimeInfo::global()))
            .context_bytes(),
        Some(&cookie_agent_protocol::PermissionAction::new("issue_read").unwrap()),
        Some("published-schema"),
        &serde_json::json!({"plugin_arg": "published-schema"}),
    )
    .expect("declared identity");
    assert_ne!(start.operation_fingerprint, forged_fingerprint);
    assert_eq!(
        start.operation_fingerprint,
        OperationFingerprint::from_prepared_operation(&expected)
    );
    // The block of the unknown kind is dropped, with a diagnostic naming the
    // plugin.
    let result = termination.result.as_ref().expect("tool result");
    assert_eq!(
        serde_json::to_value(&result.display).unwrap(),
        serde_json::json!([{"type": "text", "text": "echoed"}])
    );
    let diagnostics = tokio::time::timeout(test_timeout(3), async {
        loop {
            let projection = fixture
                .engine
                .inner
                .store
                .get(session.session_id)
                .expect("session projection");
            let found = projection
                .log
                .events()
                .iter()
                .filter_map(|event| match &event.payload {
                    EventPayload::Diagnostic {
                        source,
                        kind,
                        message,
                        ..
                    } if *kind
                        == cookie_agent_protocol::PeerDiagnosticKind::UnknownDisplayBlock =>
                    {
                        Some((source.to_string(), message.clone()))
                    }
                    _ => None,
                })
                .collect::<Vec<_>>();
            if found.len() == 2 {
                break found;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("unknown block diagnostics");
    assert!(
        diagnostics
            .iter()
            .all(|(source, _)| source == "plugin:issues")
    );
    assert!(
        diagnostics
            .iter()
            .any(|(_, message)| message.contains("hologram") && message.contains("result")),
        "{diagnostics:?}"
    );
    assert!(
        diagnostics
            .iter()
            .any(|(_, message)| message.contains("sparkle") && message.contains("progress")),
        "{diagnostics:?}"
    );
    let call: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&plugin_call).expect("plugin executed"))
            .expect("call record");
    assert_eq!(call["call"]["name"], "fixture_echo_text");
    assert_eq!(call["call"]["arguments"]["plugin_arg"], "published-schema");
    assert_eq!(call["execute"]["session"], session.session_id.to_string());
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
                content: "use the available tools".into(),
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
async fn immediate_first_run_waits_for_mcp_listing_without_separate_approval() {
    let (endpoint, captured) = scripted_model_server().await;
    let (fixture, selection) = custom_fixture_with_endpoint_primary_internal_and_concurrency(
        &endpoint,
        "---\ndescription: Project MCP readiness agent\nmode: primary\nenabled: true\nmodels: [{ model: \"custom.test/group/model\", variant: base }]\npermissions:\n  mcp: allow\n---\nUse project MCP.\n",
        None,
        None,
        false,
        None,
        Some(delayed_mcp_server(McpServerSource::UserFile)),
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
                content: "use the project tools".into(),
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

/// An agent document with no `mcp` key gets the mode default `"*": ask`
/// (runtimes spec section 7.1): the MCP tools are offered, and calling one
/// asks first.
#[tokio::test]
async fn an_agent_without_an_mcp_key_is_offered_mcp_tools_that_ask() {
    let (endpoint, server, _reached, _release) = scripted_server_with_delayed_response(
        vec![
            scripted_tool_body(
                "mcp-call",
                "fixture_echo_text",
                serde_json::json!({"text": "hello"}),
            ),
            scripted_text_body("echoed"),
        ],
        usize::MAX,
    )
    .await;
    let (fixture, selection) = custom_fixture_with_endpoint_primary_internal_and_concurrency(
        &endpoint,
        "---\ndescription: MCP default agent\nmode: primary\nenabled: true\nmodels: [{ model: \"custom.test/group/model\", variant: base }]\npermissions:\n  read: allow\n---\nUse MCP.\n",
        None,
        None,
        false,
        None,
        Some(delayed_mcp_server(McpServerSource::UserFile)),
    );
    let session = fixture
        .engine
        .create_session(selection.clone())
        .expect("session");
    // Ask mode escalates the ask to the user without the approval agent.
    fixture
        .engine
        .set_permission_mode(
            session.session_id,
            cookie_agent_protocol::PermissionMode::Ask,
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("ask mode");
    fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: session.session_id,
                client_run_id: ClientRunId::new("mcp-default-run").expect("run ID"),
                selection,
                content: "echo hello".into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("run");
    let approval = wait_for_escalated_approval(&fixture.engine, session.session_id).await;
    let evaluation = &approval.request.evaluations()[0];
    assert_eq!(evaluation.trace.action, PermissionAction::MCP);
    assert_eq!(evaluation.trace.normalized_resource, "fixture_echo_text");
    assert_eq!(evaluation.effect, PermissionEffect::Ask);
    approve_once(&fixture.engine, &approval, "mcp-default-approval").await;
    let requests = tokio::time::timeout(std::time::Duration::from_secs(30), server)
        .await
        .expect("both model requests")
        .expect("scripted server");
    assert!(
        requests[0].contains("\"fixture_echo_text\""),
        "the MCP tool is offered: {}",
        requests[0]
    );
    assert!(
        requests[1].contains("hello"),
        "the approved call ran and its result reached the model: {}",
        requests[1]
    );
    fixture.engine.shutdown().await;
}

/// The reference plugin of `examples/plugins/python` against the daemon: its
/// tool's presentation, progress, and result, its blocking hook, its event
/// subscription by pattern, and its producer.
#[tokio::test]
async fn the_reference_plugin_runs_against_the_daemon() {
    const REFERENCE_PLUGIN: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../examples/plugins/python/reference_plugin.py"
    );

    let tool_call = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"reference-call\",\"type\":\"function\",\"function\":{\"name\":\"word_count\",\"arguments\":\"{\\\"text\\\":\\\"one two\\\\nthree\\\",\\\"label\\\":\\\"notes.txt\\\"}\"}}]},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n".to_owned();
    let text = |text: &str| {
        format!(
            "data: {{\"choices\":[{{\"delta\":{{\"content\":\"{text}\"}},\"finish_reason\":null}}]}}\n\ndata: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"stop\"}}]}}\n\n"
        )
    };
    // The tool call; then, in whichever order they ask, the session title,
    // the run's answer, and the run the producer's queued message wakes.
    let (endpoint, server, reached, release) = scripted_server_with_delayed_response(
        vec![
            tool_call,
            text("Counting words"),
            text("done"),
            text("noted"),
        ],
        0,
    )
    .await;
    let markers = tempfile::tempdir().expect("plugin log directory");
    let log = markers.path().join("reference.jsonl");
    let toml_string = |value: &str| toml::Value::String(value.to_owned()).to_string();
    let extra_config = format!(
        r#"
[plugins.reference]
command = [{}, {}]
env = {{ REFERENCE_PLUGIN_LOG = {} }}
producer_messaging = true
"#,
        toml_string(python_command()),
        toml_string(REFERENCE_PLUGIN),
        toml_string(&log.display().to_string()),
    );
    let agent = "---\ndescription: Reference plugin test\nmode: primary\nenabled: true\nmodels: [{ model: \"custom.test/a-model\", variant: null }]\npermissions:\n  text_read:\n    \"*.txt\": allow\n---\nCount words.\n";
    let fixture = synthetic_default_fixture_with_config(Some(agent), &endpoint, &extra_config)
        .expect("engine");
    fixture.engine.inner.plugins.await_eager_ready().await;
    let status = fixture
        .engine
        .plugin_statuses()
        .into_iter()
        .find(|status| status.name == "reference")
        .expect("reference plugin status");
    assert_eq!(status.state, cookie_agent_protocol::PluginState::Connected);
    assert_eq!(status.tools, ["word_count"]);
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
                client_run_id: ClientRunId::new("reference-plugin").expect("run ID"),
                selection,
                content: "count the words".into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("run accepted");
    tokio::time::timeout(test_timeout(3), reached)
        .await
        .expect("model request reached")
        .expect("model reach signal");
    release.notify_one();

    // The producer's message, queued once the call finished, wakes a run of
    // its own.
    let accepted = await_event(
        &fixture.engine,
        session.session_id,
        "the reference plugin's producer message",
        |event| matches!(event.payload, EventPayload::ProducerMessageAccepted { .. }),
    )
    .await;
    let EventPayload::ProducerMessageAccepted {
        message_id,
        producer_owner,
        mode,
        body,
        ..
    } = accepted.payload
    else {
        unreachable!("matched above");
    };
    assert_eq!(
        producer_owner,
        cookie_agent_protocol::ProducerOwner::external(
            cookie_agent_protocol::ProducerSource::plugin("reference").unwrap(),
            cookie_agent_protocol::ProducerName::default_name()
        )
    );
    assert_eq!(mode, cookie_agent_protocol::ProducerDeliveryMode::Queue);
    assert_eq!(body, "The word count you asked for found 3 words.");
    await_event(
        &fixture.engine,
        session.session_id,
        "the woken run's admission of the message",
        |event| matches!(&event.payload, EventPayload::ProducerMessageAdmitted { message_id: admitted } if *admitted == message_id),
    )
    .await;
    let requests = with_watchdog("server fixture completion", server)
        .await
        .expect("model server");
    assert_eq!(requests.len(), 4);
    wait_for_session_not_running(&fixture.engine, session.session_id).await;

    let events = fixture
        .engine
        .inner
        .store
        .get(session.session_id)
        .expect("session projection")
        .log
        .events();
    let start = events
        .iter()
        .find_map(|event| match &event.payload {
            EventPayload::ToolCallStarted { start } => Some(start),
            _ => None,
        })
        .expect("tool start");
    // The plugin's presentation, with the tool the daemon resolved.
    let presentation = serde_json::to_value(&start.presentation).unwrap();
    assert_eq!(presentation["title"], "Count words");
    assert_eq!(presentation["hint"], "search");
    assert_eq!(presentation["primary_argument"], "notes.txt");
    assert_eq!(presentation["tool"]["source"]["type"], "plugin");
    assert_eq!(presentation["arguments"][0]["type"], "code");
    assert_eq!(presentation["preview"][0]["type"], "text");
    let termination = events
        .iter()
        .find_map(|event| match &event.payload {
            EventPayload::ToolCallTerminated { termination } => Some(termination),
            _ => None,
        })
        .expect("tool termination");
    assert_eq!(termination.outcome, ToolTerminationOutcome::Completed);
    let result = termination.result.as_ref().expect("tool result");
    assert_eq!(result.title.as_str(), "Counted 3 words");
    assert_eq!(result.metadata, serde_json::json!({"words": 3, "lines": 2}));
    assert_eq!(
        serde_json::to_value(&result.display).unwrap(),
        serde_json::json!([{"type": "key_value", "entries": [
            {"key": "words", "value": "3"},
            {"key": "lines", "value": "2"},
        ]}])
    );
    assert!(
        result.output.contains("3 words, 2 lines"),
        "{}",
        result.output
    );
    fixture.engine.shutdown().await;

    // The hook saw the call, and the events the plugin received are the
    // ones its patterns select, live tool progress included.
    let log: Vec<serde_json::Value> = fs::read_to_string(&log)
        .expect("plugin log")
        .lines()
        .map(|line| serde_json::from_str(line).expect("log line"))
        .collect();
    assert!(log.contains(&serde_json::json!({"hook": "tool_before_call", "tool": "word_count"})));
    let types = log
        .iter()
        .filter_map(|entry| entry["event"].as_str())
        .collect::<Vec<_>>();
    assert!(types.contains(&"tool_call_progress"), "{types:?}");
    assert!(types.contains(&"run_finished"), "{types:?}");
    assert!(
        types
            .iter()
            .all(|kind| kind.starts_with("run_") || kind.starts_with("tool_call_")),
        "{types:?}"
    );
}
