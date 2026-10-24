use async_trait::async_trait;
use jiff::Timestamp;

use super::*;
use crate as cookie_agent_protocol;
use crate::{ClientSubscriptionMessage as EventSubscriptionMessage, EventPayload, MessageStream};

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
    serde_json::json!({
        "revision": format!("view-{digit}"),
        "catalog": {
            "source": "bootstrap",
            "stale": true,
            "age": "current",
            "provider_quarantine_count": 0,
            "model_quarantine_count": 0,
            "last_error": null
        },
        "providers": [],
        "models": [],
        "agents": []
    })
}

fn credential_values(secret: &str) -> crate::ProviderCredentialValues {
    let serialized = Zeroizing::new(format!(r#"{{"api_key":"{secret}"}}"#).into_bytes());
    serde_json::from_slice(&serialized).expect("credential values")
}

fn event(session_id: SessionId, seq: u64) -> ClientEvent {
    crate::StoredEvent {
        engine_version: None,
        origin: None,
        session_id,
        run_id: Some(crate::RunId::new_v7()),
        seq,
        timestamp: Timestamp::now(),
        payload: EventPayload::UserInputAdmitted {
            content: seq.to_string().into(),
        },
    }
    .into()
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
    let request = request_json(sent_rx.recv().await.expect("goal get request"));
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
    let request = request_json(sent_rx.recv().await.expect("goal set request"));
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
    let request = request_json(sent_rx.recv().await.expect("goal lifecycle request"));
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
    let request = request_json(sent_rx.recv().await.expect("session producers request"));
    assert_eq!(request["method"], crate::SESSION_PRODUCERS_METHOD);
    assert_eq!(
        request["params"],
        serde_json::json!({ "session_id": session_id })
    );
    incoming
        .send(MessageFrame::Value(serde_json::json!({
            "jsonrpc": "2.0",
            "id": request["id"],
            "result": { "producers": [], "sources": [] },
        })))
        .expect("session producers response");
    let result = producers
        .await
        .expect("session producers task")
        .expect("session producers");
    assert!(result.producers.is_empty());
    assert!(result.sources.is_empty());
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
            event: std::sync::Arc::new(event(session_id, 2)),
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
        matches!(receiver.recv().await, Some(ClientDelivery::Live { message, .. }) if matches!(message.as_ref(), crate::EventSubscriptionMessage::Event { event } if event.seq == 2))
    );
    route_live(
        EventSubscriptionMessage::Event {
            event: std::sync::Arc::new(event(session_id, 3)),
        },
        &deliveries,
        &subscriptions,
        &recovery,
    )
    .await;
    assert!(matches!(
        receiver.recv().await,
        Some(ClientDelivery::Live { message, .. })
            if matches!(message.as_ref(), crate::EventSubscriptionMessage::Event { event } if event.seq == 3)
    ));
    assert!(
        tokio::time::timeout(Duration::from_millis(10), recovery_receiver.recv())
            .await
            .is_err()
    );
}

fn transient(session_id: SessionId, after_seq: u64, text: &str) -> EventSubscriptionMessage {
    EventSubscriptionMessage::Transient {
        event: std::sync::Arc::new(crate::TransientEvent {
            session_id,
            run_id: Some(crate::RunId::new_v7()),
            after_seq,
            timestamp: Timestamp::now(),
            payload: EventPayload::TextDelta {
                attempt_id: crate::AttemptId::new_v7(),
                text: text.into(),
            }
            .into(),
        }),
    }
}

fn delivered_text(delivery: Option<ClientDelivery>) -> Option<String> {
    match delivery? {
        ClientDelivery::Live { message, .. } => match *message {
            crate::EventSubscriptionMessage::Transient { event } => {
                match std::sync::Arc::unwrap_or_clone(event).payload {
                    EventPayload::TextDelta { text, .. } => Some(text),
                    _ => None,
                }
            }
            crate::EventSubscriptionMessage::Event { event } => {
                Some(format!("event {}", event.seq))
            }
            crate::EventSubscriptionMessage::Gap { .. }
            | crate::EventSubscriptionMessage::Rewound { .. } => None,
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
            event: std::sync::Arc::new(event(session_id, 3)),
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
    let subscribe_request = request_json(sent_rx.recv().await.expect("subscribe request"));
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
    let tree_request = request_json(
        tokio::time::timeout(Duration::from_secs(1), sent_rx.recv())
            .await
            .expect("tree request was not blocked")
            .expect("connection open"),
    );
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
                    client.create_session(SessionCreateParams::new(
                        cookie_agent_protocol::RunSelection {
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
                    )),
                )
                .await
            }
        });
        let request = request_json(
            tokio::time::timeout(Duration::from_secs(1), sent_rx.recv())
                .await
                .expect("session.create request timeout")
                .expect("connection open"),
        );
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
    let request = request_json(
        tokio::time::timeout(Duration::from_secs(1), sent_rx.recv())
            .await
            .expect("runtime snapshot request timeout")
            .expect("connection open"),
    );
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
        let previous_revision = previous.map(|digit| serde_json::json!(format!("view-{digit}")));
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
        Some(&first.snapshot.revision)
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
                    expected_revision: cookie_agent_protocol::ViewRevision::new("view-1")
                        .expect("view revision"),
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
        expected_revision: cookie_agent_protocol::ViewRevision::new("view-1")
            .expect("view revision"),
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
            expected_revision: cookie_agent_protocol::ViewRevision::new("view-1")
                .expect("view revision"),
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
                event: std::sync::Arc::new(event(session_id, seq)),
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
                event: std::sync::Arc::new(event(session_id, seq)),
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
            if matches!(*message, crate::EventSubscriptionMessage::Rewound { through_seq: 3, .. })
    ));
    // Appends after the cut reuse its sequences and follow on from it.
    route_live(
        EventSubscriptionMessage::Event {
            event: std::sync::Arc::new(event(session_id, 4)),
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
            event: std::sync::Arc::new(event(session_id, 2)),
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
    request_json(
        tokio::time::timeout(Duration::from_secs(1), sent.recv())
            .await
            .expect("request sent")
            .expect("connection open"),
    )
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

fn request_json(frame: MessageFrame) -> Value {
    let MessageFrame::Text(text) = frame else {
        panic!("expected a text request");
    };
    serde_json::from_str(&text).expect("JSON request")
}

#[tokio::test]
async fn a_malformed_response_fails_only_the_call_it_answers() {
    let session_id = SessionId::new_v7();
    let (incoming, incoming_rx) = mpsc::unbounded_channel();
    let (sent, mut sent_rx) = mpsc::unbounded_channel();
    let client = Client::connect_stream(ScriptedStream {
        incoming: incoming_rx,
        sent,
    });
    let mut deliveries = client.subscribe_deliveries().expect("deliveries");
    let call = |client: &Client| {
        let client = client.clone();
        tokio::spawn(async move {
            client
                .get_session_goal(SessionGoalGetParams { session_id })
                .await
        })
    };
    let malformed = call(&client);
    let malformed_id = request_json(sent_rx.recv().await.expect("first request"))["id"].clone();
    let healthy = call(&client);
    let healthy_id = request_json(sent_rx.recv().await.expect("second request"))["id"].clone();

    // A response may not carry both a result and an error.
    incoming
        .send(MessageFrame::Bytes(
            serde_json::to_vec(&serde_json::json!({
                "jsonrpc": "2.0",
                "id": malformed_id,
                "result": { "goal": null },
                "error": { "code": -32603, "message": "internal error" },
            }))
            .expect("frame")
            .into(),
        ))
        .expect("malformed response");
    assert!(matches!(
        malformed.await.expect("malformed task"),
        Err(ClientError::InvalidFrame(_))
    ));

    let plugin_event = crate::ExtensionBusEventParams {
        session_id,
        context_id: None,
        plugin: "fixture".into(),
        name: "notice".into(),
        payload: serde_json::json!({ "ok": true }),
    };
    incoming
        .send(MessageFrame::Bytes(
            serde_json::to_vec(&serde_json::json!({
                "jsonrpc": "2.0",
                "method": "events.plugin",
                "params": plugin_event,
            }))
            .expect("frame")
            .into(),
        ))
        .expect("plugin notification");
    incoming
        .send(MessageFrame::Bytes(
            serde_json::to_vec(&serde_json::json!({
                "jsonrpc": "2.0",
                "id": healthy_id,
                "result": { "goal": null },
            }))
            .expect("frame")
            .into(),
        ))
        .expect("healthy response");
    assert_eq!(
        healthy
            .await
            .expect("healthy task")
            .expect("healthy call")
            .goal,
        None
    );
    let delivery = tokio::time::timeout(Duration::from_secs(1), deliveries.recv())
        .await
        .expect("delivery in time")
        .expect("delivery");
    assert!(matches!(delivery, ClientDelivery::PluginEvent(event) if event == plugin_event));
}

fn notification(method: &str, params: Value) -> MessageFrame {
    MessageFrame::Value(serde_json::json!({
        "jsonrpc": "2.0", "method": method, "params": params,
    }))
}

async fn next_delivery(deliveries: &mut mpsc::UnboundedReceiver<ClientDelivery>) -> ClientDelivery {
    tokio::time::timeout(Duration::from_secs(1), deliveries.recv())
        .await
        .expect("delivery in time")
        .expect("delivery")
}

/// A notification this build cannot read is dropped and reported; the call
/// in flight still gets its answer, and the connection carries on.
#[tokio::test]
async fn an_unreadable_notification_is_reported_and_fails_no_call() {
    let session_id = SessionId::new_v7();
    let (incoming, incoming_rx) = mpsc::unbounded_channel();
    let (sent, mut sent_rx) = mpsc::unbounded_channel();
    let client = Client::connect_stream(ScriptedStream {
        incoming: incoming_rx,
        sent,
    });
    let mut deliveries = client.subscribe_deliveries().expect("deliveries");
    let call = tokio::spawn({
        let client = client.clone();
        async move {
            client
                .get_session_goal(SessionGoalGetParams { session_id })
                .await
        }
    });
    let request = next_request(&mut sent_rx).await;
    for frame in [
        // Known types that do not decode.
        notification(
            "sessions.changed",
            serde_json::json!({"change": "upserted"}),
        ),
        notification("runtime.changed", serde_json::json!({"snapshot": 7})),
        // Open types with tags this build does not know, and a method it
        // does not know.
        notification(
            "events.subscription",
            serde_json::json!({"type": "from_the_future", "session_id": session_id}),
        ),
        notification(
            "sessions.changed",
            serde_json::json!({"change": "archived"}),
        ),
        notification("from.the_future", serde_json::json!({})),
    ] {
        incoming.send(frame).expect("notification");
    }
    // Envelope members this build does not know are ignored.
    incoming
        .send(MessageFrame::Value(serde_json::json!({
            "jsonrpc": "2.0",
            "id": request["id"],
            "result": { "goal": null, "added_later": true },
            "trace": "abc",
        })))
        .expect("response");
    assert_eq!(call.await.expect("call task").expect("call").goal, None);

    for method in ["sessions.changed", "runtime.changed"] {
        let delivery = next_delivery(&mut deliveries).await;
        assert!(
            matches!(&delivery, ClientDelivery::Diagnostic { message } if message.contains(method)),
            "{delivery:?}"
        );
    }
    // A change of a kind this build does not know reads as a resync.
    assert!(matches!(
        next_delivery(&mut deliveries).await,
        ClientDelivery::SessionsChanged(changed)
            if *changed == crate::SessionsChangedNotification::Resync
    ));
    assert!(deliveries.try_recv().is_err());
}

/// A frame that is not JSON-RPC at all ends the connection.
#[tokio::test]
async fn a_frame_that_is_not_json_rpc_closes_the_connection() {
    let session_id = SessionId::new_v7();
    let (incoming, incoming_rx) = mpsc::unbounded_channel();
    let (sent, mut sent_rx) = mpsc::unbounded_channel();
    let client = Client::connect_stream(ScriptedStream {
        incoming: incoming_rx,
        sent,
    });
    let mut deliveries = client.subscribe_deliveries().expect("deliveries");
    let call = tokio::spawn({
        let client = client.clone();
        async move {
            client
                .get_session_goal(SessionGoalGetParams { session_id })
                .await
        }
    });
    next_request(&mut sent_rx).await;
    incoming
        .send(MessageFrame::Text("not json".into()))
        .expect("frame");
    assert!(matches!(
        call.await.expect("call task"),
        Err(ClientError::Transport(_))
    ));
    assert!(matches!(
        next_delivery(&mut deliveries).await,
        ClientDelivery::Disconnected { .. }
    ));
}

/// Events a newer daemon wrote take their place in a replay and a live tail
/// as unknown events; one whose envelope cannot be read is dropped and
/// reported, and the replay still completes.
#[tokio::test]
async fn events_this_build_cannot_read_take_their_place_in_the_replay() {
    let session_id = SessionId::new_v7();
    let (incoming, incoming_rx) = mpsc::unbounded_channel();
    let (sent, mut sent_rx) = mpsc::unbounded_channel();
    let client = Client::connect_stream(ScriptedStream {
        incoming: incoming_rx,
        sent,
    });
    let mut deliveries = client.subscribe_deliveries().expect("deliveries");
    let subscribe = tokio::spawn({
        let client = client.clone();
        async move { client.subscribe_events(session_id, None).await }
    });
    let request = next_request(&mut sent_rx).await;
    let envelope = |seq: u64, payload: Value| {
        let mut event = serde_json::to_value(event(session_id, seq)).unwrap();
        event["payload"] = payload;
        event
    };
    let mut broken_envelope = serde_json::to_value(event(session_id, 5)).unwrap();
    broken_envelope
        .as_object_mut()
        .unwrap()
        .remove("session_id");
    incoming
        .send(MessageFrame::Value(serde_json::json!({
            "jsonrpc": "2.0",
            "id": request["id"],
            "result": {
                "events": [
                    event(session_id, 1),
                    envelope(2, serde_json::json!({"type": "from_the_future", "x": 1})),
                    envelope(3, serde_json::json!({"type": "run_finished", "outcome": 7})),
                    event(session_id, 4),
                    broken_envelope,
                ],
            },
        })))
        .expect("page");
    subscribe.await.expect("task").expect("subscribe");

    let delivery = next_delivery(&mut deliveries).await;
    assert!(
        matches!(&delivery, ClientDelivery::Diagnostic { message } if message.contains("seq 5")),
        "{delivery:?}"
    );
    assert!(matches!(
        next_delivery(&mut deliveries).await,
        ClientDelivery::ReplayStart { final_seq: 4, .. }
    ));
    assert!(matches!(
        next_delivery(&mut deliveries).await,
        ClientDelivery::ReplayEvent { event, .. } if event.seq == 1
    ));
    for (seq, tag) in [(2, "from_the_future"), (3, "run_finished")] {
        let delivery = next_delivery(&mut deliveries).await;
        assert!(
            matches!(&delivery, ClientDelivery::UnknownEvent { event, replay_final_seq: Some(4), .. }
                if event.seq == seq && event.tag == tag),
            "{delivery:?}"
        );
    }
    assert!(matches!(
        next_delivery(&mut deliveries).await,
        ClientDelivery::ReplayEvent { event, .. } if event.seq == 4
    ));
    assert!(matches!(
        next_delivery(&mut deliveries).await,
        ClientDelivery::ReplayEnd { final_seq: 4, .. }
    ));

    // Live, the same holds, and the cursor moves past the unknown event.
    incoming
        .send(notification(
            "events.subscription",
            serde_json::json!({
                "type": "event",
                "event": envelope(5, serde_json::json!({"type": "from_the_future"})),
            }),
        ))
        .expect("live event");
    incoming
        .send(notification(
            "events.subscription",
            serde_json::json!({"type": "event", "event": event(session_id, 6)}),
        ))
        .expect("live event");
    assert!(matches!(
        next_delivery(&mut deliveries).await,
        ClientDelivery::UnknownEvent { event, replay_final_seq: None, .. } if event.seq == 5
    ));
    assert!(matches!(
        next_delivery(&mut deliveries).await,
        ClientDelivery::Live { message, .. }
            if matches!(message.as_ref(), crate::EventSubscriptionMessage::Event { event } if event.seq == 6)
    ));
}

/// A result with an outcome this build does not know fails that call with
/// a readable error, and so does a method the daemon does not know.
#[tokio::test]
async fn unknown_outcomes_and_methods_fail_only_their_call() {
    let session_id = SessionId::new_v7();
    let (incoming, incoming_rx) = mpsc::unbounded_channel();
    let (sent, mut sent_rx) = mpsc::unbounded_channel();
    let client = Client::connect_stream(ScriptedStream {
        incoming: incoming_rx,
        sent,
    });
    let recall = tokio::spawn({
        let client = client.clone();
        async move {
            client
                .recall_input(crate::SessionRecallInputParams { session_id, seq: 3 })
                .await
        }
    });
    let request = next_request(&mut sent_rx).await;
    incoming
        .send(MessageFrame::Value(serde_json::json!({
            "jsonrpc": "2.0", "id": request["id"], "result": {"outcome": "archived"},
        })))
        .expect("response");
    let error = recall.await.expect("task").expect_err("unknown outcome");
    assert!(
        matches!(&error, ClientError::UnknownOutcome { method, outcome }
            if method == "session.recall_input" && outcome == "archived"),
        "{error:?}"
    );

    let list = tokio::spawn({
        let client = client.clone();
        async move { client.list_plugins().await }
    });
    let request = next_request(&mut sent_rx).await;
    incoming
        .send(MessageFrame::Value(serde_json::json!({
            "jsonrpc": "2.0", "id": request["id"],
            "error": {"code": -32601, "message": "The other side does not know the method plugins.list.",
                      "data": {"code": "method_not_found", "method": "plugins.list"}},
        })))
        .expect("response");
    let error = list.await.expect("task").expect_err("unknown method");
    assert_eq!(error.to_string(), "the daemon is too old for plugins.list");
}

#[test]
fn a_replay_page_drops_an_event_it_cannot_read_and_resumes_after_it() {
    let session_id = SessionId::new_v7();
    let good = serde_json::to_value(event(session_id, 1)).expect("event");
    let mut unknown = serde_json::to_value(event(session_id, 2)).expect("event");
    unknown["payload"] = serde_json::json!({"type": "from_a_newer_build", "field": 1});
    let mut broken = serde_json::to_value(event(session_id, 3)).expect("event");
    broken
        .as_object_mut()
        .expect("an object")
        .remove("session_id");
    let page = serde_json::json!({"events": [good, unknown, broken], "has_more": true});
    let page: ReplayPageWire = serde_json::from_str(&page.to_string()).expect("page");
    let (deliveries, mut delivered) = delivery_channel();
    let read = page.read(session_id, &deliveries);
    let seqs = read
        .events
        .iter()
        .map(|event| event.seq)
        .collect::<Vec<_>>();
    assert_eq!(seqs, [1, 2]);
    assert!(matches!(
        &read.events[1].payload,
        crate::Open::Unknown { tag, .. } if tag == "from_a_newer_build"
    ));
    // The next page starts after the dropped event.
    assert_eq!(read.last_seq, Some(3));
    let Ok(ClientDelivery::Diagnostic { message }) = delivered.try_recv() else {
        panic!("no diagnostic");
    };
    assert!(message.contains("(seq 3)"), "{message}");
}
