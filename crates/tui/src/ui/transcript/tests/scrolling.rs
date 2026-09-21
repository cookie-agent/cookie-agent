use crate::ui::transcript::*;

use cookie_agent_protocol::{AttemptId, EventSubscriptionMessage, SessionId};

use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

use crate::client::ClientDelivery;

use crate::markdown::MarkdownDocument;

use crate::state::{AssistantChild, SessionState};

use crate::ui::app::*;

use super::support::*;

#[test]
fn scrollbar_geometry_maps_track_ends_to_the_exact_offset_range() {
    let track = Rect::new(10, 2, 1, 10);
    let geometry = ScrollbarGeometry::resolve(track, 100).expect("geometry");
    assert_eq!(geometry.max_offset, 90);
    assert_eq!(geometry.thumb_top(0), 0);
    assert_eq!(
        geometry.thumb_top(90) + geometry.thumb_size(),
        usize::from(track.height)
    );
    assert_eq!(geometry.offset_for_track_row(track.y), 0);
    assert_eq!(
        geometry.clamp_offset(geometry.offset_for_track_row(track.y + track.height - 1)),
        90
    );
}

#[test]
fn thumb_height_is_constant_across_top_middle_and_bottom() {
    let track = Rect::new(0, 0, 1, 12);
    let geometry = ScrollbarGeometry::resolve(track, 120).expect("geometry");
    let size = geometry.thumb_size();
    for offset in [0, 27, 54, 108] {
        assert_eq!(geometry.thumb_size(), size);
        let _ = geometry.with_thumb(offset);
    }
}

#[test]
fn thumb_drag_round_trips_offsets_at_constant_height() {
    let track = Rect::new(0, 0, 1, 12);
    let geometry = ScrollbarGeometry::resolve(track, 120).expect("geometry");
    let tolerance = (geometry.max_offset / usize::from(track.height)) + 2;
    for offset in [0, 13, 54, 108] {
        let top = geometry.thumb_top(offset);
        let round_trip = geometry
            .clamp_offset(geometry.offset_for_thumb_anchor(u16::try_from(top).expect("row"), 0));
        assert!(
            (round_trip as i64 - offset as i64).unsigned_abs() as usize <= tolerance,
            "offset {offset} round-tripped to {round_trip}"
        );
    }
}

#[test]
fn conversation_scroll_reengages_following_at_the_exact_bottom() {
    let mut scroll = ConversationScroll::default();
    scroll.up(5);
    scroll.clamp(200, 10);
    assert!(!scroll.following);
    scroll.scroll_to(ConversationScroll::max_offset(200, 10));
    assert!(scroll.following);
    scroll.clamp(220, 10);
    assert_eq!(scroll.offset, 210, "follow output arriving before redraw");
    scroll.up(3);
    scroll.down(3);
    assert!(scroll.following);
    scroll.clamp(240, 10);
    assert_eq!(scroll.offset, 230);
}

#[tokio::test]
async fn expansion_at_bottom_preserves_header_and_preceding_screen_rows() {
    for kind in [
        "thinking",
        "tool",
        "plugin",
        "compaction",
        "producer",
        "system",
    ] {
        for width in [100, 28] {
            let (mut app, _, block) = expansion_scroll_app(kind).await;
            let before = conversation_rows(&mut app, width, 24);
            let hit = *app
                .hit_map
                .blocks
                .iter()
                .find(|hit| hit.id == block)
                .unwrap();
            let offset = app.conversation_scroll.offset;
            let header_row = usize::from(hit.rect.y - app.hit_map.conversation.unwrap().y);
            assert!(app.conversation_scroll.following, "{kind} at {width}");
            app.handle_click(hit.rect.x, hit.rect.y).await;
            let after = conversation_rows(&mut app, width, 24);
            let expanded = app
                .hit_map
                .blocks
                .iter()
                .find(|hit| hit.id == block)
                .unwrap();
            assert_eq!(expanded.rect.y, hit.rect.y, "{kind} at {width}");
            assert_eq!(
                &after[..header_row],
                &before[..header_row],
                "{kind} at {width}"
            );
            assert_eq!(app.conversation_scroll.offset, offset, "{kind} at {width}");
            assert!(!app.conversation_scroll.following, "{kind} at {width}");
            app.toggle_block(block);
            conversation_rows(&mut app, width, 24);
            assert_eq!(
                app.conversation_scroll.offset, offset,
                "collapse {kind} at {width}"
            );
            assert!(app.conversation_scroll.following);
        }
    }
}

#[tokio::test]
async fn expansion_above_viewport_keeps_following_content_anchored() {
    let (mut app, session, block) = expansion_scroll_app("thinking").await;
    let state = app.store.sessions.get_mut(&session).unwrap();
    let TranscriptItem::Assistant { children, .. } = state.transcript.last_mut().unwrap() else {
        unreachable!()
    };
    children.push(AssistantChild::Text {
        id: 3,
        version: 0,
        markdown: MarkdownDocument::new("after thinking\n\n".repeat(60)),
    });
    for width in [100, 28] {
        app.conversation_scroll.bottom();
        let before = conversation_rows(&mut app, width, 24);
        let offset = app.conversation_scroll.offset;
        for _ in 0..3 {
            app.toggle_block(block);
            let expanded = conversation_rows(&mut app, width, 24);
            assert_eq!(expanded, before);
            assert!(app.conversation_scroll.offset > offset);
            app.toggle_block(block);
            assert_eq!(conversation_rows(&mut app, width, 24), before);
            assert_eq!(app.conversation_scroll.offset, offset);
        }
    }
}

#[tokio::test]
async fn collapse_near_bottom_clamps_to_the_last_valid_offset() {
    let (mut app, _, block) = expansion_scroll_app("tool").await;
    conversation_rows(&mut app, 80, 24);
    let collapsed_bottom = app.conversation_scroll.offset;
    app.toggle_block(block);
    conversation_rows(&mut app, 80, 24);
    app.conversation_scroll.bottom();
    conversation_rows(&mut app, 80, 24);
    assert!(app.conversation_scroll.offset > collapsed_bottom);
    app.toggle_block(block);
    conversation_rows(&mut app, 80, 24);
    assert_eq!(app.conversation_scroll.offset, collapsed_bottom);
    assert!(app.conversation_scroll.following);
}

#[tokio::test]
async fn nested_output_expansion_keeps_the_visible_prefix_in_place() {
    let (mut app, _, block) = expansion_scroll_app("tool").await;
    app.toggle_block(block);
    app.conversation_scroll.bottom();
    let before = conversation_rows(&mut app, 80, 24);
    let notice = *app
        .hit_map
        .blocks
        .iter()
        .find(|hit| matches!(hit.id, BlockId::ToolOutput { .. }))
        .unwrap();
    let row = usize::from(notice.rect.y - app.hit_map.conversation.unwrap().y);
    let offset = app.conversation_scroll.offset;
    app.handle_click(notice.rect.x, notice.rect.y).await;
    let after = conversation_rows(&mut app, 80, 24);
    assert_eq!(&after[..row], &before[..row]);
    assert_eq!(app.conversation_scroll.offset, offset);
    assert!(!app.conversation_scroll.following);
}

#[tokio::test]
async fn streaming_follows_only_when_already_at_bottom_across_expansion_and_scroll() {
    let mut app = test_app().await;
    let session = SessionId::new_v7();
    let run = run_id();
    let attempt = AttemptId::new_v7();
    app.selected = Some(session);
    app.tree_root = Some(session);
    for event in [
        session_created(session, 1),
        attempt_started(session, 2, run, attempt, None),
        reasoning_delta(session, 3, run, attempt, &"thought\n".repeat(70)),
    ] {
        app.store.apply_event(event);
    }
    conversation_rows(&mut app, 60, 24);
    let block = app
        .hit_map
        .blocks
        .iter()
        .find(|hit| matches!(hit.id, BlockId::Thinking(_)))
        .unwrap()
        .id;
    let mut seq = 4;
    for _ in 0..3 {
        app.conversation_scroll.top();
        conversation_rows(&mut app, 60, 24);
        app.toggle_block(block);
        let before = conversation_rows(&mut app, 60, 24);
        let offset = app.conversation_scroll.offset;
        app.handle_delivery(ClientDelivery::Live {
            message: Box::new(EventSubscriptionMessage::Event {
                event: Box::new(text_delta(
                    session,
                    seq,
                    run,
                    attempt,
                    &"agent progress\n\n".repeat(40),
                )),
            }),
            generation: 0,
        })
        .await;
        seq += 1;
        let after = conversation_rows(&mut app, 60, 24);
        assert_eq!(app.conversation_scroll.offset, offset);
        // Thinking's streaming label may settle when text begins.
        assert_eq!(&after[3..], &before[3..]);
        assert!(!app.conversation_scroll.following);

        // Return to the old bottom, then receive output before any redraw.
        app.conversation_scroll.scroll_to(usize::MAX);
        assert!(app.conversation_scroll.following);
        app.store
            .apply_event(text_delta(session, seq, run, attempt, "\n\nnew tail\n\n"));
        seq += 1;
        let rows = conversation_rows(&mut app, 60, 24);
        assert!(rows.iter().any(|row| row.contains("new tail")));
        assert_eq!(
            app.conversation_scroll.offset,
            app.scrollbar_geometry.unwrap().max_offset
        );
        app.toggle_block(block);
        conversation_rows(&mut app, 60, 24);
    }
}

#[tokio::test]
async fn scrollbar_is_reserved_drawn_and_pages_on_track_press() {
    let mut app = test_app().await;
    let session = SessionId::new_v7();
    app.selected = Some(session);
    app.tree_root = Some(session);
    app.store
        .sessions
        .insert(session, tall_transcript_state(300));
    rendered_frame(&mut app, 80, 50);
    let track = app.hit_map.scrollbar.expect("scrollbar reserved");
    assert_eq!(track.width, 1);
    let geometry = app.scrollbar_geometry.expect("geometry");
    assert!((1..=track.height).contains(&geometry.thumb.height));
    app.handle_click(track.x, track.y + track.height - 1).await;
    assert!(app.conversation_scroll.offset > 0);
}

#[tokio::test]
async fn an_empty_conversation_reserves_the_same_columns_as_an_overflowing_one() {
    let mut app = test_app().await;
    let session = SessionId::new_v7();
    app.selected = Some(session);
    app.tree_root = Some(session);
    app.store.sessions.insert(session, SessionState::default());
    rendered_frame(&mut app, 80, 24);
    let empty = app.hit_map.conversation.expect("conversation viewport");
    let track = app
        .hit_map
        .scrollbar
        .expect("the track column is reserved even with no thumb");
    assert_eq!(empty.width, pane_text_width(80));
    assert_eq!(track.width, SCROLLBAR_TRACK_COLUMNS);
    assert_eq!(track.x, empty.right() + SCROLLBAR_MARGIN);
    assert_eq!(app.scrollbar_geometry, None, "nothing to scroll yet");
    let cells = frame_cells(&mut app, 80, 24);
    let border = usize::from(empty.x + empty.width + SCROLLBAR_RESERVE);
    assert_eq!(cells[usize::from(empty.y)][border], "│");
    for row in empty.y..empty.bottom() {
        for column in empty.right()..track.right() {
            assert_eq!(
                cells[usize::from(row)][usize::from(column)],
                " ",
                "reserved columns stay blank ({row},{column})"
            );
        }
    }

    app.store
        .sessions
        .get_mut(&session)
        .expect("session")
        .transcript = tall_transcript_state(300).transcript;
    rendered_frame(&mut app, 80, 24);
    assert_eq!(
        app.hit_map.conversation.expect("conversation viewport"),
        empty
    );
    assert_eq!(app.hit_map.scrollbar, Some(track));
    assert_eq!(
        app.layout_cache.key.expect("layout key").width,
        pane_text_width(80)
    );
    assert!(
        app.scrollbar_geometry.is_some(),
        "the same pane now overflows"
    );
    let cells = frame_cells(&mut app, 80, 24);
    assert!(
        (empty.y..empty.bottom()).any(|row| cells[usize::from(row)][usize::from(track.x)] == "█"),
        "the thumb draws in the column it was always reserved"
    );
    for row in empty.y..empty.bottom() {
        assert_eq!(cells[usize::from(row)][usize::from(empty.right())], " ");
    }
}

#[tokio::test]
async fn composer_scrollbar_drag_scrolls_without_moving_the_text_cursor() {
    let (mut app, geometry) = app_with_overflowing_composer().await;
    let cursor_before = app.input.cursor_byte();
    // The cursor anchors the bottom of the draft, so the thumb rests at
    // the bottom of the track.
    assert!(app.input.viewport_row() > 0);
    let press = |kind, column, row| MouseEvent {
        kind,
        column,
        row,
        modifiers: KeyModifiers::NONE,
    };
    app.handle_mouse(press(
        MouseEventKind::Down(MouseButton::Left),
        geometry.thumb.x,
        geometry.thumb.y,
    ))
    .await;
    assert!(
        matches!(app.scrollbar_drag, Some(drag) if drag.target == ScrollbarTarget::Input),
        "thumb press captures an input drag: {:?}",
        app.scrollbar_drag
    );
    // Dragging the thumb to the top of the track scrolls the composer…
    app.handle_mouse(press(
        MouseEventKind::Drag(MouseButton::Left),
        geometry.track.x,
        geometry.track.y,
    ))
    .await;
    rendered_frame(&mut app, 80, 50);
    assert_eq!(app.input.viewport_row(), 0);
    // …while the text cursor never moves.
    assert_eq!(app.input.cursor_byte(), cursor_before);
    // Releasing the press ends the capture.
    app.handle_mouse(press(
        MouseEventKind::Up(MouseButton::Left),
        geometry.track.x,
        geometry.track.y,
    ))
    .await;
    assert!(app.scrollbar_drag.is_none());
}

#[tokio::test]
async fn composer_scrollbar_drag_stays_captured_outside_the_track() {
    let (mut app, geometry) = app_with_overflowing_composer().await;
    let event = |kind, column, row| MouseEvent {
        kind,
        column,
        row,
        modifiers: KeyModifiers::NONE,
    };
    app.handle_mouse(event(
        MouseEventKind::Down(MouseButton::Left),
        geometry.thumb.x,
        geometry.thumb.y,
    ))
    .await;
    // The pointer wanders far into the conversation pane; the captured
    // drag keeps its anchor and clamps against the original geometry.
    app.handle_mouse(event(MouseEventKind::Drag(MouseButton::Left), 0, 0))
        .await;
    rendered_frame(&mut app, 80, 50);
    assert_eq!(app.input.viewport_row(), 0);
    assert!(
        matches!(app.scrollbar_drag, Some(drag) if drag.target == ScrollbarTarget::Input),
        "capture survives leaving the track"
    );
    app.handle_mouse(event(MouseEventKind::Up(MouseButton::Left), 0, 0))
        .await;
    assert!(app.scrollbar_drag.is_none());
}

#[tokio::test]
async fn composer_scrollbar_track_press_pages_the_viewport() {
    let (mut app, geometry) = app_with_overflowing_composer().await;
    let cursor_before = app.input.cursor_byte();
    assert!(app.input.viewport_row() > 0);
    // Bare track above the thumb pages the viewport toward that offset
    // without capturing a drag or moving the text cursor.
    app.handle_click(geometry.track.x, geometry.track.y).await;
    assert_eq!(app.input.viewport_row(), 0);
    assert!(app.scrollbar_drag.is_none());
    assert_eq!(app.input.cursor_byte(), cursor_before);
}

#[tokio::test]
async fn composer_scrollbar_hold_reanchors_on_the_next_edit() {
    let (mut app, geometry) = app_with_overflowing_composer().await;
    app.handle_click(geometry.track.x, geometry.track.y).await;
    rendered_frame(&mut app, 80, 50);
    assert_eq!(app.input.viewport_row(), 0);
    // The next edit ends the hold: the viewport follows the cursor back
    // to the bottom of the draft.
    app.handle_paste("x");
    rendered_frame(&mut app, 80, 50);
    assert_eq!(app.input.viewport_row(), 3);
}

#[tokio::test]
async fn composer_wheel_still_scrolls_the_overflowing_viewport() {
    let (mut app, geometry) = app_with_overflowing_composer().await;
    let wheel = |kind| MouseEvent {
        kind,
        column: geometry.track.x - 1,
        row: geometry.track.y,
        modifiers: KeyModifiers::NONE,
    };
    // The wheel keeps its existing composer semantics: it walks the text
    // cursor three visual rows per tick, and the viewport follows.
    assert_eq!(app.input.viewport_row(), 3);
    for _ in 0..3 {
        app.handle_mouse(wheel(MouseEventKind::ScrollUp)).await;
    }
    rendered_frame(&mut app, 80, 50);
    assert_eq!(app.input.viewport_row(), 0);
    for _ in 0..3 {
        app.handle_mouse(wheel(MouseEventKind::ScrollDown)).await;
    }
    rendered_frame(&mut app, 80, 50);
    assert_eq!(app.input.viewport_row(), 3);
}

#[tokio::test]
async fn the_composer_reserves_columns_only_for_an_overflowing_ceiling() {
    let mut app = test_app().await;
    app.handle_paste("a\nb\nc\nd\ne");
    rendered_frame(&mut app, 80, 40);
    let fitted = app.hit_map.input.expect("composer hit");
    assert_eq!(fitted.text_rect.height, crate::ui::input::MAX_TEXT_ROWS);
    assert_eq!(fitted.text_rect.width, fitted.rect.width - 2);
    assert_eq!(fitted.scrollbar, None);
    assert_eq!(app.scrollbar_geometry, None);
    assert!(!app.input.has_overflow());

    app.handle_paste("\nf");
    rendered_frame(&mut app, 80, 40);
    let clipped = app.hit_map.input.expect("composer hit");
    assert_eq!(clipped.rect, fitted.rect, "the box does not move");
    assert_eq!(
        clipped.text_rect.width,
        fitted.text_rect.width - SCROLLBAR_RESERVE
    );
    let track = clipped.scrollbar.expect("composer track").track;
    assert_eq!(track.width, SCROLLBAR_TRACK_COLUMNS);
    assert_eq!(track.x, clipped.text_rect.right() + SCROLLBAR_MARGIN);
    assert_eq!(
        (track.y, track.height),
        (clipped.text_rect.y, clipped.text_rect.height)
    );
    // The renderer, the wheel gate and the pane height all measure the same
    // draft at the same width: overflow is real, not a reservation artefact.
    assert!(app.input.has_overflow());
    let geometry = clipped.scrollbar.expect("composer thumb");
    assert!(geometry.content_height > geometry.viewport_height);
    assert_eq!(geometry.thumb.width, geometry.track.width);
    assert_eq!(
        app.input.composer_rows(fitted.rect.width - 2),
        usize::from(crate::ui::input::MAX_TEXT_ROWS) + 1
    );
}
