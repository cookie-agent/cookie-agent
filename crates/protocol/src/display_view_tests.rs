use std::sync::Arc;

use serde_json::json;

use super::*;
use crate::{AcpSessionUpdate, SessionId};

/// A recorded session (the `coding` golden history): three runs with tool
/// calls, approvals, a compaction, and a 66 KB tool result at seq 36.
const CODING: &str = include_str!(
    "../../engine/tests/fixtures/history/1/coding/01a152fa-2785-7822-b056-5a3ad818855f/events.jsonl"
);

fn coding() -> Vec<Arc<StoredEvent>> {
    CODING
        .lines()
        .map(|line| Arc::new(serde_json::from_str(line).unwrap()))
        .collect()
}

fn kind(event: &StoredEvent) -> String {
    serde_json::to_value(&event.payload).unwrap()["type"]
        .as_str()
        .unwrap()
        .to_owned()
}

fn kinds(events: &[Arc<StoredEvent>]) -> Vec<String> {
    events.iter().map(|event| kind(event)).collect()
}

fn seqs(events: &[Arc<StoredEvent>]) -> Vec<u64> {
    events.iter().map(|event| event.seq).collect()
}

/// `events` with their sequences renumbered from 1, as a log holds them.
fn renumbered(events: impl IntoIterator<Item = StoredEvent>) -> Vec<Arc<StoredEvent>> {
    events
        .into_iter()
        .enumerate()
        .map(|(index, mut event)| {
            event.seq = index as u64 + 1;
            Arc::new(event)
        })
        .collect()
}

fn acp_update(template: &StoredEvent, update: AcpSessionUpdate) -> StoredEvent {
    with_payload(template, EventPayload::AcpSessionUpdated { update })
}

fn commands(name: &str) -> Option<Vec<crate::AcpCommand>> {
    Some(vec![
        serde_json::from_value(json!({"name": name, "description": "a command"})).unwrap(),
    ])
}

fn context(used: u64) -> Option<crate::AcpContextUsage> {
    Some(serde_json::from_value(json!({"used": used, "size": 200_000})).unwrap())
}

fn presentation(call: ToolCallId, title: &str) -> Option<crate::AcpToolPresentation> {
    Some(crate::AcpToolPresentation {
        tool_call_id: call,
        presentation: crate::PreparedPresentation::new(
            crate::SafeDisplayText::new(title).unwrap(),
            None,
        ),
    })
}

#[test]
fn the_view_leaves_out_bookkeeping_and_keeps_what_transcripts_read() {
    let events = coding();
    let (shown, _) = display_events(&events, false);
    let shown_kinds = kinds(&shown);
    let left = kinds(&events)
        .into_iter()
        .filter(|kind| !shown_kinds.contains(kind))
        .collect::<HashSet<_>>();
    // No evaluation in it discards anything.
    assert_eq!(
        left,
        HashSet::from([
            "model_request_prepared".to_owned(),
            "model_replay_evaluated".to_owned()
        ])
    );
    for kind in [
        "model_attempt_started",
        "model_output_started",
        "model_usage_recorded",
        "model_turn_committed",
    ] {
        assert_eq!(
            kinds(&events).iter().filter(|seen| *seen == kind).count(),
            shown_kinds.iter().filter(|seen| *seen == kind).count(),
            "{kind}"
        );
    }
    // A replay evaluation stays only when it discards something, and only
    // the last of the page's published tool lists.
    let evaluated = |events: &[Arc<StoredEvent>]| {
        events
            .iter()
            .filter(|event| matches!(event.payload, EventPayload::ModelReplayEvaluated { .. }))
            .count()
    };
    assert!(evaluated(&events) > 0);
    assert_eq!(evaluated(&shown), 0);
    let tools = shown
        .iter()
        .filter(|event| matches!(event.payload, EventPayload::ModelToolsPublished { .. }))
        .map(|event| event.seq)
        .collect::<Vec<_>>();
    let last_tools = events
        .iter()
        .rfind(|event| matches!(event.payload, EventPayload::ModelToolsPublished { .. }))
        .unwrap()
        .seq;
    assert_eq!(tools, [last_tools]);
}

#[test]
fn a_long_tool_result_is_cut_to_its_first_bytes_and_its_length() {
    let events = coding();
    let long = event_at(&events, 36).unwrap();
    let EventPayload::ToolCallTerminated { termination } = &long.payload else {
        panic!("seq 36 is a tool result");
    };
    let stored = termination.result.as_ref().unwrap();
    // Its display holds most of it.
    assert!(serialized_len(&stored.display) > CUT_ABOVE_BYTES);
    let (cut_event, cut) = shown(long).unwrap();
    assert_eq!(
        cut,
        Some(CutOutput {
            seq: 36,
            bytes: serialized_len(stored) as u64,
        })
    );
    let EventPayload::ToolCallTerminated { termination } = &cut_event.payload else {
        panic!("still a tool result");
    };
    let kept = termination.result.as_ref().unwrap();
    assert!(kept.output.len() <= CUT_KEEP_BYTES);
    assert!(stored.output.starts_with(&kept.output));
    assert!(serialized_len(&kept.display) <= CUT_KEEP_BYTES);
    assert_eq!(kept.metadata, serde_json::Value::Null);
    // It still reads as an event, and a short result is sent as stored.
    let text = serde_json::to_string(&*cut_event).unwrap();
    let _: StoredEvent = serde_json::from_str(&text).unwrap();
    let short = event_at(&events, 18).unwrap();
    let (kept, none) = shown(short).unwrap();
    assert_eq!(none, None);
    let (
        EventPayload::ToolCallTerminated {
            termination: stored,
        },
        EventPayload::ToolCallTerminated { termination: kept },
    ) = (&short.payload, &kept.payload)
    else {
        panic!("seq 18 is a tool result");
    };
    let (stored, kept) = (
        stored.result.as_ref().unwrap(),
        kept.result.as_ref().unwrap(),
    );
    assert_ne!(stored.metadata, serde_json::Value::Null);
    assert_eq!(kept.metadata, serde_json::Value::Null);
    assert_eq!(
        (&kept.output, &kept.display),
        (&stored.output, &stored.display)
    );
    // Any other event is sent as stored.
    let user = events
        .iter()
        .find(|event| matches!(event.payload, EventPayload::UserInputSubmitted { .. }))
        .unwrap();
    assert!(Arc::ptr_eq(&shown(user).unwrap().0, user));
}

#[test]
fn a_cut_never_splits_a_character() {
    let events = coding();
    let long = event_at(&events, 36).unwrap();
    let mut event = (**long).clone();
    let EventPayload::ToolCallTerminated { termination } = &mut event.payload else {
        panic!("seq 36 is a tool result");
    };
    let result = termination.result.as_mut().unwrap();
    result.output = format!("a{}", "é".repeat(CUT_ABOVE_BYTES));
    let (cut_event, cut) = cut_result(&event).unwrap();
    let EventPayload::ToolCallTerminated { termination } = &cut_event.payload else {
        panic!("still a tool result");
    };
    let output = &termination.result.as_ref().unwrap().output;
    assert_eq!(output.len(), CUT_KEEP_BYTES - 1);
    assert!(cut.bytes > 2 * CUT_ABOVE_BYTES as u64);
}

#[test]
fn a_page_drops_acp_settings_and_presentations_a_later_event_replaces() {
    let template = coding()[5].as_ref().clone();
    let call = ToolCallId::new_v7();
    let other = ToolCallId::new_v7();
    let events = renumbered([
        acp_update(
            &template,
            AcpSessionUpdate {
                commands: commands("old"),
                context: context(1),
                ..AcpSessionUpdate::default()
            },
        ),
        acp_update(
            &template,
            AcpSessionUpdate {
                tool_presentation: presentation(call, "first"),
                ..AcpSessionUpdate::default()
            },
        ),
        acp_update(
            &template,
            AcpSessionUpdate {
                tool_presentation: presentation(other, "other"),
                ..AcpSessionUpdate::default()
            },
        ),
        acp_update(
            &template,
            AcpSessionUpdate {
                tool_presentation: presentation(call, "second"),
                commands: commands("new"),
                ..AcpSessionUpdate::default()
            },
        ),
    ]);
    let (shown, cut) = display_events(&events, false);
    assert!(cut.is_empty());
    // The first keeps its context use, which later rows read; the second
    // replaced presentation leaves nothing.
    assert_eq!(seqs(&shown), [1, 3, 4]);
    let EventPayload::AcpSessionUpdated { update } = &shown[0].payload else {
        panic!("an ACP update");
    };
    assert_eq!(update.commands, None);
    assert_eq!(update.context, context(1));
    let EventPayload::AcpSessionUpdated { update } = &shown[2].payload else {
        panic!("an ACP update");
    };
    assert_eq!(update.commands, commands("new"));
    assert_eq!(update.tool_presentation, presentation(call, "second"));
}

#[test]
fn a_forward_page_keeps_its_last_event_for_the_next_cursor() {
    let events = coding();
    let prepared = events
        .iter()
        .position(|event| matches!(event.payload, EventPayload::ModelRequestPrepared { .. }))
        .unwrap();
    let page = &events[..=prepared];
    let (dropped, _) = display_events(page, false);
    assert_ne!(dropped.last().unwrap().seq, page.last().unwrap().seq);
    let (kept, _) = display_events(page, true);
    assert_eq!(kept.last().unwrap().seq, page.last().unwrap().seq);
}

#[test]
fn the_header_holds_the_latest_settings_before_a_window() {
    let events = coding();
    let header = state_events(&events);
    let kinds = kinds(&header);
    assert_eq!(kinds.first().map(String::as_str), Some("session_created"));
    for kind in ["run_started", "model_tools_published", "runtime_attached"] {
        assert!(kinds.iter().any(|seen| seen == kind), "{kind}: {kinds:?}");
    }
    assert_eq!(
        kinds.iter().filter(|kind| *kind == "run_started").count(),
        1
    );
    let latest_run = events
        .iter()
        .rfind(|event| matches!(event.payload, EventPayload::RunStarted { .. }))
        .unwrap();
    assert!(header.iter().any(|event| event.seq == latest_run.seq));
    assert!(header.windows(2).all(|pair| pair[0].seq < pair[1].seq));
}

#[test]
fn the_header_keeps_each_acp_setting_from_its_latest_update_only() {
    let template = coding()[5].as_ref().clone();
    let events = renumbered([
        acp_update(
            &template,
            AcpSessionUpdate {
                commands: commands("old"),
                context: context(1),
                ..AcpSessionUpdate::default()
            },
        ),
        acp_update(
            &template,
            AcpSessionUpdate {
                commands: commands("new"),
                tool_presentation: presentation(ToolCallId::new_v7(), "a call"),
                ..AcpSessionUpdate::default()
            },
        ),
        acp_update(
            &template,
            AcpSessionUpdate {
                context: context(2),
                ..AcpSessionUpdate::default()
            },
        ),
    ]);
    let header = state_events(&events);
    assert_eq!(seqs(&header), [2, 3]);
    let EventPayload::AcpSessionUpdated { update } = &header[0].payload else {
        panic!("an ACP update");
    };
    assert_eq!(
        update,
        &AcpSessionUpdate {
            commands: commands("new"),
            ..AcpSessionUpdate::default()
        }
    );
}

#[test]
fn the_header_keeps_a_hold_and_queued_inputs_until_something_clears_them() {
    let events = coding();
    let template = events[5].as_ref().clone();
    let held = with_payload(
        &template,
        EventPayload::AutomaticRunsHeld {
            reason: Open::Known(crate::HeldReason::Stopped {}),
        },
    );
    let queued = with_payload(
        &template,
        serde_json::from_value(json!({
            "type": "user_input_queued",
            "content": [{"type": "text", "text": "later"}],
            "selection": {"agent": "primary", "model": {"model": "custom.test/group/model", "variant": null}},
            "client_run_id": "queued-1",
        }))
        .unwrap(),
    );
    let submitted = events
        .iter()
        .find(|event| matches!(event.payload, EventPayload::UserInputSubmitted { .. }))
        .unwrap()
        .as_ref()
        .clone();
    let waiting = renumbered([held.clone(), queued.clone()]);
    assert_eq!(seqs(&state_events(&waiting)), [1, 2]);
    let cleared = renumbered([held, queued, submitted]);
    assert_eq!(seqs(&state_events(&cleared)), [2]);
}

#[test]
fn a_tail_window_starts_at_a_run_and_names_the_history_before_it() {
    let events = coding();
    let all = tail_window(&events, EventsView::Display, usize::MAX / 2);
    assert_eq!(all.older_cursor, None);
    assert!(all.header.is_empty());
    assert_eq!(all.events.first().unwrap().seq, 1);
    assert_eq!(all.events.last().unwrap().seq, events.last().unwrap().seq);

    let budget = 24 * 1024;
    let window = tail_window(&events, EventsView::Display, budget);
    let first = window.events.first().unwrap();
    assert!(is_run_boundary(first), "{}", kind(first));
    assert_eq!(window.older_cursor, Some(first.seq));
    assert_eq!(window.gap, None);
    assert_eq!(
        window.events.last().unwrap().seq,
        events.last().unwrap().seq
    );
    let start = events
        .iter()
        .position(|event| event.seq == first.seq)
        .unwrap();
    assert_eq!(window.header, state_events(&events[..start]));
    assert!(Sizes::new(&events, EventsView::Display).sum(start, events.len()) <= budget);
}

#[test]
fn a_run_larger_than_the_budget_sends_its_start_a_gap_and_its_newest_attempts() {
    let events = coding();
    let last_run = events
        .iter()
        .rposition(|event| is_run_boundary(event))
        .unwrap();
    let run_bytes = Sizes::new(&events, EventsView::Display).sum(last_run, events.len());
    let window = tail_window(&events, EventsView::Display, run_bytes / 3);
    let gap = window.gap.expect("the run does not fit");
    assert!(gap.skipped > 0);
    assert_eq!(window.events.first().unwrap().seq, events[last_run].seq);
    assert_eq!(window.older_cursor, Some(events[last_run].seq));
    assert!(window.events.iter().any(|event| matches!(
        event.payload,
        EventPayload::UserInputSubmitted { .. }
    ) && event.seq <= gap.after_seq));
    let after = window
        .events
        .iter()
        .find(|event| event.seq >= gap.before_seq)
        .unwrap();
    assert_eq!(after.seq, gap.before_seq);
    assert!(matches!(
        after.payload,
        EventPayload::ModelAttemptStarted { .. }
    ));
    assert_eq!(
        window.events.last().unwrap().seq,
        events.last().unwrap().seq
    );
    assert!(
        window
            .events
            .windows(2)
            .all(|pair| pair[0].seq < pair[1].seq)
    );
}

#[test]
fn older_pages_read_back_to_the_start_of_the_log() {
    let events = coding();
    let window = tail_window(&events, EventsView::Display, 16 * 1024);
    let mut before = window.older_cursor;
    let mut held = seqs(&window.events);
    let mut pages = 0;
    while let Some(cursor) = before {
        let page = older_page(&events, cursor, EventsView::Display, 16 * 1024);
        assert!(!page.events.is_empty());
        assert!(page.events.last().unwrap().seq < cursor);
        if let Some(older) = page.older_cursor {
            assert_eq!(older, page.events.first().unwrap().seq);
        }
        held.extend(seqs(&page.events));
        before = page.older_cursor;
        pages += 1;
        assert!(pages < events.len(), "paging makes progress");
    }
    held.sort_unstable();
    held.dedup();
    let (whole, _) = display_events(&events, true);
    let mut expected = seqs(&whole);
    // Each page keeps only its own last published tool list.
    held.retain(|seq| {
        !matches!(
            event_at(&events, *seq).unwrap().payload,
            EventPayload::ModelToolsPublished { .. }
        )
    });
    expected.retain(|seq| {
        !matches!(
            event_at(&events, *seq).unwrap().payload,
            EventPayload::ModelToolsPublished { .. }
        )
    });
    assert_eq!(held, expected);
    assert_eq!(
        older_page(&events, 1, EventsView::Display, 1024),
        Window::default()
    );
}

#[test]
fn an_older_page_holds_at_least_one_event() {
    let events = coding();
    let page = older_page(&events, 37, EventsView::Display, 1);
    assert_eq!(seqs(&page.events), [36]);
    assert_eq!(page.older_cursor, Some(36));
}

#[test]
fn a_gap_keeps_an_approval_still_waiting() {
    let events = coding();
    let requested = events
        .iter()
        .position(|event| matches!(event.payload, EventPayload::ApprovalRequested { .. }))
        .unwrap();
    // Cut the log before the approval is answered.
    let open = events[..=requested + 1].to_vec();
    let kept = kept_in_gap(&open[1..requested + 1], &open[requested + 1..]);
    assert!(kept.iter().any(|event| event.seq == events[requested].seq));
    let answered = kept_in_gap(&events[1..requested + 1], &events[requested + 1..]);
    assert!(
        !answered
            .iter()
            .any(|event| event.seq == events[requested].seq)
    );
}

#[test]
fn an_empty_log_has_an_empty_window() {
    assert_eq!(
        tail_window(&[], EventsView::Display, 1024),
        Window::default()
    );
    let _ = SessionId::new_v7();
}

#[test]
fn a_replay_evaluation_that_discards_history_stays() {
    let events = coding();
    let evaluated = events
        .iter()
        .find(|event| matches!(event.payload, EventPayload::ModelReplayEvaluated { .. }))
        .unwrap();
    assert!(leaves_out(&evaluated.payload));
    let mut discarding = evaluated.as_ref().clone();
    let EventPayload::ModelReplayEvaluated {
        ordered_decisions, ..
    } = &mut discarding.payload
    else {
        unreachable!()
    };
    ordered_decisions.push(
        serde_json::from_value(json!({
            "history_index": 3,
            "disposition": {"type": "discarded_invalid_payload", "reason": "unreadable"},
        }))
        .unwrap(),
    );
    assert!(!leaves_out(&discarding.payload));
}

/// A reader takes a `session_created` of any history format (client
/// protocol 1.63.0); a client before 1.63.0, which refuses any but the
/// first, receives the first, as its whole values are.
#[test]
fn a_session_created_of_any_format_reads_and_older_clients_get_the_first() {
    let mut creation =
        serde_json::from_str::<serde_json::Value>(CODING.lines().next().unwrap()).unwrap();
    for format in [1, 2, 3] {
        creation["payload"]["history_format"] = json!(format);
        let event = Arc::new(serde_json::from_value::<StoredEvent>(creation.clone()).unwrap());
        let format_of = |event: &StoredEvent| match event.payload {
            EventPayload::SessionCreated { history_format, .. } => history_format,
            _ => unreachable!(),
        };
        let older = crate::DisplayAudience::new(crate::SemVer::new(1, 62, 0)).stored(&event);
        assert_eq!(format_of(&older), crate::FIRST_HISTORY_FORMAT);
        let current = crate::DisplayAudience::new(crate::CLIENT_PROTOCOL_VERSION).stored(&event);
        assert_eq!(format_of(&current), format);
    }
}

/// The display view leaves out the live state a conversion left out, and
/// keeps every other omitted record.
#[test]
fn omitted_live_state_is_left_out_of_the_display_view() {
    let omitted = |kind: &str| EventPayload::RecordOmitted {
        omitted_type: crate::EventType::new(kind).unwrap(),
    };
    assert!(leaves_out(&omitted("acp_session_updated")));
    assert!(!leaves_out(&omitted("delegation_finished")));
}

/// Every recorded root log of the golden histories.
const FIXTURES: [&str; 7] = [
    CODING,
    include_str!(
        "../../engine/tests/fixtures/history/1/delegation/01a152fa-3f63-7442-8dfc-045922599ed0/events.jsonl"
    ),
    include_str!(
        "../../engine/tests/fixtures/history/1/native-compaction/01a152fa-7378-7941-8d7b-52396ad88f8e/events.jsonl"
    ),
    include_str!(
        "../../engine/tests/fixtures/history/1/native-compaction-fallback/01a152fa-7bec-7cd0-ac0e-84b23de3e730/events.jsonl"
    ),
    include_str!(
        "../../engine/tests/fixtures/history/1/replay/01a152fa-5dca-7581-981a-81e689dfb764/events.jsonl"
    ),
    include_str!(
        "../../engine/tests/fixtures/history/1/revert-fork/01a152fa-6218-7691-aa4b-b406b1dd003d/events.jsonl"
    ),
    include_str!(
        "../../engine/tests/fixtures/history/1/revert-fork/01a152fa-6aa4-7d93-a025-8bfc1f369131/events.jsonl"
    ),
];

fn parsed(log: &str) -> Vec<Arc<StoredEvent>> {
    log.lines()
        .map(|line| Arc::new(serde_json::from_str(line).unwrap()))
        .collect()
}

/// The coding history with ACP settings, a hold, and a queued input among
/// its events: every kind of setting a header keeps.
fn with_settings() -> Vec<Arc<StoredEvent>> {
    let events = coding();
    let template = events[5].as_ref().clone();
    let call = ToolCallId::new_v7();
    let mut mixed = Vec::new();
    for (index, event) in events.iter().enumerate() {
        mixed.push(event.as_ref().clone());
        let update = match index % 23 {
            3 => AcpSessionUpdate {
                commands: commands(&format!("c{index}")),
                context: context(index as u64),
                ..AcpSessionUpdate::default()
            },
            9 => AcpSessionUpdate {
                options: Some(Vec::new()),
                tool_presentation: presentation(call, &format!("p{index}")),
                ..AcpSessionUpdate::default()
            },
            15 => AcpSessionUpdate {
                context: context(index as u64),
                ..AcpSessionUpdate::default()
            },
            _ => continue,
        };
        mixed.push(acp_update(&template, update));
        if index == 40 {
            mixed.push(with_payload(
                &template,
                EventPayload::AutomaticRunsHeld {
                    reason: Open::Known(crate::HeldReason::Stopped {}),
                },
            ));
            mixed.push(with_payload(
                &template,
                serde_json::from_value(json!({
                    "type": "user_input_queued",
                    "content": [{"type": "text", "text": "later"}],
                    "selection": {"agent": "primary", "model": {"model": "custom.test/group/model", "variant": null}},
                    "client_run_id": "queued-1",
                }))
                .unwrap(),
            ));
        }
    }
    renumbered(mixed)
}

fn logs() -> Vec<Vec<Arc<StoredEvent>>> {
    FIXTURES
        .iter()
        .map(|log| parsed(log))
        .chain([with_settings()])
        .collect()
}

#[test]
fn a_header_followed_by_later_events_folds_to_the_header_after_them() {
    for events in logs() {
        for split in 0..=events.len() {
            let header = state_events(&events[..split]);
            for end in split..=events.len() {
                let folded = header
                    .iter()
                    .chain(&events[split..end])
                    .cloned()
                    .collect::<Vec<_>>();
                assert_eq!(
                    state_events(&folded),
                    state_events(&events[..end]),
                    "split {split}, end {end}"
                );
            }
        }
    }
}

#[test]
fn a_window_read_from_a_run_start_is_the_window_of_the_whole_log() {
    for events in logs() {
        let total = Sizes::new(&events, EventsView::Display).sum(0, events.len());
        for budget in [total / 9, total / 4, total / 2, total * 2] {
            for view in [EventsView::Display, EventsView::Full] {
                let whole = tail_window(&events, view, budget);
                let mut answered = 0;
                for start in (1..events.len()).filter(|&index| is_run_boundary(&events[index])) {
                    let header = state_events(&events[..start]);
                    if let Some(window) = tail_window_after(&header, &events[start..], view, budget)
                    {
                        assert_eq!(window, whole, "start {start}, budget {budget}");
                        answered += 1;
                    }
                }
                // The cache's start answers whenever the log does not fit.
                if let Some(start) = cache_start(&events, budget) {
                    let header = state_events(&events[..start]);
                    assert_eq!(
                        tail_window_after(&header, &events[start..], view, budget),
                        Some(whole),
                        "cache start {start}, budget {budget}"
                    );
                    assert!(answered > 0);
                }
            }
        }
    }
}

#[test]
fn a_window_reaching_before_the_events_read_is_not_answered() {
    let events = coding();
    let start = events
        .iter()
        .rposition(|event| is_run_boundary(event))
        .unwrap();
    let header = state_events(&events[..start]);
    assert_eq!(
        tail_window_after(
            &header,
            &events[start..],
            EventsView::Display,
            usize::MAX / 2
        ),
        None
    );
    // Nor one that does not start at a run.
    assert_eq!(
        tail_window_after(&header, &events[start + 1..], EventsView::Display, 1),
        None
    );
    assert_eq!(cache_start(&events, usize::MAX / 2), None);
}

#[test]
fn a_header_delta_merged_into_the_held_header_is_the_older_header() {
    for events in logs() {
        let starts = (0..events.len()).collect::<Vec<_>>();
        for &older in &starts {
            let new = state_events(&events[..older]);
            for &held_at in starts.iter().filter(|&&held_at| held_at > older) {
                let held = state_events(&events[..held_at]);
                let delta = header_delta(&held, new.clone());
                let cursor = (older > 0).then(|| events[older].seq);
                assert_eq!(
                    merge_header(held.clone(), delta, cursor),
                    if cursor.is_some() {
                        new.clone()
                    } else {
                        Vec::new()
                    },
                    "older {older}, held {held_at}"
                );
            }
        }
    }
}

#[test]
fn a_header_delta_leaves_out_what_the_client_holds() {
    let events = with_settings();
    let last = events.len() - 1;
    let held = state_events(&events[..last]);
    let older = state_events(&events[..last - 1]);
    let delta = header_delta(&held, older.clone());
    assert!(
        delta.len() < older.len(),
        "{} of {}",
        delta.len(),
        older.len()
    );
}

fn result_event(output: &str, display: Vec<DisplayBlock>) -> StoredEvent {
    let template = coding()
        .into_iter()
        .find(|event| {
            matches!(&event.payload, EventPayload::ToolCallTerminated { termination }
                if termination.result.is_some())
        })
        .unwrap();
    let EventPayload::ToolCallTerminated { termination } = &template.payload else {
        unreachable!()
    };
    let mut termination = termination.clone();
    let result = termination.result.as_mut().unwrap();
    result.output = output.to_owned();
    result.display = display.into_iter().map(Open::Known).collect();
    with_payload(&template, EventPayload::ToolCallTerminated { termination })
}

/// `event` as a client of 1.64.0 reads it: spanned, sent, and restored.
fn round_trip(event: &StoredEvent) -> (StoredEvent, usize) {
    let Some((sent, spans)) = output_spans(event) else {
        return (event.clone(), 0);
    };
    let mut value = serde_json::to_value(&sent).unwrap();
    restore_output_spans(&mut value, &spans).unwrap();
    (serde_json::from_value(value).unwrap(), spans.len())
}

#[test]
fn display_text_that_repeats_the_output_is_sent_once_and_read_back_whole() {
    let line = "x".repeat(80);
    let output =
        format!("[stdout]\n{line}\nmore\n[stderr]\n{line}!\n1: {line}\n2: second {line}\ntail");
    let event = result_event(
        &output,
        vec![
            serde_json::from_value(json!({
                "type": "terminal",
                "segments": [
                    {"stream": "stdout", "text": format!("{line}\nmore")},
                    {"stream": "stderr", "text": format!("{line}!")},
                    {"stream": "stderr", "text": "short"},
                ],
            }))
            .unwrap(),
            serde_json::from_value(json!({
                "type": "file_excerpt", "path": "a.rs", "start_line": 1,
                "lines": [line.clone(), format!("second {line}")]
            }))
            .unwrap(),
            DisplayBlock::text("not in the output, and long enough to be worth a span here"),
        ],
    );
    let (sent, spans) = output_spans(&event).expect("spans");
    assert_eq!(spans.len(), 3);
    assert!(
        serde_json::to_string(&sent).unwrap().len() < serde_json::to_string(&event).unwrap().len()
    );
    assert_eq!(round_trip(&event), (event, 3));
}

#[test]
fn every_recorded_result_reads_back_whole() {
    let mut spanned = 0;
    for events in logs() {
        for event in events {
            let (read, spans) = round_trip(&event);
            assert_eq!(read, *event);
            spanned += spans;
        }
    }
    assert!(spanned > 0);
}

#[test]
fn a_span_outside_the_output_fails() {
    let event = result_event("short", vec![DisplayBlock::text("")]);
    let mut value = serde_json::to_value(&event).unwrap();
    let span = OutputSpan {
        seq: event.seq,
        block: 0,
        segment: None,
        start: 0,
        end: 99,
    };
    assert!(restore_output_spans(&mut value, &[span]).is_err());
    let span = OutputSpan {
        block: 3,
        end: 2,
        ..span
    };
    assert!(restore_output_spans(&mut value, &[span]).is_err());
}

/// Reads `events` through the readers, each compared with the whole-list answer
/// it replaces.
fn assert_readers_match(events: &[Arc<StoredEvent>]) {
    for budget in [1, 16 * 1024, usize::MAX / 2] {
        for view in [EventsView::Display, EventsView::Full] {
            let mut tail = TailWindowReader::new(view, budget);
            let mut page = OlderPageReader::new(view, budget);
            let mut fold = StateEventsFold::default();
            let mut placed = CacheStartFold::new(budget);
            for (read, event) in events.iter().enumerate() {
                let through = &events[..=read];
                tail.observe(Arc::clone(event));
                page.observe(Arc::clone(event));
                fold.observe(event);
                placed.observe(event);
                if read % 16 != 0 && read + 1 != events.len() {
                    continue;
                }
                let context = format!("{view:?} budget {budget} through {read}");
                assert_eq!(
                    tail.clone().window(),
                    tail_window(through, view, budget),
                    "{context}"
                );
                let before = event.seq + 1;
                assert_eq!(
                    page.clone().finish(),
                    older_page(through, before, view, budget),
                    "{context}"
                );
                assert_eq!(page.header_at_end(), state_events(through), "{context}");
                assert_eq!(fold.clone().finish(), state_events(through), "{context}");
                if view == EventsView::Display {
                    let start = cache_start(through, budget);
                    assert_eq!(
                        tail.clone().cache_start(),
                        start.map(|start| (
                            Arc::clone(&through[start]),
                            state_events(&through[..start])
                        )),
                        "{context}"
                    );
                    // Read one event at a time, or started from the list at
                    // any point and read on from there.
                    let seeded = CacheStartFold::of(through, budget);
                    let mut read_on = CacheStartFold::of(&through[..read / 2], budget);
                    through[read / 2..]
                        .iter()
                        .for_each(|event| read_on.observe(event));
                    for fold in [&placed, &seeded, &read_on] {
                        let Some(placed) = fold.start() else {
                            continue;
                        };
                        // At or before the latest start, and it answers.
                        let at = through
                            .iter()
                            .position(|event| event.seq == placed.seq)
                            .expect("a record of the log");
                        assert!(start.is_some_and(|start| at <= start), "{context}");
                        assert!(is_run_boundary(&through[at]), "{context}");
                        assert_eq!(fold.header(), state_events(&through[..at]), "{context}");
                        assert_eq!(
                            tail_window_after(fold.header(), &through[at..], view, budget),
                            Some(tail_window(through, view, budget)),
                            "{context}"
                        );
                    }
                    assert_eq!(
                        seeded.start().map(|start| start.seq),
                        start.map(|start| through[start].seq),
                        "{context}"
                    );
                }
            }
        }
    }
    let mut count = DisplayCount::default();
    for (read, event) in events.iter().enumerate() {
        count.observe(event);
        assert_eq!(
            count.shown(),
            display_events(&events[..=read], false).0.len() as u64,
            "through {read}"
        );
    }
}

/// A cache's start keeps up with a log read one event at a time, from what it
/// noted, and what it holds does not grow with the number of runs.
#[test]
fn a_cache_start_read_one_event_at_a_time_keeps_up_and_holds_a_few_headers() {
    let events = coding();
    let budget = 16 * 1024;
    let mut fold = CacheStartFold::new(budget);
    let mut moved = std::collections::HashSet::new();
    let mut log = Vec::new();
    for round in 0..40_u64 {
        for event in events.iter().take(40) {
            let mut copy = (**event).clone();
            copy.seq = log.len() as u64 + 1;
            let copy = Arc::new(copy);
            fold.observe(&copy);
            log.push(copy);
            moved.extend(fold.start().map(|start| start.seq));
        }
        assert!(
            fold.noted.len() <= 5,
            "round {round} notes {} starts",
            fold.noted.len()
        );
    }
    assert!(moved.len() > 2, "the start moved {} times", moved.len());
    let latest = cache_start(&log, budget).expect("the log does not fit");
    let placed = fold.start().expect("a start");
    assert!(placed.seq <= log[latest].seq);
    let at = log
        .iter()
        .position(|event| event.seq == placed.seq)
        .unwrap();
    let span: usize = log[at..]
        .iter()
        .map(|event| view_size(event, EventsView::Display))
        .sum();
    assert!(span <= 2 * budget + 64 * 1024, "{span}");
}

#[test]
fn reading_a_log_one_event_at_a_time_gives_what_the_whole_list_gives() {
    assert_readers_match(&coding());
}

#[test]
fn the_state_fold_settles_as_it_reads_and_holds_only_the_header() {
    let created = coding().remove(0);
    let mut events = vec![created];
    for seq in 0..3 * StateEventsFold::SETTLES_EVERY as u64 {
        let mut held = (*events[0]).clone();
        held.seq = seq + 2;
        held.payload = EventPayload::AutomaticRunsResumed {};
        events.push(Arc::new(held));
    }
    let mut fold = StateEventsFold::default();
    let mut most = 0;
    for event in &events {
        fold.observe(event);
        most = most.max(fold.held.len());
    }
    assert!(most <= StateEventsFold::SETTLES_EVERY + 1, "{most}");
    assert_eq!(fold.finish(), state_events(&events));
}
