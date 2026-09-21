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

    assert!(rendered.iter().any(|line| line == "│ │ abcdefghijklmn"));
    assert!(rendered.iter().any(|line| line.starts_with("│ │opq")));
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
fn inline_code_spans_use_a_foreground_chip_except_in_high_contrast() {
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

    // Default theme: warm terracotta foreground, never a background —
    // the source backticks stay visible, and bold carries the
    // distinction where color is unavailable.
    let default = inline_code_style(&Theme::default());
    assert!(default.bg.is_none());
    assert_eq!(default.fg, Theme::default().inline_code().fg);
    assert!(
        default
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
