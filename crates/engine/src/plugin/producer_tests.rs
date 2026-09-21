use super::*;
use cookie_agent_protocol::{ProducerId, ProducerMessageId, SessionId};
use serde_json::json;

fn registry(enabled: bool, producers: bool, script: &str) -> (tempfile::TempDir, PluginRegistry) {
    let directory = tempfile::tempdir().unwrap();
    let mcp = Arc::new(
        crate::McpRegistry::new(Default::default(), directory.path().join("oauth.json")).unwrap(),
    );
    let config = serde_json::from_value(json!({
            "command": if cfg!(windows) { "python" } else { "python3" },
            "args": ["-c", script], "enabled": enabled, "producer_messaging": producers,
            "env": std::env::vars().filter(|(name, _)| ["PATH", "SYSTEMROOT", "WINDIR"].contains(&name.as_str())).collect::<std::collections::BTreeMap<_, _>>(),
        })).unwrap();
    (
        directory,
        PluginRegistry::new(IndexMap::from([("owner".into(), config)]), mcp),
    )
}

fn activate(registry: &PluginRegistry, epoch: u64) -> Arc<PluginRuntime> {
    let runtime = Arc::clone(&registry.inner.plugins["owner"]);
    *runtime.producer_connection.lock().unwrap() = ProducerConnection {
        epoch: Some(epoch),
        live: true,
        recovery: Some(PluginRecoveryStatus::Starting),
        outcome: None,
    };
    runtime
}

fn request(method: &str, params: Value) -> Request {
    Request::new(
        JsonRpcId::String("originating-id".into()),
        method,
        Some(params),
    )
}

#[test]
fn producer_methods_decode_strictly_and_serialize_only_matching_inner_results() {
    let session = SessionId::new_v7();
    let producer = ProducerId::new_v7();
    let message = ProducerMessageId::new_v7();
    let cases = [
        (
            PLUGIN_PRODUCER_REGISTER_METHOD,
            json!({"session_id": session}),
            PluginProducerResponse::Register(ExtensionProducerRegisterResult {
                producer_id: producer,
            }),
            json!({"producer_id": producer}),
        ),
        (
            PLUGIN_PRODUCER_SEND_METHOD,
            json!({"session_id": session, "producer_id": producer, "description": "Greeting", "body": "hello", "mode": "queue", "idempotency_key": "key"}),
            PluginProducerResponse::Send(ExtensionProducerSendResult {
                message_id: message,
            }),
            json!({"message_id": message}),
        ),
        (
            PLUGIN_PRODUCER_UNREGISTER_METHOD,
            json!({"session_id": session, "producer_id": producer}),
            PluginProducerResponse::Unregister(ExtensionProducerUnregisterResult {}),
            json!({}),
        ),
        (
            PLUGIN_RECOVERY_COMPLETE_METHOD,
            json!({"outcome": {"status": "ready"}}),
            PluginProducerResponse::RecoveryComplete(ExtensionRecoveryCompleteResult {}),
            json!({}),
        ),
        (
            PLUGIN_PRODUCER_DISCARD_METHOD,
            json!({"session_id": session, "message_id": message}),
            PluginProducerResponse::Discard(ExtensionProducerDiscardResult {}),
            json!({}),
        ),
    ];
    for (method, params, response, expected) in cases {
        assert!(decode_producer_request(method, params.clone()).is_ok());
        let mut foreign = params;
        foreign["plugin"] = json!("foreign");
        assert_eq!(
            decode_producer_request(method, foreign).unwrap_err().code,
            -32602
        );
        let result = producer_result(method, response).unwrap();
        let wire = serde_json::to_value(producer_response(
            JsonRpcId::String("same-id".into()),
            Ok(result),
        ))
        .unwrap();
        assert_eq!(wire["id"], "same-id");
        assert_eq!(wire["result"], expected);
    }
    assert_eq!(
        decode_producer_request("producer.register", json!({}))
            .unwrap_err()
            .code,
        -32601
    );
    assert!(
        producer_result(
            PLUGIN_PRODUCER_SEND_METHOD,
            PluginProducerResponse::Unregister(ExtensionProducerUnregisterResult {})
        )
        .is_err()
    );
}

#[tokio::test]
async fn producer_capability_absent_handler_and_overload_fail_explicitly() {
    let (_directory, registry) = registry(true, false, "");
    assert!(registry.producer_recovery_states().is_empty());
    let runtime = &registry.inner.plugins["owner"];
    let mut tasks = JoinSet::new();
    let register = || {
        request(
            PLUGIN_PRODUCER_REGISTER_METHOD,
            json!({"session_id": SessionId::new_v7()}),
        )
    };
    assert!(matches!(
        runtime.dispatch_producer_request(register(), &mut tasks),
        Some(Response::Error(_))
    ));
    let runtime = activate(&registry, 7);
    assert!(matches!(
        runtime.dispatch_producer_request(register(), &mut tasks),
        Some(Response::Error(_))
    ));
    registry.set_producer_handler(Arc::new(|_, _| Box::pin(std::future::pending())));
    let (sender, _receiver) = mpsc::channel(1);
    *runtime.control.lock().unwrap() = Some(sender);
    for _ in 0..PLUGIN_PRODUCER_CONCURRENCY {
        assert!(
            runtime
                .dispatch_producer_request(register(), &mut tasks)
                .is_none()
        );
    }
    let response = runtime
        .dispatch_producer_request(register(), &mut tasks)
        .unwrap();
    let wire = serde_json::to_value(response).unwrap();
    assert_eq!(wire["id"], "originating-id");
    assert_eq!(wire["error"]["code"], -32000);
}

#[test]
fn producer_discard_wire_excludes_registration_and_caller_authority() {
    let params =
        json!({"session_id": SessionId::new_v7(), "message_id": ProducerMessageId::new_v7()});
    assert!(decode_producer_request("plugin/producer/discard", params.clone()).is_ok());
    for (field, value) in [
        ("producer_id", json!(ProducerId::new_v7())),
        ("plugin", json!("foreign")),
        ("connection_epoch", json!(17)),
    ] {
        let mut foreign = params.clone();
        foreign[field] = value;
        assert_eq!(
            decode_producer_request("plugin/producer/discard", foreign)
                .unwrap_err()
                .code,
            -32602
        );
    }
}

#[tokio::test]
async fn producer_discard_routes_current_authority_without_a_registration() {
    let (_directory, registry) = registry(true, true, "");
    let (control, mut receive) = mpsc::channel(1);
    let runtime = &registry.inner.plugins["owner"];
    *runtime.control.lock().unwrap() = Some(control);
    let authorities = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&authorities);
    registry.set_producer_handler(Arc::new(move |authority, request| {
        assert!(matches!(request, PluginProducerRequest::Discard(_)));
        recorded.lock().unwrap().push(authority);
        Box::pin(async {
            Err(producer_error(
                -32000,
                "message already consumed or in flight",
            ))
        })
    }));
    let mut tasks = JoinSet::new();
    let params =
        json!({"session_id": SessionId::new_v7(), "message_id": ProducerMessageId::new_v7()});
    assert!(matches!(
        runtime.dispatch_producer_request(
            request("plugin/producer/discard", params.clone()),
            &mut tasks
        ),
        Some(Response::Error(_))
    ));
    for epoch in [31, 32] {
        let runtime = activate(&registry, epoch);
        assert!(
            runtime
                .dispatch_producer_request(
                    request("plugin/producer/discard", params.clone()),
                    &mut tasks
                )
                .is_none()
        );
        let Control::ReplyFrame(Response::Error(response)) = receive.recv().await.unwrap() else {
            panic!("runtime rejection must be returned")
        };
        assert_eq!(response.id, JsonRpcId::String("originating-id".into()));
        assert_eq!(
            response.error.message,
            "message already consumed or in flight"
        );
        tasks.join_next().await.unwrap().unwrap();
        drop(ProducerConnectionLease(runtime, epoch));
    }
    assert_eq!(
        *authorities.lock().unwrap(),
        vec![
            PluginConnectionAuthority {
                plugin: "owner".into(),
                connection_epoch: 31
            },
            PluginConnectionAuthority {
                plugin: "owner".into(),
                connection_epoch: 32
            },
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn producer_readiness_is_epoch_bound_idempotent_and_watched_without_deadline() {
    let (_directory, registry) = registry(true, true, "");
    let mut changes = registry.subscribe_producer_changes();
    let runtime = activate(&registry, 9);
    let authority = PluginConnectionAuthority {
        plugin: "owner".into(),
        connection_epoch: 9,
    };
    assert!(registry.producer_connection_is_current("owner", &9));
    assert!(!registry.producer_connection_is_current("foreign", &9));
    assert!(!registry.producer_connection_is_current("owner", &8));
    tokio::time::advance(Duration::from_secs(86_400)).await;
    assert_eq!(
        registry.producer_recovery_states()[0].status,
        PluginRecoveryStatus::Starting
    );
    registry
        .complete_producer_recovery(&authority, &ExtensionRecoveryOutcome::Ready)
        .unwrap();
    changes.changed().await.unwrap();
    registry
        .complete_producer_recovery(&authority, &ExtensionRecoveryOutcome::Ready)
        .unwrap();
    assert!(!changes.has_changed().unwrap());
    tokio::time::advance(Duration::from_secs(86_400)).await;
    assert_eq!(
        registry.producer_recovery_states()[0].status,
        PluginRecoveryStatus::Ready
    );
    let failed = ExtensionRecoveryOutcome::Failed {
        message: cookie_agent_protocol::SafeErrorMessage::new("lost work").unwrap(),
    };
    assert!(
        registry
            .complete_producer_recovery(&authority, &failed)
            .is_err()
    );
    drop(ProducerConnectionLease(runtime, 9));
    changes.changed().await.unwrap();
    assert!(!registry.producer_connection_is_current("owner", &9));
    assert_eq!(
        registry.producer_recovery_states()[0].status,
        PluginRecoveryStatus::Failed
    );
    assert!(
        registry
            .complete_producer_recovery(&authority, &ExtensionRecoveryOutcome::Ready)
            .is_err()
    );
    let replacement = activate(&registry, 10);
    drop(ProducerConnectionLease(replacement, 9));
    assert!(registry.producer_connection_is_current("owner", &10));
    registry
        .complete_producer_recovery(
            &PluginConnectionAuthority {
                plugin: "owner".into(),
                connection_epoch: 10,
            },
            &failed,
        )
        .unwrap();
    assert_eq!(
        registry.producer_recovery_states()[0].status,
        PluginRecoveryStatus::Failed
    );
    assert_eq!(registry.statuses()[0].reason.as_deref(), Some("lost work"));
    assert!(
        registry
            .complete_producer_recovery(&authority, &ExtensionRecoveryOutcome::Ready)
            .is_err()
    );
    let (_directory, disabled) = self::registry(false, true, "");
    assert_eq!(
        disabled.producer_recovery_states()[0].status,
        PluginRecoveryStatus::Disabled
    );
}

#[tokio::test]
async fn producer_handshake_requires_both_configuration_and_plugin_capability() {
    let script = r#"
import json, sys
def send(value):
    print(json.dumps(value), flush=True)
rejected = False
ping = None
for line in sys.stdin:
    frame = json.loads(line)
    method = frame.get('method')
    if method == 'plugin/initialize':
        send({'jsonrpc':'2.0','id':frame['id'],'result':{
            'protocol_version':frame['params']['protocol_version'],'name':'owner','version':'1',
            'capabilities':{'producer_messaging':capable,'tools':False,'resources':False,'subscribe_events':False,'subscribe_bus':False,'publish_bus':False,'publish_session_events':False,'intercept':[]},'tools':[]}})
        send({'jsonrpc':'2.0','id':'register','method':'plugin/producer/register','params':{'session_id':'01900000-0000-7000-8000-000000000001'}})
    elif frame.get('id') == 'register':
        assert 'error' in frame
        rejected = True
    elif method == 'plugin/ping':
        ping = frame['id']
    elif method == 'plugin/shutdown':
        break
    else:
        raise AssertionError(frame)
    if rejected and ping is not None:
        send({'jsonrpc':'2.0','id':ping,'result':{}})
        ping = None
"#;
    for (configured, capable) in [(false, "True"), (true, "False")] {
        let (_directory, registry) =
            registry(true, configured, &format!("capable = {capable}\n{script}"));
        registry.set_producer_handler(Arc::new(|_, _| panic!("disabled producer dispatched")));
        registry.start_eager(&tokio::runtime::Handle::current());
        tokio::time::timeout(Duration::from_secs(5), registry.await_eager_ready())
            .await
            .unwrap();
        registry.ping("owner").await.unwrap();
        assert_eq!(
            registry.producer_recovery_states()[0].status,
            PluginRecoveryStatus::Disabled
        );
        registry.shutdown().await;
    }
}

#[tokio::test]
async fn producer_handler_replies_wait_for_control_capacity_and_recheck_epoch() {
    let (_directory, registry) = registry(true, true, "");
    let runtime = activate(&registry, 4);
    let (control, mut receive) = mpsc::channel(1);
    *runtime.control.lock().unwrap() = Some(control.clone());
    control
        .send(Control::ReplyFrame(producer_response(
            JsonRpcId::Number(0),
            Ok(json!({})),
        )))
        .await
        .unwrap();
    let release = Arc::new(Notify::new());
    let wait = Arc::clone(&release);
    registry.set_producer_handler(Arc::new(move |authority, request| {
        assert_eq!(authority.plugin, "owner");
        assert_eq!(authority.connection_epoch, 4);
        assert!(matches!(request, PluginProducerRequest::Register(_)));
        let wait = Arc::clone(&wait);
        Box::pin(async move {
            wait.notified().await;
            Ok(PluginProducerResponse::Register(
                ExtensionProducerRegisterResult {
                    producer_id: ProducerId::new_v7(),
                },
            ))
        })
    }));
    let mut tasks = JoinSet::new();
    assert!(
        runtime
            .dispatch_producer_request(
                request(
                    PLUGIN_PRODUCER_REGISTER_METHOD,
                    json!({"session_id": SessionId::new_v7()})
                ),
                &mut tasks
            )
            .is_none()
    );
    tokio::task::yield_now().await;
    drop(ProducerConnectionLease(Arc::clone(&runtime), 4));
    release.notify_one();
    tokio::task::yield_now().await;
    assert!(!tasks.is_empty());
    receive.recv().await.unwrap();
    let Control::ReplyFrame(Response::Error(response)) = receive.recv().await.unwrap() else {
        panic!("stale request must reject")
    };
    assert_eq!(response.id, JsonRpcId::String("originating-id".into()));
    tasks.join_next().await.unwrap().unwrap();
}

#[tokio::test]
async fn producer_recovery_register_and_handler_notification_before_reply_do_not_deadlock() {
    // This fixture requires a host notification and ping to be serviced while
    // its register request is still awaiting the async runtime handler.
    let script = r#"
import json, sys
def send(value):
    print(json.dumps(value), flush=True)
session = '01900000-0000-7000-8000-000000000001'
notice = False
for line in sys.stdin:
    frame = json.loads(line)
    method = frame.get('method')
    if method == 'plugin/initialize':
        assert frame['params']['capabilities']['producer_messaging']
        send({'jsonrpc':'2.0','id':frame['id'],'result':{
            'protocol_version':frame['params']['protocol_version'],'name':'owner','version':'1',
            'capabilities':{'producer_messaging':True,'tools':False,'resources':False,'subscribe_events':False,'subscribe_bus':True,'publish_bus':False,'publish_session_events':False,'intercept':[]},'tools':[]}})
    elif method == 'plugin/recovery/start':
        assert 'id' not in frame and frame['params'] == {}
        send({'jsonrpc':'2.0','id':'register','method':'plugin/producer/register','params':{'session_id':session}})
    elif method == 'plugin/bus_event':
        notice = True
    elif method == 'plugin/ping':
        assert notice
        send({'jsonrpc':'2.0','id':frame['id'],'result':{}})
    elif frame.get('id') == 'register':
        assert notice and set(frame['result']) == {'producer_id'}
        send({'jsonrpc':'2.0','id':'complete','method':'plugin/recovery/complete','params':{'outcome':{'status':'ready'}}})
    elif frame.get('id') == 'complete':
        assert frame['result'] == {}
    elif method == 'plugin/shutdown':
        break
"#;
    let (_directory, registry) = registry(true, true, script);
    let mut changes = registry.subscribe_producer_changes();
    let weak = Arc::downgrade(&registry.inner);
    registry.set_producer_handler(Arc::new(move |authority, request| {
        let registry = PluginRegistry {
            inner: weak.upgrade().unwrap(),
        };
        Box::pin(async move {
            assert!(
                registry
                    .producer_connection_is_current(&authority.plugin, &authority.connection_epoch)
            );
            match request {
                PluginProducerRequest::Register(params) => {
                    assert_eq!(
                        registry.producer_recovery_states()[0].status,
                        PluginRecoveryStatus::Starting
                    );
                    let control = registry.inner.plugins["owner"]
                        .control
                        .lock()
                        .unwrap()
                        .clone()
                        .unwrap();
                    control
                        .send(Control::Notify {
                            notification: Notification::new(
                                PLUGIN_BUS_EVENT_METHOD,
                                Some(json!({})),
                            ),
                            session_id: params.session_id,
                            context_id: plugin_context_id(),
                            context_lifetime: Duration::from_secs(5),
                        })
                        .await
                        .unwrap();
                    registry.ping("owner").await.unwrap();
                    Ok(PluginProducerResponse::Register(
                        ExtensionProducerRegisterResult {
                            producer_id: ProducerId::new_v7(),
                        },
                    ))
                }
                PluginProducerRequest::RecoveryComplete(params) => {
                    registry.complete_producer_recovery(&authority, &params.outcome)?;
                    Ok(PluginProducerResponse::RecoveryComplete(
                        ExtensionRecoveryCompleteResult {},
                    ))
                }
                _ => panic!("unexpected request"),
            }
        })
    }));
    registry.start_eager(&tokio::runtime::Handle::current());
    tokio::time::timeout(Duration::from_secs(5), async {
        while registry.producer_recovery_states()[0].status != PluginRecoveryStatus::Ready {
            changes.changed().await.unwrap();
            assert_ne!(
                registry.producer_recovery_states()[0].status,
                PluginRecoveryStatus::Failed
            );
        }
    })
    .await
    .expect("recovery must remain serviceable");
    registry.ping("owner").await.unwrap();
    registry.shutdown().await;
    assert_eq!(
        registry.producer_recovery_states()[0].status,
        PluginRecoveryStatus::Failed
    );
}
