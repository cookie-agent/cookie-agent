//! Work an ACP agent starts on its own: a run started by a producer
//! message from the agent itself.

use cookie_agent_protocol::{EventPayload, ProducerOwner, RunId, SessionId};

use super::*;

/// The runs of `session` a wake of its agent started, by the wake's
/// message, and how each ended.
async fn wake_run(engine: &Engine, session: SessionId) -> (RunId, RunOutcome) {
    let started = await_event(engine, session, "a wake's run", |event| {
        matches!(&event.payload, EventPayload::ProducerMessageAdmitted { .. })
            && event.run_id.is_some()
            && events(engine, session).iter().any(|accepted| {
                matches!(
                    &accepted.payload,
                    EventPayload::ProducerMessageAccepted {
                        producer_owner: ProducerOwner::AcpAgent { .. },
                        ..
                    }
                )
            })
    })
    .await;
    let run = started.run_id.expect("the admission's run");
    (run, finished(engine, session, run).await)
}

#[tokio::test(flavor = "multi_thread")]
async fn work_the_agent_starts_on_its_own_is_a_run_its_wake_started() {
    let acp = acp_fixture(&[], &[]).await;
    let engine = acp.engine();
    let session = engine.create_session(selection()).unwrap().session_id;
    let outcome = turn(engine, session, "first", "wake-later 0.3 the build passed").await;
    assert_eq!(final_text(&outcome), "later");
    let (_, outcome) = wake_run(engine, session).await;
    assert_eq!(final_text(&outcome), "the build passed");
    // The wake's message names the agent, and it never went to the agent:
    // it got one prompt.
    let events = events(engine, session);
    assert!(events.iter().any(|event| matches!(
        &event.payload,
        EventPayload::ProducerMessageAccepted {
            producer_owner: ProducerOwner::AcpAgent { agent },
            description,
            ..
        } if *agent == agent_id() && description.as_str() == "Woke on its own"
    )));
    assert_eq!(prompts(&acp).len(), 1);
    // The session takes the next prompt as before.
    let outcome = turn(engine, session, "next", "say again").await;
    assert_eq!(final_text(&outcome), "again");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_permission_request_of_the_agents_own_work_is_decided_in_its_run() {
    let document = "---\ndescription: Fixture with rules\nmode: all\nacp: {}\npermissions:\n  bash: { \"git *\": allow, \"*\": deny }\n---\n";
    let acp = acp_fixture(&[], &[("fixture.md", document)]).await;
    let engine = acp.engine();
    let session = engine.create_session(selection()).unwrap().session_id;
    turn(engine, session, "first", "wake-ask 0.3 execute git status").await;
    let (_, outcome) = wake_run(engine, session).await;
    assert_eq!(final_text(&outcome), "woke and asked: yes");
}

/// Waits until the agent's own work says "waiting" in a wake's run, and
/// returns that run.
async fn waiting_wake(engine: &Engine, session: SessionId) -> RunId {
    let started = await_event(engine, session, "a wake's run", |event| {
        matches!(&event.payload, EventPayload::ProducerMessageAdmitted { .. })
            && event.run_id.is_some()
    })
    .await;
    let run = started.run_id.expect("the admission's run");
    await_event(engine, session, "the agent waiting", |event| {
        event.run_id == Some(run)
            && matches!(&event.payload, EventPayload::ModelAttemptStarted { .. })
    })
    .await;
    run
}

#[tokio::test(flavor = "multi_thread")]
async fn a_message_sent_during_the_agents_own_work_steers_into_it() {
    let acp = acp_fixture(&[], &[]).await;
    let engine = acp.engine();
    let session = engine.create_session(selection()).unwrap().session_id;
    turn(engine, session, "first", "wake-steer 0.3").await;
    let run = waiting_wake(engine, session).await;
    let steered = engine
        .steer(run, "look at the log".into(), origin())
        .await
        .unwrap();
    assert!(steered.accepted);
    let outcome = finished(engine, session, run).await;
    assert_eq!(final_text(&outcome), "steered: look at the log");
    let received = prompts(&acp);
    assert_eq!(received.len(), 2, "the first prompt, and the steer");
    assert_eq!(received[1], ["look at the log"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn stop_cancels_the_agents_own_work() {
    let acp = acp_fixture(&[], &[]).await;
    let engine = acp.engine();
    let session = engine.create_session(selection()).unwrap().session_id;
    turn(engine, session, "first", "wake-steer 0.3").await;
    let run = waiting_wake(engine, session).await;
    engine.cancel_run(run, origin()).await.expect("cancel");
    let outcome = finished(engine, session, run).await;
    assert!(
        matches!(outcome, RunOutcome::Cancelled { .. }),
        "{outcome:?}"
    );
    assert!(
        acp.received()
            .iter()
            .any(|message| message["method"] == "session/cancel")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_agents_own_work_shows_after_the_user_stopped_a_run() {
    let acp = acp_fixture(&[], &[]).await;
    let engine = acp.engine();
    let session = engine.create_session(selection()).unwrap().session_id;
    // Its own work comes later; meanwhile the user stops a run, which stops
    // what would start next, but not the agent's own work.
    turn(
        engine,
        session,
        "first",
        "wake-later 1.5 the tests finished",
    )
    .await;
    let run = start(engine, session, "wait", "wait").await;
    await_event(engine, session, "the agent waiting", |event| {
        event.run_id == Some(run)
            && matches!(&event.payload, EventPayload::ModelAttemptStarted { .. })
    })
    .await;
    engine.cancel_run(run, origin()).await.expect("cancel");
    finished(engine, session, run).await;
    let (_, outcome) = wake_run(engine, session).await;
    assert_eq!(final_text(&outcome), "the tests finished");
}
