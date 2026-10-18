use serde_json::json;

use super::*;

fn artifact() -> ArtifactRef {
    ArtifactRef {
        sha256: "0f".repeat(32),
        byte_length: 482_113,
        media_type: "image/png".into(),
        filename: Some("screenshot.png".into()),
    }
}

fn line(op: DiffLineOp, text: &str) -> DiffLine {
    DiffLine {
        op,
        text: text.into(),
    }
}

fn diff() -> DiffBlock {
    DiffBlock {
        path: "src/main.rs".into(),
        old_path: None,
        language: Some("rust".into()),
        change: DiffChange::Modified,
        hunks: vec![
            DiffHunk {
                old_start: 10,
                old_lines: 3,
                new_start: 10,
                new_lines: 3,
                lines: vec![
                    line(DiffLineOp::Context, "fn main() {"),
                    line(DiffLineOp::Remove, "    old();"),
                    line(DiffLineOp::Add, "    new();"),
                    line(DiffLineOp::Context, "}"),
                ],
            },
            DiffHunk {
                old_start: 40,
                old_lines: 0,
                new_start: 41,
                new_lines: 1,
                lines: vec![line(DiffLineOp::Add, "// end")],
            },
        ],
        added: 2,
        removed: 1,
        omitted: None,
        as_of: DiffAsOf::Execute,
        full: None,
    }
}

fn stream(stream: TerminalStream, text: &str) -> TerminalSegment {
    TerminalSegment {
        stream,
        text: text.into(),
    }
}

#[test]
fn every_kind_has_a_since_entry_matching_the_schema() {
    let tags = crate::schema_tags::<DisplayBlock>("type");
    let since = DISPLAY_BLOCK_SINCE
        .iter()
        .map(|(kind, _)| (*kind).to_owned())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(tags, since);
    assert_eq!(DisplayBlock::since("diff"), Some(FIRST_STABLE_MINOR));
    assert_eq!(DisplayBlock::since("chart"), None);
}

#[test]
fn blocks_round_trip_and_unknown_kinds_stay_raw() {
    let blocks = json!([
        {"type": "text", "text": "hello"},
        {"type": "diff", "path": "a", "change": "created", "hunks": [], "added": 3,
         "removed": 0, "as_of": "prepare"},
        {"type": "terminal", "segments": [{"stream": "stderr", "text": "x"}], "exit_code": 1},
        {"type": "chart", "series": [1, 2]}
    ]);
    let decoded: Vec<Open<DisplayBlock>> = serde_json::from_value(blocks.clone()).unwrap();
    assert!(decoded[3].is_unknown());
    assert_eq!(decoded[1].known().unwrap().kind(), "diff");
    assert_eq!(serde_json::to_value(&decoded).unwrap(), blocks);
    let schema = schemars::schema_for!(Open<DisplayBlock>);
    assert_eq!(schema.get(crate::OPEN_SCHEMA_KEY), Some(&json!(true)));
}

#[test]
fn open_fields_read_unknown_values_as_their_fallback() {
    let block: DisplayBlock = serde_json::from_value(json!({
        "type": "diff", "path": "a", "change": "copied", "hunks": [], "added": 0,
        "removed": 0, "as_of": "execute"
    }))
    .unwrap();
    let DisplayBlock::Diff(diff) = &block else {
        panic!("{block:?}");
    };
    assert_eq!(diff.change, DiffChange::Unknown);
    assert_eq!(block.to_text(), "a: modified (+0 -0)");
    let segment: TerminalSegment =
        serde_json::from_value(json!({"stream": "pty", "text": "x"})).unwrap();
    assert_eq!(segment.stream, TerminalStream::Unknown);
}

#[test]
fn text_kinds_render_their_text() {
    assert_eq!(DisplayBlock::text("plain\ttext").to_text(), "plain\ttext");
    let markdown = DisplayBlock::Markdown {
        text: "# Title\n\n*body*".into(),
        omitted_lines: Some(2),
    };
    assert_eq!(markdown.to_text(), "# Title\n\n*body*\n… 2 more lines");
    let code = DisplayBlock::Code {
        text: "ls -la\n".into(),
        language: Some("sh".into()),
        start_line: None,
        omitted_lines: Some(1),
    };
    assert_eq!(code.to_text(), "ls -la\n… 1 more line");
}

#[test]
fn file_excerpts_number_their_lines() {
    let block = DisplayBlock::FileExcerpt {
        path: "src/lib.rs".into(),
        language: None,
        start_line: 98,
        lines: vec!["a".into(), "b".into(), "".into()],
        total_lines: Some(400),
        omitted_lines: Some(5),
        full: None,
    };
    assert_eq!(
        block.to_text(),
        "src/lib.rs\n 98  a\n 99  b\n100  \n… 5 more lines"
    );
}

#[test]
fn diffs_render_as_unified_diffs_with_file_line_numbers() {
    assert_eq!(
        DisplayBlock::Diff(diff()).to_text(),
        "--- src/main.rs\n+++ src/main.rs\n@@ -10,3 +10,3 @@\n fn main() {\n-    old();\n+    new();\n }\n@@ -40,0 +41 @@\n+// end"
    );
    let mut created = diff();
    created.change = DiffChange::Created;
    created.hunks.truncate(1);
    created.omitted = Some(DiffOmitted { hunks: 3, lines: 1 });
    let text = DisplayBlock::Diff(created).to_text();
    assert!(
        text.starts_with("--- /dev/null\n+++ src/main.rs\n"),
        "{text}"
    );
    assert!(text.ends_with("\n… 3 more hunks (1 line)"), "{text}");
    let mut renamed = diff();
    renamed.change = DiffChange::Renamed;
    renamed.old_path = Some("src/old.rs".into());
    renamed.hunks.clear();
    assert_eq!(
        DisplayBlock::Diff(renamed.clone()).to_text(),
        "src/main.rs: renamed (+2 -1)"
    );
    renamed.change = DiffChange::Deleted;
    renamed.hunks = diff().hunks;
    assert!(
        DisplayBlock::Diff(renamed)
            .to_text()
            .starts_with("--- src/old.rs\n+++ /dev/null\n")
    );
}

#[test]
fn terminals_render_without_colour_and_with_their_exit_code() {
    let block = DisplayBlock::Terminal {
        segments: vec![
            stream(TerminalStream::Stdout, "\u{1b}[1;32mok\u{1b}[0m\n"),
            stream(TerminalStream::Stderr, "warning"),
        ],
        exit_code: Some(2),
        omitted_lines: None,
        full: None,
    };
    assert_eq!(block.to_text(), "ok\nwarning\n[exit code 2]");
}

#[test]
fn key_values_tables_and_atoms_render_as_text() {
    let entries = DisplayBlock::KeyValue {
        entries: vec![
            KeyValueEntry {
                key: "status".into(),
                value: "200".into(),
            },
            KeyValueEntry {
                key: "type".into(),
                value: "text/html".into(),
            },
        ],
        omitted_entries: Some(3),
    };
    assert_eq!(
        entries.to_text(),
        "status: 200\ntype: text/html\n… 3 more entries"
    );

    let table = DisplayBlock::Table {
        columns: vec!["name".into(), "size".into()],
        rows: vec![
            vec!["Cargo.toml".into(), "1 KB".into()],
            vec!["é".into(), "12 KB".into()],
        ],
        omitted_rows: Some(1),
    };
    assert_eq!(
        table.to_text(),
        "name        size\n----------  -----\nCargo.toml  1 KB\né           12 KB\n… 1 more row"
    );

    let json_block = DisplayBlock::Json {
        value: json!({"a": [1]}),
    };
    assert_eq!(json_block.to_text(), "{\n  \"a\": [\n    1\n  ]\n}");

    let media = DisplayBlock::Media {
        artifact: artifact(),
        alt: Some("the login form".into()),
    };
    assert_eq!(
        media.to_text(),
        "[image: screenshot.png, 471 KB] the login form"
    );

    let link = DisplayBlock::Link {
        url: "https://example.com".into(),
        label: Some("Example".into()),
    };
    assert_eq!(link.to_text(), "Example <https://example.com>");
    let bare = DisplayBlock::Link {
        url: "https://example.com".into(),
        label: None,
    };
    assert_eq!(bare.to_text(), "https://example.com");

    let session_id = SessionId::new_v7();
    let session = DisplayBlock::SessionRef {
        session_id,
        short_id: Some("k3x".into()),
        label: Some("reviewer".into()),
    };
    assert_eq!(session.to_text(), "session k3x: reviewer");
    let unnamed = DisplayBlock::SessionRef {
        session_id,
        short_id: None,
        label: None,
    };
    assert_eq!(unnamed.to_text(), format!("session {session_id}"));
}

#[test]
fn newer_kinds_downgrade_to_their_text() {
    let block = DisplayBlock::Diff(diff());
    assert_eq!(block.clone().downgrade(FIRST_STABLE_MINOR), block);
    assert_eq!(block.clone().downgrade_from(Some(3), 3), block);
    assert_eq!(
        block.clone().downgrade_from(Some(3), 2),
        DisplayBlock::text(block.to_text())
    );
    let blocks = vec![
        Open::Known(DisplayBlock::text("a")),
        serde_json::from_value(json!({"type": "chart"})).unwrap(),
        Open::Known(DisplayBlock::text("b")),
    ];
    assert_eq!(display_text(&blocks), "a\nb");
}

#[test]
fn sanitizing_strips_escape_sequences_and_keeps_sgr_only_in_terminals() {
    let hostile = "a\u{1b}[31mred\u{1b}[0m\u{1b}]0;title\u{7}\u{1b}[2J\r\u{8}b\tc\n\u{1b}Pdcs\u{1b}\\d\u{7f}\u{85}e";
    assert_eq!(sanitize_display_text(hostile), "aredb\tc\nde");
    assert_eq!(
        sanitize_terminal_text(hostile),
        "a\u{1b}[31mred\u{1b}[0mb\tc\nde"
    );
    assert_eq!(strip_sgr("\u{1b}[1;38;5;208mx\u{1b}[m"), "x");
    assert!(matches!(
        sanitize_display_text("clean\ttext\n"),
        Cow::Borrowed(_)
    ));
    assert_eq!(sanitize_display_text("cut\u{1b}[31"), "cut");
    assert_eq!(sanitize_display_text("lone\u{1b}"), "lone");
    assert_eq!(sanitize_display_text("\u{1b}[31é"), "é");
    assert!(is_terminal_safe("\u{1b}[1;31mx\u{1b}[0m"));
    assert!(!is_terminal_safe("\u{1b}[2Jx"));
    assert!(!is_display_safe("\u{1b}[1mx"));

    let mut block = DisplayBlock::Terminal {
        segments: vec![stream(TerminalStream::Stdout, hostile)],
        exit_code: None,
        omitted_lines: None,
        full: None,
    };
    assert!(block.validate().is_err());
    block.sanitize();
    assert_eq!(block.validate(), Ok(()));
    let mut excerpt = DisplayBlock::FileExcerpt {
        path: "p\u{1b}[1m".into(),
        language: None,
        start_line: 1,
        lines: vec!["x\u{0}y".into(), "two\nlines".into()],
        total_lines: None,
        omitted_lines: None,
        full: None,
    };
    excerpt.sanitize();
    assert_eq!(excerpt.validate(), Ok(()));
    let DisplayBlock::FileExcerpt { path, lines, .. } = &excerpt else {
        unreachable!()
    };
    assert_eq!(
        (path.as_str(), lines.as_slice()),
        ("p", &["xy".to_owned(), "two lines".to_owned()][..])
    );
}

#[test]
fn validation_checks_structure() {
    assert_eq!(DisplayBlock::Diff(diff()).validate(), Ok(()));
    let mut miscounted = diff();
    miscounted.hunks[0].old_lines = 4;
    assert_eq!(
        DisplayBlock::Diff(miscounted).validate(),
        Err(DisplayBlockError::HunkCounts)
    );
    let ragged = DisplayBlock::Table {
        columns: vec!["a".into(), "b".into()],
        rows: vec![vec!["1".into()]],
        omitted_rows: None,
    };
    assert_eq!(ragged.validate(), Err(DisplayBlockError::TableShape));
    let zero = DisplayBlock::Code {
        text: String::new(),
        language: None,
        start_line: Some(0),
        omitted_lines: None,
    };
    assert_eq!(
        zero.validate(),
        Err(DisplayBlockError::LineNumber("start_line"))
    );
    let mut bad_artifact = artifact();
    bad_artifact.sha256 = "x".into();
    let media = DisplayBlock::Media {
        artifact: bad_artifact,
        alt: None,
    };
    assert!(matches!(
        media.validate(),
        Err(DisplayBlockError::Artifact(_))
    ));
    assert_eq!(
        DisplayBlock::text("bell\u{7}").validate(),
        Err(DisplayBlockError::ControlCharacter("text"))
    );
}

#[test]
fn blocks_within_budget_are_unchanged() {
    let blocks = vec![DisplayBlock::text("a\nb\n"), DisplayBlock::Diff(diff())];
    let budget = serialized_len(&blocks);
    assert_eq!(fit_display_budget(blocks.clone(), budget), blocks);
}

#[test]
fn the_budget_cuts_whole_units_and_counts_what_it_left_out() {
    let lines = (0..100)
        .map(|index| format!("line {index}\n"))
        .collect::<String>();
    let text = DisplayBlock::text(lines);
    let tail = DisplayBlock::KeyValue {
        entries: vec![KeyValueEntry {
            key: "exit".into(),
            value: "0".into(),
        }],
        omitted_entries: None,
    };
    let budget = 400;
    let fitted = fit_display_budget(vec![text, tail.clone()], budget);
    assert!(
        serialized_len(&fitted) <= budget,
        "{}",
        serialized_len(&fitted)
    );
    let DisplayBlock::Text {
        text,
        omitted_lines,
    } = &fitted[0]
    else {
        panic!("{fitted:?}");
    };
    assert!(
        text.ends_with('\n') && text.starts_with("line 0\n"),
        "{text:?}"
    );
    let kept = text.lines().count() as u64;
    assert!(kept > 10, "{kept}");
    assert_eq!(*omitted_lines, Some(100 - kept));
    // What remains after the cut still holds the small block behind it.
    assert_eq!(fitted.get(1), Some(&tail));
}

#[test]
fn the_budget_cuts_diffs_by_hunk_tables_by_row_and_terminals_by_line() {
    let mut big = diff();
    big.hunks = (0..50).map(|_| diff().hunks[0].clone()).collect();
    let fitted = fit_display_budget(vec![DisplayBlock::Diff(big)], 1500);
    let [DisplayBlock::Diff(cut)] = fitted.as_slice() else {
        panic!("{fitted:?}");
    };
    let kept = cut.hunks.len() as u64;
    assert!((1..50).contains(&kept), "{kept}");
    assert_eq!(
        cut.omitted,
        Some(DiffOmitted {
            hunks: 50 - kept,
            lines: (50 - kept) * 4
        })
    );
    assert!(serialized_len(&fitted) <= 1500);

    let table = DisplayBlock::Table {
        columns: vec!["n".into()],
        rows: (0..200).map(|index| vec![index.to_string()]).collect(),
        omitted_rows: Some(7),
    };
    let fitted = fit_display_budget(vec![table], 300);
    let [
        DisplayBlock::Table {
            rows, omitted_rows, ..
        },
    ] = fitted.as_slice()
    else {
        panic!("{fitted:?}");
    };
    assert_eq!(*omitted_rows, Some(7 + 200 - rows.len() as u64));
    assert_eq!(rows[0], ["0"]);

    let terminal = DisplayBlock::Terminal {
        segments: vec![
            stream(TerminalStream::Stdout, &"out\n".repeat(30)),
            stream(TerminalStream::Stderr, &"err\n".repeat(30)),
        ],
        exit_code: Some(1),
        omitted_lines: None,
        full: Some(artifact()),
    };
    let fitted = fit_display_budget(vec![terminal], 400);
    let [
        DisplayBlock::Terminal {
            segments,
            omitted_lines,
            exit_code,
            full,
        },
    ] = fitted.as_slice()
    else {
        panic!("{fitted:?}");
    };
    let kept = segments
        .iter()
        .map(|segment| segment.text.lines().count() as u64)
        .sum::<u64>();
    assert_eq!(*omitted_lines, Some(60 - kept));
    assert!(segments.iter().all(|segment| segment.text.ends_with('\n')));
    assert_eq!((*exit_code, full.as_ref()), (Some(1), Some(&artifact())));
}

#[test]
fn blocks_that_cannot_be_cut_are_dropped_when_they_do_not_fit() {
    let json_block = DisplayBlock::Json {
        value: json!({"blob": "x".repeat(500)}),
    };
    let small = DisplayBlock::text("ok");
    let fitted = fit_display_budget(vec![json_block, small.clone()], 100);
    assert_eq!(fitted, vec![small]);
    assert!(fit_display_budget(vec![DisplayBlock::text("x".repeat(50))], 2).is_empty());
}

#[test]
fn deltas_append_to_text_and_terminal_blocks_or_replace_the_list() {
    let mut live = vec![
        Open::Known(DisplayBlock::text("a")),
        Open::Known(DisplayBlock::Terminal {
            segments: vec![stream(TerminalStream::Stdout, "1")],
            exit_code: None,
            omitted_lines: None,
            full: None,
        }),
        Open::Known(DisplayBlock::Json { value: json!(1) }),
    ];
    let append = |index, text: &str, stream| DisplayDelta::Append {
        index,
        text: text.into(),
        stream,
    };
    append(0, "b", None).apply(&mut live).unwrap();
    append(1, "2", None).apply(&mut live).unwrap();
    append(1, "e", Some(TerminalStream::Stderr))
        .apply(&mut live)
        .unwrap();
    append(1, "3", Some(TerminalStream::Stdout))
        .apply(&mut live)
        .unwrap();
    assert_eq!(live[0].known(), Some(&DisplayBlock::text("ab")));
    let Some(DisplayBlock::Terminal { segments, .. }) = live[1].known() else {
        panic!("{live:?}");
    };
    assert_eq!(
        segments,
        &[
            stream(TerminalStream::Stdout, "12"),
            stream(TerminalStream::Stderr, "e"),
            stream(TerminalStream::Stdout, "3"),
        ]
    );
    assert_eq!(
        append(2, "x", None).apply(&mut live),
        Err(DisplayDeltaError::NotAppendable(2))
    );
    assert_eq!(
        append(9, "x", None).apply(&mut live),
        Err(DisplayDeltaError::NoSuchBlock(9))
    );
    assert_eq!(
        append(0, "x", Some(TerminalStream::Stdout)).apply(&mut live),
        Err(DisplayDeltaError::StreamOnText)
    );
    let replace: DisplayDelta = serde_json::from_value(
        json!({"type": "replace", "blocks": [{"type": "text", "text": "z"}]}),
    )
    .unwrap();
    replace.apply(&mut live).unwrap();
    assert_eq!(live, vec![Open::Known(DisplayBlock::text("z"))]);
    let wire = serde_json::to_value(append(1, "x", None)).unwrap();
    assert_eq!(wire, json!({"type": "append", "index": 1, "text": "x"}));
}

#[test]
fn an_append_just_past_the_list_creates_its_block() {
    let mut live = Vec::new();
    DisplayDelta::Append {
        index: 0,
        text: "line 1\n".into(),
        stream: None,
    }
    .apply(&mut live)
    .unwrap();
    DisplayDelta::Append {
        index: 1,
        text: "err".into(),
        stream: Some(TerminalStream::Stderr),
    }
    .apply(&mut live)
    .unwrap();
    assert_eq!(live[0].known(), Some(&DisplayBlock::text("line 1\n")));
    assert!(matches!(
        live[1].known(),
        Some(DisplayBlock::Terminal { segments, .. })
            if segments == &[stream(TerminalStream::Stderr, "err")]
    ));
    assert_eq!(
        DisplayDelta::Append {
            index: 3,
            text: "x".into(),
            stream: None,
        }
        .apply(&mut live),
        Err(DisplayDeltaError::NoSuchBlock(3))
    );
}

#[test]
fn a_live_display_keeps_its_deltas_within_one_budget() {
    let append = |text: String| DisplayDelta::Append {
        index: 0,
        text,
        stream: None,
    };
    let mut live = LiveDisplay::default();
    let chunk = "é".repeat(DisplayDelta::MAX_APPEND_BYTES / 2);
    let mut admitted = 0;
    while let Some(delta) = live.admit(append(chunk.clone())).unwrap() {
        let DisplayDelta::Append { text, .. } = delta else {
            panic!("an append stays an append");
        };
        admitted += text.len();
    }
    // The last append was cut on a character boundary to the budget.
    assert_eq!(admitted, DISPLAY_BUDGET_BYTES);
    assert_eq!(display_text(live.blocks()).len(), DISPLAY_BUDGET_BYTES);
    assert_eq!(live.admit(append("more".into())), Ok(None));

    let mut live = LiveDisplay::default();
    live.admit(append("a".into())).unwrap();
    assert_eq!(
        live.admit(DisplayDelta::Append {
            index: 4,
            text: "x".into(),
            stream: None,
        }),
        Err(DisplayDeltaError::NoSuchBlock(4))
    );
    let replaced = live
        .admit(DisplayDelta::Replace {
            blocks: vec![Open::Known(DisplayBlock::text(
                "line\n".repeat(DISPLAY_BUDGET_BYTES),
            ))],
        })
        .unwrap()
        .expect("a cut replacement still fits");
    let DisplayDelta::Replace { blocks } = replaced else {
        panic!("a replacement stays a replacement");
    };
    assert!(serde_json::to_vec(&blocks).unwrap().len() < DISPLAY_BUDGET_BYTES);
    assert_eq!(live.blocks(), blocks.as_slice());
    assert!(matches!(
        live.blocks()[0].known(),
        Some(DisplayBlock::Text {
            omitted_lines: Some(_),
            ..
        })
    ));
}

#[test]
fn result_displays_are_sanitized_fit_and_validated() {
    assert!(text_display("").is_empty());
    assert_eq!(
        text_display("ok"),
        vec![Open::Known(DisplayBlock::text("ok"))]
    );
    let unknown: Open<DisplayBlock> =
        serde_json::from_value(json!({"type": "hologram", "frames": 3})).unwrap();
    let fitted = fit_result_display(vec![
        Open::Known(DisplayBlock::text("a\u{1b}[31mb")),
        unknown.clone(),
        Open::Known(DisplayBlock::text("line\n".repeat(DISPLAY_BUDGET_BYTES))),
    ]);
    assert_eq!(fitted[0].known(), Some(&DisplayBlock::text("ab")));
    assert_eq!(fitted.len(), 2);
    assert_eq!(validate_display(&fitted), Ok(()));
    assert_eq!(validate_display(&[unknown]), Ok(()));
    assert_eq!(
        validate_display(&[Open::Known(DisplayBlock::text("a\u{7}"))]),
        Err(DisplayBlockError::ControlCharacter("text"))
    );
    assert_eq!(
        validate_display(&[Open::Known(DisplayBlock::text(
            "x".repeat(DISPLAY_BUDGET_BYTES)
        ))]),
        Err(DisplayBlockError::TooLarge)
    );
}

#[test]
fn presentations_carry_the_tool_and_open_sources() {
    let prepared: PreparedPresentation = serde_json::from_value(json!({
        "title": "Edit src/main.rs",
        "hint": "file_edit",
        "primary_argument": "src/main.rs",
        "preview": [{"type": "text", "text": "x"}]
    }))
    .unwrap();
    assert_eq!(prepared.arguments, None);
    let source: Open<ToolSource> = serde_json::from_value(json!({
        "type": "runtime", "runtime_id": "labpc", "attachment": "labpc_repo", "capability": "coding"
    }))
    .unwrap();
    let presentation = ToolPresentation::new(
        prepared,
        ToolIdentity {
            name: "edit".into(),
            published_name: "labpc_edit".into(),
            source,
        },
    );
    let wire = serde_json::to_value(&presentation).unwrap();
    assert_eq!(wire["tool"]["source"]["type"], "runtime");
    assert_eq!(wire["hint"], "file_edit");
    assert_eq!(
        serde_json::from_value::<ToolPresentation>(wire).unwrap(),
        presentation
    );
    let future: Open<ToolSource> =
        serde_json::from_value(json!({"type": "remote_agent", "id": 1})).unwrap();
    assert!(future.is_unknown());
    let mcp: Open<ToolSource> =
        serde_json::from_value(json!({"type": "mcp", "server": "docs"})).unwrap();
    assert_eq!(
        mcp.known(),
        Some(&ToolSource::Mcp {
            server: "docs".into(),
            runtime_id: None
        })
    );
}

#[test]
fn hints_are_open_labels_that_render_unknown_as_generic() {
    for documented in ToolHint::DOCUMENTED {
        assert_eq!(ToolHint::new(*documented).unwrap().rendered(), *documented);
    }
    assert_eq!(
        ToolHint::new("database").unwrap().rendered(),
        ToolHint::GENERIC
    );
    for invalid in ["", "File", "1shell", "a-b", &"a".repeat(33)] {
        assert!(ToolHint::new(invalid).is_err(), "{invalid:?}");
    }
}
