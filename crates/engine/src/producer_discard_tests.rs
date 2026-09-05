use super::*;

use cookie_agent_protocol::{
    EventOrigin, GoalId, PluginRecoveryStatus, ProducerDeliveryMode, ProducerIdempotencyKey,
    ProducerMessageId, ProducerOwner, SessionProducersParams,
};

use crate::runtime::producers::ProducerAuthority;

const DISCARD_CALLBACK_PLUGIN: &str = r#"
import json, os, sys

def send(value):
    print(json.dumps(value, separators=(',', ':')), flush=True)

session = os.environ['SESSION_ID']
receipt_file = os.environ['RECEIPT_FILE']
producer_id = None
message_id = None
started = False
first_discarded = False

for line in sys.stdin:
    frame = json.loads(line)
    method = frame.get('method')
    request_id = frame.get('id')
    if method == 'plugin/initialize':
        assert frame['params']['protocol_version'] == '0.0.5'
        assert frame['params']['capabilities']['producer_messaging'] is True
        send({'jsonrpc':'2.0','id':request_id,'result':{
            'protocol_version':'0.0.5','name':'discard_callback','version':'1',
            'capabilities':{'producer_messaging':True,'tools':False,'resources':False,
                'subscribe_events':True,'subscribe_bus':False,'publish_bus':False,
                'publish_session_events':False,'intercept':[]},'tools':[]}})
    elif method == 'plugin/recovery/start':
        assert 'id' not in frame and frame['params'] == {}
        send({'jsonrpc':'2.0','id':'register','method':'plugin/producer/register',
            'params':{'session_id':session}})
    elif request_id == 'register':
        producer_id = frame['result']['producer_id']
    elif method == 'plugin/event':
        params = frame['params']
        if (not started and producer_id is not None and params['session_id'] == session
                and params['event']['type'] == 'model_request_prepared'):
            started = True
            send({'jsonrpc':'2.0','id':'send','method':'plugin/producer/send','params':{
                'session_id':session,'producer_id':producer_id,'mode':'queue',
                'idempotency_key':'discard-after-unregister',
                'body':'plugin callback body must never reach the model'}})
    elif request_id == 'send':
        message_id = frame['result']['message_id']
        send({'jsonrpc':'2.0','id':'unregister','method':'plugin/producer/unregister',
            'params':{'session_id':session,'producer_id':producer_id}})
    elif request_id == 'unregister':
        assert frame['result'] == {}
        send({'jsonrpc':'2.0','id':'discard-first','method':'plugin/producer/discard',
            'params':{'session_id':session,'message_id':message_id}})
    elif request_id == 'discard-first':
        assert frame['result'] == {}
        first_discarded = True
        send({'jsonrpc':'2.0','id':'discard-repeat','method':'plugin/producer/discard',
            'params':{'session_id':session,'message_id':message_id}})
    elif request_id == 'discard-repeat':
        assert frame['result'] == {} and first_discarded
        send({'jsonrpc':'2.0','id':'complete','method':'plugin/recovery/complete',
            'params':{'outcome':{'status':'ready'}}})
    elif request_id == 'complete':
        assert frame['result'] == {}
        with open(receipt_file, 'w', encoding='utf-8') as output:
            json.dump({'message_id':message_id,'unregistered':True,
                'first_discard':True,'repeat_discard':True,'ready':True}, output)
    elif method == 'plugin/ping':
        send({'jsonrpc':'2.0','id':request_id,'result':{}})
    elif method == 'plugin/shutdown':
        break
"#;

fn authority(owner: ProducerOwner) -> ProducerAuthority {
    ProducerAuthority {
        owner,
        connection_epoch: None,
    }
}

fn delegation_authority() -> ProducerAuthority {
    authority(ProducerOwner::Delegation {
        invocation_id: InvocationId::new_v7(),
    })
}

fn discard_key(value: &str) -> ProducerIdempotencyKey {
    ProducerIdempotencyKey::new(value).expect("producer idempotency key")
}

fn discard_origin() -> EventOrigin {
    EventOrigin::new("client:producer-discard-test").expect("discard test origin")
}

fn discard_events(
    engine: &Engine,
    session_id: SessionId,
) -> Vec<cookie_agent_protocol::StoredEvent> {
    engine
        .inner
        .store
        .get(session_id)
        .expect("producer discard session")
        .log
        .events()
}

fn discard_projection(
    engine: &Engine,
    session_id: SessionId,
) -> crate::goal_projection::GoalProducerProjection {
    crate::goal_projection::GoalProducerProjection::from_events(&discard_events(engine, session_id))
}

fn append_accepted_message(
    engine: &Engine,
    session_id: SessionId,
    message_id: ProducerMessageId,
    owner: ProducerOwner,
    body: &str,
) {
    engine
        .append_direct(
            session_id,
            None,
            discard_origin(),
            EventPayload::ProducerMessageAccepted {
                message_id,
                producer_owner: owner,
                mode: ProducerDeliveryMode::Steer,
                idempotency_key: discard_key(&format!("direct-{message_id}")),
                body: body.into(),
                reminder: None,
            },
        )
        .expect("append trusted producer acceptance");
}

async fn held_first_request_server(
    serve_second: bool,
) -> (
    String,
    tokio::sync::oneshot::Receiver<()>,
    Option<tokio::sync::oneshot::Receiver<()>>,
    Arc<tokio::sync::Notify>,
    tokio::task::JoinHandle<Vec<String>>,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("producer discard listener");
    let address = listener.local_addr().expect("producer discard address");
    let (first_seen_tx, first_seen_rx) = tokio::sync::oneshot::channel();
    let (second_seen_tx, second_seen_rx) = tokio::sync::oneshot::channel();
    let release = Arc::new(tokio::sync::Notify::new());
    let task_release = Arc::clone(&release);
    let captured = tokio::spawn(async move {
        let (mut first, first_request) =
            accept_scripted_planned_request(&listener, "producer discard first request").await;
        let requests = vec![String::from_utf8(first_request).expect("first request UTF-8")];
        let _ = first_seen_tx.send(());
        task_release.notified().await;
        write_scripted_sse(
            &mut first,
            &scripted_text_body("first producer discard response"),
        )
        .await;

        if !serve_second {
            return requests;
        }
        let mut requests = requests;
        let (mut second, second_request) =
            accept_scripted_planned_request(&listener, "producer discard second request").await;
        let _ = second_seen_tx.send(());
        requests.push(String::from_utf8(second_request).expect("second request UTF-8"));
        write_scripted_sse(
            &mut second,
            &scripted_text_body("second producer discard response"),
        )
        .await;
        requests
    });
    (
        format!("http://{address}/v1"),
        first_seen_rx,
        serve_second.then_some(second_seen_rx),
        release,
        captured,
    )
}

#[tokio::test]
async fn queued_message_can_be_discarded_after_unregister_by_its_stable_owner() {
    let (endpoint, first_seen, _, release, captured) = held_first_request_server(false).await;
    let (fixture, selection) = custom_fixture_with_endpoint(&endpoint);
    let session = fixture
        .engine
        .create_session(selection.clone())
        .expect("queued discard session");
    let session_id = session.session_id;
    fixture
        .engine
        .start_run(
            RunStartParams {
                session_id,
                client_run_id: ClientRunId::new("producer-discard-queued").unwrap(),
                selection,
                input: "hold the first user request".into(),
            },
            discard_origin(),
        )
        .await
        .expect("start held user run");
    first_seen.await.expect("first request reached server");

    let owner = delegation_authority();
    let registration = fixture
        .engine
        .register_producer(session_id, owner.clone())
        .await
        .expect("register queued producer");
    let message_id = fixture
        .engine
        .send_producer_message(
            session_id,
            owner.clone(),
            registration,
            ProducerDeliveryMode::Queue,
            discard_key("queued-after-snapshot"),
            "discarded queued body".into(),
        )
        .await
        .expect("accept queued producer message");
    fixture
        .engine
        .unregister_producer(session_id, owner.clone(), registration)
        .await
        .expect("unregister queued producer");

    fixture
        .engine
        .discard_producer_message(session_id, owner.clone(), message_id)
        .await
        .expect("owner discards without registration");
    fixture
        .engine
        .discard_producer_message(session_id, owner.clone(), message_id)
        .await
        .expect("repeated owner discard is idempotent");
    let foreign = delegation_authority();
    assert!(
        fixture
            .engine
            .discard_producer_message(session_id, foreign, message_id)
            .await
            .expect_err("foreign discard")
            .to_string()
            .contains("another producer owner")
    );

    let discarded = discard_events(&fixture.engine, session_id)
        .into_iter()
        .filter(|event| {
            matches!(
                &event.payload,
                EventPayload::ProducerMessageDiscarded {
                    message_id: discarded,
                    reminder: None,
                    producer_owner: Some(discard_owner),
                } if *discarded == message_id && discard_owner == &owner.owner
            )
        })
        .count();
    assert_eq!(discarded, 1);

    release.notify_one();
    wait_for_session_not_running(&fixture.engine, session_id).await;
    let requests = captured.await.expect("queued discard requests");
    assert_eq!(requests.len(), 1);
    assert!(!requests[0].contains("discarded queued body"));
    let record = discard_projection(&fixture.engine, session_id)
        .messages
        .into_iter()
        .find(|message| message.message_id == message_id)
        .expect("discarded queued record");
    assert!(record.discarded);
    assert!(!record.consumed);
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn admitted_message_added_after_request_snapshot_can_still_be_discarded() {
    let (endpoint, first_seen, _, release, captured) = held_first_request_server(false).await;
    let (fixture, selection) = custom_fixture_with_endpoint(&endpoint);
    let session = fixture
        .engine
        .create_session(selection.clone())
        .expect("admitted discard session");
    let session_id = session.session_id;
    let run = fixture
        .engine
        .start_run(
            RunStartParams {
                session_id,
                client_run_id: ClientRunId::new("producer-discard-admitted").unwrap(),
                selection,
                input: "snapshot before trusted admission".into(),
            },
            discard_origin(),
        )
        .await
        .expect("start admitted discard run")
        .run_id;
    first_seen.await.expect("first request reached server");

    let owner = delegation_authority();
    let message_id = ProducerMessageId::new_v7();
    append_accepted_message(
        &fixture.engine,
        session_id,
        message_id,
        owner.owner.clone(),
        "late admitted discarded body",
    );
    fixture
        .engine
        .append_direct(
            session_id,
            Some(run),
            discard_origin(),
            EventPayload::ProducerMessageAdmitted { message_id },
        )
        .expect("append trusted producer admission");
    fixture
        .engine
        .discard_producer_message(session_id, owner, message_id)
        .await
        .expect("discard unclaimed admitted message");

    release.notify_one();
    wait_for_session_not_running(&fixture.engine, session_id).await;
    let requests = captured.await.expect("admitted discard requests");
    assert_eq!(requests.len(), 1);
    assert!(!requests[0].contains("late admitted discarded body"));
    assert!(!discard_events(&fixture.engine, session_id).iter().any(|event| {
        matches!(event.payload, EventPayload::ProducerMessageConsumed { message_id: consumed, .. } if consumed == message_id)
    }));
    let record = discard_projection(&fixture.engine, session_id)
        .messages
        .into_iter()
        .find(|message| message.message_id == message_id)
        .expect("admitted discard record");
    assert!(record.discarded);
    assert!(!record.consumed);
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn inflight_steer_is_too_late_once_claimed_and_after_commit() {
    let (endpoint, first_seen, second_seen, release_first, captured) =
        held_first_request_server(true).await;
    let mut second_seen = second_seen.expect("second request signal");
    let (fixture, selection) = custom_fixture_with_endpoint(&endpoint);
    let session = fixture
        .engine
        .create_session(selection.clone())
        .expect("claimed steer session");
    let session_id = session.session_id;
    let run = fixture
        .engine
        .start_run(
            RunStartParams {
                session_id,
                client_run_id: ClientRunId::new("producer-discard-claimed").unwrap(),
                selection,
                input: "hold before claimed steer".into(),
            },
            discard_origin(),
        )
        .await
        .expect("start claimed steer run")
        .run_id;
    first_seen.await.expect("first request reached server");

    let owner = delegation_authority();
    let registration = fixture
        .engine
        .register_producer(session_id, owner.clone())
        .await
        .expect("register steer producer");
    let message_id = fixture
        .engine
        .send_producer_message(
            session_id,
            owner.clone(),
            registration,
            ProducerDeliveryMode::Steer,
            discard_key("claimed-steer"),
            "claimed steer body".into(),
        )
        .await
        .expect("accept steer message");
    let (snapshot_reached, release_snapshot) =
        fixture.engine.install_prompt_snapshot_hook_for_test();
    release_first.notify_one();
    tokio::time::timeout(test_timeout(3), snapshot_reached)
        .await
        .expect("claimed snapshot hook timeout")
        .expect("claimed snapshot hook reached");

    let claim = discard_events(&fixture.engine, session_id)
        .into_iter()
        .find(|event| {
            event.run_id == Some(run)
                && matches!(
                    &event.payload,
                    EventPayload::ProducerMessagesClaimed { message_ids }
                        if message_ids == &[message_id]
                )
        })
        .expect("run-scoped producer claim");
    assert!(
        fixture
            .engine
            .discard_producer_message(session_id, owner.clone(), message_id)
            .await
            .expect_err("claimed discard")
            .to_string()
            .contains("too late")
    );
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), &mut second_seen)
            .await
            .is_err(),
        "model request reached the wire while the post-claim hook was held"
    );

    release_snapshot.notify_one();
    tokio::time::timeout(test_timeout(3), &mut second_seen)
        .await
        .expect("second request timeout")
        .expect("second request reached server");
    await_event(&fixture.engine, session_id, "claimed steer consumed", |event| {
        matches!(event.payload, EventPayload::ProducerMessageConsumed { message_id: consumed, run_id } if consumed == message_id && run_id == run)
    })
    .await;
    assert!(
        fixture
            .engine
            .discard_producer_message(session_id, owner, message_id)
            .await
            .expect_err("consumed discard")
            .to_string()
            .contains("too late")
    );
    let released = await_event(&fixture.engine, session_id, "claim released after commit", |event| {
            event.run_id == Some(run)
                && matches!(event.payload, EventPayload::ProducerMessagesReleased { claim_seq } if claim_seq == claim.seq)
        }).await;
    assert!(released.seq > claim.seq);

    let requests = captured.await.expect("claimed steer requests");
    assert_eq!(requests.len(), 2);
    assert!(requests[1].contains("claimed steer body"));
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn cancelling_a_claimed_request_releases_only_that_request_lease() {
    let (endpoint, first_seen, release_first, captured) =
        super::producer_runtime_tests::cancelled_request_boundary_server().await;
    let (fixture, selection) = custom_fixture_with_endpoint(&endpoint);
    let session_id = fixture.engine.create_session(selection).unwrap().session_id;
    let owner = delegation_authority();
    let registration = fixture
        .engine
        .register_producer(session_id, owner.clone())
        .await
        .unwrap();
    let message_id = fixture
        .engine
        .send_producer_message(
            session_id,
            owner.clone(),
            registration,
            ProducerDeliveryMode::Steer,
            discard_key("cancelled-claim"),
            "cancelled attempt input".into(),
        )
        .await
        .unwrap();
    first_seen.await.unwrap();
    let claim = discard_events(&fixture.engine, session_id).into_iter().find(|event| {
        matches!(&event.payload, EventPayload::ProducerMessagesClaimed { message_ids } if message_ids.contains(&message_id))
    }).unwrap();
    fixture
        .engine
        .unregister_producer(session_id, owner, registration)
        .await
        .unwrap();
    fixture
        .engine
        .cancel_run(claim.run_id.unwrap())
        .await
        .unwrap();
    await_event(&fixture.engine, session_id, "cancelled request claim released", |event| {
        matches!(event.payload, EventPayload::ProducerMessagesReleased { claim_seq } if claim_seq == claim.seq)
    }).await;
    assert!(
        !discard_projection(&fixture.engine, session_id)
            .claims
            .contains_key(&claim.seq)
    );
    release_first.notify_one();
    let consumed = await_event(&fixture.engine, session_id, "retried message committed", |event| {
        matches!(event.payload, EventPayload::ProducerMessageConsumed { message_id: id, .. } if id == message_id)
    }).await;
    assert_ne!(consumed.run_id, claim.run_id);
    wait_for_session_not_running(&fixture.engine, session_id).await;
    let requests = captured.await.unwrap();
    assert_eq!(requests.len(), 2);
    assert!(
        requests
            .iter()
            .all(|request| request.contains("cancelled attempt input"))
    );
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn failed_request_releases_claim_so_owner_can_discard_before_retry() {
    let (endpoint, captured) = retry_model_server(vec![RetryModelResponse::Status(503)]).await;
    let (fixture, selection) = custom_fixture_with_endpoint(&endpoint);
    fixture
        .engine
        .inner
        .model_retry_sleep_hook
        .set_mode(ModelRetrySleepMode::Blocked);
    let session_id = fixture.engine.create_session(selection).unwrap().session_id;
    let owner = delegation_authority();
    let registration = fixture
        .engine
        .register_producer(session_id, owner.clone())
        .await
        .unwrap();
    let message_id = fixture
        .engine
        .send_producer_message(
            session_id,
            owner.clone(),
            registration,
            ProducerDeliveryMode::Steer,
            discard_key("failed-request"),
            "input seen by the failed request".into(),
        )
        .await
        .unwrap();
    tokio::time::timeout(
        test_timeout(3),
        fixture
            .engine
            .inner
            .model_retry_sleep_hook
            .wait_until_reached(1),
    )
    .await
    .expect("retry entered blocked backoff");
    let projection = discard_projection(&fixture.engine, session_id);
    let message = projection
        .messages
        .iter()
        .find(|message| message.message_id == message_id)
        .unwrap();
    assert!(!message.consumed);
    assert!(message.claims.is_empty());
    let run = message.admission.unwrap().0;
    fixture
        .engine
        .unregister_producer(session_id, owner.clone(), registration)
        .await
        .unwrap();
    fixture
        .engine
        .discard_producer_message(session_id, owner, message_id)
        .await
        .unwrap();
    fixture.engine.cancel_run(run).await.unwrap();
    wait_for_run_inactive(&fixture.engine, run).await;
    let projection = discard_projection(&fixture.engine, session_id);
    let message = projection
        .messages
        .iter()
        .find(|message| message.message_id == message_id)
        .unwrap();
    assert!(message.discarded);
    assert!(!message.consumed);
    let requests = captured.await.unwrap();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].contains("input seen by the failed request"));
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn recovery_releases_stale_claim_before_owner_discard() {
    let (endpoint, first_seen, _, _release, server) = held_first_request_server(false).await;
    let (fixture, selection) = custom_fixture_with_endpoint(&endpoint);
    let session = fixture
        .engine
        .create_session(selection.clone())
        .expect("stale claim session");
    let session_id = session.session_id;
    let run = fixture
        .engine
        .start_run(
            RunStartParams {
                session_id,
                client_run_id: ClientRunId::new("producer-discard-recovery").unwrap(),
                selection,
                input: "hold stale claim run".into(),
            },
            discard_origin(),
        )
        .await
        .expect("start stale claim run")
        .run_id;
    first_seen
        .await
        .expect("stale claim request reached server");
    let message_id = ProducerMessageId::new_v7();
    let owner = authority(ProducerOwner::GoalControl {
        goal_id: GoalId::new_v7(),
    });
    append_accepted_message(
        &fixture.engine,
        session_id,
        message_id,
        owner.owner.clone(),
        "recovered stale claim body",
    );
    fixture
        .engine
        .append_direct(
            session_id,
            Some(run),
            discard_origin(),
            EventPayload::ProducerMessageAdmitted { message_id },
        )
        .expect("append stale admission");
    fixture
        .engine
        .append_direct(
            session_id,
            Some(run),
            discard_origin(),
            EventPayload::ProducerMessagesClaimed {
                message_ids: vec![message_id],
            },
        )
        .expect("append stale claim");
    let claim_seq = fixture
        .engine
        .inner
        .store
        .get(session_id)
        .expect("stale claim projection")
        .meta
        .last_event_seq;
    fixture
        .engine
        .inner
        .store
        .persist_buffered_session(session_id)
        .expect("persist stale claim");
    fixture.engine.shutdown().await;
    server.abort();

    let reopened = reopen_engine(&fixture);
    let foreign = delegation_authority();
    assert!(
        reopened
            .discard_producer_message(session_id, foreign, message_id)
            .await
            .expect_err("foreign recovered discard")
            .to_string()
            .contains("another producer owner")
    );
    reopened
        .discard_producer_message(session_id, owner.clone(), message_id)
        .await
        .expect("owner discards after stale claim recovery");

    let events = discard_events(&reopened, session_id);
    let released = events
        .iter()
        .find(|event| {
            event.run_id == Some(run)
                && matches!(event.payload, EventPayload::ProducerMessagesReleased { claim_seq: released } if released == claim_seq)
        })
        .expect("run-scoped stale claim release");
    let discarded = events
        .iter()
        .find(|event| {
            matches!(event.payload, EventPayload::ProducerMessageDiscarded { message_id: discarded, .. } if discarded == message_id)
        })
        .expect("discard after stale claim release");
    assert!(released.seq < discarded.seq);
    assert_eq!(
        discard_projection(&reopened, session_id).messages[0].producer_owner,
        owner.owner
    );
    assert!(discard_projection(&reopened, session_id).messages[0].discarded);
    reopened.shutdown().await;
}

#[tokio::test]
async fn installed_plugin_callback_discards_after_unregister_and_repeats_successfully() {
    let (endpoint, first_seen, _, release, captured) = held_first_request_server(false).await;
    let (mut fixture, selection) = custom_fixture_with_endpoint(&endpoint);
    let session = fixture
        .engine
        .create_session(selection.clone())
        .expect("persistent plugin discard session");
    let session_id = session.session_id;
    fixture
        .engine
        .inner
        .store
        .persist_buffered_session(session_id)
        .expect("persist plugin discard session");
    fixture.engine.shutdown().await;

    let receipt_file = fixture
        ._directory
        .path()
        .join("producer-discard-callback.json");
    fixture.config.plugins.insert(
        "discard_callback".into(),
        PluginConfig {
            command: Some(python_command().into()),
            args: vec!["-c".into(), DISCARD_CALLBACK_PLUGIN.into()],
            env: BTreeMap::from([
                ("SESSION_ID".into(), session_id.to_string()),
                ("RECEIPT_FILE".into(), receipt_file.display().to_string()),
            ]),
            cwd: None,
            enabled: true,
            producer_messaging: true,
            interception_timeout_ms: 2_000,
            startup_timeout_ms: 10_000,
            shutdown_grace_ms: 3_000,
            tool_timeout_ms: 30_000,
        },
    );
    drop(fixture.engine);
    let engine = reopen_engine_parts(&fixture._directory, &fixture.config, &fixture.manager);
    engine.inner.plugins.await_eager_ready().await;

    tokio::time::timeout(test_timeout(5), async {
        loop {
            let inspection = engine
                .session_producers(SessionProducersParams { session_id })
                .await
                .expect("plugin registration before user run");
            if inspection.producers.iter().any(|registration| {
                registration.producer_owner
                    == ProducerOwner::Plugin {
                        plugin: "discard_callback".into(),
                    }
            }) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("plugin registered persistent session");

    let run = engine
        .start_run(
            RunStartParams {
                session_id,
                client_run_id: ClientRunId::new("plugin-discard-callback").unwrap(),
                selection,
                input: "ordinary user request held at the model".into(),
            },
            discard_origin(),
        )
        .await
        .expect("start ordinary user run")
        .run_id;
    first_seen
        .await
        .expect("ordinary user request reached server");

    let receipt: serde_json::Value = tokio::time::timeout(test_timeout(5), async {
        loop {
            if let Ok(contents) = fs::read_to_string(&receipt_file)
                && let Ok(receipt) = serde_json::from_str(&contents)
            {
                break receipt;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("plugin discard callback receipts");
    assert_eq!(receipt["unregistered"], true);
    assert_eq!(receipt["first_discard"], true);
    assert_eq!(receipt["repeat_discard"], true);
    assert_eq!(receipt["ready"], true);

    let message_id: ProducerMessageId = receipt["message_id"]
        .as_str()
        .expect("plugin receipt message ID")
        .parse()
        .expect("valid plugin receipt message ID");
    let plugin_owner = ProducerOwner::Plugin {
        plugin: "discard_callback".into(),
    };
    let events = discard_events(&engine, session_id);
    let accepted = events
        .iter()
        .find(|event| {
            matches!(
                &event.payload,
                EventPayload::ProducerMessageAccepted {
                    message_id: accepted,
                    producer_owner,
                    body,
                    ..
                } if *accepted == message_id
                    && producer_owner == &plugin_owner
                    && body == "plugin callback body must never reach the model"
            )
        })
        .expect("durable plugin producer acceptance");
    let discarded = events
        .iter()
        .filter(|event| {
            matches!(
                &event.payload,
                EventPayload::ProducerMessageDiscarded {
                    message_id: discarded,
                    reminder: None,
                    producer_owner: Some(owner),
                } if *discarded == message_id && owner == &plugin_owner
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(discarded.len(), 1);
    assert!(accepted.seq < discarded[0].seq);

    let inspection = engine
        .session_producers(SessionProducersParams { session_id })
        .await
        .expect("plugin state after discard callback");
    assert!(inspection.producers.is_empty());
    assert_eq!(inspection.plugin_recovery.len(), 1);
    assert_eq!(
        inspection.plugin_recovery[0].status,
        PluginRecoveryStatus::Ready
    );
    assert!(
        !events.iter().any(|event| {
            event.run_id == Some(run)
                && matches!(event.payload, EventPayload::ProducerMessageAdmitted { message_id: admitted } if admitted == message_id)
        }),
        "discarded queue message must remain unadmitted"
    );

    release.notify_one();
    wait_for_session_not_running(&engine, session_id).await;
    let requests = captured.await.expect("plugin discard model requests");
    assert_eq!(requests.len(), 1);
    assert!(requests[0].contains("ordinary user request held at the model"));
    assert!(!requests[0].contains("plugin callback body must never reach the model"));
    assert!(!discard_projection(&engine, session_id).messages[0].consumed);
    engine.shutdown().await;
}
