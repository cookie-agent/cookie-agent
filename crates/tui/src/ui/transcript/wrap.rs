//! Line wrapping, gutters, and selection extraction primitives.

use super::*;

pub(super) fn prefixed_unwrapped_line(
    mut prefix: Vec<Span<'static>>,
    line: Line<'static>,
    width: u16,
) -> Line<'static> {
    let width = usize::from(width.max(1));
    let prefix_width = prefix
        .iter()
        .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
        .sum::<usize>();
    if prefix_width >= width {
        prefix.clear();
    }
    let line_style = line.style;
    prefix.extend(line.spans);
    Line::from(prefix).style(line_style)
}

pub(super) fn repeated_prefixed_hanging_line(
    mut prefix: Vec<Span<'static>>,
    line: Line<'static>,
    width: u16,
    continuation_indent: usize,
) -> Vec<Line<'static>> {
    let width = usize::from(width.max(1));
    let unbreakable = unbreakable_columns(&line);
    let mut prefix_width = prefix
        .iter()
        .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
        .sum::<usize>();
    // The gutter is indentation too, so it stands back until the row can host the
    // widest grapheme that has to sit on it whole — and the inner budget is then
    // measured against the columns the row actually has left.
    if prefix_width + unbreakable > width {
        prefix.clear();
        prefix_width = 0;
    }
    let inner_width = width.saturating_sub(prefix_width).max(1);
    let continuation_indent = continuation_indent
        .min(inner_width.saturating_sub(1))
        .min(inner_width.saturating_sub(unbreakable));
    let line_style = line.style;
    let (first_prefix, content) = split_spans_at_width(line.spans, continuation_indent);
    let mut wrapped = wrapped_line(
        Line::from(content).style(line_style),
        u16::try_from(inner_width.saturating_sub(continuation_indent).max(1)).unwrap_or(u16::MAX),
    );
    for (index, line) in wrapped.iter_mut().enumerate() {
        let mut spans = prefix.clone();
        if index == 0 {
            spans.extend(first_prefix.clone());
        } else if continuation_indent > 0 {
            spans.push(Span::raw(" ".repeat(continuation_indent)));
        }
        spans.append(&mut line.spans);
        line.spans = spans;
    }
    wrapped
}

pub(super) fn split_spans_at_width(
    spans: Vec<Span<'static>>,
    width: usize,
) -> (Vec<Span<'static>>, Vec<Span<'static>>) {
    let mut prefix = Vec::new();
    let mut content = Vec::new();
    let mut consumed = 0;
    for span in spans {
        for grapheme in span.content.graphemes(true) {
            let target = if consumed < width {
                consumed += UnicodeWidthStr::width(grapheme);
                &mut prefix
            } else {
                &mut content
            };
            append_span(target, grapheme.to_owned(), span.style);
        }
    }
    (prefix, content)
}

pub(super) fn role_block(
    role: Role,
    body: Vec<Line<'static>>,
    width: u16,
    theme: &Theme,
) -> Vec<Line<'static>> {
    role_block_lines(role, body, width, theme)
}

pub(super) fn role_block_lines(
    role: Role,
    body: Vec<Line<'static>>,
    width: u16,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let diagnostic = matches!(
        role,
        Role::Debug | Role::Internal | Role::Warning | Role::Error
    );
    let (label, marker, gutter, style) = match role {
        Role::User => ("USER", "┌─", "│ ", theme.user()),
        Role::Action => ("ACTION", "--", "│ ", theme.user()),
        Role::Goal => ("GOAL", "◆─", "│ ", theme.assistant()),
        Role::ToolRunning => ("TOOL RUNNING", "┏…", "┃ ", theme.tool_running()),
        Role::ToolSuccess => ("TOOL SUCCESS", "┏✓", "┃ ", theme.tool_success()),
        Role::ToolFailure => ("TOOL FAILURE", "┏!", "┃ ", theme.tool_failure()),
        Role::Debug => ("DEBUG [D]", "··", "· ", theme.muted()),
        // VS16 makes emoji terminals and the width table agree on U+26A0: two cells.
        Role::Warning => ("WARNING [W]", "⚠️─", "│ ", theme.warning()),
        Role::Error => ("ERROR [E]", "!!", "! ", theme.error()),
        Role::Internal => ("EVENT [I]", "--", "· ", theme.internal()),
    };
    if matches!(role, Role::User | Role::Goal | Role::Action) {
        if width == 0 {
            return Vec::new();
        }
        let header = if width < 8 {
            format!(
                "[{}]",
                match role {
                    Role::Goal => "G",
                    Role::Action => "A",
                    Role::User => "U",
                    _ => "P",
                }
            )
        } else {
            format!("{marker} {label}")
        };
        let mut lines = wrapped_line(Line::styled(header, style), width);
        for line in body {
            lines.extend(repeated_prefixed_wrapped_line(
                vec![Span::styled(gutter, style)],
                line,
                width,
            ));
        }
        return lines;
    }
    if width < 8 {
        let short = match role {
            Role::User => "U",
            Role::Action => "A",
            Role::Goal => "G",
            Role::ToolRunning => "T…",
            Role::ToolSuccess => "T✓",
            Role::ToolFailure => "T!",
            Role::Debug => "D",
            Role::Warning => "W",
            Role::Error => "E",
            Role::Internal => "I",
        };
        if diagnostic {
            if width == 0 {
                return Vec::new();
            }
            if body.len() == 1 && body[0].width() + 4 <= usize::from(width) {
                return repeated_prefixed_wrapped_line(
                    vec![Span::styled(format!("[{short}] "), style)],
                    body.into_iter().next().expect("one diagnostic line"),
                    width,
                );
            }
            let mut lines = wrapped_line(Line::styled(format!("[{short}]"), style), width);
            for line in body {
                lines.extend(repeated_prefixed_wrapped_line(
                    vec![Span::styled(gutter, style)],
                    line,
                    width,
                ));
            }
            return lines;
        }
        let mut lines = Vec::new();
        for (index, line) in body.into_iter().enumerate() {
            let prefix = if index == 0 {
                format!("[{short}] ")
            } else {
                "    ".into()
            };
            lines.extend(prefixed_wrapped_line(prefix, style, line, width));
        }
        return lines;
    }
    let mut lines = if matches!(role, Role::Internal) {
        Vec::new()
    } else {
        wrapped_line(
            Line::from(vec![
                Span::styled(format!("{marker} {label}"), style),
                Span::raw(" "),
            ]),
            width,
        )
    };
    for line in body {
        if diagnostic {
            lines.extend(repeated_prefixed_wrapped_line(
                vec![Span::styled(gutter, style)],
                line,
                width,
            ));
        } else {
            lines.extend(prefixed_wrapped_line(gutter.into(), style, line, width));
        }
    }
    lines
}

/// Widest grapheme `line` has to carry whole. Wrapping cannot split a grapheme,
/// so any leading indentation has to stand back this far or the row it belongs to
/// overflows the viewport.
pub(super) fn unbreakable_columns(line: &Line<'_>) -> usize {
    line.spans
        .iter()
        .map(|span| {
            span.content
                .graphemes(true)
                .map(UnicodeWidthStr::width)
                .max()
                .unwrap_or(0)
        })
        .max()
        .unwrap_or(0)
        .max(1)
}

/// Whether a row can carry `prefix` and still host the widest grapheme it must
/// break whole. Below that the gutter is dropped rather than overflow the
/// viewport — it is indentation, and indentation is allowed to disappear.
pub(super) fn gutter_fits(prefix: &[Span<'_>], line: &Line<'_>, width: u16) -> bool {
    let prefix_width = prefix
        .iter()
        .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
        .sum::<usize>();
    prefix_width + unbreakable_columns(line) <= usize::from(width.max(1))
}

pub(super) fn prefixed_wrapped_line(
    prefix: String,
    prefix_style: Style,
    line: Line<'static>,
    width: u16,
) -> Vec<Line<'static>> {
    let room = usize::from(width);
    // A label that cannot leave the row both a column and the widest grapheme it
    // has to carry whole is dropped: it could not be hung off its own
    // continuation rows without overflowing the viewport.
    let prefix = if UnicodeWidthStr::width(prefix.as_str()) + unbreakable_columns(&line) > room {
        String::new()
    } else {
        prefix
    };
    let prefix_width = UnicodeWidthStr::width(prefix.as_str());
    let continuation = " ".repeat(prefix_width);
    let mut wrapped = wrapped_line(
        line,
        u16::try_from(room.saturating_sub(prefix_width).max(1)).unwrap_or(u16::MAX),
    );
    for (index, line) in wrapped.iter_mut().enumerate() {
        line.spans.insert(
            0,
            if index == 0 {
                Span::styled(prefix.clone(), prefix_style)
            } else {
                Span::raw(continuation.clone())
            },
        );
    }
    wrapped
}

pub(super) fn repeated_prefixed_wrapped_line(
    mut prefix: Vec<Span<'static>>,
    line: Line<'static>,
    width: u16,
) -> Vec<Line<'static>> {
    let guttered = gutter_fits(&prefix, &line, width);
    let width = usize::from(width.max(1));
    if !guttered {
        prefix.clear();
    }
    let prefix_width = prefix
        .iter()
        .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
        .sum::<usize>();
    wrapped_line(
        line,
        u16::try_from(width.saturating_sub(prefix_width).max(1)).unwrap_or(u16::MAX),
    )
    .into_iter()
    .map(|line| {
        let mut spans = prefix.clone();
        // Keep inherited text styling on the content, not on its gutter.
        spans.extend(line.spans.into_iter().map(|mut span| {
            span.style = line.style.patch(span.style);
            span
        }));
        Line::from(spans)
    })
    .collect()
}

/// Word-wrap a styled line using the same word-boundary behavior as the
/// paragraph renderer. Long individual words fall back to grapheme wrapping.
pub(in crate::ui) fn wrapped_line(line: Line<'static>, width: u16) -> Vec<Line<'static>> {
    let width = usize::from(width.max(1));
    // A line-level style (`Line::styled`) applies to every span it contains;
    // preserve it on each wrapped output line so underline/bold emphasis
    // survives wrapping.
    let line_style = line.style;
    let mut lines = Vec::new();
    let mut current = Vec::new();
    let mut current_width = 0;
    let mut pending_whitespace = Vec::new();
    let mut pending_whitespace_width = 0;

    for (content, style, whitespace) in line_tokens(line) {
        let token_width = UnicodeWidthStr::width(content.as_str());
        if whitespace {
            append_span(&mut pending_whitespace, content, style);
            pending_whitespace_width += token_width;
            continue;
        }
        if current_width > 0 && current_width + pending_whitespace_width + token_width > width {
            lines.push(line_from_spans(std::mem::take(&mut current)));
            current_width = 0;
            pending_whitespace.clear();
            pending_whitespace_width = 0;
        }
        if !pending_whitespace.is_empty() {
            if current_width + pending_whitespace_width <= width {
                current.append(&mut pending_whitespace);
                current_width += pending_whitespace_width;
            }
            pending_whitespace_width = 0;
        }
        append_word(
            &mut lines,
            &mut current,
            &mut current_width,
            content,
            style,
            width,
        );
    }
    if !current.is_empty() || lines.is_empty() {
        if !pending_whitespace.is_empty() && current_width + pending_whitespace_width <= width {
            current.append(&mut pending_whitespace);
        }
        lines.push(line_from_spans(current));
    }
    if line_style != Style::default() {
        for line in &mut lines {
            line.style = line.style.patch(line_style);
        }
    }
    lines
}

pub(in crate::ui) fn line_tokens(line: Line<'static>) -> Vec<(String, Style, bool)> {
    let mut tokens = Vec::new();
    for span in line.spans {
        let mut token = String::new();
        let mut whitespace = None;
        for character in span.content.chars() {
            let is_whitespace = character.is_whitespace();
            if let Some(previous) = whitespace
                && previous != is_whitespace
            {
                tokens.push((std::mem::take(&mut token), span.style, previous));
            }
            token.push(character);
            whitespace = Some(is_whitespace);
        }
        if let Some(whitespace) = whitespace {
            tokens.push((token, span.style, whitespace));
        }
    }
    tokens
}

pub(in crate::ui) fn append_word(
    lines: &mut Vec<Line<'static>>,
    current: &mut Vec<Span<'static>>,
    current_width: &mut usize,
    word: String,
    style: Style,
    width: usize,
) {
    let word_width = UnicodeWidthStr::width(word.as_str());
    if *current_width + word_width <= width {
        append_span(current, word, style);
        *current_width += word_width;
        return;
    }
    for grapheme in word.graphemes(true) {
        let grapheme_width = UnicodeWidthStr::width(grapheme);
        if *current_width > 0 && *current_width + grapheme_width > width {
            lines.push(line_from_spans(std::mem::take(current)));
            *current_width = 0;
        }
        // A grapheme is the smallest unit a row can hold, so one wider than the
        // row has to be cut to it: spend the remaining columns on the same
        // ellipsis the rest of the transcript truncates with.
        let drawn = if *current_width + grapheme_width > width {
            crate::ui::app::truncate_with_ellipsis(grapheme, width - *current_width)
        } else {
            grapheme.to_owned()
        };
        let drawn_width = UnicodeWidthStr::width(drawn.as_str());
        append_span(current, drawn, style);
        *current_width += drawn_width;
    }
}

pub(in crate::ui) fn append_span(spans: &mut Vec<Span<'static>>, content: String, style: Style) {
    if let Some(last) = spans.last_mut()
        && last.style == style
    {
        last.content.to_mut().push_str(&content);
    } else {
        spans.push(Span::styled(content, style));
    }
}

pub(in crate::ui) fn line_from_spans(spans: Vec<Span<'static>>) -> Line<'static> {
    let mut merged = Vec::new();
    for span in spans {
        append_span(&mut merged, span.content.into_owned(), span.style);
    }
    Line::from(merged)
}

/// Span contents that are pure row chrome (gutters and quote bars) in any
/// leading position. [`FIRST_SPAN_GUTTERS`] additionally holds wrap
/// continuations and narrow-mode tags, which are chrome only in span
/// position 0: a two-space span after a real gutter is code indentation,
/// never a wrap continuation. Text extraction skips exactly these spans,
/// so copied text is the raw content with no band or glyph chrome.
pub(super) const GUTTER_SPANS: &[&str] = &["│ ", "┆ ", "┃ ", "· ", "! ", "> "];

pub(super) const FIRST_SPAN_GUTTERS: &[&str] = &[
    "  ",
    "    ",
    "[U] ",
    "[G] ",
    "[P] ",
    "[T\u{2026}] ",
    "[T\u{2713}] ",
    "[T!] ",
    "[T] ",
    "[D] ",
    "[W] ",
    "[E] ",
    "[I] ",
];

/// The chrome token span `index` of a row hangs in front of its text, if any.
///
/// Row gutters are chrome wherever they stack, and a gutter span welded to its
/// text is still the gutter a row builder hung there: the glyph gutters count
/// from their prefix alone.
///
/// The wrap-continuation indents and narrow-mode tags (the first-span set) are
/// *not* weld-eligible. Leading spaces in span 0 cannot be told apart from
/// indentation that belongs to the content — `"  indented code"` is a code row,
/// not a chrome band — so they only count when a span holds nothing but the
/// token, exactly as [`extract_line`] requires before it strips a span from
/// copied text. Past span 0 *nothing* welds: `"│ nested"` in column 3 is a tree
/// row, while a `"│ "` welded to the front of the row is the gutter it was
/// built as. Longest match wins so a tag is never mistaken for its own indent.
pub(super) fn leading_gutter_token(content: &str, index: usize) -> Option<&'static str> {
    let first = index == 0;
    GUTTER_SPANS
        .iter()
        .copied()
        .filter(|token| content == *token || (first && content.starts_with(token)))
        .chain(
            first
                .then(|| FIRST_SPAN_GUTTERS.iter().copied())
                .into_iter()
                .flatten()
                .filter(|token| content == *token),
        )
        .max_by_key(|token| token.len())
}

/// Leading chrome columns of one rendered row: the gutter spans hanging off
/// its front, peeled positionally from span 0 onwards.
///
/// This is deliberately *not* a content-equality test over every span: it walks
/// the front of the row only. Row builders prepend gutters as whole spans, but
/// `append_span` merges same-style runs, so the chrome can end up welded to the
/// text it belongs to (`"│ title"`): the prefix counts, the rest of that span
/// does not, and a `"│ "` arriving later is output, not chrome.
pub(in crate::ui) fn leading_gutter_columns(line: &Line<'_>) -> u16 {
    let mut columns = 0usize;
    for (index, span) in line.spans.iter().enumerate() {
        let content = span.content.as_ref();
        let Some(token) = leading_gutter_token(content, index) else {
            break;
        };
        columns += UnicodeWidthStr::width(token);
        // A welded gutter ends the chrome run at its own width; only a span
        // that is nothing but gutter lets the walk continue behind it.
        if content.len() > token.len() {
            break;
        }
    }
    u16::try_from(columns).unwrap_or(u16::MAX)
}

/// First characters of gutterless header/border/footer rows (role headers,
/// assistant attribution and footer, code fences, table grids). Such rows
/// are chrome-only: they vanish from an extraction rather than leaking
/// border glyphs into copied text.
pub(super) const CHROME_ROW_PREFIXES: &[&str] = &[
    "┌", "└", "┏", "╭", "╰", "├", "··", "!!", "⚠", "--", "◆", "◇",
];

/// Extract the copyable text of one rendered line inside the display-column
/// window `[col_start, col_end)`: gutter spans are stripped, chrome-only
/// rows yield `None`, and the remaining text is cut on grapheme boundaries.
/// `col_end` beyond the line width selects to the line end; trailing
/// padding is trimmed.
///
/// A row is chrome-only when it is gutterless (or only quote-barred) and
/// starts with a header/border glyph — role headers, attribution, footers —
/// or when every remaining span carries the code/table border signature:
/// fence headers and table grids vanish even inside a role gutter, while
/// code content (syntax-styled, even when it starts with a box glyph)
/// stays. The signature is the border's foreground *and* modifier set,
/// compared exactly: the parchment band only ever patches backgrounds, and
/// in high contrast a quantized plain-code foreground equals the border's
/// white, so only the border's DIM|BOLD set tells a chrome row apart from
/// content there (syntect never emits DIM).
pub(super) fn extract_line(
    line: &Line<'static>,
    col_start: u16,
    col_end: u16,
    theme: &Theme,
) -> Option<String> {
    if col_start >= col_end {
        return None;
    }
    let border_style = theme.code_border();
    let mut span_index = 0usize;
    let mut spans = line.spans.iter().peekable();
    let mut gutter_width = 0u16;
    // Quoted content rows keep only "> " gutters; a border row inside a
    // quote ("> ┌──┬──") is therefore still recognized as chrome.
    let mut only_quote_gutters = true;
    while let Some(span) = spans.peek() {
        let content = span.content.as_ref();
        let is_gutter = GUTTER_SPANS.contains(&content)
            || (span_index == 0 && FIRST_SPAN_GUTTERS.contains(&content));
        if !is_gutter {
            break;
        }
        gutter_width = gutter_width.saturating_add(UnicodeWidthStr::width(content) as u16);
        if content != "> " {
            only_quote_gutters = false;
        }
        span_index += 1;
        spans.next();
    }
    let remaining: Vec<&ratatui::text::Span<'static>> = spans.collect();
    let rest: String = remaining.iter().map(|span| span.content.as_ref()).collect();
    // Standalone narrow headers carry the diagnostic style on the Line and
    // have no content gutter. Identical message text follows a gutter, so it
    // remains copyable rather than being classified by its text alone.
    if gutter_width == 0
        && match rest.as_str() {
            "[D]" => line.style == theme.muted(),
            "[I]" => line.style == theme.internal(),
            "[W]" => line.style == theme.warning(),
            "[E]" => line.style == theme.error(),
            _ => false,
        }
    {
        return None;
    }
    if (gutter_width == 0 || only_quote_gutters)
        && CHROME_ROW_PREFIXES
            .iter()
            .any(|prefix| rest.starts_with(prefix))
    {
        return None;
    }
    if !rest.is_empty()
        && border_style.fg.is_some()
        && remaining.iter().all(|span| {
            span.style.fg == border_style.fg && span.style.add_modifier == border_style.add_modifier
        })
    {
        return None;
    }
    // The window shifts into content coordinates: cells left of the gutter
    // hold no copyable text.
    let start = col_start.saturating_sub(gutter_width);
    let end = col_end.saturating_sub(gutter_width);
    let mut extracted = String::new();
    let mut column = 0u16;
    for grapheme in rest.graphemes(true) {
        let width = UnicodeWidthStr::width(grapheme).max(1) as u16;
        let next = column.saturating_add(width);
        if next > start && column < end {
            extracted.push_str(grapheme);
        }
        column = next;
    }
    Some(extracted.trim_end().to_owned())
}

/// Extract a normalized multi-line selection (start before end, both
/// `(logical line, display column)`) from the rendered conversation lines.
/// Chrome-only rows vanish; blank rows inside the range stay as paragraph
/// breaks; leading/trailing blank rows are dropped.
pub(in crate::ui) fn extract_selection(
    lines: &[Line<'static>],
    start: (usize, u16),
    end: (usize, u16),
    theme: &Theme,
) -> String {
    if start.0 >= lines.len() || start >= end {
        return String::new();
    }
    let last = end.0.min(lines.len() - 1);
    let mut extracted = Vec::new();
    for (index, line) in lines.iter().enumerate().take(last + 1).skip(start.0) {
        let col_start = if index == start.0 { start.1 } else { 0 };
        let col_end = if index == end.0 { end.1 } else { u16::MAX };
        if let Some(text) = extract_line(line, col_start, col_end, theme) {
            extracted.push(text);
        }
    }
    while extracted.first().is_some_and(String::is_empty) {
        extracted.remove(0);
    }
    while extracted.last().is_some_and(String::is_empty) {
        extracted.pop();
    }
    extracted.join("\n")
}
