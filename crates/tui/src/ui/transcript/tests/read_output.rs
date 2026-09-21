use crate::ui::transcript::*;

use ratatui::text::Line;

use super::support::*;

#[test]
fn read_rust_output_is_syntax_highlighted_with_tool_gutter_preserved() {
    let state = read_tool_state(
        "src/main.rs",
        ToolStatus::Completed,
        &read_detail(&[(1, "fn main() {"), (2, "    let x = 1;"), (3, "}")]),
    );
    let lines = expanded_read_layout(&state, &Theme::default());
    let rendered = lines
        .iter()
        .map(|line| line.to_string())
        .collect::<Vec<_>>();
    assert!(rendered.iter().any(|line| line.contains("fn main() {")));
    // Highlighting produces more than one distinct foreground color.
    let colors = lines
        .iter()
        .flat_map(|line| line.spans.iter())
        .filter_map(|span| span.style.fg)
        .collect::<std::collections::HashSet<_>>();
    assert!(colors.len() > 1);
}

#[test]
fn read_errors_and_non_read_tools_stay_plain() {
    let state = read_tool_state("src/main.rs", ToolStatus::Failed, "permission denied");
    let lines = expanded_read_layout(&state, &Theme::default());
    let failure_colors = lines
        .iter()
        .flat_map(|line| line.spans.iter())
        .filter(|span| span.content.contains("permission denied"))
        .filter_map(|span| span.style.fg)
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(failure_colors.len(), 1, "{failure_colors:?}");
    let mut bash = read_tool_state("src/main.rs", ToolStatus::Completed, "fn main() {}");
    bash.tools.values_mut().next().expect("tool").presentation = presentation("bash", None);
    let lines = expanded_read_layout(&bash, &Theme::default());
    // A non-read tool never gets read highlighting: content spans share
    // the single tool-success foreground.
    let content_colors = lines
        .iter()
        .flat_map(|line| line.spans.iter())
        .filter(|span| span.content.contains("fn main"))
        .filter_map(|span| span.style.fg)
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(content_colors.len(), 1);
}

#[test]
fn tool_path_and_extension_parsing_is_structural() {
    let file_path =
        ParsedToolArguments::parse(r#"{"filePath":"src/main.rs","content":"path: fake.txt"}"#)
            .expect("arguments");
    assert_eq!(file_path.file_path(), Some("src/main.rs"));
    let path = ParsedToolArguments::parse(r#"{"path":"src/lib.rs"}"#).expect("arguments");
    assert_eq!(path.file_path(), Some("src/lib.rs"));
    assert_eq!(path_extension("src/main.rs"), Some("rs"));
    assert_eq!(path_extension("README"), None);
    assert!(ParsedToolArguments::parse("not json").is_none());
}

#[test]
fn read_line_number_gutter_aligns_single_and_four_digit_numbers() {
    let state = read_tool_state(
        "src/main.rs",
        ToolStatus::Completed,
        &read_detail(&[(1, "one"), (1000, "thousand")]),
    );
    let lines = expanded_read_layout(&state, &Theme::default());
    let rendered = lines.iter().map(ToString::to_string).collect::<Vec<_>>();
    assert!(rendered.iter().any(|line| line.contains("│    1 │ one")));
    assert!(
        rendered
            .iter()
            .any(|line| line.contains("│ 1000 │ thousand"))
    );
}

#[test]
fn guttered_code_wraps_without_repeating_the_line_number() {
    let body = vec![ToolBodyLine::guttered_code(
        Line::from("abcdefghijklmnopqrstuvwxyz"),
        vec![Span::raw("1"), Span::raw(" │ ")],
        vec![Span::raw(" "), Span::raw(" │ ")],
    )];
    let rendered = tool_block_lines(Role::ToolSuccess, body, 12, &Theme::default()).lines;
    let text = rendered.iter().map(ToString::to_string).collect::<Vec<_>>();
    assert!(text.len() > 1);
    assert_eq!(text.iter().filter(|line| line.contains("1 │ ")).count(), 1);
    assert!(text.iter().skip(1).all(|line| line.starts_with("│   │ ")));
    assert!(text.last().is_some_and(|line| line.ends_with('z')));
    assert!(
        text.iter()
            .all(|line| UnicodeWidthStr::width(line.as_str()) <= 12)
    );
}

#[test]
fn guttered_code_handles_zero_and_one_column_widths() {
    for width in [0, 1] {
        let body = vec![ToolBodyLine::guttered_code(
            Line::from("abc"),
            vec![Span::raw("100 │ ")],
            vec![Span::raw("    │ ")],
        )];
        let rendered = tool_block_lines(Role::ToolSuccess, body, width, &Theme::default())
            .lines
            .iter()
            .map(ToString::to_string)
            .collect::<String>();
        assert_eq!(rendered, "abc", "width {width}");
    }

    for (width, expected) in [(0, "�"), (1, "�"), (2, "界"), (3, "│ �")] {
        let body = vec![ToolBodyLine::guttered_code(
            Line::from("界"),
            vec![Span::raw("1 │ ")],
            vec![Span::raw("  │ ")],
        )];
        let lines = tool_block_lines(Role::ToolSuccess, body, width, &Theme::default()).lines;
        assert_eq!(snapshot_lines(&lines), expected, "width {width}");
        assert!(lines.iter().all(|line| {
            UnicodeWidthStr::width(line.to_string().as_str()) <= usize::from(width.max(1))
        }));
    }
}

#[test]
fn absent_and_unknown_read_languages_fall_back_to_plain_code() {
    for path in ["README", "src/value.unknown-language"] {
        let state = read_tool_state(
            path,
            ToolStatus::Completed,
            &read_detail(&[(1, "plain value")]),
        );
        let rendered = snapshot_lines(&expanded_read_layout(&state, &Theme::default()));
        assert!(rendered.contains("1 │ plain value"), "{path}: {rendered}");
    }
}
