//! Grapheme-safe multiline message input rendering and editing.

use ratatui::{
    Frame,
    layout::Rect,
    text::{Line, Span, Text},
    widgets::Paragraph,
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;
use zeroize::{Zeroize, Zeroizing};

use crate::theme::Theme;

use super::transcript::{
    SCROLLBAR_RESERVE, ScrollbarGeometry, pane_scrollbar_track, render_scrollbar_track,
};

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

    /// Rows the composer box needs inside a box of `inner` interior width,
    /// measured at exactly the width [`render`] will wrap at.
    ///
    /// The box grows to the ceiling and only then reserves the scrollbar
    /// columns, so the two widths disagree by design: a draft that still fits
    /// once those columns are gone must not reserve them, and one that
    /// overflows at the reserved width must not be counted at the full width
    /// (which would promise a shorter box than the renderer produces). Sharing
    /// this decision is what keeps the pane height, the rendered wrap, and
    /// [`InputState::has_overflow`] from flip-flopping each other.
    ///
    /// The ceiling is assumed here — it is what the caller clamps to — since
    /// the height is derived from this count: a draft over the reserved ceiling
    /// is exactly the draft that ends up clipped at it.
    pub(crate) fn composer_rows(&self, inner_width: u16) -> usize {
        let (width, _) = composer_wrap(self.as_str(), inner_width, MAX_TEXT_ROWS);
        self.content_rows(width.max(1))
    }

    /// True when content rows exceed the laid-out viewport: the box is at
    /// its ceiling and wheel scrolling is meaningful.
    ///
    /// Both sides of the comparison come from the rendered box itself — the
    /// width [`render`] wrapped at and the height it drew — so this agrees with
    /// the scrollbar's presence by construction, reservation and all.
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

    /// The buffer byte nearest a display position (row within the visible
    /// window, column within the text rect) — the same mapping a click uses
    /// to place the cursor, exposed for mouse selection anchors.
    pub(crate) fn byte_at_display_position(&self, row: u16, column: u16) -> usize {
        if self.layout_width == 0 || self.layout_height == 0 {
            return self.cursor;
        }
        let rows = visual_rows(&self.value, self.layout_width);
        if rows.is_empty() {
            return 0;
        }
        let target_row = (self.viewport_row + usize::from(row)).min(rows.len() - 1);
        cursor_at_column(&rows[target_row], column)
    }

    /// Display cells covered by a normalized byte range, as
    /// `(row within the visible window, column start, column end)` triples.
    /// Soft-wrap boundaries are exact: a wrapped row selected to its end
    /// covers its full width, and the next row starts at the same byte.
    pub(crate) fn selection_cells(&self, start: usize, end: usize) -> Vec<(u16, u16, u16)> {
        let mut cells = Vec::new();
        if self.layout_width == 0 || self.layout_height == 0 || start >= end {
            return cells;
        }
        let rows = visual_rows(&self.value, self.layout_width);
        let visible = usize::from(self.layout_height);
        for (index, row) in rows
            .iter()
            .enumerate()
            .skip(self.viewport_row)
            .take(visible)
        {
            let cover_start = start.max(row.start);
            let cover_end = end.min(row.end);
            if cover_start >= cover_end {
                continue;
            }
            let column_start = width_between(row, cover_start);
            let column_end = width_between(row, cover_end);
            if column_start < column_end {
                cells.push((
                    u16::try_from(index - self.viewport_row).unwrap_or(u16::MAX),
                    column_start,
                    column_end,
                ));
            }
        }
        cells
    }

    /// Remove a normalized byte range (the composer cut): the cursor moves
    /// to the removal point and the viewport follows it.
    pub(crate) fn delete_byte_range(&mut self, start: usize, end: usize) {
        if start >= end || end > self.value.len() {
            return;
        }
        self.value.replace_range(start..end, "");
        self.cursor = start;
        self.snap_cursor_forward();
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

/// The width a composer wraps at, and whether it reserves the scrollbar
/// columns: one decision, shared by [`render`], [`InputState::composer_rows`]
/// and (through the width the renderer records) [`InputState::has_overflow`],
/// so the wrap, the painted text, the cursor, the pane height and the wheel
/// gate can never disagree about which columns the draft lives in.
///
/// The reservation is a *ceiling* affordance for the message composer: a box
/// laid out shorter than [`MAX_TEXT_ROWS`] has room to grow into and never gives
/// columns up. On top of that it is taken only when
///
/// * the reduced width still leaves text to draw (`reserved > 0`), and
/// * the draft genuinely overflows *at that reduced width* — testing it at the
///   full width and then wrapping narrower would re-flow every row the moment
///   the bar appeared.
///
/// A pane narrower than the reservation itself is the case the first condition
/// is for: reserving there would collapse the text area to zero columns, which
/// paints no draft at all while leaving the scroll state reading a zero-width
/// layout as "fits". Such a pane keeps its full interior and draws no track.
fn composer_wrap(value: &str, inner_width: u16, inner_height: u16) -> (u16, bool) {
    let reserved = inner_width.saturating_sub(SCROLLBAR_RESERVE);
    let reserves = inner_height == MAX_TEXT_ROWS
        && reserved > 0
        && visual_rows(value, reserved).len() > usize::from(inner_height);
    (if reserves { reserved } else { inner_width }, reserves)
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
    let block = crate::ui::panel_block().border_style(border_style);
    // The focused composer paints the shared cream panel beneath its text;
    // the bold accent border, not a second fill, marks the active editing
    // surface.
    let block = if focused {
        block.style(theme.panel())
    } else {
        block
    };
    let inner = block.inner(area);
    // The wrap width and the reservation come from one shared decision
    // (`composer_wrap`), measured at the width the text will actually be laid
    // out in. Only the message composer is ever laid out at the ceiling, which
    // scopes the affordance to it.
    let (text_width, reserved) = composer_wrap(input.as_str(), inner.width, inner.height);
    let text_area = Rect::new(inner.x, inner.y, text_width, inner.height);
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
            super::app::truncate_with_ellipsis(placeholder, usize::from(text_width.max(1)));
        *first = Line::from(Span::styled(fitted, theme.muted()));
    }
    let title_width = line_width(&title);
    // The title's blank pad columns come out of the width it may fill, and
    // the left one shifts where the clickable title starts.
    let pad = crate::ui::PANEL_TITLE_PAD;
    let title_layout = overflow_title(
        title,
        rows_above,
        rows_below,
        area.width.saturating_sub(2 * pad),
    );
    let title_rect = title_layout.original_offset.and_then(|offset| {
        let offset = offset.saturating_add(pad);
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
    let block = block.title(crate::ui::panel_title(title_layout.text));
    frame.render_widget(block, area);
    frame.render_widget(Paragraph::new(Text::from(lines)), text_area);
    // The track lives in the rightmost reserved column, one blank margin away
    // from the text — cell-for-cell the conversation pane's geometry.
    let track = pane_scrollbar_track(area, text_area);
    let scrollbar = (reserved && track.width > 0)
        .then(|| {
            ScrollbarGeometry::resolve(track, total_rows)
                .map(|geometry| geometry.with_thumb(rows_above))
        })
        .flatten();
    if let Some(geometry) = scrollbar {
        render_scrollbar_track(frame, geometry, theme);
    }
    // The cursor sits in the painted text area, not the box interior: those
    // two differ by exactly the reserved columns.
    if focused && text_area.width > 0 && text_area.height > 0 {
        frame.set_cursor_position((
            text_area.x.saturating_add(cursor_column),
            text_area
                .y
                .saturating_add(cursor_row.min(text_area.height - 1)),
        ));
    }
    RenderedInput {
        text_rect: text_area,
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
mod tests;
