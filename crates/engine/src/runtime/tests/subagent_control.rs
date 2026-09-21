use std::sync::Arc;

use cookie_agent_protocol::{
    ClientRunId, EventPayload, ProducerDeliveryMode, RunStartParams, SessionId, SessionStatus,
};

use crate::{AgentMessageInvocation, Engine, EngineOptions};

use super::support::*;

#[tokio::test]
async fn running_subagent_result_is_empty_waits_and_cancel_is_session_addressed() {
    let (endpoint, server) = scripted_cancellable_delegation_server().await;
    let (fixture, selection) = custom_fixture_with_endpoint(&endpoint);
    fixture
        .engine
        .register_tool_provider(Arc::new(TestDelegateProvider {
            engine: fixture.engine.clone(),
        }));
    let parent = fixture
        .engine
        .create_session(selection.clone())
        .expect("cancellable parent");
    fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: parent.session_id,
                client_run_id: ClientRunId::new("cancellable-delegation").expect("run ID"),
                selection,
                input: "start a cancellable child".into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("accepted cancellable parent run");

    let child_session_id = await_child(
        &fixture.engine,
        parent.session_id,
        "running child",
        |child| child.status == SessionStatus::Running,
    )
    .await
    .session_id;
    let immediate = fixture
        .engine
        .get_subagent_result(
            parent.session_id,
            child_session_id,
            false,
            0,
            20,
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .expect("running result");
    assert_eq!(
        immediate.output,
        "<status>running</status>\n<content>\n</content>"
    );

    let wait_engine = fixture.engine.clone();
    let waiter = tokio::spawn(async move {
        wait_engine
            .get_subagent_result(
                parent.session_id,
                child_session_id,
                true,
                0,
                20,
                tokio_util::sync::CancellationToken::new(),
            )
            .await
    });
    let cancelled = fixture
        .engine
        .cancel_subagent(
            parent.session_id,
            child_session_id,
            Some("test cancellation".into()),
        )
        .await
        .expect("cancel subagent");
    assert_eq!(cancelled.metadata["status"], "cancelled");
    let waited = waiter
        .await
        .expect("result waiter task")
        .expect("waited result");
    assert!(waited.output.starts_with("<status>cancelled</status>"));
    server.abort();
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn running_subagent_steer_promotes_user_input_and_enforces_ownership_and_state() {
    let (endpoint, reached, release, server) = scripted_running_steer_server().await;
    let (fixture, selection) = custom_fixture_with_endpoint(&endpoint);
    fixture
        .engine
        .register_tool_provider(Arc::new(TestDelegateProvider {
            engine: fixture.engine.clone(),
        }));
    let parent = fixture
        .engine
        .create_session(selection.clone())
        .expect("steer parent");
    let foreign = fixture
        .engine
        .create_session(selection.clone())
        .expect("foreign parent");
    fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: parent.session_id,
                client_run_id: ClientRunId::new("running-subagent-steer").expect("run ID"),
                selection,
                input: "start a child to steer".into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("accepted steer parent run");
    with_watchdog("reached fixture completion", reached)
        .await
        .expect("child request reached server");

    let child_session_id = await_child(
        &fixture.engine,
        parent.session_id,
        "running steer child",
        |child| child.status == SessionStatus::Running,
    )
    .await
    .session_id;
    let steered = fixture
        .engine
        .steer_subagent(
            parent.session_id,
            child_session_id,
            "focus on the revised requirement".into(),
        )
        .await
        .expect("steer running child");
    assert_eq!(steered.metadata["status"], "running");
    let foreign_error = fixture
        .engine
        .steer_subagent(foreign.session_id, child_session_id, "foreign steer".into())
        .await
        .expect_err("foreign parent cannot steer child");
    assert!(
        foreign_error
            .to_string()
            .contains("not owned by the caller")
    );
    let foreign_result_error = fixture
        .engine
        .get_subagent_result(
            foreign.session_id,
            child_session_id,
            false,
            0,
            2000,
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .expect_err("foreign parent cannot read child result");
    let foreign_result_message = foreign_result_error.to_string();
    assert!(
        foreign_result_message.contains("unknown subagent reference"),
        "foreign reference must self-repair: {foreign_result_message}"
    );
    assert!(foreign_result_message.contains(&child_session_id.to_string()));
    // The child's *handle* is likewise scoped to its own tree: a foreign caller
    // that lists no children of its own must not resolve it.
    let child_handle = fixture
        .engine
        .get_session(child_session_id)
        .expect("child session")
        .short_id
        .expect("generated child handle");
    let foreign_handle_error = fixture
        .engine
        .get_subagent_result(
            foreign.session_id,
            child_handle.clone(),
            false,
            0,
            2000,
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .expect_err("foreign parent cannot resolve another tree's handle");
    let foreign_handle_message = foreign_handle_error.to_string();
    assert!(foreign_handle_message.contains("unknown subagent reference"));
    assert!(foreign_handle_message.contains(&child_handle));
    let missing_id = SessionId::new_v7();
    let missing_error = fixture
        .engine
        .steer_subagent(parent.session_id, missing_id, "missing steer".into())
        .await
        .expect_err("missing child cannot be steered");
    assert!(missing_error.to_string().contains(&missing_id.to_string()));
    release.send(()).expect("release child response");

    await_child(
        &fixture.engine,
        parent.session_id,
        "steered child completion",
        |child| child.status == SessionStatus::Completed,
    )
    .await;
    let child_events = fixture
        .engine
        .inner
        .store
        .get(child_session_id)
        .expect("steered child projection")
        .log
        .events();
    assert!(child_events.iter().any(|event| matches!(
        &event.payload,
        EventPayload::UserInputAdmitted { input }
            if input == "focus on the revised requirement"
    )));
    assert!(child_events.iter().any(|event| matches!(
        &event.payload,
        EventPayload::UserInputSubmitted { input }
            if input == "focus on the revised requirement"
    )));
    let terminal_error = fixture
        .engine
        .steer_subagent(parent.session_id, child_session_id, "too late".into())
        .await
        .expect_err("terminal child cannot be steered");
    assert!(terminal_error.to_string().contains("terminal (completed)"));
    let requests = with_watchdog("server fixture completion", server)
        .await
        .expect("running steer server");
    assert_eq!(requests.len(), 3);
    assert!(requests[2].contains("focus on the revised requirement"));
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn finished_subagent_woken_by_send_message_reports_running_then_new_turn_text() {
    let (endpoint, server) = scripted_finished_wake_server().await;
    let (fixture, selection) = custom_fixture_with_endpoint(&endpoint);
    fixture
        .engine
        .register_tool_provider(Arc::new(TestDelegateProvider {
            engine: fixture.engine.clone(),
        }));
    let (wake_reached, wake_release) = fixture.engine.install_producer_wake_hook();
    let parent = fixture
        .engine
        .create_session(selection.clone())
        .expect("finished wake parent");
    fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: parent.session_id,
                client_run_id: ClientRunId::new("finished-wake").expect("run ID"),
                selection,
                input: "start a child that finishes".into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("accepted finished wake parent run");

    let child_session_id = await_child(
        &fixture.engine,
        parent.session_id,
        "finished wake child",
        |child| child.status == SessionStatus::Completed,
    )
    .await
    .session_id;

    let first = fixture
        .engine
        .get_subagent_result(
            parent.session_id,
            child_session_id,
            false,
            0,
            2000,
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .expect("first completed result");
    assert!(first.output.starts_with("<status>completed</status>"));
    assert!(first.output.contains("child turn one"));

    fixture
        .engine
        .send_agent_message(AgentMessageInvocation {
            sender_session_id: parent.session_id,
            sender_run_id: cookie_agent_protocol::RunId::new_v7(),
            sender_tool_call_id: cookie_agent_protocol::ToolCallId::new_v7(),
            recipient_session_id: child_session_id,
            body: "next task".into(),
            mode: ProducerDeliveryMode::Steer,
        })
        .await
        .expect("wake send");

    // The wake is paused before its `RunStarted`, so the projection is still
    // terminal. Liveness must nevertheless report running, not turn-one text.
    with_watchdog("wake_reached fixture completion", wake_reached)
        .await
        .expect("producer wake paused");
    let immediate = fixture
        .engine
        .get_subagent_result(
            parent.session_id,
            child_session_id,
            false,
            0,
            2000,
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .expect("running result after wake accepted");
    assert_eq!(
        immediate.output,
        "<status>running</status>\n<content>\n</content>"
    );
    assert!(!immediate.output.contains("child turn one"));

    let wait_engine = fixture.engine.clone();
    let mut waiter = tokio::spawn(async move {
        wait_engine
            .get_subagent_result(
                parent.session_id,
                child_session_id,
                true,
                0,
                2000,
                tokio_util::sync::CancellationToken::new(),
            )
            .await
    });
    let premature = tokio::time::timeout(std::time::Duration::from_millis(150), &mut waiter).await;
    assert!(
        premature.is_err(),
        "wait=true must block until the steered turn ends, not return turn-one text"
    );

    wake_release.notify_one();

    let waited = waiter
        .await
        .expect("woken result waiter")
        .expect("woken result");
    assert!(waited.output.starts_with("<status>completed</status>"));
    assert!(waited.output.contains("child turn two"));
    assert!(!waited.output.contains("child turn one"));

    let requests = with_watchdog("server fixture completion", server)
        .await
        .expect("finished wake server");
    assert_eq!(requests.len(), 4);
    assert!(requests[3].contains("next task"));
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn queued_subagent_steer_survives_restart_and_promotes_on_first_run() {
    let (endpoint, reached, release, server) = scripted_queued_steer_recovery_server().await;
    let (fixture, selection) = custom_fixture_with_endpoint(&endpoint);
    fixture
        .engine
        .register_tool_provider(Arc::new(TestDelegateProvider {
            engine: fixture.engine.clone(),
        }));
    let parent = fixture
        .engine
        .create_session(selection.clone())
        .expect("queued steer parent");
    fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: parent.session_id,
                client_run_id: ClientRunId::new("queued-subagent-steer").expect("run ID"),
                selection,
                input: "queue five children".into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("accepted queued steer parent run");
    with_watchdog("reached fixture completion", reached)
        .await
        .expect("queue reached capacity");
    let queued_id = fixture
        .engine
        .inner
        .delegation_events
        .entries()
        .into_iter()
        .find(|entry| entry.child_run_id.is_none())
        .expect("queued child reservation event")
        .reservation
        .child_session_id;
    let steered = fixture
        .engine
        .steer_subagent(
            parent.session_id,
            queued_id,
            "apply this queued correction".into(),
        )
        .await
        .expect("steer queued child");
    assert_eq!(steered.metadata["status"], "queued");
    let queued = fixture
        .engine
        .inner
        .store
        .get(queued_id)
        .expect("queued child projection");
    assert!(queued.log.is_persisted());
    assert!(queued.log.events().iter().any(|event| {
        event.run_id.is_none()
            && matches!(
                &event.payload,
                EventPayload::UserInputAdmitted { input }
                    if input == "apply this queued correction"
            )
    }));

    let snapshot = private_tempdir();
    let cwd = fixture._directory.path().to_owned();
    let config = fixture.config.clone();
    let manager = Arc::clone(&fixture.manager);
    for session in fixture.engine.inner.store.all() {
        session.log.flush().expect("flush crash snapshot");
    }
    copy_test_tree(
        &fixture._directory.path().join("data"),
        &snapshot.path().join("data"),
    );
    fixture.engine.shutdown().await;
    release.send(()).expect("release stopped child sockets");
    drop(fixture.engine);
    let reopened = Engine::open(EngineOptions {
        data_dir: snapshot.path().join("data"),
        cwd,
        config,
        model_manager: manager,
        tools: Vec::new(),
        model_snapshot_directory: Some(snapshot.path().join("model-snapshots")),
    })
    .expect("reopen queued child snapshot");
    reopened
        .resume(parent.session_id)
        .await
        .expect("adopt queued parent for recovery");
    await_running_background_delegations(
        &reopened,
        parent.session_id,
        0,
        "parent adoption releases interrupted child capacity",
    )
    .await;
    for child_id in reopened
        .inner
        .delegation_events
        .entries()
        .into_iter()
        .filter_map(|entry| {
            (entry.reservation.child_session_id != queued_id)
                .then_some(entry.reservation.child_session_id)
        })
    {
        reopened
            .resume(child_id)
            .await
            .expect("adopt running child for recovery");
    }
    reopened
        .resume(queued_id)
        .await
        .expect("adopt queued child for recovery");
    await_projection(
        &reopened,
        queued_id,
        "recovered queued steer completion",
        |child| child.status == SessionStatus::Completed,
    )
    .await;
    let recovered_events = reopened
        .inner
        .store
        .get(queued_id)
        .expect("recovered queued child")
        .log
        .events();
    assert!(recovered_events.iter().any(|event| matches!(
        &event.payload,
        EventPayload::UserInputSubmitted { input }
            if input == "apply this queued correction"
    )));
    let requests = with_watchdog("server fixture completion", server)
        .await
        .expect("queued steer recovery server");
    assert_eq!(requests.len(), 2);
    assert!(requests[1].contains("apply this queued correction"));
    reopened.shutdown().await;
}
