use crate::ui::transcript::*;

use cookie_agent_protocol::{ApprovalUserDecision, AttemptId, SessionId, SessionTree};

use crossterm::event::{KeyModifiers, MouseEvent, MouseEventKind};

use ratatui::{Terminal, backend::TestBackend};

use crate::markdown::{MarkdownDocument, PlainHighlighter};

use crate::state::AssistantChild;

use crate::ui::app::*;

use super::support::*;

#[tokio::test]
async fn hover_follows_mouse_moves_and_styles_the_target_cells() {
    let mut app = app_with_approval().await;
    // Draw once to populate the hit map.
    let _ = rendered_frame(&mut app, 120, 40);
    let target = app.hit_map.approval_actions[0].rect;
    let moved = |column: u16, row: u16| MouseEvent {
        kind: MouseEventKind::Moved,
        column,
        row,
        modifiers: KeyModifiers::NONE,
    };

    // Moving onto a button resolves it and asks for one redraw.
    assert!(app.handle_mouse(moved(target.x, target.y)).await);
    assert_eq!(
        app.hover,
        Some(HoverTarget::ApprovalAction(
            ApprovalUserDecision::ApproveOnce
        ))
    );
    // Staying put is not redraw-worthy; moving to the next button is.
    assert!(!app.handle_mouse(moved(target.x, target.y)).await);
    let next = app.hit_map.approval_actions[1].rect;
    assert!(app.handle_mouse(moved(next.x, next.y)).await);
    assert_eq!(
        app.hover,
        Some(HoverTarget::ApprovalAction(ApprovalUserDecision::Reject))
    );
    // While the approval is up it owns the pointer: anywhere off a
    // button clears the hover instead of leaking to content beneath.
    assert!(app.handle_mouse(moved(0, 0)).await);
    assert_eq!(app.hover, None);
    assert!(!app.handle_mouse(moved(0, 0)).await);

    // The hovered button is visibly filled with the glaze hover color.
    app.hover = Some(HoverTarget::ApprovalAction(
        ApprovalUserDecision::ApproveOnce,
    ));
    let backend = TestBackend::new(120, 40);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    terminal
        .draw(|frame| app.draw_for_test(frame))
        .expect("app render");
    let buffer = terminal.backend().buffer();
    let cell = buffer[(target.x.saturating_add(1), target.y.saturating_add(1))].style();
    // Environment-independent: whatever the detected color level, the
    // hovered button carries exactly the theme's glaze hover fill, and
    // it visibly differs from the unhovered cream panel.
    assert_eq!(cell.bg, app.theme.hover_fill().bg, "glaze fill: {cell:?}");
    assert_ne!(cell.bg, app.theme.panel().bg, "fill changed: {cell:?}");
}

#[tokio::test]
async fn hover_only_targets_elements_with_a_click_action() {
    let mut app = test_app().await;
    let session = SessionId::new_v7();
    let child = SessionId::new_v7();
    app.selected = Some(session);
    app.tree_root = Some(session);
    app.store.sessions.insert(
        session,
        assistant_state(vec![AssistantChild::Thinking {
            id: 1,
            version: 0,
            text: "thought".into(),
        }]),
    );
    app.tree = Some(SessionTree {
        session: titled_meta(session, "root", 1),
        children: vec![SessionTree {
            session: delegated_meta(child, session, "worker"),
            children: Vec::new(),
        }],
    });
    rendered_frame(&mut app, 80, 24);
    let moved = |column: u16, row: u16| MouseEvent {
        kind: MouseEventKind::Moved,
        column,
        row,
        modifiers: KeyModifiers::NONE,
    };

    // Passive surfaces stay quiet; collapsible blocks have a click action.
    let input = app.hit_map.input.expect("input hit").rect;
    assert!(
        !app.handle_mouse(moved(input.x.saturating_add(1), input.y.saturating_add(1)))
            .await
    );
    assert_eq!(app.hover, None);
    let block = app.hit_map.blocks.first().copied().expect("block hit");
    assert!(app.handle_mouse(moved(block.rect.x, block.rect.y)).await);
    assert_eq!(app.hover, Some(HoverTarget::TranscriptBlock(block.id)));
    let track = app.hit_map.scrollbar.expect("scrollbar reserved");
    assert!(app.handle_mouse(moved(track.x, track.y)).await);
    assert_eq!(app.hover, None);

    // Elements whose click performs a real action do hover: cycle the
    // permission mode, cycle the event-level filter, select/watch a
    // tree row.
    let mode = app.hit_map.permission_mode.expect("permission mode hit");
    assert!(app.handle_mouse(moved(mode.x, mode.y)).await);
    assert_eq!(app.hover, Some(HoverTarget::PermissionMode));
    let filter = app.hit_map.event_level_filter.expect("event filter hit");
    assert!(app.handle_mouse(moved(filter.x, filter.y)).await);
    assert_eq!(app.hover, Some(HoverTarget::EventLevelFilter));
    let row = app
        .hit_map
        .tree_rows
        .first()
        .copied()
        .expect("tree row hit");
    assert!(app.handle_mouse(moved(row.rect.x, row.rect.y)).await);
    assert_eq!(app.hover, Some(HoverTarget::TreeRow(session)));
}

#[test]
fn transcript_items_get_exactly_one_breathing_row_between_them() {
    let mut state = assistant_state(vec![AssistantChild::Text {
        id: 2,
        version: 0,
        markdown: MarkdownDocument::new("answer".into()),
    }]);
    state
        .transcript
        .insert(0, TranscriptItem::user("question one"));
    let layout = transcript_layout(&state, None, 60);
    let rendered = layout
        .lines
        .iter()
        .map(|line| line.to_string())
        .collect::<Vec<_>>();
    let blanks = rendered
        .iter()
        .enumerate()
        .filter(|(_, line)| line.trim().is_empty())
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    assert_eq!(blanks.len(), 1, "{rendered:?}");
    assert!(
        rendered[..blanks[0]]
            .iter()
            .any(|line| line.contains("USER"))
    );
    assert!(
        rendered[blanks[0] + 1..]
            .iter()
            .any(|line| line.contains("answer"))
    );

    // Filtered-out event rows contribute no lines and no spacer, so
    // hiding diagnostics never leaves stray blank rows behind.
    state.transcript.push(TranscriptItem::Event {
        id: 3,
        version: 0,
        level: crate::state::EventLevel::Debug,
        text: "hidden diagnostic".into(),
    });
    state.transcript.push(TranscriptItem::Event {
        id: 4,
        version: 0,
        level: crate::state::EventLevel::Error,
        text: "visible failure".into(),
    });
    let layout = transcript_layout_with_level(
        &state,
        None,
        60,
        &Theme::default(),
        &PlainHighlighter,
        crate::state::EventLevel::Warning,
    );
    let rendered = layout
        .lines
        .iter()
        .map(|line| line.to_string())
        .collect::<Vec<_>>();
    let blanks = rendered
        .iter()
        .filter(|line| line.trim().is_empty())
        .count();
    assert_eq!(blanks, 2, "{rendered:?}");
    assert!(rendered.iter().any(|line| line.contains("visible failure")));
    assert!(
        !rendered
            .iter()
            .any(|line| line.contains("hidden diagnostic"))
    );
}

#[test]
fn empty_conversation_guidance_wraps_inside_the_pane() {
    for (has_session, headline, hint) in [
        (false, "No session selected.", "/sessions"),
        (true, "Fresh session", "ctrl+p"),
    ] {
        let lines = empty_conversation_lines(has_session, 60, &Theme::default());
        let rendered = snapshot_lines(&lines);
        assert!(rendered.contains(headline), "{rendered}");
        assert!(rendered.contains(hint), "{rendered}");
        for width in [8, 13, 24] {
            for line in empty_conversation_lines(has_session, width, &Theme::default()) {
                assert!(
                    unicode_width::UnicodeWidthStr::width(line.to_string().as_str())
                        <= usize::from(width),
                    "width {width}: {line}"
                );
            }
        }
    }
}

#[tokio::test]
async fn first_launch_and_fresh_session_show_guidance_without_system_prompt() {
    let mut app = test_app().await;
    let rendered = rendered_frame(&mut app, 100, 30);
    assert!(rendered.contains("No session selected."), "{rendered}");
    assert!(rendered.contains("/sessions"), "{rendered}");

    // The creation snapshot is intentionally not rendered before a run.
    let session = SessionId::new_v7();
    assert!(app.store.apply_event(session_created(session, 1)));
    app.selected = Some(session);
    let rendered = rendered_frame(&mut app, 100, 30);
    assert!(!rendered.contains("⚙ ▸ system prompt"), "{rendered}");
    assert!(rendered.contains("Fresh session"), "{rendered}");

    // Once content exists the guidance is gone.
    let run = run_id();
    let attempt = AttemptId::new_v7();
    for event in [
        attempt_started(session, 2, run, attempt, None),
        text_delta(session, 3, run, attempt, "hello"),
    ] {
        assert!(app.store.apply_event(event));
    }
    let rendered = rendered_frame(&mut app, 100, 30);
    assert!(rendered.contains("hello"), "{rendered}");
    assert!(!rendered.contains("Fresh session"), "{rendered}");
}
