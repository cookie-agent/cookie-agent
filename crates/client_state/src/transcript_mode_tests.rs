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
