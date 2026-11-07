//! An ACP session's window (`docs/site/specs/forever-sessions.md` sections 3.2,
//! 3.5, 5.4, and 7), against the scripted agent. Every store append of these
//! compares the log with a reopening of its file at each committed turn, as
//! every ACP test's does.

use cookie_agent_protocol::SessionStatus;

use super::*;

/// The log's window and the passes asked of it since the last time.
fn passes(engine: &Engine, session: SessionId) -> Vec<String> {
    engine
        .inner
        .store
        .log(session)
        .unwrap()
        .take_history_calls_for_test()
}

/// Runs one prompt, waiting for it without reading the log as a client does
/// (`turn` and `wait_for_session_not_running` subscribe to it).
async fn quiet_turn(engine: &Engine, session: SessionId, key: &str, text: &str) {
    start(engine, session, key, text).await;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(60);
    while engine.inner.store.get_resident(session).unwrap().status == SessionStatus::Running {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the run did not end"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_normal_acp_turn_asks_for_no_pass_over_its_log() {
    // The reference folds of a store append read the whole log: they would
    // count, and are what this keeps a turn from doing.
    crate::session::skip_reference_fold();
    let acp = acp_fixture(&[], &[]).await;
    let engine = acp.engine();
    let session = engine.create_session(selection()).unwrap().session_id;
    // The session's first turn publishes its log and loads its tree from what
    // it holds: nothing yet left its window.
    quiet_turn(engine, session, "first", "say one").await;
    let first = passes(engine, session);
    assert!(
        first.iter().all(|call| !call.starts_with("file")),
        "{first:#?}"
    );
    let mut calls = Vec::new();
    for (key, text) in [("two", "work"), ("three", "say three"), ("four", "work")] {
        quiet_turn(engine, session, key, text).await;
        calls.extend(passes(engine, session));
    }
    assert!(calls.is_empty(), "{calls:#?}");
    assert!(
        engine
            .inner
            .store
            .log(session)
            .unwrap()
            .window_for_test()
            .is_some(),
        "the window moved"
    );
}
