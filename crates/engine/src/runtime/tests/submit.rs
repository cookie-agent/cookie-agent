use std::time::Duration;

use cookie_agent_protocol::{
    ClientRunId, EventOrigin, EventPayload, InputDelivery, QueuedInputs, SessionStatus,
    SessionSubmitInputParams, SessionSubmitInputResult,
};

use super::support::*;

fn text_response(text: &str) -> String {
    format!(
        "data: {}\n\ndata: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"stop\"}}]}}\n\n",
        serde_json::json!({"choices":[{"delta":{"content":text},"finish_reason":null}]})
    )
}

fn origin(value: &str) -> EventOrigin {
    EventOrigin::new(value).unwrap()
}

fn submit(
    session_id: cookie_agent_protocol::SessionId,
    input: &str,
    delivery: InputDelivery,
    client_run_id: &str,
) -> SessionSubmitInputParams {
    SessionSubmitInputParams {
        session_id,
        content: input.into(),
        delivery,
        selection: None,
        client_run_id: ClientRunId::new(client_run_id).unwrap(),
        reset_fallback: false,
    }
}

#[tokio::test]
async fn a_queued_input_starts_the_next_run_as_its_author_once_the_run_completes() {
    let (endpoint, _captured, reached, release) = scripted_server_with_delayed_response(
        vec![text_response("first done"), text_response("queued done")],
        0,
    )
    .await;
    let (fixture, selection) = custom_fixture_with_endpoint(&endpoint);
    let session = fixture.engine.create_session(selection).unwrap();

    // Idle: either delivery starts a run, using the session's selection.
    let started = fixture
        .engine
        .submit_input(
            submit(session.session_id, "first", InputDelivery::Queue, "first"),
            origin("client:tui-a"),
        )
        .await
        .expect("start");
    assert!(matches!(started, SessionSubmitInputResult::Started { .. }));
    tokio::time::timeout(Duration::from_secs(5), reached)
        .await
        .expect("first model request")
        .expect("reached");

    let queued = fixture
        .engine
        .submit_input(
            submit(
                session.session_id,
                "after you",
                InputDelivery::Queue,
                "later",
            ),
            origin("client:tui-b"),
        )
        .await
        .expect("queue");
    let SessionSubmitInputResult::Queued { queued_seq } = queued else {
        panic!("a running session queues: {queued:?}");
    };
    // A retried submission resolves to the same entry.
    assert_eq!(
        fixture
            .engine
            .submit_input(
                submit(
                    session.session_id,
                    "after you",
                    InputDelivery::Queue,
                    "later"
                ),
                origin("client:tui-b"),
            )
            .await
            .expect("retry"),
        SessionSubmitInputResult::Queued { queued_seq }
    );
    let projection = fixture.engine.inner.store.get(session.session_id).unwrap();
    let queue = QueuedInputs::from_events(&projection.log.event_snapshot());
    assert_eq!(
        queue
            .head()
            .map(|entry| entry.content.as_plain_text().into_owned()),
        Some("after you".to_owned())
    );

    release.notify_one();
    let projection = await_projection(
        &fixture.engine,
        session.session_id,
        "queued run completed",
        |projection| projection.status == SessionStatus::Completed && projection.runs.len() == 2,
    )
    .await;
    let events = projection.log.events();
    assert!(QueuedInputs::from_events(&events).is_empty());
    let queued_run = projection
        .runs
        .values()
        .find(|run| run.client_run_id.as_str() == "later")
        .expect("the queued input started its own run");
    let submitted = events
        .iter()
        .find(|event| {
            event.run_id == Some(queued_run.id)
                && matches!(event.payload, EventPayload::UserInputSubmitted { .. })
        })
        .expect("queued input submitted");
    assert_eq!(submitted.origin, Some(origin("client:tui-b")));
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn cancelling_the_run_voids_the_queue_instead_of_starting_it() {
    let (endpoint, _captured, reached, _release) =
        scripted_server_with_delayed_response(vec![text_response("never")], 0).await;
    let (fixture, selection) = custom_fixture_with_endpoint(&endpoint);
    let session = fixture.engine.create_session(selection).unwrap();
    let SessionSubmitInputResult::Started { run_id } = fixture
        .engine
        .submit_input(
            submit(session.session_id, "first", InputDelivery::Steer, "first"),
            origin("client:tui-a"),
        )
        .await
        .expect("start")
    else {
        panic!("an idle session starts a run");
    };
    tokio::time::timeout(Duration::from_secs(5), reached)
        .await
        .expect("first model request")
        .expect("reached");
    assert!(matches!(
        fixture
            .engine
            .submit_input(
                submit(
                    session.session_id,
                    "then this",
                    InputDelivery::Queue,
                    "then"
                ),
                origin("client:tui-b"),
            )
            .await
            .expect("queue"),
        SessionSubmitInputResult::Queued { .. }
    ));
    fixture
        .engine
        .cancel_run(run_id, origin("client:tui-a"))
        .await
        .expect("cancel");
    let projection = await_projection(
        &fixture.engine,
        session.session_id,
        "cancelled run",
        |projection| projection.status == SessionStatus::Cancelled,
    )
    .await;
    // Give a mistaken drain the chance to start a run before checking.
    tokio::time::sleep(Duration::from_millis(50)).await;
    let projection = fixture
        .engine
        .inner
        .store
        .get(projection.meta.session_id)
        .unwrap();
    assert_eq!(projection.runs.len(), 1);
    assert!(QueuedInputs::from_events(&projection.log.event_snapshot()).is_empty());
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn a_picked_model_is_the_session_selection_every_later_run_uses() {
    let (endpoint, _captured, _reached, _release) =
        scripted_server_with_delayed_response(vec![text_response("from the pick")], usize::MAX)
            .await;
    let (fixture, selection) =
        retry_fixture_with_endpoint(&endpoint, cookie_agent_config::ModelRetryConfig::default())
            .await;
    let session = fixture.engine.create_session(selection.clone()).unwrap();
    let mut picked = selection.clone();
    picked.model.model = "custom.test/group/fallback".parse().unwrap();

    assert!(
        fixture
            .engine
            .select_model(session.session_id, picked.clone(), origin("client:tui-a"))
            .await
            .expect("pick")
    );
    // Picking what the session already has records nothing.
    assert!(
        !fixture
            .engine
            .select_model(session.session_id, picked.clone(), origin("client:tui-b"))
            .await
            .expect("same pick")
    );
    let mut missing = selection.clone();
    missing.model.model = "custom.test/group/missing".parse().unwrap();
    assert!(
        fixture
            .engine
            .select_model(session.session_id, missing, origin("client:tui-a"))
            .await
            .is_err()
    );
    assert_eq!(
        fixture
            .engine
            .session_model_selection(session.session_id)
            .unwrap(),
        picked
    );

    // Another client's message with no selection of its own runs on the pick.
    let SessionSubmitInputResult::Started { run_id } = fixture
        .engine
        .submit_input(
            submit(
                session.session_id,
                "hello",
                InputDelivery::Steer,
                "after-pick",
            ),
            origin("client:tui-b"),
        )
        .await
        .expect("submit")
    else {
        panic!("an idle session starts a run");
    };
    let projection = await_projection(
        &fixture.engine,
        session.session_id,
        "run on the picked model",
        |projection| projection.status == SessionStatus::Completed,
    )
    .await;
    let events = projection.log.events();
    let started = events
        .iter()
        .find_map(|event| match &event.payload {
            EventPayload::RunStarted { selection, .. } if event.run_id == Some(run_id) => {
                Some(selection.clone())
            }
            _ => None,
        })
        .expect("run started");
    assert_eq!(started.model, picked.model);
    let picked_by = events
        .iter()
        .find(|event| matches!(event.payload, EventPayload::SessionModelSelected { .. }))
        .and_then(|event| event.origin.clone());
    assert_eq!(picked_by, Some(origin("client:tui-a")));
    fixture.engine.shutdown().await;
}

/// Any client can take back an input another client queued: the queue
/// loses it, the recall is recorded under the client that recalled it, and
/// the content comes back to that client.
#[tokio::test]
async fn any_client_recalls_another_clients_queued_input() {
    let (endpoint, _captured, reached, release) =
        scripted_server_with_delayed_response(vec![text_response("first done")], 0).await;
    let (fixture, selection) = custom_fixture_with_endpoint(&endpoint);
    let session = fixture.engine.create_session(selection).unwrap();
    fixture
        .engine
        .submit_input(
            submit(session.session_id, "first", InputDelivery::Queue, "first"),
            origin("client:tui-a"),
        )
        .await
        .expect("start");
    tokio::time::timeout(Duration::from_secs(5), reached)
        .await
        .expect("first model request")
        .expect("reached");
    let SessionSubmitInputResult::Queued { queued_seq } = fixture
        .engine
        .submit_input(
            submit(session.session_id, "later", InputDelivery::Queue, "later"),
            origin("client:web-b"),
        )
        .await
        .expect("queue")
    else {
        panic!("a running session queues");
    };

    assert_eq!(
        fixture
            .engine
            .recall_input(session.session_id, queued_seq, origin("client:tui-a"))
            .await
            .expect("recall"),
        cookie_agent_protocol::SessionRecallInputResult::Recalled {
            content: "later".into()
        }
    );
    let projection = fixture.engine.inner.store.get(session.session_id).unwrap();
    let events = projection.log.event_snapshot();
    assert!(QueuedInputs::from_events(&events).head().is_none());
    let recall = events
        .iter()
        .find(|event| matches!(event.payload, EventPayload::UserInputQueueRecalled { .. }))
        .expect("the recall");
    assert_eq!(recall.origin, Some(origin("client:tui-a")));
    release.notify_one();
}
