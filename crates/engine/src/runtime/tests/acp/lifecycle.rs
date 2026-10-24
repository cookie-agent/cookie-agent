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
