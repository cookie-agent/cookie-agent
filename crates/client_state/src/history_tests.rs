use std::sync::Arc;

use cookie_agent_protocol::{
    EventsView, StoredEvent,
    display_view::{self, Window},
};
use cookie_agent_protocol_transport::{ClientDelivery, EventsPage, ReplayHistory, ReplayWindow};

use super::*;
use crate::{DeliveryOutcome, reduce::reduce_session_events};

/// The `coding` golden history: three runs with tool calls, approvals, a
/// compaction, and a 66 KB tool result at seq 36.
const CODING: &str = include_str!(
    "../../engine/tests/fixtures/history/1/coding/01a152fa-2785-7822-b056-5a3ad818855f/events.jsonl"
);
/// The `delegation` golden history: a goal with its checklist and changes,
/// subagents, and producer messages.
const DELEGATION: &str = include_str!(
    "../../engine/tests/fixtures/history/1/delegation/01a152fa-3f63-7442-8dfc-045922599ed0/events.jsonl"
);

fn log(text: &str) -> Vec<Arc<StoredEvent>> {
    text.lines()
        .map(|line| Arc::new(serde_json::from_str(line).unwrap()))
        .collect()
}

/// Opens `events`' session as a newest-first replay of `window` delivers it.
fn open(store: &mut StateStore, events: &[Arc<StoredEvent>], window: Window) -> SessionId {
    let session_id = events[0].session_id;
    let final_seq = window.events.last().unwrap().seq;
    let mut deliveries = vec![ClientDelivery::ReplayStart {
        session_id,
        generation: 0,
        final_seq,
        rebuild: true,
        history: Some(Box::new(ReplayHistory {
            window: Some(ReplayWindow {
                header: window
                    .header
                    .iter()
                    .map(|event| (**event).clone())
                    .collect(),
                older_cursor: window.older_cursor,
                gap: window.gap,
            }),
            cut: window.cut.clone(),
        })),
    }];
    deliveries.extend(
        window
            .events
            .iter()
            .map(|event| ClientDelivery::ReplayEvent {
                session_id,
                generation: 0,
                final_seq,
                event: Box::new((**event).clone()),
            }),
    );
    deliveries.push(ClientDelivery::ReplayEnd {
        session_id,
        generation: 0,
        final_seq,
    });
    for delivery in deliveries {
        assert_eq!(store.apply_delivery(delivery), DeliveryOutcome::Applied);
    }
    session_id
}

/// Reads the history `before` stands for from `events`, as the daemon
/// answers `events.page`.
fn load(
    store: &mut StateStore,
    events: &[Arc<StoredEvent>],
    session_id: SessionId,
    before: u64,
    budget: usize,
) {
    let params = store
        .request_history(session_id, before)
        .expect("the history is not loaded");
    assert_eq!(params.before, before);
    let page = display_view::older_page(events, before, EventsView::Display, budget);
    store.apply_history_page(
        session_id,
        before,
        Ok(EventsPage {
            events: page
                .events
                .iter()
                .map(|event| (**event).clone().into())
                .collect(),
            header: page.header.iter().map(|event| (**event).clone()).collect(),
            older_cursor: page.older_cursor,
            cut: page.cut,
        }),
    );
}

/// Loads every page of history the transcript leaves out.
fn load_all(
    store: &mut StateStore,
    events: &[Arc<StoredEvent>],
    session_id: SessionId,
    budget: usize,
) {
    let mut pages = 0;
    while let Some(before) = store.next_history(session_id) {
        load(store, events, session_id, before, budget);
        pages += 1;
        assert!(pages < events.len(), "loading makes progress");
    }
}

/// What the transcript shows, without row ids: each row's debug form.
fn rows(state: &SessionState) -> Vec<String> {
    state
        .transcript
        .iter()
        .filter(|item| !matches!(item, TranscriptItem::History { .. }))
        .map(|item| {
            let text = format!("{item:?}");
            // Ids differ between folds; what the rows hold does not.
            let mut kept = String::new();
            let mut rest = text.as_str();
            while let Some(at) = rest.find("id: ") {
                kept.push_str(&rest[..at]);
                rest = &rest[at + 4..];
                rest = rest.trim_start_matches(|c: char| c.is_ascii_digit());
            }
            kept.push_str(rest);
            kept
        })
        .collect()
}

/// The state a session's settings leave, which a window's header must
/// carry.
fn settings(state: &SessionState) -> String {
    format!(
        "{:?} {:?} {:?} {:?} {:?} {:?} {:?} {:?} {:?} {:?} {:?} {:?} {:?}",
        state.created_at,
        state.creation_agent.as_ref().map(|agent| &agent.agent),
        state.run_agent,
        state.active_run,
        state.model_tools,
        state.model_selection,
        state.runtime_attachments,
        state.goal,
        state.acp.commands,
        state.acp.options,
        state.held,
        state.queued_inputs,
        state.deferred_tool_groups,
    )
}

#[test]
fn a_window_folds_to_the_settings_a_full_replay_reaches() {
    for text in [CODING, DELEGATION] {
        let events = log(text);
        let full = reduce_session_events(events[0].session_id, 0, &events);
        for budget in [8 * 1024, 24 * 1024, 64 * 1024] {
            let mut store = StateStore::default();
            let window = display_view::tail_window(&events, EventsView::Display, budget);
            let session_id = open(&mut store, &events, window);
            let state = &store.sessions[&session_id];
            assert_eq!(settings(state), settings(&full), "budget {budget}");
            assert_eq!(state.last_seq, events.last().unwrap().seq);
        }
    }
}

#[test]
fn loading_every_page_shows_what_the_whole_display_view_shows() {
    for text in [CODING, DELEGATION] {
        let events = log(text);
        let (whole, _) = display_view::display_events(&events, true);
        let expected = reduce_session_events(events[0].session_id, 0, &whole);
        let mut store = StateStore::default();
        let window = display_view::tail_window(&events, EventsView::Display, 8 * 1024);
        let session_id = open(&mut store, &events, window);
        assert!(store.next_history(session_id).is_some());
        load_all(&mut store, &events, session_id, 8 * 1024);
        let state = &store.sessions[&session_id];
        assert_eq!(state.history, HistoryState::default());
        assert!(
            !state
                .transcript
                .iter()
                .any(|item| matches!(item, TranscriptItem::History { .. }))
        );
        // Each page keeps only its own last published tools, which no row
        // shows.
        assert_eq!(rows(state), rows(&expected));
        assert_eq!(settings(state), settings(&expected));
    }
}

#[test]
fn rows_already_shown_keep_their_ids_when_older_history_arrives() {
    let events = log(CODING);
    let mut store = StateStore::default();
    let window = display_view::tail_window(&events, EventsView::Display, 16 * 1024);
    let session_id = open(&mut store, &events, window);
    let ids = |store: &StateStore| {
        store.sessions[&session_id]
            .transcript
            .iter()
            .filter(|item| !matches!(item, TranscriptItem::History { .. }))
            .map(|item| (item.id(), format!("{:?}", std::mem::discriminant(item))))
            .collect::<Vec<_>>()
    };
    let before = ids(&store);
    let older = store.next_history(session_id).unwrap();
    load(&mut store, &events, session_id, older, 16 * 1024);
    let after = ids(&store);
    assert!(after.len() > before.len());
    assert!(after.ends_with(&before), "{before:?}\n{after:?}");
}

#[test]
fn a_gap_shows_a_row_and_closes_when_its_events_arrive() {
    let events = log(CODING);
    let last_run = events
        .iter()
        .rposition(|event| {
            matches!(
                event.payload,
                EventPayload::RunStarted { .. } | EventPayload::UserInputSubmitted { .. }
            )
        })
        .unwrap();
    let (run, _) = display_view::display_events(&events[last_run..], true);
    let run_bytes = serde_json::to_vec(&run).unwrap().len();
    let mut store = StateStore::default();
    let window = display_view::tail_window(&events, EventsView::Display, run_bytes / 3);
    let gap = window.gap.expect("the newest run does not fit");
    let session_id = open(&mut store, &events, window);
    let state = &store.sessions[&session_id];
    assert_eq!(state.history.gap, Some(gap));
    assert!(state.transcript.iter().any(|item| matches!(
        item,
        TranscriptItem::History { before, skipped: Some(_), .. } if *before == gap.before_seq
    )));
    assert_eq!(store.next_history(session_id), Some(gap.before_seq));
    while store.sessions[&session_id].history.gap.is_some() {
        let before = store.next_history(session_id).unwrap();
        load(&mut store, &events, session_id, before, run_bytes / 3);
    }
    assert!(
        !store.sessions[&session_id]
            .transcript
            .iter()
            .any(|item| matches!(
                item,
                TranscriptItem::History {
                    skipped: Some(_),
                    ..
                }
            ))
    );
}

#[test]
fn a_page_nothing_waits_for_is_dropped_and_a_failed_one_shows() {
    let events = log(CODING);
    let mut store = StateStore::default();
    let window = display_view::tail_window(&events, EventsView::Display, 16 * 1024);
    let session_id = open(&mut store, &events, window);
    let before = store.next_history(session_id).unwrap();
    // Only one page at a time, and only for history the transcript lacks.
    assert!(store.request_history(session_id, before + 1).is_none());
    assert!(store.request_history(session_id, before).is_some());
    assert!(store.request_history(session_id, before).is_none());
    store.apply_history_page(session_id, before, Err("timed out".into()));
    let state = &store.sessions[&session_id];
    assert_eq!(state.history.failed.as_deref(), Some("timed out"));
    assert_eq!(state.history.loading, None);
    let version = store.sessions[&session_id].version;
    store.apply_history_page(session_id, before, Err("late".into()));
    assert_eq!(store.sessions[&session_id].version, version);
    assert!(store.request_history(session_id, before).is_some());
}

#[test]
fn a_cut_result_is_read_whole_on_request() {
    let events = log(CODING);
    let mut store = StateStore::default();
    let window = display_view::tail_window(&events, EventsView::Display, usize::MAX / 2);
    let session_id = open(&mut store, &events, window);
    let whole = display_view::event_at(&events, 36).unwrap();
    let EventPayload::ToolCallTerminated { termination } = &whole.payload else {
        panic!("seq 36 is a tool result");
    };
    let call = termination.tool_call_id;
    let tool = &store.sessions[&session_id].tools[&call];
    let cut = tool.cut.clone().expect("the view cut it");
    assert_eq!(cut.seq, 36);
    assert_eq!(cut.state, CutState::Cut);
    let shown = tool.display.clone();

    let read = store.request_cut_result(session_id, call).expect("a read");
    assert_eq!(read.seq, 36);
    assert!(
        store.request_cut_result(session_id, call).is_none(),
        "one read at a time"
    );
    store.apply_cut_result(session_id, call, Err("closed".into()));
    assert_eq!(
        store.sessions[&session_id].tools[&call]
            .cut
            .as_ref()
            .map(|cut| &cut.state),
        Some(&CutState::Failed("closed".into()))
    );
    store
        .request_cut_result(session_id, call)
        .expect("tried again");
    store.apply_cut_result(session_id, call, Ok(Some((**whole).clone())));
    let tool = &store.sessions[&session_id].tools[&call];
    assert_eq!(tool.cut, None);
    assert_ne!(tool.display, shown);
    assert_eq!(tool.display, termination.result.as_ref().unwrap().display);
}

#[test]
fn live_events_after_a_window_survive_a_fold() {
    let events = log(CODING);
    let split = events.len() - 10;
    let mut store = StateStore::default();
    let window = display_view::tail_window(&events[..split], EventsView::Display, 16 * 1024);
    let session_id = open(&mut store, &events, window);
    for event in &events[split..] {
        assert!(store.apply_event((**event).clone()));
    }
    let live = rows(&store.sessions[&session_id]);
    let older = store.next_history(session_id).unwrap();
    load(&mut store, &events, session_id, older, 16 * 1024);
    let folded = rows(&store.sessions[&session_id]);
    assert!(folded.ends_with(&live[live.len() - 10..]));
    assert_eq!(
        store.sessions[&session_id].last_seq,
        events.last().unwrap().seq
    );
}
