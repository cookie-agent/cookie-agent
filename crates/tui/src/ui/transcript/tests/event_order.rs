//! Warning and error rows render at the point they occurred relative to the
//! viewed conversation, whether they come from the viewed session itself or
//! from a descendant session, live and after replay alike.

use cookie_agent_protocol::{
    AttemptId, EventPayload, InvocationId, SafeErrorMessage, SessionId, SessionStatus, SessionTree,
    StoredEvent, ToolCallId, ToolTerminationOutcome,
};

use jiff::Timestamp;

use crate::ui::app::App;

use super::support::*;

const WIDTH: u16 = 200;
const HEIGHT: u16 = 200;

fn at(mut event: StoredEvent, second: i64) -> StoredEvent {
    event.timestamp = Timestamp::new(second, 0).expect("timestamp");
    event
}

/// The row index of the single conversation row containing `needle`.
fn row_of(rows: &[String], needle: &str) -> usize {
    let matches = rows
        .iter()
        .enumerate()
        .filter(|(_, row)| row.contains(needle))
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    assert_eq!(
        matches.len(),
        1,
        "{needle:?} renders exactly once in:\n{}",
        rows.join("\n")
    );
    matches[0]
}

fn assert_order(rows: &[String], needles: &[&str]) {
    let positions = needles
        .iter()
        .map(|needle| row_of(rows, needle))
        .collect::<Vec<_>>();
    assert!(
        positions.windows(2).all(|pair| pair[0] < pair[1]),
        "expected {needles:?} in order, got rows {positions:?}:\n{}",
        rows.join("\n")
    );
}

async fn tree_app(root: SessionId, child: SessionId) -> App {
    let mut app = test_app().await;
    app.tree = Some(SessionTree {
        session: titled_meta(root, "root session", 1),
        children: vec![SessionTree {
            session: titled_meta(child, "child session", 1),
            children: Vec::new(),
        }],
    });
    app.tree_root = Some(root);
    app.selected = Some(root);
    app
}

/// Deliver events live, in the global order they occurred.
async fn live_app(root: SessionId, child: SessionId, events: &[StoredEvent]) -> App {
    let mut app = tree_app(root, child).await;
    for event in events {
        app.handle_delivery(live_event(event.clone())).await;
    }
    app
}

/// Reduce each session's log on its own, as an attach/resume replay does:
/// the viewed session's events never interleave with the child's.
async fn replayed_app(root: SessionId, child: SessionId, events: &[StoredEvent]) -> App {
    let mut app = tree_app(root, child).await;
    for session in [root, child] {
        for event in events.iter().filter(|event| event.session_id == session) {
            assert!(app.store.apply_event(event.clone()));
        }
    }
    app
}

/// The conversation pane's rows of a full frame, trimmed of trailing
/// padding. Other panes (the agents tree) legitimately differ between a live
/// view and a replay, so they are left out.
fn conversation_rows(app: &mut App) -> Vec<String> {
    let mut rows = frame_rows(app, WIDTH, HEIGHT)
        .into_iter()
        .map(|row| row.trim_end().to_owned())
        .skip_while(|row| !row.starts_with("╭ Conversation"))
        .take_while(|row| !row.starts_with('╰'))
        .collect::<Vec<_>>();
    // The pane's height follows the other panes; its empty tail does not
    // matter.
    while rows
        .last()
        .is_some_and(|row| row.trim_end_matches(['│', ' ']).is_empty())
    {
        rows.pop();
    }
    rows
}

/// A parent run that delegates in its first turn and resumes in a second
/// turn of the same run after the delegate returns; the child session emits
/// a warning while the delegate tool is still running.
fn delegate_run(root: SessionId, child: SessionId) -> Vec<StoredEvent> {
    let root_run = run_id();
    let first = AttemptId::new_v7();
    let second = AttemptId::new_v7();
    let call = ToolCallId::new_v7();
    let child_run = run_id();
    let child_attempt = AttemptId::new_v7();
    vec![
        at(attempt_started(root, 1, root_run, first, None), 1),
        at(text_delta(root, 2, root_run, first, "delegating now"), 2),
        at(
            turn_committed(
                root,
                3,
                root_run,
                first,
                1,
                vec![text_part("delegating now"), tool_part("delegate-call")],
                Vec::new(),
                None,
            ),
            3,
        ),
        at(
            tool_started_at(
                root,
                4,
                root_run,
                call,
                1,
                "delegate-call",
                1,
                "delegate-call",
                None,
            ),
            4,
        ),
        at(attempt_started(child, 1, child_run, child_attempt, None), 5),
        at(
            turn_committed(
                child,
                2,
                child_run,
                child_attempt,
                1,
                vec![text_part("child output")],
                vec!["child provider hiccup"],
                None,
            ),
            6,
        ),
        at(
            tool_terminated(
                root,
                5,
                root_run,
                call,
                1,
                "delegate-call",
                ToolTerminationOutcome::Completed,
            ),
            7,
        ),
        at(attempt_started(root, 6, root_run, second, None), 8),
        at(text_delta(root, 7, root_run, second, "parent resumes"), 9),
        at(
            turn_committed(
                root,
                8,
                root_run,
                second,
                2,
                vec![text_part("parent resumes")],
                Vec::new(),
                None,
            ),
            10,
        ),
    ]
}

#[tokio::test]
async fn descendant_warning_mid_run_renders_between_parent_turns_live_and_replayed() {
    let root = SessionId::new_v7();
    let child = SessionId::new_v7();
    let events = delegate_run(root, child);
    let mut live = live_app(root, child, &events).await;
    let mut replayed = replayed_app(root, child, &events).await;
    let live_rows = conversation_rows(&mut live);
    let replayed_rows = conversation_rows(&mut replayed);
    for rows in [&live_rows, &replayed_rows] {
        assert_order(
            rows,
            &[
                "delegating now",
                "delegate-call",
                "child provider hiccup",
                "parent resumes",
            ],
        );
    }
    assert_eq!(live_rows, replayed_rows, "live and replayed views agree");
}

#[tokio::test]
async fn descendant_warning_while_parent_streams_renders_identically_live_and_replayed() {
    let root = SessionId::new_v7();
    let child = SessionId::new_v7();
    let root_run = run_id();
    let first = AttemptId::new_v7();
    let child_run = run_id();
    let child_attempt = AttemptId::new_v7();
    let events = vec![
        at(attempt_started(root, 1, root_run, first, None), 1),
        at(
            reasoning_delta(root, 2, root_run, first, "parent thinks"),
            2,
        ),
        at(attempt_started(child, 1, child_run, child_attempt, None), 3),
        at(
            turn_committed(
                child,
                2,
                child_run,
                child_attempt,
                1,
                vec![text_part("child output")],
                vec!["child provider hiccup"],
                None,
            ),
            4,
        ),
        at(text_delta(root, 3, root_run, first, "parent answers"), 5),
        at(
            turn_committed(
                root,
                4,
                root_run,
                first,
                1,
                vec![reasoning_part("parent thinks"), text_part("parent answers")],
                Vec::new(),
                None,
            ),
            6,
        ),
    ];
    let mut live = live_app(root, child, &events).await;
    let mut replayed = replayed_app(root, child, &events).await;
    let live_rows = conversation_rows(&mut live);
    let replayed_rows = conversation_rows(&mut replayed);
    for rows in [&live_rows, &replayed_rows] {
        // A committed turn is placed at its commit, as in-session rows
        // place it: the whole turn lands below a row from mid-generation.
        assert_order(
            rows,
            &["child provider hiccup", "thought", "parent answers"],
        );
    }
    assert_eq!(live_rows, replayed_rows, "live and replayed views agree");
}

#[tokio::test]
async fn descendant_run_failure_renders_in_parent_at_its_time() {
    let root = SessionId::new_v7();
    let child = SessionId::new_v7();
    let mut events = delegate_run(root, child);
    // The child's run fails while the delegate is still running.
    let child_run = events
        .iter()
        .find(|event| event.session_id == child)
        .and_then(|event| event.run_id)
        .expect("child run");
    let failure = at(
        event(
            child,
            3,
            child_run,
            EventPayload::RunFailed {
                error: SafeErrorMessage::new("child provider exploded").expect("error"),
                model_error: None,
                resolved_model: None,
            },
        ),
        6,
    );
    let position = events
        .iter()
        .position(|event| event.session_id == child && event.seq == 2)
        .expect("child commit");
    events.insert(position + 1, failure);
    let mut live = live_app(root, child, &events).await;
    let mut replayed = replayed_app(root, child, &events).await;
    let live_rows = conversation_rows(&mut live);
    let replayed_rows = conversation_rows(&mut replayed);
    for rows in [&live_rows, &replayed_rows] {
        assert_order(
            rows,
            &[
                "delegate-call",
                "child provider hiccup",
                "child provider exploded",
                "parent resumes",
            ],
        );
    }
    assert_eq!(live_rows, replayed_rows, "live and replayed views agree");
}

#[tokio::test]
async fn delegate_failure_between_parent_turns_renders_before_later_turns() {
    let root = SessionId::new_v7();
    let child = SessionId::new_v7();
    let root_run = run_id();
    let first = AttemptId::new_v7();
    let second = AttemptId::new_v7();
    let events = vec![
        at(attempt_started(root, 1, root_run, first, None), 1),
        at(
            turn_committed(
                root,
                2,
                root_run,
                first,
                1,
                vec![text_part("first turn")],
                Vec::new(),
                None,
            ),
            2,
        ),
        // A background delegate reports failure to the parent between two
        // of its turns.
        at(
            event(
                root,
                3,
                root_run,
                EventPayload::DelegateFinishedV2 {
                    invocation_id: InvocationId::new_v7(),
                    session_id: child,
                    short_id: None,
                    status: SessionStatus::Failed,
                    preview: String::new(),
                    total_lines: 0,
                },
            ),
            3,
        ),
        at(attempt_started(root, 4, root_run, second, None), 4),
        at(
            turn_committed(
                root,
                5,
                root_run,
                second,
                2,
                vec![text_part("second turn")],
                Vec::new(),
                None,
            ),
            5,
        ),
    ];
    let mut live = live_app(root, child, &events).await;
    let mut replayed = replayed_app(root, child, &events).await;
    let live_rows = conversation_rows(&mut live);
    let replayed_rows = conversation_rows(&mut replayed);
    for rows in [&live_rows, &replayed_rows] {
        assert_order(rows, &["first turn", "finished: failed", "second turn"]);
    }
    assert_eq!(live_rows, replayed_rows, "live and replayed views agree");
}

#[tokio::test]
async fn descendant_warning_lands_between_tool_rows_by_start_time() {
    let root = SessionId::new_v7();
    let child = SessionId::new_v7();
    let root_run = run_id();
    let attempt = AttemptId::new_v7();
    let (call_a, call_b) = (ToolCallId::new_v7(), ToolCallId::new_v7());
    let child_run = run_id();
    let child_attempt = AttemptId::new_v7();
    let events = vec![
        at(attempt_started(root, 1, root_run, attempt, None), 1),
        at(
            turn_committed(
                root,
                2,
                root_run,
                attempt,
                1,
                vec![
                    text_part("two calls"),
                    tool_part("call-a"),
                    tool_part("call-b"),
                ],
                Vec::new(),
                None,
            ),
            2,
        ),
        at(
            tool_started_at(root, 3, root_run, call_a, 1, "call-a", 1, "call-a", None),
            3,
        ),
        at(attempt_started(child, 1, child_run, child_attempt, None), 4),
        at(
            turn_committed(
                child,
                2,
                child_run,
                child_attempt,
                1,
                Vec::new(),
                vec!["child provider hiccup"],
                None,
            ),
            5,
        ),
        // The second call was queued and only starts after the warning.
        at(
            tool_started_at(root, 4, root_run, call_b, 1, "call-b", 2, "call-b", None),
            6,
        ),
    ];
    let mut live = live_app(root, child, &events).await;
    let mut replayed = replayed_app(root, child, &events).await;
    let live_rows = conversation_rows(&mut live);
    let replayed_rows = conversation_rows(&mut replayed);
    for rows in [&live_rows, &replayed_rows] {
        assert_order(rows, &["call-a", "child provider hiccup", "call-b"]);
    }
    assert_eq!(live_rows, replayed_rows, "live and replayed views agree");
}

#[tokio::test]
async fn tool_hit_regions_follow_rows_spliced_into_their_block() {
    let root = SessionId::new_v7();
    let child = SessionId::new_v7();
    let events = delegate_run(root, child);
    let mut app = replayed_app(root, child, &events).await;
    let rows = frame_rows(&mut app, WIDTH, HEIGHT);
    let tool = app
        .hit_map
        .blocks
        .iter()
        .find(|block| matches!(block.id, crate::ui::transcript::BlockId::Tool(_)))
        .copied()
        .expect("tool row hit region");
    // The region is the tool row itself: it starts on it and does not grow
    // over the warning spliced in right after it.
    let top = usize::from(tool.rect.y);
    let bottom = top + usize::from(tool.rect.height);
    assert!(rows[top].contains("delegate-call"), "{}", rows[top]);
    assert!(
        rows[top..bottom]
            .iter()
            .all(|row| !row.contains("child provider hiccup")),
        "{}",
        rows[top..bottom].join("\n")
    );
}

#[tokio::test]
async fn scrolled_view_stays_put_below_spliced_rows_when_the_layout_grows() {
    let root = SessionId::new_v7();
    let child = SessionId::new_v7();
    let mut events = delegate_run(root, child);
    // A long second turn, so the view can scroll within it.
    let long = (1..=60)
        .map(|line| format!("resumed line {line}"))
        .collect::<Vec<_>>()
        .join("\n\n");
    let commit = events.last_mut().expect("second commit");
    if let EventPayload::ModelTurnCommitted { turn, .. } = &mut commit.payload {
        turn.content = vec![text_part(&long)];
    }
    let mut app = replayed_app(root, child, &events).await;
    let visible = |app: &mut App| {
        frame_rows(app, WIDTH, 40)
            .into_iter()
            .skip_while(|row| !row.starts_with("╭ Conversation"))
            .skip(1)
            .take_while(|row| !row.starts_with('╰'))
            .collect::<Vec<_>>()
    };
    visible(&mut app);
    // Scroll up into the second turn, well below the spliced warning.
    app.conversation_scroll.following = false;
    app.conversation_scroll.offset = app.conversation_scroll.offset.saturating_sub(40);
    let before = visible(&mut app);
    assert!(before.iter().any(|row| row.contains("resumed line")));
    // The run keeps going: the layout grows below the viewport.
    let run = events[0].run_id.expect("root run");
    let attempt = AttemptId::new_v7();
    for event in [
        at(attempt_started(root, 9, run, attempt, None), 12),
        at(text_delta(root, 10, run, attempt, "more output"), 13),
    ] {
        assert!(app.store.apply_event(event));
    }
    assert_eq!(
        visible(&mut app),
        before,
        "the scrolled view does not drift"
    );
}

#[tokio::test]
async fn descendant_rows_follow_the_event_level_filter() {
    let root = SessionId::new_v7();
    let child = SessionId::new_v7();
    let mut events = delegate_run(root, child);
    let child_run = events
        .iter()
        .find(|event| event.session_id == child)
        .and_then(|event| event.run_id)
        .expect("child run");
    events.push(at(
        event(
            child,
            3,
            child_run,
            EventPayload::RunFailed {
                error: SafeErrorMessage::new("child provider exploded").expect("error"),
                model_error: None,
                resolved_model: None,
            },
        ),
        11,
    ));
    let mut app = replayed_app(root, child, &events).await;
    app.tui_config.minimum_event_level = crate::state::EventLevel::Error;
    let rows = conversation_rows(&mut app);
    assert!(
        !rows.iter().any(|row| row.contains("child provider hiccup")),
        "an error filter hides descendant warnings"
    );
    assert_order(&rows, &["parent resumes", "child provider exploded"]);
}
