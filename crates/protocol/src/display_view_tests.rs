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
