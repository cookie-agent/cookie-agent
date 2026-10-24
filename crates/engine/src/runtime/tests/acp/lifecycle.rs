//! A cookie session's ACP sessions across forks and reverts
//! (`docs/site/specs/acp-agents.md` section 15, lifecycle), against the
//! scripted agent, whose `recall` says which prompts its session
//! remembers.

use cookie_agent_protocol::{AcpDetachReason, AcpSessionStart, EventPayload, SessionId};

use super::*;

/// The sequence number of the end of `session`'s `index`th run.
fn run_end_seq(engine: &Engine, session: SessionId, index: usize) -> u64 {
    events(engine, session)
        .iter()
        .filter(|event| matches!(event.payload, EventPayload::RunFinished { .. }))
        .nth(index)
        .expect("the run ended")
        .seq
}

/// The methods the agent was asked, with the session each named.
fn requests(acp: &AcpFixture, method: &str) -> Vec<serde_json::Value> {
    acp.received()
        .into_iter()
        .filter(|message| message["method"] == method)
        .map(|message| message["params"].clone())
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

async fn fork(engine: &Engine, session: SessionId, through_seq: u64) -> SessionId {
    engine
        .fork_session(session, through_seq, origin(), None)
        .await
        .expect("forked")
        .session_id
}

#[tokio::test(flavor = "multi_thread")]
async fn a_fork_forks_the_agents_session_at_its_fork_point_and_the_source_keeps_its_own() {
    let acp = acp_fixture(&[("ACP_FIXTURE_FORK", "1")], &[]).await;
    let engine = acp.engine();
    let parent = engine.create_session(selection()).unwrap().session_id;
    turn(engine, parent, "one", "say one").await;
    turn(engine, parent, "two", "say two").await;
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
    let forks = requests(&acp, "session/fork");
    assert_eq!(forks.len(), 1);
    assert_eq!(forks[0]["sessionId"], parent_session.as_str());
    assert_eq!(
        forks[0]["_meta"]["jetbrains"]["air"]["fork"]["messageId"],
        detached.message_id.clone().expect("the fork point")
    );
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
async fn without_session_fork_a_fork_starts_a_new_session_given_the_transcript() {
    let document = "---\ndescription: Fixture with a body\nmode: all\nacp: { hide_tools: [\"*\"] }\n---\nYou review changes.\n";
    let acp = acp_fixture(&[], &[("fixture.md", document)]).await;
    let engine = acp.engine();
    let parent = engine.create_session(selection()).unwrap().session_id;
    turn(engine, parent, "one", "say one").await;
    turn(engine, parent, "two", "work").await;
    let tip = engine.inner.store.get(parent).unwrap().meta.last_event_seq;
    let child = fork(engine, parent, tip).await;

    let outcome = turn(engine, child, "child", "echo").await;
    let text = final_text(&outcome);
    let started = session_starts(engine, child);
    let own = started.last().expect("the fork's own session");
    assert_eq!(own.how, AcpSessionStart::New);
    assert_ne!(own.acp_session_id, started[0].acp_session_id);
    // The first prompt: the memory, the body, the transcript, then the
    // input.
    let prompt = prompts(&acp).last().cloned().expect("the fork's prompt");
    let [memory, body, transcript, input] = prompt.as_slice() else {
        panic!("{prompt:?}");
    };
    assert!(memory.contains("# Memory"), "{memory}");
    assert_eq!(body, "You review changes.\n");
    assert_eq!(input, "echo");
    assert!(
        transcript.starts_with("<cookie_transcript>"),
        "{transcript}"
    );
    for entry in [
        "User: say one",
        "Assistant: one",
        "User: work",
        "Assistant: Editing.",
        "Tool `acp.edit` {\"path\":\"/work/notes.txt\"}: edited /work/notes.txt",
        "Assistant: Done.",
    ] {
        assert!(transcript.contains(entry), "{entry} in {transcript}");
    }
    assert!(text.ends_with(" | echo"), "{text}");
    assert!(
        diagnostics(engine, child)
            .iter()
            .any(|message| message.contains("does not offer `session/fork`")),
        "{:?}",
        diagnostics(engine, child)
    );
    // The next prompt is the input alone; the source keeps its session.
    let outcome = turn(engine, child, "child-again", "say more").await;
    assert_eq!(final_text(&outcome), "more");
    let outcome = turn(engine, parent, "parent-recall", "recall").await;
    assert_eq!(final_text(&outcome), "remembers: say one, work");
    assert_eq!(session_starts(engine, parent).len(), 1);
}

async fn revert(engine: &Engine, session: SessionId, through_seq: u64) {
    engine
        .revert_session(session, through_seq, origin(), None)
        .await
        .expect("reverted");
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

#[tokio::test(flavor = "multi_thread")]
async fn a_revert_forks_the_agents_session_at_the_revert_point_and_deletes_the_old_one() {
    let acp = acp_fixture(
        &[("ACP_FIXTURE_FORK", "1"), ("ACP_FIXTURE_DELETE", "1")],
        &[],
    )
    .await;
    let engine = acp.engine();
    let session = engine.create_session(selection()).unwrap().session_id;
    for (key, text) in [
        ("one", "say one"),
        ("two", "say two"),
        ("three", "say three"),
    ] {
        turn(engine, session, key, text).await;
    }
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
    let forks = requests(&acp, "session/fork");
    assert_eq!(forks.len(), 1);
    assert_eq!(forks[0]["sessionId"], old.as_str());
    assert_eq!(
        forks[0]["_meta"]["jetbrains"]["air"]["fork"]["messageId"],
        detached.message_id.clone().expect("the fork point")
    );
    let started = session_starts(engine, session);
    let forked = started.last().expect("the forked session");
    assert_eq!(forked.how, AcpSessionStart::Forked);
    assert_eq!(forked.forked_from.as_deref(), Some(old.as_str()));
    // The old session is closed and deleted, after the fork.
    let methods = acp.received_methods();
    let fork_at = methods.iter().position(|method| method == "session/fork");
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
async fn without_session_fork_a_revert_starts_a_new_session_given_the_kept_transcript() {
    let acp = acp_fixture(&[("ACP_FIXTURE_DELETE", "1")], &[]).await;
    let engine = acp.engine();
    let session = engine.create_session(selection()).unwrap().session_id;
    for (key, text) in [
        ("one", "say one"),
        ("two", "say two"),
        ("three", "say three"),
    ] {
        turn(engine, session, key, text).await;
    }
    let old = session_starts(engine, session)[0].acp_session_id.clone();
    revert(engine, session, run_end_seq(engine, session, 1)).await;

    let outcome = turn(engine, session, "echo", "echo").await;
    assert!(final_text(&outcome).ends_with(" | echo"));
    let prompt = prompts(&acp).last().cloned().expect("the prompt");
    let transcript = prompt
        .iter()
        .find(|block| block.starts_with("<cookie_transcript>"))
        .unwrap_or_else(|| panic!("{prompt:?}"));
    assert!(
        transcript.contains("User: say two\n\nAssistant: two"),
        "{transcript}"
    );
    assert!(!transcript.contains("three"), "{transcript}");
    let started = session_starts(engine, session);
    assert_eq!(started.last().unwrap().how, AcpSessionStart::New);
    assert_ne!(started.last().unwrap().acp_session_id, old);
    let deleted = asked(&acp, "session/delete", 1).await;
    assert_eq!(deleted[0]["sessionId"], old.as_str());
    assert!(
        diagnostics(engine, session).iter().any(|message| message
            .contains("cannot continue from the revert point (it does not offer `session/fork`)")),
        "{:?}",
        diagnostics(engine, session)
    );
    let outcome = turn(engine, session, "recall", "recall").await;
    assert_eq!(final_text(&outcome), "remembers: echo");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_revert_before_the_agents_first_turn_deletes_its_session_and_starts_fresh() {
    let acp = acp_fixture(
        &[("ACP_FIXTURE_FORK", "1"), ("ACP_FIXTURE_DELETE", "1")],
        &[],
    )
    .await;
    let engine = acp.engine();
    let session = engine.create_session(selection()).unwrap().session_id;
    turn(engine, session, "one", "say one").await;
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
    assert!(requests(&acp, "session/fork").is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn reverting_a_fork_into_its_sources_turns_leaves_the_sources_session_alone() {
    let acp = acp_fixture(
        &[("ACP_FIXTURE_FORK", "1"), ("ACP_FIXTURE_DELETE", "1")],
        &[],
    )
    .await;
    let engine = acp.engine();
    let parent = engine.create_session(selection()).unwrap().session_id;
    turn(engine, parent, "one", "say one").await;
    turn(engine, parent, "two", "say two").await;
    let source = session_starts(engine, parent)[0].acp_session_id.clone();
    let tip = engine.inner.store.get(parent).unwrap().meta.last_event_seq;
    let child = fork(engine, parent, tip).await;
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
    let forks = requests(&acp, "session/fork");
    assert_eq!(forks.last().unwrap()["sessionId"], source.as_str());
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
