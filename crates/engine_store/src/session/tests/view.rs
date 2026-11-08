//! The view cache (`view.json`) and newest-first answers that complete no
//! tree and open no session (`docs/site/specs/session-storage-v3.md`
//! sections 3.2 and 4.1).

use cookie_agent_protocol::{EventSubscriptionMessage, EventsView, display_view};

use super::super::{TailOwner, VIEW_FILE};
use super::*;

/// Budgets a client may ask for, the largest the cache covers.
const BUDGETS: [usize; 3] = [
    16 * 1024,
    cookie_agent_protocol::HISTORY_WINDOW_DEFAULT_BYTES as usize,
    cookie_agent_protocol::HISTORY_WINDOW_MAX_BYTES as usize,
];

fn append(store: &SessionStore, id: SessionId, run: RunId, payload: EventPayload) {
    store
        .append(id, Some(run), fuzz_origin(), payload)
        .expect("append");
}

fn filler(index: usize) -> EventPayload {
    EventPayload::Diagnostic {
        source: cookie_agent_protocol::DiagnosticSource::engine("test").expect("source"),
        kind: cookie_agent_protocol::DiagnosticKind::new("filler").expect("kind"),
        message: format!("{index} {}", "x".repeat(1_000)),
        count: None,
        detail: None,
    }
}

/// A session of `runs` finished runs of 1 KiB records, larger than the
/// largest window, and the run left open after them.
fn long_session(store: &SessionStore, runs: usize) -> (SessionId, RunId) {
    super::super::skip_reference_fold();
    let id = persist_test_session(store);
    let (mut run, _, _, _, run_started) = fuzz_scaffolding(store, id);
    for number in 0..runs {
        for index in 0..30 {
            append(store, id, run, filler(number * 100 + index));
        }
        append(
            store,
            id,
            run,
            EventPayload::run_finished(cookie_agent_protocol::RunOutcome::Completed {
                final_text: None,
            }),
        );
        run = RunId::new_v7();
        append(store, id, run, run_started.clone());
        append(
            store,
            id,
            run,
            EventPayload::UserInputSubmitted {
                content: format!("input {number}").into(),
            },
        );
    }
    (id, run)
}

/// What the newest-first answer of the whole log is, for each budget.
fn expected(
    store: &SessionStore,
    id: SessionId,
) -> Vec<cookie_agent_protocol::EventsSubscribeResult> {
    let events = store.get(id).expect("session").log.event_snapshot();
    BUDGETS
        .iter()
        .map(|&budget| {
            super::super::view::newest_first(display_view::tail_window(
                &events,
                EventsView::Display,
                budget,
            ))
        })
        .collect()
}

fn cold_answers(
    store: &SessionStore,
    id: SessionId,
) -> Vec<cookie_agent_protocol::EventsSubscribeResult> {
    BUDGETS
        .iter()
        .map(|&budget| {
            store
                .snapshot_tail(id, EventsView::Display, budget)
                .expect("cold answer")
        })
        .collect()
}

fn store_in(temporary: &tempfile::TempDir) -> Arc<SessionStore> {
    let cwd = temporary.path().join("workspace");
    create_private_test_dir_all(&cwd);
    SessionStore::open(&temporary.path().join("data"), &cwd).expect("store")
}

#[test]
fn a_cold_tail_reads_the_view_cache_a_run_end_wrote_and_answers_as_the_whole_log() {
    let temporary = private_tempdir();
    let store = store_in(&temporary);
    let (id, _) = long_session(&store, 40);
    let cache = store.session_dir(id).join(VIEW_FILE);
    assert!(cache.is_file(), "a run end writes the cache");
    let expected = expected(&store, id);
    assert!(store.evict(id).expect("evict"));

    let opens = store.log_open_count(id);
    assert_eq!(cold_answers(&store, id), expected);
    assert_eq!(
        store.log_open_count(id),
        opens,
        "the log is not parsed whole"
    );
    assert!(!store.is_resident(id), "a view opens nothing");
}

#[test]
fn a_cold_tail_goes_live_where_its_events_end() {
    let temporary = private_tempdir();
    let store = store_in(&temporary);
    let (id, run) = long_session(&store, 40);
    assert!(store.evict(id).expect("evict"));

    let (answer, mut tail) = store
        .subscribe_tail(id, EventsView::Display, BUDGETS[1], Some(TailOwner(1)))
        .expect("live answer");
    let tip = answer.events.last().expect("events").seq;
    assert!(!store.is_resident(id));
    // The next append reopens the session; the tail gets it, and only it.
    append(&store, id, run, filler(9_999));
    match tail.try_recv().expect("the appended event") {
        EventSubscriptionMessage::Event { event } => assert_eq!(event.seq, tip + 1),
        other => panic!("{other:?}"),
    }
    assert!(tail.try_recv().is_err());
}

#[test]
fn a_resident_session_answers_from_memory_and_goes_live() {
    let temporary = private_tempdir();
    let store = store_in(&temporary);
    let (id, run) = long_session(&store, 4);
    let expected = expected(&store, id);
    let (answer, mut tail) = store
        .subscribe_tail(id, EventsView::Display, BUDGETS[0], None)
        .expect("live answer");
    assert_eq!(answer, expected[0]);
    let tip = answer.events.last().expect("events").seq;
    append(&store, id, run, filler(9_999));
    match tail.try_recv().expect("the appended event") {
        EventSubscriptionMessage::Event { event } => assert_eq!(event.seq, tip + 1),
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_view_cache_that_no_longer_matches_the_log_is_not_used() {
    let temporary = private_tempdir();
    let store = store_in(&temporary);
    let (id, _) = long_session(&store, 40);
    let cache = store.session_dir(id).join(VIEW_FILE);
    let written = fs::read(&cache).expect("cache");
    // A revert into the second run, and a different history after it.
    store.truncate(id, 40).expect("revert");
    let (_, run, _) = store
        .get(id)
        .expect("session")
        .log
        .last_run_started()
        .expect("a run");
    for index in 0..50 {
        append(&store, id, run, filler(index));
    }
    let expected = expected(&store, id);
    assert!(store.evict(id).expect("evict"));
    fs::write(&cache, &written).expect("stale cache");
    assert_eq!(cold_answers(&store, id), expected);

    fs::write(&cache, b"{not json").expect("corrupt cache");
    assert_eq!(cold_answers(&store, id), expected);
    fs::remove_file(&cache).expect("no cache");
    assert_eq!(cold_answers(&store, id), expected);
}

#[test]
fn a_cold_answer_of_a_log_without_a_cache_parses_it_and_writes_one() {
    let temporary = private_tempdir();
    let store = store_in(&temporary);
    let (id, _) = long_session(&store, 40);
    let cache = store.session_dir(id).join(VIEW_FILE);
    let expected = expected(&store, id);
    assert!(store.evict(id).expect("evict"));
    fs::remove_file(&cache).expect("no cache");
    // Another process: nothing is written into a tree it does not own.
    drop(store);
    let store = store_in(&temporary);
    assert_eq!(cold_answers(&store, id), expected);
    assert!(!cache.exists());
    // Owned here: the parse leaves a cache behind.
    store.open_for_write(id).expect("adopt");
    assert!(store.evict(id).expect("evict"));
    fs::remove_file(&cache).ok();
    let opens = store.log_open_count(id);
    assert_eq!(cold_answers(&store, id), expected);
    assert_eq!(store.log_open_count(id), opens + 1);
    assert!(cache.is_file());
    assert_eq!(cold_answers(&store, id), expected);
    assert_eq!(store.log_open_count(id), opens + 1);
}

/// The `coding` golden history as history format 1 wrote it: three runs,
/// tool calls, and a 66 KB tool result.
const CODING_V1: &str = include_str!(
    "../../../../engine/tests/fixtures/history/1/coding/01a152fa-2785-7822-b056-5a3ad818855f/events.jsonl"
);

/// The format-1 `coding` history with an ACP agent's live state among its
/// events (a presentation after each call starts, a context use after each
/// attempt starts), converted to history format 2 in `dir`, the folder of
/// its root: a log with values in `blobs/` and `record_omitted` records.
fn converted_coding(dir: &Path) -> (SessionId, std::path::PathBuf) {
    let events = CODING_V1
        .lines()
        .map(|line| serde_json::from_str::<cookie_agent_protocol::StoredEvent>(line).unwrap())
        .collect::<Vec<_>>();
    let id = events[0].session_id;
    let mut mixed = Vec::new();
    for event in events {
        let live = match &event.payload {
            EventPayload::ToolCallStarted { start } => {
                Some(cookie_agent_protocol::AcpSessionUpdate {
                    tool_presentation: Some(cookie_agent_protocol::AcpToolPresentation {
                        tool_call_id: start.tool_call_id,
                        presentation: cookie_agent_protocol::PreparedPresentation::new(
                            SafeDisplayText::new("a call, refined").unwrap(),
                            None,
                        ),
                    }),
                    ..Default::default()
                })
            }
            EventPayload::ModelAttemptStarted { .. } => {
                Some(cookie_agent_protocol::AcpSessionUpdate {
                    context: Some(cookie_agent_protocol::AcpContextUsage {
                        used: event.seq * 100,
                        size: Some(200_000),
                    }),
                    ..Default::default()
                })
            }
            _ => None,
        };
        let template = event.clone();
        mixed.push(event);
        if let Some(update) = live {
            mixed.push(cookie_agent_protocol::StoredEvent {
                payload: EventPayload::AcpSessionUpdated { update },
                ..template
            });
        }
    }
    let mut log = String::new();
    for (index, mut event) in mixed.into_iter().enumerate() {
        event.seq = index as u64 + 1;
        log.push_str(
            &cookie_agent_protocol::write_stored_history(|| serde_json::to_string(&event)).unwrap(),
        );
        log.push('\n');
    }
    create_private_test_dir_all(dir);
    write_private_test_file(&dir.join(EVENTS_FILE), &log);
    // A view cache of the old log goes with the conversion.
    write_private_test_file(&dir.join(VIEW_FILE), b"{}");
    let conversion = super::super::convert::convert_tree(id, dir).expect("conversion");
    assert_eq!(conversion.converted.len(), 1);
    assert!(
        !dir.join(VIEW_FILE).exists(),
        "the conversion drops the view cache"
    );
    (id, dir.join(EVENTS_FILE))
}

#[test]
fn a_format_2_log_read_from_any_line_is_the_loaders_and_answers_the_same_window() {
    let temporary = private_tempdir();
    let dir = temporary.path().join("root");
    let (id, path) = converted_coding(&dir);
    let text = fs::read_to_string(&path).expect("converted log");
    assert!(text.contains("\"record_omitted\""), "live state is omitted");
    assert!(text.contains("_blob\""), "values moved to blobs");

    // Read whole, so it knows where every line starts: a native window would
    // let go of those before its checkpoint's context.
    let log = crate::events::EventLog::parsed_read_only(path.clone(), id).expect("full load");
    assert!(log.diagnostics().is_empty(), "{:?}", log.diagnostics());
    let events = log.event_snapshot();
    let total: usize = events
        .iter()
        .map(|event| serde_json::to_string(&**event).unwrap().len())
        .sum();
    let mut moved_values_read = 0;
    for (index, event) in events.iter().enumerate() {
        let offset = log.line_start(event.seq).expect("line");
        let read = crate::events::read_records_from(&path, offset)
            .expect("read")
            .expect("every line reads as the loader reads it");
        assert_eq!(
            read.iter()
                .map(|event| Arc::new(event.clone()))
                .collect::<Vec<_>>(),
            events[index..].to_vec(),
            "from seq {}",
            event.seq
        );
        if text
            .lines()
            .nth(index)
            .is_some_and(|line| line.contains("_blob\""))
        {
            moved_values_read += 1;
        }
        if index == 0
            || !matches!(
                event.payload,
                EventPayload::RunStarted { .. } | EventPayload::UserInputSubmitted { .. }
            )
        {
            continue;
        }
        let header = display_view::state_events(&events[..index]);
        let suffix = read.into_iter().map(Arc::new).collect::<Vec<_>>();
        for budget in [total / 12, total / 5, total / 2] {
            if let Some(window) =
                display_view::tail_window_after(&header, &suffix, EventsView::Display, budget)
            {
                assert_eq!(
                    window,
                    display_view::tail_window(&events, EventsView::Display, budget),
                    "from seq {}, budget {budget}",
                    event.seq
                );
            }
        }
    }
    assert!(moved_values_read > 0);
    // A missing blob is the loader's to report: the partial read declines.
    for entry in fs::read_dir(dir.join("blobs")).expect("blobs") {
        fs::remove_file(entry.expect("blob").path()).expect("remove blob");
    }
    assert_eq!(
        crate::events::read_records_from(&path, 0).expect("read"),
        None
    );
}

#[test]
fn a_view_cache_of_another_history_format_is_not_used() {
    let temporary = private_tempdir();
    let store = store_in(&temporary);
    let (id, _) = long_session(&store, 40);
    let cache = store.session_dir(id).join(VIEW_FILE);
    let expected = expected(&store, id);
    assert!(store.evict(id).expect("evict"));
    let mut written: serde_json::Value =
        serde_json::from_slice(&fs::read(&cache).expect("cache")).expect("cache json");
    written["history_format"] = serde_json::json!(cookie_agent_protocol::HISTORY_FORMAT - 1);
    fs::write(&cache, serde_json::to_vec(&written).unwrap()).expect("older cache");
    let opens = store.log_open_count(id);
    assert_eq!(cold_answers(&store, id), expected);
    assert_eq!(
        store.log_open_count(id),
        opens + 1,
        "the log is parsed instead"
    );
}
