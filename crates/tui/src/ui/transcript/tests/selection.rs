use std::sync::{Arc, Mutex};

use crate::ui::transcript::*;

use cookie_agent_protocol::{EventPayload, SessionId};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEventKind};

use ratatui::{Terminal, backend::TestBackend, text::Line};

use crate::client::ClientDelivery;

use crate::markdown::{MarkdownDocument, PlainHighlighter};

use crate::state::SessionState;

use crate::theme::{ColorLevel, ThemeKind};

use crate::ui::app::*;

use super::support::*;

#[test]
fn osc52_sequence_frames_base64_clipboard_text() {
    assert_eq!(osc52_sequence("hello"), "\x1b]52;c;aGVsbG8=\x07");
    assert_eq!(osc52_sequence(""), "\x1b]52;c;\x07");
}

#[test]
fn extraction_strips_chrome_and_copies_raw_content() {
    let theme = Theme::default();
    // One user block, then an assistant block with prose and a fenced
    // code block — exactly what render_conversation chains.
    let mut lines = role_block(
        Role::User,
        vec![Line::from("raw question".to_owned())],
        60,
        &theme,
    );
    lines.push(Line::default());
    lines.extend(assistant_header("primary • test-model", 60, &theme));
    lines.extend(
        crate::markdown::render_markdown_width(
            &MarkdownDocument::new(
                "some prose\n\n```rust\nfn main() {\n    let x = 1;\n}\n```\n\ntrail".into(),
            ),
            &theme,
            &PlainHighlighter,
            58,
        )
        .into_iter()
        .flat_map(|line| assistant_body_line(line, 60, &theme)),
    );
    let end = (lines.len() - 1, u16::MAX);
    let extracted = extract_selection(&lines, (0, 0), end, &theme);
    assert_eq!(
        extracted, "raw question\n\nsome prose\n\nfn main() {\n    let x = 1;\n}\n\ntrail",
        "gutters stripped, fence borders and role headers gone, code raw:\n{extracted:?}"
    );
}

#[test]
fn code_band_markers_never_reach_copied_text() {
    let theme = Theme::default();
    // Narrow enough that the long line wraps onto a `↪` continuation, and
    // with leading indentation that must survive the marker stripping.
    let lines = crate::markdown::render_markdown_width(
        &MarkdownDocument::new("```sh\n  echo abcdefghijklmnop\n```".into()),
        &theme,
        &PlainHighlighter,
        16,
    )
    .into_iter()
    .flat_map(|line| assistant_body_line(line, 18, &theme))
    .collect::<Vec<_>>();
    let rendered = lines.iter().map(ToString::to_string).collect::<Vec<_>>();
    // The fence language picks the highlighter but is never shown.
    assert!(
        !rendered.iter().any(|line| line.trim_end().ends_with(" sh")),
        "{rendered:?}"
    );
    assert!(
        rendered.iter().any(|line| line.contains('↪')),
        "{rendered:?}"
    );
    let extracted = extract_selection(&lines, (0, 0), (lines.len() - 1, u16::MAX), &theme);
    assert_eq!(extracted, "  echo abcdefgh\nijklmnop");
}

#[test]
fn assistant_header_leads_with_the_agent_and_mutes_the_model() {
    let theme = Theme::default();
    let header = assistant_header("primary • test-model[base]", 60, &theme);
    assert_eq!(header.len(), 1);
    let spans = &header[0].spans;
    assert_eq!(
        header[0].to_string().trim_end(),
        "╭─ primary • test-model[base]"
    );
    assert_eq!(spans[0].content, "╭─ primary");
    assert_eq!(spans[0].style, theme.assistant());
    assert_eq!(spans[1].content, " • test-model[base]");
    assert_eq!(spans[1].style, theme.muted());
}

#[test]
fn extraction_column_windows_cut_on_grapheme_boundaries() {
    let theme = Theme::default();
    let lines = role_block(
        Role::User,
        vec![Line::from("abcdef".to_owned())],
        60,
        &theme,
    );
    // The body row is "│ abcdef": display column 2 is 'a'. Selecting
    // columns 4..7 of the rendered line yields "cde" — the gutter is
    // skipped by the coordinate shift, never copied.
    let body_row = 1;
    assert_eq!(
        extract_selection(&lines, (body_row, 4), (body_row, 7), &theme),
        "cde"
    );
    // An empty window extracts nothing.
    assert!(extract_selection(&lines, (body_row, 4), (body_row, 4), &theme).is_empty());
}

#[test]
fn wrapped_user_gutters_repeat_and_copy_without_chrome() {
    let theme = Theme::default();
    let lines = role_block(
        Role::User,
        vec![Line::from(
            "a user line long enough to wrap at this width for sure",
        )],
        24,
        &theme,
    );
    assert!(lines.len() > 2);
    assert!(
        lines
            .iter()
            .skip(1)
            .all(|line| line.to_string().starts_with("│ "))
    );
    let text = lines
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    insta::assert_snapshot!(text);
    let copied = extract_selection(&lines, (0, 0), (lines.len() - 1, u16::MAX), &theme);
    assert!(!copied.contains('│'));
    assert!(copied.contains("enough to wrap at this"));
}

#[test]
fn extraction_keeps_code_indentation_behind_real_gutters() {
    let theme = Theme::default();
    // A fenced code line whose content begins with a two-space span
    // (indentation split from the rest by highlighting): continuation
    // indents are chrome only in span position 0, so the code keeps
    // its leading spaces.
    let mut lines = crate::markdown::render_markdown_width(
        &MarkdownDocument::new("```\n  indented\n```".into()),
        &theme,
        &PlainHighlighter,
        58,
    )
    .into_iter()
    .flat_map(|line| assistant_body_line(line, 60, &theme))
    .collect::<Vec<_>>();
    // A wrapped user row repeats its gutter; copying strips every gutter.
    lines.extend(role_block(
        Role::User,
        vec![Line::from(
            "a user line long enough to wrap at this width for sure".to_owned(),
        )],
        24,
        &theme,
    ));
    let end = (lines.len() - 1, u16::MAX);
    let extracted = extract_selection(&lines, (0, 0), end, &theme);
    assert!(
        extracted.contains("  indented"),
        "code indentation preserved: {extracted:?}"
    );
    // One copied line per rendered row: the wrapped user row's
    // continuations strip their gutter and join with
    // newlines, exactly as displayed.
    assert!(
        extracted.contains("\nenough to wrap at this"),
        "continuation gutter stripped: {extracted:?}"
    );
}

#[test]
fn extraction_keeps_high_contrast_content_sharing_the_border_foreground() {
    let theme = Theme::new(ThemeKind::HighContrast, ColorLevel::Ansi16);
    let border = theme.code_border();
    // High contrast paints fence grids and syntect's quantized plain
    // code foreground the same white; only the border's DIM|BOLD set
    // still distinguishes a chrome row from content.
    assert_eq!(border.fg, Some(ratatui::style::Color::White));
    let plain_code = Style::default().fg(ratatui::style::Color::White);
    let lines = vec![
        Line::from(vec![
            Span::styled("│ ", border),
            Span::styled("┌─ code: rust", border),
        ]),
        Line::from(vec![
            Span::styled("│ ", border),
            Span::styled("let answer = 42;", plain_code),
        ]),
        Line::from(vec![Span::styled("│ ", border), Span::styled("└─", border)]),
    ];
    let extracted = extract_selection(&lines, (0, 0), (2, u16::MAX), &theme);
    assert_eq!(
        extracted, "let answer = 42;",
        "border rows vanish, same-foreground content stays: {extracted:?}"
    );
}

#[tokio::test]
async fn conversation_drag_selects_and_ctrl_c_copies_raw_text() {
    let (mut app, _, copied) = app_with_user_messages().await;
    rendered_frame(&mut app, 80, 24);
    let viewport = app.hit_map.conversation.expect("viewport");
    let body_row = viewport.y + 1; // first body line of message seq 2
    app.handle_mouse(mouse(
        MouseEventKind::Down(MouseButton::Left),
        viewport.x,
        body_row,
    ))
    .await;
    assert!(app.selection.is_none(), "a press alone selects nothing");
    app.handle_mouse(mouse(
        MouseEventKind::Drag(MouseButton::Left),
        viewport.x + 20,
        body_row,
    ))
    .await;
    assert_eq!(
        app.selection,
        Some(TextSelection::Conversation {
            anchor: (1, 0),
            head: (1, 20),
        })
    );
    app.handle_mouse(mouse(
        MouseEventKind::Up(MouseButton::Left),
        viewport.x + 20,
        body_row,
    ))
    .await;
    assert!(
        app.selection.is_some(),
        "a finished drag keeps its selection"
    );
    // ctrl+c copies the raw content and retires the selection.
    app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL))
        .await;
    assert!(app.selection.is_none());
    assert_eq!(
        copied.lock().expect("capture").as_slice(),
        ["first question"]
    );
}

#[tokio::test]
async fn sub_threshold_drag_stays_a_click_and_opens_the_menu() {
    let (mut app, _, _) = app_with_user_messages().await;
    rendered_frame(&mut app, 80, 24);
    let hit = user_hit(&app, 2);
    let (x, y) = (hit.rect.x + 2, hit.rect.y);
    // A one-cell wobble between press and release is a click, not a
    // selection: the press dispatches on release.
    app.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), x, y))
        .await;
    app.handle_mouse(mouse(MouseEventKind::Drag(MouseButton::Left), x + 1, y))
        .await;
    assert!(app.selection.is_none());
    app.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), x + 1, y))
        .await;
    assert!(app.selection.is_none());
    assert_eq!(app.modal, Modal::UserMessage, "the click dispatched");
}

#[tokio::test]
async fn plain_click_and_esc_each_clear_a_finished_selection() {
    let (mut app, _, _) = app_with_user_messages().await;
    rendered_frame(&mut app, 80, 24);
    let viewport = app.hit_map.conversation.expect("viewport");
    drag_selection(&mut app, viewport, 8, 1).await;
    assert!(app.selection.is_some());
    // A fresh press anywhere clears the selection before doing its
    // work. (The blank spacer row between the two messages carries no
    // click action of its own.)
    app.handle_mouse(mouse(
        MouseEventKind::Down(MouseButton::Left),
        viewport.x,
        viewport.y + 2,
    ))
    .await;
    assert!(app.selection.is_none());
    app.handle_mouse(mouse(
        MouseEventKind::Up(MouseButton::Left),
        viewport.x,
        viewport.y + 2,
    ))
    .await;
    assert_eq!(app.modal, Modal::None, "nothing opened from a blank row");
    drag_selection(&mut app, viewport, 8, 1).await;
    assert!(app.selection.is_some());
    // Esc retires the selection without touching the escape-cancel
    // streak or the run.
    app.last_escape = Some(std::time::Instant::now());
    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
        .await;
    assert!(app.selection.is_none());
    assert_eq!(app.last_escape, None);
}

#[tokio::test]
async fn ctrl_c_without_a_selection_still_cancels_the_active_run() {
    let (mut app, _session, _run) = app_with_active_run().await;
    let (client, recorded, _incoming) = live_recording_client();
    app.client = client;
    app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL))
        .await;
    wait_for_recorded_request(&recorded, "run.cancel", 1).await;
    assert!(app.selection.is_none());
}

#[tokio::test]
async fn composer_drag_and_ctrl_x_cuts_the_selected_draft_text() {
    let mut app = test_app().await;
    let copied = Arc::new(Mutex::new(Vec::new()));
    app.clipboard_sink = ClipboardSink::Capture(copied.clone());
    app.selected = Some(SessionId::new_v7());
    app.input.set_buffer("hello world".to_owned());
    rendered_frame(&mut app, 80, 24);
    let text_rect = app.hit_map.input.expect("input").text_rect;
    // The draft is a single visual row: drag from the 'w' cell to past
    // the 'r' cell to select "wor".
    app.handle_mouse(mouse(
        MouseEventKind::Down(MouseButton::Left),
        text_rect.x + 6,
        text_rect.y,
    ))
    .await;
    app.handle_mouse(mouse(
        MouseEventKind::Drag(MouseButton::Left),
        text_rect.x + 9,
        text_rect.y,
    ))
    .await;
    assert_eq!(
        app.selection,
        Some(TextSelection::Composer { anchor: 6, head: 9 })
    );
    app.handle_mouse(mouse(
        MouseEventKind::Up(MouseButton::Left),
        text_rect.x + 9,
        text_rect.y,
    ))
    .await;
    app.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL))
        .await;
    assert_eq!(copied.lock().expect("capture").as_slice(), ["wor"]);
    assert_eq!(app.input.as_str(), "hello ld");
    assert!(app.selection.is_none());
}

#[tokio::test]
async fn composer_selection_is_deleted_by_backspace_and_delete() {
    for (code, modifiers) in [
        (KeyCode::Backspace, KeyModifiers::NONE),
        (KeyCode::Delete, KeyModifiers::NONE),
        (KeyCode::Backspace, KeyModifiers::CONTROL),
        (KeyCode::Delete, KeyModifiers::CONTROL),
    ] {
        let mut app = test_app().await;
        let copied = Arc::new(Mutex::new(Vec::new()));
        app.clipboard_sink = ClipboardSink::Capture(copied.clone());
        app.selected = Some(SessionId::new_v7());
        app.input.set_buffer("hello world".to_owned());
        rendered_frame(&mut app, 80, 24);
        let text_rect = app.hit_map.input.expect("input").text_rect;
        // Drag right to left this time: the range is normalized either way.
        for kind in [
            MouseEventKind::Down(MouseButton::Left),
            MouseEventKind::Drag(MouseButton::Left),
        ] {
            let column = if matches!(kind, MouseEventKind::Down(_)) {
                9
            } else {
                6
            };
            app.handle_mouse(mouse(kind, text_rect.x + column, text_rect.y))
                .await;
        }
        app.handle_mouse(mouse(
            MouseEventKind::Up(MouseButton::Left),
            text_rect.x + 6,
            text_rect.y,
        ))
        .await;
        assert_eq!(
            app.selection,
            Some(TextSelection::Composer { anchor: 9, head: 6 })
        );
        app.handle_key(KeyEvent::new(code, modifiers)).await;
        assert_eq!(app.input.as_str(), "hello ld", "{code:?} {modifiers:?}");
        assert!(app.selection.is_none());
        assert!(
            copied.lock().expect("capture").is_empty(),
            "deleting is not cutting"
        );
        // The cursor sits in the gap, so typing fills it.
        app.handle_key(KeyEvent::new(KeyCode::Char('X'), KeyModifiers::NONE))
            .await;
        assert_eq!(app.input.as_str(), "hello Xld");
    }
}

#[tokio::test]
async fn paste_over_a_composer_selection_replaces_it() {
    let mut app = test_app().await;
    app.selected = Some(SessionId::new_v7());
    app.input.set_buffer("hello world".to_owned());
    rendered_frame(&mut app, 80, 24);
    let text_rect = app.hit_map.input.expect("input").text_rect;
    // Select "wor" right to left; the range is normalized either way.
    composer_drag_selection(&mut app, text_rect, 9, 6).await;
    assert_eq!(
        app.selection,
        Some(TextSelection::Composer { anchor: 9, head: 6 })
    );
    app.handle_paste("brave new\r\nwo");
    assert_eq!(app.input.as_str(), "hello brave new\nwold");
    assert!(app.selection.is_none());
    // The cursor ends after the pasted text, so typing continues there.
    app.handle_key(KeyEvent::new(KeyCode::Char('X'), KeyModifiers::NONE))
        .await;
    assert_eq!(app.input.as_str(), "hello brave new\nwoXld");

    // Without a selection a paste still inserts at the cursor.
    let mut plain = test_app().await;
    plain.selected = Some(SessionId::new_v7());
    plain.input.set_buffer("ab".to_owned());
    plain.handle_paste("-");
    assert_eq!(plain.input.as_str(), "ab-");
}

#[tokio::test]
async fn composer_click_without_drag_places_the_cursor_as_before() {
    let mut app = test_app().await;
    app.selected = Some(SessionId::new_v7());
    app.input.set_buffer("hello world".to_owned());
    rendered_frame(&mut app, 80, 24);
    let text_rect = app.hit_map.input.expect("input").text_rect;
    app.handle_mouse(mouse(
        MouseEventKind::Down(MouseButton::Left),
        text_rect.x + 5,
        text_rect.y,
    ))
    .await;
    app.handle_mouse(mouse(
        MouseEventKind::Up(MouseButton::Left),
        text_rect.x + 5,
        text_rect.y,
    ))
    .await;
    assert!(app.selection.is_none());
    assert_eq!(app.input.cursor_byte(), 5, "the click moved the cursor");
    assert!(app.composer_focused());
}

#[tokio::test]
async fn conversation_selection_retires_on_session_switch() {
    let (mut app, _session_a, copied) = app_with_user_messages().await;
    let session_b = SessionId::new_v7();
    app.store
        .sessions
        .insert(session_b, SessionState::default());
    rendered_frame(&mut app, 80, 24);
    let viewport = app.hit_map.conversation.expect("viewport");
    drag_selection(&mut app, viewport, 8, 1).await;
    assert!(
        matches!(app.selection, Some(TextSelection::Conversation { .. })),
        "the drag selected conversation rows"
    );
    app.set_selected_session(session_b);
    assert!(
        app.selection.is_none(),
        "watching another session retires the stale conversation leg"
    );
    // ctrl+c after the switch has no selection: nothing is copied from
    // the newly watched session's transcript.
    app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL))
        .await;
    assert!(
        copied.lock().expect("capture").is_empty(),
        "no stale text copied after the switch"
    );
}

#[tokio::test]
async fn conversation_selection_retires_on_transcript_rebuild() {
    let (mut app, session, copied) = app_with_user_messages().await;
    rendered_frame(&mut app, 80, 24);
    let viewport = app.hit_map.conversation.expect("viewport");
    drag_selection(&mut app, viewport, 8, 1).await;
    assert!(
        matches!(app.selection, Some(TextSelection::Conversation { .. })),
        "the drag selected conversation rows"
    );
    // A revert marker rebuilds the visible transcript onto a new
    // branch; the pre-rebuild selection coordinates are meaningless.
    app.handle_delivery(live_event(runless_event(
        session,
        3,
        EventPayload::SessionReverted { through_seq: 1 },
    )))
    .await;
    assert!(
        app.selection.is_none(),
        "the rebuild retired the stale conversation leg"
    );
    app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL))
        .await;
    assert!(
        copied.lock().expect("capture").is_empty(),
        "no stale text copied after the rebuild"
    );
}

#[tokio::test]
async fn conversation_selection_retires_on_recovery_replay() {
    let (mut app, session, copied) = app_with_user_messages().await;
    rendered_frame(&mut app, 80, 24);
    let viewport = app.hit_map.conversation.expect("viewport");
    drag_selection(&mut app, viewport, 8, 1).await;
    assert!(
        matches!(app.selection, Some(TextSelection::Conversation { .. })),
        "the drag selected conversation rows"
    );
    // A recovery replay swaps in a whole new projection; the
    // selection's coordinates address the replaced one.
    app.handle_delivery(ClientDelivery::ReplayStart {
        session_id: session,
        generation: 0,
        final_seq: 2,
        rebuild: true,
    })
    .await;
    for seq in 1..=2 {
        app.handle_delivery(ClientDelivery::ReplayEvent {
            session_id: session,
            generation: 0,
            final_seq: 2,
            event: Box::new(user_input(session, seq, run_id(), "replayed")),
        })
        .await;
    }
    app.handle_delivery(ClientDelivery::ReplayEnd {
        session_id: session,
        generation: 0,
        final_seq: 2,
    })
    .await;
    assert!(
        app.selection.is_none(),
        "the recovery replay retired the stale conversation leg"
    );
    app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL))
        .await;
    assert!(
        copied.lock().expect("capture").is_empty(),
        "no stale text copied after the replay"
    );
}

#[tokio::test]
async fn wheel_during_overlay_transition_is_owned_by_state_not_stale_geometry() {
    let (mut app, session, _) = app_with_user_messages().await;
    rendered_frame(&mut app, 80, 12);
    let viewport = app.hit_map.conversation.expect("viewport");
    assert!(app.hit_map.approval.is_none());
    let (x, y) = (viewport.x + 1, viewport.y + 1);
    // Baseline: the conversation wheel-scrolls with no panel around.
    app.handle_wheel(x, y, false);
    let scrolled = app.conversation_scroll.offset;
    assert!(scrolled > 0, "content overflows the cramped viewport");
    app.conversation_scroll.offset = 0;
    // Arrival side: a panel that opened since the frame owns the wheel
    // even though its geometry does not exist yet.
    app.store
        .sessions
        .entry(session)
        .or_default()
        .approvals
        .push(approval(session));
    assert!(app.current_approval().is_some());
    app.handle_wheel(x, y, false);
    assert_eq!(
        app.conversation_scroll.offset, 0,
        "the state-open panel swallowed the wheel"
    );
    // Steady state: geometry rendered, the rect targets the panel
    // scroll.
    rendered_frame(&mut app, 80, 12);
    let approval_rect = app.hit_map.approval.expect("approval geometry");
    let (px, py) = (approval_rect.x + 1, approval_rect.y + 1);
    app.handle_wheel(px, py, false);
    assert_eq!(
        app.conversation_scroll.offset, 0,
        "the rendered panel still owns the wheel over its rect"
    );
    // Close side: the approval resolved but the hit map still
    // describes the gone panel — the wheel must reach the content.
    app.store
        .sessions
        .get_mut(&session)
        .expect("session")
        .approvals
        .clear();
    assert!(app.current_approval().is_none());
    app.handle_wheel(px, py, false);
    assert!(
        app.conversation_scroll.offset > 0,
        "stale panel geometry did not eat the wheel"
    );
}

#[tokio::test]
async fn wheel_follows_topmost_first_ownership_when_modal_and_approval_coexist() {
    let (mut app, session, _) = app_with_user_messages().await;
    app.store
        .sessions
        .entry(session)
        .or_default()
        .approvals
        .push(approval(session));
    app.modal = Modal::Sessions;
    // Both panels render, stacked modal-over-approval like every other
    // pointer path.
    rendered_frame(&mut app, 80, 24);
    let approval_rect = app.hit_map.approval.expect("approval geometry");
    let picker_rect = app.hit_map.picker.expect("picker geometry");
    let left = approval_rect.x.max(picker_rect.x);
    let top = approval_rect.y.max(picker_rect.y);
    let right = approval_rect.right().min(picker_rect.right());
    let bottom = approval_rect.bottom().min(picker_rect.bottom());
    assert!(
        left < right && top < bottom,
        "centered panels overlap: {approval_rect:?} {picker_rect:?}"
    );
    // A wheel over the overlap reaches the topmost modal — the
    // obscured approval panel never scrolls.
    app.handle_wheel(left, top, false);
    assert_eq!(
        app.approval_scroll, 0,
        "the modal owns the overlap; the approval beneath never scrolled"
    );
    assert_eq!(
        app.conversation_scroll.offset, 0,
        "nothing leaked through to the content"
    );
}

#[tokio::test]
async fn composer_selection_survives_a_watch_and_retires_on_mutation() {
    let mut app = test_app().await;
    app.selected = Some(SessionId::new_v7());
    app.input.set_buffer("hello world".to_owned());
    rendered_frame(&mut app, 80, 24);
    let text_rect = app.hit_map.input.expect("input").text_rect;
    composer_drag_selection(&mut app, text_rect, 1, 5).await;
    assert!(
        matches!(app.selection, Some(TextSelection::Composer { .. })),
        "the drag selected draft bytes"
    );
    // The draft buffer persists across a session watch, so the leg
    // stays valid.
    let session_b = SessionId::new_v7();
    app.store
        .sessions
        .insert(session_b, SessionState::default());
    app.set_selected_session(session_b);
    assert!(
        matches!(app.selection, Some(TextSelection::Composer { .. })),
        "a watch alone keeps the composer leg"
    );
    // Typing mutates the buffer: the stale byte range retires.
    app.handle_input_key(KeyEvent::new(KeyCode::Char('!'), KeyModifiers::NONE))
        .await;
    assert!(app.selection.is_none(), "typing retired the composer leg");
    // A paste retires it the same way.
    composer_drag_selection(&mut app, text_rect, 1, 5).await;
    assert!(matches!(
        app.selection,
        Some(TextSelection::Composer { .. })
    ));
    app.handle_paste("pasted");
    assert!(app.selection.is_none(), "pasting retired the composer leg");
}

#[tokio::test]
async fn composer_selection_is_replaced_by_multibyte_typing_then_cut_is_sane() {
    let mut app = test_app().await;
    let copied = Arc::new(Mutex::new(Vec::new()));
    app.clipboard_sink = ClipboardSink::Capture(copied.clone());
    app.selected = Some(SessionId::new_v7());
    app.input.set_buffer("hello world".to_owned());
    rendered_frame(&mut app, 80, 24);
    let text_rect = app.hit_map.input.expect("input").text_rect;
    composer_drag_selection(&mut app, text_rect, 6, 9).await;
    assert_eq!(
        app.selection,
        Some(TextSelection::Composer { anchor: 6, head: 9 }),
        "the drag selected \"wor\""
    );
    // Typing replaces the selection. The multibyte replacement shifts every
    // later byte offset, so the range must be gone afterwards, not
    // silently retargeted.
    app.handle_input_key(KeyEvent::new(KeyCode::Char('é'), KeyModifiers::NONE))
        .await;
    assert!(
        app.selection.is_none(),
        "the replaced byte range is retired"
    );
    assert_eq!(app.input.as_str(), "hello éld");
    // ctrl+x with no selection cuts and copies nothing.
    app.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL))
        .await;
    assert_eq!(app.input.as_str(), "hello éld", "nothing was cut");
    assert!(
        copied.lock().expect("capture").is_empty(),
        "nothing was copied"
    );
    // Cursor navigation retires a selection the same way.
    composer_drag_selection(&mut app, text_rect, 6, 9).await;
    assert!(matches!(
        app.selection,
        Some(TextSelection::Composer { .. })
    ));
    app.handle_input_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE))
        .await;
    assert!(
        app.selection.is_none(),
        "cursor navigation retired the composer leg"
    );
}

#[tokio::test]
async fn selection_highlight_paints_covered_cells_and_preserves_foregrounds() {
    let (mut app, _, _) = app_with_user_messages().await;
    rendered_frame(&mut app, 80, 24);
    let viewport = app.hit_map.conversation.expect("viewport");
    let body_row = viewport.y + 1;
    app.handle_mouse(mouse(
        MouseEventKind::Down(MouseButton::Left),
        viewport.x,
        body_row,
    ))
    .await;
    app.handle_mouse(mouse(
        MouseEventKind::Drag(MouseButton::Left),
        viewport.x + 10,
        body_row,
    ))
    .await;
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    terminal
        .draw(|frame| app.draw_for_test(frame))
        .expect("app render");
    let buffer = terminal.backend().buffer();
    let selection = app.theme.text_selection();
    let covered = buffer[(viewport.x + 4, body_row)].style();
    assert_eq!(covered.bg, selection.bg, "covered cells take the wash");
    assert_eq!(
        covered.fg,
        buffer[(viewport.x + 12, body_row)].style().fg,
        "foregrounds are preserved across the boundary"
    );
    let outside = buffer[(viewport.x + 12, body_row)].style();
    assert_ne!(outside.bg, selection.bg, "uncovered cells keep their fill");
}

#[tokio::test]
async fn user_message_menu_snapshot() {
    let (mut app, _, _) = app_with_user_messages().await;
    rendered_frame(&mut app, 80, 24);
    let hit = user_hit(&app, 2);
    app.handle_click(hit.rect.x + 2, hit.rect.y).await;
    insta::assert_snapshot!(rendered_frame(&mut app, 80, 24));
}

#[tokio::test]
async fn selection_overlay_snapshot() {
    let (mut app, _, _) = app_with_user_messages().await;
    rendered_frame(&mut app, 80, 24);
    let viewport = app.hit_map.conversation.expect("viewport");
    // Drag from mid-first-message down into the second: covered rows
    // highlight from the drag column to the row end on the first row,
    // and from the row start to the drag column on the last.
    app.handle_mouse(mouse(
        MouseEventKind::Down(MouseButton::Left),
        viewport.x + 6,
        viewport.y + 1,
    ))
    .await;
    app.handle_mouse(mouse(
        MouseEventKind::Drag(MouseButton::Left),
        viewport.x + 10,
        viewport.y + 4,
    ))
    .await;
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    terminal
        .draw(|frame| app.draw_for_test(frame))
        .expect("app render");
    let buffer = terminal.backend().buffer();
    let selected_bg = app.theme.text_selection().bg;
    // Text with selected cells marked '#': the overlay shows the exact
    // covered region, gutters included, with no layout disturbance.
    let overlay = (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| {
                    if selected_bg.is_some() && buffer[(x, y)].style().bg == selected_bg {
                        '#'
                    } else {
                        buffer[(x, y)].symbol().chars().next().unwrap_or(' ')
                    }
                })
                .collect::<String>()
                .trim_end()
                .to_owned()
        })
        .collect::<Vec<_>>()
        .join("\n");
    insta::assert_snapshot!(overlay);
}

#[tokio::test]
async fn ctrl_a_selects_the_whole_draft_for_copy_typing_and_paste() {
    let ctrl = |character| KeyEvent::new(KeyCode::Char(character), KeyModifiers::CONTROL);
    let mut app = test_app().await;
    let copied = Arc::new(Mutex::new(Vec::new()));
    app.clipboard_sink = ClipboardSink::Capture(copied.clone());
    app.selected = Some(SessionId::new_v7());

    // An empty draft has nothing to select.
    app.handle_key(ctrl('a')).await;
    assert!(app.selection.is_none());

    type_input(&mut app, "héllo wörld").await;
    app.handle_key(ctrl('a')).await;
    let len = app.input.as_str().len();
    assert_eq!(
        app.selection,
        Some(TextSelection::Composer {
            anchor: 0,
            head: len
        })
    );
    app.handle_key(ctrl('c')).await;
    assert_eq!(
        copied.lock().expect("capture").as_slice(),
        ["héllo wörld".to_owned()]
    );
    assert_eq!(app.input.as_str(), "héllo wörld", "copying keeps the draft");

    // Typing over the selection replaces the whole draft.
    app.handle_key(ctrl('a')).await;
    app.handle_key(KeyEvent::new(KeyCode::Char('X'), KeyModifiers::NONE))
        .await;
    assert_eq!(app.input.as_str(), "X");
    assert!(app.selection.is_none());

    // So does a paste.
    app.handle_key(ctrl('a')).await;
    app.handle_paste("pasted");
    assert_eq!(app.input.as_str(), "pasted");
}

#[test]
fn inline_code_caps_never_reach_copied_text() {
    let theme = Theme::default();
    let lines = crate::markdown::render_markdown_width(
        &MarkdownDocument::new("run `cargo test` or `a``b` now".into()),
        &theme,
        &PlainHighlighter,
        58,
    )
    .into_iter()
    .flat_map(|line| assistant_body_line(line, 60, &theme))
    .collect::<Vec<_>>();
    let rendered = lines.iter().map(ToString::to_string).collect::<Vec<_>>();
    assert!(rendered[0].contains("▐cargo test▌"), "{rendered:?}");
    assert_eq!(
        extract_selection(&lines, (0, 0), (0, u16::MAX), &theme),
        // `a``b` is one code span whose text keeps its inner backticks.
        "run cargo test or a``b now"
    );
}
