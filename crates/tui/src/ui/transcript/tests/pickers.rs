use crate::ui::transcript::*;

use cookie_agent_protocol::{
    AgentId, AttemptId, EventPayload, EventSubscriptionMessage, ModelSelection, RunSelection,
    SessionId, Sha256Digest,
};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use ratatui::{Terminal, backend::TestBackend, style::Modifier};

use crate::client::ClientDelivery;

use crate::markdown::MarkdownDocument;

use crate::state::AssistantChild;

use crate::theme::{ColorLevel, ThemeKind};

use crate::ui::app::*;

use crate::ui::pickers::SearchPickerFocus;

use crate::ui::terminal_layout_with_tree_rows;

use super::support::*;

#[tokio::test]
async fn message_title_is_exact_agent_model_variant_with_hit_regions() {
    let mut app = test_app().await;
    app.agents = vec![descriptor("primary", true)];
    app.models = vec![model_descriptor()];
    app.draft = Some(RunSelection {
        agent: agent_id(),
        model: ModelSelection {
            model: model_key(),
            variant: Some(cookie_agent_protocol::VariantId::new("high").expect("variant")),
        },
        preset: None,
    });
    let rendered = rendered_frame(&mut app, 100, 30);
    assert!(rendered.contains("primary • gateway/arbitrary-model[high]"));
    let segments = &app.hit_map.title_segments;
    assert_eq!(segments.len(), 3);
    let agent_rect = segments
        .iter()
        .find(|hit| hit.segment == TitleSegment::Agent)
        .expect("agent segment")
        .rect;
    let model_rect = segments
        .iter()
        .find(|hit| hit.segment == TitleSegment::Model)
        .expect("model segment")
        .rect;
    let variant_rect = segments
        .iter()
        .find(|hit| hit.segment == TitleSegment::Variant)
        .expect("variant segment")
        .rect;
    assert_eq!(agent_rect.width, 7);
    assert_eq!(model_rect.width, 23);
    assert_eq!(variant_rect.width, 6);
    assert_eq!(model_rect.x, agent_rect.x + agent_rect.width + 3);
    assert_eq!(variant_rect.x, model_rect.x + model_rect.width);
    let bullet_x = agent_rect.x + agent_rect.width + 1;
    assert!(
        segments
            .iter()
            .all(|hit| !hit.rect.contains((bullet_x, agent_rect.y).into())),
        "the bullet must remain decoration"
    );

    let narrow = rendered_frame(&mut app, 28, 12);
    assert!(narrow.contains("primary • gateway/arbit"));
    assert_eq!(app.hit_map.title_segments.len(), 2);
    let narrow_model = app
        .hit_map
        .title_segments
        .iter()
        .find(|hit| hit.segment == TitleSegment::Model)
        .expect("clipped model segment");
    assert_eq!(narrow_model.rect.x, agent_rect.x + agent_rect.width + 3);
    assert_eq!(narrow_model.rect.width, 16);
    assert!(
        app.hit_map
            .title_segments
            .iter()
            .all(|hit| hit.segment != TitleSegment::Variant)
    );
}

#[tokio::test]
async fn message_title_bolds_only_the_agent_name() {
    let mut app = test_app().await;
    // Pin the default true-color theme so style assertions do not
    // depend on the developer's ambient tui.toml or terminal detection.
    app.theme = Theme::default();
    app.agents = vec![descriptor("primary", true)];
    app.models = vec![model_descriptor()];
    app.draft = Some(RunSelection {
        agent: agent_id(),
        model: ModelSelection {
            model: model_key(),
            variant: Some(cookie_agent_protocol::VariantId::new("high").expect("variant")),
        },
        preset: None,
    });
    let backend = TestBackend::new(100, 30);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    terminal
        .draw(|frame| app.draw_for_test(frame))
        .expect("app render");
    let buffer = terminal.backend().buffer();
    let segment_rect = |segment: TitleSegment| {
        app.hit_map
            .title_segments
            .iter()
            .find(|hit| hit.segment == segment)
            .expect("title segment")
            .rect
    };
    let agent = segment_rect(TitleSegment::Agent);
    let model = segment_rect(TitleSegment::Model);
    let variant = segment_rect(TitleSegment::Variant);
    // The bullet is decoration between the agent and model segments.
    let bullet = Rect::new(
        agent.x.saturating_add(agent.width),
        agent.y,
        model.x.saturating_sub(agent.x.saturating_add(agent.width)),
        1,
    );

    // The focused composer's border is bold honey; only the agent
    // name keeps that weight. Everything in the title shares the border
    // accent color — bold is the emphasis, never a color marker.
    for x in agent.x..agent.x.saturating_add(agent.width) {
        let cell = buffer[(x, agent.y)].style();
        assert!(
            cell.add_modifier.contains(Modifier::BOLD),
            "agent name is bold: {cell:?}"
        );
        assert_eq!(
            cell.fg,
            app.theme.input_border(true).fg,
            "border accent: {cell:?}"
        );
    }
    for rect in [bullet, model, variant] {
        for x in rect.x..rect.x.saturating_add(rect.width) {
            let cell = buffer[(x, rect.y)].style();
            assert!(
                !cell.add_modifier.contains(Modifier::BOLD),
                "segment stays regular: {cell:?}"
            );
            assert_eq!(
                cell.fg,
                app.theme.input_border(true).fg,
                "shared color: {cell:?}"
            );
        }
    }
}

#[tokio::test]
async fn scrolled_message_title_hits_follow_visible_cells_or_disappear() {
    async fn app_at_scroll_position(position: usize, width: u16) -> (App, Vec<String>) {
        let mut app = test_app().await;
        app.agents = vec![descriptor("primary", true)];
        app.models = vec![model_descriptor()];
        app.draft = Some(RunSelection {
            agent: agent_id(),
            model: ModelSelection {
                model: model_key(),
                variant: None,
            },
            preset: None,
        });
        app.input
            .set_buffer("zero\none\ntwo\nthree\nfour\nfive\nsix".into());
        frame_rows(&mut app, width, 24);
        match position {
            0 => app.input.move_buffer_home(),
            1 => {
                app.input.move_buffer_home();
                for _ in 0..3 {
                    app.input.move_down();
                }
            }
            2 => {}
            _ => unreachable!(),
        }
        // The seven-line draft grows the composer to its five-text-row
        // ceiling, so the viewport only scrolls once the cursor passes
        // row four: positions land at rows 0, 3, and 6.
        assert_eq!(app.input.viewport_row(), [0, 0, 2][position]);
        let rows = frame_rows(&mut app, width, 24);
        (app, rows)
    }

    for position in 0..3 {
        let (mut app, rows) = app_at_scroll_position(position, 100).await;
        let agent = app
            .hit_map
            .title_segments
            .iter()
            .find(|hit| hit.segment == TitleSegment::Agent)
            .copied()
            .expect("visible agent");
        let model = app
            .hit_map
            .title_segments
            .iter()
            .find(|hit| hit.segment == TitleSegment::Model)
            .copied()
            .expect("visible model");
        let variant = app
            .hit_map
            .title_segments
            .iter()
            .find(|hit| hit.segment == TitleSegment::Variant)
            .copied()
            .expect("visible variant");
        assert_eq!(rect_text(&rows, agent.rect), "primary");
        assert_eq!(rect_text(&rows, model.rect), "gateway/arbitrary-model");
        assert_eq!(rect_text(&rows, variant.rect), "[base]");

        app.handle_click(agent.rect.x, agent.rect.y).await;
        assert_eq!(app.modal, Modal::Agents);
        assert!(app.draft.as_ref().expect("draft").model.variant.is_none());
        app.modal = Modal::None;
        frame_rows(&mut app, 100, 24);

        app.handle_click(model.rect.x, model.rect.y).await;
        assert_eq!(app.modal, Modal::Models);
        assert!(app.draft.as_ref().expect("draft").model.variant.is_none());
        app.modal = Modal::None;
        frame_rows(&mut app, 100, 24);

        app.handle_click(variant.rect.x, variant.rect.y).await;
        assert_eq!(app.modal, Modal::None);
        assert_eq!(
            app.draft
                .as_ref()
                .and_then(|draft| draft.model.variant.as_ref())
                .map(|variant| variant.as_str()),
            Some("fast")
        );

        let rows = frame_rows(&mut app, 100, 24);
        let agent = app
            .hit_map
            .title_segments
            .iter()
            .find(|hit| hit.segment == TitleSegment::Agent)
            .copied()
            .expect("visible agent");
        let model = app
            .hit_map
            .title_segments
            .iter()
            .find(|hit| hit.segment == TitleSegment::Model)
            .copied()
            .expect("visible model");
        let bullet_x = agent.rect.x + agent.rect.width + 1;
        assert_eq!(
            rect_text(&rows, Rect::new(bullet_x, agent.rect.y, 1, 1)),
            "•"
        );
        app.handle_click(bullet_x, agent.rect.y).await;
        assert_eq!(app.modal, Modal::None);
        assert_eq!(model.rect.x, bullet_x + 2);
        assert_eq!(
            app.draft
                .as_ref()
                .and_then(|draft| draft.model.variant.as_ref())
                .map(|variant| variant.as_str()),
            Some("fast")
        );

        let marker_x = rows[usize::from(agent.rect.y)]
            .chars()
            .position(|character| matches!(character, '↑' | '↓'))
            .and_then(|column| u16::try_from(column).ok())
            .expect("scroll marker");
        app.handle_click(marker_x, agent.rect.y).await;
        assert_eq!(app.modal, Modal::None);
        assert_eq!(
            app.draft
                .as_ref()
                .and_then(|draft| draft.model.variant.as_ref())
                .map(|variant| variant.as_str()),
            Some("fast")
        );
    }

    for position in 0..3 {
        let (mut app, rows) = app_at_scroll_position(position, 28).await;
        // Mirror draw(): the composer's text-row demand comes from its
        // actual content at the frame width, not a fixed height.
        let input_text_rows = u16::try_from(app.input.content_rows(28 - 2))
            .unwrap_or(u16::MAX)
            .clamp(1, crate::ui::input::MAX_TEXT_ROWS);
        let title_y = terminal_layout_with_tree_rows(
            Rect::new(0, 0, 28, 24),
            app.tree_entries().len(),
            0,
            false,
            input_text_rows,
        )
        .input
        .y;
        assert!(app.hit_map.title_segments.is_empty());
        assert!(!rows[usize::from(title_y)].contains("primary"));
        let original_variant = app.draft.as_ref().expect("draft").model.variant.clone();
        for column in [1, 9, 11] {
            app.handle_click(column, title_y).await;
            assert_eq!(app.modal, Modal::None);
            assert_eq!(
                app.draft.as_ref().expect("draft").model.variant,
                original_variant
            );
        }
        let marker_x = rows[usize::from(title_y)]
            .chars()
            .position(|character| matches!(character, '↑' | '↓'))
            .and_then(|column| u16::try_from(column).ok())
            .expect("scroll marker");
        app.handle_click(marker_x, title_y).await;
        assert_eq!(app.modal, Modal::None);
        assert_eq!(
            app.draft.as_ref().expect("draft").model.variant,
            original_variant
        );
    }
}

#[tokio::test]
async fn title_segments_open_pickers_and_cycle_variant_by_mouse() {
    let mut app = test_app().await;
    app.agents = vec![descriptor("primary", true)];
    app.models = vec![model_descriptor()];
    app.draft = Some(RunSelection {
        agent: agent_id(),
        model: ModelSelection {
            model: model_key(),
            variant: None,
        },
        preset: None,
    });
    rendered_frame(&mut app, 100, 30);
    let agent = app
        .hit_map
        .title_segments
        .iter()
        .find(|hit| hit.segment == TitleSegment::Agent)
        .expect("agent segment")
        .rect;
    app.handle_click(agent.x + 1, agent.y).await;
    assert_eq!(app.modal, Modal::Agents);
    app.modal = Modal::None;
    rendered_frame(&mut app, 100, 30);
    let variant = app
        .hit_map
        .title_segments
        .iter()
        .find(|hit| hit.segment == TitleSegment::Variant)
        .expect("variant segment")
        .rect;
    app.handle_click(variant.x + 1, variant.y).await;
    assert_eq!(app.modal, Modal::None);
    assert_eq!(
        app.draft
            .as_ref()
            .and_then(|draft| draft.model.variant.as_ref())
            .map(|variant| variant.as_str()),
        Some("fast")
    );
}

#[tokio::test]
async fn draft_model_picker_uses_global_catalog_and_variant_cycle_is_inline() {
    let mut app = test_app().await;
    app.agents = vec![descriptor("primary", true)];
    app.models = vec![model_descriptor()];
    app.draft = Some(RunSelection {
        agent: agent_id(),
        model: ModelSelection {
            model: model_key(),
            variant: None,
        },
        preset: None,
    });
    assert_eq!(app.draft_models().len(), app.models.len());
    let variants = app.draft_variants();
    assert_eq!(variants.len(), 3);
    assert!(variants[0].is_none());
    assert_eq!(variants[1].as_ref().map(|v| v.as_str()), Some("fast"));
    assert_eq!(variants[2].as_ref().map(|v| v.as_str()), Some("high"));

    // Cycling a variant changes only the draft; active runs are frozen.
    let session = SessionId::new_v7();
    app.selected = Some(session);
    app.store.sessions.entry(session).or_default().active_run = Some(run_id());
    app.store
        .sessions
        .get_mut(&session)
        .expect("session")
        .run_agent = Some(agent_id());
    frame_rows(&mut app, 80, 24);
    let variant_hit = app
        .hit_map
        .title_segments
        .iter()
        .find(|hit| hit.segment == TitleSegment::Variant)
        .copied()
        .expect("variant hit");
    app.handle_click(variant_hit.rect.x, variant_hit.rect.y)
        .await;
    assert!(app.status.contains("the active run is unchanged"));
    assert_eq!(
        app.active_run_agent().map(|agent| agent.as_str()),
        Some("primary")
    );
}

#[tokio::test]
async fn global_out_of_chain_models_render_and_select_at_normal_and_narrow_widths() {
    let mut app = test_app().await;
    app.agents = vec![descriptor("primary", true)];
    let mut outside = catalog_model("other/catalog-model", &["default", "high"], Some("default"));
    outside.display_name = "Outside".into();
    let mut base = model_descriptor();
    base.display_name = "Base".into();
    app.models = vec![base, outside.clone()];
    app.draft = app.default_draft_selection();
    assert_eq!(app.filtered_draft_models().len(), 2);

    for (width, theme) in [
        (100, Theme::new(ThemeKind::Default, ColorLevel::TrueColor)),
        (48, Theme::new(ThemeKind::Mono, ColorLevel::None)),
    ] {
        app.theme = theme;
        app.modal = Modal::Models;
        let rows = frame_rows(&mut app, width, 24);
        if width == 100 {
            assert!(
                rows.iter()
                    .any(|row| row.contains("Outside other/catalog-model[default]")),
                "width {width}: {rows:?}"
            );
            assert!(
                rows.iter()
                    .any(|row| row.contains("Base gateway/arbitrary-model[base]")),
                "width {width}: {rows:?}"
            );
        } else {
            // Narrow panels retain the display name and ellipsize the
            // trailing canonical key.
            assert!(
                rows.iter()
                    .any(|row| row.contains("Outside other/catalog") && row.contains('…'))
            );
            assert!(
                rows.iter()
                    .any(|row| row.contains("Base gateway/arbitrar") && row.contains('…'))
            );
        }
        assert_eq!(app.hit_map.picker_rows.len(), 2);
    }

    app.choose_picker_entry(1).await;
    let draft = app.draft.as_ref().expect("draft");
    assert_eq!(draft.model.model, outside.key);
    assert_eq!(
        draft.model.variant.as_ref().map(|variant| variant.as_str()),
        Some("default")
    );
}

#[tokio::test]
async fn agent_search_filters_resets_transitions_focus_and_selects_filtered_indices() {
    let mut app = test_app().await;
    let mut first = descriptor("alpha", true);
    first.description = "First Choice".into();
    let mut second = descriptor("bravo", true);
    second.description = "Needle Agent".into();
    let mut third = descriptor("charlie", true);
    third.description = "Third Choice with a deliberately long explanation".into();
    let first_id = first.id.clone();
    let second_id = second.id.clone();
    app.agents = vec![first, second, third];
    app.models = vec![model_descriptor()];
    app.draft = app.default_draft_selection();

    app.open_selection_modal(Modal::Agents);
    assert_eq!(app.agent_search.focus(), SearchPickerFocus::Input);
    assert_eq!(
        app.filtered_agent_picker_candidates().len(),
        3,
        "empty query matches all"
    );
    type_input(&mut app, "nEeDlE").await;
    assert_eq!(
        app.filtered_agent_picker_candidates()
            .iter()
            .map(|agent| &agent.id)
            .collect::<Vec<_>>(),
        [&second_id]
    );
    let rendered = rendered_frame(&mut app, 100, 30);
    assert!(rendered.contains("Agent (1/3)"));
    assert!(rendered.contains("bravo Needle Agent"));

    app.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL))
        .await;
    type_input(&mut app, "BRAVO").await;
    assert_eq!(
        app.filtered_agent_picker_candidates().len(),
        1,
        "ID match ignores case"
    );
    app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))
        .await;
    assert_eq!(app.agent_search.focus(), SearchPickerFocus::List);
    app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE))
        .await;
    assert_eq!(app.agent_search.focus(), SearchPickerFocus::Input);
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .await;
    assert_eq!(app.agent_search.focus(), SearchPickerFocus::List);
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .await;
    assert_eq!(app.modal, Modal::None);
    assert_eq!(
        app.draft.as_ref().map(|draft| &draft.agent),
        Some(&second_id),
        "filtered keyboard index maps to the underlying agent"
    );

    app.open_selection_modal(Modal::Agents);
    assert!(app.agent_search.query().is_empty(), "opening resets search");
    type_input(&mut app, "CHARLIE").await;
    let narrow = rendered_frame(&mut app, 48, 24);
    assert!(
        narrow.contains("charlie ") && narrow.contains('…'),
        "{narrow}"
    );
    app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))
        .await;
    rendered_frame(&mut app, 100, 30);
    let input = app.hit_map.picker_input.expect("agent search input");
    app.handle_click(input.text_rect.x, input.text_rect.y).await;
    assert_eq!(app.agent_search.focus(), SearchPickerFocus::Input);
    app.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL))
        .await;
    type_input(&mut app, "ALPHA").await;
    rendered_frame(&mut app, 100, 30);
    let row = app.hit_map.picker_rows[0].rect;
    app.handle_click(row.x, row.y).await;
    assert_eq!(
        app.draft.as_ref().map(|draft| &draft.agent),
        Some(&first_id),
        "filtered mouse index maps to the underlying agent"
    );

    app.agents[0].description.clear();
    app.open_selection_modal(Modal::Agents);
    assert!(app.agent_search.query().is_empty());
    type_input(&mut app, "ALPHA").await;
    let rendered = rendered_frame(&mut app, 100, 30);
    assert!(rendered.contains("> alpha"), "{rendered}");
    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
        .await;
    assert_eq!(app.modal, Modal::None);
    assert!(app.agent_search.query().is_empty());
}

#[tokio::test]
async fn agent_picker_styled_rows_snapshot_across_themes() {
    let mut snapshots = Vec::new();
    for (label, theme) in [
        (
            "default",
            Theme::new(ThemeKind::Default, ColorLevel::TrueColor),
        ),
        ("mono", Theme::new(ThemeKind::Mono, ColorLevel::None)),
        (
            "high-contrast",
            Theme::new(ThemeKind::HighContrast, ColorLevel::Ansi16),
        ),
    ] {
        let mut app = test_app().await;
        app.theme = theme;
        let mut first = descriptor("alpha", true);
        first.description = "First Choice".into();
        let mut second = descriptor("bravo", true);
        second.description = "Needle Agent".into();
        app.agents = vec![first, second];
        app.models = vec![model_descriptor()];
        app.draft = app.default_draft_selection();
        app.open_selection_modal(Modal::Agents);
        app.agent_search.focus_list();
        app.picker_state.select(Some(1));

        let mut terminal = Terminal::new(TestBackend::new(100, 30)).expect("terminal");
        terminal
            .draw(|frame| app.draw_for_test(frame))
            .expect("agent picker render");
        let buffer = terminal.backend().buffer();
        let rows = app.hit_map.picker_rows.clone();
        let row_text = |row: Rect| {
            (row.x..row.right())
                .map(|x| buffer[(x, row.y)].symbol())
                .collect::<String>()
                .trim_end()
                .to_owned()
        };
        let first_id_style = buffer[(rows[0].rect.x + 2, rows[0].rect.y)].style();
        let first_description_style = buffer[(rows[0].rect.x + 2 + 5, rows[0].rect.y)].style();
        let selected_id_style = buffer[(rows[1].rect.x + 2, rows[1].rect.y)].style();
        let selected_description_style = buffer[(rows[1].rect.x + 2 + 5, rows[1].rect.y)].style();
        assert_ne!(
            selected_id_style, selected_description_style,
            "{label} keeps span hierarchy"
        );
        assert_eq!(
            selected_id_style.bg, selected_description_style.bg,
            "{label} shares the selection background"
        );
        assert_eq!(
            selected_id_style.add_modifier.contains(Modifier::REVERSED),
            selected_description_style
                .add_modifier
                .contains(Modifier::REVERSED),
            "{label} shares reverse-video selection"
        );
        assert!(selected_id_style.add_modifier.contains(Modifier::BOLD));
        match label {
            "default" => {
                assert_ne!(selected_id_style.fg, selected_description_style.fg);
                assert!(
                    !selected_description_style
                        .add_modifier
                        .contains(Modifier::BOLD)
                );
            }
            "mono" => {
                assert!(
                    selected_description_style
                        .add_modifier
                        .contains(Modifier::DIM)
                );
                assert!(
                    !selected_description_style
                        .add_modifier
                        .contains(Modifier::BOLD)
                );
            }
            "high-contrast" => {
                assert_ne!(selected_id_style.fg, selected_description_style.fg);
                assert!(
                    selected_description_style
                        .add_modifier
                        .contains(Modifier::DIM)
                );
            }
            _ => unreachable!("covered theme"),
        }
        snapshots.push(format!(
                "== {label} ==\n{}\n{}\nplain id: {first_id_style:?}\nplain description: {first_description_style:?}\nselected id: {selected_id_style:?}\nselected description: {selected_description_style:?}",
                row_text(rows[0].rect),
                row_text(rows[1].rect),
            ));
    }
    insta::assert_snapshot!(snapshots.join("\n"));
}

#[tokio::test]
async fn model_search_filters_resets_transitions_focus_and_selects_filtered_indices() {
    let mut app = test_app().await;
    app.agents = vec![descriptor("primary", true)];
    let mut first = catalog_model("alpha/first-model", &[], None);
    first.display_name = "First Choice".into();
    let mut second = catalog_model("beta/second-model", &["fast"], Some("fast"));
    second.display_name = "Needle Model".into();
    let mut third = catalog_model("gamma/third-model", &[], None);
    third.display_name = "Third Choice".into();
    let third_key = third.key.clone();
    app.models = vec![first, second.clone(), third];
    app.draft = app.default_draft_selection();

    app.open_selection_modal(Modal::Models);
    assert_eq!(app.model_search.focus(), SearchPickerFocus::Input);
    assert_eq!(
        app.filtered_draft_models().len(),
        3,
        "empty query matches all"
    );
    type_input(&mut app, "nEeDlE").await;
    assert_eq!(
        app.filtered_draft_models(),
        vec![ModelSelection {
            model: second.key.clone(),
            variant: second.default_variant.clone(),
        }]
    );
    let rendered = rendered_frame(&mut app, 100, 30);
    assert!(rendered.contains("Model (1/3)"));
    assert!(rendered.contains("Needle Model beta/second-model[fast]"));

    app.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL))
        .await;
    type_input(&mut app, "SECOND-MODEL").await;
    assert_eq!(
        app.filtered_draft_models().len(),
        1,
        "key match ignores case"
    );
    app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))
        .await;
    assert_eq!(app.model_search.focus(), SearchPickerFocus::List);
    app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE))
        .await;
    assert_eq!(app.model_search.focus(), SearchPickerFocus::Input);
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .await;
    assert_eq!(app.model_search.focus(), SearchPickerFocus::List);
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        .await;
    assert_eq!(app.modal, Modal::None);
    assert_eq!(
        app.draft.as_ref().map(|draft| &draft.model.model),
        Some(&second.key),
        "filtered index maps to the underlying model"
    );

    app.open_selection_modal(Modal::Models);
    assert!(app.model_search.query().is_empty(), "opening resets search");
    type_input(&mut app, "FAST").await;
    assert_eq!(
        app.filtered_draft_models().len(),
        1,
        "selected variant display names are searchable"
    );
    app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))
        .await;
    rendered_frame(&mut app, 100, 30);
    let input = app.hit_map.picker_input.expect("model search input");
    app.handle_click(input.text_rect.x, input.text_rect.y).await;
    assert_eq!(app.model_search.focus(), SearchPickerFocus::Input);
    app.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL))
        .await;
    type_input(&mut app, "THIRD-MODEL").await;
    rendered_frame(&mut app, 100, 30);
    let row = app.hit_map.picker_rows[0].rect;
    app.handle_click(row.x, row.y).await;
    assert_eq!(
        app.draft.as_ref().map(|draft| &draft.model.model),
        Some(&third_key),
        "filtered row clicks map to the underlying model"
    );

    app.open_selection_modal(Modal::Models);
    assert!(app.model_search.query().is_empty());
    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
        .await;
    assert_eq!(app.modal, Modal::None);
    assert!(app.model_search.query().is_empty());
}

#[tokio::test]
async fn model_picker_styled_rows_snapshot_across_themes() {
    let mut snapshots = Vec::new();
    for (label, theme) in [
        (
            "default",
            Theme::new(ThemeKind::Default, ColorLevel::TrueColor),
        ),
        ("mono", Theme::new(ThemeKind::Mono, ColorLevel::None)),
        (
            "high-contrast",
            Theme::new(ThemeKind::HighContrast, ColorLevel::Ansi16),
        ),
    ] {
        let mut app = test_app().await;
        app.theme = theme;
        let mut first = catalog_model("alpha/first-model", &[], None);
        first.display_name = "First Choice".into();
        let mut second = catalog_model("beta/second-model", &["fast"], Some("fast"));
        second.display_name = "Needle Model".into();
        app.models = vec![first, second];
        app.draft = app.default_draft_selection();
        app.open_selection_modal(Modal::Models);
        app.model_search.focus_list();
        app.picker_state.select(Some(1));

        let mut terminal = Terminal::new(TestBackend::new(100, 30)).expect("terminal");
        terminal
            .draw(|frame| app.draw_for_test(frame))
            .expect("model picker render");
        let buffer = terminal.backend().buffer();
        let rows = app.hit_map.picker_rows.clone();
        let row_text = |row: Rect| {
            (row.x..row.right())
                .map(|x| buffer[(x, row.y)].symbol())
                .collect::<String>()
                .trim_end()
                .to_owned()
        };
        let first_display = buffer[(rows[0].rect.x + 2, rows[0].rect.y)].style();
        let first_key = buffer[(rows[0].rect.x + 2 + 12, rows[0].rect.y)].style();
        let selected_display = buffer[(rows[1].rect.x + 2, rows[1].rect.y)].style();
        let selected_key = buffer[(rows[1].rect.x + 2 + 12, rows[1].rect.y)].style();
        assert_ne!(
            selected_display, selected_key,
            "{label} keeps span hierarchy"
        );
        assert_eq!(
            selected_display.bg, selected_key.bg,
            "{label} shares the selection background"
        );
        assert_eq!(
            selected_display.add_modifier.contains(Modifier::REVERSED),
            selected_key.add_modifier.contains(Modifier::REVERSED),
            "{label} shares reverse-video selection"
        );
        assert!(selected_display.add_modifier.contains(Modifier::BOLD));
        match label {
            "default" => {
                assert_ne!(selected_display.fg, selected_key.fg);
                assert!(!selected_key.add_modifier.contains(Modifier::BOLD));
            }
            "mono" => {
                assert!(selected_key.add_modifier.contains(Modifier::DIM));
                assert!(!selected_key.add_modifier.contains(Modifier::BOLD));
            }
            "high-contrast" => {
                assert_ne!(selected_display.fg, selected_key.fg);
                assert!(selected_key.add_modifier.contains(Modifier::DIM));
            }
            _ => unreachable!("covered theme"),
        }
        snapshots.push(format!(
                "== {label} ==\n{}\n{}\nplain display: {first_display:?}\nplain key: {first_key:?}\nselected display: {selected_display:?}\nselected key: {selected_key:?}",
                row_text(rows[0].rect),
                row_text(rows[1].rect),
            ));
    }
    insta::assert_snapshot!(snapshots.join("\n"));
}

#[tokio::test]
async fn composer_variant_hit_cycles_in_declared_order_wraps_and_one_entry_is_noop() {
    let mut app = test_app().await;
    app.agents = vec![descriptor("primary", true)];
    app.models = vec![catalog_model(MODEL, &["high", "default", "fast"], None)];
    app.draft = app.default_draft_selection();

    for expected in [Some("high"), Some("default"), Some("fast"), None] {
        let rows = frame_rows(&mut app, 48, 24);
        let hit = app
            .hit_map
            .title_segments
            .iter()
            .find(|hit| hit.segment == TitleSegment::Variant)
            .copied()
            .expect("visible bracketed variant hit");
        let before = app
            .draft
            .as_ref()
            .and_then(|draft| draft.model.variant.as_ref())
            .map_or("base", |variant| variant.as_str());
        assert!(
            rows.iter().any(|row| row.contains(&format!("[{before}]"))),
            "{rows:?}"
        );
        assert_eq!(hit.rect.width, u16::try_from(before.len() + 2).unwrap());
        app.handle_click(hit.rect.x, hit.rect.y).await;
        assert_eq!(
            app.draft
                .as_ref()
                .and_then(|draft| draft.model.variant.as_ref())
                .map(|variant| variant.as_str()),
            expected
        );
    }

    app.models = vec![catalog_model(MODEL, &[], None)];
    app.revalidate_draft();
    let before = app.draft.clone();
    app.cycle_draft_variant();
    assert_eq!(app.draft, before);

    app.models = vec![catalog_model(MODEL, &["high", "default", "fast"], None)];
    app.models[0].variant_order =
        vec![cookie_agent_protocol::VariantId::new("high").expect("variant")];
    app.revalidate_draft();
    assert_eq!(
        app.draft_variants()
            .iter()
            .map(|variant| variant.as_ref().map(|id| id.as_str()))
            .collect::<Vec<_>>(),
        vec![None, Some("default"), Some("fast"), Some("high")],
        "descriptor drift falls back to lexical order"
    );
}

#[tokio::test]
async fn composer_variant_hit_cycles_k3_base_low_high_max() {
    let mut app = test_app().await;
    app.agents = vec![descriptor("primary", true)];
    app.models = vec![catalog_model(
        "kimi-for-coding/k3",
        &["low", "high", "max"],
        None,
    )];
    app.draft = app.default_draft_selection();

    assert_eq!(
        app.draft_variants()
            .iter()
            .map(|variant| variant.as_ref().map(|id| id.as_str()))
            .collect::<Vec<_>>(),
        vec![None, Some("low"), Some("high"), Some("max")]
    );
    for expected in [Some("low"), Some("high"), Some("max"), None] {
        app.cycle_draft_variant();
        assert_eq!(
            app.draft
                .as_ref()
                .and_then(|draft| draft.model.variant.as_ref())
                .map(|variant| variant.as_str()),
            expected
        );
    }
}

#[tokio::test]
async fn committed_fallback_updates_next_draft_but_preserves_explicit_picker_reset() {
    let mut app = test_app().await;
    let session = SessionId::new_v7();
    let run = run_id();
    let a = resolved_model(None);
    let mut b = a.clone();
    b.selection.model = "gateway/fallback-model".parse().unwrap();
    b.model_id = cookie_agent_protocol::ProviderModelId::new("fallback-model").unwrap();
    b.selection_fingerprint = Sha256Digest::of_bytes(b"fallback");
    app.models = vec![
        model_descriptor(),
        catalog_model("gateway/fallback-model", &[], None),
    ];
    app.sessions = vec![session_meta(session)];
    app.set_selected_session(session);
    let start = run_started_with_suffix(session, 2, run, vec![a.clone(), b.clone()]);
    let mut committed = turn_committed(
        session,
        3,
        run,
        AttemptId::new_v7(),
        3,
        vec![],
        vec![],
        None,
    );
    if let EventPayload::ModelTurnCommitted { resolved_model, .. } = &mut committed.payload {
        *resolved_model = b.clone();
    }
    let events = vec![
        session_created_with(session, 1, AGENT, vec![a.clone(), b.clone()], 0),
        start.clone(),
        committed.clone(),
    ];
    for event in &events {
        app.handle_delivery(ClientDelivery::Live {
            message: Box::new(EventSubscriptionMessage::Event {
                event: Box::new(event.clone()),
            }),
            generation: 0,
        })
        .await;
    }
    assert_eq!(app.draft.as_ref().unwrap().model, b.selection);
    assert_eq!(
        app.store.sessions[&session]
            .run_selected_suffix
            .as_ref()
            .unwrap()[0]
            .selection,
        a.selection,
        "frozen run history is unchanged"
    );
    app.set_draft_model(a.selection.model.clone());
    assert!(app.draft_reset_fallback);
    committed.seq = 4;
    app.handle_delivery(ClientDelivery::Live {
        message: Box::new(EventSubscriptionMessage::Event {
            event: Box::new(committed),
        }),
        generation: 0,
    })
    .await;
    assert_eq!(
        app.draft.as_ref().unwrap().model,
        a.selection,
        "late commits cannot override the explicit draft"
    );
    let mut restarted = test_app().await;
    restarted.models = app.models.clone();
    restarted.sessions = vec![session_meta(session)];
    for event in events {
        restarted.store.apply_event(event);
    }
    restarted.set_selected_session(session);
    assert_eq!(
        restarted.draft.as_ref().unwrap().model,
        b.selection,
        "watch/replay uses session progress, not creation selection"
    );
}

#[tokio::test]
async fn model_agent_and_refresh_normalization_preserve_only_valid_draft_parts() {
    let mut app = test_app().await;
    let first = model_descriptor();
    let second = catalog_model("other/catalog-model", &["default", "high"], Some("default"));
    let mut primary = descriptor("primary", true);
    primary.resolved_fallback = vec![ModelSelection {
        model: second.key.clone(),
        variant: Some(cookie_agent_protocol::VariantId::new("high").expect("variant")),
    }];
    let mut reviewer = descriptor("reviewer", true);
    reviewer.resolved_fallback = vec![ModelSelection {
        model: second.key.clone(),
        variant: None,
    }];
    app.agents = vec![primary, reviewer];
    app.models = vec![first.clone(), second.clone()];
    app.draft = Some(RunSelection {
        agent: agent_id(),
        model: ModelSelection {
            model: first.key.clone(),
            variant: Some(cookie_agent_protocol::VariantId::new("high").expect("variant")),
        },
        preset: None,
    });

    app.set_draft_model(first.key.clone());
    assert_eq!(
        app.draft
            .as_ref()
            .and_then(|draft| draft.model.variant.as_ref())
            .map(|variant| variant.as_str()),
        Some("high"),
        "reselecting the current model preserves its variant"
    );
    app.set_draft_agent(AgentId::new("reviewer").expect("agent"));
    assert_eq!(app.draft.as_ref().expect("draft").model.model, second.key);
    assert_eq!(
        app.draft
            .as_ref()
            .and_then(|draft| draft.model.variant.as_ref())
            .map(|variant| variant.as_str()),
        None,
        "agent changes select the new agent's first live fallback"
    );

    app.set_draft_model(second.key.clone());
    assert_eq!(
        app.draft
            .as_ref()
            .and_then(|draft| draft.model.variant.as_ref())
            .map(|variant| variant.as_str()),
        Some("default"),
        "model changes select the model default"
    );
    app.draft.as_mut().expect("draft").model.variant =
        Some(cookie_agent_protocol::VariantId::new("removed").expect("variant"));
    app.revalidate_draft();
    assert_eq!(
        app.draft
            .as_ref()
            .and_then(|draft| draft.model.variant.as_ref())
            .map(|variant| variant.as_str()),
        Some("default"),
        "a missing variant resets to the model default"
    );

    app.draft.as_mut().expect("draft").agent = agent_id();
    app.draft.as_mut().expect("draft").model.model = "missing/model".parse().expect("model");
    app.revalidate_draft();
    let draft = app.draft.as_ref().expect("draft");
    assert_eq!(draft.model.model, second.key);
    assert_eq!(
        draft.model.variant.as_ref().map(|variant| variant.as_str()),
        Some("high"),
        "missing models prefer the agent's authored available fallback"
    );
}

#[tokio::test]
async fn draft_clicks_do_not_mutate_active_or_committed_frozen_attribution() {
    let mut app = test_app().await;
    app.agents = vec![descriptor("primary", true)];
    app.models = vec![model_descriptor()];
    app.draft = app.default_draft_selection();
    let session = SessionId::new_v7();
    app.selected = Some(session);
    let state = app.store.sessions.entry(session).or_default();
    state.active_run = Some(run_id());
    state.run_agent = Some(agent_id());
    state.transcript = vec![TranscriptItem::Assistant {
        id: 1,
        version: 0,
        attribution: attribution(Some("default")),
        committed_turn_seq: Some(1),
        children: vec![AssistantChild::Text {
            id: 2,
            version: 0,
            markdown: MarkdownDocument::new("frozen".into()),
        }],
    }];

    frame_rows(&mut app, 80, 24);
    let variant_hit = app
        .hit_map
        .title_segments
        .iter()
        .find(|hit| hit.segment == TitleSegment::Variant)
        .copied()
        .expect("variant hit");
    app.handle_click(variant_hit.rect.x, variant_hit.rect.y)
        .await;
    let TranscriptItem::Assistant { attribution, .. } = &app.store.sessions[&session].transcript[0]
    else {
        panic!("assistant")
    };
    assert_eq!(
        attribution.header(),
        "primary • gateway/arbitrary-model[default]"
    );
    assert_eq!(app.active_run_agent().map(AgentId::as_str), Some("primary"));
    let rendered = frame_rows(&mut app, 80, 24).join("\n");
    assert!(rendered.contains("primary • gateway/arbitrary-model[default]"));
    assert!(rendered.contains("primary • gateway/arbitrary-model[fast]"));
}
