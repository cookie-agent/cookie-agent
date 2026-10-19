//! Tool output limits and the sanitizing they share.

use super::*;

pub(super) fn display_line_count(text: &str) -> usize {
    text.split('\n').count()
}

pub(super) const MAX_EXPANDED_BODY_LINES: usize = 64;

pub(super) const MAX_EXPANDED_BODY_BYTES: usize = 8 * 1024;

pub(super) const MAX_EXPANDED_TOOL_OUTPUT_LINES: usize = 1024;

pub(super) const MAX_EXPANDED_TOOL_OUTPUT_BYTES: usize = 128 * 1024;

pub(super) const MAX_SYSTEM_PROMPT_BODY_LINES: usize = 256;

pub(super) const MAX_SYSTEM_PROMPT_BODY_BYTES: usize = 32 * 1024;

#[derive(Clone, Copy)]
pub(super) struct RenderLimits {
    pub(super) lines: usize,
    pub(super) bytes: usize,
}

pub(super) const COLLAPSED_TOOL_OUTPUT_LIMITS: RenderLimits = RenderLimits {
    lines: MAX_EXPANDED_BODY_LINES,
    bytes: MAX_EXPANDED_BODY_BYTES,
};

pub(super) const EXPANDED_TOOL_OUTPUT_LIMITS: RenderLimits = RenderLimits {
    lines: MAX_EXPANDED_TOOL_OUTPUT_LINES,
    bytes: MAX_EXPANDED_TOOL_OUTPUT_BYTES,
};

pub(super) fn bounded_safe_display_text(
    text: &str,
    style: Style,
    max_lines: usize,
    max_bytes: usize,
) -> Vec<Line<'static>> {
    bounded_safe_display_lines(text.split('\n'), style, max_lines, max_bytes)
}

pub(super) fn bounded_safe_display_lines<'a>(
    lines: impl IntoIterator<Item = &'a str>,
    style: Style,
    max_lines: usize,
    max_bytes: usize,
) -> Vec<Line<'static>> {
    let mut rendered = Vec::new();
    let mut rendered_bytes = 0;
    let mut fully_rendered_lines = 0usize;
    let mut total_lines = 0usize;

    for line in lines {
        total_lines += 1;
        if rendered.len() >= max_lines || rendered_bytes >= max_bytes {
            continue;
        }
        let available = max_bytes - rendered_bytes;
        let (sanitized, complete) = sanitized_display_prefix(line, available);
        if complete {
            rendered_bytes += sanitized.len();
            rendered.push(Line::styled(sanitized, style));
            fully_rendered_lines += 1;
            continue;
        }

        if !sanitized.is_empty() {
            rendered.push(Line::styled(sanitized, style));
        }
        rendered_bytes = max_bytes;
    }

    let omitted_lines = total_lines.saturating_sub(fully_rendered_lines);
    if omitted_lines > 0 {
        rendered.push(Line::styled(
            format!("… truncated ({omitted_lines} more lines)"),
            style,
        ));
    }
    rendered
}

pub(super) fn safe_display_text(text: &str) -> String {
    sanitized_display_prefix(text, usize::MAX).0
}

/// Control characters become U+FFFD and tabs expand to the next tab stop
/// (see [`crate::markdown::expand_tabs_in_spans`]), so measured and drawn
/// widths agree. `max_bytes` bounds the expanded output.
pub(super) fn sanitized_display_prefix(text: &str, max_bytes: usize) -> (String, bool) {
    let mut sanitized = String::with_capacity(text.len().min(max_bytes));
    let mut column = 0;
    for character in text.chars() {
        if character == '\t' {
            let advance = crate::markdown::tab_advance(column);
            if sanitized.len().saturating_add(advance) > max_bytes {
                return (sanitized, false);
            }
            sanitized.extend(std::iter::repeat_n(' ', advance));
            column += advance;
            continue;
        }
        let character = if character.is_control() {
            '\u{FFFD}'
        } else {
            character
        };
        if sanitized.len().saturating_add(character.len_utf8()) > max_bytes {
            return (sanitized, false);
        }
        sanitized.push(character);
        column += unicode_width::UnicodeWidthChar::width(character).unwrap_or(0);
    }
    (sanitized, true)
}

pub(super) fn path_extension(path: &str) -> Option<&str> {
    let name = path.rsplit(['/', '\\']).next()?;
    name.rsplit_once('.')
        .filter(|(stem, extension)| !stem.is_empty() && !extension.is_empty())
        .map(|(_, extension)| extension)
}

pub(super) fn append_output_notice(
    output: &mut Vec<ToolBodyLine>,
    omitted: usize,
    expanded: bool,
    section: ToolOutputSection,
    budget: &mut RenderBudget,
    theme: &Theme,
) {
    let notice = if omitted > 0 && expanded {
        format!("… {omitted} more lines (maximum shown; click to collapse)")
    } else if omitted > 0 {
        format!("… {omitted} more lines (click to expand)")
    } else if expanded {
        "▴ click to collapse".to_owned()
    } else {
        return;
    };
    if budget.consume_notice(&notice) {
        output.push(ToolBodyLine::toggle(
            Line::from(Span::styled(notice, theme.muted())),
            section,
        ));
    }
}
