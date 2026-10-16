use std::{collections::BTreeMap, fs};

use cookie_agent_config::PluginConfig;

use cookie_agent_protocol::EventPayload;

use super::support::*;

use cookie_agent_protocol::{
    EventOrigin, GoalLifecycleAction, GoalStatus, ProducerOwner, ProducerSourceStatus,
    SessionGoalLifecycleParams, SessionGoalSetParams, SessionProducersParams,
};

const PRODUCER_PLUGIN: &str = r#"
import json, os, sys

def send(value):
    print(json.dumps(value), flush=True)

session = os.environ['SESSION_ID']
receipt_file = os.environ['RECEIPT_FILE']
release_file = os.environ['RELEASE_FILE']
message_id = None
recovery_pending = False
for line in sys.stdin:
    frame = json.loads(line)
    method = frame.get('method')
    if method == 'plugin/initialize':
        assert frame['params']['capabilities']['producer_messaging'] is True
        send({'jsonrpc':'2.0','id':frame['id'],'result':{
            'protocol_version':frame['params']['protocol_version'],
            'name':'producer_fixture','version':'1',
            'capabilities':{'producer_messaging':True,'tools':False,'resources':False,
                'subscribe_events':False,'subscribe_bus':False,'publish_bus':False,
                'publish_session_events':False,'intercept':[]},'tools':[]}})
    elif method == 'plugin/recovery/start':
        assert 'id' not in frame and frame['params'] == {}
        send({'jsonrpc':'2.0','id':'register','method':'plugin/producer/register',
            'params':{'session_id':session,'name':'default'}})
    elif frame.get('id') == 'register':
        producer_id = frame['result']['producer_id']
        send({'jsonrpc':'2.0','id':'send','method':'plugin/producer/send','params':{
            'session_id':session,'producer_id':producer_id,'mode':'queue',
            'description':'External plugin work completed',
            'idempotency_key':'plugin-engine-integration','body':'real plugin producer input'}})
    elif frame.get('id') == 'send':
        message_id = frame['result']['message_id']
        send({'jsonrpc':'2.0','id':'unregister','method':'plugin/producer/unregister',
            'params':{'session_id':session,'producer_id':producer_id}})
    elif frame.get('id') == 'unregister':
        assert frame['result'] == {}
        recovery_pending = True
        with open(receipt_file, 'w', encoding='utf-8') as output:
            json.dump({'message_id':message_id}, output)
    elif frame.get('id') == 'complete':
        assert frame['result'] == {}
    elif method == 'plugin/ping':
        send({'jsonrpc':'2.0','id':frame['id'],'result':{}})
    elif method == 'plugin/shutdown':
        break
    else:
        raise AssertionError(frame)
    if recovery_pending and os.path.exists(release_file):
        recovery_pending = False
        send({'jsonrpc':'2.0','id':'complete','method':'plugin/recovery/complete',
            'params':{'outcome':{'status':'ready'}}})
"#;

#[tokio::test]
async fn installed_plugin_producer_callbacks_are_durable_and_drive_a_model_turn() {
    let (endpoint, responses, captured) = scripted_channel_server(1).await;
    let (mut fixture, selection) = custom_fixture_with_endpoint(&endpoint);
    let session = fixture
        .engine
        .create_session(selection)
        .expect("persistent plugin producer session");
    let session_id = session.session_id;
    let origin = EventOrigin::new("client:producer-plugin-test").expect("event origin");
    let (compaction_reached, compaction_release) =
        fixture.engine.install_compaction_execution_hook_for_test();
    let compaction = fixture
        .engine
        .enqueue_compact_without_residency_for_test(session_id)
        .await
        .expect("hold plugin goal setup compaction");
    tokio::time::timeout(test_timeout(2), compaction_reached)
        .await
        .expect("plugin goal compaction hook timeout")
        .expect("plugin goal compaction reached hook");
    let goal = fixture
        .engine
        .set_session_goal(
            SessionGoalSetParams {
                session_id,
                objective: "Remain paused while plugin recovery runs".into(),
                selection: None,
            },
            origin.clone(),
        )
        .await
        .expect("persistent goal")
        .goal;
    let paused = fixture
        .engine
        .change_session_goal_lifecycle(
            SessionGoalLifecycleParams {
                session_id,
                goal_id: goal.goal_id,
                expected_revision: goal.revision,
                action: GoalLifecycleAction::Pause,
                selection: None,
            },
            origin,
        )
        .await
        .expect("pause persistent goal")
        .goal;
    assert_eq!(paused.status, GoalStatus::Paused);
    assert!(
        !fixture
            .engine
            .inner
            .store
            .get(session_id)
            .expect("paused plugin goal history")
            .log
            .events()
            .iter()
            .any(|event| matches!(
                &event.payload,
                EventPayload::ProducerMessageAccepted {
                    producer_owner: ProducerOwner::GoalControl { .. },
                    ..
                }
            ))
    );
    compaction_release.notify_waiters();
    let _ = tokio::time::timeout(test_timeout(2), compaction)
        .await
        .expect("plugin goal compaction completion timeout");
    fixture.engine.shutdown().await;

    let receipt_file = fixture
        ._directory
        .path()
        .join("producer-plugin-receipt.json");
    let release_file = fixture._directory.path().join("producer-plugin-release");
    fixture.config.plugins.insert(
        "producer_fixture".into(),
        PluginConfig {
            command: Some(python_command().into()),
            args: vec!["-c".into(), PRODUCER_PLUGIN.into()],
            env: BTreeMap::from([
                ("SESSION_ID".into(), session_id.to_string()),
                ("RECEIPT_FILE".into(), receipt_file.display().to_string()),
                ("RELEASE_FILE".into(), release_file.display().to_string()),
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
    .expect("plugin producer RPC receipts");
    let accepted = engine
        .inner
        .store
        .get(session_id)
        .expect("reopened producer session")
        .log
        .events()
        .into_iter()
        .find_map(|event| match event.payload {
            EventPayload::ProducerMessageAccepted {
                message_id,
                producer_owner,
                description,
                body,
                ..
            } if body == "real plugin producer input" => {
                Some((message_id, producer_owner, description))
            }
            _ => None,
        })
        .expect("durable producer acceptance before send ACK");
    assert_eq!(receipt["message_id"], accepted.0.to_string());
    assert_eq!(accepted.2.as_str(), "External plugin work completed");
    assert_eq!(
        accepted.1,
        ProducerOwner::external(
            cookie_agent_protocol::ProducerSource::plugin("producer_fixture").unwrap(),
            cookie_agent_protocol::ProducerName::default_name()
        )
    );

    let starting = engine
        .session_producers(SessionProducersParams { session_id })
        .await
        .expect("producer registry during plugin recovery");
    assert!(starting.producers.is_empty());
    assert_eq!(starting.sources.len(), 1);
    assert_eq!(starting.sources[0].status, ProducerSourceStatus::Starting);
    let before_commit = crate::goal_projection::GoalProducerProjection::from_events(
        &engine
            .inner
            .store
            .get(session_id)
            .expect("pending producer session")
            .log
            .events(),
    );
    assert!(
        before_commit
            .messages
            .iter()
            .any(|message| message.message_id == accepted.0 && !message.consumed)
    );
    assert!(
        engine
            .inner
            .store
            .get(session_id)
            .expect("plugin producer history")
            .log
            .events()
            .iter()
            .all(|event| !matches!(event.payload, EventPayload::PluginEventAdded { .. }))
    );

    let admission = await_event(&engine, session_id, "plugin producer admission", |event| {
        matches!(event.payload, EventPayload::ProducerMessageAdmitted { message_id } if message_id == accepted.0)
    })
    .await;
    let run_id = admission.run_id.expect("automatic plugin producer run");
    assert_eq!(
        engine
            .session_producers(SessionProducersParams { session_id })
            .await
            .expect("recovery barrier during automatic run")
            .sources[0]
            .status,
        ProducerSourceStatus::Starting
    );

    write_private_test_file(&release_file, b"ready");
    engine
        .inner
        .plugins
        .ping("producer_fixture")
        .await
        .expect("plugin transport remains healthy");
    tokio::time::timeout(test_timeout(5), async {
        loop {
            let inspection = engine
                .session_producers(SessionProducersParams { session_id })
                .await
                .expect("producer registry after plugin recovery");
            if inspection.sources[0].status == ProducerSourceStatus::Ready {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("plugin recovery readiness");
    responses
        .send(MatchedScriptedResponse::last_message_contains(
            "real plugin producer input",
            scripted_text_body("plugin producer model commit"),
        ))
        .expect("plugin producer model response");
    await_event(&engine, session_id, "plugin producer consumption", |event| {
        matches!(event.payload, EventPayload::ProducerMessageConsumed { message_id, run_id: consumed_run } if message_id == accepted.0 && consumed_run == run_id)
    })
    .await;
    wait_for_session_not_running(&engine, session_id).await;

    let requests = captured.await.expect("plugin producer model request");
    assert_eq!(requests.len(), 1);
    assert!(requests[0].contains("real plugin producer input"));
    engine.shutdown().await;
}

const NAMED_PRODUCERS_PLUGIN: &str = r#"
import json, os, sys

def send(value):
    print(json.dumps(value), flush=True)

session = os.environ['SESSION_ID']
receipt_file = os.environ['RECEIPT_FILE']
state = {}

def message(producer, body):
    return {'session_id':session,'producer_id':producer,'mode':'queue',
        'description':'Named producer','idempotency_key':'same-key','body':body}

for line in sys.stdin:
    with open(receipt_file + '.log', 'a', encoding='utf-8') as log:
        log.write(line)
    frame = json.loads(line)
    method = frame.get('method')
    request_id = frame.get('id')
    if method == 'plugin/initialize':
        send({'jsonrpc':'2.0','id':request_id,'result':{
            'protocol_version':frame['params']['protocol_version'],
            'name':'named_fixture','version':'1',
            'capabilities':{'producer_messaging':True,'tools':False,'resources':False,
                'subscribe_events':False,'subscribe_bus':False,'publish_bus':False,
                'publish_session_events':False,'intercept':[]},'tools':[]}})
    elif method == 'plugin/recovery/start':
        send({'jsonrpc':'2.0','id':'alpha','method':'plugin/producer/register',
            'params':{'session_id':session,'name':'alpha'}})
    elif request_id == 'alpha':
        state['alpha'] = frame['result']['producer_id']
        send({'jsonrpc':'2.0','id':'beta','method':'plugin/producer/register',
            'params':{'session_id':session,'name':'beta'}})
    elif request_id == 'beta':
        state['beta'] = frame['result']['producer_id']
        send({'jsonrpc':'2.0','id':'send-alpha','method':'plugin/producer/send',
            'params':message(state['alpha'], 'from alpha')})
    elif request_id == 'send-alpha':
        state['alpha_message'] = frame['result']['message_id']
        send({'jsonrpc':'2.0','id':'send-beta','method':'plugin/producer/send',
            'params':message(state['beta'], 'from beta')})
    elif request_id == 'send-beta':
        state['beta_message'] = frame['result']['message_id']
        send({'jsonrpc':'2.0','id':'too-big','method':'plugin/producer/send',
            'params':message(state['alpha'], 'x' * 65)})
    elif request_id == 'too-big':
        state['too_big_rejected'] = 'error' in frame
        send({'jsonrpc':'2.0','id':'bad-name','method':'plugin/producer/register',
            'params':{'session_id':session,'name':'Bad'}})
    elif request_id == 'bad-name':
        state['bad_name_rejected'] = 'error' in frame
        send({'jsonrpc':'2.0','id':'discard','method':'plugin/producer/discard',
            'params':{'session_id':session,'message_id':state['alpha_message']}})
    elif request_id == 'discard':
        state['discarded'] = frame.get('result') == {}
        send({'jsonrpc':'2.0','id':'complete','method':'plugin/recovery/complete',
            'params':{'outcome':{'status':'ready'}}})
    elif request_id == 'complete':
        with open(receipt_file, 'w', encoding='utf-8') as output:
            json.dump(state, output)
    elif method == 'plugin/ping':
        send({'jsonrpc':'2.0','id':request_id,'result':{}})
    elif method == 'plugin/shutdown':
        break
"#;

#[tokio::test]
async fn a_plugin_runs_named_producers_each_with_its_own_keys() {
    let (endpoint, _responses, _captured) = scripted_channel_server(0).await;
    let (mut fixture, selection) = custom_fixture_with_endpoint(&endpoint);
    let session_id = fixture
        .engine
        .create_session(selection)
        .expect("named producer session")
        .session_id;
    fixture
        .engine
        .inner
        .store
        .persist_buffered_session(session_id)
        .expect("persist the new session");
    fixture.engine.shutdown().await;
    let receipt_file = fixture._directory.path().join("named-producers.json");
    fixture.config.runtime.producers.max_body_bytes = 64;
    fixture.config.plugins.insert(
        "named_fixture".into(),
        PluginConfig {
            command: Some(python_command().into()),
            args: vec!["-c".into(), NAMED_PRODUCERS_PLUGIN.into()],
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
    let receipt: serde_json::Value = tokio::time::timeout(test_timeout(5), async {
        loop {
            if let Ok(contents) = fs::read_to_string(&receipt_file)
                && let Ok(receipt) = serde_json::from_str(&contents)
            {
                break receipt;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "named producer receipts; frames: {}",
            fs::read_to_string(receipt_file.with_extension("json.log")).unwrap_or_default()
        )
    });

    // One idempotency key, two producers: two messages, one per owner.
    assert_ne!(receipt["alpha_message"], receipt["beta_message"]);
    let source = cookie_agent_protocol::ProducerSource::plugin("named_fixture").unwrap();
    let events = engine
        .inner
        .store
        .get(session_id)
        .expect("named producer session")
        .log
        .events();
    let accepted = events
        .iter()
        .filter_map(|event| match &event.payload {
            EventPayload::ProducerMessageAccepted {
                producer_owner,
                body,
                ..
            } => Some((producer_owner.clone(), body.clone())),
            _ => None,
        })
        .collect::<Vec<_>>();
    let owner = |name: &str| {
        ProducerOwner::external(
            source.clone(),
            cookie_agent_protocol::ProducerName::new(name).unwrap(),
        )
    };
    assert_eq!(
        accepted,
        vec![
            (owner("alpha"), "from alpha".to_owned()),
            (owner("beta"), "from beta".to_owned()),
        ]
    );
    assert_eq!(receipt["too_big_rejected"], true, "{receipt}");
    assert_eq!(receipt["bad_name_rejected"], true, "{receipt}");
    // The source discards a message of any of its producers.
    assert_eq!(receipt["discarded"], true, "{receipt}");
    assert!(events.iter().any(|event| matches!(
        &event.payload,
        EventPayload::ProducerMessageDiscarded {
            producer_owner: Some(discarded),
            ..
        } if *discarded == owner("alpha")
    )));
    let inspection = engine
        .session_producers(SessionProducersParams { session_id })
        .await
        .expect("producer inspection");
    assert_eq!(inspection.producers.len(), 2);
    assert_eq!(
        inspection.sources,
        vec![cookie_agent_protocol::ProducerSourceState {
            source,
            status: ProducerSourceStatus::Ready,
        }]
    );
    engine.shutdown().await;
}
