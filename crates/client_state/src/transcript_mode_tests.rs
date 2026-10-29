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

#[test]
fn the_subtext_reads_the_newest_work_part() {
    let state = SessionState::default();
    assert_eq!(working_subtext(&state, &[]), "Starting…");
    assert_eq!(working_subtext(&state, &[thinking("")]), "Thinking…");
    assert_eq!(
        working_subtext(&state, &[thinking("**Plan**"), committed_tool("bash")]),
        "bash"
    );
    assert_eq!(
        working_subtext(&state, &[committed_tool("bash"), text("Now the tests.\n")]),
        "Now the tests."
    );
    // A call this state never started says nothing; the part before it does.
    assert_eq!(
        working_subtext(&state, &[thinking("**Plan**"), tool()]),
        "Plan"
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

#[test]
fn a_runs_blocks_fold_into_one_across_its_background_rows() {
    let state = transcript(
        vec![
            block(
                1,
                vec![
                    thinking("**Plan**"),
                    committed_tool("bash"),
                    text("waiting"),
                ],
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
        plan.get(1),
        Some(ItemFold::Block {
            lead: 1,
            reply_start: 3,
            last: false
        })
    );
    assert_eq!(plan.get(2), Some(ItemFold::Row { lead: 1 }));
    assert_eq!(plan.get(3), Some(ItemFold::Row { lead: 1 }));
    assert_eq!(
        plan.get(4),
        Some(ItemFold::Block {
            lead: 1,
            reply_start: 1,
            last: true
        })
    );
    assert_eq!(plan.get(5), None);
    let row = plan
        .work_row(&state, 1, jiff::Timestamp::UNIX_EPOCH)
        .expect("a work row");
    assert_eq!(row.summary.tool_calls, 2);
    // The first block's text between calls is work; the last's reply is not.
    assert_eq!(plan.work_row(&state, 4, jiff::Timestamp::UNIX_EPOCH), None);
}

#[test]
fn another_run_a_shown_row_or_a_warning_ends_a_fold() {
    let blocks = || {
        (
            block(1, vec![committed_tool("bash"), text("one")]),
            block(4, vec![committed_tool("bash"), text("two")]),
        )
    };
    let lone = |plan: &FoldPlan| {
        plan.get(1)
            == Some(ItemFold::Block {
                lead: 1,
                reply_start: 1,
                last: true,
            })
            && plan.get(4)
                == Some(ItemFold::Block {
                    lead: 4,
                    reply_start: 1,
                    last: true,
                })
    };
    // Another run.
    let (first, second) = blocks();
    let state = transcript(vec![first, background_ended(2), second], &[(1, 0), (4, 1)]);
    let plan = FoldPlan::of(&state);
    assert!(lone(&plan));
    assert_eq!(plan.get(2), None);
    // A warning between them.
    let (first, second) = blocks();
    let state = transcript(
        vec![first, event(2, EventLevel::Warning), second],
        &[(1, 0), (4, 0)],
    );
    assert!(lone(&FoldPlan::of(&state)));
    // A user message between them.
    let (first, second) = blocks();
    let state = transcript(
        vec![first, TranscriptItem::user("steer"), second],
        &[(1, 0), (4, 0)],
    );
    assert!(lone(&FoldPlan::of(&state)));
    // Blocks without a run never join.
    let (first, second) = blocks();
    let state = transcript(vec![first, background_ended(2), second], &[]);
    assert!(lone(&FoldPlan::of(&state)));
}

#[test]
fn a_fold_without_thinking_or_tools_is_none() {
    let state = transcript(
        vec![
            block(1, vec![text("one")]),
            background_ended(2),
            block(3, vec![text("two")]),
        ],
        &[(1, 0), (3, 0)],
    );
    let plan = FoldPlan::of(&state);
    assert_eq!(plan.get(1), None);
    assert_eq!(plan.get(2), None);
    assert_eq!(plan.get(3), None);
}
