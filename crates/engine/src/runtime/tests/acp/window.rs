//! An ACP session's window (`docs/site/specs/forever-sessions.md` sections 3.2,
//! 3.5, 5.4, and 7), against the scripted agent. Every store append of these,
//! as of every ACP test, compares the log with a reopening of its file at each
//! committed turn; these say the window was there to compare, and what else it
//! must answer.

use std::{num::NonZeroU32, path::Path, sync::Arc};

use cookie_agent_protocol::{
    EventPayload, EventsView, InputDelivery, ModelSelection, RunSelection, SessionId,
    SessionStatus, StoredEvent, display_view,
};
use serde_json::Value;

use super::*;
use crate::events::{EventLog, EventLogError};

/// A log's answers, each read from the window and from a reopening of the file.
pub(in crate::runtime::tests) fn assert_answers_as_reopened(log: &Arc<EventLog>, context: &str) {
    log.assert_window_reads_as_reopened();
    let reopened =
        EventLog::open_read_only(log.path().to_owned(), log.creation().unwrap().session_id)
            .expect("the log reopens");
    let tip = log.physical_tip_seq();
    for view in [EventsView::Display, EventsView::Full] {
        for budget in [2 * 1024, 1024 * 1024] {
            assert_eq!(
                log.newest_pass()
                    .unwrap()
                    .tail_window(view, budget)
                    .unwrap(),
                reopened
                    .history()
                    .unwrap()
                    .tail_window(view, budget)
                    .unwrap(),
                "{context}: the newest window"
            );
            let before = tip / 2;
            let page = log
                .history_range(..before)
                .unwrap()
                .older_page(view, budget, true)
                .unwrap();
            let whole = reopened
                .history_range(..before)
                .unwrap()
                .older_page(view, budget, true)
                .unwrap();
            assert_eq!(
                page.window, whole.window,
                "{context}: the page before {before}"
            );
            assert_eq!(
                page.header_at_end, whole.header_at_end,
                "{context}: the header before {before}"
            );
        }
    }
    for cursor in [None, Some(1), Some(tip / 2), Some(tip.saturating_sub(1))] {
        for limit in [None, NonZeroU32::new(3)] {
            let read = log.events_after(cursor, limit).unwrap();
            let whole = reopened.events_after(cursor, limit).unwrap();
            assert_eq!(
                read.events, whole.events,
                "{context}: the page after {cursor:?}"
            );
            assert_eq!(
                read.has_more, whole.has_more,
                "{context}: more after {cursor:?}"
            );
        }
    }
    let values = |log: &EventLog, from: u64| {
        let mut values = Vec::<Value>::new();
        log.visit_json(from.., |value| values.push(value.clone()))
            .unwrap();
        values
    };
    for from in [0, tip / 2] {
        assert_eq!(
            values(log, from),
            values(&reopened, from),
            "{context}: the lines from {from}"
        );
    }
    let mut count = display_view::DisplayCount::default();
    for event in log.history().unwrap() {
        count.observe(&event.unwrap());
    }
    assert_eq!(
        count.shown(),
        display_view::display_events(&reopened.event_snapshot(), false)
            .0
            .len() as u64,
        "{context}: the view's count"
    );
    assert_eq!(
        log.record_count(),
        reopened.event_snapshot().len() as u64,
        "{context}: the records"
    );
    // A native model's context, whether the log holds only that or reads it whole.
    let held = log.model_context_events().unwrap();
    let whole = reopened.model_context_events().unwrap();
    assert!(
        held.len() == whole.len() && held.iter().zip(whole.iter()).all(|(a, b)| a == b),
        "{context}: the model's context"
    );
    let folded = crate::session::projection(Arc::clone(log)).unwrap();
    let whole = crate::session::projection(reopened).unwrap();
    assert_eq!(folded.meta, whole.meta, "{context}: the metadata");
    assert_eq!(folded.runs, whole.runs, "{context}: the runs");
    assert_eq!(folded.usage, whole.usage, "{context}: the usage");
}

/// Waits for a record of `session` that `found` accepts.
async fn logged(engine: &Engine, session: SessionId, found: impl Fn(&EventPayload) -> bool) {
    await_event(engine, session, "a record", |event| found(&event.payload)).await;
}

fn passes(engine: &Engine, session: SessionId) -> Vec<String> {
    engine
        .inner
        .store
        .log(session)
        .unwrap()
        .take_history_calls_for_test()
}

fn log_of(engine: &Engine, session: SessionId) -> Arc<EventLog> {
    engine.inner.store.log(session).unwrap()
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
    for (key, text) in [
        ("two", "work"),
        ("three", "say three"),
        ("four", "cached"),
        ("five", "two-messages"),
        ("six", "work"),
    ] {
        quiet_turn(engine, session, key, text).await;
        calls.extend(passes(engine, session));
        assert_eq!(
            log_of(engine, session).window_for_test(),
            log_of(engine, session).state_latest_turn_seq_for_test(),
            "after {key}"
        );
    }
    assert!(calls.is_empty(), "{calls:#?}");
    assert!(log_of(engine, session).window_for_test().is_some());
}

/// Until a title commits every turn asks to generate one, from what the log
/// opened with, and that takes no pass either.
#[tokio::test(flavor = "multi_thread")]
async fn an_untitled_acp_session_asks_for_its_title_without_a_pass() {
    crate::session::skip_reference_fold();
    let (config, captured) = super::titles::title_model(&["", "", ""]).await;
    let acp = acp_fixture_with(&[], &[], &config).await;
    let engine = acp.engine();
    let session = engine.create_session(selection()).unwrap().session_id;
    quiet_turn(engine, session, "first", "say the opening words").await;
    let first = passes(engine, session);
    assert!(
        first.iter().all(|call| !call.starts_with("file")),
        "{first:#?}"
    );
    let mut calls = Vec::new();
    for (key, text) in [("two", "say two"), ("three", "say three")] {
        quiet_turn(engine, session, key, text).await;
        calls.extend(passes(engine, session));
    }
    assert!(calls.is_empty(), "{calls:#?}");
    assert!(log_of(engine, session).window_for_test().is_some());
    assert_eq!(engine.inner.store.get(session).unwrap().meta.title, None);
    drop(acp);
    let requests = captured.await.unwrap();
    assert_eq!(requests.len(), 3, "a title asked for each turn");
    for words in ["say the opening words", "say two", "say three"] {
        assert!(
            requests[2].contains(words),
            "the title's prompt names the session's opening messages"
        );
    }
}

/// A background delegation's completion and the agent's compaction, which are not
/// everyday turns, read their own run of the log and not what precedes it.
#[tokio::test(flavor = "multi_thread")]
async fn a_delegation_and_a_compaction_read_only_their_own_run() {
    crate::session::skip_reference_fold();
    let document = "---\ndescription: Fixture\nmode: all\nacp: {}\npermissions:\n  delegate: { fixture: allow }\n---\nReview.\n";
    let acp = acp_fixture(&[], &[("fixture.md", document)]).await;
    let engine = acp.engine();
    let session = engine.create_session(selection()).unwrap().session_id;
    for round in 0..3 {
        quiet_turn(engine, session, &format!("turn-{round}"), "say one").await;
    }
    passes(engine, session);
    let call = format!(
        "mcp call delegate_subagent {}",
        serde_json::json!({
            "description": "A child",
            "prompt": "say from the child",
            "agent_type": "fixture",
            "background": true,
        })
    );
    let before = log_of(engine, session).physical_tip_seq();
    quiet_turn(engine, session, "delegate", &call).await;
    logged(engine, session, |payload| {
        matches!(payload, EventPayload::ProducerMessageAccepted { .. })
    })
    .await;
    wait_for_session_not_running(engine, session).await;
    assert_reads_only_since(&passes(engine, session), "producers.rs", before);
    let before = log_of(engine, session).physical_tip_seq();
    assert!(
        engine
            .compact_session_result(session, None, origin())
            .await
            .expect("forwarded")
            .compacted
    );
    assert_reads_only_since(&passes(engine, session), "compaction.rs", before);
}

/// A subagent's report reaches its parent whenever it is delivered: one that
/// waited while the child's projection let go of its run, as an ACP child's
/// keeps only the latest runs that ended, is still delivered, from the log.
#[tokio::test(flavor = "multi_thread")]
async fn a_report_of_a_run_the_child_let_go_of_is_still_delivered() {
    crate::session::skip_reference_fold();
    let document = "---\ndescription: Fixture\nmode: all\nacp: {}\npermissions:\n  delegate: { fixture: allow }\n---\nReview.\n";
    let acp = acp_fixture(&[], &[("fixture.md", document)]).await;
    let engine = acp.engine();
    let session = engine.create_session(selection()).unwrap().session_id;
    let call = format!(
        "mcp call delegate_subagent {}",
        serde_json::json!({
            "description": "A child",
            "prompt": "say from the child",
            "agent_type": "fixture",
        })
    );
    quiet_turn(engine, session, "delegate", &call).await;
    let child = engine.children(session).unwrap()[0].session_id;
    let first = *engine
        .inner
        .store
        .get(child)
        .unwrap()
        .runs
        .keys()
        .next()
        .expect("the delegation's run");
    // The next run's report waits while the child runs more than the runs
    // its projection keeps.
    let (reached, release) = engine.install_subagent_report_hook(vec![first]);
    quiet_turn(engine, child, "reported", "say one").await;
    with_watchdog("the held report", reached)
        .await
        .expect("held");
    let reported = engine
        .inner
        .store
        .get(child)
        .unwrap()
        .runs
        .values()
        .find(|run| run.client_run_id.as_str() == "reported")
        .expect("the run")
        .id;
    for later in 0..17 {
        quiet_turn(engine, child, &format!("later-{later}"), "say one").await;
    }
    assert!(
        !engine
            .inner
            .store
            .get(child)
            .unwrap()
            .runs
            .contains_key(&reported),
        "the child's projection let go of the run"
    );
    let owner = cookie_agent_protocol::ProducerOwner::Subagent { session_id: child };
    let key = cookie_agent_protocol::ProducerIdempotencyKey::new(reported.to_string()).unwrap();
    assert_eq!(
        engine
            .goal_producer_projection(session)
            .unwrap()
            .accepted(&owner, &key),
        None,
        "not delivered yet"
    );
    engine
        .deliver_subagent_report_until_done(child, reported)
        .await;
    assert!(
        engine
            .goal_producer_projection(session)
            .unwrap()
            .accepted(&owner, &key)
            .is_some(),
        "the report reached the parent"
    );
    release.notify_one();
}

/// Panics if what `reader` asked of the file starts at or before `seq`.
fn assert_reads_only_since(calls: &[String], reader: &str, seq: u64) {
    for call in calls.iter().filter(|call| call.contains(reader)) {
        let Some(from) = call
            .strip_prefix("file from ")
            .and_then(|rest| rest.split(':').next())
        else {
            continue;
        };
        assert!(from.parse::<u64>().unwrap() > seq, "{call}");
    }
}

/// A long run of turns of every kind holds a turn's events, and answers as the
/// file does all along.
#[tokio::test(flavor = "multi_thread")]
async fn a_long_scripted_session_holds_one_turn_and_answers_as_its_file() {
    let acp = acp_fixture(
        &[("ACP_FIXTURE_FORK", "1"), ("ACP_FIXTURE_DELETE", "1")],
        &[],
    )
    .await;
    let engine = acp.engine();
    let session = engine.create_session(selection()).unwrap().session_id;
    let verbs = [
        "say hello",
        "work",
        "cached",
        "two-messages",
        "think about it",
        "notices",
        "work",
        "title A title",
        "say goodbye",
    ];
    let mut largest_held = 0;
    for round in 0..24 {
        let verb = verbs[round % verbs.len()];
        turn(engine, session, &format!("turn-{round}"), verb).await;
        let log = log_of(engine, session);
        assert_eq!(log.window_for_test(), log.state_latest_turn_seq_for_test());
        largest_held = largest_held.max(log.recent().len());
        if round % 8 == 7 {
            assert_answers_as_reopened(&log, &format!("round {round}"));
        }
    }
    // A steered input and a queued one, in a turn the window moves through.
    let run = start(engine, session, "steer", "steer-wait 10").await;
    logged(engine, session, |payload| {
        matches!(payload, EventPayload::ModelAttemptStarted { .. })
    })
    .await;
    for (key, text, delivery) in [
        ("steered", "the steer", InputDelivery::Steer),
        ("queued", "say queued", InputDelivery::Queue),
    ] {
        engine
            .submit_input(
                cookie_agent_protocol::SessionSubmitInputParams {
                    session_id: session,
                    content: text.into(),
                    delivery,
                    selection: None,
                    client_run_id: ClientRunId::new(key).unwrap(),
                    reset_fallback: false,
                },
                origin(),
            )
            .await
            .expect("input submitted");
    }
    finished(engine, session, run).await;
    // The queued input's run starts only once the steered one ends: the
    // session is briefly not running between them.
    super::super::golden_history::quiesce(engine, session).await;
    let log = log_of(engine, session);
    let all = log.event_snapshot().len();
    assert!(all > 200, "{all}");
    assert!(
        largest_held * 4 < all,
        "held {largest_held} of {all} records"
    );
    assert_answers_as_reopened(&log, "at the end");
}

/// What a revert keeps reads as the file does, whether the cut falls in the
/// window or before it, and the session goes on.
#[tokio::test(flavor = "multi_thread")]
async fn a_revert_of_a_windowed_session_folds_what_stays_from_its_file() {
    let acp = acp_fixture(&[("ACP_FIXTURE_FORK", "1")], &[]).await;
    let engine = acp.engine();
    let session = engine.create_session(selection()).unwrap().session_id;
    for round in 0..6 {
        turn(engine, session, &format!("turn-{round}"), "work").await;
    }
    let log = log_of(engine, session);
    let window = log.window_for_test().expect("the window moved");
    let run_ends = |engine: &Engine| {
        events(engine, session)
            .iter()
            .filter(|event| matches!(event.payload, EventPayload::RunFinished { .. }))
            .map(|event| event.seq)
            .collect::<Vec<_>>()
    };
    // Back before the window, by runs.
    let ends = run_ends(engine);
    let cut = ends[2];
    assert!(cut < window);
    let kept = log.event_snapshot()[..]
        .iter()
        .filter(|event| event.seq <= cut)
        .cloned()
        .collect::<Vec<_>>();
    engine
        .revert_session(session, cut, origin(), None)
        .await
        .expect("reverted");
    let log = log_of(engine, session);
    let after = log.event_snapshot();
    assert_eq!(
        after
            .iter()
            .take(kept.len())
            .map(|event| event.seq)
            .collect::<Vec<_>>(),
        kept.iter().map(|event| event.seq).collect::<Vec<_>>()
    );
    assert!(log.window_for_test().is_some_and(|start| start <= cut));
    assert_answers_as_reopened(&log, "after a revert before the window");
    // The sequences it cut name new records now.
    turn(engine, session, "after", "say after").await;
    let log = log_of(engine, session);
    assert!(log.window_for_test().is_some_and(|start| start > cut));
    assert_answers_as_reopened(&log, "after the next turn");
    // Back to the run just ended: inside what the window nearly holds.
    let ends = run_ends(engine);
    engine
        .revert_session(session, ends[ends.len() - 2], origin(), None)
        .await
        .expect("reverted again");
    assert_answers_as_reopened(&log_of(engine, session), "after a second revert");
    let outcome = turn(engine, session, "again", "say again").await;
    assert_eq!(final_text(&outcome), "again");
}

/// A fork of a windowed session copies what it asks for from the file, and is a
/// windowed session itself; its source stays as it was.
#[tokio::test(flavor = "multi_thread")]
async fn a_fork_of_a_windowed_session_reads_its_source_from_the_file() {
    let acp = acp_fixture(
        &[("ACP_FIXTURE_FORK", "1"), ("ACP_FIXTURE_DELETE", "1")],
        &[],
    )
    .await;
    let engine = acp.engine();
    let source = engine.create_session(selection()).unwrap().session_id;
    for round in 0..5 {
        turn(engine, source, &format!("turn-{round}"), "work").await;
    }
    let log = log_of(engine, source);
    let window = log.window_for_test().expect("the window moved");
    let through = events(engine, source)
        .iter()
        .filter(|event| matches!(event.payload, EventPayload::RunFinished { .. }))
        .nth(2)
        .unwrap()
        .seq;
    assert!(through < window);
    let copied = log.event_snapshot()[..]
        .iter()
        .take_while(|event| event.seq <= through)
        .map(|event| (event.seq, event.payload.clone()))
        .collect::<Vec<_>>();
    let fork = engine
        .fork_session(source, through, origin(), None)
        .await
        .expect("forked")
        .session_id;
    let fork_log = log_of(engine, fork);
    assert_eq!(
        fork_log.event_snapshot()[..copied.len()]
            .iter()
            .map(|event| (event.seq, event.payload.clone()))
            .collect::<Vec<_>>()
            .iter()
            .map(|(seq, _)| *seq)
            .collect::<Vec<_>>(),
        copied.iter().map(|(seq, _)| *seq).collect::<Vec<_>>()
    );
    assert!(
        fork_log
            .window_for_test()
            .is_some_and(|start| start <= through),
        "a fork lets go of what its file holds"
    );
    assert_answers_as_reopened(&fork_log, "the fork");
    assert_answers_as_reopened(&log_of(engine, source), "its source");
    turn(engine, fork, "fork-turn", "say fork").await;
    assert_answers_as_reopened(&log_of(engine, fork), "the fork's next turn");
    turn(engine, source, "source-turn", "say source").await;
    assert_answers_as_reopened(&log_of(engine, source), "its source's next turn");
}

const NATIVE_DOCUMENT: &str = "---\ndescription: A native agent\nmode: all\nmodels: [\"custom.test/group/model\"]\npermissions: {}\n---\nBe native.\n";

/// The switch to a native agent reads the log whole for the model, which then
/// sees the ACP agent's turns; the log holds it all until an ACP agent commits
/// a turn again (section 5.4).
#[tokio::test(flavor = "multi_thread")]
async fn a_switch_to_a_native_agent_and_back_reads_the_log_whole_and_windows_it_again() {
    let (config, captured) =
        super::titles::title_model(&["A title", "from the native model"]).await;
    let acp = acp_fixture_with(&[], &[("native.md", NATIVE_DOCUMENT)], &config).await;
    let engine = acp.engine();
    let session = engine.create_session(selection()).unwrap().session_id;
    for round in 0..3 {
        turn(
            engine,
            session,
            &format!("acp-{round}"),
            &format!("say acp {round}"),
        )
        .await;
    }
    let log = log_of(engine, session);
    assert!(log.window_for_test().is_some());

    // An assembled history of a session whose agent is still ACP reads whole,
    // and leaves the window where it is.
    let whole = log.model_context_events().unwrap();
    assert_eq!(whole.len(), log.event_snapshot().len());
    assert!(log.window_for_test().is_some());

    let native = engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: session,
                client_run_id: ClientRunId::new("native").unwrap(),
                selection: RunSelection {
                    agent: cookie_agent_protocol::AgentId::new("native").unwrap(),
                    model: ModelSelection {
                        model: "custom.test/group/model".parse().unwrap(),
                        variant: None,
                    },
                    preset: None,
                    acp_runtime: None,
                },
                content: "say native".into(),
            },
            origin(),
        )
        .await
        .expect("the native run starts")
        .run_id;
    let outcome = finished(engine, session, native).await;
    assert_eq!(final_text(&outcome), "from the native model");
    wait_for_session_not_running(engine, session).await;
    assert_eq!(
        log.window_for_test(),
        None,
        "a native agent's log holds it all"
    );
    assert_eq!(log.recent().len(), log.event_snapshot().len());

    // Back to the ACP agent: its next committed turn windows the log again.
    turn(engine, session, "back", "say back").await;
    assert_eq!(log.window_for_test(), log.state_latest_turn_seq_for_test());
    assert!(log.window_for_test().is_some());
    assert_answers_as_reopened(&log, "back on the ACP agent");

    // Cut back to where the native agent was: the log holds it all again.
    let native_end = log
        .event_snapshot()
        .iter()
        .find(|event| {
            event.run_id == Some(native)
                && matches!(event.payload, EventPayload::RunFinished { .. })
        })
        .unwrap()
        .seq;
    log.truncate_after_for_test(native_end).unwrap();
    assert_eq!(log.window_for_test(), None);
    let held = log.recent();
    assert_eq!(held.last().unwrap().seq, native_end);
    assert_eq!(held.len(), log.history().unwrap().count());
    assert_eq!(held.first().unwrap().seq, 1);
    drop(acp);
    let requests = captured.await.unwrap();
    let request = requests.last().unwrap();
    for words in ["say acp 0", "say acp 2", "say native"] {
        assert!(request.contains(words), "the native model read {words}");
    }
}

/// Starts a run of the native agent and waits for it to end, returning its final
/// text.
async fn native_turn(engine: &Engine, session: SessionId, key: &str, text: &str) -> String {
    let run = engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: session,
                client_run_id: ClientRunId::new(key).unwrap(),
                selection: RunSelection {
                    agent: cookie_agent_protocol::AgentId::new("native").unwrap(),
                    model: ModelSelection {
                        model: "custom.test/group/model".parse().unwrap(),
                        variant: None,
                    },
                    preset: None,
                    acp_runtime: None,
                },
                content: text.into(),
            },
            origin(),
        )
        .await
        .expect("the native run starts")
        .run_id;
    let outcome = finished(engine, session, run).await;
    wait_for_session_not_running(engine, session).await;
    final_text(&outcome)
}

/// Section 5.4 both ways and back, with a native model's checkpoint: from an ACP
/// agent to a native one the log reads whole, and holds it until the checkpoint,
/// which moves its window to what the model's context needs; back to the ACP
/// agent it narrows at the agent's next committed turn; and to the native one
/// again it reads whole once and holds the context since that checkpoint.
#[tokio::test(flavor = "multi_thread")]
async fn a_switch_between_an_acp_agent_and_a_native_one_with_a_checkpoint_windows_either_way() {
    let (config, captured) = super::titles::title_model(&[
        "A title",
        "native one",
        "native more",
        "Summary: the session so far.",
        "native two",
        "native three",
    ])
    .await;
    // Room for a summary and the tail it keeps beside the tools.
    let config = config.replace("context_tokens = 8192", "context_tokens = 200000");
    let acp = acp_fixture_with(&[], &[("native.md", NATIVE_DOCUMENT)], &config).await;
    let engine = acp.engine();
    let session = engine.create_session(selection()).unwrap().session_id;
    for round in 0..3 {
        turn(
            engine,
            session,
            &format!("acp-{round}"),
            &format!("say acp {round}"),
        )
        .await;
    }
    let log = log_of(engine, session);
    let acp_window = log.window_for_test().expect("an ACP agent's window");

    // To the native agent: whole, as its model reads it all.
    assert_eq!(
        native_turn(engine, session, "native-1", "say native one").await,
        "native one"
    );
    assert_eq!(log.window_for_test(), None, "whole until a checkpoint");
    assert_eq!(log.recent().len() as u64, log.record_count());
    assert_eq!(
        native_turn(engine, session, "native-more", "say more").await,
        "native more"
    );

    // Its checkpoint moves the window past where the ACP agent's was.
    let result = engine
        .compact_session_result(session, None, origin())
        .await
        .expect("compaction");
    assert!(result.compacted, "{result:?}");
    let native_window = log.window_for_test().expect("a native window");
    assert!(native_window > acp_window);
    assert!((log.recent().len() as u64) < log.record_count());
    assert_answers_as_reopened(&log, "at the native checkpoint");
    assert_eq!(
        native_turn(engine, session, "native-2", "say native two").await,
        "native two"
    );
    assert_eq!(
        log.window_for_test(),
        Some(native_window),
        "a turn leaves it"
    );
    assert_answers_as_reopened(&log, "after the native checkpoint");

    // Back to the ACP agent: its next committed turn narrows the log to it.
    turn(engine, session, "back", "say back").await;
    assert_eq!(log.window_for_test(), log.state_latest_turn_seq_for_test());
    assert!(log.window_for_test() > Some(native_window));
    assert_eq!(
        log.recent().len(),
        log.storage_len_for_test(),
        "an ACP agent's window pins nothing"
    );
    assert_answers_as_reopened(&log, "back on the ACP agent");

    // And native again: read whole once, then the context since its checkpoint.
    assert_eq!(
        native_turn(engine, session, "native-3", "say native three").await,
        "native three"
    );
    assert_eq!(log.window_for_test(), Some(native_window));
    assert_answers_as_reopened(&log, "native again");
    drop(acp);
    let requests = captured.await.unwrap();
    let request = requests.last().unwrap();
    assert!(
        request.contains("say back"),
        "the native model read the ACP turn"
    );
    assert!(request.contains("say native three"));
    assert!(
        !request.contains("say acp 0"),
        "nor what the summary covers"
    );
}

/// A pass reads the file for what precedes the window it saw: what the log
/// appends later is not in it, and a cut under it ends it rather than handing
/// out what the file holds now.
#[test]
fn a_pass_in_flight_stays_bounded_and_stops_at_a_cut() {
    let directory = private_tempdir();
    let (log, records) = windowed_log(directory.path());
    let window = log.window_for_test().expect("the window moved");
    assert!(window > 2);

    // Bounded: a pass asked now ends at this tip, whatever follows it.
    let tip = log.physical_tip_seq();
    let mut bounded = log.history().unwrap();
    assert_eq!(bounded.next().unwrap().unwrap().seq, 1);
    log.append(
        None,
        records.last().unwrap().origin.clone().unwrap(),
        EventPayload::SessionPermissionOverlaySet {
            overlay: cookie_agent_protocol::SessionPermissionOverlay { rules: Vec::new() },
        },
    )
    .unwrap();
    assert_eq!(log.physical_tip_seq(), tip + 1);
    let rest = bounded.collect::<Result<Vec<_>, _>>().unwrap();
    assert_eq!(rest.last().unwrap().seq, tip);
    assert_eq!(rest.len() as u64, tip - 1);

    // Asked before the cut, read after it: the pass ends with an error.
    let cut = window - 2;
    let mut in_flight = log.history().unwrap();
    let also_in_flight = log.history_range(2..cut).unwrap();
    assert_eq!(
        in_flight.next().unwrap().unwrap().seq,
        1,
        "the creation is held"
    );
    log.truncate_after_for_test(cut).unwrap();
    assert!(matches!(
        in_flight.next(),
        Some(Err(EventLogError::HistoryCut { .. }))
    ));
    assert!(in_flight.next().is_none(), "nothing follows an error");
    assert!(
        also_in_flight
            .collect::<Result<Vec<_>, _>>()
            .is_err_and(|error| matches!(error, EventLogError::HistoryCut { .. }))
    );
    // A pass asked afterwards reads what stands.
    let stands = log
        .history()
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(stands.last().unwrap().seq, cut);
    assert_eq!(stands.len() as u64, cut);
}

/// A pass reads ahead only so far: what it has yet to read when the file is cut
/// is refused, and what it read stays true of the log as it stood.
#[test]
fn a_pass_past_its_read_ahead_stops_at_a_cut() {
    let directory = private_tempdir();
    let (log, _) = windowed_log_of_large_records(directory.path());
    let window = log.window_for_test().expect("the window moved");
    let mut pass = log.history().unwrap();
    assert_eq!(pass.next().unwrap().unwrap().seq, 1);
    let second = pass.next().unwrap().unwrap();
    assert_eq!(second.seq, 2);
    log.truncate_after_for_test(window - 1).unwrap();
    // Records past the cut are still before it: nothing it read is taken back,
    // and what it has yet to read is refused rather than read as it is now.
    let rest = pass.collect::<Result<Vec<_>, _>>();
    assert!(rest.is_err_and(|error| matches!(error, EventLogError::HistoryCut { .. })));
}

/// A log's records appended in order as the store appends them, its window
/// moved by each committed turn of the golden ACP scenario's root.
fn windowed_log(directory: &Path) -> (Arc<EventLog>, Vec<StoredEvent>) {
    let records = EventLog::open_read_only(golden_acp_root(), golden_acp_session())
        .expect("the golden log")
        .event_snapshot()
        .iter()
        .map(|event| event.as_ref().clone())
        .collect::<Vec<_>>();
    let log = EventLog::create(
        directory.join("events.jsonl"),
        records[0].session_id,
        records[0].origin.clone().unwrap(),
        records[0].payload.clone(),
    )
    .unwrap();
    for record in &records[1..] {
        log.append(
            record.run_id,
            record.origin.clone().unwrap(),
            record.payload.clone(),
        )
        .unwrap();
    }
    (log, records)
}

/// [`windowed_log`], its first runs' inputs larger than a pass reads ahead.
fn windowed_log_of_large_records(directory: &Path) -> (Arc<EventLog>, Vec<StoredEvent>) {
    let mut records = EventLog::open_read_only(golden_acp_root(), golden_acp_session())
        .expect("the golden log")
        .event_snapshot()
        .iter()
        .map(|event| event.as_ref().clone())
        .collect::<Vec<_>>();
    for record in &mut records {
        if let EventPayload::UserInputSubmitted { content } = &mut record.payload {
            *content = "large ".repeat(40 * 1024).into();
        }
    }
    let log = EventLog::create(
        directory.join("events.jsonl"),
        records[0].session_id,
        records[0].origin.clone().unwrap(),
        records[0].payload.clone(),
    )
    .unwrap();
    for record in &records[1..] {
        log.append(
            record.run_id,
            record.origin.clone().unwrap(),
            record.payload.clone(),
        )
        .unwrap();
    }
    (log, records)
}

fn golden_acp_session() -> SessionId {
    "01a1b46f-2d5c-7df2-85cb-827811e29929".parse().unwrap()
}

fn golden_acp_root() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/history")
        .join(cookie_agent_protocol::HISTORY_FORMAT.to_string())
        .join("acp")
        .join(golden_acp_session().to_string())
        .join("events.jsonl")
}

/// A session that left memory comes back from its state file, parsing nothing,
/// and goes on as the file says: the store compares that open with a parse, and
/// each committed turn with a reopening.
#[tokio::test(flavor = "multi_thread")]
async fn a_session_that_left_memory_comes_back_from_its_state_file() {
    let acp = acp_fixture(&[], &[]).await;
    let engine = acp.engine();
    let session = engine.create_session(selection()).unwrap().session_id;
    for round in 0..3 {
        quiet_turn(engine, session, &format!("turn-{round}"), "say something").await;
    }
    let directory = engine.inner.store.session_dir(session);
    let state = directory.join(crate::session::STATE_FILE);
    let tip = log_of(engine, session).physical_tip_seq();
    assert!(engine.inner.store.evict(session).unwrap());
    assert!(state.is_file(), "leaving memory writes the file");
    let written = std::fs::read(&state).unwrap();

    let parses = crate::events::EVENT_LOG_PARSES.with(std::cell::Cell::get);
    let log = log_of(engine, session);
    assert!(log.opened_from_state_for_test());
    assert_eq!(log.physical_tip_seq(), tip);
    assert_eq!(
        crate::events::EVENT_LOG_PARSES.with(std::cell::Cell::get),
        parses,
        "nothing parsed"
    );
    assert_eq!(
        std::fs::read(&state).unwrap(),
        written,
        "nothing to rewrite"
    );

    quiet_turn(engine, session, "after", "say something").await;
    assert!(log_of(engine, session).physical_tip_seq() > tip);
    assert_answers_as_reopened(&log_of(engine, session), "after it came back");
}

/// A committed turn writes the file once over 1 MiB of log came since the last
/// write, so a long run is covered and an ordinary turn does not pay for one.
#[tokio::test(flavor = "multi_thread")]
async fn a_committed_turn_writes_the_state_file_per_mib_of_log() {
    // The reference fold writes a state file of its own at each committed turn.
    crate::session::skip_reference_fold();
    let acp = acp_fixture(&[], &[]).await;
    let engine = acp.engine();
    let session = engine.create_session(selection()).unwrap().session_id;
    let words = "word ".repeat(16 * 1024);
    let directory = engine.inner.store.session_dir(session);
    let state = directory.join(crate::session::STATE_FILE);
    let log_path = directory.join("events.jsonl");
    let mut last_written: Option<(Vec<u8>, u64)> = None;
    let mut writes = 0;
    for round in 0..80 {
        quiet_turn(
            engine,
            session,
            &format!("turn-{round}"),
            &format!("say {words}"),
        )
        .await;
        let length = std::fs::metadata(&log_path).unwrap().len();
        let Ok(bytes) = std::fs::read(&state) else {
            assert!(
                length < 2 * 1024 * 1024,
                "no file with {length} bytes of log"
            );
            continue;
        };
        match &last_written {
            Some((last, _)) if *last == bytes => {}
            previous => {
                let since = previous.as_ref().map_or(0, |(_, at)| *at);
                assert!(
                    length - since > 1024 * 1024,
                    "a write {} bytes after the last",
                    length - since
                );
                writes += 1;
                last_written = Some((bytes, length));
            }
        }
        if writes == 2 {
            break;
        }
    }
    assert_eq!(writes, 2, "a write per MiB of log");
    let log = log_of(engine, session);
    assert!(log.state_behind().unwrap() < 1024 * 1024);
}

/// A revert removes the file: it names what the revert cut.
#[tokio::test(flavor = "multi_thread")]
async fn a_revert_removes_the_state_file() {
    let acp = acp_fixture(&[], &[]).await;
    let engine = acp.engine();
    let session = engine.create_session(selection()).unwrap().session_id;
    quiet_turn(engine, session, "first", "say something").await;
    let keep = log_of(engine, session).physical_tip_seq();
    quiet_turn(engine, session, "second", "say something").await;
    let directory = engine.inner.store.session_dir(session);
    assert!(engine.inner.store.evict(session).unwrap());
    let state = directory.join(crate::session::STATE_FILE);
    assert!(state.is_file());
    log_of(engine, session);
    engine.inner.store.truncate(session, keep).unwrap();
    assert!(!state.exists());
}

/// An open of an ACP agent's log holds its window from the start, whoever opens
/// it, as what it lets go of is in the file.
#[test]
fn an_open_of_a_log_of_an_acp_agent_holds_its_window() {
    let read_only =
        EventLog::open_read_only(golden_acp_root(), golden_acp_session()).expect("the golden log");
    assert!(read_only.window_for_test().is_some());
    assert!(!read_only.opened_from_state_for_test());

    let directory = private_tempdir();
    let (owned, _) = windowed_log(directory.path());
    assert!(owned.window_for_test().is_some());
    let reopened = EventLog::open(owned.path().to_owned(), golden_acp_session()).unwrap();
    assert_eq!(reopened.window_for_test(), owned.window_for_test());
    assert_eq!(reopened.creation().unwrap().seq, 1);
}

/// A session's view cache moves as its log grows without a pass over the log,
/// and a cold reader and a reader of the resident session get what the whole
/// file gives.
#[tokio::test(flavor = "multi_thread")]
async fn a_windowed_session_places_its_view_cache_without_a_pass() {
    crate::session::skip_reference_fold();
    let acp = acp_fixture(&[], &[]).await;
    let engine = acp.engine();
    let session = engine.create_session(selection()).unwrap().session_id;
    let words = "word ".repeat(16 * 1024);
    quiet_turn(engine, session, "first", &format!("say {words}")).await;
    passes(engine, session);
    let cache = engine
        .inner
        .store
        .session_dir(session)
        .join(crate::session::VIEW_FILE);
    let mut moved = std::collections::HashSet::new();
    for round in 0..40 {
        quiet_turn(
            engine,
            session,
            &format!("turn-{round}"),
            &format!("say {words}"),
        )
        .await;
        assert!(
            passes(engine, session).is_empty(),
            "a pass at round {round}"
        );
        if let Ok(bytes) = std::fs::read(&cache) {
            moved.insert(bytes);
        }
    }
    assert!(moved.len() > 2, "the cache moved {} times", moved.len());
    let log = log_of(engine, session);
    assert_answers_as_reopened(&log, "at the end");
    let whole = log.event_snapshot();
    let expected = display_view::tail_window(&whole, EventsView::Display, 64 * 1024);
    let resident = engine
        .inner
        .store
        .snapshot_tail(session, EventsView::Display, 64 * 1024)
        .unwrap();
    assert_eq!(resident.events, expected.events);
    drop(log);
    assert!(engine.inner.store.evict(session).unwrap());
    let opens = engine.inner.store.log_open_count(session);
    let cold = engine
        .inner
        .store
        .snapshot_tail(session, EventsView::Display, 64 * 1024)
        .unwrap();
    assert_eq!(cold.events, expected.events);
    assert_eq!(
        engine.inner.store.log_open_count(session),
        opens,
        "the cache answered"
    );
}

/// What the load repaired or left out of a log it reads the same way, or leaves
/// out the same way, when a pass reads it from the file, and the lines stay.
#[test]
fn a_pass_reads_damage_as_the_load_did() {
    let directory = private_tempdir();
    let dir = directory.path().join(golden_acp_session().to_string());
    create_private_test_dir(&dir);
    let path = dir.join("events.jsonl");
    let text = std::fs::read_to_string(golden_acp_root()).unwrap();
    let mut lines = text.lines().map(str::to_owned).collect::<Vec<_>>();
    // Early records, all before the last turn's window: one that fails a
    // cross-event check, which the load quarantines, one this build cannot
    // read, and a line that is no JSON.
    let updates = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| line.contains("\"acp_session_updated\""))
        .map(|(index, _)| index)
        .take(3)
        .collect::<Vec<_>>();
    let mut quarantined: Value = serde_json::from_str(&lines[updates[0]]).unwrap();
    let quarantined_seq = quarantined["seq"].as_u64().unwrap();
    quarantined["payload"] = serde_json::json!({
        "type": "run_finished",
        "outcome": { "type": "completed" },
    });
    quarantined["run_id"] = serde_json::json!(cookie_agent_protocol::RunId::new_v7());
    lines[updates[0]] = quarantined.to_string();
    let mut unknown: Value = serde_json::from_str(&lines[updates[1]]).unwrap();
    let unknown_seq = unknown["seq"].as_u64().unwrap();
    unknown["payload"] = serde_json::json!({ "type": "from_a_newer_build" });
    unknown["skippable"] = Value::Bool(true);
    lines[updates[1]] = unknown.to_string();
    lines[updates[2]] = "not a record".to_owned();
    std::fs::write(&path, lines.join("\n") + "\n").unwrap();

    let log = EventLog::open(path, golden_acp_session()).expect("the log opens");
    assert!(
        log.diagnostics()
            .iter()
            .any(|diagnostic| diagnostic.seq == quarantined_seq
                && diagnostic.reason.starts_with("quarantined")),
        "{:?}",
        log.diagnostics()
    );
    crate::session::projection(Arc::clone(&log)).unwrap();
    assert!(log.window_for_test().is_some());
    let read = log.history().unwrap().map(|event| event.unwrap().seq);
    let read = read.collect::<Vec<_>>();
    assert!(!read.contains(&quarantined_seq));
    assert!(!read.contains(&unknown_seq));
    assert_answers_as_reopened(&log, "over damaged lines");
    let mut lines_read = Vec::new();
    log.visit_json(.., |value| {
        lines_read.push(value["seq"].as_u64().unwrap());
    })
    .unwrap();
    assert!(lines_read.contains(&quarantined_seq));
    assert!(lines_read.contains(&unknown_seq));
}
