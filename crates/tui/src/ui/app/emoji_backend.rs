//! Width-agnostic drawing of VS16 emoji (`❤️`, `☀️`, `✔️`).
//!
//! A text-default glyph followed by VS16 (U+FE0F) is two cells to the buffer,
//! but terminals split on it: VTE, xterm, and Alacritty draw it in one cell
//! and never paint the second, while macOS terminals, kitty, Ghostty, and
//! WezTerm draw it in two. Whatever is written to the shadow cell *after* the
//! emoji clobbers it on the wide terminals, and whatever is skipped lingers
//! on the narrow ones. [`EmojiBackend`] writes the shadow cell *first* and the
//! emoji over it, so the narrow terminals keep the blank and the wide ones
//! cover it. [`bind_emoji_shadows`] gives the shadow cell the emoji's style,
//! which makes the shadow change exactly when the emoji does: the diff resends
//! the emoji on those frames, and the backend repaints the shadow with it.

use ratatui::{
    backend::{Backend, ClearType, WindowSize},
    buffer::{Buffer, Cell, CellWidth},
    layout::{Position, Size},
};

/// A wide cell whose width comes from a presentation selector, which not
/// every terminal honours.
fn is_selector_wide(cell: &Cell) -> bool {
    cell.cell_width() > 1 && cell.symbol().contains('\u{FE0F}')
}

/// Rewrites the cells behind every VS16 emoji as blanks in the emoji's own
/// style, the contract [`EmojiBackend`] relies on to paint them itself.
pub(super) fn bind_emoji_shadows(buffer: &mut Buffer) {
    let width = usize::from(buffer.area.width);
    for index in 0..buffer.content.len() {
        let cell = &buffer.content[index];
        if !is_selector_wide(cell) {
            continue;
        }
        let style = cell.style();
        let row_end = (index / width + 1) * width;
        let end = (index + usize::from(cell.cell_width())).min(row_end);
        for shadow in &mut buffer.content[index + 1..end] {
            shadow.reset();
            shadow.set_style(style);
        }
    }
}

/// Reorders each frame's diff so a VS16 emoji's shadow cells are painted
/// before the emoji, never after it, and drops the trailing writes ratatui
/// emits behind such an emoji: those assume the cursor moved one cell per
/// column, so on a terminal that drew the emoji wide they land one column to
/// the right, and every contiguous write after them with it. Leaving a gap
/// behind the emoji makes the backend reposition before the next cell.
pub(super) struct EmojiBackend<B> {
    inner: B,
}

impl<B> EmojiBackend<B> {
    pub(super) const fn new(inner: B) -> Self {
        Self { inner }
    }
}

impl<B: Backend> Backend for EmojiBackend<B> {
    type Error = B::Error;

    fn draw<'a, I>(&mut self, content: I) -> Result<(), Self::Error>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        let content: Vec<_> = content.collect();
        let blanks: Vec<Cell> = content
            .iter()
            .filter(|(_, _, cell)| is_selector_wide(cell))
            .map(|(_, _, cell)| {
                let mut blank = Cell::default();
                blank.set_style(cell.style());
                blank
            })
            .collect();
        let mut blanks = blanks.iter();
        let mut ordered = Vec::with_capacity(content.len() + blanks.len());
        let mut shadowed = None;
        for (x, y, cell) in content {
            if let Some((row, ref columns)) = shadowed
                && row == y
                && std::ops::Range::contains(columns, &x)
            {
                continue;
            }
            if is_selector_wide(cell) {
                let blank = blanks.next().expect("one blank per VS16 emoji");
                let columns = x.saturating_add(1)..x.saturating_add(cell.cell_width());
                ordered.extend(columns.clone().map(|shadow_x| (shadow_x, y, blank)));
                shadowed = Some((y, columns));
            }
            ordered.push((x, y, cell));
        }
        self.inner.draw(ordered.into_iter())
    }

    fn append_lines(&mut self, n: u16) -> Result<(), Self::Error> {
        self.inner.append_lines(n)
    }

    fn hide_cursor(&mut self) -> Result<(), Self::Error> {
        self.inner.hide_cursor()
    }

    fn show_cursor(&mut self) -> Result<(), Self::Error> {
        self.inner.show_cursor()
    }

    fn get_cursor_position(&mut self) -> Result<Position, Self::Error> {
        self.inner.get_cursor_position()
    }

    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> Result<(), Self::Error> {
        self.inner.set_cursor_position(position)
    }

    fn clear(&mut self) -> Result<(), Self::Error> {
        self.inner.clear()
    }

    fn clear_region(&mut self, clear_type: ClearType) -> Result<(), Self::Error> {
        self.inner.clear_region(clear_type)
    }

    fn size(&self) -> Result<Size, Self::Error> {
        self.inner.size()
    }

    fn window_size(&mut self) -> Result<WindowSize, Self::Error> {
        self.inner.window_size()
    }

    fn flush(&mut self) -> Result<(), Self::Error> {
        self.inner.flush()
    }
}

#[cfg(test)]
mod tests {
    use ratatui::{
        backend::{Backend, CrosstermBackend},
        buffer::Buffer,
        layout::Rect,
        style::{Color, Style},
    };

    use super::{EmojiBackend, bind_emoji_shadows};

    /// Keeps the bytes the backend wrote readable after it is done with them.
    #[derive(Clone, Default)]
    struct Output(std::rc::Rc<std::cell::RefCell<Vec<u8>>>);

    impl std::io::Write for Output {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.borrow_mut().write(bytes)
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn frames(previous: &str, next: &str, style: Style) -> (Buffer, Buffer) {
        let area = Rect::new(0, 0, 6, 1);
        let mut before = Buffer::empty(area);
        before.set_string(0, 0, previous, Style::default());
        let mut after = Buffer::empty(area);
        after.set_string(0, 0, next, style);
        bind_emoji_shadows(&mut after);
        (before, after)
    }

    fn drawn(previous: &str, next: &str) -> String {
        let (before, after) = frames(previous, next, Style::default());
        let output = Output::default();
        let mut backend = EmojiBackend::new(CrosstermBackend::new(output.clone()));
        backend.draw(before.diff(&after).into_iter()).expect("draw");
        String::from_utf8(output.0.take()).expect("utf-8")
    }

    #[test]
    fn shadow_cells_take_the_emoji_style() {
        let style = Style::default().fg(Color::Red).bg(Color::Blue);
        let (_, after) = frames("", "a❤️b", style);
        let shadow = &after.content[2];
        assert_eq!(shadow.symbol(), " ");
        assert_eq!(shadow.style(), after.content[1].style());
        assert_eq!(after.content[3].symbol(), "b");
    }

    #[test]
    fn shadow_is_painted_before_the_emoji_and_the_next_cell_is_repositioned() {
        // MoveTo(x, y) is `ESC [ y+1 ; x+1 H`. The shadow at column 2 goes
        // out first, the emoji at column 1 over it, and `b` at column 3 gets
        // its own MoveTo instead of trusting the cursor after the emoji.
        let output = drawn("zzzz", "a❤️b");
        assert!(
            output.contains("\x1b[1;1Ha\x1b[1;3H \x1b[1;2H❤️\x1b[1;4Hb"),
            "{output:?}"
        );
    }

    #[test]
    fn unchanged_emoji_and_shadow_are_not_resent() {
        assert!(!drawn("a❤️b", "a❤️b").contains('❤'));
    }

    #[test]
    fn already_wide_emoji_draw_as_usual() {
        let output = drawn("zzzz", "a💻b");
        assert!(output.contains("\x1b[1;1Ha💻\x1b[1;4Hb"), "{output:?}");
    }
}
