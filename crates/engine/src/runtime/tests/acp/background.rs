//! The ACP agent's commands still running after their calls ended: listed
//! beside cookie's background calls, kept alive, and gone when they exit.

use std::time::Duration;

use super::*;

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
    let outcome = turn(engine, session, "first", "background 1.5").await;
    assert_eq!(final_text(&outcome), "started");
    let calls = engine.background_calls(session);
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert_eq!(calls[0].job, "term-bg");
    assert_eq!(calls[0].tool, "Bash");
    assert_eq!(calls[0].display.as_deref(), Some("sleep 1.5"));
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
    assert!(engine.get_session(session).unwrap().waiting_on.is_empty());
    let ended = ended();
    assert_eq!(ended.len(), 1, "{ended:?}");
    let (run, ended) = &ended[0];
    assert_eq!(*run, None);
    assert_eq!(ended.tool_call_id, started.tool_call_id);
    assert_eq!(ended.tool, "Bash");
    assert_eq!(ended.display.as_deref(), Some("sleep 1.5"));
    assert_eq!(ended.started_at, started.started_at);
    assert_eq!(ended.exit_code, Some(0));
    assert_eq!(ended.signal, None);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_running_command_keeps_the_agents_process_from_idling_out() {
    let acp = acp_fixture_with(&[], &[], "[acp]\nidle_shutdown_ms = 200\n").await;
    let engine = acp.engine();
    let session = engine.create_session(selection()).unwrap().session_id;
    turn(engine, session, "first", "background 1.5").await;
    // Well past the idle time, the command still runs: so does the agent.
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert!(
        agent_running(engine),
        "the agent idled out under its command"
    );
    // Once it exits, the agent idles out.
    with_watchdog("the agent idling out", async {
        while agent_running(engine) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_command_that_ends_during_a_run_is_recorded_in_that_run() {
    let acp = acp_fixture(&[], &[]).await;
    let engine = acp.engine();
    let session = engine.create_session(selection()).unwrap().session_id;
    turn(engine, session, "first", "background 1").await;
    // The next run waits past the command's exit.
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
    assert_eq!(ended[0].1.display.as_deref(), Some("sleep 1"));
    assert!(engine.background_calls(session).is_empty());
}
