use crate::ui::transcript::*;

use cookie_agent_protocol::SessionId;

use ratatui::{Terminal, backend::TestBackend, text::Line};

use crate::markdown::PlainHighlighter;

use crate::theme::{ColorLevel, ThemeKind};

use crate::ui::app::*;

use super::support::*;

#[test]
fn tool_icons_map_core_tools_and_keep_hammer_for_plugins() {
    for (name, icon) in [
        ("bash", "💻"),
        ("read", "📖"),
        ("write", "✏️"),
        ("edit", "✏️"),
        ("delegate_subagent", "🤖"),
        ("get_subagent_result", "🤖"),
        ("steer_subagent", "🤖"),
        ("cancel_subagent", "🤖"),
        ("skill", "✨"),
        ("goal_get", "🎯"),
        ("goal_update", "🎯"),
        ("plugin.build", "🔨"),
        ("mcp__server__read", "🔨"),
        ("unknown", "🔨"),
    ] {
        assert_eq!(tool_icon(name), icon, "{name}");
    }
    for name in ["goal_get", "goal_update"] {
        let mut state = read_tool_state("unused", ToolStatus::Completed, "goal result");
        let call_id = read_tool_id(&state);
        state.tools.get_mut(&call_id).unwrap().presentation = presentation(name, None);
        let expanded = HashSet::from([BlockId::Tool(call_id)]);
        for (blocks, chevron) in [(None, '▸'), (Some(&expanded), '▾')] {
            assert!(
                snapshot_lines(&transcript_layout(&state, blocks, 80).lines)
                    .contains(&format!("🎯 {chevron} {name}"))
            );
        }
    }
}

#[test]
fn builtin_tool_headers_abbreviate_arguments_without_losing_expanded_content() {
    let path = "src/very/deeply/nested/module/transcript.rs";
    let long_command = format!("command {} done", "long-argument ".repeat(12));
    // A 40-column row keeps 38 columns of content behind its 2-column
    // gutter, the `{icon} {chevron} ` markers take 5 of those, and `Read `
    // takes 5 more: 28 columns are left for the argument.
    assert_eq!(tool_header_content_width(40), 38);
    assert_eq!(
        UnicodeWidthStr::width(tool_header_chrome("📖", '▸', 40).as_str()),
        5
    );
    let read_budget = header_argument_width("Read", 33);
    assert_eq!(read_budget, 28);
    assert_eq!(
        abbreviate_tool_argument("read", path, read_budget),
        "src/…/transcript.rs"
    );
    // Paths keep both ends while the budget covers the tail, otherwise the
    // row truncates at the right edge.
    assert_eq!(abbreviate_tool_argument("edit", path, 14), "…transcript.rs");
    assert_eq!(abbreviate_tool_argument("read", path, 43), path);
    // A `running` suffix costs two columns, and the argument fills exactly
    // what the row leaves over rather than wrapping onto a second line.
    let bash_budget = header_argument_width("bash", tool_header_content_width(80) - 5 - 2);
    assert_eq!(bash_budget, 66);
    let abbreviated = abbreviate_tool_argument("bash", &long_command, bash_budget);
    assert_eq!(UnicodeWidthStr::width(abbreviated.as_str()), bash_budget);
    assert!(abbreviated.ends_with('…'));
    // On-wire arguments may carry controls; the header flattens them.
    assert_eq!(
        abbreviate_tool_argument("bash", "printf hello\n  && true", 80),
        "printf hello && true"
    );
    assert_eq!(
        abbreviate_tool_argument("bash", "printf \u{1b}[31mred\u{0}\nsecond\tline", 80),
        "printf [31mred second line"
    );
    for name in [
        "bash",
        "read",
        "write",
        "edit",
        "delegate_subagent",
        "get_subagent_result",
        "steer_subagent",
        "cancel_subagent",
        "skill",
        "goal_get",
        "goal_update",
    ] {
        let argument = if matches!(name, "read" | "write" | "edit") {
            path.to_owned()
        } else {
            long_command.clone()
        };
        let mut state = read_tool_state(path, ToolStatus::Completed, "result");
        let id = read_tool_id(&state);
        let tool = state.tools.get_mut(&id).unwrap();
        tool.presentation = presentation(name, Some(&argument));
        tool.arguments = serde_json::json!({"command": argument, "filePath": path}).to_string();
        let expanded = HashSet::from([BlockId::Tool(id)]);
        let label = if name == "read" { "Read" } else { name };
        for width in [8, 12, 20, 40, 80, 160] {
            // A zero budget means the label alone fills the row and the
            // header drops the argument entirely; `.max(1)` keeps the
            // abbreviation helper comparable here.
            let budget = header_argument_width(
                label,
                tool_header_content_width(width)
                    - UnicodeWidthStr::width(
                        tool_header_chrome(tool_icon(name), '▸', width).as_str(),
                    ),
            )
            .max(1);
            let abbreviated = abbreviate_tool_argument(name, &argument, budget);
            assert!(UnicodeWidthStr::width(abbreviated.as_str()) <= budget);
            assert!(!abbreviated.contains(['\n', '\r', '\t']));
            for blocks in [None, Some(&expanded)] {
                let layout = transcript_layout_with(
                    &state,
                    blocks,
                    width,
                    &Theme::default(),
                    &PlainHighlighter,
                );
                let region = layout
                    .regions
                    .iter()
                    .find(|region| region.id == BlockId::Tool(id))
                    .unwrap();
                assert_eq!(
                    region.header_lines,
                    Some(1),
                    "{name}@{width}: {:?}",
                    snapshot_lines(&layout.lines)
                );
            }
        }
        if matches!(name, "read" | "write" | "edit") {
            let text = snapshot_lines(&transcript_layout(&state, Some(&expanded), 80).lines);
            assert!(text.contains(path), "{name}: {text}");
        }
    }
    let unicode = "src/目录/文件/文件👩‍💻.rs";
    for width in [8, 16, 24, 40] {
        let budget = header_argument_width("Read", tool_header_content_width(width) - 5).max(1);
        let short = abbreviate_tool_argument("read", unicode, budget);
        assert!(UnicodeWidthStr::width(short.as_str()) <= budget);
        assert!(!short.starts_with("…\u{200d}"));
    }
    let mut plugin = read_tool_state(path, ToolStatus::Completed, "result");
    let id = read_tool_id(&plugin);
    let tool = plugin.tools.get_mut(&id).unwrap();
    tool.presentation = presentation("plugin.tool", Some(path));
    // The plugin header owns the full row: a wide row keeps its compact
    // title, whose label width (not a fixed five-column budget) is spent.
    assert_eq!(tool_header_row(tool, '▸', "", 80).1, tool.compact_title());
    tool.presentation = presentation("plugin.tool", Some("run\u{1b} echo"));
    assert_eq!(tool_header_row(tool, '▸', "", 40).1, "plugin.tool run echo");
}

#[test]
fn builtin_tool_headers_hold_one_row_at_every_width_and_status() {
    let longest = "x".repeat(cookie_agent_protocol::BoundedDisplayText::MAX_BYTES);
    // A plugin label longer than any row, and the built-in whose expanded
    // body draws the terminal band that used to widen past the viewport.
    for (name, argument) in [
        ("mcp__long_running_server__exec_command", longest.as_str()),
        ("bash", "xxxxxxx"),
        // Two-column graphemes cannot be split, so a row that spends its
        // last column on an indent would otherwise be pushed out of the
        // viewport by a single character.
        ("bash", "echo 世界界界"),
    ] {
        let mut state = read_tool_state("src/lib.rs", ToolStatus::Completed, "partial");
        let id = read_tool_id(&state);
        let tool = state.tools.get_mut(&id).unwrap();
        tool.presentation = presentation(name, Some(argument));
        tool.arguments = serde_json::json!({"command": argument}).to_string();
        let expanded = HashSet::from([BlockId::Tool(id)]);
        // `tool_block_lines` fronts a narrow row with its `"[T…] "` role
        // label (shortened with the row) and the `"│ "` gutter from 8 columns
        // up; the widest status suffix is `" interrupted"` (12) and the
        // running marker grows with the animation clock, so every phase and
        // every terminal status gets its own pass from one column up.
        for status in [
            ToolStatus::Completed,
            ToolStatus::Running,
            ToolStatus::Failed,
            ToolStatus::Cancelled,
            ToolStatus::Interrupted,
        ] {
            let status_name = format!("{status:?}");
            state.tools.get_mut(&id).unwrap().status = status;
            for clock_bucket in 0..=3u8 {
                for width in [1u16, 2, 3, 4, 5, 6, 7, 8, 9, 10, 12, 16, 19, 20, 24, 40, 80] {
                    for blocks in [None, Some(&expanded)] {
                        let layout = tool_layout_at_clock(&state, blocks, width, clock_bucket);
                        let region = layout
                            .regions
                            .iter()
                            .find(|region| region.id == BlockId::Tool(id))
                            .unwrap();
                        let rendered = snapshot_lines(&layout.lines);
                        assert_eq!(
                            region.header_lines,
                            Some(1),
                            "{name}@{status_name}@{clock_bucket}x{width}: {rendered:?}"
                        );
                        assert!(
                            layout
                                .lines
                                .iter()
                                .all(|line| line.width() <= usize::from(width)),
                            "{name}@{status_name}@{clock_bucket}x{width}: {rendered:?}"
                        );
                        // Drawing through the widget the app renders with
                        // proves the overflow is gone rather than merely
                        // clipped: ratatui resets the cell behind a
                        // double-width glyph, so text is compared with
                        // spacing ignored.
                        let rows = render_to_buffer(&layout.lines, width);
                        let compact = |text: String| -> String {
                            text.chars().filter(|c| !c.is_whitespace()).collect()
                        };
                        let laid_out = layout
                            .lines
                            .iter()
                            .map(ToString::to_string)
                            .collect::<String>();
                        assert_eq!(
                            compact(rows.join("")),
                            compact(laid_out),
                            "{name}@{status_name}@{clock_bucket}x{width}: {rows:?}"
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn tool_header_surrenders_suffix_argument_and_label_by_turn_on_narrow_rows() {
    let mut state = read_tool_state("src/lib.rs", ToolStatus::Interrupted, "partial");
    let id = read_tool_id(&state);
    let tool = state.tools.get_mut(&id).unwrap();
    tool.presentation = presentation("bash", Some("printf hello"));
    let header = |width| {
        let layout = tool_layout_at_clock(&state, None, width, 0);
        let region = layout
            .regions
            .iter()
            .find(|region| region.id == BlockId::Tool(id))
            .unwrap();
        assert_eq!(region.header_lines, Some(1), "width {width}");
        layout.lines[region.start_line].to_string()
    };
    // 20 columns: gutter (2) + markers (5) + `" interrupted"` (12) leaves one
    // column of title, so the argument gives way and the status stays.
    assert_eq!(header(20), "│ 💻 ▸ … interrupted");
    // One column narrower no title column survives, and the markers close up
    // instead of leaving a gap in front of the suffix.
    assert_eq!(header(19), "│ 💻 ▸ interrupted");
    // Below the suffix's own width it drops out entirely: the row's failure
    // colour already says the call did not finish, and the argument returns.
    assert_eq!(header(18), "│ 💻 ▸ bash print…");
    // Under 8 columns the gutter swaps for the role label, which leaves room
    // for the chevron alone.
    assert_eq!(header(7), "[T!] ▸");
    assert_eq!(header(6), "[T!] ▸");
    // From here the label shortens with the row instead of forcing a wrap,
    // and always keeps a column for the markers.
    assert_eq!(header(5), "[T!]▸");
    assert_eq!(header(4), "[T…▸");
    assert_eq!(header(3), "[…▸");
    assert_eq!(header(2), "…▸");
    // One column left: the label cannot survive, so the row keeps nothing that
    // would overflow it.
    assert_eq!(header(1), "");
}

#[test]
fn expanded_read_path_renders_embedded_tabs_as_spaces() {
    let path = "src/my\tfile.rs";
    let mut state = read_tool_state(path, ToolStatus::Completed, "result");
    let id = read_tool_id(&state);
    state.tools.get_mut(&id).unwrap().presentation = presentation("read", Some(path));
    let expanded = HashSet::from([BlockId::Tool(id)]);
    let rows = render_to_buffer(&transcript_layout(&state, Some(&expanded), 80).lines, 80);
    // The terminal renderer drops control characters outright, so a literal
    // tab would silently vanish from the buffer row. Both the collapsed
    // header and the expanded `path:` row must show the flattened space.
    assert!(
        rows.iter().any(|row| row.ends_with("Read src/my file.rs")),
        "{rows:?}"
    );
    assert!(
        rows.iter().any(|row| row.ends_with("path: src/my file.rs")),
        "{rows:?}"
    );
    assert!(
        !rows.iter().any(|row| row.contains("src/myfile")),
        "{rows:?}"
    );
}

#[test]
fn builtin_tool_headers_flatten_control_bearing_arguments() {
    let argument = "printf \u{1b}[31mred\u{1b}[0m\nsecond\tline";
    let mut bash = read_tool_state("unused", ToolStatus::Completed, "result");
    let id = read_tool_id(&bash);
    bash.tools.get_mut(&id).unwrap().presentation = presentation("bash", Some(argument));
    for blocks in [None, Some(&HashSet::from([BlockId::Tool(id)]))] {
        let rendered = snapshot_lines(&transcript_layout(&bash, blocks, 80).lines);
        assert!(
            rendered.contains("bash printf [31mred [0m second line"),
            "{rendered:?}"
        );
        assert!(!rendered.contains('\u{1b}'), "{rendered:?}");
    }
    let mut read = read_tool_state("unused", ToolStatus::Completed, "result");
    let id = read_tool_id(&read);
    read.tools.get_mut(&id).unwrap().presentation = presentation("read", Some("src/lib\u{1b}.rs"));
    let rendered = snapshot_lines(&transcript_layout(&read, None, 80).lines);
    assert!(rendered.contains("📖 ▸ Read src/lib .rs"), "{rendered:?}");
    assert!(!rendered.contains('\u{1b}'), "{rendered:?}");
    let rendered = snapshot_lines(
        &transcript_layout(&read, Some(&HashSet::from([BlockId::Tool(id)])), 80).lines,
    );
    assert!(
        rendered.contains("path: src/lib\u{fffd}.rs"),
        "{rendered:?}"
    );
    assert!(!rendered.contains('\u{1b}'), "{rendered:?}");
}

#[test]
fn bash_expanded_command_keeps_its_line_breaks() {
    let mut state = read_tool_state("unused", ToolStatus::Completed, "ok\n");
    let id = read_tool_id(&state);
    let tool = state.tools.get_mut(&id).unwrap();
    tool.presentation = presentation("bash", Some("python3 - <<'PY'"));
    tool.arguments = serde_json::json!({
        "command": "python3 - <<'PY'\nimport re\nprint(\"hi\u{1b}\")\nPY\ncat /proc/meminfo"
    })
    .to_string();
    let expanded = HashSet::from([BlockId::Tool(id)]);
    let layout = transcript_layout(&state, Some(&expanded), 80);
    let region = layout
        .regions
        .iter()
        .find(|region| region.id == BlockId::Tool(id))
        .unwrap();
    let rows: Vec<String> = layout.lines[region.start_line..region.end_line]
        .iter()
        .map(|line| line.to_string().trim_end().to_owned())
        .collect();
    assert!(rows[1].ends_with("❯ python3 - <<'PY'"), "{rows:?}");
    assert!(rows[2].ends_with("  import re"), "{rows:?}");
    // Other control characters still flatten to the replacement character.
    assert!(rows[3].ends_with("  print(\"hi\u{fffd}\")"), "{rows:?}");
    assert!(rows[4].ends_with("  PY"), "{rows:?}");
    assert!(rows[5].ends_with("  cat /proc/meminfo"), "{rows:?}");
    let rendered = snapshot_lines(&layout.lines[region.start_line..region.end_line]);
    assert!(!rendered.contains("<<'PY'\u{fffd}"), "{rendered}");

    // A long script is elided at the command byte budget, on the last line
    // shown, and the lines past it are dropped rather than flattened.
    let script = (0..400)
        .map(|index| format!("echo line {index:03}\n"))
        .collect::<String>();
    let tool = state.tools.get_mut(&id).unwrap();
    tool.arguments = serde_json::json!({ "command": script }).to_string();
    let layout = transcript_layout(&state, Some(&expanded), 80);
    let region = layout
        .regions
        .iter()
        .find(|region| region.id == BlockId::Tool(id))
        .unwrap();
    let rows: Vec<String> = layout.lines[region.start_line..region.end_line]
        .iter()
        .map(|line| line.to_string().trim_end().to_owned())
        .collect();
    let command_rows: Vec<&String> = rows.iter().filter(|row| row.contains("echo")).collect();
    assert!(command_rows.len() > 100, "{}", command_rows.len());
    assert!(command_rows.len() < 400, "{}", command_rows.len());
    assert!(command_rows.last().unwrap().ends_with('…'), "{rows:?}");
    let rendered = snapshot_lines(&layout.lines[region.start_line..region.end_line]);
    assert!(!rendered.contains('\u{fffd}'), "{rendered}");
}

#[test]
fn bash_expanded_rows_sit_on_the_terminal_band() {
    let mut state = read_tool_state(
        "unused",
        ToolStatus::Completed,
        &(0..100)
            .map(|index| format!("output {index}\n"))
            .collect::<String>(),
    );
    let id = read_tool_id(&state);
    let tool = state.tools.get_mut(&id).unwrap();
    tool.presentation = presentation("bash", Some("printf hello"));
    tool.arguments = r#"{"command":"printf hello"}"#.into();
    let expanded = HashSet::from([BlockId::Tool(id)]);
    for theme in [
        Theme::default(),
        Theme::new(ThemeKind::Dark, ColorLevel::TrueColor),
        Theme::new(ThemeKind::Default, ColorLevel::Ansi256),
        Theme::new(ThemeKind::Dark, ColorLevel::Ansi256),
        Theme::new(ThemeKind::Mono, ColorLevel::None),
        Theme::new(ThemeKind::HighContrast, ColorLevel::Ansi16),
    ] {
        let collapsed = transcript_layout_with(&state, None, 80, &theme, &PlainHighlighter);
        let region = collapsed
            .regions
            .iter()
            .find(|region| region.id == BlockId::Tool(id))
            .unwrap();
        assert!(
            collapsed.lines[region.start_line..region.end_line]
                .iter()
                .flat_map(|line| &line.spans)
                .all(|span| span.style.bg.is_none())
        );
        let layout = transcript_layout_with(&state, Some(&expanded), 80, &theme, &PlainHighlighter);
        let region = layout
            .regions
            .iter()
            .find(|region| region.id == BlockId::Tool(id))
            .unwrap();
        let rows = &layout.lines[region.start_line..region.end_line];
        assert!(rows[0].to_string().contains("💻 ▾ bash printf hello"));
        assert!(rows[1].to_string().contains("❯ printf hello"));
        assert!(!snapshot_lines(rows).contains("arguments:"));
        let body_width = rows[1].width();
        for (index, row) in rows.iter().enumerate() {
            let banded = !row.to_string().contains("more lines");
            // Counted chrome columns, not span text: `"│ "` in the middle of
            // a row is tree output and belongs on the band.
            let chrome = usize::from(leading_gutter_columns(row));
            let mut column = 0;
            for span in row.spans.iter() {
                let is_gutter = column < chrome;
                assert_eq!(
                    span.style.bg,
                    if banded && !is_gutter {
                        theme.terminal_background()
                    } else {
                        None
                    },
                    "{index}: {row}"
                );
                column += UnicodeWidthStr::width(span.content.as_ref());
            }
            if banded && theme.terminal_background().is_some() {
                assert_eq!(row.width(), body_width);
            }
        }
    }
    state.tools.get_mut(&id).unwrap().arguments = r#"{"command":42}"#.into();
    assert!(
        snapshot_lines(&transcript_layout(&state, Some(&expanded), 80).lines)
            .contains(r#"arguments: {"command":42}"#)
    );
}

#[test]
fn the_band_covers_tree_shaped_output_and_spares_the_block_gutter() {
    let theme = Theme::default();
    let background = theme
        .terminal_background()
        .expect("the parchment theme bands the terminal");
    let mut state = read_tool_state(
        "unused",
        ToolStatus::Completed,
        "│ \n│ ├── src\n│ └── tests\n",
    );
    let id = read_tool_id(&state);
    let tool = state.tools.get_mut(&id).unwrap();
    tool.presentation = presentation("bash", Some("tree"));
    tool.arguments = r#"{"command":"tree"}"#.into();
    let layout = transcript_layout_with(
        &state,
        Some(&HashSet::from([BlockId::Tool(id)])),
        80,
        &theme,
        &PlainHighlighter,
    );
    let region = layout
        .regions
        .iter()
        .find(|region| region.id == BlockId::Tool(id))
        .unwrap();
    // A row whose own text starts with the block's gutter glyph is the
    // whole trick: the chrome is one span wide and the output beside it,
    // every padded column of it, is banded.
    let row = layout.lines[region.start_line..region.end_line]
        .iter()
        .find(|row| row.to_string().contains("├──"))
        .expect("the tree row reaches the layout");
    assert_eq!(leading_gutter_columns(row), 2, "one `│ ` of chrome: {row}");
    assert_eq!(row.spans[0].style.bg, None, "the gutter is never banded");
    assert!(
        row.spans[1..]
            .iter()
            .all(|span| span.style.bg == Some(background)),
        "every column of output sits on the band: {row}"
    );
    // Short output still reads as one panel: every banded row runs to the
    // block's full width, not just to its widest row.
    let banded_widths = layout.lines[region.start_line..region.end_line]
        .iter()
        .filter(|row| {
            row.spans
                .iter()
                .any(|span| span.style.bg == Some(background))
        })
        .map(Line::width)
        .collect::<Vec<_>>();
    assert!(
        banded_widths.iter().all(|width| *width >= 60),
        "the band spans the block, not the text: {banded_widths:?}"
    );
}

#[test]
fn leading_chrome_is_counted_by_position_not_by_string_equality() {
    assert_eq!(leading_gutter_columns(&Line::from("plain row")), 0);
    assert_eq!(
        leading_gutter_columns(&Line::from(vec![Span::raw("│ "), Span::raw("text")])),
        2
    );
    assert_eq!(leading_gutter_columns(&Line::from("│ welded gutter")), 2);
    assert_eq!(
        leading_gutter_columns(&Line::from(vec![
            Span::raw("│ "),
            Span::raw("│ nested"),
            Span::raw(" tail"),
        ])),
        2,
        "tree output behind the gutter is not a second gutter"
    );
    assert_eq!(
        leading_gutter_columns(&Line::from(vec![Span::raw("│ "), Span::raw("┃ ")])),
        4
    );
    assert_eq!(
        leading_gutter_columns(&Line::from(vec![Span::raw("│ "), Span::raw("  indent")])),
        2,
        "indentation behind a real gutter is content"
    );
    // Leading spaces welded to the front of a row are ambiguous — a
    // hanging indent and code indentation look identical — so they count
    // as content, the same call `extract_line` makes about what to strip
    // from a copy. A continuation indent the builder emitted as its own
    // span is chrome.
    assert_eq!(leading_gutter_columns(&Line::from("  indented code")), 0);
    assert_eq!(
        leading_gutter_columns(&Line::from(vec![Span::raw("  "), Span::raw("code")])),
        2
    );
    assert_eq!(
        leading_gutter_columns(&Line::from(vec![Span::raw("  "), Span::raw("  ")])),
        2,
        "a continuation indent is chrome once, never twice"
    );
    assert_eq!(
        leading_gutter_columns(&Line::from(vec![Span::raw("[T…] "), Span::raw("[T…] ")])),
        5,
        "a narrow-mode tag is chrome; the same tag beside it is content"
    );
}

#[test]
fn expanded_bash_body_yields_its_hanging_indent_to_wide_graphemes() {
    let command = "echo 世界界界";
    let mut state = read_tool_state("unused", ToolStatus::Completed, "partial");
    let id = read_tool_id(&state);
    let tool = state.tools.get_mut(&id).unwrap();
    tool.presentation = presentation("bash", Some(command));
    tool.arguments = serde_json::json!({"command": command}).to_string();
    let expanded = HashSet::from([BlockId::Tool(id)]);
    let compact = |text: String| -> String {
        text.chars()
            .filter(|character| !character.is_whitespace())
            .collect()
    };
    let tool_rows = |width: u16| {
        let layout = transcript_layout(&state, Some(&expanded), width);
        for line in &layout.lines {
            assert!(
                line.width() <= usize::from(width),
                "width {width}: {:?}",
                snapshot_lines(&layout.lines)
            );
        }
        let rows = render_to_buffer(&layout.lines, width);
        // Nothing is clipped by the renderer either: no row spills into cells
        // the terminal would have to drop.
        assert_eq!(
            compact(rows.join("")),
            compact(
                layout
                    .lines
                    .iter()
                    .map(ToString::to_string)
                    .collect::<String>()
            ),
            "width {width}: {rows:?}"
        );
        let region = layout
            .regions
            .iter()
            .find(|region| region.id == BlockId::Tool(id))
            .unwrap();
        layout.lines[region.start_line..region.end_line]
            .iter()
            .map(|line| line.to_string().trim_end().to_owned())
            .collect::<Vec<_>>()
    };
    // Five indentation columns plus a two-column glyph used to make this row
    // seven wide; the indent now spends four and the glyph keeps its two.
    assert_eq!(
        tool_rows(6),
        [
            "[T✓] ▾",
            "    ❯",
            "    ec",
            "    ho",
            "    世",
            "    界",
            "    界",
            "    界",
            "     p",
            "     a",
            "     r",
            "     t",
            "     i",
            "     a",
            "     l",
        ]
    );
    assert_eq!(
        tool_rows(2),
        [
            "…▾", "❯", "ec", "ho", "世", "界", "界", "界", " p", " a", " r", " t", " i", " a",
            " l",
        ]
    );
    // The other shared wrappers carry the same guarantee, and the same
    // stand-back for their gutters: at three columns a two-column `"│ "`
    // gutter would leave a single column, so it yields and the glyph survives
    // instead of being cut to an ellipsis.
    let rows = |lines: Vec<Line<'static>>| -> Vec<String> {
        lines
            .iter()
            .map(|line| line.to_string().trim_end().to_owned())
            .collect()
    };
    assert_eq!(
        rows(prefixed_wrapped_line(
            "   ".into(),
            Style::new(),
            Line::from("世界界"),
            3
        )),
        ["世", "界", "界"]
    );
    assert_eq!(
        rows(repeated_prefixed_wrapped_line(
            vec![Span::raw("│ ")],
            Line::from("世界界"),
            3
        )),
        ["世", "界", "界"]
    );
    assert_eq!(
        rows(repeated_prefixed_hanging_line(
            vec![Span::raw("│ ")],
            Line::from("- 世界界"),
            3,
            2
        )),
        ["-", " 世", " 界", " 界"]
    );
    for width in 1u16..=8 {
        for lines in [
            prefixed_wrapped_line("   ".into(), Style::new(), Line::from("世界界"), width),
            repeated_prefixed_wrapped_line(vec![Span::raw("│ ")], Line::from("世界界"), width),
            repeated_prefixed_hanging_line(vec![Span::raw("│ ")], Line::from("- 世界界"), width, 2),
        ] {
            assert!(
                lines.iter().all(|line| line.width() <= usize::from(width)),
                "width {width}: {lines:?}"
            );
        }
    }
    // Only a row too narrow to host the glyph in any form replaces it.
    let narrow = tool_rows(1);
    assert!(narrow.iter().any(|row| row == "…"), "{narrow:?}");
    assert!(!narrow.iter().any(|row| row.contains('世')), "{narrow:?}");
    assert!(
        tool_rows(8).iter().any(|row| row == "│ 世界界"),
        "{:?}",
        tool_rows(8)
    );
}

#[test]
fn read_rows_render_book_header_and_hide_duplicate_argument_lines() {
    let mut state = read_tool_state(
        "src/main.rs",
        ToolStatus::Completed,
        &read_detail(&[(1, "fn main() {}")]),
    );
    let id = read_tool_id(&state);
    state.tools.get_mut(&id).unwrap().presentation = presentation("read", Some("src/main.rs"));
    let collapsed = transcript_layout(&state, None, 80);
    assert!(snapshot_lines(&collapsed.lines).contains("📖 ▸ Read src/main.rs"));
    let expanded = HashSet::from([BlockId::Tool(id)]);
    let layout = transcript_layout(&state, Some(&expanded), 80);
    let text = snapshot_lines(&layout.lines);
    assert!(text.contains("📖 ▾ Read src/main.rs"));
    assert!(!text.contains("arguments:"));
    assert!(!text.contains("Read file src/main.rs"));
    assert_eq!(text.matches("src/main.rs").count(), 1);
    assert!(text.contains("fn main() {}"));
    assert!(
        !layout
            .lines
            .iter()
            .flat_map(|line| &line.spans)
            .any(|span| span.content == "> ")
    );
    let copied = extract_selection(
        &layout.lines,
        (0, 0),
        (layout.lines.len(), 0),
        &Theme::default(),
    );
    assert!(copied.contains("📖 ▾ Read src/main.rs"));
}

#[tokio::test]
async fn bash_header_hover_covers_visible_header_only() {
    use ratatui::style::Modifier;

    let mut app = test_app().await;
    let session = SessionId::new_v7();
    app.selected = Some(session);
    let mut state = read_tool_state("unused", ToolStatus::Completed, &"output\n".repeat(40));
    let id = read_tool_id(&state);
    let tool = state.tools.get_mut(&id).unwrap();
    tool.presentation = presentation("bash", Some("printf a-very-long-command"));
    tool.arguments = r#"{"command":"printf a-very-long-command"}"#.into();
    app.store.sessions.insert(session, state);
    app.expanded_blocks
        .insert(session, HashSet::from([BlockId::Tool(id)]));
    for (kind, level) in [
        (ThemeKind::Default, ColorLevel::TrueColor),
        (ThemeKind::Dark, ColorLevel::TrueColor),
        (ThemeKind::Default, ColorLevel::Ansi256),
        (ThemeKind::Dark, ColorLevel::Ansi256),
        (ThemeKind::Mono, ColorLevel::None),
        (ThemeKind::HighContrast, ColorLevel::Ansi16),
    ] {
        app.theme = Theme::new(kind, level);
        app.conversation_scroll.following = false;
        app.conversation_scroll.offset = 0;
        app.hover = None;
        let mut terminal = Terminal::new(TestBackend::new(24, 24)).unwrap();
        terminal.draw(|frame| app.draw_for_test(frame)).unwrap();
        let region = *app
            .layout_cache
            .layout
            .regions
            .iter()
            .find(|region| region.id == BlockId::Tool(id))
            .unwrap();
        let header_lines = region.header_lines.unwrap();
        // The header takes exactly one row: an over-long argument truncates
        // to the row instead of wrapping a second header line.
        assert_eq!(header_lines, 1);
        for (offset, visible_header_lines) in [
            (0, header_lines),
            (region.start_line + 1, header_lines - 1),
            (region.start_line + header_lines, 0),
        ] {
            app.conversation_scroll.following = false;
            app.conversation_scroll.offset = offset;
            app.hover = None;
            terminal.draw(|frame| app.draw_for_test(frame)).unwrap();
            let before = terminal.backend().buffer().clone();
            let hit = *app
                .hit_map
                .blocks
                .iter()
                .find(|hit| hit.id == BlockId::Tool(id))
                .unwrap();
            assert_eq!(
                hit.hover_rect.map_or(0, |rect| usize::from(rect.height)),
                visible_header_lines
            );
            app.hover = app.hover_target_at(hit.rect.x, hit.rect.y);
            assert_eq!(
                app.hover,
                hit.toggle_rect
                    .map(|_| HoverTarget::TranscriptBlock(BlockId::Tool(id)))
            );
            terminal.draw(|frame| app.draw_for_test(frame)).unwrap();
            let after = terminal.backend().buffer();
            for y in hit.rect.y..hit.rect.bottom() {
                for x in hit.rect.x..hit.rect.right() {
                    // Wide glyph continuation cells are reset by the test backend.
                    if x > hit.rect.x && UnicodeWidthStr::width(after[(x - 1, y)].symbol()) > 1 {
                        continue;
                    }
                    let cell = &after[(x, y)];
                    assert!(!cell.modifier.contains(Modifier::UNDERLINED));
                    assert_eq!(cell.fg, before[(x, y)].fg);
                    if hit
                        .hover_rect
                        .is_some_and(|rect| rect.contains(ratatui::layout::Position::new(x, y)))
                    {
                        assert_eq!(
                            cell.bg,
                            app.theme.block_hover().bg.unwrap_or(before[(x, y)].bg),
                            "header at {x},{y}, offset {offset}: {cell:?}"
                        );
                    } else {
                        assert_eq!(cell, &before[(x, y)], "body changed at {x},{y}");
                    }
                }
            }
        }
    }
}

#[tokio::test]
async fn tool_rows_never_underline_across_themes_and_hover() {
    use ratatui::style::Modifier;

    let mut app = test_app().await;
    let session = SessionId::new_v7();
    app.selected = Some(session);
    let mut state = read_tool_state("src/main.rs", ToolStatus::Completed, "output");
    let id = read_tool_id(&state);
    state.tools.get_mut(&id).unwrap().arguments = r#"{"command":"true"}"#.into();
    app.store.sessions.insert(session, state);
    app.expanded_blocks
        .insert(session, HashSet::from([BlockId::Tool(id)]));
    for kind in [
        ThemeKind::Default,
        ThemeKind::Dark,
        ThemeKind::Mono,
        ThemeKind::HighContrast,
    ] {
        for level in [
            ColorLevel::None,
            ColorLevel::Ansi16,
            ColorLevel::Ansi256,
            ColorLevel::TrueColor,
        ] {
            app.theme = Theme::new(kind, level);
            for name in ["bash", "read", "write", "edit", "plugin.build"] {
                let state = app.store.sessions.get_mut(&session).unwrap();
                state.tools.get_mut(&id).unwrap().presentation =
                    presentation(name, Some("src/main.rs"));
                // The fixture changes persisted presentation directly, so rebuild its cache.
                app.layout_cache = LayoutCache::default();
                let layout = transcript_layout_with(
                    state,
                    app.expanded_blocks.get(&session),
                    80,
                    &app.theme,
                    &PlainHighlighter,
                );
                let region = layout
                    .regions
                    .iter()
                    .find(|region| region.id == BlockId::Tool(id))
                    .unwrap();
                for line in &layout.lines[region.start_line..region.end_line] {
                    assert!(!line.style.add_modifier.contains(Modifier::UNDERLINED));
                    assert!(
                        line.spans
                            .iter()
                            .all(|span| !span.style.add_modifier.contains(Modifier::UNDERLINED))
                    );
                }
                for hovered in [false, true] {
                    app.hover = hovered.then_some(HoverTarget::TranscriptBlock(BlockId::Tool(id)));
                    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
                    terminal.draw(|frame| app.draw_for_test(frame)).unwrap();
                    let hit = app
                        .hit_map
                        .blocks
                        .iter()
                        .find(|hit| hit.id == BlockId::Tool(id))
                        .unwrap();
                    assert_eq!(
                        app.hover_target_at(hit.rect.x, hit.rect.y),
                        Some(HoverTarget::TranscriptBlock(BlockId::Tool(id)))
                    );
                    for y in hit.rect.y..hit.rect.bottom() {
                        for x in hit.rect.x..hit.rect.right() {
                            assert!(
                                !terminal.backend().buffer()[(x, y)]
                                    .modifier
                                    .contains(Modifier::UNDERLINED),
                                "{kind:?} {level:?} {name} hover={hovered}"
                            );
                        }
                    }
                    if name == "bash"
                        && let Some(bg) = app.theme.terminal_background()
                    {
                        assert_eq!(
                            terminal.backend().buffer()[(hit.rect.x + 2, hit.rect.y + 1)].bg,
                            bg
                        );
                    }
                }
            }
        }
    }
}
