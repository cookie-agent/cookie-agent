//! Grapheme-safe multiline message input rendering and editing.

use ratatui::{
    Frame,
    layout::Rect,
    text::{Line, Span, Text},
    widgets::{Block, Borders, Paragraph},
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;
use zeroize::{Zeroize, Zeroizing};

use crate::theme::Theme;

use super::transcript::{ScrollbarGeometry, render_scrollbar_track};

/// Visible text-row ceiling for the growing message composer: the layout
/// reclaims conversation rows up to this height, and the box scrolls beyond
/// it. Credential and search fields are always single-row and never reach
/// the ceiling.
pub(crate) const MAX_TEXT_ROWS: u16 = 5;

#[derive(Default)]
pub(crate) struct InputState {
    value: String,
    cursor: usize,
    viewport_row: usize,
    preferred_column: Option<u16>,
    layout_width: u16,
    layout_height: u16,
    /// Set by scrollbar interaction: the viewport stays where the drag or
    /// track press put it instead of chasing the text cursor, until the
    /// next edit or cursor key re-anchors it.
    scroll_hold: bool,
}

/// Secret-bearing single-field editor used only by provider setup.
///
/// This performs best-effort wiping of buffers owned by the process. Rust,
/// allocator, terminal, transport, and kernel copies are outside its control.
#[derive(Default)]
pub(crate) struct CredentialInput {
    input: InputState,
}

impl CredentialInput {
    pub(crate) fn as_str(&self) -> &str {
        self.input.as_str()
    }

    pub(crate) fn insert(&mut self, character: char) {
        self.input.insert(character);
    }

    /// Moves a temporary secret buffer into the editor without copying its bytes.
    pub(crate) fn insert_owned(&mut self, text: String) {
        self.input.insert_owned(text);
    }

    pub(crate) fn set_buffer(&mut self, buffer: String) {
        self.wipe();
        self.input.set_buffer(buffer);
    }

    pub(crate) fn backspace(&mut self) {
        self.input.backspace();
    }

    pub(crate) fn delete(&mut self) {
        self.input.delete();
    }

    pub(crate) fn move_left(&mut self) {
        self.input.move_left();
    }

    pub(crate) fn move_right(&mut self) {
        self.input.move_right();
    }

    pub(crate) fn move_buffer_home(&mut self) {
        self.input.move_buffer_home();
    }

    pub(crate) fn move_buffer_end(&mut self) {
        self.input.move_buffer_end();
    }

    pub(crate) fn state_mut(&mut self) -> &mut InputState {
        &mut self.input
    }

    pub(crate) fn wipe(&mut self) {
        let had_value = !self.input.as_str().is_empty();
        self.input.wipe();
        if had_value {
            record_credential_wipe();
        }
    }
}

impl Drop for CredentialInput {
    fn drop(&mut self) {
        self.wipe();
    }
}

#[derive(Debug)]
struct VisualRow {
    start: usize,
    end: usize,
    width: u16,
    text: String,
    cursor_columns: Vec<(usize, u16)>,
}

impl InputState {
    pub(crate) fn as_str(&self) -> &str {
        &self.value
    }

    fn insert_owned(&mut self, mut text: String) {
        if text.is_empty() {
            return;
        }
        let inserted_len = text.len();
        let previous = Zeroizing::new(std::mem::take(&mut self.value));
        text.reserve(previous.len());
        text.insert_str(0, &previous[..self.cursor]);
        text.push_str(&previous[self.cursor..]);
        self.value = text;
        self.cursor += inserted_len;
        self.preferred_column = None;
        self.reanchor_cursor();
    }

    #[cfg(test)]
    pub(crate) fn cursor_byte(&self) -> usize {
        self.cursor
    }

    #[cfg(test)]
    pub(crate) fn viewport_row(&self) -> usize {
        self.viewport_row
    }

    #[cfg(test)]
    pub(crate) fn visual_row_count(&self, width: u16) -> usize {
        self.content_rows(width)
    }

    /// Soft-wrapped visual row count at a display width — the composer's
    /// adaptive height derives from this.
    pub(crate) fn content_rows(&self, width: u16) -> usize {
        visual_rows(&self.value, width).len()
    }

    /// True when content rows exceed the laid-out viewport: the box is at
    /// its ceiling and wheel scrolling is meaningful.
    pub(crate) fn has_overflow(&self) -> bool {
        self.layout_width > 0
            && self.layout_height > 0
            && self.content_rows(self.layout_width) > usize::from(self.layout_height)
    }

    #[cfg(test)]
    pub(crate) fn cursor_visual_position(&self, width: u16) -> (usize, u16) {
        let rows = visual_rows(&self.value, width);
        cursor_position(&rows, self.cursor)
    }

    pub(crate) fn insert(&mut self, character: char) {
        let mut encoded = [0; 4];
        self.insert_text(character.encode_utf8(&mut encoded));
    }

    pub(crate) fn insert_text(&mut self, text: &str) {
        self.value.insert_str(self.cursor, text);
        self.cursor += text.len();
        self.snap_cursor_forward();
        self.preferred_column = None;
        self.reanchor_cursor();
    }

    pub(crate) fn insert_newline(&mut self) {
        self.insert('\n');
    }

    pub(crate) fn backspace(&mut self) {
        let start = previous_grapheme_start(&self.value, self.cursor);
        if start < self.cursor {
            self.value.replace_range(start..self.cursor, "");
            self.cursor = start;
            self.snap_cursor_forward();
            self.preferred_column = None;
            self.reanchor_cursor();
        }
    }

    pub(crate) fn delete(&mut self) {
        let end = next_grapheme_end(&self.value, self.cursor);
        if end > self.cursor {
            self.value.replace_range(self.cursor..end, "");
            self.snap_cursor_forward();
            self.preferred_column = None;
            self.reanchor_cursor();
        }
    }

    pub(crate) fn move_left(&mut self) {
        self.cursor = previous_grapheme_start(&self.value, self.cursor);
        self.preferred_column = None;
        self.reanchor_cursor();
    }

    pub(crate) fn move_right(&mut self) {
        self.cursor = next_grapheme_end(&self.value, self.cursor);
        self.preferred_column = None;
        self.reanchor_cursor();
    }

    pub(crate) fn move_up(&mut self) {
        self.move_vertical(true);
    }

    pub(crate) fn move_down(&mut self) {
        self.move_vertical(false);
    }

    pub(crate) fn move_wheel(&mut self, up: bool) {
        self.move_visual_rows(up, 3);
    }

    /// Scroll the viewport to an exact row without moving the text cursor:
    /// scrollbar thumb-drag and track paging. The hold survives renders and
    /// ends on the next edit or cursor key, which re-anchors the viewport
    /// to the cursor as usual.
    pub(crate) fn scroll_to(&mut self, row: usize) {
        self.viewport_row = row;
        self.scroll_hold = true;
        if self.layout_width > 0 && self.layout_height > 0 {
            let rows = visual_rows(&self.value, self.layout_width);
            self.viewport_row = self
                .viewport_row
                .min(rows.len().saturating_sub(usize::from(self.layout_height)));
        }
    }

    pub(crate) fn move_word_left(&mut self) {
        let boundaries = grapheme_boundaries(&self.value);
        let mut index = boundary_index(&boundaries, self.cursor);
        while index > 0
            && grapheme_is_whitespace(&self.value, boundaries[index - 1], boundaries[index])
        {
            index -= 1;
        }
        while index > 0
            && !grapheme_is_whitespace(&self.value, boundaries[index - 1], boundaries[index])
        {
            index -= 1;
        }
        self.cursor = boundaries[index];
        self.preferred_column = None;
        self.reanchor_cursor();
    }

    pub(crate) fn move_word_right(&mut self) {
        let boundaries = grapheme_boundaries(&self.value);
        let mut index = boundary_index(&boundaries, self.cursor);
        while index + 1 < boundaries.len()
            && !grapheme_is_whitespace(&self.value, boundaries[index], boundaries[index + 1])
        {
            index += 1;
        }
        while index + 1 < boundaries.len()
            && grapheme_is_whitespace(&self.value, boundaries[index], boundaries[index + 1])
        {
            index += 1;
        }
        self.cursor = boundaries[index];
        self.preferred_column = None;
        self.reanchor_cursor();
    }

    pub(crate) fn delete_word_left(&mut self) {
        let end = self.cursor;
        self.move_word_left();
        self.value.replace_range(self.cursor..end, "");
        self.snap_cursor_forward();
        self.reanchor_cursor();
    }

    pub(crate) fn delete_word_right(&mut self) {
        let start = self.cursor;
        self.move_word_right();
        self.value.replace_range(start..self.cursor, "");
        self.cursor = start;
        self.snap_cursor_forward();
        self.reanchor_cursor();
    }

    pub(crate) fn move_home(&mut self) {
        self.cursor = self.value[..self.cursor]
            .rfind('\n')
            .map_or(0, |offset| offset + 1);
        self.preferred_column = None;
        self.reanchor_cursor();
    }

    pub(crate) fn move_end(&mut self) {
        self.cursor = self.value[self.cursor..]
            .find('\n')
            .map_or(self.value.len(), |offset| self.cursor + offset);
        self.preferred_column = None;
        self.reanchor_cursor();
    }

    pub(crate) fn move_buffer_home(&mut self) {
        self.cursor = 0;
        self.preferred_column = None;
        self.reanchor_cursor();
    }

    pub(crate) fn move_buffer_end(&mut self) {
        self.cursor = self.value.len();
        self.preferred_column = None;
        self.reanchor_cursor();
    }

    pub(crate) fn take(&mut self) -> String {
        self.cursor = 0;
        self.viewport_row = 0;
        self.preferred_column = None;
        std::mem::take(&mut self.value)
    }

    fn wipe(&mut self) {
        self.value.zeroize();
        self.value.clear();
        self.cursor = 0;
        self.viewport_row = 0;
        self.preferred_column = None;
    }

    pub(crate) fn set_buffer(&mut self, buffer: String) {
        self.cursor = buffer.len();
        self.value = buffer;
        self.viewport_row = 0;
        self.preferred_column = None;
        self.reanchor_cursor();
    }

    pub(crate) fn set_cursor_from_display_position(&mut self, row: u16, column: u16) {
        if self.layout_width == 0 || self.layout_height == 0 {
            return;
        }
        let rows = visual_rows(&self.value, self.layout_width);
        let target_row = (self.viewport_row + usize::from(row)).min(rows.len().saturating_sub(1));
        self.cursor = cursor_at_column(&rows[target_row], column);
        self.preferred_column = None;
        self.reanchor_cursor();
    }

    fn move_vertical(&mut self, up: bool) {
        self.move_visual_rows(up, 1);
    }

    fn move_visual_rows(&mut self, up: bool, count: usize) {
        if count == 0 || self.layout_width == 0 {
            return;
        }
        let rows = visual_rows(&self.value, self.layout_width.max(1));
        let (row, column) = cursor_position(&rows, self.cursor);
        let preferred = self.preferred_column.unwrap_or(column);
        let target = if up {
            row.saturating_sub(count)
        } else {
            row.saturating_add(count).min(rows.len().saturating_sub(1))
        };
        self.cursor = cursor_at_column(&rows[target], preferred);
        self.preferred_column = Some(preferred);
        self.reanchor_cursor();
    }

    fn snap_cursor_forward(&mut self) {
        if self
            .value
            .grapheme_indices(true)
            .any(|(offset, _)| offset == self.cursor)
            || self.cursor == self.value.len()
        {
            return;
        }
        self.cursor = self
            .value
            .grapheme_indices(true)
            .map(|(offset, _)| offset)
            .find(|offset| *offset > self.cursor)
            .unwrap_or(self.value.len());
    }

    fn reanchor_cursor(&mut self) {
        // Any edit or cursor key ends a scrollbar hold: the viewport
        // follows the text cursor again.
        self.scroll_hold = false;
        if self.layout_width == 0 || self.layout_height == 0 {
            return;
        }
        let rows = visual_rows(&self.value, self.layout_width);
        let (cursor_row, _) = cursor_position(&rows, self.cursor);
        let visible = usize::from(self.layout_height);
        if cursor_row < self.viewport_row {
            self.viewport_row = cursor_row;
        } else if cursor_row >= self.viewport_row.saturating_add(visible) {
            self.viewport_row = cursor_row + 1 - visible;
        }
        self.viewport_row = self.viewport_row.min(rows.len().saturating_sub(visible));
    }

    fn visible_rows(
        &mut self,
        width: u16,
        height: u16,
    ) -> (Vec<Line<'static>>, u16, u16, usize, usize) {
        self.layout_width = width;
        self.layout_height = height;
        if width == 0 {
            return (Vec::new(), 0, 0, 0, 0);
        }
        let rows = visual_rows(&self.value, width);
        if height == 0 {
            let rows_above = self.viewport_row.min(rows.len());
            return (
                Vec::new(),
                0,
                0,
                rows_above,
                rows.len().saturating_sub(rows_above),
            );
        }
        let visible = usize::from(height);
        if self.scroll_hold {
            // A scrollbar drag positions the viewport directly; renders
            // only clamp it against shrinking content, never chase the
            // cursor.
            self.viewport_row = self.viewport_row.min(rows.len().saturating_sub(visible));
        } else {
            self.reanchor_cursor();
        }
        let (cursor_row, cursor_column) = cursor_position(&rows, self.cursor);
        let lines = rows
            .iter()
            .skip(self.viewport_row)
            .take(visible)
            .map(|row| Line::from(row.text.clone()))
            .collect();
        (
            lines,
            cursor_column.min(width.saturating_sub(1)),
            u16::try_from(cursor_row.saturating_sub(self.viewport_row)).unwrap_or(u16::MAX),
            self.viewport_row,
            rows.len()
                .saturating_sub(self.viewport_row.saturating_add(visible)),
        )
    }
}

#[cfg(test)]
static CREDENTIAL_WIPE_COUNT: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

#[cfg(test)]
fn record_credential_wipe() {
    CREDENTIAL_WIPE_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

#[cfg(not(test))]
fn record_credential_wipe() {}

#[cfg(test)]
pub(crate) fn credential_wipe_count() -> usize {
    CREDENTIAL_WIPE_COUNT.load(std::sync::atomic::Ordering::Relaxed)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RenderedInput {
    pub(crate) text_rect: Rect,
    /// The visible cells occupied by the caller-provided title, excluding any
    /// scroll prefix. `None` means scroll indicators replaced the title.
    pub(crate) title_rect: Option<Rect>,
    /// Track and thumb geometry of the reserved scrollbar column, present
    /// only when the box is at its height ceiling and content overflows —
    /// the click/drag hit source mirroring the conversation scrollbar.
    pub(crate) scrollbar: Option<ScrollbarGeometry>,
}

pub(crate) fn render(
    frame: &mut Frame,
    area: Rect,
    input: &mut InputState,
    focused: bool,
    title: impl Into<Line<'static>>,
    placeholder: Option<&str>,
    theme: &Theme,
) -> RenderedInput {
    let title = title.into();
    let border_style = theme.input_border(focused);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(border_style);
    // The focused composer paints the shared cream panel beneath its text;
    // the bold accent border, not a second fill, marks the active editing
    // surface.
    let block = if focused {
        block.style(theme.panel())
    } else {
        block
    };
    let inner = block.inner(area);
    // A box at its height ceiling with overflowing content reserves its
    // rightmost column for a scrollbar — the same track/thumb treatment as
    // the conversation pane. Only the message composer is ever laid out at
    // the ceiling, which scopes the affordance to it.
    let overflowing = inner.height == MAX_TEXT_ROWS
        && visual_rows(input.as_str(), inner.width.max(1)).len() > usize::from(inner.height);
    let text_width = inner.width.saturating_sub(u16::from(overflowing));
    let (mut lines, cursor_column, cursor_row, rows_above, rows_below) =
        input.visible_rows(text_width, inner.height);
    let total_rows = rows_above
        .saturating_add(lines.len())
        .saturating_add(rows_below);
    // An empty editor invites typing, the way a web form placeholder does;
    // it disappears as soon as there is real content.
    if input.as_str().is_empty()
        && let Some(placeholder) = placeholder
        && let Some(first) = lines.first_mut()
    {
        let fitted =
            super::app::truncate_with_ellipsis(placeholder, usize::from(inner.width.max(1)));
        *first = Line::from(Span::styled(fitted, theme.muted()));
    }
    let title_width = line_width(&title);
    let title_layout = overflow_title(title, rows_above, rows_below, area.width);
    let title_rect = title_layout.original_offset.and_then(|offset| {
        let available = area.width.saturating_sub(2);
        let visible_width = available.saturating_sub(offset).min(title_width);
        (visible_width > 0).then(|| {
            Rect::new(
                area.x.saturating_add(1).saturating_add(offset),
                area.y,
                visible_width,
                1,
            )
        })
    });
    let block = block.title(title_layout.text);
    frame.render_widget(block, area);
    frame.render_widget(
        Paragraph::new(Text::from(lines)),
        Rect::new(inner.x, inner.y, text_width, inner.height),
    );
    let scrollbar = if overflowing {
        ScrollbarGeometry::resolve(
            Rect::new(inner.x.saturating_add(text_width), inner.y, 1, inner.height),
            total_rows,
        )
        .map(|geometry| geometry.with_thumb(rows_above))
    } else {
        None
    };
    if let Some(geometry) = scrollbar {
        render_scrollbar_track(frame, geometry, theme);
    }
    if focused && inner.width > 0 && inner.height > 0 {
        frame.set_cursor_position((
            inner.x.saturating_add(cursor_column),
            inner.y.saturating_add(cursor_row.min(inner.height - 1)),
        ));
    }
    RenderedInput {
        text_rect: inner,
        title_rect,
        scrollbar,
    }
}

pub(crate) fn render_masked(
    frame: &mut Frame,
    area: Rect,
    input: &mut CredentialInput,
    focused: bool,
    title: &str,
    theme: &Theme,
) -> RenderedInput {
    let cursor_graphemes = input.input.value[..input.input.cursor]
        .graphemes(true)
        .count();
    let grapheme_count = input.input.value.graphemes(true).count();
    let mut masked = InputState {
        value: "•".repeat(grapheme_count),
        cursor: cursor_graphemes.saturating_mul('•'.len_utf8()),
        ..InputState::default()
    };
    // Credential fields show no placeholder: their title and help text carry
    // the explanation, and a stray hint could read as a default value.
    let rendered = render(
        frame,
        area,
        &mut masked,
        focused,
        title.to_owned(),
        None,
        theme,
    );
    // The masked stand-in owned the render layout; mirror it back so mouse
    // positioning on the real (secret) buffer resolves display cells.
    input.input.layout_width = masked.layout_width;
    input.input.layout_height = masked.layout_height;
    input.input.viewport_row = masked.viewport_row;
    rendered
}

struct OverflowTitle {
    text: Line<'static>,
    original_offset: Option<u16>,
}

fn line_width(line: &Line) -> u16 {
    u16::try_from(line.width()).unwrap_or(u16::MAX)
}

/// The input box title plus its scroll-position prefix. The caller's styled
/// title spans pass through untouched; only the prefix/fallback indicators
/// are plain text that inherits the border accent.
fn overflow_title(
    title: Line<'static>,
    rows_above: usize,
    rows_below: usize,
    width: u16,
) -> OverflowTitle {
    if rows_above == 0 && rows_below == 0 {
        return OverflowTitle {
            text: title,
            original_offset: Some(0),
        };
    }
    let available = width.saturating_sub(2);
    let prefix = format!("Input ↑{rows_above} ↓{rows_below} · ");
    let prefix_width = text_width(&prefix);
    if prefix_width.saturating_add(line_width(&title)) <= available {
        let mut spans = vec![Span::raw(prefix)];
        spans.extend(title.spans);
        return OverflowTitle {
            text: Line::from(spans),
            original_offset: Some(prefix_width),
        };
    }
    let labelled = format!("Input ↑{rows_above} ↓{rows_below}");
    if text_width(&labelled) <= available {
        return OverflowTitle {
            text: Line::from(labelled),
            original_offset: None,
        };
    }
    OverflowTitle {
        text: Line::from(format!("↑{rows_above}↓{rows_below}")),
        original_offset: None,
    }
}

fn visual_rows(value: &str, width: u16) -> Vec<VisualRow> {
    if width == 0 {
        return vec![VisualRow {
            start: 0,
            end: value.len(),
            width: 0,
            text: String::new(),
            cursor_columns: vec![(0, 0), (value.len(), 0)],
        }];
    }
    let mut rows = Vec::new();
    let mut row_start = 0;
    let mut row_end = 0;
    let mut row_width = 0;
    let mut row_text = String::new();
    let mut cursor_columns = vec![(0, 0)];
    for (offset, grapheme) in value.grapheme_indices(true) {
        if grapheme == "\n" {
            rows.push(VisualRow {
                start: row_start,
                end: offset,
                width: row_width,
                text: std::mem::take(&mut row_text),
                cursor_columns: std::mem::take(&mut cursor_columns),
            });
            row_start = offset + grapheme.len();
            row_end = row_start;
            row_width = 0;
            cursor_columns.push((row_start, 0));
            continue;
        }
        let grapheme_width = display_grapheme_width(grapheme, row_width);
        if row_end > row_start && row_width.saturating_add(grapheme_width) > width {
            rows.push(VisualRow {
                start: row_start,
                end: offset,
                width: row_width,
                text: std::mem::take(&mut row_text),
                cursor_columns: std::mem::take(&mut cursor_columns),
            });
            row_start = offset;
            row_width = 0;
            cursor_columns.push((row_start, 0));
        }
        append_display_grapheme(&mut row_text, grapheme, row_width, width);
        row_width = row_width
            .saturating_add(display_grapheme_width(grapheme, row_width))
            .min(width);
        row_end = offset + grapheme.len();
        cursor_columns.push((row_end, row_width));
    }
    rows.push(VisualRow {
        start: row_start,
        end: row_end,
        width: row_width,
        text: row_text,
        cursor_columns,
    });
    if row_width == width && row_end == value.len() && !value.is_empty() {
        rows.push(VisualRow {
            start: row_end,
            end: row_end,
            width: 0,
            text: String::new(),
            cursor_columns: vec![(row_end, 0)],
        });
    }
    rows
}

fn append_display_grapheme(output: &mut String, grapheme: &str, column: u16, width: u16) {
    let available = width.saturating_sub(column);
    if available == 0 {
        return;
    }
    if grapheme == "\t" {
        output.extend(std::iter::repeat_n(
            ' ',
            usize::from(display_grapheme_width(grapheme, column).min(available)),
        ));
        return;
    }
    if grapheme.chars().any(char::is_control) || text_width(grapheme) > available {
        output.push('�');
        return;
    }
    output.push_str(grapheme);
}

fn display_grapheme_width(grapheme: &str, column: u16) -> u16 {
    if grapheme == "\t" {
        return 4 - column % 4;
    }
    if grapheme.chars().any(char::is_control) {
        return 1;
    }
    text_width(grapheme)
}

fn cursor_position(rows: &[VisualRow], cursor: usize) -> (usize, u16) {
    for (index, row) in rows.iter().enumerate() {
        if cursor < row.start || cursor > row.end {
            continue;
        }
        if cursor == row.end && rows.get(index + 1).is_some_and(|next| next.start == cursor) {
            continue;
        }
        return (index, width_between(row, cursor));
    }
    let index = rows.len().saturating_sub(1);
    (index, rows[index].width)
}

fn cursor_at_column(row: &VisualRow, column: u16) -> usize {
    row.cursor_columns
        .iter()
        .take_while(|(_, candidate)| *candidate <= column)
        .last()
        .map_or(row.start, |(offset, _)| *offset)
}

fn width_between(row: &VisualRow, cursor: usize) -> u16 {
    row.cursor_columns
        .iter()
        .find(|(offset, _)| *offset == cursor)
        .map_or(row.width, |(_, column)| *column)
}

fn previous_grapheme_start(value: &str, cursor: usize) -> usize {
    value[..cursor]
        .grapheme_indices(true)
        .next_back()
        .map_or(cursor, |(offset, _)| offset)
}

fn next_grapheme_end(value: &str, cursor: usize) -> usize {
    value[cursor..]
        .graphemes(true)
        .next()
        .map_or(cursor, |grapheme| cursor + grapheme.len())
}

fn grapheme_boundaries(value: &str) -> Vec<usize> {
    value
        .grapheme_indices(true)
        .map(|(offset, _)| offset)
        .chain(std::iter::once(value.len()))
        .collect()
}

fn boundary_index(boundaries: &[usize], cursor: usize) -> usize {
    boundaries
        .binary_search(&cursor)
        .expect("input cursor is always on a grapheme boundary")
}

fn grapheme_is_whitespace(value: &str, start: usize, end: usize) -> bool {
    value[start..end].chars().all(char::is_whitespace)
}

fn text_width(text: &str) -> u16 {
    UnicodeWidthStr::width(text).min(usize::from(u16::MAX)) as u16
}

#[cfg(test)]
mod tests {
    use ratatui::{
        Terminal,
        backend::TestBackend,
        layout::Rect,
        style::{Modifier, Style},
    };

    use super::{CredentialInput, InputState, ScrollbarGeometry, render};
    use crate::theme::Theme;

    #[test]
    fn focused_and_unfocused_message_borders_differ_by_weight_and_fill() {
        fn rendered_styles(focused: bool) -> (Style, Style, Style) {
            let mut input = InputState::default();
            let mut terminal = Terminal::new(TestBackend::new(12, 3)).expect("terminal");
            terminal
                .draw(|frame| {
                    render(
                        frame,
                        frame.area(),
                        &mut input,
                        focused,
                        "Message",
                        None,
                        &Theme::default(),
                    );
                })
                .expect("render Message box");
            let buffer = terminal.backend().buffer();
            (
                buffer[(0, 0)].style(),
                buffer[(1, 0)].style(),
                buffer[(1, 1)].style(),
            )
        }

        let theme = Theme::default();
        let (focused_border, focused_title, focused_text) = rendered_styles(true);
        let (unfocused_border, unfocused_title, unfocused_text) = rendered_styles(false);

        // Focus carries the warm user accent in bold; the resting box falls
        // back to a dim crust border. Weight — not color alone — still
        // distinguishes the states when color is unavailable.
        assert_eq!(focused_border.fg, theme.user().fg);
        assert_eq!(unfocused_border.fg, theme.panel_border().fg);
        assert!(focused_border.add_modifier.contains(Modifier::BOLD));
        assert!(!focused_border.add_modifier.contains(Modifier::DIM));
        assert!(unfocused_border.add_modifier.contains(Modifier::DIM));
        assert!(!unfocused_border.add_modifier.contains(Modifier::BOLD));
        // The focused composer interior is filled with the panel surface.
        assert_eq!(focused_text.bg, theme.panel().bg);
        assert!(matches!(
            unfocused_text.bg,
            None | Some(ratatui::style::Color::Reset)
        ));
        // Titles pick up the border accent of their state.
        assert_eq!(focused_title.fg, theme.user().fg);
        assert_eq!(unfocused_title.fg, theme.panel_border().fg);
    }

    #[test]
    fn empty_input_shows_the_placeholder_until_text_arrives() {
        fn buffer_text(input: &mut InputState, placeholder: Option<&str>) -> String {
            let mut terminal = Terminal::new(TestBackend::new(30, 3)).expect("terminal");
            terminal
                .draw(|frame| {
                    render(
                        frame,
                        frame.area(),
                        input,
                        true,
                        "Message",
                        placeholder,
                        &Theme::default(),
                    );
                })
                .expect("render");
            let buffer = terminal.backend().buffer();
            (0..3)
                .flat_map(|y| (0..30).map(move |x| buffer[(x, y)].symbol().to_owned()))
                .collect()
        }

        let mut input = InputState::default();
        let rendered = buffer_text(&mut input, Some("Type a message · / for commands"));
        assert!(rendered.contains("Type a message"), "{rendered}");
        // Too narrow for the full hint: it ellipsizes instead of clipping
        // mid-word at the border.
        assert!(rendered.contains('…'), "{rendered}");

        input.set_buffer("h".into());
        let rendered = buffer_text(&mut input, Some("Type a message · / for commands"));
        assert!(!rendered.contains("Type a message"), "{rendered}");

        // No placeholder configured: the row stays blank.
        let mut blank = InputState::default();
        let rendered = buffer_text(&mut blank, None);
        assert!(!rendered.contains("Type a message"), "{rendered}");
    }

    #[test]
    fn credential_owned_insert_reuses_the_sanitized_allocation() {
        let mut sanitized = "sentinel-secret".to_owned();
        let allocation = sanitized.as_ptr();
        let mut input = CredentialInput::default();
        input.insert_owned(std::mem::take(&mut sanitized));
        assert!(sanitized.is_empty());
        assert_eq!(input.as_str(), "sentinel-secret");
        assert_eq!(input.as_str().as_ptr(), allocation);
    }

    #[test]
    fn multiline_editing_wraps_at_the_exact_inner_width() {
        let mut input = InputState::default();
        input.set_buffer("abcdef\ngh".into());
        assert_eq!(input.visual_row_count(3), 3);
        assert_eq!(input.cursor_visual_position(3), (2, 2));

        input.move_home();
        input.backspace();
        assert_eq!(input.as_str(), "abcdefg h".replace(' ', ""));
        input.insert_newline();
        assert_eq!(input.as_str(), "abcdef\ngh");
    }

    #[test]
    fn cursor_and_editing_treat_emoji_zwj_and_combining_sequences_as_graphemes() {
        let family = "👨‍👩‍👧‍👦";
        let combining = "e\u{301}";
        let mut input = InputState::default();
        input.set_buffer(format!("a{family}\n{combining}b"));

        input.move_left();
        input.backspace();
        assert_eq!(input.as_str(), format!("a{family}\nb"));
        input.move_home();
        input.move_left();
        input.backspace();
        assert_eq!(input.as_str(), "a\nb");
        input.delete();
        assert_eq!(input.as_str(), "ab");

        input.set_buffer("ab".into());
        input.move_left();
        input.insert_text("\u{301}");
        assert_eq!(input.cursor_byte(), "a\u{301}".len());
        input.backspace();
        assert_eq!(input.as_str(), "b");
    }

    #[test]
    fn up_down_home_end_follow_logical_and_wrapped_rows() {
        let mut input = InputState::default();
        input.set_buffer("abcd\nef".into());
        input.layout_width = 3;
        input.move_home();
        assert_eq!(input.cursor_byte(), 5);
        input.move_up();
        assert_eq!(input.cursor_byte(), 3);
        input.move_up();
        assert_eq!(input.cursor_byte(), 0);
        input.move_down();
        assert_eq!(input.cursor_byte(), 3);
        input.move_end();
        assert_eq!(input.cursor_byte(), 4);
        input.move_buffer_end();
        assert_eq!(input.cursor_byte(), input.as_str().len());
    }

    #[test]
    fn exact_width_content_places_the_insertion_cursor_on_a_trailing_empty_row() {
        let mut input = InputState::default();
        input.set_buffer("abcd".into());
        assert_eq!(input.visual_row_count(4), 2);
        assert_eq!(input.cursor_visual_position(4), (1, 0));

        let mut terminal = Terminal::new(TestBackend::new(6, 5)).expect("terminal");
        terminal
            .draw(|frame| {
                render(
                    frame,
                    frame.area(),
                    &mut input,
                    true,
                    "Message",
                    None,
                    &Theme::default(),
                );
            })
            .expect("render exact-width input");
        assert_eq!(
            terminal.get_cursor_position().expect("cursor"),
            (1, 2).into()
        );
    }

    #[test]
    fn three_row_viewport_scrolls_to_keep_cursor_visible() {
        let mut input = InputState::default();
        input.set_buffer("one\ntwo\nthree\nfour".into());
        let mut terminal = Terminal::new(TestBackend::new(12, 5)).expect("terminal");
        terminal
            .draw(|frame| {
                render(
                    frame,
                    Rect::new(0, 0, 12, 5),
                    &mut input,
                    true,
                    "Message",
                    None,
                    &Theme::default(),
                );
            })
            .expect("render");
        assert_eq!(input.viewport_row(), 1);
        let position = terminal.get_cursor_position().expect("cursor position");
        assert_eq!(position.y, 3);

        input.move_buffer_home();
        terminal
            .draw(|frame| {
                render(
                    frame,
                    Rect::new(0, 0, 12, 5),
                    &mut input,
                    true,
                    "Message",
                    None,
                    &Theme::default(),
                );
            })
            .expect("render");
        assert_eq!(input.viewport_row(), 0);
        assert_eq!(terminal.get_cursor_position().expect("cursor").y, 1);
    }

    #[test]
    fn incremental_newline_and_wrap_only_typing_reanchors_the_three_row_viewport() {
        let mut newline = InputState::default();
        let mut terminal = Terminal::new(TestBackend::new(8, 5)).expect("terminal");
        terminal
            .draw(|frame| {
                render(
                    frame,
                    frame.area(),
                    &mut newline,
                    true,
                    "Message",
                    None,
                    &Theme::default(),
                );
            })
            .expect("establish layout");
        for character in "a\nb\nc".chars() {
            newline.insert(character);
        }
        assert_eq!(newline.viewport_row(), 0);
        newline.insert_newline();
        assert_eq!(newline.cursor_visual_position(6), (3, 0));
        assert_eq!(newline.viewport_row(), 1);

        let mut wrapped = InputState::default();
        let mut terminal = Terminal::new(TestBackend::new(5, 5)).expect("terminal");
        terminal
            .draw(|frame| {
                render(
                    frame,
                    frame.area(),
                    &mut wrapped,
                    true,
                    "Message",
                    None,
                    &Theme::default(),
                );
            })
            .expect("establish narrow layout");
        for character in "abcdefghi".chars() {
            wrapped.insert(character);
        }
        assert_eq!(wrapped.cursor_visual_position(3), (3, 0));
        assert_eq!(wrapped.viewport_row(), 1);
    }

    #[test]
    fn edits_navigation_set_buffer_click_and_take_keep_viewport_consistent() {
        let mut input = InputState::default();
        let mut terminal = Terminal::new(TestBackend::new(8, 5)).expect("terminal");
        terminal
            .draw(|frame| {
                render(
                    frame,
                    frame.area(),
                    &mut input,
                    true,
                    "Message",
                    None,
                    &Theme::default(),
                );
            })
            .expect("establish layout");

        input.set_buffer("zero\none\ntwo\nthree\nfour".into());
        assert_eq!(input.viewport_row(), 2);
        input.move_up();
        input.move_up();
        input.move_up();
        assert_eq!(input.viewport_row(), 1);
        input.set_cursor_from_display_position(0, 0);
        assert_eq!(input.cursor_visual_position(6).0, 1);
        input.backspace();
        assert_eq!(input.cursor_visual_position(6).0, 0);
        assert_eq!(input.viewport_row(), 0);

        input.move_buffer_end();
        assert_eq!(input.viewport_row(), 2);
        input.move_home();
        assert_eq!(input.cursor_visual_position(6).0, 4);
        input.move_end();
        assert_eq!(input.cursor_visual_position(6).0, 4);
        input.move_buffer_home();
        assert_eq!(input.viewport_row(), 0);
        input.move_buffer_end();
        assert_eq!(input.viewport_row(), 2);

        let retained = input.viewport_row();
        terminal
            .draw(|frame| {
                render(
                    frame,
                    frame.area(),
                    &mut input,
                    false,
                    "Message",
                    None,
                    &Theme::default(),
                );
            })
            .expect("unfocused render");
        assert_eq!(input.viewport_row(), retained);
        assert!(!input.take().is_empty());
        assert_eq!(input.viewport_row(), 0);
    }

    #[test]
    fn overflow_title_reports_input_rows_above_and_below_without_using_text_rows() {
        let mut input = InputState::default();
        input.set_buffer("zero\none\ntwo\nthree\nfour".into());
        let mut terminal = Terminal::new(TestBackend::new(40, 5)).expect("terminal");
        terminal
            .draw(|frame| {
                render(
                    frame,
                    frame.area(),
                    &mut input,
                    true,
                    "Message",
                    None,
                    &Theme::from_environment("dark", true, "xterm", "truecolor"),
                );
            })
            .expect("render overflow title");
        let top = (0..40)
            .map(|x| terminal.backend().buffer()[(x, 0)].symbol())
            .collect::<String>();
        assert!(top.contains("Input ↑2 ↓0 · Message"));
        assert_eq!(terminal.backend().buffer()[(1, 1)].symbol(), "t");

        input.move_up();
        input.move_up();
        input.move_up();
        terminal
            .draw(|frame| {
                render(
                    frame,
                    frame.area(),
                    &mut input,
                    false,
                    "Message",
                    None,
                    &Theme::from_environment("dark", true, "xterm", "truecolor"),
                );
            })
            .expect("render split overflow title");
        let top = (0..40)
            .map(|x| terminal.backend().buffer()[(x, 0)].symbol())
            .collect::<String>();
        assert!(top.contains("Input ↑1 ↓1 · Message"));

        let mut tiny = Terminal::new(TestBackend::new(8, 5)).expect("tiny terminal");
        tiny.draw(|frame| {
            render(
                frame,
                frame.area(),
                &mut input,
                false,
                "Message",
                None,
                &Theme::from_environment("dark", true, "xterm", "truecolor"),
            );
        })
        .expect("tiny overflow title");
        let top = (0..8)
            .map(|x| tiny.backend().buffer()[(x, 0)].symbol())
            .collect::<String>();
        assert!(top.contains("↑1↓1"));
    }

    #[test]
    fn resize_reflows_without_overflow_and_keeps_cursor_visible() {
        let mut input = InputState::default();
        input.set_buffer("界👩‍💻abcdef".into());
        let mut terminal = Terminal::new(TestBackend::new(14, 5)).expect("terminal");
        terminal
            .draw(|frame| {
                render(
                    frame,
                    frame.area(),
                    &mut input,
                    true,
                    "Message",
                    None,
                    &Theme::default(),
                );
            })
            .expect("wide render");
        terminal.backend_mut().resize(7, 5);
        terminal.autoresize().expect("resize");
        terminal
            .draw(|frame| {
                render(
                    frame,
                    frame.area(),
                    &mut input,
                    true,
                    "Message",
                    None,
                    &Theme::default(),
                );
            })
            .expect("narrow render");
        assert_eq!(input.visual_row_count(5), 3);
        assert_eq!(input.cursor_visual_position(5), (2, 0));
        let cursor = terminal.backend().cursor_position();
        assert!(cursor.x < 7 && cursor.y < 5);
    }

    #[test]
    fn resize_reflow_and_zero_inner_dimensions_preserve_safe_cursor_state() {
        let mut input = InputState::default();
        input.set_buffer("界界界\nabc\ndef".into());
        let mut terminal = Terminal::new(TestBackend::new(10, 5)).expect("terminal");
        terminal
            .draw(|frame| {
                render(
                    frame,
                    frame.area(),
                    &mut input,
                    true,
                    "Message",
                    None,
                    &Theme::default(),
                );
            })
            .expect("wide render");
        let wide_viewport = input.viewport_row();

        terminal.backend_mut().resize(6, 2);
        terminal.autoresize().expect("resize to zero inner area");
        terminal
            .draw(|frame| {
                render(
                    frame,
                    frame.area(),
                    &mut input,
                    true,
                    "Message",
                    None,
                    &Theme::default(),
                );
            })
            .expect("zero-inner render");
        assert_eq!(input.viewport_row(), wide_viewport);
        assert!(!terminal.backend().cursor_visible());

        terminal.backend_mut().resize(6, 4);
        terminal.autoresize().expect("resize narrow");
        terminal
            .draw(|frame| {
                render(
                    frame,
                    frame.area(),
                    &mut input,
                    true,
                    "Message",
                    None,
                    &Theme::default(),
                );
            })
            .expect("narrow render");
        let (cursor_row, _) = input.cursor_visual_position(4);
        assert!(cursor_row >= input.viewport_row());
        assert!(cursor_row < input.viewport_row() + 2);
        let cursor = terminal.backend().cursor_position();
        assert!(cursor.x >= 1 && cursor.x < 5 && cursor.y >= 1 && cursor.y < 3);
    }

    #[test]
    fn tiny_areas_render_safely() {
        let mut input = InputState::default();
        input.set_buffer("👩‍💻\ntext".into());
        for (width, height) in [(1, 1), (2, 2), (3, 3)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
            terminal
                .draw(|frame| {
                    render(
                        frame,
                        frame.area(),
                        &mut input,
                        true,
                        "Message",
                        None,
                        &Theme::default(),
                    );
                })
                .expect("tiny render");
        }
    }

    #[test]
    fn composer_at_ceiling_renders_scrollbar_only_when_content_overflows() {
        fn track_symbols(lines: &str) -> Vec<String> {
            let mut input = InputState::default();
            input.set_buffer(lines.to_owned());
            let mut terminal = Terminal::new(TestBackend::new(20, 7)).expect("terminal");
            terminal
                .draw(|frame| {
                    render(
                        frame,
                        Rect::new(0, 0, 20, 7),
                        &mut input,
                        true,
                        "Message",
                        None,
                        &Theme::default(),
                    );
                })
                .expect("render");
            let buffer = terminal.backend().buffer();
            // The reserved track column is the last inner cell, left of the
            // right border at x = 19.
            (1..6)
                .map(|y| buffer[(18, y)].symbol().to_owned())
                .collect()
        }

        // Eight rows of content in a five-row box: the track column shows a
        // muted rail with a thumb covering the visible fraction.
        let overflowing = track_symbols("a\nb\nc\nd\ne\nf\ng\nh");
        assert!(
            overflowing
                .iter()
                .all(|symbol| symbol == "│" || symbol == "█"),
            "track column: {overflowing:?}"
        );
        assert!(
            overflowing.iter().any(|symbol| symbol == "█"),
            "thumb present: {overflowing:?}"
        );

        // Three rows fit the box: no reservation, the column stays text.
        let fitting = track_symbols("a\nb\nc");
        assert!(
            fitting.iter().all(|symbol| symbol != "│" && symbol != "█"),
            "no track when fitting: {fitting:?}"
        );
    }

    #[test]
    fn rendered_input_reports_scrollbar_geometry_only_at_the_overflowing_ceiling() {
        fn rendered_scrollbar(lines: &str, area: Rect) -> Option<ScrollbarGeometry> {
            let mut input = InputState::default();
            input.set_buffer(lines.to_owned());
            let mut terminal =
                Terminal::new(TestBackend::new(area.width, area.height)).expect("terminal");
            let mut rendered = None;
            terminal
                .draw(|frame| {
                    rendered = Some(render(
                        frame,
                        area,
                        &mut input,
                        true,
                        "Message",
                        None,
                        &Theme::default(),
                    ));
                })
                .expect("render");
            rendered.and_then(|rendered| rendered.scrollbar)
        }

        // Eight rows in the five-row ceiling box: the reserved column
        // reports its track and thumb as the click/drag hit source.
        let geometry = rendered_scrollbar("a\nb\nc\nd\ne\nf\ng\nh", Rect::new(0, 0, 20, 7))
            .expect("scrollbar while overflowing at the ceiling");
        assert_eq!(geometry.track, Rect::new(18, 1, 1, 5));
        assert!(!geometry.thumb.is_empty());

        // Fitting content at the same height has nothing to scroll.
        assert!(rendered_scrollbar("a\nb\nc", Rect::new(0, 0, 20, 7)).is_none());
    }

    #[test]
    fn scroll_to_holds_the_viewport_until_an_edit_or_cursor_key_reanchors() {
        fn draw(terminal: &mut Terminal<TestBackend>, input: &mut InputState) {
            terminal
                .draw(|frame| {
                    render(
                        frame,
                        Rect::new(0, 0, 12, 7),
                        input,
                        true,
                        "Message",
                        None,
                        &Theme::default(),
                    );
                })
                .expect("render");
        }

        let mut input = InputState::default();
        input.set_buffer("a\nb\nc\nd\ne\nf\ng\nh".to_owned());
        let mut terminal = Terminal::new(TestBackend::new(12, 7)).expect("terminal");
        // Eight rows in a five-row viewport: the cursor anchors the bottom.
        draw(&mut terminal, &mut input);
        assert_eq!(input.viewport_row(), 3);

        // scroll_to positions the viewport directly, clamped to the exact
        // offset range, and renders keep the held position instead of
        // chasing the cursor at the bottom.
        input.scroll_to(0);
        draw(&mut terminal, &mut input);
        assert_eq!(input.viewport_row(), 0);
        input.scroll_to(usize::MAX);
        assert_eq!(input.viewport_row(), 3);

        // A cursor key ends the hold: the viewport follows the cursor again
        // (a held viewport would have stayed at row 1, hiding the cursor).
        input.scroll_to(1);
        draw(&mut terminal, &mut input);
        assert_eq!(input.viewport_row(), 1);
        input.move_up();
        draw(&mut terminal, &mut input);
        assert_eq!(input.viewport_row(), 2);

        // So does an edit (cursor back at the buffer end first).
        input.move_buffer_end();
        input.scroll_to(1);
        draw(&mut terminal, &mut input);
        input.insert('x');
        draw(&mut terminal, &mut input);
        assert_eq!(input.viewport_row(), 3);
    }
}
