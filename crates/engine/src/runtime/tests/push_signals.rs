//! When the engine signals a push notification
//! (`docs/site/specs/push-notifications.md` sections 3 and 4): a root that
//! settles, held back by what can still wake it but not by recurring cron
//! jobs, and approvals and questions anywhere in a tree.

use std::time::Duration;

use cookie_agent_protocol::{
    ClientRunId, CronCreateParams, CronMaxRuns, CronSchedule, CronTarget, EventOrigin,
    EventPayload, RunOutcome, RunStartParams, SafeErrorMessage, SessionId,
};
use tokio::sync::broadcast;

use super::support::*;
use crate::{Engine, NeedsYouItem, PushSignal, SettledOutcome};

fn job(
    schedule: CronSchedule,
    max_runs: Option<CronMaxRuns>,
    target: CronTarget,
) -> CronCreateParams {
    CronCreateParams {
        name: None,
        prompt: "Wake up.".into(),
        schedule,
        max_runs,
        target,
        delivery: None,
        permission_mode: None,
        max_consecutive_failures: None,
        enabled: None,
    }
}

/// A fixture whose model is never called: runs end through [`end_run`].
async fn quiet_fixture() -> (Fixture, cookie_agent_protocol::RunSelection) {
    let (endpoint, _responses, _captured) = scripted_channel_server(0).await;
    custom_fixture_with_endpoint(&endpoint)
}

fn daily() -> CronSchedule {
    CronSchedule::Cron("0 3 * * *".into())
}

/// Ends a run of `session` as `outcome` would, without a model.
fn end_run(engine: &Engine, session: SessionId, outcome: RunOutcome) {
    engine.observe_push_event(session, &EventPayload::run_finished(outcome));
}

fn completed(text: &str) -> RunOutcome {
    RunOutcome::Completed {
        final_text: Some(text.into()),
    }
}

async fn next_signal(signals: &mut broadcast::Receiver<PushSignal>) -> PushSignal {
    with_watchdog("a push signal", async {
        signals.recv().await.expect("push signal")
    })
    .await
}

async fn no_signal(signals: &mut broadcast::Receiver<PushSignal>) {
    let signal = tokio::time::timeout(Duration::from_millis(300), signals.recv()).await;
    assert!(signal.is_err(), "unexpected signal: {signal:?}");
}

async fn settled_root(
    signals: &mut broadcast::Receiver<PushSignal>,
) -> (SessionId, SettledOutcome) {
    match next_signal(signals).await {
        PushSignal::Settled(settled) => (settled.root, settled.outcome),
        other => panic!("expected a settle, got {other:?}"),
    }
}

#[tokio::test]
async fn a_completed_root_turn_settles_once_with_its_reply() {
    let (endpoint, responses, _captured) = scripted_channel_server(1).await;
    let (fixture, selection) = custom_fixture_with_endpoint(&endpoint);
    let engine = &fixture.engine;
    let mut signals = engine.subscribe_push_signals();
    let session = engine.create_session(selection.clone()).unwrap().session_id;
    responses
        .send(MatchedScriptedResponse::last_message_contains(
            "say done",
            scripted_text_body("All done."),
        ))
        .unwrap();
    engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: session,
                client_run_id: ClientRunId::new("push").unwrap(),
                selection,
                content: "say done".into(),
            },
            EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("run started");
    let (root, outcome) = settled_root(&mut signals).await;
    assert_eq!(root, session);
    assert!(matches!(
        outcome,
        SettledOutcome::Completed { final_text: Some(text) } if text == "All done."
    ));
    no_signal(&mut signals).await;
}

#[tokio::test]
async fn failed_runs_settle_and_cancelled_or_interrupted_ones_do_not() {
    let (fixture, selection) = quiet_fixture().await;
    let engine = &fixture.engine;
    let mut signals = engine.subscribe_push_signals();
    let session = engine.create_session(selection).unwrap().session_id;

    end_run(
        engine,
        session,
        RunOutcome::Failed {
            error: SafeErrorMessage::new("rate limited").unwrap(),
            model_error: None,
            resolved_model: None,
        },
    );
    let (_, outcome) = settled_root(&mut signals).await;
    assert!(matches!(outcome, SettledOutcome::Failed { error } if error == "rate limited"));

    end_run(engine, session, RunOutcome::Cancelled { reason: None });
    end_run(engine, session, RunOutcome::Interrupted { reason: None });
    no_signal(&mut signals).await;
}

#[tokio::test]
async fn a_one_time_job_holds_the_push_until_it_is_gone_and_a_recurring_one_does_not() {
    let (fixture, selection) = quiet_fixture().await;
    let engine = &fixture.engine;
    let mut signals = engine.subscribe_push_signals();
    let session = engine.create_session(selection).unwrap().session_id;

    // `in`, and a cron expression with the default one run: one-time.
    let in_an_hour = engine
        .create_cron(job(
            CronSchedule::In("1h".into()),
            None,
            CronTarget::Session(session),
        ))
        .await
        .unwrap();
    let once = engine
        .create_cron(job(daily(), None, CronTarget::Session(session)))
        .await
        .unwrap();
    // Recurring, also with only a few runs: they count for nothing.
    engine
        .create_cron(job(
            daily(),
            Some(CronMaxRuns::Unlimited),
            CronTarget::Session(session),
        ))
        .await
        .unwrap();
    engine
        .create_cron(job(
            daily(),
            Some(CronMaxRuns::Count(3)),
            CronTarget::Session(session),
        ))
        .await
        .unwrap();

    end_run(engine, session, completed("Waiting for the fire."));
    no_signal(&mut signals).await;
    engine.delete_cron(in_an_hour.id).await.unwrap();
    no_signal(&mut signals).await;
    engine.delete_cron(once.id).await.unwrap();
    let (root, _) = settled_root(&mut signals).await;
    assert_eq!(root, session);
}

#[tokio::test]
async fn a_subagents_one_time_job_holds_the_root_and_its_recurring_job_does_not() {
    let (fixture, selection) = quiet_fixture().await;
    let engine = &fixture.engine;
    let mut signals = engine.subscribe_push_signals();
    let root = engine.create_session(selection).unwrap().session_id;
    let child = create_buffered_delegated_child(engine, root);
    engine
        .create_cron(job(
            daily(),
            Some(CronMaxRuns::Unlimited),
            CronTarget::Subagent(child),
        ))
        .await
        .unwrap();
    // A delegated session's turn never settles on its own.
    end_run(engine, child, completed("child"));
    no_signal(&mut signals).await;

    end_run(engine, root, completed("root"));
    assert_eq!(settled_root(&mut signals).await.0, root);

    let once = engine
        .create_cron(job(
            CronSchedule::In("1h".into()),
            None,
            CronTarget::Subagent(child),
        ))
        .await
        .unwrap();
    end_run(engine, root, completed("root again"));
    no_signal(&mut signals).await;
    engine.delete_cron(once.id).await.unwrap();
    assert_eq!(settled_root(&mut signals).await.0, root);
}

#[tokio::test]
async fn new_input_or_a_new_run_disarms_the_root() {
    let (fixture, selection) = quiet_fixture().await;
    let engine = &fixture.engine;
    let mut signals = engine.subscribe_push_signals();
    let session = engine.create_session(selection).unwrap().session_id;
    let once = engine
        .create_cron(job(
            CronSchedule::In("1h".into()),
            None,
            CronTarget::Session(session),
        ))
        .await
        .unwrap();
    end_run(engine, session, completed("held"));
    // The user typed: they are there, so the turn before is not pushed.
    engine.observe_push_event(
        session,
        &EventPayload::UserInputAdmitted {
            content: cookie_agent_protocol::UserContent::text("more"),
        },
    );
    engine.delete_cron(once.id).await.unwrap();
    no_signal(&mut signals).await;
}

#[tokio::test]
async fn questions_anywhere_in_a_tree_name_their_root_and_subagent() {
    let (fixture, selection) = quiet_fixture().await;
    let engine = &fixture.engine;
    let mut signals = engine.subscribe_push_signals();
    let root = engine.create_session(selection).unwrap().session_id;
    let child = create_buffered_delegated_child(engine, root);
    assert_eq!(engine.root_session_of(child), root);
    assert_eq!(engine.root_session_of(root), root);
    engine.signal_question_needs_you(child, "claude-code".into(), "Which branch?".into(), false);
    let PushSignal::NeedsYou(needs) = next_signal(&mut signals).await else {
        panic!("expected a needs-you signal");
    };
    assert_eq!(needs.root, root);
    assert_eq!(needs.session, child);
    assert!(needs.handle.is_some());
    assert!(matches!(
        needs.item,
        NeedsYouItem::Question { agent, message, url: false }
            if agent == "claude-code" && message == "Which branch?"
    ));
}
