//! `session.compact` on an ACP session: the agent's own `compact`
//! command, or a refusal.

use cookie_agent_protocol::EventPayload;

use super::*;

#[tokio::test(flavor = "multi_thread")]
async fn without_a_compact_command_compaction_is_refused() {
    let acp = acp_fixture(&[("ACP_FIXTURE_NO_COMPACT", "1")], &[]).await;
    let engine = acp.engine();
    let session = engine.create_session(selection()).unwrap().session_id;
    turn(engine, session, "one", "say one").await;
    let error = engine
        .compact_session_result(session, None, origin())
        .await
        .expect_err("refused");
    assert_eq!(error.to_string(), "`fixture` does not offer compaction");
    assert!(!events(engine, session).iter().any(|event| matches!(
        event.payload,
        EventPayload::ContextCheckpointCommitted { .. }
    )));
}

#[tokio::test(flavor = "multi_thread")]
async fn compaction_goes_to_the_agent_as_its_compact_command() {
    let acp = acp_fixture(&[], &[]).await;
    let engine = acp.engine();
    let session = engine.create_session(selection()).unwrap().session_id;
    turn(engine, session, "one", "say one").await;
    let result = engine
        .compact_session_result(session, Some("the parser"), origin())
        .await
        .expect("forwarded");
    assert!(result.compacted);
    assert_eq!(
        prompts(&acp).last().unwrap().last().map(String::as_str),
        Some("/compact the parser")
    );
    wait_for_session_not_running(engine, session).await;
    assert!(!events(engine, session).iter().any(|event| matches!(
        event.payload,
        EventPayload::ContextCheckpointCommitted { .. }
    )));
}

/// A revert between the compaction's checks and its run reuses sequences, so
/// the run's end is looked for from the run's own start, not from the tip the
/// compaction saw first: it is found, and the compaction ends.
#[tokio::test(flavor = "multi_thread")]
async fn a_compaction_after_a_racing_revert_finds_its_run_end() {
    let acp = acp_fixture(&[("ACP_FIXTURE_FORK", "1")], &[]).await;
    let engine = acp.engine();
    let session = engine.create_session(selection()).unwrap().session_id;
    for round in 0..3 {
        turn(engine, session, &format!("turn-{round}"), "work").await;
    }
    let first_end = events(engine, session)
        .iter()
        .find(|event| matches!(event.payload, EventPayload::RunFinished { .. }))
        .expect("the first run's end")
        .seq;
    let (reached, release) = engine.install_acp_compaction_start_hook();
    let compacting = engine.clone();
    let compaction = tokio::spawn(async move {
        compacting
            .compact_session_result(session, None, origin())
            .await
    });
    with_watchdog("the compaction's checks", reached)
        .await
        .expect("held");
    engine
        .revert_session(session, first_end, origin(), None)
        .await
        .expect("reverted");
    release.notify_one();
    let result = tokio::time::timeout(std::time::Duration::from_secs(20), compaction)
        .await
        .expect("the compaction ends")
        .expect("the task")
        .expect("forwarded");
    assert!(result.compacted);
}
