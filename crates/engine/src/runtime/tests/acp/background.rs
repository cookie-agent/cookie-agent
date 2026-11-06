//! The ACP agent's commands still running after their calls ended: listed
//! beside cookie's background calls, kept alive, and gone when they exit.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

use super::*;

/// Sets how long an exited command waits for the agent to follow up.
fn set_settle_window(engine: &Engine, window: Duration) {
    *engine.inner.acp.settle_window.lock().unwrap() = Some(window);
}

/// Installs a restart hook that records being called.
fn record_restart(engine: &Engine) -> Arc<AtomicBool> {
    let called = Arc::new(AtomicBool::new(false));
    let hook = Arc::clone(&called);
    engine.set_restart_hook(Box::new(move || hook.store(true, Ordering::SeqCst)));
    called
}

/// Whether the fixture agent's process runs.
fn agent_running(engine: &Engine) -> bool {
    let key = super::super::super::acp::process::ProcessKey {
        runtime: "local".to_owned(),
        agent: "fixture".to_owned(),
    };
    engine.inner.acp.processes.running(&key).is_some()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_command_the_agent_left_running_is_background_work_until_it_exits() {
    let acp = acp_fixture(&[], &[]).await;
    let engine = acp.engine();
    let session = engine.create_session(selection()).unwrap().session_id;
    // The command runs until the test is done looking at it.
    let release = acp.marker("release");
    let command = format!("sleep {}", release.display());
    let outcome = turn(
        engine,
        session,
        "first",
        &format!("background {}", release.display()),
    )
    .await;
    assert_eq!(final_text(&outcome), "started");
    let calls = engine.background_calls(session);
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert_eq!(calls[0].job, "term-bg");
    assert_eq!(calls[0].tool, "Bash");
    assert_eq!(calls[0].display.as_deref(), Some(command.as_str()));
    assert!(calls[0].agent_task);
    // Only the agent stops it.
    let refused = engine
        .stop_background_as_user(session, "term-bg")
        .await
        .expect_err("cookie cannot stop it");
    assert!(
        refused.to_string().contains("agent's own command"),
        "{refused}"
    );
    // It keeps its session resident, and an idle restart waits for it.
    assert!(engine.acp_background_running(session));
    assert_eq!(engine.acp_background_count(), 1);
    // The session waits on it, as on a background call of cookie's.
    assert_eq!(
        engine.get_session(session).unwrap().waiting_on,
        [cookie_agent_protocol::WaitingOn::BackgroundCall]
    );
    let started = calls[0].clone();
    std::fs::write(&release, "").unwrap();
    // Its exit, between runs, ends it, and the session records the end
    // outside any run, once.
    let ended = || {
        events(engine, session)
            .into_iter()
            .filter_map(|event| match event.payload {
                EventPayload::AcpSessionUpdated { update } => {
                    update.background_ended.map(|ended| (event.run_id, ended))
                }
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    with_watchdog("the command's exit", async {
        while !engine.background_calls(session).is_empty() || ended().is_empty() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    assert!(!engine.acp_background_running(session));
    // The agent may yet follow up, so the end only settles for now.
    assert!(engine.acp_background_pending(session));
    assert_eq!(engine.acp_background_count(), 1);
    assert_eq!(
        engine.get_session(session).unwrap().waiting_on,
        [cookie_agent_protocol::WaitingOn::BackgroundCall]
    );
    let ended = ended();
    assert_eq!(ended.len(), 1, "{ended:?}");
    let (run, ended) = &ended[0];
    assert_eq!(*run, None);
    assert_eq!(ended.tool_call_id, started.tool_call_id);
    assert_eq!(ended.tool, "Bash");
    assert_eq!(ended.display.as_deref(), Some(command.as_str()));
    assert_eq!(ended.started_at, started.started_at);
    assert_eq!(ended.exit_code, Some(0));
    assert_eq!(ended.signal, None);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_running_or_settling_command_keeps_the_agents_process_from_idling_out() {
    let acp = acp_fixture_with(&[], &[], "[acp]\nidle_shutdown_ms = 200\n").await;
    let engine = acp.engine();
    set_settle_window(engine, Duration::from_secs(4));
    let session = engine.create_session(selection()).unwrap().session_id;
    let release = acp.marker("release");
    turn(
        engine,
        session,
        "first",
        &format!("background {}", release.display()),
    )
    .await;
    // Well past the idle time, the command still runs: so does the agent.
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert!(
        agent_running(engine),
        "the agent idled out under its command"
    );
    // Once it exits it still may wake the agent, which stays.
    std::fs::write(&release, "").unwrap();
    with_watchdog("the command's exit", async {
        while engine.acp_background_running(session) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert!(agent_running(engine), "the agent idled out while settling");
    // Once it settled with no follow-up, the agent idles out, and nothing
    // holds the session.
    with_watchdog("the agent idling out", async {
        while agent_running(engine) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    assert!(!engine.acp_background_pending(session));
    assert_eq!(engine.acp_background_count(), 0);
    assert!(engine.get_session(session).unwrap().waiting_on.is_empty());
}

/// A restart waiting on a command that exited holds until the agent's
/// follow-up has run; it would otherwise end the agent as it wakes.
#[tokio::test(flavor = "multi_thread")]
async fn an_idle_restart_waits_for_the_agent_to_follow_up_on_a_command() {
    let acp = acp_fixture(&[], &[]).await;
    let engine = acp.engine();
    let restarted = record_restart(engine);
    let session = engine.create_session(selection()).unwrap().session_id;
    let release = acp.marker("release");
    turn(
        engine,
        session,
        "first",
        &format!("background-wake {} 2", release.display()),
    )
    .await;
    engine
        .schedule_restart(cookie_agent_protocol::RestartWhen::Idle)
        .unwrap();
    std::fs::write(&release, "").unwrap();
    with_watchdog("the command's exit", async {
        while engine.acp_background_running(session) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    // The exit left nothing running, and the agent has not woken yet.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        !restarted.load(Ordering::SeqCst),
        "restarted before the wake"
    );
    // The restart comes after the wake's run, which got to say its part.
    with_watchdog("the restart", async {
        while !restarted.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    let woke = events(engine, session).into_iter().any(|event| {
        matches!(
            event.payload,
            EventPayload::RunFinished {
                outcome: cookie_agent_protocol::Open::Known(RunOutcome::Completed {
                    final_text: Some(text),
                }),
                ..
            } if text == "woke"
        )
    });
    assert!(woke, "the wake's run finished first");
    assert!(!engine.acp_background_pending(session));
}

/// With no follow-up, the restart goes once the command stopped settling.
#[tokio::test(flavor = "multi_thread")]
async fn an_idle_restart_waits_out_a_command_nothing_followed_up() {
    let acp = acp_fixture(&[], &[]).await;
    let engine = acp.engine();
    set_settle_window(engine, Duration::from_secs(2));
    let restarted = record_restart(engine);
    let session = engine.create_session(selection()).unwrap().session_id;
    let release = acp.marker("release");
    turn(
        engine,
        session,
        "first",
        &format!("background {}", release.display()),
    )
    .await;
    engine
        .schedule_restart(cookie_agent_protocol::RestartWhen::Idle)
        .unwrap();
    std::fs::write(&release, "").unwrap();
    with_watchdog("the command's exit", async {
        while engine.acp_background_running(session) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    assert!(engine.background_calls(session).is_empty());
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        !restarted.load(Ordering::SeqCst),
        "restarted while settling"
    );
    with_watchdog("the restart", async {
        while !restarted.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    assert!(!engine.acp_background_pending(session));
}

/// A message of the user's after a command ended: the run holds what the
/// command held, and takes in any follow-up.
#[tokio::test(flavor = "multi_thread")]
async fn a_run_ends_the_settling_of_a_command() {
    let acp = acp_fixture(&[], &[]).await;
    let engine = acp.engine();
    let session = engine.create_session(selection()).unwrap().session_id;
    let release = acp.marker("release");
    turn(
        engine,
        session,
        "first",
        &format!("background {}", release.display()),
    )
    .await;
    std::fs::write(&release, "").unwrap();
    with_watchdog("the command's exit", async {
        while engine.acp_background_running(session) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    assert!(engine.acp_background_pending(session));
    turn(engine, session, "second", "say hello").await;
    assert!(!engine.acp_background_pending(session));
    assert!(engine.get_session(session).unwrap().waiting_on.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_command_that_ends_during_a_run_is_recorded_in_that_run() {
    let acp = acp_fixture(&[], &[]).await;
    let engine = acp.engine();
    let session = engine.create_session(selection()).unwrap().session_id;
    // The command exits once the next prompt arrives, while its run waits.
    turn(engine, session, "first", "background next").await;
    let run = start(engine, session, "second", "steer-wait 3").await;
    finished(engine, session, run).await;
    let ended = events(engine, session)
        .into_iter()
        .filter_map(|event| match event.payload {
            EventPayload::AcpSessionUpdated { update } => {
                update.background_ended.map(|ended| (event.run_id, ended))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(ended.len(), 1, "{ended:?}");
    assert_eq!(ended[0].0, Some(run));
    assert_eq!(ended[0].1.display.as_deref(), Some("sleep next"));
    assert!(engine.background_calls(session).is_empty());
}

/// The next settle of `signals`: its root and the reply it ended with.
async fn next_settle(
    signals: &mut tokio::sync::broadcast::Receiver<crate::PushSignal>,
) -> (cookie_agent_protocol::SessionId, Option<String>) {
    loop {
        let signal = with_watchdog("a settle", async {
            signals.recv().await.expect("push signal")
        })
        .await;
        if let crate::PushSignal::Settled(settled) = signal {
            let text = match settled.outcome {
                crate::SettledOutcome::Completed { final_text } => final_text,
                crate::SettledOutcome::Failed { error } => panic!("failed: {error}"),
            };
            return (settled.root, text);
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_turn_that_left_a_command_running_is_never_pushed() {
    let acp = acp_fixture(&[], &[]).await;
    let engine = acp.engine();
    let mut signals = engine.subscribe_push_signals();
    let session = engine.create_session(selection()).unwrap().session_id;
    let release = acp.marker("release");
    turn(
        engine,
        session,
        "first",
        &format!("background {}", release.display()),
    )
    .await;
    // The turn ended with the command running: the session could still
    // wake, as with a background call of cookie's.
    std::fs::write(&release, "").unwrap();
    with_watchdog("the command's exit", async {
        while engine.acp_background_running(session) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    // The agent never follows up, and the turn is not pushed once the
    // command is gone either, however long after.
    let late = tokio::time::timeout(Duration::from_secs(6), signals.recv()).await;
    assert!(late.is_err(), "a settle for the turn: {late:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_agent_that_follows_up_settles_once_after_it() {
    let acp = acp_fixture(&[], &[]).await;
    let engine = acp.engine();
    let mut signals = engine.subscribe_push_signals();
    let session = engine.create_session(selection()).unwrap().session_id;
    // The agent follows up a while after its command ended, as Claude Code
    // can take seconds to.
    let release = acp.marker("release");
    turn(
        engine,
        session,
        "first",
        &format!("background-wake {} 6", release.display()),
    )
    .await;
    std::fs::write(&release, "").unwrap();
    // One settle, for the follow-up, none for the turn that left the
    // command running.
    let (root, text) = next_settle(&mut signals).await;
    assert_eq!(root, session);
    assert_eq!(text.as_deref(), Some("woke"));
    let late = tokio::time::timeout(Duration::from_millis(500), signals.recv()).await;
    assert!(late.is_err(), "a second settle: {late:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_command_a_cancel_left_running_is_background_work_until_it_exits() {
    let acp = acp_fixture(&[], &[]).await;
    let engine = acp.engine();
    let session = engine.create_session(selection()).unwrap().session_id;
    // The command runs until the test is done looking at it.
    let release = acp.marker("release");
    let run = start(
        engine,
        session,
        "left",
        &format!("cancel-leaves {}", release.display()),
    )
    .await;
    await_event(engine, session, "the command started", |event| {
        matches!(&event.payload, EventPayload::ToolCallStarted { .. })
    })
    .await;
    engine.cancel_run(run, origin()).await.expect("cancel");
    let outcome = finished(engine, session, run).await;
    assert!(
        matches!(outcome, RunOutcome::Cancelled { .. }),
        "{outcome:?}"
    );
    let calls = engine.background_calls(session);
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert_eq!(calls[0].job, "term-left");
    assert_eq!(
        calls[0].display.as_deref(),
        Some(format!("sleep {}", release.display()).as_str()),
        "unwrapped"
    );
    assert!(calls[0].agent_task);
    std::fs::write(&release, "").unwrap();
    tokio::time::timeout(Duration::from_secs(30), async {
        while !engine.background_calls(session).is_empty() {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("its exit ends it");
}
