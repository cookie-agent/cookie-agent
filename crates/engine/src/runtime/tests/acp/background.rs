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
    // Its exit, between runs, ends it.
    with_watchdog("the command's exit", async {
        while !engine.background_calls(session).is_empty() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    assert!(!engine.acp_background_running(session));
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
