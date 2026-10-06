use async_trait::async_trait;
use jiff::Timestamp;

use super::*;
use crate as cookie_agent_protocol;
use crate::{EventPayload, MessageStream};

fn delivery_channel() -> (
    mpsc::UnboundedSender<ClientDelivery>,
    mpsc::UnboundedReceiver<ClientDelivery>,
) {
    mpsc::unbounded_channel()
}

fn recovery() -> (
    Arc<RecoveryQueue>,
    mpsc::UnboundedReceiver<(bool, Option<SessionId>)>,
) {
    let (sender, receiver) = mpsc::unbounded_channel();
    (
        Arc::new(RecoveryQueue {
            sender,
            state: StdMutex::new(RecoveryQueueState::default()),
        }),
        receiver,
    )
}

fn runtime_snapshot_json(digit: &str) -> Value {
    let revision = format!("sha256:{}", digit.repeat(64 / digit.len()));
    serde_json::json!({
        "snapshot_schema_version": crate::RuntimeSnapshotSchemaVersion::current(),
        "recipe_registry_revision": revision,
        "catalog_revision": revision,
        "catalog_source": "bootstrap",
        "catalog_state": {
            "stale": true,
            "age": "current",
            "provider_quarantine_count": 0,
            "model_quarantine_count": 0,
            "quarantine_digest": digit.repeat(64 / digit.len()),
            "last_error": null
        },
        "provider_state_revision": revision,
        "provider_store_generation": 1,
        "model_revision": revision,
        "agent_revision": revision,
        "runtime_revision": revision,
        "providers": [],
        "models": [],
        "agents": []
    })
}

fn credential_values(secret: &str) -> crate::ProviderCredentialValues {
    let serialized = Zeroizing::new(format!(r#"{{"api_key":"{secret}"}}"#).into_bytes());
    serde_json::from_slice(&serialized).expect("credential values")
}

fn event(session_id: SessionId, seq: u64) -> StoredEvent {
    StoredEvent {
        engine_version: None,
        origin: None,
        session_id,
        run_id: Some(crate::RunId::new_v7()),
        seq,
        timestamp: Timestamp::now(),
        payload: EventPayload::UserInputAdmitted {
            input: seq.to_string(),
        },
    }
}

struct ClosingStream;

struct FailingStream {
    fail_send: bool,
    sent: bool,
}

#[async_trait]
impl MessageStream for FailingStream {
    async fn send(&mut self, _: MessageFrame) -> Result<(), TransportError> {
        self.sent = true;
        if self.fail_send {
            Err(TransportError::Other(
                "socket write refused; Bearer private-value".into(),
            ))
        } else {
            Ok(())
        }
    }
    async fn recv(&mut self) -> Result<Option<MessageFrame>, TransportError> {
        if !self.sent {
            std::future::pending::<()>().await;
        }
        Err(TransportError::Other(
            "TLS peer reset connection; Bearer private-value".into(),
        ))
    }
}

#[tokio::test]
async fn transport_failures_reach_pending_calls_and_connected_clients() {
    for fail_send in [false, true] {
        let client = Client::connect_stream(FailingStream {
            fail_send,
            sent: false,
        });
        let mut deliveries = client.subscribe_deliveries().unwrap();
        let error = tokio::time::timeout(Duration::from_secs(3), client.handshake())
            .await
            .unwrap()
            .unwrap_err()
            .to_string();
        assert!(error.contains(if fail_send {
            "socket write refused"
        } else {
            "TLS peer reset"
        }));
        assert!(error.contains("private-value"));
        let delivery = tokio::time::timeout(Duration::from_secs(3), deliveries.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(
            matches!(delivery, ClientDelivery::Disconnected { error } if error.contains(if fail_send { "socket write refused" } else { "TLS peer reset" }) && error.contains("private-value"))
        );
    }
}

#[async_trait]
impl MessageStream for ClosingStream {
    async fn send(&mut self, _: MessageFrame) -> Result<(), TransportError> {
        Ok(())
    }

    async fn recv(&mut self) -> Result<Option<MessageFrame>, TransportError> {
        Ok(None)
    }
}

struct ScriptedStream {
    incoming: mpsc::UnboundedReceiver<MessageFrame>,
    sent: mpsc::UnboundedSender<MessageFrame>,
}

#[async_trait]
impl MessageStream for ScriptedStream {
    async fn send(&mut self, frame: MessageFrame) -> Result<(), TransportError> {
        self.sent.send(frame).map_err(|_| TransportError::Closed)
    }

    async fn recv(&mut self) -> Result<Option<MessageFrame>, TransportError> {
        Ok(self.incoming.recv().await)
    }
}

#[tokio::test]
async fn goal_and_producer_calls_use_the_public_wire_contracts() {
    let session_id = SessionId::new_v7();
    let goal_id = crate::GoalId::new_v7();
    let (incoming, incoming_rx) = mpsc::unbounded_channel();
    let (sent, mut sent_rx) = mpsc::unbounded_channel();
    let client = Client::connect_stream(ScriptedStream {
        incoming: incoming_rx,
        sent,
    });

    let get = tokio::spawn({
        let client = client.clone();
        async move {
            client
                .get_session_goal(SessionGoalGetParams { session_id })
                .await
        }
    });
    let MessageFrame::Value(request) = sent_rx.recv().await.expect("goal get request") else {
        panic!("expected value request");
    };
    assert_eq!(request["method"], crate::SESSION_GOAL_GET_METHOD);
    assert_eq!(
        request["params"],
        serde_json::json!({ "session_id": session_id })
    );
    incoming
        .send(MessageFrame::Value(serde_json::json!({
            "jsonrpc": "2.0",
            "id": request["id"],
            "result": { "goal": null },
        })))
        .expect("goal get response");
    assert_eq!(
        get.await.expect("goal get task").expect("goal get").goal,
        None
    );

    let set = tokio::spawn({
        let client = client.clone();
        async move {
            client
                .set_session_goal(SessionGoalSetParams {
                    session_id,
                    objective: "ship phase one".into(),
                    selection: None,
                })
                .await
        }
    });
    let MessageFrame::Value(request) = sent_rx.recv().await.expect("goal set request") else {
        panic!("expected value request");
    };
    assert_eq!(request["method"], crate::SESSION_GOAL_SET_METHOD);
    assert_eq!(
        request["params"],
        serde_json::json!({
            "session_id": session_id,
            "objective": "ship phase one",
        })
    );
    incoming
        .send(MessageFrame::Value(serde_json::json!({
            "jsonrpc": "2.0",
            "id": request["id"],
            "result": {
                "goal": {
                    "goal_id": goal_id,
                    "objective": "ship phase one",
                    "status": "active",
                    "items": [],
                    "revision": 7,
                }
            },
        })))
        .expect("goal set response");
    assert_eq!(
        set.await
            .expect("goal set task")
            .expect("goal set")
            .goal
            .goal_id,
        goal_id
    );

    let lifecycle = tokio::spawn({
        let client = client.clone();
        async move {
            client
                .change_session_goal_lifecycle(SessionGoalLifecycleParams {
                    session_id,
                    goal_id,
                    expected_revision: 7,
                    action: crate::GoalLifecycleAction::Pause,
                    selection: None,
                })
                .await
        }
    });
    let MessageFrame::Value(request) = sent_rx.recv().await.expect("goal lifecycle request") else {
        panic!("expected value request");
    };
    assert_eq!(request["method"], crate::SESSION_GOAL_LIFECYCLE_METHOD);
    assert_eq!(
        request["params"],
        serde_json::json!({
            "session_id": session_id,
            "goal_id": goal_id,
            "expected_revision": 7,
            "action": "pause",
        })
    );
    incoming
        .send(MessageFrame::Value(serde_json::json!({
            "jsonrpc": "2.0",
            "id": request["id"],
            "result": {
                "goal": {
                    "goal_id": goal_id,
                    "objective": "ship phase one",
                    "status": "paused",
                    "items": [],
                    "revision": 8,
                }
            },
        })))
        .expect("goal lifecycle response");
    assert_eq!(
        lifecycle
            .await
            .expect("goal lifecycle task")
            .expect("goal lifecycle")
            .goal
            .revision,
        8
    );

    let producers = tokio::spawn({
        let client = client.clone();
        async move {
            client
                .session_producers(SessionProducersParams { session_id })
                .await
        }
    });
    let MessageFrame::Value(request) = sent_rx.recv().await.expect("session producers request")
    else {
        panic!("expected value request");
    };
    assert_eq!(request["method"], crate::SESSION_PRODUCERS_METHOD);
    assert_eq!(
        request["params"],
        serde_json::json!({ "session_id": session_id })
    );
    incoming
        .send(MessageFrame::Value(serde_json::json!({
            "jsonrpc": "2.0",
            "id": request["id"],
            "result": { "producers": [], "plugin_recovery": [] },
        })))
        .expect("session producers response");
    let result = producers
        .await
        .expect("session producers task")
        .expect("session producers");
    assert!(result.producers.is_empty());
    assert!(result.plugin_recovery.is_empty());
}

#[tokio::test]
async fn live_event_racing_replay_is_delivered_after_replay_end() {
    let session_id = SessionId::new_v7();
    let subscriptions = Arc::new(Mutex::new(HashMap::new()));
    let request = prepare_subscription(&subscriptions, session_id, 0, false, true)
        .await
        .expect("prepare replay");
    let (deliveries, mut receiver) = delivery_channel();
    let (recovery, mut recovery_receiver) = recovery();

    route_live(
        EventSubscriptionMessage::Event {
            event: Box::new(event(session_id, 2)),
        },
        &deliveries,
        &subscriptions,
        &recovery,
    )
    .await;
    begin_replay(
        request,
        vec![event(session_id, 1)],
        &subscriptions,
        &deliveries,
        &recovery,
    )
    .await;

    assert!(matches!(
        receiver.recv().await,
        Some(ClientDelivery::ReplayStart { .. })
    ));
    assert!(
        matches!(receiver.recv().await, Some(ClientDelivery::ReplayEvent { event, .. }) if event.seq == 1)
    );
    assert!(matches!(
        receiver.recv().await,
        Some(ClientDelivery::ReplayEnd { .. })
    ));
    assert!(
        matches!(receiver.recv().await, Some(ClientDelivery::Live { message, .. }) if matches!(message.as_ref(), EventSubscriptionMessage::Event { event } if event.seq == 2))
    );
    route_live(
        EventSubscriptionMessage::Event {
            event: Box::new(event(session_id, 3)),
        },
        &deliveries,
        &subscriptions,
        &recovery,
    )
    .await;
    assert!(matches!(
        receiver.recv().await,
        Some(ClientDelivery::Live { message, .. })
            if matches!(message.as_ref(), EventSubscriptionMessage::Event { event } if event.seq == 3)
    ));
    assert!(
        tokio::time::timeout(Duration::from_millis(10), recovery_receiver.recv())
            .await
            .is_err()
    );
}

fn transient(session_id: SessionId, after_seq: u64, text: &str) -> EventSubscriptionMessage {
    EventSubscriptionMessage::Transient {
        event: Box::new(crate::TransientEvent {
            session_id,
            run_id: Some(crate::RunId::new_v7()),
            after_seq,
            timestamp: Timestamp::now(),
            payload: EventPayload::TextDelta {
                attempt_id: crate::AttemptId::new_v7(),
                text: text.into(),
            },
        }),
    }
}

fn delivered_text(delivery: Option<ClientDelivery>) -> Option<String> {
    match delivery? {
        ClientDelivery::Live { message, .. } => match *message {
            EventSubscriptionMessage::Transient { event } => match event.payload {
                EventPayload::TextDelta { text, .. } => Some(text),
                _ => None,
            },
            EventSubscriptionMessage::Event { event } => Some(format!("event {}", event.seq)),
            EventSubscriptionMessage::Gap { .. } | EventSubscriptionMessage::Rewound { .. } => None,
        },
        _ => None,
    }
}

/// Live-only output reaches the view only right after the durable event it
/// follows. It never moves the cursor, never asks for recovery, and output
/// buffered behind a replay survives only if it follows the replayed tip.
#[tokio::test]
async fn transient_output_follows_its_durable_event_or_is_dropped() {
    let session_id = SessionId::new_v7();
    let subscriptions = Arc::new(Mutex::new(HashMap::new()));
    let request = prepare_subscription(&subscriptions, session_id, 0, false, true)
        .await
        .expect("prepare replay");
    let (deliveries, mut receiver) = delivery_channel();
    let (recovery, mut recovery_receiver) = recovery();

    // Buffered while the replay is in flight: stale output, output after the
    // replayed tip, then a durable event and the output that follows it.
    for message in [
        transient(session_id, 1, "stale"),
        transient(session_id, 2, "after replay"),
        EventSubscriptionMessage::Event {
            event: Box::new(event(session_id, 3)),
        },
        transient(session_id, 2, "overtaken"),
        transient(session_id, 3, "after three"),
    ] {
        route_live(message, &deliveries, &subscriptions, &recovery).await;
    }
    begin_replay(
        request,
        vec![event(session_id, 1), event(session_id, 2)],
        &subscriptions,
        &deliveries,
        &recovery,
    )
    .await;
    assert!(matches!(
        receiver.recv().await,
        Some(ClientDelivery::ReplayStart { final_seq: 2, .. })
    ));
    for _ in 0..2 {
        assert!(matches!(
            receiver.recv().await,
            Some(ClientDelivery::ReplayEvent { .. })
        ));
    }
    assert!(matches!(
        receiver.recv().await,
        Some(ClientDelivery::ReplayEnd { .. })
    ));
    for expected in ["after replay", "event 3", "after three"] {
        assert_eq!(
            delivered_text(receiver.recv().await).as_deref(),
            Some(expected)
        );
    }
    assert_eq!(subscriptions.lock().await[&session_id].cursor, 3);

    // Live: only output at the cursor is delivered; none of it moves the
    // cursor or schedules a recovery.
    for message in [
        transient(session_id, 3, "live"),
        transient(session_id, 2, "late"),
        transient(session_id, 9, "ahead"),
        transient(session_id, 3, "live again"),
    ] {
        route_live(message, &deliveries, &subscriptions, &recovery).await;
    }
    for expected in ["live", "live again"] {
        assert_eq!(
            delivered_text(receiver.recv().await).as_deref(),
            Some(expected)
        );
    }
    assert!(receiver.try_recv().is_err());
    assert_eq!(subscriptions.lock().await[&session_id].cursor, 3);
    assert!(
        tokio::time::timeout(Duration::from_millis(10), recovery_receiver.recv())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn replay_larger_than_delivery_capacity_reduces_completely() {
    let session_id = SessionId::new_v7();
    let subscriptions = Arc::new(Mutex::new(HashMap::new()));
    let request = prepare_subscription(&subscriptions, session_id, 0, false, true)
        .await
        .expect("prepare replay");
    let (deliveries, mut receiver) = mpsc::unbounded_channel();
    let (recovery, _recovery_receiver) = recovery();
    let events = (1..=2_000).map(|seq| event(session_id, seq)).collect();
    let task = tokio::spawn({
        let subscriptions = subscriptions.clone();
        let recovery = recovery.clone();
        async move {
            begin_replay(request, events, &subscriptions, &deliveries, &recovery).await;
        }
    });
    let mut replayed = 0;
    while let Some(delivery) = receiver.recv().await {
        let ended = matches!(&delivery, ClientDelivery::ReplayEnd { .. });
        replayed += usize::from(matches!(delivery, ClientDelivery::ReplayEvent { .. }));
        if ended {
            break;
        }
    }
    task.await.expect("replay task");
    assert_eq!(replayed, 2_000);
}

#[tokio::test]
async fn rpc_completes_while_an_unconsumed_replay_backlog_grows() {
    let session_id = SessionId::new_v7();
    let (incoming, incoming_rx) = mpsc::unbounded_channel();
    let (sent, mut sent_rx) = mpsc::unbounded_channel();
    let client = Client::connect_stream(ScriptedStream {
        incoming: incoming_rx,
        sent,
    });
    let _deliveries = client.subscribe_deliveries().expect("delivery receiver");
    let subscribe = tokio::spawn({
        let client = client.clone();
        async move { client.subscribe_events(session_id, None).await }
    });
    let MessageFrame::Value(subscribe_request) = sent_rx.recv().await.expect("subscribe request")
    else {
        panic!("expected value request");
    };
    assert_eq!(subscribe_request["method"], "events.subscribe");
    incoming
            .send(MessageFrame::Value(serde_json::json!({
                "jsonrpc": "2.0",
                "id": subscribe_request["id"],
                "result": { "events": (1..=2_000).map(|seq| event(session_id, seq)).collect::<Vec<_>>() },
            })))
            .expect("replay response");
    subscribe
        .await
        .expect("subscribe task")
        .expect("subscribe result");

    let tree = tokio::spawn({
        let client = client.clone();
        async move { client.session_tree(SessionTreeParams { session_id }).await }
    });
    let MessageFrame::Value(tree_request) =
        tokio::time::timeout(Duration::from_secs(1), sent_rx.recv())
            .await
            .expect("tree request was not blocked")
            .expect("connection open")
    else {
        panic!("expected value request");
    };
    assert_eq!(tree_request["method"], "session.tree");
    tree.abort();
}

#[tokio::test]
async fn timed_out_calls_are_pruned_and_late_responses_are_harmless() {
    let (incoming, incoming_rx) = mpsc::unbounded_channel();
    let (sent, mut sent_rx) = mpsc::unbounded_channel();
    let client = Client::connect_stream(ScriptedStream {
        incoming: incoming_rx,
        sent,
    });
    let mut timed_out_requests = Vec::new();

    for _ in 0..32 {
        let call = tokio::spawn({
            let client = client.clone();
            async move {
                tokio::time::timeout(
                    Duration::from_millis(5),
                    client.create_session(SessionCreateParams {
                        selection: cookie_agent_protocol::RunSelection {
                            agent: cookie_agent_protocol::AgentId::new("primary")
                                .expect("agent id"),
                            model: cookie_agent_protocol::ModelSelection {
                                model: "gateway/arbitrary-model"
                                    .parse::<cookie_agent_protocol::ModelKey>()
                                    .expect("model key"),
                                variant: None,
                            },
                            preset: None,
                        },
                    }),
                )
                .await
            }
        });
        let MessageFrame::Value(request) =
            tokio::time::timeout(Duration::from_secs(1), sent_rx.recv())
                .await
                .expect("session.create request timeout")
                .expect("connection open")
        else {
            panic!("expected value request");
        };
        assert_eq!(request["method"], "session.create");
        timed_out_requests.push(request);
        assert!(call.await.expect("call task").is_err());
    }

    tokio::time::timeout(Duration::from_secs(1), async {
        while client.pending_command_count() != 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("cancelled requests were not pruned");
    assert_eq!(client.pending_command_count(), 0);

    for request in timed_out_requests {
        incoming
            .send(MessageFrame::Value(serde_json::json!({
                "jsonrpc": "2.0",
                "id": request["id"],
                "result": null,
            })))
            .expect("late response");
    }

    let snapshot = tokio::spawn({
        let client = client.clone();
        async move { client.runtime_snapshot().await }
    });
    let MessageFrame::Value(request) = tokio::time::timeout(Duration::from_secs(1), sent_rx.recv())
        .await
        .expect("runtime snapshot request timeout")
        .expect("connection open")
    else {
        panic!("expected value request");
    };
    assert_eq!(request["method"], "runtime.snapshot.get");
    incoming
        .send(MessageFrame::Value(serde_json::json!({
            "jsonrpc": "2.0",
            "id": request["id"],
            "result": { "snapshot": runtime_snapshot_json("0") },
        })))
        .expect("runtime snapshot response");
    assert!(
        snapshot
            .await
            .expect("snapshot task")
            .expect("runtime snapshot result")
            .snapshot
            .agents
            .is_empty()
    );
    tokio::time::timeout(Duration::from_secs(1), async {
        while client.pending_command_count() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("completed request remained pending");
    assert_eq!(client.pending_command_count(), 0);
}

#[tokio::test]
async fn runtime_changed_notifications_reach_the_ui_in_transport_order() {
    let (incoming, incoming_rx) = mpsc::unbounded_channel();
    let (sent, _sent_rx) = mpsc::unbounded_channel();
    let client = Client::connect_stream(ScriptedStream {
        incoming: incoming_rx,
        sent,
    });
    let mut deliveries = client.subscribe_deliveries().expect("delivery receiver");
    for (previous, digit, reason) in [(None, "1", "startup"), (Some("1"), "2", "config_reloaded")] {
        let previous_revision = previous
            .map(|digit| serde_json::json!(format!("sha256:{}", digit.repeat(64 / digit.len()))));
        incoming
            .send(MessageFrame::Value(serde_json::json!({
                "jsonrpc": "2.0",
                "method": "runtime.changed",
                "params": {
                    "previous_revision": previous_revision,
                    "snapshot": runtime_snapshot_json(digit),
                    "reasons": [reason]
                }
            })))
            .expect("runtime notification");
    }
    let first = deliveries.recv().await.expect("first delivery");
    let second = deliveries.recv().await.expect("second delivery");
    let ClientDelivery::RuntimeChanged(first) = first else {
        panic!("runtime delivery")
    };
    let ClientDelivery::RuntimeChanged(second) = second else {
        panic!("runtime delivery")
    };
    assert_eq!(first.previous_revision, None);
    assert_eq!(
        second.previous_revision.as_ref(),
        Some(&first.snapshot.runtime_revision)
    );
}

#[tokio::test]
async fn cancelled_provider_connect_wipes_source_and_serialized_credentials() {
    let source_before = PROVIDER_CONNECT_WIPE_COUNT.load(Ordering::Relaxed);
    let serialized_before = SENSITIVE_SERIALIZED_WIPE_COUNT.load(Ordering::Relaxed);
    let (_incoming, incoming_rx) = mpsc::unbounded_channel();
    let (sent, mut sent_rx) = mpsc::unbounded_channel();
    let client = Client::connect_stream(ScriptedStream {
        incoming: incoming_rx,
        sent,
    });
    let connect = tokio::spawn({
        let client = client.clone();
        async move {
            client
                .connect_provider(cookie_agent_protocol::ProviderConnectParams {
                    client_connect_id: cookie_agent_protocol::ClientConnectId::new("connect-test")
                        .expect("client connect id"),
                    provider_id: cookie_agent_protocol::ProviderId::new("test")
                        .expect("provider id"),
                    expected_catalog_revision: cookie_agent_protocol::CatalogRevision::new(
                        format!("sha256:{}", "1".repeat(64)),
                    )
                    .expect("catalog revision"),
                    setup_values: std::collections::BTreeMap::new(),
                    auth_method: cookie_agent_protocol::AuthMethodId::new("api-key")
                        .expect("auth method"),
                    auth_values: credential_values("sentinel-secret"),
                })
                .await
        }
    });
    let mut request = tokio::time::timeout(Duration::from_secs(1), sent_rx.recv())
        .await
        .expect("provider.connect request timeout")
        .expect("connection open");
    assert!(matches!(
        &request,
        MessageFrame::Text(text)
            if text.contains("\"method\":\"provider.connect\"")
                && text.contains("sentinel-secret")
    ));
    if let MessageFrame::Text(text) = &mut request {
        text.zeroize();
    }
    drop(request);
    connect.abort();
    let _ = connect.await;

    tokio::time::timeout(Duration::from_secs(1), async {
        while PROVIDER_CONNECT_WIPE_COUNT.load(Ordering::Relaxed) <= source_before
            || SENSITIVE_SERIALIZED_WIPE_COUNT.load(Ordering::Relaxed) <= serialized_before
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("credential guards were not dropped after cancellation");
}

#[test]
fn sensitive_frame_debug_is_redacted_and_drop_is_observable() {
    let before = SENSITIVE_FRAME_WIPE_COUNT.load(Ordering::Relaxed);
    let frame = SensitiveFrame::new("sentinel-secret".into());
    let debug = format!("{frame:?}");
    assert_eq!(debug, "SensitiveFrame(<redacted>)");
    assert!(!debug.contains("sentinel-secret"));
    drop(frame);
    assert!(SENSITIVE_FRAME_WIPE_COUNT.load(Ordering::Relaxed) > before);
}

#[test]
fn secret_json_owner_wipes_its_sentinel_tree_on_drop() {
    let before = SENSITIVE_SERIALIZED_WIPE_COUNT.load(Ordering::Relaxed);
    let mut value = SensitiveJson::object();
    value.object_mut().insert(
        "auth_values".into(),
        serde_json::json!({"api_key": "sentinel-secret"}),
    );
    drop(value);
    assert!(SENSITIVE_SERIALIZED_WIPE_COUNT.load(Ordering::Relaxed) > before);
}

#[test]
fn cancelled_unpolled_sensitive_dispatch_drops_the_wiping_frame() {
    let before = SENSITIVE_FRAME_WIPE_COUNT.load(Ordering::Relaxed);
    let mut stream = ClosingStream;
    let dispatch = send_outbound_frame(
        &mut stream,
        OutboundFrame::Sensitive(SensitiveFrame::new("sentinel-secret".into())),
    );
    drop(dispatch);
    assert!(SENSITIVE_FRAME_WIPE_COUNT.load(Ordering::Relaxed) > before);
}

#[tokio::test]
async fn unpolled_provider_connect_future_still_wipes_owned_credentials() {
    let source_before = PROVIDER_CONNECT_WIPE_COUNT.load(Ordering::Relaxed);
    let client = Client::connect_stream(ClosingStream);
    let connect = client.connect_provider(cookie_agent_protocol::ProviderConnectParams {
        client_connect_id: cookie_agent_protocol::ClientConnectId::new("unpolled")
            .expect("client connect id"),
        provider_id: cookie_agent_protocol::ProviderId::new("test").expect("provider id"),
        expected_catalog_revision: cookie_agent_protocol::CatalogRevision::new(format!(
            "sha256:{}",
            "1".repeat(64)
        ))
        .expect("catalog revision"),
        setup_values: std::collections::BTreeMap::new(),
        auth_method: cookie_agent_protocol::AuthMethodId::new("api-key").expect("auth method"),
        auth_values: credential_values("sentinel-secret"),
    });
    drop(connect);
    assert!(PROVIDER_CONNECT_WIPE_COUNT.load(Ordering::Relaxed) > source_before);
}

#[tokio::test]
async fn failed_provider_connect_wipes_source_and_sensitive_json_tree() {
    let source_before = PROVIDER_CONNECT_WIPE_COUNT.load(Ordering::Relaxed);
    let serialized_before = SENSITIVE_SERIALIZED_WIPE_COUNT.load(Ordering::Relaxed);
    let client = Client::connect_stream(ClosingStream);
    let result = client
        .connect_provider(cookie_agent_protocol::ProviderConnectParams {
            client_connect_id: cookie_agent_protocol::ClientConnectId::new("failed-connect")
                .expect("client connect id"),
            provider_id: cookie_agent_protocol::ProviderId::new("test").expect("provider id"),
            expected_catalog_revision: cookie_agent_protocol::CatalogRevision::new(format!(
                "sha256:{}",
                "1".repeat(64)
            ))
            .expect("catalog revision"),
            setup_values: std::collections::BTreeMap::new(),
            auth_method: cookie_agent_protocol::AuthMethodId::new("api-key").expect("auth method"),
            auth_values: credential_values("sentinel-secret"),
        })
        .await;
    assert!(result.is_err());
    assert!(PROVIDER_CONNECT_WIPE_COUNT.load(Ordering::Relaxed) > source_before);
    assert!(SENSITIVE_SERIALIZED_WIPE_COUNT.load(Ordering::Relaxed) > serialized_before);
}

#[tokio::test]
async fn non_contiguous_buffered_tail_keeps_prefix_cursor_and_recovers() {
    let session_id = SessionId::new_v7();
    let subscriptions = Arc::new(Mutex::new(HashMap::new()));
    let request = prepare_subscription(&subscriptions, session_id, 0, false, true)
        .await
        .expect("prepare replay");
    let (deliveries, _receiver) = delivery_channel();
    let (recovery, mut recovery_receiver) = recovery();
    for seq in [11, 13] {
        route_live(
            EventSubscriptionMessage::Event {
                event: Box::new(event(session_id, seq)),
            },
            &deliveries,
            &subscriptions,
            &recovery,
        )
        .await;
    }
    begin_replay(
        request,
        (1..=10).map(|seq| event(session_id, seq)).collect(),
        &subscriptions,
        &deliveries,
        &recovery,
    )
    .await;
    assert_eq!(
        subscriptions.lock().await[&session_id].cursor,
        11,
        "cursor stops before the missing sequence"
    );
    assert_eq!(
        recovery_receiver.recv().await,
        Some((false, Some(session_id)))
    );
}

#[tokio::test]
async fn buffered_gap_schedules_one_recovery() {
    let session_id = SessionId::new_v7();
    let subscriptions = Arc::new(Mutex::new(HashMap::new()));
    let request = prepare_subscription(&subscriptions, session_id, 0, false, true)
        .await
        .expect("prepare replay");
    let (deliveries, _receiver) = delivery_channel();
    let (recovery, mut recovery_receiver) = recovery();
    route_live(
        EventSubscriptionMessage::Gap {
            session_id,
            last_delivered_seq: 0,
        },
        &deliveries,
        &subscriptions,
        &recovery,
    )
    .await;
    begin_replay(
        request,
        vec![event(session_id, 1)],
        &subscriptions,
        &deliveries,
        &recovery,
    )
    .await;
    assert_eq!(
        recovery_receiver.recv().await,
        Some((false, Some(session_id)))
    );
    assert!(recovery_receiver.try_recv().is_err());
}

#[tokio::test]
async fn rewound_pulls_the_cursor_back_and_queues_one_full_replay() {
    let session_id = SessionId::new_v7();
    let subscriptions = Arc::new(Mutex::new(HashMap::new()));
    let (deliveries, mut delivered) = delivery_channel();
    let (recovery, mut recovery_receiver) = recovery();
    for seq in 1..=5 {
        route_live(
            EventSubscriptionMessage::Event {
                event: Box::new(event(session_id, seq)),
            },
            &deliveries,
            &subscriptions,
            &recovery,
        )
        .await;
    }
    route_live(
        EventSubscriptionMessage::Rewound {
            session_id,
            through_seq: 3,
        },
        &deliveries,
        &subscriptions,
        &recovery,
    )
    .await;
    {
        let subscriptions = subscriptions.lock().await;
        let subscription = &subscriptions[&session_id];
        assert_eq!(subscription.cursor, 3);
        assert_eq!(subscription.cursor_timestamp, None);
    }
    assert_eq!(
        recovery_receiver.recv().await,
        Some((true, Some(session_id)))
    );
    assert!(recovery_receiver.try_recv().is_err());
    // The rewind reaches the application too, after the events it cuts.
    let mut last = None;
    while let Ok(delivery) = delivered.try_recv() {
        last = Some(delivery);
    }
    assert!(matches!(
        last,
        Some(ClientDelivery::Live { message, .. })
            if matches!(*message, EventSubscriptionMessage::Rewound { through_seq: 3, .. })
    ));
    // Appends after the cut reuse its sequences and follow on from it.
    route_live(
        EventSubscriptionMessage::Event {
            event: Box::new(event(session_id, 4)),
        },
        &deliveries,
        &subscriptions,
        &recovery,
    )
    .await;
    assert_eq!(subscriptions.lock().await[&session_id].cursor, 4);
    assert!(recovery_receiver.try_recv().is_err());
}

#[tokio::test]
async fn stale_replay_attempt_response_is_discarded() {
    let session_id = SessionId::new_v7();
    let subscriptions = Arc::new(Mutex::new(HashMap::new()));
    let first = prepare_subscription(&subscriptions, session_id, 0, false, true)
        .await
        .expect("first request");
    subscriptions
        .lock()
        .await
        .get_mut(&session_id)
        .expect("subscription")
        .fetching = false;
    let second = prepare_subscription(&subscriptions, session_id, 0, false, true)
        .await
        .expect("second request");
    let (deliveries, mut receiver) = delivery_channel();
    let (recovery, _recovery_receiver) = recovery();
    begin_replay(
        first,
        vec![event(session_id, 1)],
        &subscriptions,
        &deliveries,
        &recovery,
    )
    .await;
    assert!(receiver.try_recv().is_err());
    begin_replay(
        second,
        vec![event(session_id, 1)],
        &subscriptions,
        &deliveries,
        &recovery,
    )
    .await;
    assert!(matches!(
        receiver.recv().await,
        Some(ClientDelivery::ReplayStart { .. })
    ));
}

#[tokio::test]
async fn discontinuity_queues_targeted_recovery() {
    let session_id = SessionId::new_v7();
    let subscriptions = Arc::new(Mutex::new(HashMap::new()));
    let (deliveries, _receiver) = delivery_channel();
    let (recovery, mut receiver) = recovery();
    route_live(
        EventSubscriptionMessage::Event {
            event: Box::new(event(session_id, 2)),
        },
        &deliveries,
        &subscriptions,
        &recovery,
    )
    .await;
    assert_eq!(receiver.recv().await, Some((false, Some(session_id))));
    assert!(receiver.try_recv().is_err());
}

#[tokio::test]
async fn recovery_worker_retries_then_reports_failure() {
    let session_id = SessionId::new_v7();
    let (commands, mut command_receiver) = mpsc::channel(8);
    let subscriptions = Arc::new(Mutex::new(HashMap::from([(
        session_id,
        Subscription::default(),
    )])));
    let (recovery, recovery_receiver) = recovery();
    let (controls, mut control_receiver) = mpsc::unbounded_channel();
    spawn_recovery_worker(
        commands,
        subscriptions,
        recovery_receiver,
        recovery.clone(),
        controls,
        Duration::from_millis(10),
    );
    Client::schedule_recovery_queue(&recovery, true, Some(session_id));
    for _ in 0..RECOVERY_ATTEMPTS {
        let command = command_receiver.recv().await.expect("recovery request");
        assert_eq!(command.method, "events.subscribe");
        command
            .response
            .send(Err(ClientError::Closed))
            .expect("fail replay");
    }
    assert!(matches!(
        control_receiver.recv().await,
        Some(ConnectionControl::RecoveryFailed {
            session_id: Some(id),
            ..
        }) if id == session_id
    ));
}

#[tokio::test]
async fn recovery_timeout_retries_then_gives_up() {
    let session_id = SessionId::new_v7();
    let (commands, mut command_receiver) = mpsc::channel(8);
    let subscriptions = Arc::new(Mutex::new(HashMap::from([(
        session_id,
        Subscription::default(),
    )])));
    let (recovery, recovery_receiver) = recovery();
    let (controls, mut control_receiver) = mpsc::unbounded_channel();
    spawn_recovery_worker(
        commands,
        subscriptions,
        recovery_receiver,
        recovery.clone(),
        controls,
        Duration::from_millis(10),
    );
    Client::schedule_recovery_queue(&recovery, true, Some(session_id));
    let mut held_commands = Vec::new();
    for _ in 0..RECOVERY_ATTEMPTS {
        let mut command = command_receiver.recv().await.expect("recovery request");
        // Written but never answered: only the response deadline runs.
        let _ = command
            .written
            .take()
            .expect("replay requests track writes")
            .send(());
        held_commands.push(command);
    }
    assert!(matches!(
        control_receiver.recv().await,
        Some(ConnectionControl::RecoveryFailed { error, .. }) if error.contains("timed out")
    ));
}

#[tokio::test]
async fn queued_recovery_dies_with_a_disconnected_connection() {
    let client = Client::connect_stream(ClosingStream);
    let recovery = Arc::downgrade(&client.recovery);
    let mut deliveries = client.subscribe_deliveries().expect("delivery receiver");
    assert!(deliveries.recv().await.is_none());
    let session_id = SessionId::new_v7();
    client
        .subscriptions
        .lock()
        .await
        .insert(session_id, Subscription::default());
    client.recover_session(session_id, true);
    drop(client);
    tokio::time::timeout(Duration::from_secs(1), async {
        while recovery.upgrade().is_some() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("recovery worker released after disconnect");
}

async fn next_request(sent: &mut mpsc::UnboundedReceiver<MessageFrame>) -> Value {
    let MessageFrame::Value(request) = tokio::time::timeout(Duration::from_secs(1), sent.recv())
        .await
        .expect("request sent")
        .expect("connection open")
    else {
        panic!("expected value request");
    };
    request
}

fn replay_page(
    request: &Value,
    session_id: SessionId,
    seqs: impl IntoIterator<Item = u64>,
    has_more: bool,
) -> MessageFrame {
    MessageFrame::Value(serde_json::json!({
        "jsonrpc": "2.0",
        "id": request["id"],
        "result": {
            "events": seqs.into_iter().map(|seq| event(session_id, seq)).collect::<Vec<_>>(),
            "has_more": has_more,
        },
    }))
}

#[tokio::test]
async fn replay_is_fetched_in_pages_and_delivered_whole() {
    let session_id = SessionId::new_v7();
    let (incoming, incoming_rx) = mpsc::unbounded_channel();
    let (sent, mut sent_rx) = mpsc::unbounded_channel();
    let client = Client::connect_stream(ScriptedStream {
        incoming: incoming_rx,
        sent,
    });
    let mut deliveries = client.subscribe_deliveries().expect("delivery receiver");
    let subscribe = tokio::spawn({
        let client = client.clone();
        async move { client.subscribe_events(session_id, None).await }
    });

    let first = next_request(&mut sent_rx).await;
    assert_eq!(first["method"], "events.subscribe");
    assert_eq!(first["params"]["cursor"], 0);
    assert_eq!(first["params"]["limit"], REPLAY_PAGE_EVENTS.get());
    incoming
        .send(replay_page(&first, session_id, 1..=3, true))
        .expect("first page");
    let second = next_request(&mut sent_rx).await;
    assert_eq!(
        second["params"]["cursor"], 3,
        "the next page starts after the last event"
    );
    // Nothing reaches the UI until the final page: it sees one replay.
    assert!(deliveries.try_recv().is_err());
    incoming
        .send(replay_page(&second, session_id, 4..=5, false))
        .expect("final page");
    subscribe
        .await
        .expect("subscribe task")
        .expect("subscribe result");

    assert!(matches!(
        deliveries.recv().await,
        Some(ClientDelivery::ReplayStart { final_seq: 5, .. })
    ));
    for seq in 1..=5 {
        assert!(matches!(
            deliveries.recv().await,
            Some(ClientDelivery::ReplayEvent { event, final_seq: 5, .. }) if event.seq == seq
        ));
    }
    assert!(matches!(
        deliveries.recv().await,
        Some(ClientDelivery::ReplayEnd { final_seq: 5, .. })
    ));
}

#[tokio::test]
async fn recovery_checks_its_cursor_event_and_restarts_when_it_is_stale() {
    let session_id = SessionId::new_v7();
    let (incoming, incoming_rx) = mpsc::unbounded_channel();
    let (sent, mut sent_rx) = mpsc::unbounded_channel();
    let client = Client::connect_stream(ScriptedStream {
        incoming: incoming_rx,
        sent,
    });
    let _deliveries = client.subscribe_deliveries().expect("delivery receiver");
    let subscribe = tokio::spawn({
        let client = client.clone();
        async move { client.subscribe_events(session_id, None).await }
    });
    let initial = next_request(&mut sent_rx).await;
    assert!(initial["params"].get("cursor_timestamp").is_none());
    let events = (1..=3)
        .map(|seq| event(session_id, seq))
        .collect::<Vec<_>>();
    let cursor_timestamp = events[2].timestamp;
    incoming
        .send(MessageFrame::Value(serde_json::json!({
            "jsonrpc": "2.0",
            "id": initial["id"],
            "result": { "events": events },
        })))
        .expect("initial replay");
    subscribe
        .await
        .expect("subscribe task")
        .expect("subscribe result");

    // An incremental recovery names the event it resumes after.
    client.recover_session(session_id, false);
    let resume = next_request(&mut sent_rx).await;
    assert_eq!(resume["params"]["cursor"], 3);
    assert_eq!(
        resume["params"]["cursor_timestamp"],
        serde_json::json!(cursor_timestamp)
    );
    incoming
        .send(MessageFrame::Value(serde_json::json!({
            "jsonrpc": "2.0",
            "id": resume["id"],
            "result": { "events": [], "stale_cursor": true },
        })))
        .expect("stale answer");

    // A revert replaced it, so the session replays from the start.
    let full = next_request(&mut sent_rx).await;
    assert_eq!(full["params"]["cursor"], 0);
    assert!(full["params"].get("cursor_timestamp").is_none());
}

#[tokio::test]
async fn a_replay_outlives_a_caller_that_stops_waiting() {
    let session_id = SessionId::new_v7();
    let (incoming, incoming_rx) = mpsc::unbounded_channel();
    let (sent, mut sent_rx) = mpsc::unbounded_channel();
    let client = Client::connect_stream(ScriptedStream {
        incoming: incoming_rx,
        sent,
    });
    let mut deliveries = client.subscribe_deliveries().expect("delivery receiver");
    let caller = tokio::spawn({
        let client = client.clone();
        async move { client.subscribe_events(session_id, None).await }
    });
    let request = next_request(&mut sent_rx).await;
    caller.abort();
    let _ = caller.await;

    incoming
        .send(replay_page(&request, session_id, 1..=1, false))
        .expect("final page");
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(1), deliveries.recv()).await,
        Ok(Some(ClientDelivery::ReplayStart { .. }))
    ));
    assert!(matches!(
        deliveries.recv().await,
        Some(ClientDelivery::ReplayEvent { .. })
    ));
    assert!(matches!(
        deliveries.recv().await,
        Some(ClientDelivery::ReplayEnd { .. })
    ));

    // The session is not left marked as fetching: a later replay is accepted.
    let again = tokio::spawn({
        let client = client.clone();
        async move { client.subscribe_events(session_id, Some(1)).await }
    });
    let request = next_request(&mut sent_rx).await;
    assert_eq!(request["params"]["cursor"], 1);
    incoming
        .send(replay_page(&request, session_id, [], false))
        .expect("empty final page");
    again.await.expect("second caller").expect("second replay");
}
