//! The reduced transcript (`docs/site/specs/reduced-transcript.md`): each
//! assistant block's work folded into one row above its reply.

use crate::ui::transcript::*;

use cookie_agent_client_state::transcript_mode::TranscriptMode;
use cookie_agent_protocol::{
    AttemptId, EventPayload, RunId, SessionId, StoredEvent, ToolCallId, ToolTerminationOutcome,
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::state::{EventLevel, SessionState, StateStore};

use super::support::*;

const WIDTH: u16 = 60;

fn at(mut event: StoredEvent, seconds: i64) -> StoredEvent {
    event.timestamp = jiff::Timestamp::new(seconds, 0).expect("timestamp");
    event
}

fn time(seconds: i64) -> jiff::Timestamp {
    jiff::Timestamp::new(seconds, 0).expect("timestamp")
}

/// A run that thinks, runs `call-1` (ending as `outcome`, or still running
/// without one), and answers: its block opens at 100s.
fn worked_events(
    session: SessionId,
    run: RunId,
    call: ToolCallId,
    outcome: Option<ToolTerminationOutcome>,
) -> Vec<StoredEvent> {
    let first = AttemptId::new_v7();
    let second = AttemptId::new_v7();
    let mut events = vec![
        at(attempt_started(session, 1, run, first, None), 100),
        at(
            turn_committed(
                session,
                2,
                run,
                first,
                1,
                vec![
                    reasoning_part("**Reading the code**\nthe reducer splits blocks"),
                    tool_part("call-1"),
                ],
                Vec::new(),
                None,
            ),
            101,
        ),
        at(
            tool_started_at(session, 3, run, call, 1, "call-1", 1, "call-1", None),
            102,
        ),
    ];
    if let Some(outcome) = outcome {
        events.extend([
            at(
                tool_terminated(session, 4, run, call, 1, "call-1", outcome),
                105,
            ),
            at(attempt_started(session, 5, run, second, None), 106),
            at(
                turn_committed(
                    session,
                    6,
                    run,
                    second,
                    2,
                    vec![text_part("All done.")],
                    Vec::new(),
                    None,
                ),
                110,
            ),
        ]);
    }
    events
}

fn finished(session: SessionId, run: RunId) -> StoredEvent {
    at(
        event(
            session,
            7,
            run,
            EventPayload::run_finished(cookie_agent_protocol::RunOutcome::Completed {
                final_text: None,
            }),
        ),
        112,
    )
}

fn state_of(events: Vec<StoredEvent>) -> SessionState {
    let session = events[0].session_id;
    let mut store = StateStore::default();
    for event in events {
        assert!(store.apply_event(event));
    }
    store.sessions.remove(&session).expect("session")
}

fn rendered(
    state: &SessionState,
    expanded: Option<&HashSet<BlockId>>,
    transcript: TranscriptMode,
    now: i64,
) -> String {
    let layout = transcript_layout_viewed(
        state,
        expanded,
        WIDTH,
        &Theme::default(),
        &crate::markdown::SyntectHighlighter::default(),
        TranscriptView {
            minimum_event_level: EventLevel::Warning,
            transcript,
            now: time(now),
        },
        0,
    );
    snapshot_lines(&layout.lines)
}

fn block_id(state: &SessionState) -> u64 {
    assistant_items(state)[0].id()
}

#[test]
fn a_finished_block_folds_its_work_above_its_reply() {
    let (session, run, call) = (SessionId::new_v7(), run_id(), ToolCallId::new_v7());
    let mut events = worked_events(session, run, call, Some(ToolTerminationOutcome::Completed));
    events.push(finished(session, run));
    let state = state_of(events);

    let reduced = rendered(&state, None, TranscriptMode::Reduced, 500);
    assert!(
        reduced.contains("▸ Worked for 12s · 1 tool call"),
        "{reduced}"
    );
    assert!(reduced.contains("All done."), "{reduced}");
    assert!(!reduced.contains("call-1"), "{reduced}");
    assert!(!reduced.contains("💭"), "{reduced}");

    let full = rendered(&state, None, TranscriptMode::Full, 500);
    assert!(!full.contains("Worked for"), "{full}");
    assert!(full.contains("call-1"), "{full}");
    assert!(full.contains("💭"), "{full}");
}

#[test]
fn an_opened_work_row_shows_the_work_as_the_full_mode_does() {
    let (session, run, call) = (SessionId::new_v7(), run_id(), ToolCallId::new_v7());
    let mut events = worked_events(session, run, call, Some(ToolTerminationOutcome::Completed));
    events.push(finished(session, run));
    let state = state_of(events);
    let expanded = HashSet::from([BlockId::Work(block_id(&state))]);

    let opened = rendered(&state, Some(&expanded), TranscriptMode::Reduced, 500);
    assert!(
        opened.contains("▾ Worked for 12s · 1 tool call"),
        "{opened}"
    );
    assert!(opened.contains("call-1"), "{opened}");
    assert!(opened.contains("💭"), "{opened}");
    assert!(opened.contains("All done."), "{opened}");
}

#[test]
fn a_live_block_says_what_it_is_doing_and_ticks() {
    let (session, run, call) = (SessionId::new_v7(), run_id(), ToolCallId::new_v7());
    let state = state_of(worked_events(session, run, call, None));

    let live = rendered(&state, None, TranscriptMode::Reduced, 130);
    let lines = live.lines().collect::<Vec<_>>();
    let row = lines
        .iter()
        .position(|line| line.contains("▸ Working… 30s · 1 tool call"))
        .unwrap_or_else(|| panic!("working row: {live}"));
    assert!(lines[row + 1].contains("call-1"), "{live}");
    let later = rendered(&state, None, TranscriptMode::Reduced, 175);
    assert!(later.contains("▸ Working… 1m 15s"), "{later}");
}

#[test]
fn a_call_waiting_on_approval_stays_out_of_the_fold() {
    let (session, run, call) = (SessionId::new_v7(), run_id(), ToolCallId::new_v7());
    let mut state = state_of(worked_events(session, run, call, None));
    let folded = rendered(&state, None, TranscriptMode::Reduced, 130);
    let mut approval = approval(session);
    approval.tool_call_id = call;
    state.approvals.push(approval);

    let waiting = rendered(&state, None, TranscriptMode::Reduced, 130);
    assert!(
        waiting.contains("Waiting for approval: call-1"),
        "{waiting}"
    );
    // The call's own row shows too: one more line than the folded block.
    assert!(
        waiting.lines().count() > folded.lines().count(),
        "{folded}\n---\n{waiting}"
    );
}

#[test]
fn failed_calls_count_on_the_row() {
    let (session, run, call) = (SessionId::new_v7(), run_id(), ToolCallId::new_v7());
    let mut events = worked_events(session, run, call, Some(ToolTerminationOutcome::Failed));
    events.push(finished(session, run));
    let state = state_of(events);

    let reduced = rendered(&state, None, TranscriptMode::Reduced, 500);
    assert!(
        reduced.contains("▸ Worked for 12s · 1 tool call · 1 failed"),
        "{reduced}"
    );
}

#[test]
fn a_reply_with_no_work_has_no_row() {
    let session = SessionId::new_v7();
    let run = run_id();
    let attempt = AttemptId::new_v7();
    let state = state_of(vec![
        at(attempt_started(session, 1, run, attempt, None), 100),
        at(
            turn_committed(
                session,
                2,
                run,
                attempt,
                1,
                vec![text_part("Just an answer.")],
                Vec::new(),
                None,
            ),
            101,
        ),
        finished(session, run),
    ]);
    let reduced = rendered(&state, None, TranscriptMode::Reduced, 500);
    assert!(!reduced.contains("Worked for"), "{reduced}");
    assert_eq!(reduced, rendered(&state, None, TranscriptMode::Full, 500));
}

#[tokio::test]
async fn the_command_and_ctrl_o_switch_the_mode_and_save_it() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("tui.toml");
    std::fs::write(&path, "# mine\ntheme = \"mono\"\n").expect("write");
    let mut app = test_app().await;
    app.transcript_config_path = Some(path.clone());
    let session = SessionId::new_v7();
    app.expanded_blocks
        .entry(session)
        .or_default()
        .extend([BlockId::Work(1), BlockId::Thinking(2)]);

    run_palette_command(&mut app, "transcript reduced").await;
    assert_eq!(app.tui_config.transcript, TranscriptMode::Reduced);
    assert_eq!(app.status, "transcript: reduced");
    assert_eq!(
        std::fs::read_to_string(&path).expect("read"),
        "# mine\ntheme = \"mono\"\ntranscript = \"reduced\"\n"
    );
    // Work rows fold again; other blocks stay as they were.
    assert_eq!(
        app.expanded_blocks[&session],
        HashSet::from([BlockId::Thinking(2)])
    );

    app.handle_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL))
        .await;
    assert_eq!(app.tui_config.transcript, TranscriptMode::Full);
    assert_eq!(
        std::fs::read_to_string(&path).expect("read"),
        "# mine\ntheme = \"mono\"\ntranscript = \"full\"\n"
    );
}

#[tokio::test]
async fn a_mode_that_cannot_be_saved_still_applies() {
    let directory = tempfile::tempdir().expect("tempdir");
    let mut app = test_app().await;
    // A directory where the file should be cannot be read as one.
    app.transcript_config_path = Some(directory.path().to_owned());
    app.handle_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL))
        .await;
    assert_eq!(app.tui_config.transcript, TranscriptMode::Reduced);
    assert!(
        app.status.starts_with("transcript: reduced (not saved: "),
        "{}",
        app.status
    );
}

/// A run whose agent's background command ends between its two blocks:
/// the first thinks and runs `call-1`, the second thinks and answers.
fn run_split_by_background(session: SessionId, run: RunId, finish: bool) -> Vec<StoredEvent> {
    let call = ToolCallId::new_v7();
    let second = AttemptId::new_v7();
    let mut events = worked_events(session, run, call, None);
    events.extend([
        at(
            tool_terminated(
                session,
                4,
                run,
                call,
                1,
                "call-1",
                ToolTerminationOutcome::Completed,
            ),
            105,
        ),
        at(
            event(
                session,
                5,
                run,
                EventPayload::AcpSessionUpdated {
                    update: cookie_agent_protocol::AcpSessionUpdate {
                        background_ended: Some(cookie_agent_protocol::AcpBackgroundEnded {
                            tool_call_id: ToolCallId::new_v7(),
                            tool: "Bash".into(),
                            display: Some("npm run dev".into()),
                            started_at: time(90),
                            exit_code: Some(0),
                            signal: None,
                        }),
                        ..cookie_agent_protocol::AcpSessionUpdate::default()
                    },
                },
            ),
            106,
        ),
        at(attempt_started(session, 6, run, second, None), 107),
        at(
            turn_committed(
                session,
                7,
                run,
                second,
                2,
                vec![
                    reasoning_part("**Checking the result**"),
                    text_part("All done."),
                ],
                Vec::new(),
                None,
            ),
            110,
        ),
    ]);
    if finish {
        let mut finished = finished(session, run);
        finished.seq = 8;
        events.push(finished);
    }
    events
}

#[test]
fn a_runs_work_folds_into_one_row_across_a_background_command_ending() {
    let (session, run) = (SessionId::new_v7(), run_id());
    let state = state_of(run_split_by_background(session, run, true));
    assert_eq!(
        assistant_items(&state).len(),
        2,
        "the background row splits the run"
    );

    let full = rendered(&state, None, TranscriptMode::Full, 500);
    assert!(full.contains("Background: npm run dev ended"), "{full}");
    assert_eq!(full.matches("╭─").count(), 2, "{full}");

    let reduced = rendered(&state, None, TranscriptMode::Reduced, 500);
    assert!(
        reduced.contains("▸ Worked for 12s · 1 tool call"),
        "{reduced}"
    );
    assert_eq!(reduced.matches("Worked for").count(), 1, "{reduced}");
    assert_eq!(reduced.matches("╭─").count(), 1, "{reduced}");
    assert!(!reduced.contains("Background:"), "{reduced}");
    assert!(reduced.contains("All done."), "{reduced}");

    let expanded = HashSet::from([BlockId::Work(block_id(&state))]);
    let opened = rendered(&state, Some(&expanded), TranscriptMode::Reduced, 500);
    assert!(opened.contains("▾ Worked for 12s"), "{opened}");
    assert!(opened.contains("Background: npm run dev ended"), "{opened}");
    assert!(opened.contains("call-1"), "{opened}");
}

#[test]
fn a_run_split_by_a_background_command_works_in_one_live_row() {
    let (session, run) = (SessionId::new_v7(), run_id());
    let state = state_of(run_split_by_background(session, run, false));
    let live = rendered(&state, None, TranscriptMode::Reduced, 130);
    assert_eq!(live.matches("Working…").count(), 1, "{live}");
    assert!(live.contains("▸ Working… 30s · 1 tool call"), "{live}");
    assert!(live.contains("Checking the result"), "{live}");
    assert!(!live.contains("Worked for"), "{live}");
}
