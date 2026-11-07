//! A native session's context composed afresh at each checkpoint, and loaded
//! skills carried as turns (`docs/site/specs/forever-sessions.md` section
//! 5.2), through the engine.

use cookie_agent_protocol::{EventPayload, StoredEvent};

use super::golden_history::{
    CODING_AGENT, Rules, coding_fixture, origin, quiesce, run, script_server, text, user_text,
};
use crate::EngineHistoryView;

fn agent_md_text(event: &StoredEvent) -> Option<&str> {
    match &event.payload {
        EventPayload::AgentMdLoaded {
            entries,
            in_place: false,
            ..
        } => entries.first().map(|entry| entry.content.as_str()),
        _ => None,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_checkpoint_reads_agents_md_afresh_and_a_skill_loads_again_after_it() {
    let rules = Rules::from([
        ("one", vec![text("The first answer.")]),
        ("two", vec![text("The second answer.")]),
        ("three", vec![text("The answer after the checkpoint.")]),
    ]);
    let endpoint = script_server(rules).await;
    let (fixture, selection) = coding_fixture(&endpoint, CODING_AGENT, None).await;
    let engine = &fixture.engine;
    let workspace = fixture.root.path();
    std::fs::write(workspace.join("AGENTS.md"), "RULES-FIRST-VERSION\n").unwrap();
    let session = engine.create_session(selection.clone()).unwrap().session_id;
    run(
        engine,
        session,
        &selection,
        "one",
        user_text("#k:one Begin."),
    )
    .await;
    quiesce(engine, session).await;
    engine
        .invoke_skill(session, None, "release-notes", "")
        .await
        .expect("the skill loads");
    run(
        engine,
        session,
        &selection,
        "two",
        user_text("#k:two Go on."),
    )
    .await;
    quiesce(engine, session).await;

    // The edit reaches the model at the checkpoint, not at the next run.
    std::fs::write(workspace.join("AGENTS.md"), "RULES-SECOND-VERSION\n").unwrap();
    assert!(
        engine
            .compact_session(session, None, origin())
            .await
            .expect("compaction")
    );
    let log = engine.inner.store.log(session).unwrap();
    let events = log.event_snapshot();
    let (checkpoint, commit) = events
        .iter()
        .rev()
        .find_map(|event| match &event.payload {
            EventPayload::ContextCheckpointCommitted { commit } => Some((event.seq, commit)),
            _ => None,
        })
        .expect("a checkpoint");
    assert!(commit.context_fresh);
    let recorded = events
        .iter()
        .filter(|event| event.seq > commit.boundaries.input_through_seq && event.seq < checkpoint)
        .filter_map(|event| agent_md_text(event))
        .collect::<Vec<_>>();
    assert_eq!(recorded, ["RULES-SECOND-VERSION\n"]);
    let assembled = serde_json::to_string(
        &engine
            .get_history(session, EngineHistoryView::Assembled)
            .await
            .unwrap(),
    )
    .unwrap();
    assert!(assembled.contains("RULES-SECOND-VERSION"));
    assert!(!assembled.contains("RULES-FIRST-VERSION"));
    // The skill was summarized, as any turn before the kept tail.
    let skill_seq = events
        .iter()
        .find(|event| matches!(event.payload, EventPayload::SkillLoaded { .. }))
        .unwrap()
        .seq;
    if commit
        .boundaries
        .recent_from_seq
        .is_none_or(|recent| skill_seq < recent)
    {
        assert!(!assembled.contains("Collect the changes since the last release"));
    }

    // Loaded again, its body shows; loaded a third time, it is noted.
    for _ in 0..2 {
        engine
            .invoke_skill(session, None, "release-notes", "")
            .await
            .expect("the skill loads again");
    }
    let tail = log
        .event_snapshot()
        .iter()
        .filter(|event| event.seq > checkpoint)
        .filter_map(|event| match &event.payload {
            EventPayload::SkillLoaded { .. } => Some("loaded"),
            EventPayload::SkillInvocationNoted { .. } => Some("noted"),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(tail, ["loaded", "noted"]);
    run(
        engine,
        session,
        &selection,
        "three",
        user_text("#k:three Carry on."),
    )
    .await;
    quiesce(engine, session).await;
    engine.shutdown().await;
}
