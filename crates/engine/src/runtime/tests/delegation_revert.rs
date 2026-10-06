use std::sync::Arc;

use cookie_agent_protocol::{ClientRunId, EventPayload, RunStartParams, SessionId, SessionStatus};

use super::support::*;
use crate::runtime::EngineError;

async fn held_background_child_server(
    expect_wake: bool,
) -> (
    String,
    tokio::sync::oneshot::Receiver<()>,
    Arc<tokio::sync::Notify>,
    tokio::task::JoinHandle<Vec<String>>,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("held delegation listener");
    let address = listener.local_addr().expect("held delegation address");
    let (child_reached_tx, child_reached_rx) = tokio::sync::oneshot::channel();
    let release = Arc::new(tokio::sync::Notify::new());
    let task_release = Arc::clone(&release);
    let task = tokio::spawn(async move {
        let mut requests = Vec::new();
        let (mut parent, _) = listener.accept().await.expect("parent request");
        requests.push(
            String::from_utf8(read_scripted_http_request(&mut parent).await)
                .expect("parent request UTF-8"),
        );
        write_scripted_sse(
            &mut parent,
            &scripted_tool_body(
                "held-background-delegate",
                "delegate_subagent",
                serde_json::json!({
                    "agent_type": "worker",
                    "description": "Held background child",
                    "prompt": "held child request",
                    "background": true
                }),
            ),
        )
        .await;

        let mut child = None;
        let mut child_reached_tx = Some(child_reached_tx);
        while child.is_none() || requests.len() < 3 {
            let (mut socket, _) = listener.accept().await.expect("delegation request");
            let request = read_scripted_http_request(&mut socket).await;
            let is_child = scripted_effective_last_message(&request).is_some_and(|message| {
                message.get("role").and_then(serde_json::Value::as_str) == Some("user")
                    && message.get("content").and_then(serde_json::Value::as_str)
                        == Some("held child request")
            });
            let request_text = String::from_utf8(request).expect("delegation request UTF-8");
            requests.push(request_text);
            if is_child {
                child = Some(socket);
                if let Some(reached) = child_reached_tx.take() {
                    let _ = reached.send(());
                }
            } else {
                write_scripted_sse(&mut socket, &scripted_text_body("parent finished early")).await;
            }
        }

        task_release.notified().await;
        write_scripted_sse(
            child.as_mut().expect("held child socket"),
            &scripted_text_body("held child completed"),
        )
        .await;

        if expect_wake {
            let (mut wake, _) =
                tokio::time::timeout(test_timeout(EVENT_WATCHDOG_SECONDS), listener.accept())
                    .await
                    .expect("producer wake timeout")
                    .expect("producer wake");
            requests.push(
                String::from_utf8(read_scripted_http_request(&mut wake).await)
                    .expect("producer wake UTF-8"),
            );
            write_scripted_sse(&mut wake, &scripted_text_body("completion observed")).await;
        }
        requests
    });
    (
        format!("http://{address}/v1"),
        child_reached_rx,
        release,
        task,
    )
}

async fn start_held_background_delegation(
    expect_wake: bool,
) -> (
    Fixture,
    SessionId,
    Arc<tokio::sync::Notify>,
    tokio::task::JoinHandle<Vec<String>>,
) {
    let (endpoint, child_reached, release, server) =
        held_background_child_server(expect_wake).await;
    let (fixture, selection) = custom_fixture_with_endpoint(&endpoint);
    fixture
        .engine
        .register_tool_provider(Arc::new(TestDelegateProvider {
            engine: fixture.engine.clone(),
        }));
    let parent = fixture
        .engine
        .create_session(selection.clone())
        .expect("held delegation parent");
    fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: parent.session_id,
                client_run_id: ClientRunId::new("held-background-parent").expect("run ID"),
                selection,
                input: "start held background child".into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("start held delegation parent");
    tokio::time::timeout(test_timeout(10), child_reached)
        .await
        .expect("held child request timeout")
        .expect("held child request signal");
    await_projection(
        &fixture.engine,
        parent.session_id,
        "parent completion while child is held",
        |projection| projection.status == SessionStatus::Completed,
    )
    .await;
    (fixture, parent.session_id, release, server)
}

#[tokio::test]
async fn revert_refuses_a_running_child_and_deletes_it_once_finished() {
    let (fixture, parent_id, release, server) = start_held_background_delegation(true).await;
    let reservation = fixture
        .engine
        .inner
        .store
        .get(parent_id)
        .expect("parent projection")
        .log
        .event_snapshot()
        .iter()
        .find_map(|event| match &event.payload {
            EventPayload::DelegationReserved { reservation, .. } => {
                Some((event.seq, reservation.clone()))
            }
            _ => None,
        })
        .expect("visible delegation reservation");
    let child_id = reservation.1.child_session_id;
    let child_dir = fixture
        .engine
        .inner
        .store
        .resolve_dir(child_id)
        .expect("child directory");
    let tip_before = fixture
        .engine
        .inner
        .store
        .get(parent_id)
        .expect("parent")
        .log
        .physical_tip_seq();

    // The cut would delete the background child while it still runs.
    let refused = fixture
        .engine
        .revert_session(
            parent_id,
            reservation.0 - 1,
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await;
    assert!(
        matches!(refused, Err(EngineError::SessionRunning(id)) if id == child_id),
        "{refused:?}"
    );
    assert_eq!(
        fixture
            .engine
            .inner
            .store
            .get(parent_id)
            .expect("parent")
            .log
            .physical_tip_seq(),
        tip_before,
        "a refused revert leaves the log alone"
    );

    // Once the child finishes (and its completion wakes the root), the same
    // revert removes the delegation and deletes the child outright.
    release.notify_waiters();
    let requests = server.await.expect("held delegation server");
    assert_eq!(requests.len(), 4, "the completion woke the root");
    wait_for_session_not_running(&fixture.engine, parent_id).await;
    fixture
        .engine
        .revert_session(
            parent_id,
            reservation.0 - 1,
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("revert before delegation reservation");

    assert!(!child_dir.exists(), "child directory deleted");
    assert!(fixture.engine.inner.store.get(child_id).is_err());
    assert!(
        fixture
            .engine
            .inner
            .delegation_events
            .get(reservation.1.invocation_id)
            .is_none()
    );
    let parent = fixture
        .engine
        .inner
        .store
        .get(parent_id)
        .expect("reverted parent");
    assert!(parent.status != SessionStatus::Running);
    assert!(!parent.log.all_events().iter().any(|event| {
        matches!(
            &event.payload,
            EventPayload::DelegationReserved { .. }
                | EventPayload::DelegateFinishedV2 { .. }
                | EventPayload::ProducerMessageAccepted { .. }
        )
    }));
    // Only what closes the run the cut fell inside follows it.
    assert!(
        parent
            .log
            .all_events()
            .iter()
            .filter(|event| event.seq >= reservation.0)
            .all(|event| matches!(
                &event.payload,
                EventPayload::RunInterrupted { .. }
                    | EventPayload::ToolCallTerminated { .. }
                    | EventPayload::InternalAgentInterrupted { .. }
            )),
        "{:#?}",
        parent.log.all_events()
    );
    let children = fixture
        .engine
        .inner
        .store
        .children(parent_id)
        .expect("children");
    assert!(children.is_empty(), "{children:?}");
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn surviving_background_reservation_publishes_completion_once() {
    let (fixture, parent_id, release, server) = start_held_background_delegation(true).await;
    let through_seq = fixture
        .engine
        .inner
        .store
        .get(parent_id)
        .expect("parent projection")
        .log
        .all_events()
        .last()
        .expect("parent tip")
        .seq;
    fixture
        .engine
        .revert_session(
            parent_id,
            through_seq,
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("revert while retaining delegation reservation");

    release.notify_waiters();
    let completion = await_event(
        &fixture.engine,
        parent_id,
        "surviving delegation completion",
        |event| matches!(event.payload, EventPayload::DelegateFinishedV2 { .. }),
    )
    .await;
    let invocation_id = match completion.payload {
        EventPayload::DelegateFinishedV2 { invocation_id, .. } => invocation_id,
        _ => unreachable!(),
    };
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let events = fixture
        .engine
        .inner
        .store
        .get(parent_id)
        .expect("completed parent")
        .log
        .all_events();
    let description = events
        .iter()
        .find_map(|event| match &event.payload {
            EventPayload::ProducerMessageAccepted {
                producer_owner:
                    cookie_agent_protocol::ProducerOwner::Delegation {
                        invocation_id: accepted,
                    },
                description,
                body,
                ..
            } if *accepted == invocation_id => {
                assert!(body.starts_with("<subagent_notification>"));
                Some(description)
            }
            _ => None,
        })
        .expect("delegation producer description");
    assert!(description.as_str().starts_with("Delegation completed: "));
    assert!(description.as_str().len() <= cookie_agent_protocol::SafeDisplayText::MAX_BYTES);
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(
                &event.payload,
                EventPayload::ProducerMessageAccepted {
                    producer_owner: cookie_agent_protocol::ProducerOwner::Delegation {
                        invocation_id: accepted
                    },
                    ..
                } if *accepted == invocation_id
            ))
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(
                event.payload,
                EventPayload::DelegateFinishedV2 {
                    invocation_id: logged,
                    ..
                } if logged == invocation_id
            ))
            .count(),
        1
    );
    let requests = server.await.expect("held delegation server");
    assert_eq!(requests.len(), 4, "surviving completion should wake once");
    fixture.engine.shutdown().await;
}
