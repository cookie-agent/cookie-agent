use std::{collections::BTreeMap, path::Path, sync::Arc, time::Duration};

use cookie_agent_config::PluginConfig;
use cookie_agent_protocol::{
    EventPayload, ExtensionBusEventParams, ExtensionEmitStatus, ExtensionHookOnError,
    ExtensionInterceptionHook, ExtensionToolBeforeCallParams, ExtensionToolBeforeCallResult,
    ExtensionToolOrigin, PLUGIN_INTERCEPT_TOOL_BEFORE_CALL_METHOD, PluginDiagnosticKind,
    PluginState, SessionId, StoredEvent,
};
use cookie_agent_runtime_api::reconnect::ReconnectPolicy;
use indexmap::IndexMap;
use serde_json::Value;
use tokio::time::Instant;

use super::{
    InterceptError, PluginDeliveryClass, PluginEmitContext, PluginEmitOutcome,
    PluginNotificationQueue, PluginRegistry, QueuedNotification, raw_params,
};
use crate::{
    SessionToolContext,
    eventually::{eventually, eventually_some},
};

const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/fake_plugin.py");
const DECLARATION: &str = r#"[{"name":"fixture_echo","description":"Echo","parameters":{"type":"object","properties":{"text":{"type":"string"}}}}]"#;
#[cfg(unix)]
const PYTHON: &str = "python3";
#[cfg(windows)]
const PYTHON: &str = "python";

/// A liveness bound on the Python fixture, not a latency assertion.
const WAIT: Duration = Duration::from_secs(30);

fn capabilities(subscribe_events: &[&str], subscribe_bus: bool, intercept: &[&str]) -> String {
    serde_json::json!({
        "producer_messaging": false,
        "subscribe_events": subscribe_events,
        "subscribe_bus": subscribe_bus,
        "publish_bus": false,
        "intercept": intercept
            .iter()
            .map(|hook| serde_json::json!({"hook": hook}))
            .collect::<Vec<_>>(),
    })
    .to_string()
}

fn config(name: &str, env: &[(&str, &str)]) -> PluginConfig {
    let mut variables = BTreeMap::from([
        ("FIXTURE_NAME".to_owned(), name.to_owned()),
        ("FIXTURE_TOOLS".to_owned(), "[]".to_owned()),
        (
            "FIXTURE_CAPABILITIES".to_owned(),
            capabilities(&[], false, &[]),
        ),
    ]);
    variables.extend(
        env.iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned())),
    );
    PluginConfig {
        command: vec![PYTHON.into(), FIXTURE.into()],
        env: variables,
        cwd: None,
        enabled: true,
        producer_messaging: false,
        interception_timeout_ms: 2_000,
        startup_timeout_ms: 10_000,
        shutdown_grace_ms: 3_000,
        always_load: false,
    }
}

/// Starts `plugins` with fast reconnects and waits for each first attempt.
async fn start(plugins: Vec<(&str, PluginConfig)>) -> PluginRegistry {
    let registry = PluginRegistry::new(
        plugins
            .into_iter()
            .map(|(name, config)| (name.to_owned(), config))
            .collect::<IndexMap<_, _>>(),
    );
    registry.set_reconnect_policy(ReconnectPolicy::with_delays([Duration::from_millis(1); 2]));
    registry.start_eager(&tokio::runtime::Handle::current());
    tokio::time::timeout(WAIT, registry.await_eager_ready())
        .await
        .expect("first start attempts");
    registry
}

async fn wait_for(registry: &PluginRegistry, plugin: &str, state: PluginState) {
    eventually(WAIT, format!("`{plugin}` {state:?}"), || {
        registry
            .statuses()
            .iter()
            .any(|status| status.name == plugin && status.state == state)
    })
    .await;
}

/// Waits until `path` holds at least `count` lines and returns them.
async fn lines(path: &Path, count: usize) -> Vec<Value> {
    eventually_some(WAIT, format!("{count} lines in {}", path.display()), || {
        let lines = std::fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).expect("fixture JSON line"))
            .collect::<Vec<Value>>();
        (lines.len() >= count).then_some(lines)
    })
    .await
}

fn stored_event(session_id: SessionId, seq: u64) -> StoredEvent {
    StoredEvent {
        engine_version: None,
        origin: None,
        session_id,
        run_id: None,
        seq,
        timestamp: jiff::Timestamp::now(),
        payload: EventPayload::Diagnostic {
            source: cookie_agent_protocol::DiagnosticSource::plugin("engine").unwrap(),
            kind: PluginDiagnosticKind::HookBlocked.label(),
            message: format!("event {seq}"),
            count: None,
            detail: None,
        },
    }
}

fn tool_before_call(session_id: SessionId) -> ExtensionToolBeforeCallParams {
    ExtensionToolBeforeCallParams {
        session_id,
        context_id: "caller-context".into(),
        tool: "example".into(),
        origin: ExtensionToolOrigin::Base.into(),
        arguments: serde_json::json!({"step": "original"}),
        permission_name: Some("read".into()),
        resource: None,
    }
}

async fn intercept(
    registry: &PluginRegistry,
    plugin: &str,
    session_id: SessionId,
) -> Result<ExtensionToolBeforeCallResult, InterceptError> {
    registry
        .intercept_named(
            plugin,
            PLUGIN_INTERCEPT_TOOL_BEFORE_CALL_METHOD,
            &tool_before_call(session_id),
            Some(session_id),
            Some("caller-context"),
        )
        .await
}

#[tokio::test]
async fn restart_replaces_the_process_and_reports_the_change() {
    let marker = tempfile::tempdir().expect("marker directory");
    let pid_file = marker.path().join("pid");
    let pid_path = pid_file.to_str().expect("pid path");
    let mut disabled = config("off", &[]);
    disabled.enabled = false;
    let registry = start(vec![
        (
            "fixture",
            config("fixture", &[("FIXTURE_PID_FILE", pid_path)]),
        ),
        ("off", disabled),
    ])
    .await;
    wait_for(&registry, "fixture", PluginState::Connected).await;
    assert_eq!(registry.statuses()[1].state, PluginState::Disabled);
    let first = std::fs::read_to_string(&pid_file).expect("first pid");
    let mut changes = registry.subscribe_changes();
    changes.mark_unchanged();

    registry
        .restart("fixture")
        .expect("restart a connected plugin");
    eventually(WAIT, "a new process", || {
        std::fs::read_to_string(&pid_file).is_ok_and(|pid| pid != first)
    })
    .await;
    wait_for(&registry, "fixture", PluginState::Connected).await;
    assert!(changes.has_changed().expect("change channel"));

    assert!(
        registry
            .restart("missing")
            .is_err_and(|error| error.contains("unknown plugin"))
    );
    assert!(
        registry
            .restart("off")
            .is_err_and(|error| error.contains("disabled"))
    );
    registry.shutdown().await;
    assert_eq!(registry.statuses()[0].state, PluginState::Stopped);
}

#[tokio::test]
async fn a_plugin_that_never_connects_fails_until_restarted() {
    let marker = tempfile::tempdir().expect("marker directory");
    let gate = marker.path().join("gate");
    let registry = start(vec![(
        "fixture",
        config(
            "fixture",
            &[(
                "FIXTURE_VERSION_GATE_FILE",
                gate.to_str().expect("gate path"),
            )],
        ),
    )])
    .await;
    wait_for(&registry, "fixture", PluginState::Failed).await;
    let failed = &registry.statuses()[0];
    assert!(
        failed
            .reason
            .as_deref()
            .is_some_and(|reason| reason.contains("999")),
        "{failed:?}"
    );
    assert!(failed.tools.is_empty());

    std::fs::write(&gate, "open").expect("open the gate");
    registry
        .restart("fixture")
        .expect("restart a failed plugin");
    wait_for(&registry, "fixture", PluginState::Connected).await;
    assert_eq!(registry.statuses()[0].reason, None);
    registry.shutdown().await;
}

#[tokio::test]
async fn a_plugins_tools_compose_under_its_name_while_connected() {
    let registry = start(vec![(
        "fixture",
        config("fixture", &[("FIXTURE_TOOLS", DECLARATION)]),
    )])
    .await;
    wait_for(&registry, "fixture", PluginState::Connected).await;
    let providers = registry.tool_providers();
    assert_eq!(providers.len(), 1);
    assert_eq!(
        providers[0].composition_source().as_deref(),
        Some("fixture")
    );
    let context = SessionToolContext::new(SessionId::new_v7());
    let names = |provider: &Arc<dyn crate::ToolProvider>| {
        provider
            .tools_for_session(&context)
            .expect("tool listing")
            .into_iter()
            .map(|spec| spec.name)
            .collect::<Vec<_>>()
    };
    assert_eq!(names(&providers[0]), ["fixture_echo"]);
    assert_eq!(registry.statuses()[0].tools, ["fixture_echo"]);
    registry.shutdown().await;
    // Without a connection the plugin publishes nothing.
    assert!(names(&providers[0]).is_empty());
}

#[tokio::test]
async fn events_stream_in_order_and_bus_events_skip_their_source() {
    let marker = tempfile::tempdir().expect("marker directory");
    let event_file = marker.path().join("events.jsonl");
    let bus_file = marker.path().join("bus.jsonl");
    let registry = start(vec![(
        "fixture",
        config(
            "fixture",
            &[
                ("FIXTURE_CAPABILITIES", &capabilities(&["*"], true, &[])),
                (
                    "FIXTURE_EVENT_FILE",
                    event_file.to_str().expect("event path"),
                ),
                (
                    "FIXTURE_BUS_EVENT_FILE",
                    bus_file.to_str().expect("bus path"),
                ),
            ],
        ),
    )])
    .await;
    wait_for(&registry, "fixture", PluginState::Connected).await;
    let session_id = SessionId::new_v7();
    for seq in [2, 3] {
        assert!(
            registry
                .stream_session_event(&stored_event(session_id, seq))
                .is_empty()
        );
    }
    for source in ["fixture", "other"] {
        let event = ExtensionBusEventParams {
            session_id,
            context_id: None,
            plugin: source.into(),
            name: "notice".into(),
            payload: serde_json::json!({"from": source}),
        };
        assert!(registry.stream_bus_event(&event, Some(source)).is_empty());
    }

    let events = lines(&event_file, 2).await;
    assert_eq!(
        events.iter().map(|event| &event["seq"]).collect::<Vec<_>>(),
        [&serde_json::json!(2), &serde_json::json!(3)]
    );
    assert_ne!(events[0]["context_id"], events[1]["context_id"]);
    let bus = lines(&bus_file, 1).await;
    // The plugin's own bus event, queued first, never reached it.
    assert_eq!(bus.len(), 1);
    assert_eq!(bus[0]["plugin"], "other");
    registry.shutdown().await;
}

#[tokio::test]
async fn each_hook_call_gets_its_own_context() {
    let marker = tempfile::tempdir().expect("marker directory");
    let intercept_file = marker.path().join("intercept.jsonl");
    let registry = start(vec![(
        "fixture",
        config(
            "fixture",
            &[
                (
                    "FIXTURE_CAPABILITIES",
                    &capabilities(&[], false, &["tool_before_call"]),
                ),
                (
                    "FIXTURE_INTERCEPT_FILE",
                    intercept_file.to_str().expect("intercept path"),
                ),
            ],
        ),
    )])
    .await;
    wait_for(&registry, "fixture", PluginState::Connected).await;
    let session_id = SessionId::new_v7();
    for _ in 0..2 {
        intercept(&registry, "fixture", session_id)
            .await
            .expect("hook result");
    }
    let calls = lines(&intercept_file, 2).await;
    let contexts = calls
        .iter()
        .map(|call| call["params"]["context_id"].as_str().expect("context ID"))
        .collect::<Vec<_>>();
    assert_ne!(contexts[0], contexts[1]);
    assert!(!contexts.contains(&"caller-context"));
    assert_eq!(
        calls[0]["params"]["origin"],
        serde_json::json!({"kind": "base"})
    );
    registry.shutdown().await;
}

#[tokio::test]
async fn hook_failures_fail_open_with_their_diagnostic_kind() {
    let hook = capabilities(&[], false, &["tool_before_call"]);
    let mut slow = config(
        "slow",
        &[
            ("FIXTURE_CAPABILITIES", &hook),
            ("FIXTURE_INTERCEPT_DELAY_MS", "500"),
        ],
    );
    slow.interception_timeout_ms = 30;
    let registry = start(vec![
        (
            "rejecting",
            config(
                "rejecting",
                &[
                    ("FIXTURE_CAPABILITIES", &hook),
                    ("FIXTURE_INTERCEPT_ERROR", "not today"),
                ],
            ),
        ),
        (
            "malformed",
            config(
                "malformed",
                &[
                    ("FIXTURE_CAPABILITIES", &hook),
                    ("FIXTURE_TOOL_BEFORE_RESULT", r#"{"action":"explode"}"#),
                ],
            ),
        ),
        ("slow", slow),
        (
            "crashing",
            config(
                "crashing",
                &[
                    ("FIXTURE_CAPABILITIES", &hook),
                    ("FIXTURE_CRASH_DURING_INTERCEPT", "1"),
                ],
            ),
        ),
        ("passive", config("passive", &[])),
    ])
    .await;
    for plugin in ["rejecting", "malformed", "slow", "crashing", "passive"] {
        wait_for(&registry, plugin, PluginState::Connected).await;
    }
    assert_eq!(
        registry
            .interception_plugins(ExtensionInterceptionHook::ToolBeforeCall)
            .into_iter()
            .map(|plugin| (plugin.name, plugin.on_error))
            .collect::<Vec<_>>(),
        ["rejecting", "malformed", "slow", "crashing"]
            .map(|name| (name.to_owned(), ExtensionHookOnError::Allow))
    );
    let session_id = SessionId::new_v7();

    let rejected = intercept(&registry, "rejecting", session_id)
        .await
        .expect_err("an error answer");
    assert!(
        matches!(&rejected, InterceptError::Rejected(message) if message.contains("not today"))
    );
    assert_eq!(rejected.kind(), PluginDiagnosticKind::InterceptionRejected);

    let malformed = intercept(&registry, "malformed", session_id)
        .await
        .expect_err("a malformed answer");
    assert!(matches!(malformed, InterceptError::Malformed(_)));
    assert_eq!(malformed.kind(), PluginDiagnosticKind::InterceptionRejected);

    let timeout = intercept(&registry, "slow", session_id)
        .await
        .expect_err("no answer in time");
    assert_eq!(timeout, InterceptError::Timeout);
    assert_eq!(timeout.kind(), PluginDiagnosticKind::InterceptionTimeout);

    let crashed = intercept(&registry, "crashing", session_id)
        .await
        .expect_err("the plugin exited");
    assert_eq!(crashed, InterceptError::Disconnected);
    assert_eq!(crashed.kind(), PluginDiagnosticKind::InterceptionCrash);

    assert_eq!(
        intercept(&registry, "missing", session_id).await,
        Err(InterceptError::Disconnected)
    );
    registry.shutdown().await;
}

#[tokio::test]
async fn an_emit_spends_the_context_it_was_delivered_with() {
    let marker = tempfile::tempdir().expect("marker directory");
    let result_file = marker.path().join("emit-results.jsonl");
    let registry = start(vec![(
        "fixture",
        config(
            "fixture",
            &[
                ("FIXTURE_CAPABILITIES", &capabilities(&["*"], false, &[])),
                (
                    "FIXTURE_EMIT_ON_EVENT",
                    r#"{"name":"note","payload":{"n":1}}"#,
                ),
                ("FIXTURE_EMIT_COUNT", "2"),
                (
                    "FIXTURE_EMIT_RESULT_FILE",
                    result_file.to_str().expect("result path"),
                ),
            ],
        ),
    )])
    .await;
    let (requests, mut received) = tokio::sync::mpsc::unbounded_channel();
    registry.set_emit_handler(Arc::new(move |request: super::PluginEmitRequest| {
        let requests = requests.clone();
        Box::pin(async move {
            let granted = matches!(request.context, PluginEmitContext::Granted);
            let _ = requests.send(request);
            PluginEmitOutcome {
                bus: if granted {
                    ExtensionEmitStatus::Published
                } else {
                    ExtensionEmitStatus::Rejected
                },
                reason: None,
            }
        })
    }));
    wait_for(&registry, "fixture", PluginState::Connected).await;
    let session_id = SessionId::new_v7();
    assert!(
        registry
            .stream_session_event(&stored_event(session_id, 1))
            .is_empty()
    );

    let mut contexts = Vec::new();
    for _ in 0..2 {
        let request = tokio::time::timeout(WAIT, received.recv())
            .await
            .expect("an emit")
            .expect("emit channel");
        assert_eq!(
            (request.plugin.as_str(), request.name.as_str()),
            ("fixture", "note")
        );
        assert_eq!(request.session_id, session_id);
        assert_eq!(request.payload, serde_json::json!({"n": 1}));
        contexts.push(request.context);
    }
    assert_eq!(
        contexts
            .iter()
            .filter(|context| matches!(context, PluginEmitContext::Granted))
            .count(),
        1
    );
    assert!(contexts.iter().any(|context| matches!(
        context,
        PluginEmitContext::Rejected { reason, .. } if reason.contains("already consumed")
    )));
    let mut statuses = lines(&result_file, 2)
        .await
        .into_iter()
        .map(|result| result["bus"].as_str().expect("bus status").to_owned())
        .collect::<Vec<_>>();
    statuses.sort();
    assert_eq!(statuses, ["published", "rejected"]);
    registry.shutdown().await;
}

#[tokio::test]
async fn contexts_are_one_shot_per_session_and_expire() {
    let registry = PluginRegistry::new(IndexMap::from([(
        "fixture".to_owned(),
        config("fixture", &[]),
    )]));
    let runtime = registry
        .inner
        .plugins
        .get("fixture")
        .expect("fixture runtime");
    let (owner, other) = (SessionId::new_v7(), SessionId::new_v7());
    let later = || Instant::now() + Duration::from_secs(5);

    runtime.register_context("granted", owner, later());
    assert!(matches!(
        runtime.consume_context("granted", owner),
        PluginEmitContext::Granted
    ));
    assert!(matches!(
        runtime.consume_context("granted", owner),
        PluginEmitContext::Rejected { diagnostic_session_id: Some(session), reason }
            if session == owner && reason.contains("already consumed")
    ));

    runtime.register_context("misused", owner, later());
    assert!(matches!(
        runtime.consume_context("misused", other),
        PluginEmitContext::Rejected { diagnostic_session_id: Some(session), reason }
            if session == owner && reason.contains("does not match")
    ));

    runtime.register_context("expiring", owner, Instant::now() + Duration::from_millis(1));
    tokio::time::sleep(Duration::from_millis(5)).await;
    assert!(matches!(
        runtime.consume_context("expiring", owner),
        PluginEmitContext::Rejected { diagnostic_session_id: Some(session), .. } if session == owner
    ));

    // A context revoked before its notification was written stays spent.
    runtime.revoke_context("revoked", owner);
    runtime.register_context("revoked", owner, later());
    assert!(matches!(
        runtime.consume_context("revoked", owner),
        PluginEmitContext::Rejected { .. }
    ));

    assert!(matches!(
        runtime.consume_context("unknown", owner),
        PluginEmitContext::Rejected {
            diagnostic_session_id: None,
            ..
        }
    ));
}

#[test]
fn a_full_queue_evicts_what_a_notification_outranks_and_keeps_order() {
    let queue = PluginNotificationQueue::new(3);
    let session_id = SessionId::new_v7();
    let params = raw_params(&serde_json::json!({}));
    let item = |label: &str, class| QueuedNotification {
        method: "plugin/event",
        params: Arc::clone(&params),
        session_id,
        context_id: label.to_owned(),
        class,
    };
    assert_eq!(queue.push(item("chunk", PluginDeliveryClass::Chunk)), None);
    assert_eq!(
        queue.push(item("first", PluginDeliveryClass::Ordinary)),
        None
    );
    assert_eq!(
        queue.push(item("second", PluginDeliveryClass::Ordinary)),
        None
    );
    // Full: an ordinary event evicts the chunk.
    assert_eq!(
        queue.push(item("third", PluginDeliveryClass::Ordinary)),
        Some(PluginDeliveryClass::Chunk)
    );
    // A chunk outranks nothing left, so it is the one dropped.
    assert_eq!(
        queue.push(item("late chunk", PluginDeliveryClass::Chunk)),
        Some(PluginDeliveryClass::Chunk)
    );
    // A terminal event evicts the oldest ordinary one.
    assert_eq!(
        queue.push(item("terminal", PluginDeliveryClass::Terminal)),
        Some(PluginDeliveryClass::Ordinary)
    );
    let delivered = std::iter::from_fn(|| queue.pop())
        .map(|item| item.context_id)
        .collect::<Vec<_>>();
    assert_eq!(delivered, ["second", "third", "terminal"]);
}

/// The borrowed params serialize each event once for every plugin; they
/// must stay wire-identical to the protocol's own params.
#[test]
fn borrowed_params_decode_as_the_protocol_params() {
    let session_id = SessionId::new_v7();
    let event = stored_event(session_id, 7);
    let encoded = super::EncodedEvent::of(&event.payload);
    assert_eq!(encoded.tag, "diagnostic");
    let params = raw_params(&super::EventParamsRef {
        session_id,
        context_id: "context-1",
        seq: event.seq,
        event: &encoded.json,
        timestamp: event.timestamp,
    });
    assert_eq!(
        serde_json::from_str::<cookie_agent_protocol::ExtensionEventParams>(params.get())
            .expect("event params"),
        cookie_agent_protocol::ExtensionEventParams {
            session_id,
            context_id: "context-1".into(),
            seq: 7,
            event: event.payload.clone().into(),
            timestamp: event.timestamp,
        }
    );

    let payload = serde_json::json!({"ok": true});
    let params = raw_params(&super::BusEventParamsRef {
        session_id,
        context_id: "context-2",
        plugin: "other",
        name: "notice",
        payload: &payload,
    });
    assert_eq!(
        serde_json::from_str::<ExtensionBusEventParams>(params.get()).expect("bus params"),
        ExtensionBusEventParams {
            session_id,
            context_id: Some("context-2".into()),
            plugin: "other".into(),
            name: "notice".into(),
            payload,
        }
    );
}

#[tokio::test]
async fn event_patterns_select_what_each_plugin_receives() {
    let marker = tempfile::tempdir().expect("marker directory");
    let subscribers = [
        ("runs", &["run_*"][..]),
        ("diagnostics", &["diagnostic"][..]),
        ("all", &["*"][..]),
        ("none", &[][..]),
    ];
    let files = subscribers.map(|(name, _)| marker.path().join(format!("{name}.jsonl")));
    let registry = start(
        subscribers
            .iter()
            .zip(&files)
            .map(|((name, patterns), file)| {
                (
                    *name,
                    config(
                        name,
                        &[
                            ("FIXTURE_CAPABILITIES", &capabilities(patterns, false, &[])),
                            ("FIXTURE_EVENT_FILE", file.to_str().expect("event path")),
                        ],
                    ),
                )
            })
            .collect(),
    )
    .await;
    for (name, _) in subscribers {
        wait_for(&registry, name, PluginState::Connected).await;
    }
    assert_eq!(
        registry
            .statuses()
            .iter()
            .map(|status| (
                status.name.as_str(),
                status
                    .subscribe_events
                    .iter()
                    .map(|pattern| pattern.as_str().to_owned())
                    .collect::<Vec<_>>()
            ))
            .collect::<Vec<_>>(),
        [
            ("runs", vec!["run_*".to_owned()]),
            ("diagnostics", vec!["diagnostic".to_owned()]),
            ("all", vec!["*".to_owned()]),
            ("none", vec![]),
        ]
    );
    let session_id = SessionId::new_v7();
    let run_id = cookie_agent_protocol::RunId::new_v7();
    assert!(
        registry
            .stream_session_event(&stored_event(session_id, 2))
            .is_empty()
    );
    let mut finished = stored_event(session_id, 3);
    finished.run_id = Some(run_id);
    finished.payload = EventPayload::run_finished(cookie_agent_protocol::RunOutcome::Completed {
        final_text: None,
    });
    assert!(registry.stream_session_event(&finished).is_empty());
    // A live-only event is selected by its type too.
    let delta = cookie_agent_protocol::TransientEvent {
        session_id,
        run_id: Some(run_id),
        after_seq: 3,
        timestamp: jiff::Timestamp::now(),
        payload: EventPayload::TextDelta {
            attempt_id: cookie_agent_protocol::AttemptId::new_v7(),
            text: "live".into(),
        },
    };
    assert!(registry.stream_transient_event(&delta).is_empty());

    let types = |events: Vec<Value>| {
        events
            .iter()
            .map(|event| {
                event["event"]["type"]
                    .as_str()
                    .expect("event type")
                    .to_owned()
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(
        types(lines(&files[2], 3).await),
        ["diagnostic", "run_finished", "text_delta"]
    );
    assert_eq!(types(lines(&files[0], 1).await), ["run_finished"]);
    assert_eq!(types(lines(&files[1], 1).await), ["diagnostic"]);
    // Each plugin's queue is in order, so by now the others would have had
    // theirs; the plugin that subscribes to nothing got none.
    assert!(!files[3].exists());
    registry.shutdown().await;
}
