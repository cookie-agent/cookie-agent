//! How an ACP prompt round ends: its usage and its stop reason.

use cookie_agent_protocol::{EventPayload, SessionId, Usage};

use super::*;

/// The usage `session` recorded, in order.
fn recorded_usage(engine: &Engine, session: SessionId) -> Vec<Usage> {
    events(engine, session)
        .into_iter()
        .filter_map(|event| match event.payload {
            EventPayload::ModelUsageRecorded { usage, .. } => Some(usage),
            _ => None,
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_or_cancelled_round_records_the_usage_that_arrived() {
    let acp = acp_fixture(&[], &[]).await;
    let engine = acp.engine();
    let session = engine.create_session(selection()).unwrap().session_id;
    // The answer's usage, on a failed and on a cancelled round.
    turn(engine, session, "refusal", "stop refusal").await;
    turn(engine, session, "cancelled", "stop cancelled").await;
    let usage = recorded_usage(engine, session);
    assert_eq!(usage.len(), 2, "{usage:?}");
    for usage in &usage {
        assert_eq!(usage.input_tokens, Some(40));
        assert_eq!(usage.output_tokens, Some(2));
    }

    // A round cancelled from cookie: the last usage update's.
    let run = start(engine, session, "wait", "wait").await;
    await_event(engine, session, "the agent waiting", |event| {
        event.run_id == Some(run)
            && matches!(&event.payload, EventPayload::ModelAttemptStarted { .. })
    })
    .await;
    engine.cancel_run(run, origin()).await.expect("cancel");
    finished(engine, session, run).await;
    wait_for_session_not_running(engine, session).await;
    let usage = recorded_usage(engine, session);
    assert_eq!(usage.len(), 3, "{usage:?}");
    assert_eq!(usage[2].input_tokens, Some(555));
    assert_eq!(usage[2].output_tokens, None);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stop_reason_names_why_the_run_failed_and_how_the_turn_finished() {
    let acp = acp_fixture(&[], &[]).await;
    let engine = acp.engine();
    let session = engine.create_session(selection()).unwrap().session_id;
    for (index, (reason, message, finish)) in [
        (
            "refusal",
            "the agent refused (stop reason `refusal`)",
            cookie_agent_protocol::ModelFinishReason::Refused,
        ),
        (
            "max_tokens",
            "the agent hit its output token limit (stop reason `max_tokens`)",
            cookie_agent_protocol::ModelFinishReason::Length,
        ),
        (
            "max_turn_requests",
            "the agent hit its turn request limit (stop reason `max_turn_requests`)",
            cookie_agent_protocol::ModelFinishReason::Other("max_turn_requests".to_owned()),
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let run = start(
            engine,
            session,
            &format!("stop-{index}"),
            &format!("stop {reason}"),
        )
        .await;
        let outcome = finished(engine, session, run).await;
        wait_for_session_not_running(engine, session).await;
        assert!(
            matches!(&outcome, RunOutcome::Failed { error, .. } if error.as_str() == message),
            "{outcome:?}"
        );
        let finished_as = events(engine, session)
            .into_iter()
            .rev()
            .find_map(|event| match event.payload {
                EventPayload::ModelTurnCommitted { turn, .. } if event.run_id == Some(run) => {
                    Some(turn.finish_reason)
                }
                _ => None,
            })
            .expect("the turn so far");
        assert_eq!(finished_as, finish);
    }
}
