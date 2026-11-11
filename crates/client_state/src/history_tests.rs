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

/// The daemon's answer to `params` from `events`.
fn page(events: &[Arc<StoredEvent>], params: &EventsPageParams, budget: usize) -> EventsPage {
    let before = params.before;
    let page = display_view::older_page(events, before, EventsView::Display, budget);
    let header = match params.header {
        PageHeader::Full => page.header,
        PageHeader::None => Vec::new(),
        PageHeader::Delta => display_view::header_delta(
            &display_view::state_events(
                &events[..events.partition_point(|event| event.seq < before)],
            ),
            page.header,
        ),
    };
    EventsPage {
        events: page
            .events
            .iter()
            .map(|event| (**event).clone().into())
            .collect(),
        header: header.iter().map(|event| (**event).clone()).collect(),
        header_kind: params.header,
        older_cursor: page.older_cursor,
        cut: page.cut,
    }
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
    let page = page(events, &params, budget);
    assert!(
        store.apply_history_page(session_id, before, Ok(page)),
        "the page went in"
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
    // Pages this small start at model attempts inside a run, so a reply
    // shown in part starts again in the page before it: that reply is the
    // one row a page may give a new id.
    for history in [CODING, DELEGATION] {
        let events = log(history);
        let budget = 2 * 1024;
        let mut store = StateStore::default();
        let window = display_view::tail_window(&events, EventsView::Display, budget);
        let session_id = open(&mut store, &events, window);
        let ids = |store: &StateStore| {
            store.sessions[&session_id]
                .transcript
                .iter()
                .filter(|item| !matches!(item, TranscriptItem::History { .. }))
                .map(TranscriptItem::id)
                .collect::<Vec<_>>()
        };
        let mut pages = 0;
        while let Some(older) = store.next_history(session_id) {
            let before = ids(&store);
            load(&mut store, &events, session_id, older, budget);
            let after = ids(&store);
            let lost = before.iter().filter(|id| !after.contains(id)).count();
            assert!(
                lost <= 1,
                "page {pages} renumbered {lost} rows:\n{before:?}\n{after:?}"
            );
            pages += 1;
        }
        assert!(pages > 3, "{pages} pages");
    }
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
fn only_the_page_a_read_waits_for_goes_in() {
    let events = log(CODING);
    let budget = 16 * 1024;
    let mut store = StateStore::default();
    let window = display_view::tail_window(&events, EventsView::Display, budget);
    let session_id = open(&mut store, &events, window);
    let before = store.next_history(session_id).unwrap();
    let params = store.request_history(session_id, before).unwrap();
    // A failure shows on the row; no rows go in.
    assert!(!store.apply_history_page(session_id, before, Err("timed out".into())));
    // Nothing waits for a page now.
    assert!(!store.apply_history_page(session_id, before, Ok(page(&events, &params, budget))));
    assert_eq!(
        store.sessions[&session_id].history.older_cursor,
        Some(before)
    );
    let params = store.request_history(session_id, before).unwrap();
    // Nor for one of another session.
    assert!(!store.apply_history_page(
        SessionId::new_v7(),
        before,
        Ok(page(&events, &params, budget))
    ));
    assert!(store.apply_history_page(session_id, before, Ok(page(&events, &params, budget))));
    assert_ne!(
        store.sessions[&session_id].history.older_cursor,
        Some(before)
    );
    // The same answer again is dropped.
    assert!(!store.apply_history_page(session_id, before, Ok(page(&events, &params, budget))));
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

/// Held live state keeps only the newest value of each: a call's
/// presentation until its end, the context use until the run's end.
#[test]
fn held_live_state_keeps_the_newest_of_each_until_its_end() {
    use cookie_agent_protocol::{
        AcpContextUsage, AcpLiveUpdate, AcpToolPresentation, PreparedPresentation, RunId,
        SafeDisplayText, ToolCallId, TransientEvent,
    };
    let session_id = SessionId::new_v7();
    let run = RunId::new_v7();
    let (first, second) = (ToolCallId::new_v7(), ToolCallId::new_v7());
    let live = |update: AcpLiveUpdate| TransientEvent {
        session_id,
        run_id: Some(run),
        after_seq: 1,
        timestamp: jiff::Timestamp::UNIX_EPOCH,
        payload: EventPayload::AcpLiveUpdate { update },
    };
    let presentation = |call, title: &str| AcpLiveUpdate {
        tool_presentation: Some(AcpToolPresentation {
            tool_call_id: call,
            presentation: PreparedPresentation::new(SafeDisplayText::new(title).unwrap(), None),
        }),
        context: None,
    };
    let context = |used| AcpLiveUpdate {
        tool_presentation: None,
        context: Some(AcpContextUsage { used, size: None }),
    };
    let mut held = HeldHistory::default();
    for update in [
        presentation(first, "Read"),
        context(1),
        presentation(second, "Edit"),
        presentation(first, "Read notes.txt"),
        context(2),
    ] {
        let event = live(update);
        held.settle_transient(&event.payload);
        held.hold_transient(event);
    }
    let kept = |held: &HeldHistory| {
        held.in_flight
            .iter()
            .filter_map(|event| match &event.payload {
                EventPayload::AcpLiveUpdate { update } => Some(update.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(
        kept(&held),
        [
            presentation(second, "Edit"),
            presentation(first, "Read notes.txt"),
            context(2)
        ]
    );
    let terminated: EventPayload = serde_json::from_value(serde_json::json!({
        "type": "tool_call_terminated",
        "tool_call_id": first,
        "owner": {"model_turn_seq": 1, "content_index": 0, "model_call_id": "call", "provider_item_id": null},
        "outcome": "completed",
        "result": {"title": "Read", "output": "", "display": [], "metadata": null, "attachments": []},
        "error": null,
    }))
    .unwrap();
    held.settle(Some(run), &terminated);
    assert_eq!(kept(&held), [presentation(second, "Edit"), context(2)]);
    held.settle(
        Some(run),
        &EventPayload::run_finished(cookie_agent_protocol::RunOutcome::Cancelled { reason: None }),
    );
    assert!(kept(&held).is_empty());
}

/// The reply cut of each finished run's last block, in transcript order,
/// and the revert cut of each compaction.
fn cuts(state: &SessionState) -> (Vec<u64>, Vec<u64>) {
    let replies = state
        .transcript
        .iter()
        .filter_map(|item| state.reply_cut(item.id()))
        .collect();
    let compactions = state
        .transcript
        .iter()
        .filter_map(|item| match item {
            TranscriptItem::Compaction { seq, .. } => state.compaction_point(*seq),
            _ => None,
        })
        .map(|point| point.through_seq())
        .collect();
    (replies, compactions)
}

#[test]
fn each_reply_ends_before_what_starts_after_it_paged_or_whole() {
    let events = log(CODING);
    let (whole, _) = display_view::display_events(&events, true);
    let expected = reduce_session_events(events[0].session_id, 0, &whole);
    // The runs end at 50, 124, 141, 154, and 168, the session's last event.
    // The third's reply stops at its run's end: a producer's message comes
    // in at 142, before the next run. So does the fourth's: the compaction
    // after it read through 154, and committed at 158.
    assert_eq!(cuts(&expected), (vec![50, 124, 141, 154, 168], vec![154]));
    let mut store = StateStore::default();
    let window = display_view::tail_window(&events, EventsView::Display, 8 * 1024);
    let session_id = open(&mut store, &events, window);
    load_all(&mut store, &events, session_id, 8 * 1024);
    assert_eq!(cuts(&store.sessions[&session_id]), cuts(&expected));
}

#[test]
fn a_reply_has_its_cut_once_its_run_finishes_and_keeps_later_settings() {
    let events = log(CODING);
    let session_id = events[0].session_id;
    let mut store = StateStore::default();
    // The first run, up to its `run_finished` at 50.
    for event in events.iter().take_while(|event| event.seq < 50) {
        assert!(store.apply_event((**event).clone()));
    }
    let last_block = |store: &StateStore| {
        store.sessions[&session_id]
            .transcript
            .iter()
            .rev()
            .find(|item| matches!(item, TranscriptItem::Assistant { .. }))
            .map(TranscriptItem::id)
            .expect("block")
    };
    let item = last_block(&store);
    let blocks = |state: &SessionState| {
        state
            .transcript
            .iter()
            .filter(|item| matches!(item, TranscriptItem::Assistant { .. }))
            .map(TranscriptItem::id)
            .collect::<Vec<_>>()
    };
    assert_eq!(store.sessions[&session_id].reply_cut(item), None);
    assert_eq!(store.sessions[&session_id].reply_end(item), None);
    assert!(store.apply_event((*events[49]).clone()));
    let state = &store.sessions[&session_id];
    assert_eq!(state.reply_cut(item), Some(50));
    // Each of its blocks, the first too, is part of the reply ending there.
    for block in blocks(state) {
        assert_eq!(state.reply_end(block), Some(item));
    }
    // Its text is every text part of the run, whichever block holds it.
    let text = state.reply_text(item).expect("reply text");
    assert!(!text.is_empty() && !text.starts_with('\n'));
    assert_eq!(
        state.reply_point(item),
        Some(crate::cut_points::CutPoint::ReplyEnd {
            through_seq: 50,
            at_tip: true
        })
    );
    // A setting changed after the reply stays with it.
    let mut renamed = (*events[49]).clone();
    renamed.seq = 51;
    renamed.run_id = None;
    renamed.payload = EventPayload::SessionTitleCommitted {
        input_through_seq: 50,
        change: cookie_agent_protocol::SessionTitleChange::UserClear {
            client_rename_id: cookie_agent_protocol::ClientRenameId::new("rename-1")
                .expect("rename id"),
        },
    };
    assert!(store.apply_event(renamed));
    assert_eq!(store.sessions[&session_id].reply_cut(item), Some(51));
}

/// A prompt a checkpoint inside the running run composed replaces the one the
/// system prompt row shows, and the row's fingerprint, which keys its cache,
/// with it; a record of another run changes nothing.
#[test]
fn a_prompt_composed_inside_a_run_is_the_one_its_row_shows() {
    let events = log(CODING);
    let end = events
        .iter()
        .rposition(|event| matches!(event.payload, EventPayload::RunFinished { .. }))
        .unwrap();
    let run = events[end].run_id;
    let mut running = events[..end].to_vec();
    let template = (*running[end - 1]).clone();
    let composed = |seq: u64, run_id, text: &str| {
        Arc::new(StoredEvent {
            seq,
            run_id,
            payload: EventPayload::SystemPromptComposed {
                composed_prompt: text.into(),
                prompt_fingerprint: cookie_agent_protocol::Sha256Digest::of_bytes(text.as_bytes()),
                blocks: None,
            },
            ..template.clone()
        })
    };
    running.push(composed(
        running.last().unwrap().seq + 1,
        Some(cookie_agent_protocol::RunId::new_v7()),
        "another run's",
    ));
    let before = reduce_session_events(running[0].session_id, 0, &running);
    let shown = before.run_snapshot.as_deref().unwrap();
    assert_ne!(shown.composed_prompt, "another run's");
    running.push(composed(
        running.last().unwrap().seq + 1,
        run,
        "composed afresh",
    ));
    let after = reduce_session_events(running[0].session_id, 0, &running);
    let shown = after.run_snapshot.as_deref().unwrap();
    assert_eq!(shown.composed_prompt, "composed afresh");
    assert_eq!(
        shown.prompt_fingerprint,
        cookie_agent_protocol::Sha256Digest::of_bytes(b"composed afresh")
    );
    assert_eq!(
        shown.agent,
        before.run_snapshot.as_deref().unwrap().agent,
        "the rest of the run's agent stays"
    );
}
