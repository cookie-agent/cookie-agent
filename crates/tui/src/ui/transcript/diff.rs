//! Diff rows and their rendering: a `diff` block's hunks, or the unified
//! diff text of its loaded full content.

use super::*;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum DiffRowKind {
    Hunk,
    Added(u64),
    Removed(u64),
    Context(u64),
    NoNewline,
    Metadata,
}

pub(super) struct DiffRow<'a> {
    pub(super) kind: DiffRowKind,
    pub(super) text: Cow<'a, str>,
}

pub(super) fn parse_hunk_starts(line: &str) -> Option<(u64, u64)> {
    let ranges = line.strip_prefix("@@ -")?.split_once(" @@")?.0;
    let (old, new) = ranges.split_once(" +")?;
    let start = |range: &str| range.split(',').next()?.parse::<u64>().ok();
    Some((start(old)?, start(new)?))
}

pub(super) fn for_each_unified_diff_row<'a>(
    text: &'a str,
    mut visit: impl FnMut(DiffRow<'a>) -> bool,
) {
    let mut old_line = 0;
    let mut new_line = 0;
    let mut in_hunk = false;
    for line in text.lines() {
        let row = if let Some((old_start, new_start)) = parse_hunk_starts(line) {
            old_line = old_start;
            new_line = new_start;
            in_hunk = true;
            DiffRow {
                kind: DiffRowKind::Hunk,
                text: Cow::Borrowed(line),
            }
        } else if in_hunk && line.starts_with('+') && !line.starts_with("+++") {
            let row = DiffRow {
                kind: DiffRowKind::Added(new_line),
                text: Cow::Borrowed(&line[1..]),
            };
            new_line += 1;
            row
        } else if in_hunk && line.starts_with('-') && !line.starts_with("---") {
            let row = DiffRow {
                kind: DiffRowKind::Removed(old_line),
                text: Cow::Borrowed(&line[1..]),
            };
            old_line += 1;
            row
        } else if in_hunk && let Some(content) = line.strip_prefix(' ') {
            let row = DiffRow {
                kind: DiffRowKind::Context(new_line),
                text: Cow::Borrowed(content),
            };
            old_line += 1;
            new_line += 1;
            row
        } else if line == "\\ No newline at end of file" {
            DiffRow {
                kind: DiffRowKind::NoNewline,
                text: Cow::Borrowed(line),
            }
        } else {
            DiffRow {
                kind: DiffRowKind::Metadata,
                text: Cow::Borrowed(line),
            }
        };
        if !visit(row) {
            break;
        }
    }
}

pub(super) struct RenderedDiffRow {
    pub(super) kind: DiffRowKind,
    pub(super) text: String,
}

/// Diff rows behind a gutter of file line numbers and `+`/`-` markers,
/// added and removed rows on their tints, highlighted in `language` when
/// it is known.
pub(super) fn diff_body_lines(
    rows: Vec<RenderedDiffRow>,
    language: Option<&str>,
    style: &BlockStyle<'_>,
) -> Vec<ToolBodyLine> {
    let theme = style.theme;
    let max_number = rows
        .iter()
        .filter_map(|row| match row.kind {
            DiffRowKind::Added(number)
            | DiffRowKind::Removed(number)
            | DiffRowKind::Context(number) => Some(number),
            DiffRowKind::Hunk | DiffRowKind::NoNewline | DiffRowKind::Metadata => None,
        })
        .max()
        .unwrap_or(1);
    let number_width = max_number.max(1).ilog10() as usize + 1;
    let code_source = rows
        .iter()
        .filter_map(|row| match row.kind {
            DiffRowKind::Added(_) | DiffRowKind::Removed(_) | DiffRowKind::Context(_) => {
                Some(row.text.as_str())
            }
            DiffRowKind::Hunk | DiffRowKind::NoNewline | DiffRowKind::Metadata => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    let highlighted = language.map(|language| {
        style.highlighter.highlight_stable(
            &crate::markdown::normalized_language(language),
            &code_source,
            theme,
        )
    });
    let mut highlight_index = 0;
    let mut output = Vec::new();
    for row in rows {
        match row.kind {
            DiffRowKind::Hunk => output.push(ToolBodyLine::code(Line::from(Span::styled(
                row.text,
                theme.diff_hunk(),
            )))),
            DiffRowKind::NoNewline | DiffRowKind::Metadata => {
                output.push(ToolBodyLine::code(Line::from(Span::styled(
                    row.text,
                    theme.code_gutter(),
                ))));
            }
            DiffRowKind::Added(number)
            | DiffRowKind::Removed(number)
            | DiffRowKind::Context(number) => {
                let (marker, marker_style, tint) = match row.kind {
                    DiffRowKind::Added(_) => {
                        ("+", theme.diff_added(), theme.diff_added_background())
                    }
                    DiffRowKind::Removed(_) => {
                        ("-", theme.diff_removed(), theme.diff_removed_background())
                    }
                    DiffRowKind::Context(_) => (" ", theme.code_gutter(), None),
                    DiffRowKind::Hunk | DiffRowKind::NoNewline | DiffRowKind::Metadata => {
                        unreachable!()
                    }
                };
                let line = highlighted
                    .as_ref()
                    .and_then(|lines| lines.get(highlight_index))
                    .cloned()
                    .unwrap_or_else(|| Line::from(row.text));
                highlight_index += 1;
                output.push(
                    ToolBodyLine::guttered_code(
                        line,
                        vec![
                            Span::styled(format!("{number:>number_width$}"), theme.code_gutter()),
                            Span::styled(format!(" {marker} │ "), marker_style),
                        ],
                        vec![
                            Span::styled(" ".repeat(number_width), theme.code_gutter()),
                            Span::styled("   │ ", marker_style),
                        ],
                    )
                    .with_tint(tint),
                );
            }
        }
    }
    output
}
