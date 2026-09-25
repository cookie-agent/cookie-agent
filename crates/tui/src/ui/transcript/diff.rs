//! Unified and structured diff rows and their rendering.

use super::*;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum DiffRowKind {
    Hunk,
    Added(usize),
    Removed(usize),
    Context(usize),
    NoNewline,
    Metadata,
}

pub(super) struct DiffRow<'a> {
    pub(super) kind: DiffRowKind,
    pub(super) text: Cow<'a, str>,
}

pub(super) enum ToolDiff<'a> {
    Unified(&'a str),
    Edit {
        before: &'a str,
        after: &'a str,
        metadata: &'a str,
    },
    Write {
        content: &'a str,
        metadata: &'a str,
    },
}

pub(super) fn tool_diff<'a>(
    tool: &'a crate::state::ToolCallState,
    arguments: Option<&'a ParsedToolArguments<'_>>,
) -> Option<ToolDiff<'a>> {
    if is_unified_diff(&tool.detail) {
        return Some(ToolDiff::Unified(&tool.detail));
    }
    match tool.presentation.title.as_str() {
        "edit" => {
            let arguments = arguments?;
            Some(ToolDiff::Edit {
                before: arguments.before.as_deref()?,
                after: arguments.after.as_deref()?,
                metadata: &tool.detail,
            })
        }
        "write" => {
            let arguments = arguments?;
            Some(ToolDiff::Write {
                content: arguments.content.as_deref()?,
                metadata: &tool.detail,
            })
        }
        _ => None,
    }
}

pub(super) fn is_unified_diff(text: &str) -> bool {
    let mut has_file_header = false;
    for line in text.lines() {
        has_file_header |= line.starts_with("diff --git ");
        if parse_hunk_starts(line).is_some()
            || has_file_header && (line.starts_with("Binary files ") || line == "GIT binary patch")
        {
            return true;
        }
    }
    false
}

pub(super) fn parse_hunk_starts(line: &str) -> Option<(usize, usize)> {
    let ranges = line.strip_prefix("@@ -")?.split_once(" @@")?.0;
    let (old, new) = ranges.split_once(" +")?;
    let start = |range: &str| range.split(',').next()?.parse::<usize>().ok();
    Some((start(old)?, start(new)?))
}

pub(super) fn diff_range(count: usize) -> String {
    match count {
        0 => "0,0".to_owned(),
        1 => "1".to_owned(),
        count => format!("1,{count}"),
    }
}

pub(super) fn for_each_diff_row<'a>(
    diff: &'a ToolDiff<'a>,
    mut visit: impl FnMut(DiffRow<'a>) -> bool,
) {
    match diff {
        ToolDiff::Unified(text) => for_each_unified_diff_row(text, visit),
        ToolDiff::Edit {
            before,
            after,
            metadata,
        } => {
            let before_count = before.lines().count();
            let after_count = after.lines().count();
            if !visit(DiffRow {
                kind: DiffRowKind::Hunk,
                text: Cow::Owned(format!(
                    "@@ -{} +{} @@",
                    diff_range(before_count),
                    diff_range(after_count)
                )),
            }) {
                return;
            }
            for (index, text) in before.lines().enumerate() {
                if !visit(DiffRow {
                    kind: DiffRowKind::Removed(index + 1),
                    text: Cow::Borrowed(text),
                }) {
                    return;
                }
            }
            if !before.is_empty()
                && !before.ends_with('\n')
                && !visit(DiffRow {
                    kind: DiffRowKind::NoNewline,
                    text: Cow::Borrowed("\\ No newline at end of file"),
                })
            {
                return;
            }
            for (index, text) in after.lines().enumerate() {
                if !visit(DiffRow {
                    kind: DiffRowKind::Added(index + 1),
                    text: Cow::Borrowed(text),
                }) {
                    return;
                }
            }
            if !after.is_empty()
                && !after.ends_with('\n')
                && !visit(DiffRow {
                    kind: DiffRowKind::NoNewline,
                    text: Cow::Borrowed("\\ No newline at end of file"),
                })
            {
                return;
            }
            for line in metadata.lines() {
                if !visit(DiffRow {
                    kind: DiffRowKind::Metadata,
                    text: Cow::Borrowed(line),
                }) {
                    return;
                }
            }
        }
        ToolDiff::Write { content, metadata } => {
            let count = content.lines().count();
            if !visit(DiffRow {
                kind: DiffRowKind::Hunk,
                text: Cow::Owned(format!("@@ -0,0 +{} @@", diff_range(count))),
            }) {
                return;
            }
            for (index, text) in content.lines().enumerate() {
                if !visit(DiffRow {
                    kind: DiffRowKind::Added(index + 1),
                    text: Cow::Borrowed(text),
                }) {
                    return;
                }
            }
            if !content.is_empty()
                && !content.ends_with('\n')
                && !visit(DiffRow {
                    kind: DiffRowKind::NoNewline,
                    text: Cow::Borrowed("\\ No newline at end of file"),
                })
            {
                return;
            }
            for line in metadata.lines() {
                if !visit(DiffRow {
                    kind: DiffRowKind::Metadata,
                    text: Cow::Borrowed(line),
                }) {
                    return;
                }
            }
        }
    }
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

pub(super) fn diff_total_lines(diff: &ToolDiff<'_>) -> usize {
    match diff {
        ToolDiff::Unified(text) => text.lines().count(),
        ToolDiff::Edit {
            before,
            after,
            metadata,
        } => {
            1 + before.lines().count()
                + usize::from(!before.is_empty() && !before.ends_with('\n'))
                + after.lines().count()
                + usize::from(!after.is_empty() && !after.ends_with('\n'))
                + metadata.lines().count()
        }
        ToolDiff::Write { content, metadata } => {
            1 + content.lines().count()
                + usize::from(!content.is_empty() && !content.ends_with('\n'))
                + metadata.lines().count()
        }
    }
}

pub(super) fn render_diff_output(
    diff: &ToolDiff<'_>,
    language: Option<&str>,
    section: ToolOutputSection,
    expanded: bool,
    budget: &mut RenderBudget,
    future_sections: usize,
    context: &TranscriptRenderContext<'_>,
) -> Vec<ToolBodyLine> {
    let total_lines = diff_total_lines(diff);
    let mut renderer = SectionRenderer::new(budget, expanded, future_sections);
    let mut rows = Vec::new();
    for_each_diff_row(diff, |row| {
        if renderer.exhausted() {
            return false;
        }
        let Some(text) = renderer.take(row.text.as_ref()) else {
            return false;
        };
        rows.push(RenderedDiffRow {
            kind: row.kind,
            text,
        });
        true
    });
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
        context.highlighter.highlight_stable(
            &crate::markdown::normalized_language(language),
            &code_source,
            context.theme,
        )
    });
    let mut highlight_index = 0;
    let mut output = Vec::new();
    for row in rows {
        match row.kind {
            DiffRowKind::Hunk => output.push(ToolBodyLine::code(Line::from(Span::styled(
                row.text,
                context.theme.diff_hunk(),
            )))),
            DiffRowKind::NoNewline | DiffRowKind::Metadata => {
                output.push(ToolBodyLine::code(Line::from(Span::styled(
                    row.text,
                    context.theme.code_gutter(),
                ))));
            }
            DiffRowKind::Added(number)
            | DiffRowKind::Removed(number)
            | DiffRowKind::Context(number) => {
                let (marker, marker_style, tint) = match row.kind {
                    DiffRowKind::Added(_) => (
                        "+",
                        context.theme.diff_added(),
                        context.theme.diff_added_background(),
                    ),
                    DiffRowKind::Removed(_) => (
                        "-",
                        context.theme.diff_removed(),
                        context.theme.diff_removed_background(),
                    ),
                    DiffRowKind::Context(_) => (" ", context.theme.code_gutter(), None),
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
                            Span::styled(
                                format!("{number:>number_width$}"),
                                context.theme.code_gutter(),
                            ),
                            Span::styled(format!(" {marker} │ "), marker_style),
                        ],
                        vec![
                            Span::styled(" ".repeat(number_width), context.theme.code_gutter()),
                            Span::styled("   │ ", marker_style),
                        ],
                    )
                    .with_tint(tint),
                );
            }
        }
    }
    let omitted = total_lines.saturating_sub(renderer.fully_rendered);
    append_output_notice(
        &mut output,
        omitted,
        expanded,
        section,
        renderer.budget,
        context.theme,
    );
    output
}
