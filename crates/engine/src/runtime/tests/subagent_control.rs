use std::sync::Arc;

use cookie_agent_protocol::{ClientRunId, ProducerDeliveryMode, RunStartParams, SessionStatus};

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
async fn running_subagent_results_are_scoped_to_the_callers_tree() {
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
    release.send(()).expect("release child response");

    await_child(
        &fixture.engine,
        parent.session_id,
        "child completion",
        |child| child.status == SessionStatus::Completed,
    )
    .await;
    // The script also serves a steered follow-up turn that never comes.
    server.abort();
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
async fn resume_settles_recovered_background_delegations_before_returning() {
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
        .expect("recovery settle parent");
    fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: parent.session_id,
                client_run_id: ClientRunId::new("recovery-settle").expect("run ID"),
                selection,
                input: "start a background child".into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("accepted recovery settle parent run");
    with_watchdog("reached fixture completion", reached)
        .await
        .expect("child request reached server");
    let child_session_id = await_child(
        &fixture.engine,
        parent.session_id,
        "recovery settle child",
        |child| child.status == SessionStatus::Running,
    )
    .await
    .session_id;
    // The parent run must be durably terminal before the snapshot: a parent run
    // that is still running is repaired as interrupted on adoption, which marks
    // its delegations finished without any recovery at all. The leak this test
    // guards lives in the other case, where the durable facts alone say nothing
    // about a child whose run died with the daemon.
    await_projection(
        &fixture.engine,
        parent.session_id,
        "recovery settle parent run completion",
        |session| {
            session
                .runs
                .values()
                .all(|run| run.status == SessionStatus::Completed)
        },
    )
    .await;
    assert_eq!(
        fixture
            .engine
            .running_background_delegations_for_test(parent.session_id),
        1,
        "the background child holds a slot before the restart"
    );

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
    release.send(()).expect("release stopped child socket");
    drop(fixture.engine);

    let reopened = Engine::open(EngineOptions {
        data_dir: snapshot.path().join("data"),
        cwd,
        config,
        model_manager: manager,
        tools: Vec::new(),
    })
    .expect("reopen background delegate snapshot");
    reopened
        .resume(parent.session_id)
        .await
        .expect("adopt parent for recovery");
    assert_eq!(
        reopened.running_background_delegations_for_test(parent.session_id),
        0,
        "resume returns only after the delegation recovery it scheduled settled"
    );
    let child = reopened
        .inner
        .store
        .get(child_session_id)
        .expect("recovered child projection");
    assert!(
        child
            .runs
            .values()
            .all(|run| run.status == SessionStatus::Interrupted),
        "the abandoned child run is terminalized by the recovery: {:?}",
        child
            .runs
            .values()
            .map(|run| run.status)
            .collect::<Vec<_>>()
    );
    reopened.shutdown().await;
    // The fixture scripts a steer that this scenario never sends, so its task
    // is abandoned rather than joined.
    server.abort();
}
