//! Grapheme-safe multiline message input rendering and editing.

use ratatui::{
    Frame,
    layout::Rect,
    text::{Line, Text},
    widgets::{Block, Borders, Paragraph},
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;
use zeroize::{Zeroize, Zeroizing};

use crate::theme::Theme;

#[derive(Default)]
pub(crate) struct InputState {
    value: String,
    cursor: usize,
    viewport_row: usize,
    preferred_column: Option<u16>,
    layout_width: u16,
    layout_height: u16,
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
        visual_rows(&self.value, width).len()
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

    pub(crate) fn move_page_up(&mut self) {
        self.move_visual_rows(true, usize::from(self.layout_height));
    }

    pub(crate) fn move_page_down(&mut self) {
        self.move_visual_rows(false, usize::from(self.layout_height));
    }

    pub(crate) fn move_wheel(&mut self, up: bool) {
        self.move_visual_rows(up, 3);
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
        self.reanchor_cursor();
        let (cursor_row, cursor_column) = cursor_position(&rows, self.cursor);
        let visible = usize::from(height);
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
}

pub(crate) fn render(
    frame: &mut Frame,
    area: Rect,
    input: &mut InputState,
    focused: bool,
    title: &str,
    theme: &Theme,
) -> RenderedInput {
    let border_style = theme.input_border(focused);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(border_style);
    let inner = block.inner(area);
    let (lines, cursor_column, cursor_row, rows_above, rows_below) =
        input.visible_rows(inner.width, inner.height);
    let title_layout = overflow_title(title, rows_above, rows_below, area.width);
    let title_rect = title_layout.original_offset.and_then(|offset| {
        let available = area.width.saturating_sub(2);
        let visible_width = available.saturating_sub(offset).min(text_width(title));
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
    frame.render_widget(Paragraph::new(Text::from(lines)), inner);
    if focused && inner.width > 0 && inner.height > 0 {
        frame.set_cursor_position((
            inner.x.saturating_add(cursor_column),
            inner.y.saturating_add(cursor_row.min(inner.height - 1)),
        ));
    }
    RenderedInput {
        text_rect: inner,
        title_rect,
    }
}

struct OverflowTitle {
    text: String,
    original_offset: Option<u16>,
}

fn overflow_title(title: &str, rows_above: usize, rows_below: usize, width: u16) -> OverflowTitle {
    if rows_above == 0 && rows_below == 0 {
        return OverflowTitle {
            text: title.to_owned(),
            original_offset: Some(0),
        };
    }
    let available = width.saturating_sub(2);
    let prefix = format!("Input ↑{rows_above} ↓{rows_below} · ");
    let full = format!("{prefix}{title}");
    if text_width(&full) <= available {
        return OverflowTitle {
            text: full,
            original_offset: Some(text_width(&prefix)),
        };
    }
    let labelled = format!("Input ↑{rows_above} ↓{rows_below}");
    if text_width(&labelled) <= available {
        return OverflowTitle {
            text: labelled,
            original_offset: None,
        };
    }
    OverflowTitle {
        text: format!("↑{rows_above}↓{rows_below}"),
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

    use super::{CredentialInput, InputState, render};
    use crate::theme::Theme;

    #[test]
    fn focused_and_unfocused_message_borders_share_the_bright_color() {
        fn rendered_styles(focused: bool) -> (Style, Style) {
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
                        &Theme::default(),
                    );
                })
                .expect("render Message box");
            let buffer = terminal.backend().buffer();
            (buffer[(0, 0)].style(), buffer[(1, 0)].style())
        }

        let (focused_border, focused_title) = rendered_styles(true);
        let (unfocused_border, unfocused_title) = rendered_styles(false);

        assert_eq!(unfocused_border.fg, focused_border.fg);
        assert_eq!(unfocused_title.fg, focused_title.fg);
        assert!(focused_border.add_modifier.contains(Modifier::BOLD));
        assert!(!focused_border.add_modifier.contains(Modifier::DIM));
        assert!(unfocused_border.add_modifier.contains(Modifier::DIM));
        assert!(!unfocused_border.add_modifier.contains(Modifier::BOLD));
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
    fn page_movement_uses_visual_rows_and_preserves_the_preferred_column() {
        let mut input = InputState::default();
        input.set_buffer("000\n111\n222\n1界z\n444\n555\nabx".into());
        let mut terminal = Terminal::new(TestBackend::new(6, 5)).expect("terminal");
        terminal
            .draw(|frame| {
                render(
                    frame,
                    frame.area(),
                    &mut input,
                    true,
                    "Message",
                    &Theme::default(),
                );
            })
            .expect("render");
        input.move_left();
        assert_eq!(input.cursor_visual_position(4), (6, 2));

        input.move_page_up();
        assert_eq!(input.cursor_visual_position(4), (3, 1));
        assert_eq!(input.viewport_row(), 3);
        input.move_page_up();
        assert_eq!(input.cursor_visual_position(4), (0, 2));
        assert_eq!(input.viewport_row(), 0);
        input.move_page_down();
        input.move_page_down();
        assert_eq!(input.cursor_visual_position(4), (6, 2));
        assert_eq!(input.viewport_row(), 4);
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
                    &Theme::default(),
                );
            })
            .expect("establish layout");

        input.set_buffer("zero\none\ntwo\nthree\nfour".into());
        assert_eq!(input.viewport_row(), 2);
        input.move_page_up();
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
                    &Theme::from_environment("dark", true, "xterm", "truecolor"),
                );
            })
            .expect("render overflow title");
        let top = (0..40)
            .map(|x| terminal.backend().buffer()[(x, 0)].symbol())
            .collect::<String>();
        assert!(top.contains("Input ↑2 ↓0 · Message"));
        assert_eq!(terminal.backend().buffer()[(1, 1)].symbol(), "t");

        input.move_page_up();
        terminal
            .draw(|frame| {
                render(
                    frame,
                    frame.area(),
                    &mut input,
                    false,
                    "Message",
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
                        &Theme::default(),
                    );
                })
                .expect("tiny render");
        }
    }
}
