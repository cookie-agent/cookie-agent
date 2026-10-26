//! Held sessions (`docs/site/specs/held-sessions.md`): a stop or a run of
//! failures past `[model_retry] standard_retries` holds a session's
//! automatic runs until it is resumed.

use cookie_agent_protocol::{
    EventOrigin, EventPayload, HeldReason, InvocationId, Open, ProducerDeliveryMode,
    ProducerIdempotencyKey, ProducerOwner, RunOutcome, SafeDisplayText, SessionId,
};
use tokio::sync::broadcast;

use super::support::*;
use crate::runtime::ModelRetrySleepMode;
use crate::runtime::producers::ProducerAuthority;
use crate::{Engine, EngineError, PushSignal, SettledOutcome};

fn client() -> EventOrigin {
    EventOrigin::new("client:test").unwrap()
}

fn delegation() -> ProducerAuthority {
    ProducerAuthority {
        owner: ProducerOwner::Delegation {
            invocation_id: InvocationId::new_v7(),
        },
        connection_epoch: None,
    }
}

fn events(engine: &Engine, session: SessionId) -> Vec<cookie_agent_protocol::StoredEvent> {
    engine
        .inner
        .store
        .get(session)
        .unwrap()
        .log
        .events()
        .to_vec()
}

fn holds(engine: &Engine, session: SessionId) -> Vec<Open<HeldReason>> {
    events(engine, session)
        .into_iter()
        .filter_map(|event| match event.payload {
            EventPayload::AutomaticRunsHeld { reason } => Some(reason),
            _ => None,
        })
        .collect()
}

fn run_outcomes(engine: &Engine, session: SessionId) -> Vec<&'static str> {
    events(engine, session)
        .into_iter()
        .filter_map(|event| match event.payload {
            EventPayload::RunFinished {
                outcome: Open::Known(outcome),
            } => Some(match outcome {
                RunOutcome::Completed { .. } => "completed",
                RunOutcome::Failed { .. } => "failed",
                RunOutcome::Cancelled { .. } => "cancelled",
                RunOutcome::Interrupted { .. } => "interrupted",
            }),
            _ => None,
        })
        .collect()
}

async fn send(
    engine: &Engine,
    session: SessionId,
    authority: ProducerAuthority,
    key: &str,
    body: &str,
) -> Result<cookie_agent_protocol::ProducerMessageId, EngineError> {
    let registration = engine.register_producer(session, authority.clone()).await?;
    let sent = engine
        .send_producer_message(
            session,
            authority.clone(),
            registration,
            ProducerDeliveryMode::Steer,
            ProducerIdempotencyKey::new(key).unwrap(),
            SafeDisplayText::new("Producer result").unwrap(),
            body.into(),
        )
        .await;
    let _ = engine
        .unregister_producer(session, authority, registration)
        .await;
    sent
}

/// Runs a producer message starts fail at once: one run and
/// `standard_retries` retries, then the session is held for failures, and a
/// resume runs the message again.
#[tokio::test]
async fn failing_runs_hold_the_session_at_the_retry_budget_until_a_resume() {
    // Each run tries the model and its fallback.
    let (endpoint, captured) = retry_model_server(vec![
        RetryModelResponse::Status(400),
        RetryModelResponse::Status(400),
        RetryModelResponse::Status(400),
        RetryModelResponse::Status(400),
        RetryModelResponse::Success,
    ])
    .await;
    let retry = cookie_agent_config::ModelRetryConfig {
        standard_retries: 1,
        ..Default::default()
    };
    let (fixture, selection) = retry_fixture_with_endpoint(&endpoint, retry).await;
    fixture
        .engine
        .inner
        .test_hooks
        .model_retry_sleep_hook
        .set_mode(ModelRetrySleepMode::Immediate);
    let session = fixture.engine.create_session(selection).unwrap().session_id;
    send(
        &fixture.engine,
        session,
        delegation(),
        "failing",
        "every run sees this",
    )
    .await
    .unwrap();
    let held = await_projection(
        &fixture.engine,
        session,
        "held for failures",
        |projection| projection.meta.held.is_some(),
    )
    .await;
    wait_for_session_not_running(&fixture.engine, session).await;
    let Some(Open::Known(HeldReason::Failing { failures, error })) = held.meta.held.clone() else {
        panic!("held for failures: {:?}", held.meta.held);
    };
    assert_eq!(failures, 2);
    assert!(error.is_some());
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert_eq!(run_outcomes(&fixture.engine, session), ["failed", "failed"]);
    assert_eq!(holds(&fixture.engine, session).len(), 1);

    // A resume clears the hold and runs the waiting message.
    assert!(
        fixture
            .engine
            .resume_held_session(session, client())
            .await
            .unwrap()
    );
    let requests = with_watchdog("the resumed run", captured).await.unwrap();
    assert_eq!(requests.len(), 5);
    assert!(requests[4].contains("every run sees this"));
    wait_for_session_not_running(&fixture.engine, session).await;
    assert_eq!(
        run_outcomes(&fixture.engine, session),
        ["failed", "failed", "completed"]
    );
    assert_eq!(
        fixture.engine.inner.store.get(session).unwrap().meta.held,
        None
    );
    // A session that is not held is left as it is.
    assert!(
        !fixture
            .engine
            .resume_held_session(session, client())
            .await
            .unwrap()
    );
    fixture.engine.shutdown().await;
}

/// The user's stop holds the session at once: what waits, and what arrives
/// after, does not start a run.
#[tokio::test]
async fn a_stop_holds_the_session() {
    let (endpoint, first_seen, release_first, _captured) =
        super::producers::cancelled_request_boundary_server().await;
    let (fixture, selection) = custom_fixture_with_endpoint(&endpoint);
    let session = fixture.engine.create_session(selection).unwrap().session_id;
    send(
        &fixture.engine,
        session,
        delegation(),
        "report",
        "the report",
    )
    .await
    .unwrap();
    first_seen.await.unwrap();
    let run = fixture
        .engine
        .inner
        .store
        .get(session)
        .unwrap()
        .runs
        .keys()
        .next()
        .copied()
        .expect("the producer's run");
    fixture.engine.cancel_run(run, client()).await.unwrap();
    release_first.notify_one();
    wait_for_session_not_running(&fixture.engine, session).await;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert_eq!(
        fixture.engine.inner.store.get(session).unwrap().meta.held,
        Some(Open::Known(HeldReason::Stopped {}))
    );
    assert_eq!(run_outcomes(&fixture.engine, session), ["cancelled"]);

    // The engine's own reports still wait in it.
    send(
        &fixture.engine,
        session,
        delegation(),
        "later",
        "a later report",
    )
    .await
    .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert_eq!(run_outcomes(&fixture.engine, session), ["cancelled"]);

    // A revert to just after the first report leaves it waiting, held: the
    // cut takes the stopped run and its hold, and no run starts.
    let all = events(&fixture.engine, session);
    let accepted = all
        .iter()
        .find(|event| matches!(&event.payload, EventPayload::ProducerMessageAccepted { body, .. } if body.as_plain_text() == "the report"))
        .expect("the report")
        .seq;
    fixture
        .engine
        .revert_session(session, accepted, client(), Some(all.last().unwrap().seq))
        .await
        .expect("revert");
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert!(run_outcomes(&fixture.engine, session).is_empty());
    assert_eq!(
        fixture.engine.inner.store.get(session).unwrap().meta.held,
        Some(Open::Known(HeldReason::Stopped {}))
    );
    fixture.engine.shutdown().await;
}

/// A stop with nothing that could start a run on its own holds nothing: the
/// user's next message is just a message.
#[tokio::test]
async fn a_stop_with_nothing_waiting_holds_nothing() {
    let (endpoint, first_seen, release_first, _captured) =
        super::producers::cancelled_request_boundary_server().await;
    let (fixture, selection) = custom_fixture_with_endpoint(&endpoint);
    let session = fixture
        .engine
        .create_session(selection.clone())
        .unwrap()
        .session_id;
    let run = fixture
        .engine
        .start_run(
            cookie_agent_protocol::RunStartParams {
                reset_fallback: false,
                session_id: session,
                client_run_id: cookie_agent_protocol::ClientRunId::new("stopped").unwrap(),
                selection,
                content: "go slow".into(),
            },
            client(),
        )
        .await
        .unwrap()
        .run_id;
    first_seen.await.unwrap();
    fixture.engine.cancel_run(run, client()).await.unwrap();
    release_first.notify_one();
    wait_for_session_not_running(&fixture.engine, session).await;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert_eq!(run_outcomes(&fixture.engine, session), ["cancelled"]);
    assert_eq!(
        fixture.engine.inner.store.get(session).unwrap().meta.held,
        None
    );
    assert!(holds(&fixture.engine, session).is_empty());
    fixture.engine.shutdown().await;
}

/// Mail to a held subagent resumes it when its parent sends it, and is
/// refused from a sibling.
#[tokio::test]
async fn the_parents_mail_resumes_a_held_subagent_and_a_siblings_is_refused() {
    let (fixture, selection) = custom_fixture();
    let root = fixture.engine.create_session(selection).unwrap().session_id;
    let child = super::messaging::tree_session(&fixture.engine, root, root, 1);
    let sibling = super::messaging::tree_session(&fixture.engine, root, root, 1);
    fixture
        .engine
        .on_actor(child, move |engine| {
            engine.append_direct(
                child,
                None,
                EventOrigin::new("engine:test").unwrap(),
                EventPayload::AutomaticRunsHeld {
                    reason: Open::Known(HeldReason::Stopped {}),
                },
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let mail = |sender: SessionId, key: &'static str| {
        let engine = fixture.engine.clone();
        async move {
            let authority = ProducerAuthority {
                owner: ProducerOwner::Agent { session_id: sender },
                connection_epoch: None,
            };
            let producer = engine.register_producer(child, authority.clone()).await?;
            engine.accept_agent_message_direct_for_test(
                child,
                &authority,
                producer,
                ProducerDeliveryMode::Queue,
                ProducerIdempotencyKey::new(key).unwrap(),
                SafeDisplayText::new("mail").unwrap(),
                sender,
                "test",
                "a note".into(),
                0,
            )
        }
    };
    match mail(sibling, "from-sibling").await {
        Err(EngineError::Messaging(code)) => {
            assert!(code.starts_with("send_message:recipient_held"), "{code}");
        }
        other => panic!("a sibling's mail is refused: {other:?}"),
    }
    assert!(
        fixture
            .engine
            .inner
            .store
            .get(child)
            .unwrap()
            .meta
            .held
            .is_some()
    );
    mail(root, "from-parent").await.expect("the parent's mail");
    assert_eq!(
        fixture.engine.inner.store.get(child).unwrap().meta.held,
        None
    );
    assert!(
        events(&fixture.engine, child)
            .iter()
            .any(|event| matches!(event.payload, EventPayload::AutomaticRunsResumed {}))
    );
    fixture.engine.shutdown().await;
}

async fn no_signal(signals: &mut broadcast::Receiver<PushSignal>) -> bool {
    tokio::time::timeout(std::time::Duration::from_millis(200), signals.recv())
        .await
        .is_err()
}

/// A root held for failures is pushed once; one the user stopped is not.
#[tokio::test]
async fn a_hold_for_failures_is_pushed_and_a_stop_is_not() {
    let (fixture, selection) = custom_fixture();
    let engine = &fixture.engine;
    let mut signals = engine.subscribe_push_signals();
    let session = engine.create_session(selection).unwrap().session_id;
    engine.observe_push_event(
        session,
        &EventPayload::AutomaticRunsHeld {
            reason: Open::Known(HeldReason::Stopped {}),
        },
    );
    assert!(no_signal(&mut signals).await);
    engine.observe_push_event(
        session,
        &EventPayload::AutomaticRunsHeld {
            reason: Open::Known(HeldReason::Failing {
                failures: 4,
                error: Some(cookie_agent_protocol::SafeErrorMessage::new("model failure").unwrap()),
            }),
        },
    );
    let signal = with_watchdog("the push", async { signals.recv().await.unwrap() }).await;
    let PushSignal::Settled(settled) = signal else {
        panic!("a settled push");
    };
    assert_eq!(settled.root, session);
    let SettledOutcome::Failed { error } = settled.outcome else {
        panic!("a failure");
    };
    assert_eq!(error, "stopped after 4 failed runs: model failure");
    fixture.engine.shutdown().await;
}

/// A goal whose runs keep failing is held at the budget and makes no new
/// reminders: goal mode does not retry forever.
#[tokio::test]
async fn a_failing_goal_is_held_at_the_budget() {
    let (endpoint, _captured) = retry_model_server(vec![
        RetryModelResponse::Status(400),
        RetryModelResponse::Status(400),
        RetryModelResponse::Status(400),
        RetryModelResponse::Status(400),
    ])
    .await;
    let retry = cookie_agent_config::ModelRetryConfig {
        standard_retries: 1,
        ..Default::default()
    };
    let (fixture, selection) = retry_fixture_with_endpoint(&endpoint, retry).await;
    fixture
        .engine
        .inner
        .test_hooks
        .model_retry_sleep_hook
        .set_mode(ModelRetrySleepMode::Immediate);
    let session = fixture.engine.create_session(selection).unwrap().session_id;
    fixture
        .engine
        .set_session_goal(
            cookie_agent_protocol::SessionGoalSetParams {
                session_id: session,
                objective: "Keep going".into(),
                selection: None,
            },
            client(),
        )
        .await
        .expect("goal");
    await_projection(
        &fixture.engine,
        session,
        "held for failures",
        |projection| projection.meta.held.is_some(),
    )
    .await;
    wait_for_session_not_running(&fixture.engine, session).await;
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert_eq!(run_outcomes(&fixture.engine, session), ["failed", "failed"]);
    let reminders = events(&fixture.engine, session)
        .into_iter()
        .filter(|event| {
            matches!(
                &event.payload,
                EventPayload::ProducerMessageAccepted {
                    producer_owner: ProducerOwner::Goal { .. },
                    ..
                }
            )
        })
        .count();
    assert_eq!(reminders, 1);
    fixture.engine.shutdown().await;
}
