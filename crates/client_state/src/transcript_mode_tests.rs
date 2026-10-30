use std::time::Duration;

use cookie_agent_protocol::{SafeCode, ToolCallId};

use super::*;
use crate::markdown::MarkdownDocument;

fn text(source: &str) -> AssistantChild {
    AssistantChild::Text {
        id: 0,
        version: 0,
        markdown: MarkdownDocument::new(source.to_owned()),
    }
}

fn thinking(source: &str) -> AssistantChild {
    AssistantChild::Thinking {
        id: 0,
        version: 0,
        text: source.to_owned(),
    }
}

fn tool() -> AssistantChild {
    AssistantChild::Tool {
        call_id: ToolCallId::new_v7(),
    }
}

fn committed_tool(name: &str) -> AssistantChild {
    AssistantChild::CommittedTool {
        turn_seq: 1,
        content_index: 0,
        name: SafeCode::new(name).unwrap(),
    }
}

#[test]
fn modes_round_trip_their_names_and_toggle() {
    for mode in [TranscriptMode::Reduced, TranscriptMode::Full] {
        assert_eq!(TranscriptMode::parse(mode.name()), Some(mode));
        assert_eq!(mode.toggled().toggled(), mode);
        assert_ne!(mode.toggled(), mode);
    }
    assert_eq!(TranscriptMode::parse("Reduced"), None);
    assert_eq!(TranscriptMode::parse(""), None);
}

#[test]
fn the_reply_starts_after_the_last_thinking_part_or_tool_call() {
    // Text alone is all reply: the block shows as in the full mode.
    assert_eq!(reply_start(&[text("hi")]), None);
    assert_eq!(reply_start(&[]), None);
    // Text between calls is work; the text after the last call is reply.
    assert_eq!(
        reply_start(&[thinking("plan"), text("next"), tool(), text("done")]),
        Some(3)
    );
    // Ending on a call leaves no reply.
    assert_eq!(reply_start(&[text("first"), tool()]), Some(2));
    assert_eq!(
        reply_start(&[committed_tool("bash"), text("a"), text("b")]),
        Some(1)
    );
    // Thinking before the answer is work too.
    assert_eq!(reply_start(&[thinking("hmm"), text("answer")]), Some(1));
}

#[test]
fn labels_say_working_worked_or_stopped_with_their_counts() {
    let summary = |live, stopped, seconds, tool_calls, failed| WorkSummary {
        live,
        stopped,
        elapsed: Duration::from_secs(seconds),
        tool_calls,
        failed,
    };
    assert_eq!(
        summary(true, false, 42, 3, 0).label(),
        "Working… 42s · 3 tool calls"
    );
    assert_eq!(summary(true, false, 0, 0, 0).label(), "Working… 0s");
    assert_eq!(
        summary(false, false, 83, 1, 0).label(),
        "Worked for 1m 23s · 1 tool call"
    );
    assert_eq!(
        summary(false, true, 3840, 0, 0).label(),
        "Stopped after 1h 4m"
    );
    assert_eq!(
        summary(false, false, 5, 7, 2).failed_label().as_deref(),
        Some("· 2 failed")
    );
    assert_eq!(summary(false, false, 5, 7, 0).failed_label(), None);
}

#[test]
fn a_thinking_step_is_its_newest_heading_or_bold_line_else_its_newest_line() {
    assert_eq!(
        thinking_step("**Reading the reducer**\nIt splits blocks.\n\n**Checking seals**\nThe end")
            .as_deref(),
        Some("Checking seals")
    );
    assert_eq!(
        thinking_step("## Planning the change\nfirst I will").as_deref(),
        Some("Planning the change")
    );
    assert_eq!(
        thinking_step("plain words\n  then   more  \n\n").as_deref(),
        Some("then more")
    );
    // A line with bold inside is not a step.
    assert_eq!(
        thinking_step("**a** and **b**").as_deref(),
        Some("**a** and **b**")
    );
    assert_eq!(thinking_step("#hashtag").as_deref(), Some("#hashtag"));
    assert_eq!(thinking_step(" \n"), None);
}

fn lines(state: &SessionState, work: &[AssistantChild], room: usize) -> WorkLines {
    work_lines(state, work.iter().map(|child| (1, child)), room)
}

fn shown(text: Option<&str>, tool: Option<&str>) -> WorkLines {
    WorkLines {
        text: text.map(str::to_owned),
        tool: tool.map(str::to_owned),
    }
}

/// A state whose block 1 streams the part `part_id` of `kind`.
fn streaming(part_id: u64, kind: crate::AssistantPartKind) -> SessionState {
    SessionState {
        open_assistant: Some(crate::OpenAssistantProjection {
            item_id: 1,
            part_id,
            kind,
            opened_at: jiff::Timestamp::UNIX_EPOCH,
        }),
        ..SessionState::default()
    }
}

#[test]
fn a_work_row_says_its_latest_text_and_its_latest_call() {
    let state = SessionState::default();
    assert_eq!(lines(&state, &[], 80), shown(Some("Starting…"), None));
    assert_eq!(
        lines(&state, &[thinking("")], 80),
        shown(Some("Thinking…"), None)
    );
    // Both the latest text and the latest call, whichever came last.
    assert_eq!(
        lines(
            &state,
            &[thinking("**Plan**\nfirst"), committed_tool("bash")],
            80
        ),
        shown(Some("Plan"), Some("bash"))
    );
    assert_eq!(
        lines(
            &state,
            &[
                committed_tool("read"),
                text("Now the tests.\n"),
                committed_tool("bash")
            ],
            80
        ),
        shown(Some("Now the tests."), Some("bash"))
    );
    // Filler says nothing.
    assert_eq!(
        lines(
            &state,
            &[thinking("**Plan**"), committed_tool("bash"), text("…")],
            80
        ),
        shown(Some("Plan"), Some("bash"))
    );
    // A call this state never started says nothing; an older one does.
    assert_eq!(
        lines(&state, &[committed_tool("read"), tool()], 80),
        shown(None, Some("read"))
    );
}

#[test]
fn a_streaming_line_waits_until_it_ends_or_fills_the_row() {
    let text_part = |source: &str| AssistantChild::Text {
        id: 7,
        version: 0,
        markdown: MarkdownDocument::new(source.to_owned()),
    };
    let state = streaming(7, crate::AssistantPartKind::Text);
    // The open line is held back; the line before it shows.
    assert_eq!(
        lines(&state, &[text_part("Reading the reducer.\nNow the te")], 80)
            .text
            .as_deref(),
        Some("Reading the reducer.")
    );
    // Before any line ended, an older part's line shows.
    assert_eq!(
        lines(&state, &[thinking("**Plan**"), text_part("Now the te")], 80)
            .text
            .as_deref(),
        Some("Plan")
    );
    // Long enough to be cut anyway, the open line shows.
    assert_eq!(
        lines(&state, &[text_part("done.\nNow the tests")], 10)
            .text
            .as_deref(),
        Some("Now the tests")
    );
    // Once the part stops streaming, its last line shows whole.
    assert_eq!(
        lines(
            &SessionState::default(),
            &[text_part("done.\nNow the te")],
            80
        )
        .text
        .as_deref(),
        Some("Now the te")
    );
    // A thinking part waits the same way, for its step too.
    let thinking_part = |source: &str| AssistantChild::Thinking {
        id: 8,
        version: 0,
        text: source.to_owned(),
    };
    let state = streaming(8, crate::AssistantPartKind::Thinking);
    assert_eq!(
        lines(&state, &[thinking_part("**Plan**\n**Check")], 80)
            .text
            .as_deref(),
        Some("Plan")
    );
    assert_eq!(
        lines(&state, &[thinking_part("**Check")], 80)
            .text
            .as_deref(),
        Some("Thinking…")
    );
}

fn block(id: u64, children: Vec<AssistantChild>) -> TranscriptItem {
    TranscriptItem::Assistant {
        id,
        version: 0,
        attribution: crate::FrozenAssistantAttribution {
            agent: cookie_agent_protocol::AgentId::new("test").expect("agent id"),
            acp_model: None,
            resolved_model: serde_json::from_value(serde_json::json!({
                "provider_id": "test",
                "model_id": "test",
                "adapter_id": "openai-compatible",
                "selection": {"model": "test/test", "variant": null},
                "selection_fingerprint": "a".repeat(64)
            }))
            .expect("resolved model"),
        },
        committed_turn_seq: None,
        child_times: Vec::new(),
        children,
    }
}

fn background_ended(id: u64) -> TranscriptItem {
    TranscriptItem::AgentBackgroundEnded {
        id,
        seq: id,
        ended_at: jiff::Timestamp::UNIX_EPOCH,
        ended: serde_json::from_value(serde_json::json!({
            "tool_call_id": ToolCallId::new_v7(),
            "tool": "Bash",
            "started_at": "2026-10-29T10:00:00Z"
        }))
        .expect("background ended"),
    }
}

fn event(id: u64, level: EventLevel) -> TranscriptItem {
    TranscriptItem::Event {
        id,
        version: 0,
        level,
        text: "note".into(),
        repeat: 1,
    }
}

/// A state of `items`, each block in `runs` of the run at its index.
fn transcript(items: Vec<TranscriptItem>, runs: &[(u64, usize)]) -> SessionState {
    let ids = [
        cookie_agent_protocol::RunId::new_v7(),
        cookie_agent_protocol::RunId::new_v7(),
    ];
    SessionState {
        transcript: items,
        assistant_runs: runs.iter().map(|(id, run)| (*id, ids[*run])).collect(),
        ..SessionState::default()
    }
}

fn key(item: u64, child: u32) -> WorkKey {
    WorkKey { item, child }
}

fn work(item: u64, child: u32, starts: bool) -> ChildFold {
    ChildFold::Work {
        key: key(item, child),
        starts,
    }
}

fn folded(first: bool, last: bool, children: Vec<ChildFold>) -> Option<ItemFold> {
    Some(ItemFold::Block(BlockFold {
        first,
        last,
        children,
    }))
}

/// Where `item` stands in `plan`.
fn at(plan: &FoldPlan, item: u64) -> Option<ItemFold> {
    plan.get(item).cloned()
}

#[test]
fn a_runs_blocks_fold_into_one_across_its_background_rows() {
    let state = transcript(
        vec![
            block(
                1,
                vec![thinking("**Plan**"), committed_tool("bash"), text("...")],
            ),
            background_ended(2),
            event(3, EventLevel::Info),
            block(4, vec![committed_tool("bash"), text("All done.")]),
            // A row after the run's last block stays out.
            background_ended(5),
            TranscriptItem::user("next"),
        ],
        &[(1, 0), (4, 0)],
    );
    let plan = FoldPlan::of(&state);
    assert_eq!(
        at(&plan, 1),
        folded(
            true,
            false,
            vec![work(1, 0, true), work(1, 0, false), work(1, 0, false)]
        )
    );
    assert_eq!(at(&plan, 2), Some(ItemFold::Row { key: key(1, 0) }));
    assert_eq!(at(&plan, 3), Some(ItemFold::Row { key: key(1, 0) }));
    assert_eq!(
        at(&plan, 4),
        folded(false, true, vec![work(1, 0, false), ChildFold::Shown])
    );
    assert_eq!(at(&plan, 5), None);
    assert_eq!(plan.keys().collect::<Vec<_>>(), [key(1, 0)]);
    let row = plan
        .work_row(&state, key(1, 0), jiff::Timestamp::UNIX_EPOCH, 80)
        .expect("a work row");
    assert_eq!(row.summary.tool_calls, 2);
    assert_eq!(
        plan.work_row(&state, key(4, 0), jiff::Timestamp::UNIX_EPOCH, 80),
        None
    );
}

#[test]
fn only_ellipses_or_blank_text_is_filler() {
    for source in ["", " \n", "…", "...", " … \n…"] {
        assert!(filler(source), "{source:?}");
    }
    for source in ["Checking the reducer next.", "…done", "Plan:\n- read"] {
        assert!(!filler(source), "{source:?}");
    }
}

#[test]
fn any_text_between_calls_breaks_the_work_into_two_folds() {
    let state = transcript(
        vec![
            block(1, vec![committed_tool("read"), text("Now the tests.")]),
            background_ended(2),
            block(
                3,
                vec![thinking("**Next**"), committed_tool("bash"), text("Done.")],
            ),
        ],
        &[(1, 0), (3, 0)],
    );
    let plan = FoldPlan::of(&state);
    assert_eq!(
        at(&plan, 1),
        folded(true, false, vec![work(1, 0, true), ChildFold::Shown])
    );
    // The row after the text is the next fold's.
    assert_eq!(at(&plan, 2), Some(ItemFold::Row { key: key(3, 0) }));
    assert_eq!(
        at(&plan, 3),
        folded(
            false,
            true,
            vec![work(3, 0, true), work(3, 0, false), ChildFold::Shown]
        )
    );
    let mut keys = plan.keys().collect::<Vec<_>>();
    keys.sort();
    assert_eq!(keys, [key(1, 0), key(3, 0)]);
    // While the run goes on, the text after its last call breaks the work
    // too, and the fold before it is done.
    let mut live = transcript(
        vec![block(
            1,
            vec![committed_tool("read"), text("Now the tests.")],
        )],
        &[(1, 0)],
    );
    let Some(TranscriptItem::Assistant { attribution, .. }) = live.transcript.first() else {
        unreachable!()
    };
    live.open_run_assistant = Some(crate::RunAssistantProjection {
        run_id: live.assistant_runs[&1],
        item_id: 1,
        committed_prefix: 0,
        current_model: attribution.resolved_model.clone(),
        split_pending: false,
        split_at: None,
    });
    let plan = FoldPlan::of(&live);
    assert_eq!(
        at(&plan, 1),
        folded(true, true, vec![work(1, 0, true), ChildFold::Shown])
    );
    let row = plan
        .work_row(&live, key(1, 0), jiff::Timestamp::UNIX_EPOCH, 80)
        .expect("work row");
    assert!(!row.summary.live);
    assert_eq!(row.lines, None);
    // Filler and thinking never break the work.
    let state = transcript(
        vec![block(
            1,
            vec![
                thinking("First paragraph.\n\nSecond paragraph."),
                committed_tool("read"),
                text("…"),
                committed_tool("bash"),
                text("Done."),
            ],
        )],
        &[(1, 0)],
    );
    assert_eq!(
        at(&FoldPlan::of(&state), 1),
        folded(
            true,
            true,
            vec![
                work(1, 0, true),
                work(1, 0, false),
                work(1, 0, false),
                work(1, 0, false),
                ChildFold::Shown
            ]
        )
    );
}

#[test]
fn another_run_a_shown_row_or_a_warning_ends_a_stretch() {
    let blocks = || {
        (
            block(1, vec![committed_tool("bash"), text("one")]),
            block(4, vec![committed_tool("bash"), text("two")]),
        )
    };
    let apart = |plan: &FoldPlan| {
        at(plan, 1) == folded(true, true, vec![work(1, 0, true), ChildFold::Shown])
            && at(plan, 4) == folded(true, true, vec![work(4, 0, true), ChildFold::Shown])
    };
    // Another run.
    let (first, second) = blocks();
    let state = transcript(vec![first, background_ended(2), second], &[(1, 0), (4, 1)]);
    let plan = FoldPlan::of(&state);
    assert!(apart(&plan));
    assert_eq!(at(&plan, 2), None);
    // A warning between them.
    let (first, second) = blocks();
    let state = transcript(
        vec![first, event(2, EventLevel::Warning), second],
        &[(1, 0), (4, 0)],
    );
    assert!(apart(&FoldPlan::of(&state)));
    // A user message between them.
    let (first, second) = blocks();
    let state = transcript(
        vec![first, TranscriptItem::user("steer"), second],
        &[(1, 0), (4, 0)],
    );
    assert!(apart(&FoldPlan::of(&state)));
    // Blocks without a run never join.
    let (first, second) = blocks();
    let state = transcript(vec![first, background_ended(2), second], &[]);
    assert!(apart(&FoldPlan::of(&state)));
}

#[test]
fn a_stretch_without_thinking_or_tools_is_not_folded() {
    let state = transcript(
        vec![
            block(1, vec![text("one")]),
            background_ended(2),
            block(3, vec![text("two")]),
        ],
        &[(1, 0), (3, 0)],
    );
    let plan = FoldPlan::of(&state);
    assert_eq!(at(&plan, 1), None);
    assert_eq!(at(&plan, 2), None);
    assert_eq!(at(&plan, 3), None);
}
