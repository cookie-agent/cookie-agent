//! Restarting the daemon (`docs/site/specs/engine-configuration.md`
//! section 11): an `idle` restart waits for the active runs, the runs
//! started while it waits, and the running background calls, and refuses new
//! runs once it is due; `now` is due at once and the shutdown that follows
//! interrupts the runs, and an engine without a hook refuses.

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use cookie_agent_protocol::{
    ClientRunId, EventOrigin, EventPayload, InputDelivery, RestartWhen, RunId, RunSelection,
    RunStartParams, SessionId, SessionStatus, SessionSubmitInputParams, SessionSubmitInputResult,
};

use super::support::*;
use crate::{Engine, EngineError, RestartStatus};

fn origin() -> EventOrigin {
    EventOrigin::new("client:test").unwrap()
}

/// Installs a hook that records being called.
fn record_restart(engine: &Engine) -> Arc<AtomicBool> {
    let called = Arc::new(AtomicBool::new(false));
    let hook = Arc::clone(&called);
    engine.set_restart_hook(Box::new(move || hook.store(true, Ordering::SeqCst)));
    called
}

async fn start(
    engine: &Engine,
    selection: &RunSelection,
    session: SessionId,
    key: &str,
) -> Result<RunId, EngineError> {
    start_with(engine, selection, session, key, "wait for the model").await
}

async fn start_with(
    engine: &Engine,
    selection: &RunSelection,
    session: SessionId,
    key: &str,
    content: &str,
) -> Result<RunId, EngineError> {
    engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: session,
                client_run_id: ClientRunId::new(key).unwrap(),
                selection: selection.clone(),
                content: content.into(),
            },
            origin(),
        )
        .await
        .map(|started| started.run_id)
}

async fn wait_for_restart(called: &AtomicBool) {
    crate::eventually::eventually(
        test_timeout(EVENT_WATCHDOG_SECONDS),
        "the restart to be due",
        || called.load(Ordering::SeqCst),
    )
    .await;
}

async fn wait_for_status(engine: &Engine, expected: RestartStatus) {
    let mut status = engine.subscribe_restart();
    with_watchdog("the restart status", async {
        status
            .wait_for(|status| *status == Some(expected))
            .await
            .expect("the engine is alive");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_engine_without_a_restart_hook_refuses_to_restart() {
    let fixture = fixture();
    let refused = fixture
        .engine
        .schedule_restart(RestartWhen::Idle)
        .expect_err("no hook");
    assert!(
        matches!(refused, EngineError::RestartUnsupported),
        "{refused}"
    );
    assert!(!fixture.engine.restart_pending());
    fixture.engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_idle_restart_waits_for_the_runs_started_meanwhile_and_refuses_new_ones_once_due() {
    let (endpoint, responses, _captured) = scripted_channel_server(3).await;
    let (fixture, selection) = custom_fixture_with_endpoint(&endpoint);
    let engine = &fixture.engine;
    let called = record_restart(engine);
    let busy = engine.create_session(selection.clone()).unwrap().session_id;
    let idle = engine.create_session(selection.clone()).unwrap().session_id;
    // The runs wait for model answers the test has not sent yet.
    start(engine, &selection, busy, "busy").await.expect("run");

    let scheduled = engine
        .schedule_restart(RestartWhen::Idle)
        .expect("scheduled");
    assert!(scheduled.scheduled);
    assert_eq!(scheduled.active_runs, 1);
    wait_for_status(
        engine,
        RestartStatus {
            when: RestartWhen::Idle,
            active_runs: 1,
            background_calls: 0,
            due: false,
        },
    )
    .await;
    let again = engine.schedule_restart(RestartWhen::Idle).expect("again");
    assert!(!again.scheduled, "a second call changes the scheduled one");

    // A new run starts while the restart waits, and the restart waits for
    // it too.
    start_with(engine, &selection, idle, "meanwhile", "started meanwhile")
        .await
        .expect("a new run starts while the restart waits");
    wait_for_status(
        engine,
        RestartStatus {
            when: RestartWhen::Idle,
            active_runs: 2,
            background_calls: 0,
            due: false,
        },
    )
    .await;
    // Input for the running session queues, and starts when its run ends.
    let queued = engine
        .submit_input(
            SessionSubmitInputParams {
                session_id: busy,
                content: "queued meanwhile".into(),
                delivery: InputDelivery::Queue,
                selection: None,
                client_run_id: ClientRunId::new("queued").unwrap(),
                reset_fallback: false,
            },
            origin(),
        )
        .await
        .expect("queued");
    assert!(
        matches!(queued, SessionSubmitInputResult::Queued { .. }),
        "{queued:?}"
    );

    responses
        .send(MatchedScriptedResponse::last_message_contains(
            "wait for the model",
            scripted_text_body("done"),
        ))
        .unwrap();
    let busy_runs = |engine: &Engine| {
        engine
            .inner
            .store
            .get(busy)
            .unwrap()
            .log
            .events()
            .iter()
            .filter(|event| matches!(event.payload, EventPayload::RunStarted { .. }))
            .count()
    };
    with_watchdog("the queued input to start", async {
        while busy_runs(engine) < 2 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(!called.load(Ordering::SeqCst), "two runs are still active");

    for content in ["started meanwhile", "queued meanwhile"] {
        responses
            .send(MatchedScriptedResponse::last_message_contains(
                content,
                scripted_text_body("done"),
            ))
            .unwrap();
    }
    wait_for_restart(&called).await;
    wait_for_status(
        engine,
        RestartStatus {
            when: RestartWhen::Idle,
            active_runs: 0,
            background_calls: 0,
            due: true,
        },
    )
    .await;
    // Once the restart is due, a new run waits for the restarted daemon.
    let refused = start(engine, &selection, idle, "refused")
        .await
        .expect_err("the restart is due");
    assert!(matches!(refused, EngineError::Restarting), "{refused}");
    wait_for_session_not_running(engine, busy).await;
    assert_eq!(busy_runs(engine), 2);
    fixture.engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn now_overrides_an_idle_restart_and_the_shutdown_interrupts_the_run() {
    let (endpoint, _responses, _captured) = scripted_channel_server(1).await;
    let (fixture, selection) = custom_fixture_with_endpoint(&endpoint);
    let engine = &fixture.engine;
    let called = record_restart(engine);
    let session = engine.create_session(selection.clone()).unwrap().session_id;
    let run = start(engine, &selection, session, "busy")
        .await
        .expect("run");

    engine.schedule_restart(RestartWhen::Idle).expect("idle");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!called.load(Ordering::SeqCst));
    let now = engine.schedule_restart(RestartWhen::Now).expect("now");
    assert!(!now.scheduled);
    assert_eq!(now.active_runs, 1);
    wait_for_restart(&called).await;
    // `idle` never wins back over `now`.
    engine
        .schedule_restart(RestartWhen::Idle)
        .expect("idle again");
    wait_for_status(
        engine,
        RestartStatus {
            when: RestartWhen::Now,
            active_runs: 1,
            background_calls: 0,
            due: true,
        },
    )
    .await;

    // The daemon then shuts down, which ends the run as interrupted.
    engine.shutdown().await;
    let projection = engine.inner.store.get(session).unwrap();
    assert!(
        projection
            .log
            .events()
            .iter()
            .any(|event| event.run_id == Some(run)
                && matches!(
                    &event.payload,
                    EventPayload::RunFinished {
                        outcome: cookie_agent_protocol::Open::Known(
                            cookie_agent_protocol::RunOutcome::Cancelled { .. }
                        )
                    }
                )),
        "the run ends cancelled"
    );
    // The restarted engine finds nothing running.
    let restarted = reopen_engine(&fixture);
    let session = restarted.resume(session).await.expect("resumed");
    assert_ne!(session.status, SessionStatus::Running);
    restarted.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_idle_restart_waits_for_a_background_call_that_outlives_its_run() {
    let run = super::background_tasks::start_job_run(
        serde_json::json!({"command": "finish", "background": true}),
        vec![
            MatchedScriptedResponse::last_message_role("tool", scripted_text_body("waiting")),
            MatchedScriptedResponse::last_message_contains(
                "background_result",
                scripted_text_body("noted"),
            ),
        ],
        3,
    )
    .await;
    let engine = &run.fixture.engine;
    let called = record_restart(engine);
    with_watchdog("detach point", run.control.detachable.notified()).await;
    // The run ends while its call goes on in the background.
    wait_for_run_inactive(engine, run.run_id).await;

    let scheduled = engine
        .schedule_restart(RestartWhen::Idle)
        .expect("scheduled");
    assert_eq!((scheduled.active_runs, scheduled.background_calls), (0, 1));
    wait_for_status(
        engine,
        RestartStatus {
            when: RestartWhen::Idle,
            active_runs: 0,
            background_calls: 1,
            due: false,
        },
    )
    .await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(
        !called.load(Ordering::SeqCst),
        "the background call is still running"
    );

    run.control.release.notify_one();
    wait_for_restart(&called).await;
    // Its result is kept for the restarted daemon's run.
    let projection = engine.inner.store.get(run.session_id).unwrap();
    assert!(super::background_tasks::background_result(&projection).is_some());
    run.fixture.engine.shutdown().await;
}
