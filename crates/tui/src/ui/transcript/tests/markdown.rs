use crate::ui::transcript::*;

use crate::markdown::{MarkdownDocument, PlainHighlighter};

use crate::state::AssistantChild;

use super::support::*;

#[test]
fn markdown_tables_and_inline_code_render_inside_the_gutter() {
    let state = assistant_state(vec![AssistantChild::Text {
        id: 1,
        version: 0,
        markdown: MarkdownDocument::new(
            "text with `inline code`\n\n| a | b |\n|---|---|\n| 1 | 2 |".to_owned(),
        ),
    }]);
    let layout = transcript_layout(&state, None, 50);
    let rendered = layout
        .lines
        .iter()
        .map(|line| line.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(rendered.contains("inline code"));
    assert!(rendered.contains("a"));
    assert!(rendered.contains("b"));
    assert!(
        layout
            .lines
            .iter()
            .all(|line| { unicode_width::UnicodeWidthStr::width(line.to_string().as_str()) <= 50 })
    );

    let table = "| name | details | state |\n|---|---|---|\n| alpha | a long `piledDynamicModel` value → should wrap — cleanly | ready |\n| beta | another long value that has multiple words | waiting |";
    let state = assistant_state(vec![AssistantChild::Text {
        id: 1,
        version: 0,
        markdown: MarkdownDocument::new(table.to_owned()),
    }]);
    for width in [80, 90] {
        let layout = transcript_layout(&state, None, width);
        let lines = layout
            .lines
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        assert!(lines.iter().all(|line| {
            unicode_width::UnicodeWidthStr::width(line.as_str()) <= usize::from(width)
        }));
        let borders = lines
            .iter()
            .filter_map(|line| line.find('│'))
            .collect::<Vec<_>>();
        assert!(borders.windows(2).all(|columns| columns[0] == columns[1]));
    }
}

#[test]
fn markdown_code_bypasses_prose_wrap_and_lists_use_nested_hanging_indents() {
    let state = assistant_state(vec![AssistantChild::Text {
            id: 1,
            version: 0,
            markdown: MarkdownDocument::new(
                "```\nabcdefghijklmnopq\n```\n\n- alpha beta gamma delta\n  - bravo charlie delta echo\n    - charlie delta echo foxtrot"
                    .to_owned(),
            ),
        }]);
    let rendered = transcript_layout(&state, None, 18)
        .lines
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>();

    // Code rows never prose-wrap: the band wraps them itself, behind the
    // same blank marker on the first row and the continuation.
    assert!(
        rendered.iter().any(|line| line == "│  abcdefghijklmno"),
        "{rendered:?}"
    );
    assert!(
        rendered.iter().any(|line| line.starts_with("│  pq")),
        "{rendered:?}"
    );
    for (first, continuation) in [
        ("│ • alpha beta", "│   gamma delta"),
        ("│   • bravo", "│     charlie"),
        ("│     • charlie", "│       delta echo"),
    ] {
        assert!(
            rendered.iter().any(|line| line.starts_with(first)),
            "{rendered:?}"
        );
        assert!(
            rendered.iter().any(|line| line.starts_with(continuation)),
            "{rendered:?}"
        );
    }
    assert!(
        rendered
            .iter()
            .all(|line| UnicodeWidthStr::width(line.as_str()) <= 18)
    );
}

#[test]
fn inline_code_spans_take_a_near_body_colour_and_high_contrast_keeps_its_chip() {
    fn inline_code_style(theme: &Theme) -> ratatui::style::Style {
        let state = assistant_state(vec![AssistantChild::Text {
            id: 1,
            version: 0,
            markdown: MarkdownDocument::new("use `cargo test` here".to_owned()),
        }]);
        let layout = transcript_layout_with(&state, None, 60, theme, &PlainHighlighter);
        layout
            .lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .find(|span| span.content.contains("cargo test"))
            .expect("inline code span")
            .style
    }

    // Default theme: a near-body colour, no tint, regular weight; the
    // colour replaces the source backticks.
    let default = inline_code_style(&Theme::default());
    assert_eq!(default.bg, None);
    assert_eq!(default.fg, Theme::default().inline_code().fg);
    assert!(
        !default
            .add_modifier
            .contains(ratatui::style::Modifier::BOLD)
    );

    // High contrast keeps its inverse-video chip so code still pops
    // against bright text.
    let contrast = inline_code_style(&Theme::new(
        crate::theme::ThemeKind::HighContrast,
        crate::theme::ColorLevel::Ansi16,
    ));
    assert_eq!(contrast.bg, Some(ratatui::style::Color::LightYellow));
}

#[test]
fn tab_indented_code_expands_to_tab_stops_on_a_solid_band() {
    let theme = Theme::default();
    let background = theme
        .code_background()
        .expect("the parchment theme bands code");
    let state = assistant_state(vec![AssistantChild::Text {
        id: 1,
        version: 0,
        markdown: MarkdownDocument::new("```go\nfunc f() {\n\treturn\n}\n```".to_owned()),
    }]);
    let layout = transcript_layout(&state, None, 40);
    let rendered = snapshot_lines(&layout.lines);
    assert!(!rendered.contains('\t'), "{rendered}");
    assert!(rendered.contains("        return"), "{rendered}");
    let code_rows = layout
        .lines
        .iter()
        .filter(|line| {
            line.spans
                .iter()
                .any(|span| span.style.bg == Some(background))
        })
        .collect::<Vec<_>>();
    assert_eq!(code_rows.len(), 3, "{rendered}");
    let widths = code_rows
        .iter()
        .map(|line| line.width())
        .collect::<Vec<_>>();
    assert!(widths.iter().all(|width| *width == widths[0]), "{widths:?}");
}

#[test]
fn user_messages_render_markdown_and_keep_typed_newlines() {
    let mut state = assistant_state(Vec::new());
    state.transcript = vec![TranscriptItem::user(
        "fix **this** and `that`\nsecond line\n\n- item",
    )];
    let rendered = transcript_layout(&state, None, 60)
        .lines
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>();

    assert_eq!(
        rendered,
        [
            "┌─ You",
            "│ fix this and that",
            "│ second line",
            "│ ",
            "│ • item",
        ]
    );
}

/// A long transcript lays its code out plain and highlights only what is
/// within a viewport of the visible rows, before the frame draws it; code
/// scrolled into view is highlighted in the frame that shows it, on the
/// rows it already had.
#[tokio::test]
async fn code_highlights_as_it_comes_into_view() {
    use cookie_agent_protocol::{AttemptId, EventPayload, RunId, SessionId};

    let mut app = test_app().await;
    let session = SessionId::new_v7();
    let mut events = vec![session_created(session, 1)];
    let mut seq = 1;
    for turn in 0..40 {
        let run = RunId::new_v7();
        let attempt = AttemptId::new_v7();
        seq += 1;
        events.push(event(
            session,
            seq,
            run,
            EventPayload::UserInputSubmitted {
                content: format!("step {turn}").into(),
            },
        ));
        seq += 1;
        events.push(attempt_started(session, seq, run, attempt, None));
        seq += 1;
        let code = (0..6)
            .map(|line| format!("    let value_{turn}_{line} = compute({line});\n"))
            .collect::<String>();
        events.push(turn_committed(
            session,
            seq,
            run,
            attempt,
            seq,
            vec![text_part(&format!("Turn {turn}:\n\n```rust\n{code}```\n"))],
            Vec::new(),
            None,
        ));
    }
    assert!(app.store.rebuild_session(session, 0, events));
    app.selected = Some(session);

    let code_rows = |app: &App| {
        let width = app.layout_cache.key.expect("laid out").width;
        let reference = transcript_layout(&app.store.sessions[&session], None, width)
            .lines
            .into_iter()
            .filter(|line| line.to_string().contains("let value_"))
            .map(|line| (line.to_string(), line))
            .collect::<std::collections::HashMap<_, _>>();
        // Every code row, with whether it is highlighted as a full layout
        // would highlight it.
        let rows = app
            .layout_cache
            .layout
            .lines
            .iter()
            .enumerate()
            .filter(|(_, line)| line.to_string().contains("let value_"))
            .map(|(index, line)| (index, reference[&line.to_string()] == *line))
            .collect::<Vec<_>>();
        assert_eq!(rows.len(), reference.len());
        rows
    };
    let visible = |app: &App| {
        let offset = app.conversation_scroll.offset;
        let height = usize::from(app.hit_map.conversation.expect("drawn").height);
        offset..offset + height
    };

    frame_rows(&mut app, 100, 30);
    let rows = code_rows(&app);
    let view = visible(&app);
    assert!(view.start > 0, "the transcript overflows the view");
    for (index, highlighted) in &rows {
        if view.contains(index) {
            assert!(highlighted, "visible code row {index} is highlighted");
        }
    }
    assert!(
        rows.iter().any(|(_, highlighted)| !highlighted),
        "code far out of view stays plain"
    );

    app.conversation_scroll.top();
    frame_rows(&mut app, 100, 30);
    let view = visible(&app);
    assert_eq!(view.start, 0);
    for (index, highlighted) in code_rows(&app) {
        if view.contains(&index) {
            assert!(
                highlighted,
                "code row {index} scrolled into view is highlighted"
            );
        }
    }
}
