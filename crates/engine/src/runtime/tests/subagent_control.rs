use std::sync::Arc;

use cookie_agent_protocol::{ClientRunId, ProducerDeliveryMode, RunStartParams, SessionStatus};

use crate::{AgentMessageInvocation, Engine, EngineOptions};

use super::support::*;

#[tokio::test]
async fn running_subagent_result_is_empty_waits_and_stop_background_stops_its_turn() {
    let (endpoint, server) = scripted_cancellable_delegation_server().await;
    let (fixture, selection) = custom_fixture_with_endpoint(&endpoint);
    fixture.engine.register_tool_provider(
        crate::ToolLayer::Base,
        Arc::new(TestDelegateProvider {
            engine: fixture.engine.clone(),
        }),
    );
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
    // The running turn lists among the parent's background calls under the
    // child's handle, and stop_background stops it by that handle.
    let handle = fixture
        .engine
        .session_short_id(child_session_id)
        .expect("child handle");
    let calls = fixture.engine.background_calls(parent.session_id);
    assert!(
        calls
            .iter()
            .any(|call| call.job == handle && call.tool == "delegate_subagent"),
        "{calls:?}"
    );
    let stopped = fixture
        .engine
        .stop_background(
            parent.session_id,
            &handle,
            cookie_agent_protocol::BackgroundActor::Model,
        )
        .await
        .expect("stop the subagent's turn");
    assert_eq!(stopped, crate::runtime::Stopped::Subagent);
    let waited = waiter
        .await
        .expect("result waiter task")
        .expect("waited result");
    assert!(waited.output.starts_with("<status>cancelled</status>"));
    assert!(
        fixture
            .engine
            .background_calls(parent.session_id)
            .iter()
            .all(|call| call.job != handle)
    );
    // An idle child has no turn to stop.
    let error = fixture
        .engine
        .stop_background(
            parent.session_id,
            &handle,
            cookie_agent_protocol::BackgroundActor::User,
        )
        .await
        .expect_err("idle child");
    assert!(error.to_string().contains("has no turn running"), "{error}");
    server.abort();
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn running_subagent_results_are_scoped_to_the_callers_tree() {
    let (endpoint, reached, release, server) = scripted_running_steer_server().await;
    let (fixture, selection) = custom_fixture_with_endpoint(&endpoint);
    fixture.engine.register_tool_provider(
        crate::ToolLayer::Base,
        Arc::new(TestDelegateProvider {
            engine: fixture.engine.clone(),
        }),
    );
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
    fixture.engine.register_tool_provider(
        crate::ToolLayer::Base,
        Arc::new(TestDelegateProvider {
            engine: fixture.engine.clone(),
        }),
    );
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
    // The first turn was the foreground call's result; the woken turn
    // reports to the parent.
    let reports = |projection: &crate::session::SessionProjection| {
        projection
            .log
            .events()
            .iter()
            .filter_map(|event| match &event.payload {
                cookie_agent_protocol::EventPayload::ProducerMessageAccepted {
                    producer_owner: cookie_agent_protocol::ProducerOwner::Subagent { session_id },
                    body,
                    ..
                } if *session_id == child_session_id => Some(body.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    let parent = await_projection(
        &fixture.engine,
        parent.session_id,
        "the woken turn reports",
        |projection| !reports(projection).is_empty(),
    )
    .await;
    let reports = reports(&parent);
    assert_eq!(reports.len(), 1, "{reports:?}");
    assert!(reports[0].starts_with("<subagent_report "));
    assert!(reports[0].contains("child turn two"));
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn resume_settles_recovered_background_delegations_before_returning() {
    let (endpoint, reached, release, server) = scripted_running_steer_server().await;
    let (fixture, selection) = custom_fixture_with_endpoint(&endpoint);
    fixture.engine.register_tool_provider(
        crate::ToolLayer::Base,
        Arc::new(TestDelegateProvider {
            engine: fixture.engine.clone(),
        }),
    );
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
    copy_test_tree(
        &fixture._directory.path().join("data"),
        &snapshot.path().join("data"),
    );
    fixture.engine.shutdown().await;
    release.send(()).expect("release stopped child socket");
    drop(fixture.engine);

    let reopened = Engine::open(EngineOptions {
        data_dir: snapshot.path().join("data"),
        runtimes: cookie_agent_runtime_api::RuntimeOptions::new(&cwd),
        cwd,
        config,
        model_manager: manager,
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

#[tokio::test]
async fn the_user_moves_a_foreground_delegation_to_the_background() {
    let (endpoint, responses, server) = scripted_channel_server(3).await;
    responses
        .send(MatchedScriptedResponse::last_message_role(
            "user",
            scripted_tool_body(
                "detach-delegate",
                "delegate_subagent",
                serde_json::json!({
                    "agent_type": "worker",
                    "description": "Detached child",
                    "prompt": "slow child work"
                }),
            ),
        ))
        .expect("parent delegation response");
    let (fixture, selection) = custom_fixture_with_endpoint(&endpoint);
    fixture.engine.register_base_layer().expect("base layer");
    let parent = fixture
        .engine
        .create_session(selection.clone())
        .expect("parent");
    let run = fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: parent.session_id,
                client_run_id: ClientRunId::new("detach-delegate").expect("run ID"),
                selection,
                input: "delegate and wait".into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("parent run")
        .run_id;
    let child = await_child(
        &fixture.engine,
        parent.session_id,
        "running child",
        |child| child.status == SessionStatus::Running,
    )
    .await
    .session_id;
    let handle = fixture
        .engine
        .session_short_id(child)
        .expect("child handle");
    // A foreground delegation the call still waits for is not a background
    // call.
    assert!(
        fixture
            .engine
            .background_calls(parent.session_id)
            .is_empty()
    );
    let call = fixture
        .engine
        .inner
        .store
        .get(parent.session_id)
        .expect("parent projection")
        .log
        .events()
        .iter()
        .find_map(|event| match &event.payload {
            cookie_agent_protocol::EventPayload::ToolCallLinked { tool_call_id, .. } => {
                Some(*tool_call_id)
            }
            _ => None,
        })
        .expect("linked call");
    responses
        .send(MatchedScriptedResponse::last_message_role(
            "tool",
            scripted_text_body("parent carries on"),
        ))
        .expect("parent follow-up response");
    assert!(fixture.engine.request_detach(run, call));
    let projection = await_projection(
        &fixture.engine,
        parent.session_id,
        "the call ends with its immediate result",
        |projection| {
            projection
                .runs
                .get(&run)
                .is_some_and(|run| run.pending_calls.is_empty())
        },
    )
    .await;
    let immediate = projection
        .log
        .events()
        .iter()
        .find_map(|event| match &event.payload {
            cookie_agent_protocol::EventPayload::ToolCallTerminated { termination }
                if termination.tool_call_id == call =>
            {
                termination
                    .result
                    .as_ref()
                    .map(|result| result.output.clone())
            }
            _ => None,
        })
        .expect("immediate result");
    assert!(
        immediate.starts_with("User manually moved this call to the background;"),
        "{immediate}"
    );
    assert!(
        immediate.contains(&format!("[subagent session {handle}]")),
        "{immediate}"
    );
    let calls = fixture.engine.background_calls(parent.session_id);
    assert!(
        calls
            .iter()
            .any(|listed| listed.job == handle && listed.tool_call_id == call),
        "{calls:?}"
    );

    // The child's first turn reports like any other.
    responses
        .send(MatchedScriptedResponse::last_message_contains(
            "slow child work",
            scripted_text_body("child finished"),
        ))
        .expect("child response");
    let reports = |projection: &crate::session::SessionProjection| {
        projection
            .log
            .events()
            .iter()
            .filter_map(|event| match &event.payload {
                cookie_agent_protocol::EventPayload::ProducerMessageAccepted {
                    producer_owner: cookie_agent_protocol::ProducerOwner::Subagent { session_id },
                    body,
                    ..
                } if *session_id == child => Some(body.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    let projection = await_projection(
        &fixture.engine,
        parent.session_id,
        "the first turn reports",
        |projection| !reports(projection).is_empty(),
    )
    .await;
    let reports = reports(&projection);
    assert_eq!(reports.len(), 1, "{reports:?}");
    assert!(reports[0].contains("child finished"), "{reports:?}");
    server.abort();
    fixture.engine.shutdown().await;
}
