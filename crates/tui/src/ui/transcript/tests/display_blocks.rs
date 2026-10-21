//! Each display block kind as tool body rows (spec 12.6).

use crate::ui::transcript::*;

use cookie_agent_protocol::{
    ArtifactRef, DisplayBlock, Open, TerminalSegment, TerminalStream, ToolHint,
};
use ratatui::style::Color;
use ratatui::text::Line;

use crate::markdown::PlainHighlighter;

use super::support::*;

fn expanded_layout(state: &SessionState, width: u16) -> TranscriptLayout {
    let call_id = read_tool_id(state);
    let expanded = HashSet::from([BlockId::Tool(call_id)]);
    transcript_layout_with(
        state,
        Some(&expanded),
        width,
        &Theme::default(),
        &PlainHighlighter,
    )
}

fn expanded_text(state: &SessionState, width: u16) -> String {
    snapshot_lines(&expanded_layout(state, width).lines)
}

fn shown(display: Vec<DisplayBlock>) -> SessionState {
    display_tool_state(
        hinted_presentation("Probe", ToolHint::GENERIC, None),
        ToolStatus::Completed,
        display,
    )
}

fn artifact(name: &str, media_type: &str, byte_length: u64) -> ArtifactRef {
    ArtifactRef {
        sha256: "cd".repeat(32),
        byte_length,
        media_type: media_type.into(),
        filename: Some(name.into()),
    }
}

fn terminal(segments: &[(TerminalStream, &str)], exit_code: Option<i32>) -> DisplayBlock {
    DisplayBlock::Terminal {
        segments: segments
            .iter()
            .map(|(stream, text)| TerminalSegment {
                stream: *stream,
                text: (*text).to_owned(),
            })
            .collect(),
        exit_code,
        omitted_lines: None,
        full: None,
    }
}

/// The spans of the first rendered row containing `text`.
fn row_spans(lines: &[Line<'static>], text: &str) -> Vec<(String, ratatui::style::Style)> {
    lines
        .iter()
        .find(|line| line.to_string().contains(text))
        .unwrap_or_else(|| panic!("row {text}"))
        .spans
        .iter()
        .map(|span| (span.content.to_string(), span.style))
        .collect()
}

#[test]
fn excerpts_are_syntax_highlighted_behind_their_line_numbers() {
    let state = excerpt_tool_state("src/main.rs", 1, &["fn main() {", "    let x = 1;", "}"]);
    let lines = expanded_read_layout(&state, &Theme::default());
    let rendered = lines.iter().map(ToString::to_string).collect::<Vec<_>>();
    assert!(rendered.iter().any(|line| line.contains("1 │ fn main() {")));
    // Highlighting produces more than one distinct foreground color.
    let colors = lines
        .iter()
        .flat_map(|line| line.spans.iter())
        .filter_map(|span| span.style.fg)
        .collect::<HashSet<_>>();
    assert!(colors.len() > 1);
}

#[test]
fn excerpt_gutters_align_file_line_numbers_and_the_header_names_the_range() {
    let mut state = excerpt_tool_state("src/main.rs", 999, &["nine", "thousand"]);
    let call_id = read_tool_id(&state);
    if let Some(Open::Known(DisplayBlock::FileExcerpt { total_lines, .. })) =
        state.tools.get_mut(&call_id).unwrap().display.first_mut()
    {
        *total_lines = Some(1200);
    }
    let text = expanded_text(&state, 80);
    assert!(
        text.contains("src/main.rs · lines 999–1000 of 1200"),
        "{text}"
    );
    assert!(text.contains("│   999 │ nine"), "{text}");
    assert!(text.contains("│  1000 │ thousand"), "{text}");
}

#[test]
fn absent_and_unknown_languages_fall_back_to_plain_code() {
    for path in ["README", "src/value.unknown-language"] {
        let state = excerpt_tool_state(path, 1, &["plain value"]);
        let rendered = snapshot_lines(&expanded_read_layout(&state, &Theme::default()));
        assert!(rendered.contains("1 │ plain value"), "{path}: {rendered}");
    }
    assert_eq!(path_extension("src/main.rs"), Some("rs"));
    assert_eq!(path_extension("README"), None);
}

#[test]
fn failures_and_text_read_as_ordinary_text() {
    let mut state = read_tool_state("src/main.rs", ToolStatus::Failed, "");
    let call_id = read_tool_id(&state);
    state.tools.get_mut(&call_id).unwrap().failure = Some("permission denied".into());
    let lines = expanded_read_layout(&state, &Theme::default());
    let failure_colors = lines
        .iter()
        .flat_map(|line| line.spans.iter())
        .filter(|span| span.content.contains("permission denied"))
        .filter_map(|span| span.style.fg)
        .collect::<HashSet<_>>();
    // Expanded, a failure reads as ordinary text; its header suffix names
    // the failure.
    assert!(failure_colors.is_empty(), "{failure_colors:?}");
    let text = read_tool_state("src/main.rs", ToolStatus::Completed, "fn main() {}");
    let lines = expanded_read_layout(&text, &Theme::default());
    let content_colors = lines
        .iter()
        .flat_map(|line| line.spans.iter())
        .filter(|span| span.content.contains("fn main"))
        .filter_map(|span| span.style.fg)
        .collect::<HashSet<_>>();
    assert!(content_colors.is_empty(), "{content_colors:?}");
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
fn terminal_rows_draw_sgr_colours_and_stderr_in_the_warning_tone() {
    let state = shown(vec![terminal(
        &[
            (
                TerminalStream::Stdout,
                "\u{1b}[31mred\u{1b}[0m plain\n\u{1b}[1;32mstill",
            ),
            (TerminalStream::Stderr, " warned\n"),
            (TerminalStream::Stdout, "green\u{1b}[0m done\n"),
        ],
        Some(1),
    )]);
    let theme = Theme::default();
    let lines = expanded_layout(&state, 80).lines;
    let spans = row_spans(&lines, "red plain");
    let red = spans
        .iter()
        .find(|(text, _)| text == "red")
        .expect("red run");
    assert_eq!(red.1.fg, Some(Color::Red));
    let plain = spans
        .iter()
        .find(|(text, _)| text == " plain")
        .expect("plain run");
    assert_ne!(plain.1.fg, Some(Color::Red));
    // A line another stream continues keeps each stream's own rendition.
    let spans = row_spans(&lines, "still warned");
    let still = spans.iter().find(|(text, _)| text == "still").unwrap();
    assert_eq!(still.1.fg, Some(Color::Green));
    assert!(
        still
            .1
            .add_modifier
            .contains(ratatui::style::Modifier::BOLD)
    );
    let warned = spans.iter().find(|(text, _)| text == " warned").unwrap();
    assert_eq!(warned.1.fg, theme.warning().fg);
    // The stdout rendition carries over the line break.
    let spans = row_spans(&lines, "green done");
    let green = spans.iter().find(|(text, _)| text == "green").unwrap();
    assert_eq!(green.1.fg, Some(Color::Green));
    let text = snapshot_lines(&lines);
    assert!(!text.contains('\u{1b}'), "{text}");
    assert!(!text.contains("[31m"), "{text}");
    let exit = row_spans(&lines, "exit code 1");
    let exit = exit.iter().find(|(text, _)| text == "exit code 1").unwrap();
    assert_eq!(exit.1.fg, theme.tool_failure().fg);
}

#[test]
fn a_cut_terminal_block_counts_its_omitted_lines_and_offers_its_full_output() {
    let mut block = terminal(&[(TerminalStream::Stdout, "head\n")], Some(0));
    if let DisplayBlock::Terminal {
        omitted_lines,
        full,
        ..
    } = &mut block
    {
        *omitted_lines = Some(5);
        *full = Some(artifact("stdout", "text/plain", 3 * 1024 * 1024));
    }
    let state = shown(vec![block]);
    let call_id = read_tool_id(&state);
    let layout = expanded_layout(&state, 120);
    let text = snapshot_lines(&layout.lines);
    assert!(
        text.contains("… 5 more lines not shown · click to load the full output (3.0 MB)"),
        "{text}"
    );
    assert!(text.contains("exit code 0"), "{text}");
    assert!(
        layout
            .regions
            .iter()
            .any(|region| region.id == BlockId::ToolFull { call_id, block: 0 })
    );
}

#[test]
fn structured_blocks_render_by_kind() {
    let session = cookie_agent_protocol::SessionId::new_v7();
    let state = shown(vec![
        DisplayBlock::key_value([
            ("status", "200 OK"),
            ("type", "text/html"),
            ("prompt", "first line\nsecond line"),
        ]),
        DisplayBlock::Table {
            columns: vec!["name".into(), "type".into()],
            rows: vec![
                vec!["src".into(), "directory".into()],
                vec!["Cargo.toml".into(), "file".into()],
            ],
            omitted_rows: Some(3),
        },
        DisplayBlock::Json {
            value: serde_json::json!({"ok": true, "items": [1, 2]}),
        },
        DisplayBlock::Media {
            artifact: artifact("chart.png", "image/png", 482_113),
            alt: Some("a chart".into()),
        },
        DisplayBlock::Link {
            url: "https://example.com/docs".into(),
            label: Some("Docs".into()),
        },
        DisplayBlock::SessionRef {
            session_id: session,
            short_id: Some("abc123".into()),
            label: Some("reviewer".into()),
        },
        DisplayBlock::code("cargo test\n", Some("sh".into())),
        DisplayBlock::markdown("# Result\n\n- one\n- two"),
        DisplayBlock::text("done"),
    ]);
    let call_id = read_tool_id(&state);
    let layout = expanded_layout(&state, 72);
    let text = snapshot_lines(&layout.lines);
    for expected in [
        "status  200 OK",
        "type    text/html",
        "prompt  first line",
        "        second line",
        "name        type",
        "src         directory",
        "Cargo.toml  file",
        "… 3 more rows not shown",
        "\"ok\": true",
        "📎 chart.png · image/png · 471 KB — a chart",
        "Docs <https://example.com/docs>",
        "↗ session abc123 · reviewer",
        "cargo test",
        "Result",
        "one",
        "done",
    ] {
        assert!(text.contains(expected), "{expected}\n{text}");
    }
    assert!(
        layout
            .regions
            .iter()
            .any(|region| region.id == BlockId::ToolSession { call_id, session })
    );
    let link = row_spans(&layout.lines, "Docs <");
    assert!(
        link.iter()
            .any(|(text, style)| text == "Docs" && style.fg == Theme::default().link().fg)
    );
}

#[test]
fn blocks_of_an_unknown_kind_show_nothing() {
    let mut state = shown(vec![DisplayBlock::text("known")]);
    let call_id = read_tool_id(&state);
    state.tools.get_mut(&call_id).unwrap().display.push(
        serde_json::from_value(serde_json::json!({"type": "hologram", "beam": 1}))
            .expect("open block"),
    );
    assert!(matches!(
        state.tools[&call_id].display[1],
        Open::Unknown { .. }
    ));
    let text = expanded_text(&state, 80);
    assert!(text.contains("known"), "{text}");
    assert!(!text.contains("hologram"), "{text}");
}

#[test]
fn hints_choose_icons_and_tool_names_do_not() {
    for (hint, icon) in [
        (Some(ToolHint::SHELL), "💻"),
        (Some(ToolHint::FILE_READ), "📖"),
        (Some(ToolHint::FILE_WRITE), "📝"),
        (Some(ToolHint::FILE_EDIT), "📝"),
        (Some(ToolHint::FETCH), "🌐"),
        (Some(ToolHint::SEARCH), "🔍"),
        (Some(ToolHint::DELEGATE), "🤖"),
        (Some(ToolHint::MESSAGE), "📨"),
        (Some(ToolHint::SKILL), "✨"),
        (Some(ToolHint::GOAL), "🎯"),
        (Some(ToolHint::GENERIC), "🔨"),
        (Some("teleport"), "🔨"),
        (None, "🔨"),
    ] {
        assert_eq!(hint_icon(hint), icon, "{hint:?}");
    }
    // A call named like a built-in but carrying no hint is generic.
    let state = read_tool_state("unused", ToolStatus::Completed, "result");
    let mut named = state.clone();
    let call_id = read_tool_id(&named);
    named.tools.get_mut(&call_id).unwrap().presentation = presentation("bash", None);
    assert!(snapshot_lines(&transcript_layout(&named, None, 80).lines).contains("🔨 ▸ bash"));
    named.tools.get_mut(&call_id).unwrap().presentation =
        hinted_presentation("Run `ls`", ToolHint::SHELL, Some("ls"));
    // The title already names the argument, so the header does not repeat it.
    let header = snapshot_lines(&transcript_layout(&named, None, 80).lines);
    assert!(header.contains("💻 ▸ Run `ls`"), "{header}");
    assert!(!header.contains("Run `ls` ls"), "{header}");
}

#[test]
fn a_title_that_cut_its_argument_short_does_not_repeat_it() {
    let command = format!("cargo test {}", "--flag ".repeat(12));
    let title = format!("Run `{}…`", &command[..40]);
    let mut state = read_tool_state("unused", ToolStatus::Completed, "result");
    let call_id = read_tool_id(&state);
    state.tools.get_mut(&call_id).unwrap().presentation =
        hinted_presentation(&title, ToolHint::SHELL, Some(command.trim()));
    let header = snapshot_lines(&transcript_layout(&state, None, 200).lines);
    assert!(header.contains(&title), "{header}");
    assert!(!header.contains("…` cargo"), "{header}");
    // A title that does not show its argument gets it after the title.
    state.tools.get_mut(&call_id).unwrap().presentation =
        hinted_presentation("lookup", ToolHint::SEARCH, Some("rust lifetimes"));
    let header = snapshot_lines(&transcript_layout(&state, None, 80).lines);
    assert!(header.contains("🔍 ▸ lookup rust lifetimes"), "{header}");
}

#[test]
fn argument_blocks_show_above_the_output_unless_the_header_shows_them() {
    let mut state = shown(vec![terminal(&[(TerminalStream::Stdout, "hi\n")], Some(0))]);
    let call_id = read_tool_id(&state);
    let tool = state.tools.get_mut(&call_id).unwrap();
    tool.presentation = hinted_presentation("Run `echo hi`", ToolHint::SHELL, Some("echo hi"));
    tool.presentation.arguments = vec![Open::Known(DisplayBlock::code(
        "echo hi\necho there",
        Some("sh".into()),
    ))];
    let expanded = HashSet::from([BlockId::Tool(call_id)]);
    let layout = transcript_layout_with_level(
        &state,
        Some(&expanded),
        80,
        &Theme::default(),
        &PlainHighlighter,
        crate::state::EventLevel::Warning,
    );
    let text = snapshot_lines(&layout.lines);
    let arguments = text.find("arguments").expect("arguments label");
    let command = text.find("echo there").expect("command");
    let output = text
        .find("│  hi")
        .or_else(|| text.find(" hi\n"))
        .expect("output");
    assert!(arguments < command && command < output, "{text}");
    // A command block that only repeats what the header shows is left out;
    // the longer one above stayed because it says more.
    state
        .tools
        .get_mut(&call_id)
        .unwrap()
        .presentation
        .arguments = vec![Open::Known(DisplayBlock::code(
        "echo hi\n",
        Some("sh".into()),
    ))];
    let text = snapshot_lines(
        &transcript_layout_with_level(
            &state,
            Some(&expanded),
            80,
            &Theme::default(),
            &PlainHighlighter,
            crate::state::EventLevel::Warning,
        )
        .lines,
    );
    assert!(!text.contains("arguments"), "{text}");
    // The raw arguments of a call without argument blocks are diagnostic
    // detail, hidden above the `info` level.
    let mut plain = read_tool_state("unused", ToolStatus::Completed, "result");
    let plain_id = read_tool_id(&plain);
    plain.tools.get_mut(&plain_id).unwrap().arguments = r#"{"query":"x"}"#.into();
    let expanded = HashSet::from([BlockId::Tool(plain_id)]);
    for (level, shown) in [
        (crate::state::EventLevel::Info, true),
        (crate::state::EventLevel::Warning, false),
    ] {
        let text = snapshot_lines(
            &transcript_layout_with_level(
                &plain,
                Some(&expanded),
                80,
                &Theme::default(),
                &PlainHighlighter,
                level,
            )
            .lines,
        );
        assert_eq!(
            text.contains("\"query\": \"x\""),
            shown,
            "{level:?}\n{text}"
        );
    }
}

#[test]
fn arguments_leave_out_what_the_header_and_preview_already_show() {
    let mut state = shown(vec![DisplayBlock::text("done")]);
    let call_id = read_tool_id(&state);
    let tool = state.tools.get_mut(&call_id).unwrap();
    tool.arguments = r#"{"filePath":"notes.txt"}"#.into();
    tool.presentation =
        hinted_presentation("Edit notes.txt", ToolHint::FILE_EDIT, Some("notes.txt"));
    // A key/value row that repeats the primary argument is left out; the
    // others stay.
    tool.presentation.arguments = vec![Open::Known(DisplayBlock::key_value([
        ("path", "notes.txt"),
        ("replace all", "yes"),
    ]))];
    let expanded = HashSet::from([BlockId::Tool(call_id)]);
    let render = |state: &SessionState| {
        snapshot_lines(
            &transcript_layout_with_level(
                state,
                Some(&expanded),
                80,
                &Theme::default(),
                &PlainHighlighter,
                crate::state::EventLevel::Debug,
            )
            .lines,
        )
    };
    let text = render(&state);
    assert!(text.contains("replace all"), "{text}");
    assert!(!text.contains("path"), "{text}");
    // A block left with nothing to show drops the whole section, and a call
    // with a preview does not fall back to its raw arguments.
    let tool = state.tools.get_mut(&call_id).unwrap();
    tool.presentation.arguments = vec![Open::Known(DisplayBlock::key_value([(
        "path",
        "notes.txt",
    )]))];
    tool.presentation.preview = vec![Open::Known(DisplayBlock::text("preview"))];
    let text = render(&state);
    assert!(!text.contains("arguments"), "{text}");
    assert!(!text.contains("filePath"), "{text}");
}

#[test]
fn live_appends_and_the_progress_message_show_while_the_call_runs() {
    let mut state = display_tool_state(
        hinted_presentation("Run `make`", ToolHint::SHELL, Some("make")),
        ToolStatus::Running,
        Vec::new(),
    );
    let call_id = read_tool_id(&state);
    for (text, stream) in [
        ("compiling\n", TerminalStream::Stdout),
        ("warn", TerminalStream::Stderr),
    ] {
        crate::state::reduce_live_progress_for_test(
            &mut state,
            call_id,
            "make output",
            cookie_agent_protocol::DisplayDelta::Append {
                index: 0,
                text: text.into(),
                stream: Some(stream),
            },
        );
    }
    let collapsed = snapshot_lines(&transcript_layout(&state, None, 80).lines);
    assert!(
        collapsed.contains("Run `make` · make output"),
        "{collapsed}"
    );
    let lines = expanded_layout(&state, 80).lines;
    let text = snapshot_lines(&lines);
    assert!(text.contains("⋯ make output"), "{text}");
    assert!(text.contains("compiling"), "{text}");
    let warn = row_spans(&lines, "warn");
    assert!(
        warn.iter()
            .any(|(text, style)| text == "warn" && style.fg == Theme::default().warning().fg)
    );
}

#[tokio::test]
async fn clicking_the_full_output_row_reads_the_artifact_and_shows_it() {
    let mut app = test_app().await;
    let (client, recorded, _incoming) = live_recording_client();
    app.client = client;
    let session = cookie_agent_protocol::SessionId::new_v7();
    let mut block = terminal(&[(TerminalStream::Stdout, "head\n")], Some(0));
    let full = artifact("stdout", "text/plain", 3 * 1024 * 1024);
    if let DisplayBlock::Terminal {
        omitted_lines,
        full: block_full,
        ..
    } = &mut block
    {
        *omitted_lines = Some(5);
        *block_full = Some(full.clone());
    }
    let state = shown(vec![block]);
    let call_id = read_tool_id(&state);
    app.store.sessions.insert(session, state);
    app.selected = Some(session);
    app.tree_root = Some(session);
    app.expanded_blocks
        .insert(session, HashSet::from([BlockId::Tool(call_id)]));
    rendered_frame(&mut app, 120, 40);
    let row = app
        .hit_map
        .blocks
        .iter()
        .find(|hit| hit.id == BlockId::ToolFull { call_id, block: 0 })
        .and_then(|hit| hit.toggle_rect)
        .expect("full output row");
    app.handle_click(row.x, row.y).await;
    let rendered = rendered_frame(&mut app, 120, 40);
    assert!(rendered.contains("loading the full output"), "{rendered}");
    wait_for_recorded_request(&recorded, "artifact.read", 1).await;
    let params = last_request_params(&recorded, "artifact.read");
    assert_eq!(params["session_id"], session.to_string());
    assert_eq!(params["sha256"], full.sha256);
    assert_eq!(params["offset"], 0);
    // At most one chunk is read, however long the artifact is.
    assert_eq!(
        params["length"],
        cookie_agent_protocol::ARTIFACT_CHUNK_BYTES
    );
    // The output expanded with it, so the larger budget shows what loads.
    assert!(
        app.expanded_blocks[&session].contains(&BlockId::ToolOutput {
            call_id,
            section: ToolOutputSection::Detail
        })
    );

    app.handle_rpc_update(crate::ui::app::RpcUpdate::FullContentLoaded {
        session_id: session,
        call_id,
        block: 0,
        state: crate::state::FullContentState::Loaded {
            text: "line 1\n\u{1b}[32mline 2\u{1b}[0m\n".into(),
            truncated: true,
        },
    });
    let rendered = rendered_frame(&mut app, 120, 40);
    assert!(rendered.contains("line 2"), "{rendered}");
    assert!(!rendered.contains("head"), "{rendered}");
    assert!(
        rendered.contains("▴ full output (its first 1.0 MB) · click to show the cut display"),
        "{rendered}"
    );
    // Clicking again puts it away.
    let row = app
        .hit_map
        .blocks
        .iter()
        .find(|hit| hit.id == BlockId::ToolFull { call_id, block: 0 })
        .and_then(|hit| hit.toggle_rect)
        .expect("full output row");
    app.handle_click(row.x, row.y).await;
    let rendered = rendered_frame(&mut app, 120, 40);
    assert!(rendered.contains("head"), "{rendered}");
    assert!(app.store.sessions[&session].tools[&call_id].full.is_empty());
}

#[tokio::test]
async fn a_failed_read_offers_to_try_again() {
    let mut app = test_app().await;
    let session = cookie_agent_protocol::SessionId::new_v7();
    let mut block = terminal(&[(TerminalStream::Stdout, "head\n")], None);
    if let DisplayBlock::Terminal {
        omitted_lines,
        full,
        ..
    } = &mut block
    {
        *omitted_lines = Some(1);
        *full = Some(artifact("stdout", "text/plain", 100));
    }
    let state = shown(vec![block]);
    let call_id = read_tool_id(&state);
    app.store.sessions.insert(session, state);
    app.selected = Some(session);
    app.toggle_block(BlockId::ToolFull { call_id, block: 0 });
    app.handle_rpc_update(crate::ui::app::RpcUpdate::FullContentLoaded {
        session_id: session,
        call_id,
        block: 0,
        state: crate::state::FullContentState::Failed("artifact not found".into()),
    });
    let text = expanded_text(&app.store.sessions[&session], 120);
    assert!(
        text.contains("could not load the full output (artifact not found); click to retry"),
        "{text}"
    );
}

#[tokio::test]
async fn clicking_a_session_reference_opens_that_session() {
    let mut app = test_app().await;
    let session = cookie_agent_protocol::SessionId::new_v7();
    let child = cookie_agent_protocol::SessionId::new_v7();
    let state = shown(vec![DisplayBlock::SessionRef {
        session_id: child,
        short_id: Some("kid".into()),
        label: None,
    }]);
    let call_id = read_tool_id(&state);
    app.store.sessions.insert(session, state);
    app.selected = Some(session);
    app.tree_root = Some(session);
    app.expanded_blocks
        .insert(session, HashSet::from([BlockId::Tool(call_id)]));
    let rendered = rendered_frame(&mut app, 100, 30);
    assert!(rendered.contains("↗ session kid"), "{rendered}");
    let row = app
        .hit_map
        .blocks
        .iter()
        .find(|hit| {
            hit.id
                == BlockId::ToolSession {
                    call_id,
                    session: child,
                }
        })
        .and_then(|hit| hit.toggle_rect)
        .expect("session row");
    app.handle_click(row.x, row.y).await;
    assert_eq!(app.selected, Some(child));
}
