use crate::ui::transcript::*;

use cookie_agent_protocol::{
    DiffAsOf, DiffBlock, DiffChange, DiffHunk, DiffLine, DiffLineOp, DiffOmitted, DisplayBlock,
    ToolHint,
};

use super::support::*;

fn line(op: DiffLineOp, text: &str) -> DiffLine {
    DiffLine {
        op,
        text: text.into(),
    }
}

/// A change to `path` with one hunk: line 10 kept, line 11 replaced, and
/// line 12 kept, numbered as the file numbers them.
pub(super) fn sample_diff(path: &str) -> DiffBlock {
    DiffBlock {
        path: path.into(),
        old_path: None,
        language: None,
        change: DiffChange::Modified,
        hunks: vec![DiffHunk {
            old_start: 10,
            old_lines: 3,
            new_start: 10,
            new_lines: 3,
            lines: vec![
                line(DiffLineOp::Context, "fn keep() {}"),
                line(DiffLineOp::Remove, "let old = 1;"),
                line(DiffLineOp::Add, "let new = 2;"),
                line(DiffLineOp::Context, "fn after() {}"),
            ],
        }],
        added: 1,
        removed: 1,
        omitted: None,
        as_of: DiffAsOf::Execute,
        full: None,
    }
}

fn diff_state(title: &str, hint: &str, diff: DiffBlock) -> SessionState {
    display_tool_state(
        hinted_presentation(title, hint, None),
        ToolStatus::Completed,
        vec![DisplayBlock::Diff(diff)],
    )
}

fn expanded_rows(state: &SessionState, width: u16) -> Vec<String> {
    let call_id = read_tool_id(state);
    let expanded = HashSet::from([BlockId::Tool(call_id)]);
    transcript_layout_with(
        state,
        Some(&expanded),
        width,
        &Theme::default(),
        &crate::markdown::PlainHighlighter,
    )
    .lines
    .iter()
    .map(|line| line.to_string().trim_end().to_owned())
    .collect()
}

#[test]
fn diff_rows_number_each_line_from_its_hunk_and_mark_changes() {
    let rows = expanded_rows(
        &diff_state(
            "Edit src/main.rs",
            ToolHint::FILE_EDIT,
            sample_diff("src/main.rs"),
        ),
        80,
    );
    let text = rows.join("\n");
    assert!(text.contains("modified src/main.rs · +1 -1"), "{text}");
    assert!(text.contains("@@ -10,3 +10,3 @@"), "{text}");
    // Context rows carry the new file's number; a removed row the old
    // file's, an added row the new file's.
    for expected in [
        "10   │ fn keep() {}",
        "11 - │ let old = 1;",
        "11 + │ let new = 2;",
        "12   │ fn after() {}",
    ] {
        assert!(
            rows.iter().any(|row| row.contains(expected)),
            "{expected}\n{text}"
        );
    }
}

#[test]
fn diff_gutters_widen_to_the_largest_line_number() {
    let mut diff = sample_diff("src/main.rs");
    diff.hunks[0].old_start = 998;
    diff.hunks[0].new_start = 998;
    let rows = expanded_rows(&diff_state("Edit", ToolHint::FILE_EDIT, diff), 80);
    let text = rows.join("\n");
    for expected in [
        " 998   │ fn keep() {}",
        " 999 - │ let old = 1;",
        " 999 + │ let new = 2;",
        "1000   │ fn after() {}",
    ] {
        assert!(text.contains(expected), "{expected}\n{text}");
    }
}

#[test]
fn diff_headers_name_how_the_file_changed_and_hunkless_diffs_show_counts() {
    for (change, old_path, expected) in [
        (DiffChange::Created, None, "created data.bin · +120 -4"),
        (DiffChange::Deleted, None, "deleted data.bin · +120 -4"),
        (
            DiffChange::Renamed,
            Some("old.bin"),
            "renamed old.bin → data.bin · +120 -4",
        ),
        (DiffChange::Unknown, None, "modified data.bin · +120 -4"),
    ] {
        let diff = DiffBlock {
            old_path: old_path.map(str::to_owned),
            change,
            hunks: Vec::new(),
            added: 120,
            removed: 4,
            ..sample_diff("data.bin")
        };
        let rows = expanded_rows(&diff_state("Write", ToolHint::FILE_WRITE, diff), 80);
        let text = rows.join("\n");
        assert!(text.contains(expected), "{expected}\n{text}");
        assert!(!text.contains("@@"), "{text}");
    }
}

#[test]
fn a_prepare_diff_says_it_is_a_preview() {
    let diff = DiffBlock {
        as_of: DiffAsOf::Prepare,
        ..sample_diff("src/main.rs")
    };
    let text = expanded_rows(&diff_state("Edit", ToolHint::FILE_EDIT, diff), 80).join("\n");
    assert!(
        text.contains("modified src/main.rs · +1 -1 · preview"),
        "{text}"
    );
}

#[test]
fn omitted_hunks_are_counted_and_a_full_diff_is_offered() {
    let diff = DiffBlock {
        omitted: Some(DiffOmitted {
            hunks: 2,
            lines: 40,
        }),
        ..sample_diff("src/main.rs")
    };
    let text = expanded_rows(&diff_state("Edit", ToolHint::FILE_EDIT, diff.clone()), 80).join("\n");
    assert!(
        text.contains("… 2 more hunks (40 lines) not shown"),
        "{text}"
    );
    assert!(!text.contains("load the full output"), "{text}");

    let full = cookie_agent_protocol::ArtifactRef {
        sha256: "ab".repeat(32),
        byte_length: 2048,
        media_type: "text/plain".into(),
        filename: Some("main.rs.diff".into()),
    };
    let state = diff_state(
        "Edit",
        ToolHint::FILE_EDIT,
        DiffBlock {
            full: Some(full),
            ..diff
        },
    );
    let call_id = read_tool_id(&state);
    let expanded = HashSet::from([BlockId::Tool(call_id)]);
    let layout = transcript_layout(&state, Some(&expanded), 100);
    let text = snapshot_lines(&layout.lines);
    assert!(
        text.contains("… 2 more hunks (40 lines) not shown · click to load the full output (2 KB)"),
        "{text}"
    );
    assert!(
        layout
            .regions
            .iter()
            .any(|region| region.id == BlockId::ToolFull { call_id, block: 0 }),
        "{:?}",
        layout.regions
    );
}

#[test]
fn a_loaded_full_diff_replaces_the_cut_hunks_with_its_unified_text() {
    let mut state = diff_state(
        "Edit",
        ToolHint::FILE_EDIT,
        DiffBlock {
            omitted: Some(DiffOmitted { hunks: 1, lines: 2 }),
            full: Some(cookie_agent_protocol::ArtifactRef {
                sha256: "ab".repeat(32),
                byte_length: 64,
                media_type: "text/plain".into(),
                filename: None,
            }),
            ..sample_diff("src/main.rs")
        },
    );
    let call_id = read_tool_id(&state);
    state.tools.get_mut(&call_id).unwrap().full = vec![crate::state::FullContent {
        block: 0,
        state: crate::state::FullContentState::Loaded {
            text: "--- src/main.rs\n+++ src/main.rs\n@@ -40 +40 @@\n-gone\n+here\n".into(),
            truncated: false,
        },
    }];
    let text = expanded_rows(&state, 80).join("\n");
    assert!(text.contains("modified src/main.rs · +1 -1"), "{text}");
    assert!(text.contains("40 - │ gone"), "{text}");
    assert!(text.contains("40 + │ here"), "{text}");
    assert!(!text.contains("--- src/main.rs"), "{text}");
    assert!(!text.contains("let old"), "{text}");
    assert!(
        text.contains("▴ full output · click to show the cut display"),
        "{text}"
    );
}

#[test]
fn a_diff_renders_as_a_diff_whatever_the_tool_is_called() {
    // A tool named after no built-in, showing a diff, gets diff rows…
    let rows = expanded_rows(
        &diff_state("str_replace", ToolHint::GENERIC, sample_diff("src/main.rs")),
        80,
    );
    assert!(
        rows.iter().any(|row| row.contains("11 + │ let new = 2;")),
        "{rows:#?}"
    );
    // …and a plugin tool named `edit` whose arguments are not the built-in
    // edit's shows its own blocks, never a diff built from its arguments.
    let mut state = display_tool_state(
        presentation("edit", None),
        ToolStatus::Completed,
        vec![DisplayBlock::text("renamed 3 symbols")],
    );
    let call_id = read_tool_id(&state);
    state.tools.get_mut(&call_id).unwrap().arguments = serde_json::json!({
        "filePath": "src/main.rs",
        "oldString": "let old = 1;",
        "newString": "let new = 2;"
    })
    .to_string();
    let text = expanded_rows(&state, 80).join("\n");
    assert!(text.contains("renamed 3 symbols"), "{text}");
    assert!(!text.contains(" + │ "), "{text}");
    assert!(!text.contains(" - │ "), "{text}");
    // Its raw arguments are what an `info` view shows of them.
    assert!(text.contains("\"oldString\": \"let old = 1;\""), "{text}");
}

#[test]
fn tool_output_limits_toggle_and_stay_bounded_at_narrow_widths() {
    let lines = vec!["let value = 1;"; 1030];
    let state = excerpt_tool_state("src/main.rs", 1, &lines);
    let call_id = read_tool_id(&state);
    let mut expanded = HashSet::from([BlockId::Tool(call_id)]);

    let collapsed = snapshot_lines(&transcript_layout(&state, Some(&expanded), 80).lines);
    // The excerpt's header row and its first 62 lines fit the collapsed
    // budget; the other 968 lines are counted.
    assert!(
        collapsed.contains("… 968 more lines (click to expand)"),
        "{collapsed}"
    );

    expanded.insert(tool_output_id(call_id, ToolOutputSection::Detail));
    let fully_expanded = transcript_layout(&state, Some(&expanded), 12);
    let rendered = snapshot_lines(&fully_expanded.lines);
    assert!(rendered.contains("… 8 more"), "{rendered}");
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
    let mut sink = LineSink::new(&mut budget, COLLAPSED_TOOL_OUTPUT_LIMITS);
    let rendered = sink.line(&long).expect("partial line");
    assert_eq!(
        rendered.len(),
        MAX_EXPANDED_BODY_BYTES - OUTPUT_NOTICE_RESERVE_BYTES
    );
    assert_eq!(sink.omitted(), 1);
}
