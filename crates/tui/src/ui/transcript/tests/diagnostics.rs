use crate::ui::transcript::*;

use cookie_agent_protocol::SessionId;

use ratatui::{style::Modifier, text::Line};

use crate::markdown::PlainHighlighter;

use crate::state::SessionState;

use crate::theme::{ColorLevel, ThemeKind};

use super::support::*;

#[test]
fn multiline_error_details_fold_behind_their_title_until_expanded() {
    let state = SessionState {
        transcript: vec![TranscriptItem::Event {
            id: 1,
            version: 0,
            level: crate::state::EventLevel::Error,
            text: "Request rejected · HTTP 400\nResponse body:\nTemperature must be omitted".into(),
            repeat: 1,
        }],
        ..SessionState::default()
    };
    let render = |expanded: Option<&HashSet<BlockId>>| {
        transcript_layout_with_level(
            &state,
            expanded,
            80,
            &Theme::default(),
            &PlainHighlighter,
            crate::state::EventLevel::Info,
        )
    };
    let collapsed = render(None);
    let collapsed_text = snapshot_lines(&collapsed.lines);
    assert!(
        collapsed_text.contains("▸ Request rejected · HTTP 400"),
        "{collapsed_text}"
    );
    assert!(
        !collapsed_text.contains("Response body:"),
        "{collapsed_text}"
    );
    // No badge row: the marked title is the row's first line and its toggle.
    assert!(
        collapsed_text.starts_with("! ❌ ▸ Request rejected"),
        "{collapsed_text}"
    );
    assert_eq!(collapsed.regions.len(), 1);
    assert_eq!(collapsed.regions[0].id, BlockId::Event(1));
    assert_eq!(collapsed.regions[0].header_lines, Some(1));

    let expanded = HashSet::from([BlockId::Event(1)]);
    let expanded_text = snapshot_lines(&render(Some(&expanded)).lines);
    assert!(
        expanded_text.contains("▾ Request rejected · HTTP 400"),
        "{expanded_text}"
    );
    assert!(expanded_text.contains("Response body:"), "{expanded_text}");
    assert!(
        expanded_text.contains("Temperature must be omitted"),
        "{expanded_text}"
    );
}

#[test]
fn diagnostic_rows_mark_warnings_and_errors_in_their_title() {
    for level in [
        crate::state::EventLevel::Debug,
        crate::state::EventLevel::Info,
        crate::state::EventLevel::Warning,
        crate::state::EventLevel::Error,
    ] {
        let state = SessionState {
            transcript: vec![TranscriptItem::Event {
                id: 1,
                version: 0,
                level,
                text: "diagnostic".into(),
                repeat: 1,
            }],
            ..SessionState::default()
        };
        let rendered = transcript_layout_with_level(
            &state,
            None,
            60,
            &Theme::default(),
            &PlainHighlighter,
            crate::state::EventLevel::Debug,
        )
        .lines
        .iter()
        .map(|line| line.to_string())
        .collect::<Vec<_>>()
        .join("\n");
        // Warnings and errors carry their marker in the row itself, with no
        // badge row above it; debug keeps its badge and info has none.
        match level {
            crate::state::EventLevel::Info => assert_eq!(rendered, "· diagnostic"),
            crate::state::EventLevel::Warning => assert_eq!(rendered, "│ 🚨 diagnostic"),
            crate::state::EventLevel::Error => assert_eq!(rendered, "! ❌ diagnostic"),
            crate::state::EventLevel::Debug => {
                assert!(rendered.contains(level.badge()), "{rendered}");
            }
        }
    }
}

#[tokio::test]
async fn diagnostic_gutters_cover_hard_breaks_soft_wraps_and_scrolled_rows() {
    let text = format!(
        "intro\n\n  indented\nparagraph\n\n  {}\nlast",
        "e\u{301}界👩‍💻abcdefgh".repeat(40)
    );
    for (level, gutter) in [
        (crate::state::EventLevel::Debug, "· "),
        (crate::state::EventLevel::Info, "· "),
        (crate::state::EventLevel::Warning, "│ "),
        (crate::state::EventLevel::Error, "! "),
    ] {
        let mut app = test_app().await;
        let session = SessionId::new_v7();
        app.selected = Some(session);
        app.tree_root = Some(session);
        app.tui_config.minimum_event_level = crate::state::EventLevel::Debug;
        app.expanded_blocks
            .insert(session, HashSet::from([BlockId::Event(1)]));
        app.store.sessions.insert(
            session,
            SessionState {
                transcript: vec![TranscriptItem::Event {
                    id: 1,
                    version: 0,
                    level,
                    text: text.clone(),
                    repeat: 1,
                }],
                ..SessionState::default()
            },
        );
        // Reflow the same app, then clip at different visual rows.
        for width in [24, 12, 38] {
            app.conversation_scroll.top();
            conversation_rows(&mut app, width, 16);
            let content_width = app.hit_map.conversation.unwrap().width;
            let lines = &app.layout_cache.layout.lines;
            assert!(
                lines
                    .iter()
                    .all(|line| line.width() <= usize::from(content_width))
            );
            let body_start = lines
                .iter()
                .position(|line| {
                    line.spans
                        .first()
                        .is_some_and(|span| span.content == gutter)
                })
                .unwrap();
            let body = &lines[body_start..];
            assert!(body.len() > text.lines().count());
            assert!(body.iter().all(|line| line.spans[0].content == gutter));
            assert!(body.iter().any(|line| line.to_string() == gutter));
            let copied = extract_selection(body, (0, 0), (body.len() - 1, u16::MAX), &app.theme);
            assert!(!copied.contains('│'));
            assert!(!copied.contains('!'));
            assert!(!copied.contains('·'));
            assert!(copied.contains("intro\n\n  "));
            // Soft wraps add line breaks, but retain indentation and every
            // combining/ZWJ grapheme without inserting gutter characters.
            // The expanded title leads with the level's marker, if any, and
            // its chevron; a soft wrap may swallow the spaces between them.
            let copied = copied.replace('\n', "");
            let body_text = copied
                .trim_start_matches(['🚨', '❌', ' '])
                .strip_prefix('▾')
                .expect("chevron")
                .trim_start();
            assert_eq!(body_text, text.replace('\n', ""));
            for row_offset in [0, 2, 5] {
                app.conversation_scroll.scroll_to(body_start + row_offset);
                let visible = conversation_rows(&mut app, width, 16);
                assert!(visible.iter().all(|row| row.starts_with(gutter)));
            }
        }
    }
}

#[test]
fn diagnostic_wrapping_preserves_span_styles_indentation_and_graphemes() {
    use ratatui::style::Color;

    let theme = Theme::default();
    let inherited = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    let explicit = Style::default()
        .fg(Color::Red)
        .add_modifier(Modifier::ITALIC);
    let text = "e\u{301}界👩‍💻".repeat(12);
    for width in [6, 8, 10, 24] {
        let lines = role_block(
            Role::Warning,
            vec![
                Line::from(vec![
                    Span::raw("  indented"),
                    Span::styled(text.clone(), explicit),
                ])
                .style(inherited),
            ],
            width,
            &theme,
        );
        assert!(lines.iter().all(|line| line.width() <= usize::from(width)));
        let body = lines
            .iter()
            .filter(|line| line.spans.first().is_some_and(|span| span.content == "│ "))
            .collect::<Vec<_>>();
        assert!(body.len() > 1);
        let mut content = String::new();
        for line in body {
            assert_eq!(line.spans[0].style, theme.warning());
            for span in line.spans.iter().skip(1) {
                content.push_str(&span.content);
                assert!(span.style.add_modifier.contains(Modifier::BOLD));
                if span.style.fg == Some(Color::Red) {
                    assert!(span.style.add_modifier.contains(Modifier::ITALIC));
                    assert!(span.content.graphemes(true).all(|grapheme| {
                        ["e\u{301}", "界", "👩‍💻"].contains(&grapheme)
                    }));
                } else {
                    assert_eq!(span.style.fg, Some(Color::Cyan));
                }
            }
        }
        assert_eq!(content, format!("  indented{text}"));
    }
}

#[test]
fn full_diagnostic_copy_omits_narrow_headers_but_preserves_literal_tags() {
    for theme in [
        Theme::default(),
        Theme::new(ThemeKind::Mono, ColorLevel::None),
        Theme::new(ThemeKind::HighContrast, ColorLevel::Ansi16),
    ] {
        for (role, header) in [
            (Role::Debug, "[D]"),
            (Role::Internal, "[I]"),
            (Role::Warning, "[W]"),
            (Role::Error, "[E]"),
        ] {
            let message = "message\n[D]\n[I]\n[W]\n[E]\n\n  x";
            for width in [6, 7, 80] {
                let lines = role_block(
                    role,
                    message
                        .lines()
                        .map(|line| Line::from(line.to_owned()))
                        .collect(),
                    width,
                    &theme,
                );
                if width < 8 {
                    assert_eq!(lines[0].to_string(), header);
                    assert_eq!(extract_line(&lines[0], 0, u16::MAX, &theme), None);
                }
                let expected = match width {
                    6 => message.replacen("message", "mess\nage", 1),
                    7 => message.replacen("message", "messa\nge", 1),
                    _ => message.to_owned(),
                };
                assert_eq!(
                    extract_selection(&lines, (0, 0), (lines.len() - 1, u16::MAX), &theme),
                    expected,
                    "{header} at width {width}"
                );
            }
        }
    }
}

#[test]
fn event_threshold_hides_lower_levels_without_removing_them_from_state() {
    let state = SessionState {
        transcript: vec![
            TranscriptItem::Event {
                id: 1,
                version: 0,
                level: crate::state::EventLevel::Debug,
                text: "debug row".into(),
                repeat: 1,
            },
            TranscriptItem::Event {
                id: 2,
                version: 0,
                level: crate::state::EventLevel::Error,
                text: "error row".into(),
                repeat: 1,
            },
        ],
        ..SessionState::default()
    };
    let rendered = transcript_layout_with_level(
        &state,
        None,
        60,
        &Theme::default(),
        &PlainHighlighter,
        crate::state::EventLevel::Warning,
    )
    .lines
    .iter()
    .map(|line| line.to_string())
    .collect::<Vec<_>>()
    .join("\n");
    assert!(!rendered.contains("debug row"));
    assert!(rendered.contains("error row"));
    assert_eq!(state.transcript.len(), 2);
}

#[tokio::test]
async fn conversation_event_filter_hit_cycles_all_levels_wraps_and_applies_immediately() {
    let mut app = test_app().await;
    let session_id = SessionId::new_v7();
    app.selected = Some(session_id);
    app.store.sessions.insert(
        session_id,
        SessionState {
            transcript: [
                (crate::state::EventLevel::Debug, "debug row"),
                (crate::state::EventLevel::Info, "info row"),
                (crate::state::EventLevel::Warning, "warning row"),
                (crate::state::EventLevel::Error, "error row"),
            ]
            .into_iter()
            .enumerate()
            .map(|(index, (level, text))| TranscriptItem::Event {
                id: index as u64 + 1,
                version: 0,
                level,
                text: text.into(),
                repeat: 1,
            })
            .collect(),
            ..SessionState::default()
        },
    );
    app.tui_config.minimum_event_level = crate::state::EventLevel::Debug;

    for (current, next, visible_after_click, hidden_after_click) in [
        (
            crate::state::EventLevel::Debug,
            crate::state::EventLevel::Info,
            &["info row", "warning row", "error row"][..],
            &["debug row"][..],
        ),
        (
            crate::state::EventLevel::Info,
            crate::state::EventLevel::Warning,
            &["warning row", "error row"][..],
            &["debug row", "info row"][..],
        ),
        (
            crate::state::EventLevel::Warning,
            crate::state::EventLevel::Error,
            &["error row"][..],
            &["debug row", "info row", "warning row"][..],
        ),
        (
            crate::state::EventLevel::Error,
            crate::state::EventLevel::Debug,
            &["debug row", "info row", "warning row", "error row"][..],
            &[][..],
        ),
    ] {
        assert_eq!(app.tui_config.minimum_event_level, current);
        let rows = frame_rows(&mut app, 100, 30);
        let hit = app.hit_map.event_level_filter.expect("event filter hit");
        let label = format!("events ≥ {}", current.name());
        assert_eq!(rect_text(&rows, hit), label);
        assert_eq!(
            usize::from(hit.width),
            UnicodeWidthStr::width(label.as_str())
        );

        app.handle_click(hit.x, hit.y).await;
        assert_eq!(app.tui_config.minimum_event_level, next);
        assert_eq!(app.status, format!("Event level: {}", next.name()));

        let rendered = frame_rows(&mut app, 100, 30).join("\n");
        for text in visible_after_click {
            assert!(rendered.contains(text), "{next:?} should show {text}");
        }
        for text in hidden_after_click {
            assert!(!rendered.contains(text), "{next:?} should hide {text}");
        }
    }
}

#[tokio::test]
async fn descendant_retry_errors_fold_into_one_expandable_row() {
    let mut app = test_app().await;
    let root = SessionId::new_v7();
    let child = SessionId::new_v7();
    app.tree = Some(cookie_agent_protocol::SessionTree {
        session: titled_meta(root, "root session", 1),
        children: vec![cookie_agent_protocol::SessionTree {
            session: titled_meta(child, "Implement adapter", 1),
            children: Vec::new(),
        }],
    });
    app.tree_root = Some(root);
    app.selected = Some(root);
    assert!(app.store.apply_event(session_created(root, 1)));
    assert!(app.store.apply_event(run_started_with_suffix(
        root,
        2,
        cookie_agent_protocol::RunId::new_v7(),
        vec![resolved_model(None)],
    )));
    let model_error: cookie_agent_protocol::ModelErrorSummary = serde_json::from_value(serde_json::json!({
        "kind":"timeout","message":"OpenAI request failed","retryable":true,"stage":"stream_read",
        "http_status":503,"bytes_received":97,"vendor_code":null,"request_id":null,"retry_after_ms":null,
        "response_body":"{\n  \"error\": \"Service temporarily unavailable\"\n}"
    }))
    .unwrap();
    let run = cookie_agent_protocol::RunId::new_v7();
    let mut seq = 0;
    for _ in 0..6 {
        let attempt = cookie_agent_protocol::AttemptId::new_v7();
        seq += 1;
        assert!(
            app.store
                .apply_event(attempt_started(child, seq, run, attempt, None))
        );
        seq += 1;
        assert!(app.store.apply_event(event(
            child,
            seq,
            run,
            cookie_agent_protocol::EventPayload::AttemptAbandoned {
                attempt_id: attempt,
                model_error: Some(model_error.clone()),
            },
        )));
    }

    let collapsed = rendered_frame(&mut app, 160, 40);
    assert_eq!(
        collapsed.matches("model attempt abandoned").count(),
        1,
        "{collapsed}"
    );
    assert_eq!(collapsed.matches("repeated 6×").count(), 1, "{collapsed}");
    // No badge row; the marker leads the title (a wide glyph and its pad cell).
    assert!(!collapsed.contains("WARNING"), "{collapsed}");
    assert!(
        collapsed.contains("🚨  ▸ from Implement adapter"),
        "{collapsed}"
    );
    assert!(
        !collapsed.contains("Service temporarily unavailable"),
        "{collapsed}"
    );

    let row = app
        .hit_map
        .blocks
        .iter()
        .find_map(|hit| match hit.id {
            BlockId::DescendantEvent { session, .. } if session == child => Some(hit.id),
            _ => None,
        })
        .expect("the spliced row toggles");
    app.toggle_block(row);
    let expanded = rendered_frame(&mut app, 160, 40);
    assert!(
        expanded.contains("🚨  ▾ from Implement adapter"),
        "{expanded}"
    );
    assert!(
        expanded.contains("Service temporarily unavailable"),
        "{expanded}"
    );
}

#[tokio::test]
async fn multiline_event_toggles_in_its_own_session_view() {
    let mut app = test_app().await;
    let session = SessionId::new_v7();
    app.selected = Some(session);
    app.tree_root = Some(session);
    app.store.sessions.insert(
        session,
        SessionState {
            transcript: vec![TranscriptItem::Event {
                id: 1,
                version: 0,
                level: crate::state::EventLevel::Warning,
                text: "model attempt abandoned · HTTP 503\nResponse body:\nService temporarily unavailable"
                    .into(),
                repeat: 2,
            }],
            ..SessionState::default()
        },
    );
    let collapsed = rendered_frame(&mut app, 100, 24);
    assert!(collapsed.contains("repeated 2×"), "{collapsed}");
    assert!(!collapsed.contains("Response body:"), "{collapsed}");
    let row = app
        .hit_map
        .blocks
        .iter()
        .find(|hit| hit.id == BlockId::Event(1))
        .copied()
        .expect("the row toggles");
    let toggle = row.toggle_rect.expect("toggle rect");
    // Click it as a user would: through the cached layout of this session.
    app.handle_click(toggle.x, toggle.y).await;
    let expanded = rendered_frame(&mut app, 100, 24);
    assert!(expanded.contains("Response body:"), "{expanded}");
    app.handle_click(toggle.x, toggle.y).await;
    let collapsed_again = rendered_frame(&mut app, 100, 24);
    assert!(
        !collapsed_again.contains("Response body:"),
        "{collapsed_again}"
    );
}
