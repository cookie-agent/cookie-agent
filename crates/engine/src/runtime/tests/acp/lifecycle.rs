//! A cookie session's ACP sessions across forks, reverts, and deletes
//! (`docs/site/specs/acp-agents.md` section 15, lifecycle), against the
//! scripted agent, whose `recall` says which prompts its session
//! remembers. Its three ways of forking: `ACP_FIXTURE_FORK` alone forks at
//! the fork point and refuses an unknown one as invalid params; with
//! `ACP_FIXTURE_FORK_WHOLE` it ignores the fork point; without either it
//! offers no `session/fork`.

use cookie_agent_protocol::{AcpDetachReason, AcpSessionStart, EventPayload, SessionId};

use super::*;
use crate::EngineError;

const AT_MESSAGE: &[(&str, &str)] = &[("ACP_FIXTURE_FORK", "1"), ("ACP_FIXTURE_DELETE", "1")];
const WHOLE: &[(&str, &str)] = &[
    ("ACP_FIXTURE_FORK", "1"),
    ("ACP_FIXTURE_FORK_WHOLE", "1"),
    ("ACP_FIXTURE_DELETE", "1"),
];
const NO_FORK: &[(&str, &str)] = &[("ACP_FIXTURE_DELETE", "1")];

/// The sequence number of the end of `session`'s `index`th run.
fn run_end_seq(engine: &Engine, session: SessionId, index: usize) -> u64 {
    events(engine, session)
        .iter()
        .filter(|event| matches!(event.payload, EventPayload::RunFinished { .. }))
        .nth(index)
        .expect("the run ended")
        .seq
}

fn tip(engine: &Engine, session: SessionId) -> u64 {
    engine.inner.store.get(session).unwrap().meta.last_event_seq
}

/// The requests of `method` the agent received, by their params.
fn requests(acp: &AcpFixture, method: &str) -> Vec<serde_json::Value> {
    acp.received()
        .into_iter()
        .filter(|message| message["method"] == method)
        .map(|message| message["params"].clone())
        .collect()
}

fn fork_point_of(request: &serde_json::Value) -> Option<&str> {
    request["_meta"]["jetbrains"]["air"]["fork"]["messageId"].as_str()
}

/// The `session/fork` requests that were not the engine's probe.
fn forks(acp: &AcpFixture) -> Vec<serde_json::Value> {
    requests(acp, "session/fork")
        .into_iter()
        .filter(|request| {
            !fork_point_of(request).is_some_and(|id| id.starts_with("cookie-fork-probe-"))
        })
        .collect()
}

fn diagnostics(engine: &Engine, session: SessionId) -> Vec<String> {
    events(engine, session)
        .into_iter()
        .filter_map(|event| match event.payload {
            EventPayload::Diagnostic { message, .. } => Some(message),
            _ => None,
        })
        .collect()
}

/// Waits for `session` to record what the probe found.
async fn probed(engine: &Engine, session: SessionId) -> bool {
    let event = await_event(engine, session, "the fork point probe", |event| {
        matches!(&event.payload, EventPayload::AcpSessionUpdated { update } if update.fork_point.is_some())
    })
    .await;
    match event.payload {
        EventPayload::AcpSessionUpdated { update } => update.fork_point.unwrap(),
        _ => unreachable!(),
    }
}

async fn fork(engine: &Engine, session: SessionId, through_seq: u64) -> SessionId {
    engine
        .fork_session(session, through_seq, origin(), None)
        .await
        .expect("forked")
        .session_id
}

async fn revert(engine: &Engine, session: SessionId, through_seq: u64) {
    engine
        .revert_session(session, through_seq, origin(), None)
        .await
        .expect("reverted");
}

fn refusal(result: Result<impl std::fmt::Debug, EngineError>) -> String {
    match result {
        Err(EngineError::AcpUnsupported(reason)) => reason,
        other => panic!("not refused: {other:?}"),
    }
}

/// Waits for the agent to have been asked `method` `count` times.
async fn asked(acp: &AcpFixture, method: &str, count: usize) -> Vec<serde_json::Value> {
    tokio::time::timeout(std::time::Duration::from_secs(20), async {
        loop {
            let asked = requests(acp, method);
            if asked.len() >= count {
                return asked;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("the agent was not asked {method}"))
}

/// A session of the agent with `turns` completed, the probe done.
async fn session_with(engine: &Engine, turns: &[&str]) -> SessionId {
    let session = engine.create_session(selection()).unwrap().session_id;
    for (index, text) in turns.iter().enumerate() {
        turn(engine, session, &format!("turn-{index}"), text).await;
    }
    session
}

#[tokio::test(flavor = "multi_thread")]
async fn the_probe_finds_whether_the_agents_fork_takes_a_fork_point() {
    // At a message: refused as invalid params, so nothing to clean up.
    let acp = acp_fixture(AT_MESSAGE, &[]).await;
    let session = session_with(acp.engine(), &["say one"]).await;
    assert!(probed(acp.engine(), session).await);
    let probes = requests(&acp, "session/fork");
    assert_eq!(probes.len(), 1);
    assert!(
        fork_point_of(&probes[0])
            .unwrap()
            .starts_with("cookie-fork-probe-")
    );
    assert!(requests(&acp, "session/delete").is_empty());
    // A later session of the process records it at once.
    let other = session_with(acp.engine(), &["say two"]).await;
    assert_eq!(
        session_starts(acp.engine(), other)[0].fork_point,
        Some(true)
    );
    assert_eq!(session_starts(acp.engine(), other)[0].fork, Some(true));
    assert_eq!(
        requests(&acp, "session/fork").len(),
        1,
        "asked once per process"
    );

    // Whole: the probe's fork succeeds and is deleted.
    let acp = acp_fixture(WHOLE, &[]).await;
    let session = session_with(acp.engine(), &["say one"]).await;
    assert!(!probed(acp.engine(), session).await);
    let deleted = asked(&acp, "session/delete", 1).await;
    assert_ne!(
        deleted[0]["sessionId"],
        session_starts(acp.engine(), session)[0]
            .acp_session_id
            .as_str()
    );

    // No fork: nothing asked.
    let acp = acp_fixture(NO_FORK, &[]).await;
    let session = session_with(acp.engine(), &["say one"]).await;
    assert_eq!(session_starts(acp.engine(), session)[0].fork, Some(false));
    turn(acp.engine(), session, "again", "say again").await;
    assert!(requests(&acp, "session/fork").is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_fork_forks_the_agents_session_at_its_fork_point_and_the_source_keeps_its_own() {
    let acp = acp_fixture(AT_MESSAGE, &[]).await;
    let engine = acp.engine();
    let parent = session_with(engine, &["say one", "say two"]).await;
    assert!(probed(engine, parent).await);
    let parent_session = session_starts(engine, parent)[0].acp_session_id.clone();
    // Forked after the first turn, while the agent's session holds both.
    let child = fork(engine, parent, run_end_seq(engine, parent, 0)).await;
    let detached = events(engine, child)
        .into_iter()
        .find_map(|event| match event.payload {
            EventPayload::AcpSessionUpdated { update } => update.detached,
            _ => None,
        })
        .expect("the fork leaves the source's session behind");
    assert_eq!(detached.acp_session_id, parent_session);
    assert_eq!(detached.reason, AcpDetachReason::Forked);

    let outcome = turn(engine, child, "child-recall", "recall").await;
    assert_eq!(final_text(&outcome), "remembers: say one");
    let forks = forks(&acp);
    assert_eq!(forks.len(), 1);
    assert_eq!(forks[0]["sessionId"], parent_session.as_str());
    assert_eq!(fork_point_of(&forks[0]), detached.message_id.as_deref());
    let started = session_starts(engine, child);
    let forked = started.last().expect("the fork's own session");
    assert_eq!(forked.how, AcpSessionStart::Forked);
    assert_eq!(forked.forked_from.as_deref(), Some(parent_session.as_str()));
    assert_ne!(forked.acp_session_id, parent_session);

    // The fork goes on in its own session; the source in its own.
    let outcome = turn(engine, child, "child-again", "recall").await;
    assert_eq!(final_text(&outcome), "remembers: say one, recall");
    let outcome = turn(engine, parent, "parent-recall", "recall").await;
    assert_eq!(final_text(&outcome), "remembers: say one, say two");
    assert_eq!(session_starts(engine, parent).len(), 1);
    assert!(requests(&acp, "session/delete").is_empty());
    assert!(requests(&acp, "session/close").is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_whole_fork_forks_only_at_the_end_of_the_agents_conversation() {
    let acp = acp_fixture(WHOLE, &[]).await;
    let engine = acp.engine();
    let parent = session_with(engine, &["say one", "say two"]).await;
    assert!(!probed(engine, parent).await);
    let reason = refusal(
        engine
            .fork_session(parent, run_end_seq(engine, parent, 0), origin(), None)
            .await,
    );
    assert_eq!(
        reason,
        "`fixture` on `local` can't fork its own conversation at an earlier point, so the \
         session can only be forked at the end of the agent's conversation or before its \
         first reply"
    );
    // At the end: the whole conversation.
    let child = fork(engine, parent, tip(engine, parent)).await;
    let outcome = turn(engine, child, "child-recall", "recall").await;
    assert_eq!(final_text(&outcome), "remembers: say one, say two");
    assert_eq!(
        session_starts(engine, child).last().unwrap().how,
        AcpSessionStart::Forked
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_whole_fork_is_taken_when_the_session_forks_not_at_its_first_turn() {
    let acp = acp_fixture(WHOLE, &[]).await;
    let engine = acp.engine();
    let parent = session_with(engine, &["say one", "say two"]).await;
    assert!(!probed(engine, parent).await);
    let child = fork(engine, parent, tip(engine, parent)).await;
    // The agent forked as the session did.
    assert_eq!(forks(&acp).len(), 1);
    let started = session_starts(engine, child);
    let forked = started.last().expect("the fork's own session");
    assert_eq!(forked.how, AcpSessionStart::Forked);
    // The source goes on before the fork's first turn; the fork does not
    // see it.
    turn(engine, parent, "three", "say three").await;
    let outcome = turn(engine, child, "child-recall", "recall").await;
    assert_eq!(final_text(&outcome), "remembers: say one, say two");
    assert_eq!(forks(&acp).len(), 1);
    assert_eq!(
        session_starts(engine, child).last().unwrap().acp_session_id,
        forked.acp_session_id
    );
    let outcome = turn(engine, parent, "parent-recall", "recall").await;
    assert_eq!(
        final_text(&outcome),
        "remembers: say one, say two, say three"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_fork_the_agent_fails_to_take_at_once_is_refused_and_leaves_no_session() {
    let acp = acp_fixture(
        &[
            ("ACP_FIXTURE_FORK", "1"),
            ("ACP_FIXTURE_FORK_WHOLE", "1"),
            ("ACP_FIXTURE_FORK_FAIL", "1"),
        ],
        &[],
    )
    .await;
    let engine = acp.engine();
    let parent = session_with(engine, &["say one"]).await;
    assert!(!probed(engine, parent).await);
    let sessions = engine.list_sessions().len();
    let reason = refusal(
        engine
            .fork_session(parent, tip(engine, parent), origin(), None)
            .await,
    );
    assert!(
        reason.starts_with(
            "`fixture` on `local` could not fork its own conversation for the new session: "
        ),
        "{reason}"
    );
    assert!(reason.contains("the fork failed"), "{reason}");
    assert_eq!(engine.list_sessions().len(), sessions);
}

#[tokio::test(flavor = "multi_thread")]
async fn without_session_fork_a_fork_after_the_agents_first_reply_is_refused() {
    let acp = acp_fixture(NO_FORK, &[]).await;
    let engine = acp.engine();
    let parent = session_with(engine, &["say one", "say two"]).await;
    let reason = refusal(
        engine
            .fork_session(parent, tip(engine, parent), origin(), None)
            .await,
    );
    assert_eq!(
        reason,
        "`fixture` on `local` can't fork its own conversation, so the session can't be forked \
         after the agent's first reply"
    );
    // Before its first reply: the fork starts fresh.
    let first_input = events(engine, parent)
        .iter()
        .find(|event| matches!(event.payload, EventPayload::UserInputSubmitted { .. }))
        .expect("the first input")
        .seq;
    let child = fork(engine, parent, first_input).await;
    let outcome = turn(engine, child, "child-recall", "recall").await;
    assert_eq!(final_text(&outcome), "remembers: ");
    let started = session_starts(engine, child);
    assert_eq!(started.len(), 1);
    assert_eq!(started[0].how, AcpSessionStart::New);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_revert_forks_the_agents_session_at_the_revert_point_and_deletes_the_old_one() {
    let acp = acp_fixture(AT_MESSAGE, &[]).await;
    let engine = acp.engine();
    let session = session_with(engine, &["say one", "say two", "say three"]).await;
    assert!(probed(engine, session).await);
    let old = session_starts(engine, session)[0].acp_session_id.clone();
    revert(engine, session, run_end_seq(engine, session, 1)).await;
    let detached = events(engine, session)
        .into_iter()
        .find_map(|event| match event.payload {
            EventPayload::AcpSessionUpdated { update } => update.detached,
            _ => None,
        })
        .expect("the revert leaves the agent's session behind");
    assert_eq!(detached.acp_session_id, old);
    assert_eq!(detached.reason, AcpDetachReason::Reverted);

    // The agent forgot the reverted turn.
    let outcome = turn(engine, session, "recall", "recall").await;
    assert_eq!(final_text(&outcome), "remembers: say one, say two");
    let forks = forks(&acp);
    assert_eq!(forks.len(), 1);
    assert_eq!(forks[0]["sessionId"], old.as_str());
    assert_eq!(fork_point_of(&forks[0]), detached.message_id.as_deref());
    let started = session_starts(engine, session);
    let forked = started.last().expect("the forked session");
    assert_eq!(forked.how, AcpSessionStart::Forked);
    assert_eq!(forked.forked_from.as_deref(), Some(old.as_str()));
    // The old session is closed and deleted, after the fork.
    let methods = acp.received_methods();
    let fork_at = methods.iter().rposition(|method| method == "session/fork");
    let delete_at = methods.iter().position(|method| method == "session/delete");
    assert!(fork_at < delete_at, "{methods:?}");
    assert_eq!(
        requests(&acp, "session/delete")[0]["sessionId"],
        old.as_str()
    );
    assert!(
        diagnostics(engine, session)
            .iter()
            .any(|message| message.contains("continues from the revert point")),
        "{:?}",
        diagnostics(engine, session)
    );
    let outcome = turn(engine, session, "again", "recall").await;
    assert_eq!(final_text(&outcome), "remembers: say one, say two, recall");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_revert_the_agent_cannot_fork_to_is_refused_and_changes_nothing() {
    for (env, expected) in [
        (
            WHOLE,
            "`fixture` on `local` can't rewind its own conversation",
        ),
        (
            NO_FORK,
            "`fixture` on `local` can't rewind its own conversation",
        ),
    ] {
        let acp = acp_fixture(env, &[]).await;
        let engine = acp.engine();
        let session = session_with(engine, &["say one", "say two"]).await;
        if env == WHOLE {
            assert!(!probed(engine, session).await);
        }
        let before = tip(engine, session);
        let reason = refusal(
            engine
                .revert_session(session, run_end_seq(engine, session, 0), origin(), None)
                .await,
        );
        assert_eq!(
            reason,
            format!("{expected}, so the session can't be reverted past the agent's first reply")
        );
        assert_eq!(tip(engine, session), before);
        // After the end of the agent's conversation nothing is cut.
        revert(engine, session, tip(engine, session)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_revert_before_the_agent_was_asked_is_refused_until_it_is() {
    // The probe's answer takes a while.
    let acp = acp_fixture(
        &[
            ("ACP_FIXTURE_FORK", "1"),
            ("ACP_FIXTURE_DELETE", "1"),
            ("ACP_FIXTURE_FORK_DELAY", "2"),
        ],
        &[],
    )
    .await;
    let engine = acp.engine();
    let session = session_with(engine, &["say one", "say two"]).await;
    let reason = refusal(
        engine
            .revert_session(session, run_end_seq(engine, session, 0), origin(), None)
            .await,
    );
    assert_eq!(
        reason,
        "cookie has not yet checked whether `fixture` on `local` can rewind its own \
         conversation (it checks after the agent's next reply), so the session can't be \
         reverted past the agent's first reply yet"
    );
    assert!(probed(engine, session).await);
    revert(engine, session, run_end_seq(engine, session, 0)).await;
    let outcome = turn(engine, session, "recall", "recall").await;
    assert_eq!(final_text(&outcome), "remembers: say one");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_revert_before_the_agents_first_reply_deletes_its_session_and_starts_fresh() {
    let acp = acp_fixture(NO_FORK, &[]).await;
    let engine = acp.engine();
    let session = session_with(engine, &["say one"]).await;
    let old = session_starts(engine, session)[0].acp_session_id.clone();
    let first_input = events(engine, session)
        .iter()
        .find(|event| matches!(event.payload, EventPayload::UserInputSubmitted { .. }))
        .expect("the first input")
        .seq;
    revert(engine, session, first_input - 1).await;
    let deleted = asked(&acp, "session/delete", 1).await;
    assert_eq!(deleted[0]["sessionId"], old.as_str());

    let outcome = turn(engine, session, "recall", "recall").await;
    assert_eq!(final_text(&outcome), "remembers: ");
    let started = session_starts(engine, session);
    assert_eq!(started.len(), 1);
    assert_eq!(started[0].how, AcpSessionStart::New);
    assert_ne!(started[0].acp_session_id, old);
}

#[tokio::test(flavor = "multi_thread")]
async fn reverting_a_fork_into_its_sources_turns_leaves_the_sources_session_alone() {
    let acp = acp_fixture(AT_MESSAGE, &[]).await;
    let engine = acp.engine();
    let parent = session_with(engine, &["say one", "say two"]).await;
    assert!(probed(engine, parent).await);
    let source = session_starts(engine, parent)[0].acp_session_id.clone();
    let child = fork(engine, parent, tip(engine, parent)).await;
    turn(engine, child, "three", "say three").await;
    let own = session_starts(engine, child)
        .last()
        .unwrap()
        .acp_session_id
        .clone();
    assert_ne!(own, source);

    // Back into the turns the fork copied: the fork's own session goes,
    // the source's stays.
    revert(engine, child, run_end_seq(engine, child, 0)).await;
    let deleted = asked(&acp, "session/delete", 1).await;
    assert_eq!(deleted.len(), 1);
    assert_eq!(deleted[0]["sessionId"], own.as_str());
    let outcome = turn(engine, child, "recall", "recall").await;
    assert_eq!(final_text(&outcome), "remembers: say one");
    assert_eq!(forks(&acp).last().unwrap()["sessionId"], source.as_str());
    assert_eq!(requests(&acp, "session/delete").len(), 1);
    let outcome = turn(engine, parent, "parent", "recall").await;
    assert_eq!(final_text(&outcome), "remembers: say one, say two");
}

async fn delete(engine: &Engine, session: SessionId) {
    let tip = engine.inner.store.get(session).unwrap().meta.last_event_seq;
    engine.delete_session(session, tip).await.expect("deleted");
}

#[tokio::test(flavor = "multi_thread")]
async fn deleting_a_session_deletes_its_agents_sessions_but_not_its_sources() {
    let acp = acp_fixture(
        &[
            ("ACP_FIXTURE_FORK", "1"),
            ("ACP_FIXTURE_DELETE", "1"),
            ("ACP_FIXTURE_RESUME", "1"),
        ],
        &[],
    )
    .await;
    let engine = acp.engine();
    // A live session closes, then goes.
    let live = engine.create_session(selection()).unwrap().session_id;
    turn(engine, live, "one", "say one").await;
    let live_id = session_starts(engine, live)[0].acp_session_id.clone();
    delete(engine, live).await;
    let methods = acp.received_methods();
    let close_at = methods.iter().position(|method| method == "session/close");
    let delete_at = methods.iter().position(|method| method == "session/delete");
    assert!(close_at.is_some() && close_at < delete_at, "{methods:?}");
    assert_eq!(
        requests(&acp, "session/delete")[0]["sessionId"],
        live_id.as_str()
    );

    // A fork's deletion takes its own session, not its source's, even
    // when no process runs.
    let parent = engine.create_session(selection()).unwrap().session_id;
    turn(engine, parent, "parent", "say two").await;
    let source = session_starts(engine, parent)[0].acp_session_id.clone();
    let tip = engine.inner.store.get(parent).unwrap().meta.last_event_seq;
    let child = fork(engine, parent, tip).await;
    turn(engine, child, "child", "say three").await;
    let own = session_starts(engine, child)
        .last()
        .unwrap()
        .acp_session_id
        .clone();
    super::stop_process(engine).await;
    delete(engine, child).await;
    let deleted = requests(&acp, "session/delete");
    assert_eq!(deleted.len(), 2, "{deleted:?}");
    assert_eq!(deleted[1]["sessionId"], own.as_str());
    assert!(
        deleted
            .iter()
            .all(|request| request["sessionId"] != source.as_str())
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn paging_a_session_out_closes_its_agents_session_without_deleting_it() {
    let acp = acp_fixture(
        &[("ACP_FIXTURE_DELETE", "1"), ("ACP_FIXTURE_RESUME", "1")],
        &[],
    )
    .await;
    let engine = acp.engine();
    let session = engine.create_session(selection()).unwrap().session_id;
    turn(engine, session, "one", "say one").await;
    let evicted = engine
        .evict_idle_roots_with(std::time::Duration::ZERO)
        .await
        .expect("idle janitor pass");
    assert_eq!(evicted, vec![session]);
    assert_eq!(requests(&acp, "session/close").len(), 1);
    assert!(requests(&acp, "session/delete").is_empty());
    let outcome = turn(engine, session, "recall", "recall").await;
    assert_eq!(final_text(&outcome), "remembers: say one");
}
