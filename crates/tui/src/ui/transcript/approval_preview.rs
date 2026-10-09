//! The operation preview an approval panel shows for the call it gates.

use super::*;
use ratatui::style::Color;

/// Bytes of one previewed call the approval panel renders. The panel
/// scrolls, so this only bounds pathological arguments.
const MAX_PREVIEW_BYTES: usize = 64 * 1024;

/// Source rows of one previewed call before the rest is summarised.
const MAX_PREVIEW_ROWS: usize = 400;

/// One row of an approval preview, already fitted to the panel width;
/// `band` paints the whole row behind its text.
pub(in crate::ui) struct ApprovalPreviewRow {
    pub(in crate::ui) line: Line<'static>,
    pub(in crate::ui) band: Option<Color>,
}

/// The transcript's icon for a tool title, so the approval panel names a
/// call the way its transcript row does.
pub(in crate::ui) fn approval_tool_icon(title: &str) -> &'static str {
    tool_icon(title)
}

/// The call an approval gates, as the user has to judge it: bash shows its
/// whole command with its own line breaks, edit and write their diff. Other
/// tools have nothing richer than the panel's resource list, so they get
/// `None`. Rows hard-wrap to `width` rather than truncate: whatever runs
/// must be visible.
pub(in crate::ui) fn approval_operation_preview(
    tool: &crate::state::ToolCallState,
    width: u16,
    theme: &Theme,
) -> Option<Vec<ApprovalPreviewRow>> {
    let width = usize::from(width);
    if width < 8 {
        return None;
    }
    let arguments = ParsedToolArguments::parse(&tool.arguments);
    match tool.presentation.title.as_str() {
        "bash" => arguments
            .as_ref()
            .and_then(|arguments| arguments.command.as_deref())
            .map(|command| command_rows(command, width, theme)),
        "edit" | "write" => {
            tool_diff(tool, arguments.as_ref()).map(|diff| diff_rows(&diff, width, theme))
        }
        _ => None,
    }
}

/// A shell command on the code band: a muted `$` prompt, continuation rows
/// hung under the command text.
fn command_rows(command: &str, width: usize, theme: &Theme) -> Vec<ApprovalPreviewRow> {
    let band = theme.code_background();
    let (command, complete) = sanitized_display_prefix_lines(command);
    let mut rows = Vec::new();
    let source_lines = command
        .trim_end_matches('\n')
        .split('\n')
        .collect::<Vec<_>>();
    let shown = source_lines.len().min(MAX_PREVIEW_ROWS);
    for (index, line) in source_lines.iter().take(shown).enumerate() {
        let lead = if index == 0 { "$ " } else { "  " };
        for (piece_index, piece) in hard_wrap(line, width.saturating_sub(3))
            .into_iter()
            .enumerate()
        {
            let gutter = if piece_index == 0 { lead } else { "  " };
            rows.push(ApprovalPreviewRow {
                line: Line::from(vec![
                    Span::raw(" "),
                    Span::styled(gutter, theme.code_gutter()),
                    Span::styled(piece, theme.body()),
                ]),
                band,
            });
        }
    }
    push_elision(
        &mut rows,
        source_lines.len() - shown,
        !complete,
        band,
        theme,
    );
    rows
}

/// An edit or write as numbered `+`/`-` rows on their diff tints, with
/// wrapped rows hung under the text column.
fn diff_rows(diff: &ToolDiff<'_>, width: usize, theme: &Theme) -> Vec<ApprovalPreviewRow> {
    let band = theme.code_background();
    let mut source = Vec::new();
    let mut total = 0;
    let mut bytes = 0;
    for_each_diff_row(diff, |row| {
        // Metadata is the call's own detail text (model-call identities at
        // this point), not part of the change.
        if row.kind == DiffRowKind::Metadata {
            return true;
        }
        total += 1;
        if source.len() < MAX_PREVIEW_ROWS && bytes < MAX_PREVIEW_BYTES {
            let (text, _) = sanitized_display_prefix(&row.text, MAX_PREVIEW_BYTES - bytes);
            bytes += text.len();
            source.push((row.kind, text));
        }
        true
    });
    let number_width = source
        .iter()
        .filter_map(|(kind, _)| match kind {
            DiffRowKind::Added(number)
            | DiffRowKind::Removed(number)
            | DiffRowKind::Context(number) => Some(*number),
            _ => None,
        })
        .max()
        .unwrap_or(1)
        .max(1)
        .ilog10() as usize
        + 1;
    let mut rows = Vec::new();
    for (kind, text) in &source {
        let (gutter, marker_style, text_style, tint) = match kind {
            DiffRowKind::Added(number) => (
                format!("{number:>number_width$} + "),
                theme.diff_added(),
                theme.body(),
                theme.diff_added_background(),
            ),
            DiffRowKind::Removed(number) => (
                format!("{number:>number_width$} - "),
                theme.diff_removed(),
                theme.body(),
                theme.diff_removed_background(),
            ),
            DiffRowKind::Context(number) => (
                format!("{number:>number_width$}   "),
                theme.code_gutter(),
                theme.body(),
                None,
            ),
            DiffRowKind::Hunk | DiffRowKind::NoNewline | DiffRowKind::Metadata => (
                String::new(),
                theme.code_gutter(),
                if *kind == DiffRowKind::Hunk {
                    theme.diff_hunk()
                } else {
                    theme.code_gutter()
                },
                None,
            ),
        };
        let gutter_width = UnicodeWidthStr::width(gutter.as_str());
        let room = width.saturating_sub(gutter_width + 2).max(1);
        for (index, piece) in hard_wrap(text, room).into_iter().enumerate() {
            let gutter = if index == 0 {
                gutter.clone()
            } else {
                " ".repeat(gutter_width)
            };
            rows.push(ApprovalPreviewRow {
                line: Line::from(vec![
                    Span::raw(" "),
                    Span::styled(gutter, marker_style),
                    Span::styled(piece, text_style),
                ]),
                band: tint.or(band),
            });
        }
    }
    push_elision(&mut rows, total - source.len(), false, band, theme);
    rows
}

fn push_elision(
    rows: &mut Vec<ApprovalPreviewRow>,
    omitted_rows: usize,
    truncated: bool,
    band: Option<Color>,
    theme: &Theme,
) {
    let notice = match (omitted_rows, truncated) {
        (0, false) => return,
        (0, true) => " … the rest of this line is not shown".to_owned(),
        (1, _) => " … 1 more line not shown".to_owned(),
        (count, _) => format!(" … {count} more lines not shown"),
    };
    rows.push(ApprovalPreviewRow {
        line: Line::styled(notice, theme.warning()),
        band,
    });
}

/// The command text, control-sanitized line by line within the preview byte
/// budget; `false` when the budget cut it short.
fn sanitized_display_prefix_lines(text: &str) -> (String, bool) {
    let mut out = String::new();
    let mut remaining = MAX_PREVIEW_BYTES;
    for (index, line) in text.split('\n').enumerate() {
        if index > 0 {
            if remaining == 0 {
                return (out, false);
            }
            out.push('\n');
            remaining -= 1;
        }
        let (sanitized, complete) = sanitized_display_prefix(line, remaining);
        remaining -= sanitized.len();
        out.push_str(&sanitized);
        if !complete {
            return (out, false);
        }
    }
    (out, true)
}

/// Grapheme-safe hard wrap: code keeps its exact spacing, so it breaks at
/// the column rather than at words. An empty line stays one empty row.
fn hard_wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut rows = vec![String::new()];
    let mut used = 0;
    for grapheme in text.graphemes(true) {
        let grapheme_width = UnicodeWidthStr::width(grapheme);
        if used + grapheme_width > width && used > 0 {
            rows.push(String::new());
            used = 0;
        }
        rows.last_mut().expect("one row").push_str(grapheme);
        used += grapheme_width;
    }
    rows
}
