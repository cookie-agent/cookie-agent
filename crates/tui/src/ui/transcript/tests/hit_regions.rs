use crate::ui::transcript::*;

use cookie_agent_protocol::{OutputDelta, OutputStream, SessionId, ToolCallId};

use crossterm::event::{MouseButton, MouseEventKind};

use ratatui::text::Line;

use crate::client::ClientDelivery;

use crate::state::{AssistantChild, ToolCallState};

use crate::ui::app::*;

use base64::{Engine as _, engine::general_purpose::STANDARD};

use super::support::*;

#[test]
fn block_hit_rects_are_clipped_and_shifted_by_scroll_offset() {
    let region = BlockRegion {
        id: BlockId::Thinking(1),
        start_line: 10,
        end_line: 20,
        header_lines: None,
        header_gutter: None,
    };
    let viewport = Rect::new(0, 0, 40, 5);
    let hit = block_hit(region, &[], viewport, 8).expect("hit");
    assert_eq!(hit.rect.y, 2);
    assert_eq!(hit.rect.height, 3);
    assert_eq!(hit.hover_rect.unwrap(), Rect::new(0, 2, 40, 1));
    assert_eq!(hit.toggle_rect, hit.hover_rect);
    let tool = BlockRegion {
        header_lines: Some(3),
        ..region
    };
    assert_eq!(
        block_hit(tool, &[], viewport, 8).unwrap().hover_rect,
        Some(Rect::new(0, 2, 40, 3))
    );
    assert_eq!(
        block_hit(tool, &[], viewport, 11).unwrap().hover_rect,
        Some(Rect::new(0, 0, 40, 2))
    );
    assert_eq!(block_hit(tool, &[], viewport, 13).unwrap().hover_rect, None);
    assert_eq!(
        block_hit(tool, &[], viewport, 13).unwrap().toggle_rect,
        None
    );
    assert_eq!(
        block_hit(tool, &[], viewport, 11).unwrap().toggle_rect,
        Some(Rect::new(0, 0, 40, 2))
    );
    let notice = BlockRegion {
        id: tool_output_id(ToolCallId::new_v7(), ToolOutputSection::Detail),
        ..region
    };
    assert_eq!(
        block_hit(notice, &[], viewport, 8).unwrap().toggle_rect,
        Some(Rect::new(0, 2, 40, 1))
    );
    assert_eq!(
        block_hit(notice, &[], viewport, 11).unwrap().toggle_rect,
        None
    );
    assert_eq!(
        block_hit(region, &[], viewport, 13).unwrap().hover_rect,
        Some(Rect::new(0, 0, 40, 1))
    );
    assert!(block_hit(region, &[], viewport, 25).is_none());
}

#[test]
fn block_highlights_stop_at_the_gutter_and_at_the_reservation() {
    let viewport = Rect::new(1, 4, pane_text_width(80), 12);
    let lines = vec![
        Line::from(vec![Span::raw("│ "), Span::raw("💭 ▸ thinking")]),
        Line::from(vec![Span::raw("│ "), Span::raw("one thought")]),
        Line::from(vec![Span::raw("┆ "), Span::raw("a continuation")]),
    ];
    let region = BlockRegion {
        id: BlockId::Thinking(1),
        start_line: 0,
        end_line: 3,
        header_lines: Some(2),
        header_gutter: None,
    };
    let hit = block_hit(region, &lines, viewport, 0).expect("visible block");
    assert_eq!(
        hit.rect,
        Rect::new(1, 4, viewport.width, 3),
        "the gutter and the blank tail of a row still click"
    );
    let hovered = hit.hover_rect.expect("hoverable header");
    assert_eq!(hovered, Rect::new(3, 4, viewport.width - 2, 2));
    assert_eq!(
        hovered.right(),
        viewport.right(),
        "the highlight ends where the reservation begins"
    );

    // A gutter welded to its text by `append_span` is still chrome for as
    // far as it reaches, and the row is only as protected as its widest
    // hovered gutter.
    let welded = vec![
        Line::from(vec![Span::raw("│ 💭 ▸ thinking")]),
        Line::from(vec![Span::raw("two thoughts")]),
    ];
    let hit = block_hit(region, &welded, viewport, 0).expect("visible block");
    assert_eq!(hit.hover_rect, Some(Rect::new(3, 4, viewport.width - 2, 2)));
    let plain = vec![Line::from(vec![Span::raw("-- run started")])];
    let hit = block_hit(region, &plain, viewport, 0).expect("visible block");
    assert_eq!(
        hit.hover_rect,
        Some(Rect::new(1, 4, viewport.width, 2)),
        "a gutterless row is highlighted edge to edge"
    );
}

#[test]
fn hover_clamps_to_the_builder_gutter_not_to_lookalike_output() {
    let viewport = Rect::new(1, 4, pane_text_width(80), 12);
    // A tool header row: the builder's gutter, then literal output that is
    // itself a gutter glyph — a tree listing really does start with `"│ "`
    // — arriving as its own span, as highlighted output does.
    let lines = vec![Line::from(vec![
        Span::styled("│ ", Theme::default().assistant()),
        Span::raw("│ "),
        Span::raw("├── src"),
    ])];
    let hover = |header_gutter| {
        block_hit(
            BlockRegion {
                id: BlockId::ToolOutput {
                    call_id: ToolCallId::new_v7(),
                    section: ToolOutputSection::Detail,
                },
                start_line: 0,
                end_line: lines.len(),
                header_lines: Some(1),
                header_gutter,
            },
            &lines,
            viewport,
            0,
        )
        .expect("visible header")
        .hover_rect
    };
    assert_eq!(
        leading_gutter_columns(&lines[0]),
        4,
        "what reading chrome off the row's glyphs makes of it"
    );
    assert_eq!(
        hover(Some(2)),
        Some(Rect::new(3, 4, viewport.width - 2, 1)),
        "the highlight stops after the builder's own gutter"
    );
    assert_eq!(
        hover(None),
        Some(Rect::new(5, 4, viewport.width - 4, 1)),
        "without provenance the clamp falls back to the glyph walk"
    );

    // The builder's count is what reaches the region, and it counts chrome
    // the glyphs cannot see: the block gutter plus a diff's line-number
    // column, both hung there by the same code.
    let rendered = tool_block_lines(
        Role::ToolSuccess,
        vec![ToolBodyLine::guttered_code(
            Line::from("ls"),
            vec![Span::raw("1 │ ")],
            vec![Span::raw("  │ ")],
        )],
        40,
        &Theme::default(),
    );
    assert_eq!(rendered.chrome[0], 6);
    assert_eq!(header_gutter_columns(&rendered), 6);
    assert_eq!(
        leading_gutter_columns(&rendered.lines[0]),
        2,
        "a line-number column is invisible to a glyph walk"
    );
}

#[tokio::test]
async fn hovering_a_block_only_repaints_its_own_content_columns() {
    let (mut app, _session, block) = expansion_scroll_app("thinking").await;
    rendered_frame(&mut app, 80, 24);
    let plain = drawn_styles(&mut app, 80, 24);
    app.hover = Some(HoverTarget::TranscriptBlock(block));
    let hovered = drawn_styles(&mut app, 80, 24);
    let viewport = app.hit_map.conversation.expect("conversation viewport");
    let hit = app
        .hit_map
        .blocks
        .iter()
        .find(|hit| hit.id == block)
        .expect("thinking block");
    let rect = hit.hover_rect.expect("hovered header row");
    assert!(rect.x > viewport.x, "the gutter is not a highlight");
    assert_eq!(rect.right(), viewport.right());
    assert!(
        rect.rows()
            .any(|row| plain[usize::from(row.y)][usize::from(rect.x)]
                != hovered[usize::from(row.y)][usize::from(rect.x)]),
        "the header row is highlighted"
    );
    for (y, row) in hovered.iter().enumerate() {
        for (x, style) in row.iter().enumerate() {
            if rect.contains(ratatui::layout::Position::new(x as u16, y as u16)) {
                continue;
            }
            assert_eq!(
                plain[y][x], *style,
                "cell ({x},{y}) changed outside the block's content"
            );
        }
    }
}

#[tokio::test]
async fn mouse_clicks_toggle_blocks_while_the_composer_keeps_focus() {
    let mut app = test_app().await;
    let session = SessionId::new_v7();
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
    rendered_frame(&mut app, 80, 24);
    let block = app.hit_map.blocks.first().copied().expect("block hit");
    let title = block.toggle_rect.expect("title");
    app.handle_click(title.x, title.y).await;
    assert!(
        app.expanded_blocks
            .get(&session)
            .is_some_and(|set| set.contains(&block.id))
    );
    // Clicking conversation content never takes focus from the composer:
    // typing right after the click still lands in the draft.
    assert!(app.composer_focused());
    app.handle_key(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Char('x'),
        crossterm::event::KeyModifiers::NONE,
    ))
    .await;
    assert_eq!(app.input.as_str(), "x");
    app.input.set_buffer(String::new());
    rendered_frame(&mut app, 80, 24);
    let block = app.hit_map.blocks.first().copied().unwrap();
    let body_row = block.toggle_rect.unwrap().bottom();
    assert!(body_row < block.rect.bottom());
    assert_eq!(app.hover_target_at(block.rect.x, body_row), None);
    app.handle_click(block.rect.x, body_row).await;
    assert!(app.expanded_blocks[&session].contains(&block.id));
    app.handle_mouse(mouse(
        MouseEventKind::Down(MouseButton::Left),
        block.rect.x + 2,
        body_row,
    ))
    .await;
    app.handle_mouse(mouse(
        MouseEventKind::Drag(MouseButton::Left),
        block.rect.x + 7,
        body_row,
    ))
    .await;
    app.handle_mouse(mouse(
        MouseEventKind::Up(MouseButton::Left),
        block.rect.x + 7,
        body_row,
    ))
    .await;
    assert!(matches!(
        app.selection,
        Some(TextSelection::Conversation { .. })
    ));
    assert!(app.expanded_blocks[&session].contains(&block.id));
}

#[tokio::test]
async fn clicking_tool_output_notice_expands_and_collapses_nested_view() {
    let mut app = test_app().await;
    let session = SessionId::new_v7();
    let rows = (1..=70)
        .map(|number| (number, "let value = 1;"))
        .collect::<Vec<_>>();
    let state = read_tool_state("src/main.rs", ToolStatus::Completed, &read_detail(&rows));
    let call_id = read_tool_id(&state);
    app.selected = Some(session);
    app.tree_root = Some(session);
    app.store.sessions.insert(session, state);
    app.expanded_blocks
        .insert(session, HashSet::from([BlockId::Tool(call_id)]));

    rendered_frame(&mut app, 80, 100);
    let notice = app
        .hit_map
        .blocks
        .iter()
        .find(|hit| hit.id == tool_output_id(call_id, ToolOutputSection::Detail))
        .copied()
        .expect("collapsed output notice");
    let toggle = notice.toggle_rect.expect("notice toggle");
    assert_eq!(toggle.height, 1);
    app.handle_click(toggle.x, toggle.y).await;
    assert!(
        app.expanded_blocks[&session].contains(&tool_output_id(call_id, ToolOutputSection::Detail))
    );

    rendered_frame(&mut app, 80, 100);
    let collapse = app
        .hit_map
        .blocks
        .iter()
        .find(|hit| hit.id == tool_output_id(call_id, ToolOutputSection::Detail))
        .copied()
        .expect("expanded output collapse notice");
    let tool = app
        .hit_map
        .blocks
        .iter()
        .find(|hit| hit.id == BlockId::Tool(call_id))
        .copied()
        .unwrap();
    let body_row = tool.toggle_rect.unwrap().bottom();
    assert_eq!(app.hover_target_at(tool.rect.x, body_row), None);
    app.handle_click(tool.rect.x, body_row).await;
    assert!(
        app.expanded_blocks[&session].contains(&tool_output_id(call_id, ToolOutputSection::Detail))
    );
    let toggle = collapse.toggle_rect.expect("collapse toggle");
    app.handle_click(toggle.x, toggle.y).await;
    assert!(
        !app.expanded_blocks[&session]
            .contains(&tool_output_id(call_id, ToolOutputSection::Detail))
    );
}

#[tokio::test]
async fn raw_streams_do_not_create_implicit_display_click_regions() {
    let mut app = test_app().await;
    let session = SessionId::new_v7();
    let call_id = ToolCallId::new_v7();
    let text = (0..100).map(|_| "output").collect::<Vec<_>>().join("\n");
    let mut state = assistant_state(vec![AssistantChild::Tool { call_id }]);
    state.tools.insert(
        call_id,
        ToolCallState {
            id: call_id,
            owner: owner(1, "call-1"),
            presentation: presentation("bash", None),
            arguments: r#"{"command":"build"}"#.into(),
            status: ToolStatus::Running,
            detail: text.clone(),
            has_output_chunks: false,
        },
    );
    app.store.sessions.insert(session, state);
    for stream in [OutputStream::Stdout, OutputStream::Stderr] {
        app.store
            .apply_delivery(ClientDelivery::OutputDelta(OutputDelta {
                call_id,
                stream,
                byte_offset: 0,
                data: STANDARD.encode(text.as_bytes()),
            }));
    }
    app.selected = Some(session);
    app.tree_root = Some(session);
    app.expanded_blocks
        .insert(session, HashSet::from([BlockId::Tool(call_id)]));

    rendered_frame(&mut app, 80, 100);
    let sections = app
        .hit_map
        .blocks
        .iter()
        .filter_map(|hit| match hit.id {
            BlockId::ToolOutput { section, .. } => Some(section),
            _ => None,
        })
        .collect::<HashSet<_>>();
    assert_eq!(sections, HashSet::from([ToolOutputSection::Detail]));
    let detail_id = tool_output_id(call_id, ToolOutputSection::Detail);
    let detail = app
        .hit_map
        .blocks
        .iter()
        .find(|hit| hit.id == detail_id)
        .copied()
        .expect("display notice");
    let toggle = detail.toggle_rect.expect("display toggle");
    app.handle_click(toggle.x, toggle.y).await;
    assert!(app.expanded_blocks[&session].contains(&detail_id));
    assert!(
        !app.expanded_blocks[&session]
            .contains(&tool_output_id(call_id, ToolOutputSection::Stdout))
    );
    assert!(
        !app.expanded_blocks[&session]
            .contains(&tool_output_id(call_id, ToolOutputSection::Stderr))
    );
    assert!(app.expanded_blocks[&session].contains(&BlockId::Tool(call_id)));
}

#[tokio::test]
async fn composer_loses_focus_only_under_an_overlay_or_in_a_read_only_view() {
    let mut app = test_app().await;
    let session = SessionId::new_v7();
    app.selected = Some(session);
    assert!(app.composer_focused());

    app.open_command_palette();
    assert!(!app.composer_focused());
    app.close_command_palette();
    assert!(app.composer_focused());

    app.run_command(crate::ui::slash::SlashCommand::Sessions)
        .await;
    assert_eq!(app.modal, Modal::Sessions);
    assert!(!app.composer_focused());
    app.handle_key(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Esc,
        crossterm::event::KeyModifiers::NONE,
    ))
    .await;
    assert_eq!(app.modal, Modal::None);
    assert!(app.composer_focused());

    let approval = bash_approval_state();
    app.selected = Some(approval.session_id);
    app.store
        .sessions
        .entry(approval.session_id)
        .or_default()
        .approvals
        .push(approval.clone());
    assert!(!app.composer_focused());
    app.store
        .sessions
        .get_mut(&approval.session_id)
        .expect("session")
        .approvals
        .clear();
    assert!(app.composer_focused());

    app.read_only_sessions.insert(approval.session_id);
    assert!(!app.composer_focused());
}
