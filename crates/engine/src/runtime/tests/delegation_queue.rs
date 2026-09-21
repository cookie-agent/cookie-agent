use std::sync::{Arc, atomic::Ordering};

use cookie_agent_protocol::{
    ClientRunId, EventPayload, RunStartParams, SessionStatus, ToolTerminationOutcome,
};

use super::support::*;

#[tokio::test]
async fn background_startup_failure_releases_capacity_and_notifies() {
    let (endpoint, server) = scripted_startup_failure_delegation_server().await;
    let (fixture, selection) = custom_fixture_with_endpoint(&endpoint);
    fixture
        .engine
        .register_tool_provider(Arc::new(TestDelegateProvider {
            engine: fixture.engine.clone(),
        }));
    fixture
        .engine
        .inner
        .test_hooks
        .delegate_start_failures
        .store(1, Ordering::Release);
    let parent = fixture
        .engine
        .create_session(selection.clone())
        .expect("startup failure parent");
    fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: parent.session_id,
                client_run_id: ClientRunId::new("startup-failure-delegation").expect("run ID"),
                selection,
                input: "start five children with one injected failure".into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("accepted startup failure parent");

    await_projection(
        &fixture.engine,
        parent.session_id,
        "startup failure completion",
        |parent_projection| {
            let children = fixture
                .engine
                .children(parent.session_id)
                .expect("children");
            let completed = children
                .iter()
                .filter(|child| child.status == SessionStatus::Completed)
                .count();
            let failed = children
                .iter()
                .filter(|child| child.status == SessionStatus::Failed)
                .count();
            let finished = parent_projection
                .log
                .events()
                .iter()
                .filter(|event| matches!(event.payload, EventPayload::DelegateFinishedV2 { .. }))
                .count();
            completed == 4 && failed == 1 && finished == 5
        },
    )
    .await;
    let failed = fixture
        .engine
        .children(parent.session_id)
        .expect("children")
        .into_iter()
        .find(|child| child.status == SessionStatus::Failed)
        .expect("failed startup child");
    assert!(
        fixture
            .engine
            .inner
            .delegation_events
            .entries()
            .iter()
            .find(|entry| entry.reservation.child_session_id == failed.session_id)
            .is_some_and(|entry| entry.child_run_id.is_none())
    );
    with_watchdog("server fixture completion", server)
        .await
        .expect("startup failure server");
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn fifth_background_delegate_queues_and_starts_when_a_slot_frees() {
    let (endpoint, server) = scripted_queued_delegation_server().await;
    let (fixture, selection) = custom_fixture_with_endpoint(&endpoint);
    assert_eq!(fixture.config.runtime.delegation.max_concurrency, Some(4));
    fixture
        .engine
        .register_tool_provider(Arc::new(TestDelegateProvider {
            engine: fixture.engine.clone(),
        }));
    let parent = fixture
        .engine
        .create_session(selection.clone())
        .expect("queued parent session");
    fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: parent.session_id,
                client_run_id: ClientRunId::new("queued-delegation").expect("run ID"),
                selection,
                input: "launch five background children".into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("accepted queued parent run");

    await_projection(
        &fixture.engine,
        parent.session_id,
        "queued delegation completion",
        |parent_projection| {
            let queued = parent_projection
                .log
                .events()
                .iter()
                .any(|event| matches!(event.payload, EventPayload::DelegateQueued { .. }));
            let finished = parent_projection
                .log
                .events()
                .iter()
                .filter(|event| matches!(event.payload, EventPayload::DelegateFinishedV2 { .. }))
                .count();
            let completed_children = fixture
                .engine
                .children(parent.session_id)
                .expect("children")
                .iter()
                .filter(|child| child.status == SessionStatus::Completed)
                .count();
            queued && finished == 5 && completed_children == 5
        },
    )
    .await;
    assert_eq!(
        fixture
            .engine
            .children(parent.session_id)
            .expect("children")
            .len(),
        5
    );
    with_watchdog("server fixture completion", server)
        .await
        .expect("queued delegation server");
    fixture.engine.shutdown().await;
}

#[tokio::test]
async fn background_delegation_rejects_when_four_x_queue_is_full() {
    let (endpoint, server) = scripted_full_delegation_queue_server().await;
    let (fixture, selection) = custom_fixture_with_endpoint(&endpoint);
    fixture
        .engine
        .register_tool_provider(Arc::new(TestDelegateProvider {
            engine: fixture.engine.clone(),
        }));
    let parent = fixture
        .engine
        .create_session(selection.clone())
        .expect("full queue parent");
    fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: parent.session_id,
                client_run_id: ClientRunId::new("full-delegation-queue").expect("run ID"),
                selection,
                input: "fill the background delegation queue".into(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("accepted full queue parent run");

    let projection = await_projection(
        &fixture.engine,
        parent.session_id,
        "full queue parent completion",
        |projection| projection.status == SessionStatus::Completed,
    )
    .await;
    assert_eq!(
        fixture
            .engine
            .children(parent.session_id)
            .expect("children")
            .len(),
        20
    );
    assert_eq!(
        projection
            .log
            .events()
            .iter()
            .filter(|event| matches!(event.payload, EventPayload::DelegateQueued { .. }))
            .count(),
        16
    );
    assert!(projection.log.events().iter().any(|event| {
        matches!(
            &event.payload,
            EventPayload::ToolCallTerminated { termination }
                if termination.outcome == ToolTerminationOutcome::Failed
                    && termination.error.as_ref().is_some_and(|error| {
                        error.message.as_str().contains("background queue is full")
                    })
        )
    }));

    let queued_ids = projection
        .log
        .events()
        .iter()
        .filter_map(|event| match event.payload {
            EventPayload::DelegateQueued { session_id, .. } => Some(session_id),
            _ => None,
        })
        .collect::<Vec<_>>();
    let cancelled_id = queued_ids[0];
    let retry_id = queued_ids[1];
    fixture
        .engine
        .inner
        .test_hooks
        .delegate_terminal_append_failures
        .store(1, Ordering::Release);
    let error = fixture
        .engine
        .cancel_subagent(
            parent.session_id,
            cancelled_id,
            Some("first cancellation append fails".into()),
        )
        .await
        .expect_err("injected queued cancellation append failure");
    assert!(
        error
            .to_string()
            .contains("injected delegate terminal append failure")
    );
    assert!(
        fixture
            .engine
            .delegation_queue_contains(cancelled_id)
            .expect("queued child remains in FIFO")
    );
    assert_eq!(
        fixture
            .engine
            .get_subagent_result(
                parent.session_id,
                cancelled_id,
                false,
                0,
                1,
                tokio_util::sync::CancellationToken::new(),
            )
            .await
            .expect("queued child result after failed cancellation")
            .output,
        "<status>running</status>\n<content>\n</content>"
    );
    assert!(
        !fixture
            .engine
            .inner
            .store
            .get(cancelled_id)
            .expect("still queued child")
            .log
            .events()
            .iter()
            .any(|event| matches!(event.payload, EventPayload::DelegateChildTerminated { .. }))
    );
    let cancelled = fixture
        .engine
        .cancel_subagent(
            parent.session_id,
            cancelled_id,
            Some("cancel while queued".into()),
        )
        .await
        .expect("cancel queued subagent");
    assert_eq!(cancelled.metadata["status"], "cancelled");
    let cancelled_child = fixture
        .engine
        .inner
        .store
        .get(cancelled_id)
        .expect("cancelled queued child");
    assert_eq!(cancelled_child.status, SessionStatus::Cancelled);
    assert!(!cancelled_child.log.events().iter().any(|event| {
        matches!(
            event.payload,
            EventPayload::RunStarted { .. } | EventPayload::ModelAttemptStarted { .. }
        )
    }));
    assert!(
        fixture
            .engine
            .inner
            .delegation_events
            .entries()
            .iter()
            .find(|entry| entry.reservation.child_session_id == cancelled_id)
            .is_some_and(|entry| entry.child_run_id.is_none())
    );

    fixture
        .engine
        .inner
        .test_hooks
        .delegate_start_failures
        .store(100, Ordering::Release);
    let failure_observed = fixture
        .engine
        .inner
        .test_hooks
        .delegate_start_failure_observed
        .notified();
    let running_id = fixture
        .engine
        .children(parent.session_id)
        .expect("children")
        .into_iter()
        .find(|child| child.status == SessionStatus::Running)
        .expect("running child")
        .session_id;
    fixture
        .engine
        .cancel_subagent(parent.session_id, running_id, Some("free one slot".into()))
        .await
        .expect("cancel running child");
    tokio::time::timeout(test_timeout(EVENT_WATCHDOG_SECONDS), failure_observed)
        .await
        .expect("queued startup failure injection");
    assert!(
        fixture
            .engine
            .inner
            .delegation_events
            .entries()
            .iter()
            .find(|entry| entry.reservation.child_session_id == retry_id)
            .is_some_and(|entry| entry.child_run_id.is_none())
    );
    fixture
        .engine
        .inner
        .test_hooks
        .delegate_start_failures
        .store(0, Ordering::Release);
    await_session_change(
        &fixture.engine,
        retry_id,
        "queued child retained and retried",
        || {
            fixture
                .engine
                .inner
                .delegation_events
                .entries()
                .iter()
                .find(|entry| entry.reservation.child_session_id == retry_id)
                .is_some_and(|entry| entry.child_run_id.is_some())
                .then_some(())
        },
    )
    .await;
    server.abort();
    fixture.engine.shutdown().await;
}
