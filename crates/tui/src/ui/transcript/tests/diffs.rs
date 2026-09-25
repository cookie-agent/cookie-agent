use crate::ui::transcript::*;

use cookie_agent_protocol::ToolCallId;

use crate::state::{AssistantChild, ToolCallState};
use crate::theme::{ColorLevel, ThemeKind};

use super::support::*;

#[test]
fn unified_diff_detection_handles_boundaries_without_false_positives() {
    let text = "@@ -1 +1 @@\n-old\n+new";
    assert!(is_unified_diff(text));
    let mut parsed = Vec::new();
    for_each_diff_row(&ToolDiff::Unified(text), |row| {
        parsed.push((row.kind, row.text.into_owned()));
        true
    });
    assert!(matches!(parsed[1].0, DiffRowKind::Removed(1)));
    assert!(matches!(parsed[2].0, DiffRowKind::Added(1)));
    assert_eq!(parsed[2].1, "new");

    assert!(is_unified_diff(
        "diff --git a/image.png b/image.png\nBinary files a/image.png and b/image.png differ"
    ));

    assert!(!is_unified_diff(
        "status: +added-looking text\nordinary output"
    ));
    assert!(!is_unified_diff("Binary files may differ"));
}

#[test]
fn synthetic_diffs_preserve_newline_termination_semantics() {
    let terminated = ToolDiff::Edit {
        before: "old\n",
        after: "new\n",
        metadata: "",
    };
    let mut terminated_rows = Vec::new();
    for_each_diff_row(&terminated, |row| {
        terminated_rows.push((row.kind, row.text.into_owned()));
        true
    });
    assert_eq!(terminated_rows[0].1, "@@ -1 +1 @@");
    assert_eq!(
        terminated_rows
            .iter()
            .filter(|(kind, _)| matches!(kind, DiffRowKind::Added(_) | DiffRowKind::Removed(_)))
            .count(),
        2
    );
    assert!(
        !terminated_rows
            .iter()
            .any(|(kind, _)| *kind == DiffRowKind::NoNewline)
    );

    let unterminated = ToolDiff::Edit {
        before: "old",
        after: "new",
        metadata: "",
    };
    let mut markers = 0;
    for_each_diff_row(&unterminated, |row| {
        markers += usize::from(row.kind == DiffRowKind::NoNewline);
        true
    });
    assert_eq!(markers, 2);

    let empty_write = ToolDiff::Write {
        content: "",
        metadata: "",
    };
    let mut hunk = None;
    for_each_diff_row(&empty_write, |row| {
        hunk = Some(row.text.into_owned());
        false
    });
    assert_eq!(hunk.as_deref(), Some("@@ -0,0 +0,0 @@"));
}

#[test]
fn edit_arguments_render_added_and_removed_diff_gutters() {
    let call_id = ToolCallId::new_v7();
    let mut state = assistant_state(vec![AssistantChild::Tool { call_id }]);
    state.tools.insert(
        call_id,
        ToolCallState {
            id: call_id,
            owner: owner(1, "call-1"),
            presentation: presentation("edit", Some("src/main.rs")),
            arguments: serde_json::json!({
                "filePath": "src/main.rs",
                "oldString": "let old = 1;",
                "newString": "let new = 2;"
            })
            .to_string(),
            status: ToolStatus::Completed,
            detail: "Edited src/main.rs\nEdit applied atomically".into(),
            has_output_chunks: false,
        },
    );
    let expanded = HashSet::from([BlockId::Tool(call_id)]);
    let lines = transcript_layout(&state, Some(&expanded), 80).lines;
    assert!(
        lines
            .iter()
            .any(|line| line.to_string().contains(" - │ let old"))
    );
    assert!(
        lines
            .iter()
            .any(|line| line.to_string().contains(" + │ let new"))
    );
    assert!(lines.iter().flat_map(|line| line.spans.iter()).any(
        |span| span.content.contains('+') && span.style.fg == Theme::default().diff_added().fg
    ));
    assert!(
        lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .any(|span| span.content.contains('-')
                && span.style.fg == Theme::default().diff_removed().fg)
    );
}

#[test]
fn tool_output_limits_toggle_and_stay_bounded_at_narrow_widths() {
    let rows = (1..=1030)
        .map(|number| (number, "let value = 1;"))
        .collect::<Vec<_>>();
    let state = read_tool_state("src/main.rs", ToolStatus::Completed, &read_detail(&rows));
    let call_id = read_tool_id(&state);
    let mut expanded = HashSet::from([BlockId::Tool(call_id)]);

    let collapsed = snapshot_lines(&transcript_layout(&state, Some(&expanded), 80).lines);
    assert!(collapsed.contains("… 968 more lines (click to expand)"));

    expanded.insert(tool_output_id(call_id, ToolOutputSection::Detail));
    let fully_expanded = transcript_layout(&state, Some(&expanded), 12);
    let rendered = snapshot_lines(&fully_expanded.lines);
    assert!(rendered.contains("… 8 more"));
    assert!(rendered.contains("maximum"));
    assert!(
        fully_expanded
            .lines
            .iter()
            .all(|line| { UnicodeWidthStr::width(line.to_string().as_str()) <= 12 })
    );

    expanded.remove(&tool_output_id(call_id, ToolOutputSection::Detail));
    let collapsed_again = snapshot_lines(&transcript_layout(&state, Some(&expanded), 80).lines);
    assert!(collapsed_again.contains("… 968 more lines (click to expand)"));
}

#[test]
fn byte_limit_counts_a_partially_rendered_line_as_omitted() {
    let long = "x".repeat(MAX_EXPANDED_BODY_BYTES + 10);
    let mut budget = RenderBudget::new(COLLAPSED_TOOL_OUTPUT_LIMITS);
    let mut renderer = SectionRenderer::new(&mut budget, false, 0);
    let rendered = renderer.take(&long).expect("partial line");
    assert_eq!(
        rendered.len(),
        MAX_EXPANDED_BODY_BYTES - OUTPUT_NOTICE_RESERVE_BYTES
    );
    assert_eq!(renderer.fully_rendered, 0);
}

#[test]
fn detail_stdout_and_stderr_share_one_aggregate_budget() {
    let text = (0..100).map(|_| "output").collect::<Vec<_>>().join("\n");
    let mut budget = RenderBudget::new(COLLAPSED_TOOL_OUTPUT_LIMITS);
    let detail = generic_output_lines(
        None,
        OutputText::complete(&text),
        ToolOutputSection::Detail,
        false,
        &mut budget,
        2,
        &Theme::default(),
    );
    let stdout = generic_output_lines(
        Some("STDOUT:"),
        OutputText::complete(&text),
        ToolOutputSection::Stdout,
        false,
        &mut budget,
        1,
        &Theme::default(),
    );
    let stderr = generic_output_lines(
        Some("STDERR:"),
        OutputText::complete(&text),
        ToolOutputSection::Stderr,
        false,
        &mut budget,
        0,
        &Theme::default(),
    );
    let lines = detail.len() + stdout.len() + stderr.len();
    let bytes = detail
        .iter()
        .chain(&stdout)
        .chain(&stderr)
        .map(|line| line.line.to_string().len())
        .sum::<usize>();
    assert!(lines <= COLLAPSED_TOOL_OUTPUT_LIMITS.lines, "{lines}");
    assert!(bytes <= COLLAPSED_TOOL_OUTPUT_LIMITS.bytes, "{bytes}");
    assert_eq!(
        detail
            .iter()
            .chain(&stdout)
            .chain(&stderr)
            .filter(|line| line.output_toggle.is_some())
            .count(),
        3
    );
}

#[test]
fn tool_code_views_snapshot() {
    let read_id = ToolCallId::new_v7();
    let edit_id = ToolCallId::new_v7();
    let mut state = assistant_state(vec![
        AssistantChild::Tool { call_id: read_id },
        AssistantChild::Tool { call_id: edit_id },
    ]);
    state.tools.insert(
        read_id,
        ToolCallState {
            id: read_id,
            owner: owner(1, "call-read"),
            presentation: presentation("read", Some("src/main.rs")),
            arguments: r#"{"filePath":"src/main.rs"}"#.into(),
            status: ToolStatus::Completed,
            detail: read_detail(&[(1, "fn main() {"), (1000, "    println!(\"hi\");")]),
            has_output_chunks: false,
        },
    );
    state.tools.insert(
        edit_id,
        ToolCallState {
            id: edit_id,
            owner: owner(1, "call-edit"),
            presentation: presentation("edit", Some("src/main.rs")),
            arguments: serde_json::json!({
                "filePath": "src/main.rs",
                "oldString": "let old = 1;\n",
                "newString": "let new = 2;"
            })
            .to_string(),
            status: ToolStatus::Completed,
            detail: "Edit applied atomically".into(),
            has_output_chunks: false,
        },
    );
    let expanded = HashSet::from([BlockId::Tool(read_id), BlockId::Tool(edit_id)]);
    insta::assert_snapshot!(snapshot_lines(
        &transcript_layout(&state, Some(&expanded), 60).lines
    ));
}

#[test]
fn changed_diff_rows_are_tinted_inside_the_band_padding() {
    let edit_id = ToolCallId::new_v7();
    let mut state = assistant_state(vec![AssistantChild::Tool { call_id: edit_id }]);
    state.tools.insert(
        edit_id,
        ToolCallState {
            id: edit_id,
            owner: owner(1, "call-edit"),
            presentation: presentation("edit", Some("src/main.rs")),
            arguments: serde_json::json!({
                "filePath": "src/main.rs",
                "oldString": "let old = 1;\n",
                "newString": "let new = 2;\n"
            })
            .to_string(),
            status: ToolStatus::Completed,
            detail: "Edit applied atomically".into(),
            has_output_chunks: false,
        },
    );
    let theme = Theme::default();
    let band = theme.terminal_background().unwrap();
    let expanded = HashSet::from([BlockId::Tool(edit_id)]);
    let layout = transcript_layout_with(
        &state,
        Some(&expanded),
        60,
        &theme,
        &crate::markdown::PlainHighlighter,
    );
    let row = |text: &str| {
        layout
            .lines
            .iter()
            .find(|line| line.to_string().contains(text))
            .unwrap_or_else(|| panic!("row {text}"))
    };
    for (text, tint) in [
        ("let old", theme.diff_removed_background().unwrap()),
        ("let new", theme.diff_added_background().unwrap()),
    ] {
        let line = row(text);
        let spans = &line.spans;
        // `│ ` gutter unbanded, then one grey padding column…
        assert_eq!(spans[0].style.bg, None, "{line:?}");
        assert_eq!(spans[1].content, " ", "{line:?}");
        assert_eq!(spans[1].style.bg, Some(band), "{line:?}");
        // …the line number, marker and code tinted out to the last column…
        for span in &spans[2..spans.len() - 1] {
            assert_eq!(span.style.bg, Some(tint), "{line:?}");
        }
        // …which stays grey.
        let last = spans.last().unwrap();
        assert_eq!(last.content, " ", "{line:?}");
        assert_eq!(last.style.bg, Some(band), "{line:?}");
        assert_eq!(line.width(), 60, "{line:?}");
    }
    // Hunk headers and metadata keep the plain band.
    for text in ["@@ -1 +1 @@", "Edit applied atomically"] {
        assert!(
            row(text)
                .spans
                .iter()
                .skip(1)
                .all(|span| span.style.bg == Some(band)),
            "{text}"
        );
    }
    // Without a pale step (sixteen colours, mono) rows keep the plain band.
    assert_eq!(
        Theme::new(ThemeKind::Default, ColorLevel::Ansi16).diff_added_background(),
        None
    );
    assert_eq!(
        Theme::new(ThemeKind::Mono, ColorLevel::None).diff_removed_background(),
        None
    );
}
