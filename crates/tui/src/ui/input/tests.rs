use ratatui::{
    Terminal,
    backend::TestBackend,
    layout::Rect,
    style::{Modifier, Style},
};

use super::{
    CredentialInput, InputState, MAX_TEXT_ROWS, SCROLLBAR_RESERVE, ScrollbarGeometry, render,
    visual_rows,
};
use crate::theme::Theme;
use crate::ui::transcript::SCROLLBAR_MARGIN;

#[test]
fn focused_and_unfocused_message_borders_differ_by_weight_and_fill() {
    fn rendered_styles(focused: bool) -> (Style, Style, Style) {
        let mut input = InputState::default();
        let mut terminal = Terminal::new(TestBackend::new(12, 3)).expect("terminal");
        terminal
            .draw(|frame| {
                render(
                    frame,
                    frame.area(),
                    &mut input,
                    focused,
                    "Message",
                    None,
                    &Theme::default(),
                );
            })
            .expect("render Message box");
        let buffer = terminal.backend().buffer();
        (
            buffer[(0, 0)].style(),
            buffer[(1, 0)].style(),
            buffer[(1, 1)].style(),
        )
    }

    let theme = Theme::default();
    let (focused_border, focused_title, focused_text) = rendered_styles(true);
    let (unfocused_border, unfocused_title, unfocused_text) = rendered_styles(false);

    // Focus carries the honey highlight in bold; the resting box uses
    // the plain walnut border. Weight still distinguishes the states
    // when color is unavailable.
    assert_eq!(focused_border.fg, theme.input_border(true).fg);
    assert_eq!(unfocused_border.fg, theme.panel_border().fg);
    assert!(focused_border.add_modifier.contains(Modifier::BOLD));
    assert!(!focused_border.add_modifier.contains(Modifier::DIM));
    assert!(!unfocused_border.add_modifier.contains(Modifier::DIM));
    assert!(!unfocused_border.add_modifier.contains(Modifier::BOLD));
    // The focused composer interior is filled with the panel surface.
    assert_eq!(focused_text.bg, theme.panel().bg);
    assert!(matches!(
        unfocused_text.bg,
        None | Some(ratatui::style::Color::Reset)
    ));
    // Titles pick up the border accent of their state.
    assert_eq!(focused_title.fg, theme.input_border(true).fg);
    assert_eq!(unfocused_title.fg, theme.panel_border().fg);
}

#[test]
fn empty_input_shows_the_placeholder_until_text_arrives() {
    fn buffer_text(input: &mut InputState, placeholder: Option<&str>) -> String {
        let mut terminal = Terminal::new(TestBackend::new(30, 3)).expect("terminal");
        terminal
            .draw(|frame| {
                render(
                    frame,
                    frame.area(),
                    input,
                    true,
                    "Message",
                    placeholder,
                    &Theme::default(),
                );
            })
            .expect("render");
        let buffer = terminal.backend().buffer();
        (0..3)
            .flat_map(|y| (0..30).map(move |x| buffer[(x, y)].symbol().to_owned()))
            .collect()
    }

    let mut input = InputState::default();
    let rendered = buffer_text(&mut input, Some("Type a message · / for commands"));
    assert!(rendered.contains("Type a message"), "{rendered}");
    // Too narrow for the full hint: it ellipsizes instead of clipping
    // mid-word at the border.
    assert!(rendered.contains('…'), "{rendered}");

    input.set_buffer("h".into());
    let rendered = buffer_text(&mut input, Some("Type a message · / for commands"));
    assert!(!rendered.contains("Type a message"), "{rendered}");

    // No placeholder configured: the row stays blank.
    let mut blank = InputState::default();
    let rendered = buffer_text(&mut blank, None);
    assert!(!rendered.contains("Type a message"), "{rendered}");
}

#[test]
fn credential_owned_insert_reuses_the_sanitized_allocation() {
    let mut sanitized = "sentinel-secret".to_owned();
    let allocation = sanitized.as_ptr();
    let mut input = CredentialInput::default();
    input.insert_owned(std::mem::take(&mut sanitized));
    assert!(sanitized.is_empty());
    assert_eq!(input.as_str(), "sentinel-secret");
    assert_eq!(input.as_str().as_ptr(), allocation);
}

#[test]
fn multiline_editing_wraps_at_the_exact_inner_width() {
    let mut input = InputState::default();
    input.set_buffer("abcdef\ngh".into());
    assert_eq!(input.visual_row_count(3), 3);
    assert_eq!(input.cursor_visual_position(3), (2, 2));

    input.move_home();
    input.backspace();
    assert_eq!(input.as_str(), "abcdefg h".replace(' ', ""));
    input.insert_newline();
    assert_eq!(input.as_str(), "abcdef\ngh");
}

#[test]
fn cursor_and_editing_treat_emoji_zwj_and_combining_sequences_as_graphemes() {
    let family = "👨‍👩‍👧‍👦";
    let combining = "e\u{301}";
    let mut input = InputState::default();
    input.set_buffer(format!("a{family}\n{combining}b"));

    input.move_left();
    input.backspace();
    assert_eq!(input.as_str(), format!("a{family}\nb"));
    input.move_home();
    input.move_left();
    input.backspace();
    assert_eq!(input.as_str(), "a\nb");
    input.delete();
    assert_eq!(input.as_str(), "ab");

    input.set_buffer("ab".into());
    input.move_left();
    input.insert_text("\u{301}");
    assert_eq!(input.cursor_byte(), "a\u{301}".len());
    input.backspace();
    assert_eq!(input.as_str(), "b");
}

#[test]
fn up_down_home_end_follow_logical_and_wrapped_rows() {
    let mut input = InputState::default();
    input.set_buffer("abcd\nef".into());
    input.layout_width = 3;
    input.move_home();
    assert_eq!(input.cursor_byte(), 5);
    input.move_up();
    assert_eq!(input.cursor_byte(), 3);
    input.move_up();
    assert_eq!(input.cursor_byte(), 0);
    input.move_down();
    assert_eq!(input.cursor_byte(), 3);
    input.move_end();
    assert_eq!(input.cursor_byte(), 4);
    input.move_buffer_end();
    assert_eq!(input.cursor_byte(), input.as_str().len());
}

#[test]
fn exact_width_content_places_the_insertion_cursor_on_a_trailing_empty_row() {
    let mut input = InputState::default();
    input.set_buffer("abcd".into());
    assert_eq!(input.visual_row_count(4), 2);
    assert_eq!(input.cursor_visual_position(4), (1, 0));

    let mut terminal = Terminal::new(TestBackend::new(6, 5)).expect("terminal");
    terminal
        .draw(|frame| {
            render(
                frame,
                frame.area(),
                &mut input,
                true,
                "Message",
                None,
                &Theme::default(),
            );
        })
        .expect("render exact-width input");
    assert_eq!(
        terminal.get_cursor_position().expect("cursor"),
        (1, 2).into()
    );
}

#[test]
fn three_row_viewport_scrolls_to_keep_cursor_visible() {
    let mut input = InputState::default();
    input.set_buffer("one\ntwo\nthree\nfour".into());
    let mut terminal = Terminal::new(TestBackend::new(12, 5)).expect("terminal");
    terminal
        .draw(|frame| {
            render(
                frame,
                Rect::new(0, 0, 12, 5),
                &mut input,
                true,
                "Message",
                None,
                &Theme::default(),
            );
        })
        .expect("render");
    assert_eq!(input.viewport_row(), 1);
    let position = terminal.get_cursor_position().expect("cursor position");
    assert_eq!(position.y, 3);

    input.move_buffer_home();
    terminal
        .draw(|frame| {
            render(
                frame,
                Rect::new(0, 0, 12, 5),
                &mut input,
                true,
                "Message",
                None,
                &Theme::default(),
            );
        })
        .expect("render");
    assert_eq!(input.viewport_row(), 0);
    assert_eq!(terminal.get_cursor_position().expect("cursor").y, 1);
}

#[test]
fn incremental_newline_and_wrap_only_typing_reanchors_the_three_row_viewport() {
    let mut newline = InputState::default();
    let mut terminal = Terminal::new(TestBackend::new(8, 5)).expect("terminal");
    terminal
        .draw(|frame| {
            render(
                frame,
                frame.area(),
                &mut newline,
                true,
                "Message",
                None,
                &Theme::default(),
            );
        })
        .expect("establish layout");
    for character in "a\nb\nc".chars() {
        newline.insert(character);
    }
    assert_eq!(newline.viewport_row(), 0);
    newline.insert_newline();
    assert_eq!(newline.cursor_visual_position(6), (3, 0));
    assert_eq!(newline.viewport_row(), 1);

    let mut wrapped = InputState::default();
    let mut terminal = Terminal::new(TestBackend::new(5, 5)).expect("terminal");
    terminal
        .draw(|frame| {
            render(
                frame,
                frame.area(),
                &mut wrapped,
                true,
                "Message",
                None,
                &Theme::default(),
            );
        })
        .expect("establish narrow layout");
    for character in "abcdefghi".chars() {
        wrapped.insert(character);
    }
    assert_eq!(wrapped.cursor_visual_position(3), (3, 0));
    assert_eq!(wrapped.viewport_row(), 1);
}

#[test]
fn edits_navigation_set_buffer_click_and_take_keep_viewport_consistent() {
    let mut input = InputState::default();
    let mut terminal = Terminal::new(TestBackend::new(8, 5)).expect("terminal");
    terminal
        .draw(|frame| {
            render(
                frame,
                frame.area(),
                &mut input,
                true,
                "Message",
                None,
                &Theme::default(),
            );
        })
        .expect("establish layout");

    input.set_buffer("zero\none\ntwo\nthree\nfour".into());
    assert_eq!(input.viewport_row(), 2);
    input.move_up();
    input.move_up();
    input.move_up();
    assert_eq!(input.viewport_row(), 1);
    input.set_cursor_from_display_position(0, 0);
    assert_eq!(input.cursor_visual_position(6).0, 1);
    input.backspace();
    assert_eq!(input.cursor_visual_position(6).0, 0);
    assert_eq!(input.viewport_row(), 0);

    input.move_buffer_end();
    assert_eq!(input.viewport_row(), 2);
    input.move_home();
    assert_eq!(input.cursor_visual_position(6).0, 4);
    input.move_end();
    assert_eq!(input.cursor_visual_position(6).0, 4);
    input.move_buffer_home();
    assert_eq!(input.viewport_row(), 0);
    input.move_buffer_end();
    assert_eq!(input.viewport_row(), 2);

    let retained = input.viewport_row();
    terminal
        .draw(|frame| {
            render(
                frame,
                frame.area(),
                &mut input,
                false,
                "Message",
                None,
                &Theme::default(),
            );
        })
        .expect("unfocused render");
    assert_eq!(input.viewport_row(), retained);
    assert!(!input.take().is_empty());
    assert_eq!(input.viewport_row(), 0);
}

#[test]
fn overflow_title_reports_input_rows_above_and_below_without_using_text_rows() {
    let mut input = InputState::default();
    input.set_buffer("zero\none\ntwo\nthree\nfour".into());
    let mut terminal = Terminal::new(TestBackend::new(40, 5)).expect("terminal");
    terminal
        .draw(|frame| {
            render(
                frame,
                frame.area(),
                &mut input,
                true,
                "Message",
                None,
                &Theme::from_environment("dark", true, "xterm", "truecolor"),
            );
        })
        .expect("render overflow title");
    let top = (0..40)
        .map(|x| terminal.backend().buffer()[(x, 0)].symbol())
        .collect::<String>();
    assert!(top.contains("Input ↑2 ↓0 · Message"));
    assert_eq!(terminal.backend().buffer()[(1, 1)].symbol(), "t");

    input.move_up();
    input.move_up();
    input.move_up();
    terminal
        .draw(|frame| {
            render(
                frame,
                frame.area(),
                &mut input,
                false,
                "Message",
                None,
                &Theme::from_environment("dark", true, "xterm", "truecolor"),
            );
        })
        .expect("render split overflow title");
    let top = (0..40)
        .map(|x| terminal.backend().buffer()[(x, 0)].symbol())
        .collect::<String>();
    assert!(top.contains("Input ↑1 ↓1 · Message"));

    let mut tiny = Terminal::new(TestBackend::new(8, 5)).expect("tiny terminal");
    tiny.draw(|frame| {
        render(
            frame,
            frame.area(),
            &mut input,
            false,
            "Message",
            None,
            &Theme::from_environment("dark", true, "xterm", "truecolor"),
        );
    })
    .expect("tiny overflow title");
    let top = (0..8)
        .map(|x| tiny.backend().buffer()[(x, 0)].symbol())
        .collect::<String>();
    assert!(top.contains("↑1↓1"));
}

#[test]
fn resize_reflows_without_overflow_and_keeps_cursor_visible() {
    let mut input = InputState::default();
    input.set_buffer("界👩‍💻abcdef".into());
    let mut terminal = Terminal::new(TestBackend::new(14, 5)).expect("terminal");
    terminal
        .draw(|frame| {
            render(
                frame,
                frame.area(),
                &mut input,
                true,
                "Message",
                None,
                &Theme::default(),
            );
        })
        .expect("wide render");
    terminal.backend_mut().resize(7, 5);
    terminal.autoresize().expect("resize");
    terminal
        .draw(|frame| {
            render(
                frame,
                frame.area(),
                &mut input,
                true,
                "Message",
                None,
                &Theme::default(),
            );
        })
        .expect("narrow render");
    assert_eq!(input.visual_row_count(5), 3);
    assert_eq!(input.cursor_visual_position(5), (2, 0));
    let cursor = terminal.backend().cursor_position();
    assert!(cursor.x < 7 && cursor.y < 5);
}

#[test]
fn resize_reflow_and_zero_inner_dimensions_preserve_safe_cursor_state() {
    let mut input = InputState::default();
    input.set_buffer("界界界\nabc\ndef".into());
    let mut terminal = Terminal::new(TestBackend::new(10, 5)).expect("terminal");
    terminal
        .draw(|frame| {
            render(
                frame,
                frame.area(),
                &mut input,
                true,
                "Message",
                None,
                &Theme::default(),
            );
        })
        .expect("wide render");
    let wide_viewport = input.viewport_row();

    terminal.backend_mut().resize(6, 2);
    terminal.autoresize().expect("resize to zero inner area");
    terminal
        .draw(|frame| {
            render(
                frame,
                frame.area(),
                &mut input,
                true,
                "Message",
                None,
                &Theme::default(),
            );
        })
        .expect("zero-inner render");
    assert_eq!(input.viewport_row(), wide_viewport);
    assert!(!terminal.backend().cursor_visible());

    terminal.backend_mut().resize(6, 4);
    terminal.autoresize().expect("resize narrow");
    terminal
        .draw(|frame| {
            render(
                frame,
                frame.area(),
                &mut input,
                true,
                "Message",
                None,
                &Theme::default(),
            );
        })
        .expect("narrow render");
    let (cursor_row, _) = input.cursor_visual_position(4);
    assert!(cursor_row >= input.viewport_row());
    assert!(cursor_row < input.viewport_row() + 2);
    let cursor = terminal.backend().cursor_position();
    assert!(cursor.x >= 1 && cursor.x < 5 && cursor.y >= 1 && cursor.y < 3);
}

#[test]
fn tiny_areas_render_safely() {
    let mut input = InputState::default();
    input.set_buffer("👩‍💻\ntext".into());
    for (width, height) in [(1, 1), (2, 2), (3, 3)] {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
        terminal
            .draw(|frame| {
                render(
                    frame,
                    frame.area(),
                    &mut input,
                    true,
                    "Message",
                    None,
                    &Theme::default(),
                );
            })
            .expect("tiny render");
    }
}

#[test]
fn composer_at_ceiling_renders_scrollbar_only_when_content_overflows() {
    fn track_symbols(lines: &str) -> Vec<String> {
        let mut input = InputState::default();
        input.set_buffer(lines.to_owned());
        let mut terminal = Terminal::new(TestBackend::new(20, 7)).expect("terminal");
        terminal
            .draw(|frame| {
                render(
                    frame,
                    Rect::new(0, 0, 20, 7),
                    &mut input,
                    true,
                    "Message",
                    None,
                    &Theme::default(),
                );
            })
            .expect("render");
        let buffer = terminal.backend().buffer();
        // The reserved track column is the last inner cell, left of the
        // right border at x = 19.
        (1..6)
            .map(|y| buffer[(18, y)].symbol().to_owned())
            .collect()
    }

    // Eight rows of content in a five-row box: the track column shows a
    // muted rail with a thumb covering the visible fraction.
    let overflowing = track_symbols("a\nb\nc\nd\ne\nf\ng\nh");
    assert!(
        overflowing
            .iter()
            .all(|symbol| symbol == "│" || symbol == "█"),
        "track column: {overflowing:?}"
    );
    assert!(
        overflowing.iter().any(|symbol| symbol == "█"),
        "thumb present: {overflowing:?}"
    );

    // Three rows fit the box: no reservation, the column stays text.
    let fitting = track_symbols("a\nb\nc");
    assert!(
        fitting.iter().all(|symbol| symbol != "│" && symbol != "█"),
        "no track when fitting: {fitting:?}"
    );
}

#[test]
fn rendered_input_reports_scrollbar_geometry_only_at_the_overflowing_ceiling() {
    fn rendered_scrollbar(lines: &str, area: Rect) -> Option<ScrollbarGeometry> {
        let mut input = InputState::default();
        input.set_buffer(lines.to_owned());
        let mut terminal =
            Terminal::new(TestBackend::new(area.width, area.height)).expect("terminal");
        let mut rendered = None;
        terminal
            .draw(|frame| {
                rendered = Some(render(
                    frame,
                    area,
                    &mut input,
                    true,
                    "Message",
                    None,
                    &Theme::default(),
                ));
            })
            .expect("render");
        rendered.and_then(|rendered| rendered.scrollbar)
    }

    // Eight rows in the five-row ceiling box: the reserved column
    // reports its track and thumb as the click/drag hit source.
    let geometry = rendered_scrollbar("a\nb\nc\nd\ne\nf\ng\nh", Rect::new(0, 0, 20, 7))
        .expect("scrollbar while overflowing at the ceiling");
    assert_eq!(geometry.track, Rect::new(18, 1, 1, 5));
    assert!(!geometry.thumb.is_empty());

    // Fitting content at the same height has nothing to scroll.
    assert!(rendered_scrollbar("a\nb\nc", Rect::new(0, 0, 20, 7)).is_none());
}

/// Across every pane width a composer can be laid out in, the wrap the
/// renderer picks, the columns the cursor may move through, the height the
/// layout asks for and the wheel's overflow flag must describe the *same*
/// text area — and that area must never collapse to zero columns while
/// there is a draft to show. Reserving the scrollbar columns in a pane too
/// narrow to host them broke all four at once: the text vanished and the
/// box stopped scrolling because its layout was measured at width zero.
#[test]
fn the_composer_reservation_never_collapses_the_text_area() {
    let draft = "the quick brown fox\n".repeat(8);
    for width in 0..=8 {
        let mut input = InputState::default();
        input.set_buffer(draft.clone());
        let area = Rect::new(0, 0, width, MAX_TEXT_ROWS + 2);
        let interior = width.saturating_sub(2);
        let mut terminal =
            Terminal::new(TestBackend::new(width.max(1), area.height)).expect("terminal");
        let mut rendered = None;
        terminal
            .draw(|frame| {
                rendered = Some(render(
                    frame,
                    area,
                    &mut input,
                    true,
                    "Message",
                    None,
                    &Theme::default(),
                ));
            })
            .expect("render");
        let rendered = rendered.expect("rendered input");
        let text = rendered.text_rect;
        assert_eq!(text.height, MAX_TEXT_ROWS);
        // Only ever one of the two candidates: the pane interior, or that
        // minus the reservation.
        assert!(
            text.width == interior || text.width == interior.saturating_sub(SCROLLBAR_RESERVE),
            "width {width} wrapped at {}",
            text.width
        );

        if interior == 0 {
            // No interior at all: nothing to paint, nothing to scroll.
            assert_eq!(text.width, 0);
            assert!(rendered.scrollbar.is_none());
            assert!(!input.has_overflow());
            continue;
        }

        assert!(
            text.width > 0,
            "width {width} collapsed its text area to zero"
        );
        // The track exists exactly when the columns were reserved for it,
        // and never covers the border.
        assert_eq!(
            rendered.scrollbar.is_some(),
            text.width == interior.saturating_sub(SCROLLBAR_RESERVE),
            "width {width} reserved {} of {interior} columns",
            text.width
        );
        if let Some(geometry) = rendered.scrollbar {
            assert!(geometry.track.right() < area.x + area.width);
            assert_eq!(geometry.track.x, text.right() + SCROLLBAR_MARGIN);
        }
        // The draft is on screen…
        let buffer = terminal.backend().buffer();
        assert!(
            (text.y..text.bottom())
                .flat_map(|y| (text.x..text.right()).map(move |x| (x, y)))
                .any(|(x, y)| buffer[(x, y)].symbol() != " "),
            "width {width} rendered no text for a long draft"
        );
        // …and overflow is judged at exactly that width, so the wheel and
        // the scrollbar agree with what the reader can see.
        assert_eq!(
            input.has_overflow(),
            visual_rows(&draft, text.width).len() > usize::from(text.height),
            "width {width} disagrees with its own wrap"
        );
        assert!(input.has_overflow(), "width {width} lost its overflow");
        assert!(
            input.composer_rows(interior) > usize::from(MAX_TEXT_ROWS),
            "width {width} measured a draft that fits the box it cannot"
        );
    }
}

#[test]
fn scroll_to_holds_the_viewport_until_an_edit_or_cursor_key_reanchors() {
    fn draw(terminal: &mut Terminal<TestBackend>, input: &mut InputState) {
        terminal
            .draw(|frame| {
                render(
                    frame,
                    Rect::new(0, 0, 12, 7),
                    input,
                    true,
                    "Message",
                    None,
                    &Theme::default(),
                );
            })
            .expect("render");
    }

    let mut input = InputState::default();
    input.set_buffer("a\nb\nc\nd\ne\nf\ng\nh".to_owned());
    let mut terminal = Terminal::new(TestBackend::new(12, 7)).expect("terminal");
    // Eight rows in a five-row viewport: the cursor anchors the bottom.
    draw(&mut terminal, &mut input);
    assert_eq!(input.viewport_row(), 3);

    // scroll_to positions the viewport directly, clamped to the exact
    // offset range, and renders keep the held position instead of
    // chasing the cursor at the bottom.
    input.scroll_to(0);
    draw(&mut terminal, &mut input);
    assert_eq!(input.viewport_row(), 0);
    input.scroll_to(usize::MAX);
    assert_eq!(input.viewport_row(), 3);

    // A cursor key ends the hold: the viewport follows the cursor again
    // (a held viewport would have stayed at row 1, hiding the cursor).
    input.scroll_to(1);
    draw(&mut terminal, &mut input);
    assert_eq!(input.viewport_row(), 1);
    input.move_up();
    draw(&mut terminal, &mut input);
    assert_eq!(input.viewport_row(), 2);

    // So does an edit (cursor back at the buffer end first).
    input.move_buffer_end();
    input.scroll_to(1);
    draw(&mut terminal, &mut input);
    input.insert('x');
    draw(&mut terminal, &mut input);
    assert_eq!(input.viewport_row(), 3);
}

#[test]
fn byte_at_display_position_matches_the_click_cursor_mapping() {
    let mut input = InputState::default();
    input.set_buffer("hello\nwrappedworld".to_owned());
    // Lay out at a width that soft-wraps the second logical line.
    let _ = input.visible_rows(6, 4);
    // Same display position feeds cursor placement and selection
    // anchors, so both must agree byte-for-byte.
    for row in 0u16..3 {
        for column in 0u16..7 {
            let byte = input.byte_at_display_position(row, column);
            let mut probe = InputState::default();
            probe.set_buffer("hello\nwrappedworld".to_owned());
            let _ = probe.visible_rows(6, 4);
            probe.set_cursor_from_display_position(row, column);
            assert_eq!(byte, probe.cursor_byte(), "row {row} column {column}");
        }
    }
    // An empty buffer maps every position to byte 0.
    let mut empty = InputState::default();
    let _ = empty.visible_rows(6, 2);
    assert_eq!(empty.byte_at_display_position(0, 4), 0);
}

#[test]
fn selection_cells_cover_soft_wraps_and_hard_breaks_exactly() {
    let mut input = InputState::default();
    input.set_buffer("abcdefgh".to_owned());
    // set_buffer leaves the cursor (and the viewport chasing it) at the
    // end; hold the viewport at the top so all rows stay addressable.
    input.scroll_to(0);
    let _ = input.visible_rows(4, 2);
    // Bytes 2..6 span the soft wrap: the first row covers its tail, the
    // second its head; the boundary byte belongs to the next row.
    assert_eq!(
        input.selection_cells(2, 6),
        vec![(0, 2, 4), (1, 0, 2)],
        "soft-wrapped rows"
    );
    // An empty range reports nothing.
    assert!(input.selection_cells(0, 0).is_empty());

    let mut broken = InputState::default();
    broken.set_buffer("abcd\nefgh".to_owned());
    broken.scroll_to(0);
    let _ = broken.visible_rows(4, 3);
    // Bytes 1..7 cover both rows and the newline between them; the
    // newline itself owns no cells on either row.
    assert_eq!(
        broken.selection_cells(1, 7),
        vec![(0, 1, 4), (1, 0, 2)],
        "hard-break rows"
    );
    // Viewport clipping: a one-row window at the top reports only the
    // first row's cells.
    let _ = broken.visible_rows(4, 1);
    assert_eq!(broken.selection_cells(1, 7), vec![(0, 1, 4)]);
}

#[test]
fn delete_byte_range_cuts_and_moves_the_cursor_to_the_gap() {
    let mut input = InputState::default();
    input.set_buffer("hello world".to_owned());
    let _ = input.visible_rows(11, 1);
    input.delete_byte_range(6, 9);
    assert_eq!(input.as_str(), "hello ld");
    assert_eq!(input.cursor_byte(), 6);
    // Bounds and empty ranges are ignored.
    input.delete_byte_range(4, 4);
    input.delete_byte_range(7, 99);
    assert_eq!(input.as_str(), "hello ld");
}
