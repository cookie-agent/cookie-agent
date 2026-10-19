//! What an approval panel shows of the call it gates: the call's argument
//! blocks and its prepare-time preview (spec 12.2), drawn as the transcript
//! draws display blocks.

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

/// The call an approval gates, as the user has to judge it: its argument
/// blocks (a command, a path) and its preview (an edit's diff as of
/// prepare), one after the other. `None` when the presentation has
/// neither, so the panel lists the request's resources instead. Rows wrap
/// to `width` rather than truncate: whatever runs must be visible.
pub(in crate::ui) fn approval_presentation_rows(
    tool: &crate::state::ToolCallState,
    width: u16,
    theme: &Theme,
    highlighter: &dyn crate::markdown::Highlighter,
) -> Option<Vec<ApprovalPreviewRow>> {
    if width < 8 {
        return None;
    }
    let presentation = &tool.presentation;
    if presentation.arguments.is_empty() && presentation.preview.is_empty() {
        return None;
    }
    let limits = RenderLimits {
        lines: MAX_PREVIEW_ROWS,
        bytes: MAX_PREVIEW_BYTES,
    };
    let mut budget = RenderBudget::new(limits);
    let mut sink = LineSink::new(&mut budget, limits);
    let style = BlockStyle {
        theme,
        highlighter,
        // One column of margin, one of slack before the panel's edge.
        width: width.saturating_sub(2).max(1),
    };
    let mut lines = display_rows(&presentation.arguments, &[], &style, &mut sink);
    let preview = display_rows(&presentation.preview, &[], &style, &mut sink);
    if !lines.is_empty() && !preview.is_empty() {
        lines.push(ToolBodyLine::wrapped(Line::default()));
    }
    lines.extend(preview);
    let omitted = sink.omitted();
    let band = theme.code_background();
    let mut rows = lines
        .into_iter()
        .flat_map(|line| preview_rows(line, width, band))
        .collect::<Vec<_>>();
    push_elision(&mut rows, omitted, band, theme);
    Some(rows)
}

/// One display row hard- or word-wrapped to `width`, behind a one-column
/// margin, on its tint or the code band.
fn preview_rows(line: ToolBodyLine, width: u16, band: Option<Color>) -> Vec<ApprovalPreviewRow> {
    let band = line.tint.or(band);
    let room = width.saturating_sub(1).max(1);
    let margin = || vec![Span::raw(" ")];
    let rows = match line.kind {
        ToolBodyLineKind::Wrapped => repeated_prefixed_wrapped_line(margin(), line.line, width),
        ToolBodyLineKind::Hanging { indent } => {
            repeated_prefixed_hanging_line(margin(), line.line, width, indent)
        }
        ToolBodyLineKind::Code {
            mut first_gutter,
            mut continuation_gutter,
        } => {
            let gutter_width = |spans: &[Span<'_>]| {
                spans
                    .iter()
                    .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
                    .sum::<usize>()
            };
            let available = usize::from(room);
            if gutter_width(&first_gutter) >= available
                || gutter_width(&continuation_gutter) >= available
            {
                first_gutter.clear();
                continuation_gutter.clear();
            }
            crate::markdown::wrap_code_spans(
                line.line.spans,
                available.saturating_sub(gutter_width(&first_gutter)).max(1),
                available
                    .saturating_sub(gutter_width(&continuation_gutter))
                    .max(1),
            )
            .into_iter()
            .enumerate()
            .map(|(index, content)| {
                let mut spans = margin();
                spans.extend(if index == 0 {
                    first_gutter.clone()
                } else {
                    continuation_gutter.clone()
                });
                spans.extend(content);
                Line::from(spans)
            })
            .collect()
        }
    };
    rows.into_iter()
        .map(|line| ApprovalPreviewRow { line, band })
        .collect()
}

fn push_elision(
    rows: &mut Vec<ApprovalPreviewRow>,
    omitted_rows: usize,
    band: Option<Color>,
    theme: &Theme,
) {
    let notice = match omitted_rows {
        0 => return,
        1 => " … 1 more line not shown".to_owned(),
        count => format!(" … {count} more lines not shown"),
    };
    rows.push(ApprovalPreviewRow {
        line: Line::styled(notice, theme.warning()),
        band,
    });
}
