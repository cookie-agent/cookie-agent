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

/// A native session's window moves when its checkpoint commits, to what its
/// model's context needs; the checkpoint writes `state.json`, from which the
/// session comes back as it was; and an ordinary turn after it asks for no pass
/// over the log (`docs/site/specs/forever-sessions.md` sections 5 and 7).
#[tokio::test(flavor = "multi_thread")]
async fn a_native_window_moves_at_its_checkpoint_and_an_ordinary_turn_reads_no_pass() {
    // The reference folds of a store append read the whole log: they would
    // count, and are what this keeps a turn from doing.
    crate::session::skip_reference_fold();
    let rules = Rules::from([
        ("one", vec![text("The first answer.")]),
        (
            "two",
            vec![
                super::golden_history::tool_reply(
                    "read",
                    serde_json::json!({"path": "src/lib.rs"}),
                ),
                text("The second answer."),
            ],
        ),
        (
            "three",
            vec![
                super::golden_history::tool_reply(
                    "read",
                    serde_json::json!({"path": "src/lib.rs"}),
                ),
                text("An ordinary answer."),
            ],
        ),
        ("four", vec![text("The answer after it came back.")]),
    ]);
    let endpoint = script_server(rules).await;
    let (fixture, selection) = coding_fixture(&endpoint, CODING_AGENT, None).await;
    let engine = &fixture.engine;
    let session = engine.create_session(selection.clone()).unwrap().session_id;
    for (key, words) in [("one", "#k:one Begin."), ("two", "#k:two Read it.")] {
        run(engine, session, &selection, key, user_text(words)).await;
        quiesce(engine, session).await;
    }
    let log = engine.inner.store.log(session).unwrap();
    assert_eq!(log.window_for_test(), None, "no checkpoint, no window");
    let state = engine
        .inner
        .store
        .session_dir(session)
        .join(crate::session::STATE_FILE);
    assert!(!state.exists());
    log.take_history_calls_for_test();

    assert!(
        engine
            .compact_session(session, None, origin())
            .await
            .expect("compaction")
    );
    // Compaction reads the model's context, which the log holds, and no more.
    let calls = log.take_history_calls_for_test();
    assert!(calls.is_empty(), "{calls:#?}");
    let window = log
        .window_for_test()
        .expect("the checkpoint moved the window");
    let held = log.recent().len() as u64;
    assert!(
        held < log.record_count(),
        "{held} of {}",
        log.record_count()
    );
    assert!(state.is_file(), "the checkpoint writes the state file");
    let context = log.model_context_events().unwrap();
    assert!(context.iter().any(|event| event.seq < window), "it pins");

    log.take_history_calls_for_test();
    run(
        engine,
        session,
        &selection,
        "three",
        user_text("#k:three Read it again."),
    )
    .await;
    quiesce(engine, session).await;
    let calls = log.take_history_calls_for_test();
    assert!(calls.is_empty(), "{calls:#?}");
    // The assembled history is the model's context too; the full one is the
    // whole log, which it reads from the file.
    engine
        .get_history(session, EngineHistoryView::Assembled)
        .await
        .unwrap();
    let calls = log.take_history_calls_for_test();
    assert!(calls.is_empty(), "{calls:#?}");
    engine
        .get_history(session, EngineHistoryView::Full)
        .await
        .unwrap();
    assert!(
        log.take_history_calls_for_test()
            .iter()
            .any(|call| call.starts_with("file from"))
    );
    assert_eq!(log.window_for_test(), Some(window), "a turn leaves it");

    // Out of memory and back: from the state file, holding what it held.
    let context = log.model_context_events().unwrap();
    let held = log.recent();
    assert!(engine.inner.store.evict(session).unwrap());
    let back = engine.inner.store.log(session).unwrap();
    assert!(back.opened_from_state_for_test());
    assert_eq!(back.window_for_test(), Some(window));
    assert!(
        back.recent()
            .iter()
            .map(|event| event.seq)
            .eq(held.iter().map(|event| event.seq))
    );
    let reopened = back.model_context_events().unwrap();
    assert!(
        reopened.len() == context.len() && reopened.iter().zip(context.iter()).all(|(a, b)| a == b)
    );
    super::acp::window::assert_answers_as_reopened(&back, "after it came back");
    run(
        engine,
        session,
        &selection,
        "four",
        user_text("#k:four Carry on."),
    )
    .await;
    quiesce(engine, session).await;
    let assembled = serde_json::to_string(
        &engine
            .get_history(session, EngineHistoryView::Assembled)
            .await
            .unwrap(),
    )
    .unwrap();
    assert!(assembled.contains("The answer after it came back."));
    assert!(assembled.contains("Read it again."));
    engine.shutdown().await;
}
