//! When the transcript reads a page of older history
//! (`docs/site/specs/web-newest-first.md`, section 3.2).
//!
//! A page arriving above the view fires no scroll event, the scroller's
//! position counting from its end, so pages cannot call for more on their
//! own. What is left is reading on when the user has nothing to scroll: the
//! rows do not fill the box, or a page drew nothing. That stops after
//! [`UNPROMPTED_PAGES`] pages until the user scrolls again.

use crate::follow::Metrics;

/// The pages read in a row with no scroll from the user.
pub const UNPROMPTED_PAGES: u32 = 5;

/// Where the transcript stands with its reads of older history.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct HistoryScroll {
    /// How far from the end the view last stood.
    last_from_end: i32,
    /// While a page is read: the content's height when the read started.
    reading: Option<i32>,
    /// The pages read since the user last scrolled.
    unprompted: u32,
}

impl HistoryScroll {
    /// The content or the box changed size: rows growing below the view move
    /// it from the end with no help from the user. This comes as the frame is
    /// laid out, before the scroll that reports the move.
    pub fn resized(&mut self, at: Metrics) {
        self.last_from_end = at.from_end;
    }

    /// Notes a scroll to `at`, telling whether the user moved toward older
    /// rows.
    pub fn scrolled(&mut self, at: Metrics) -> bool {
        let older = at.from_end > self.last_from_end;
        self.last_from_end = at.from_end;
        if older {
            self.prompted();
        }
        older
    }

    /// The user asked for older rows, by a scroll or by an input where no
    /// scroll follows.
    pub fn prompted(&mut self) {
        self.unprompted = 0;
    }

    /// A page started reading over `height` of content.
    pub fn started(&mut self, height: i32) {
        self.reading = Some(height);
    }

    /// A read started with no scroll from the user.
    pub fn counted(&mut self) {
        self.unprompted += 1;
    }

    /// The rows drew, at `at`, with a page being read or not: whether to read
    /// another with no scroll from the user.
    pub fn read_on(&mut self, at: Metrics, reading: bool) -> bool {
        if reading {
            return false;
        }
        let drew_nothing = self
            .reading
            .take()
            .is_some_and(|height| at.height <= height);
        (drew_nothing || !at.overflows()) && self.unprompted < UNPROMPTED_PAGES
    }
}

#[cfg(test)]
mod tests {
    use super::{HistoryScroll, UNPROMPTED_PAGES};
    use crate::follow::Metrics;

    const fn at(from_end: i32, height: i32) -> Metrics {
        Metrics {
            from_end,
            height,
            view: 500,
        }
    }

    #[test]
    fn only_a_move_up_the_content_did_not_make_is_the_user_moving_older() {
        let mut history = HistoryScroll::default();
        assert!(history.scrolled(at(100, 2000)));
        assert!(!history.scrolled(at(50, 2000)), "a move down");
        // The browser keeping the view in place as a reply grows below it.
        history.resized(at(90, 2040));
        assert!(!history.scrolled(at(90, 2040)));
        assert!(history.scrolled(at(120, 2040)));
    }

    #[test]
    fn a_page_that_drew_rows_that_fill_the_box_reads_no_more() {
        let mut history = HistoryScroll::default();
        history.started(2000);
        assert!(!history.read_on(at(900, 2000), true), "still being read");
        assert!(!history.read_on(at(900, 8000), false));
    }

    #[test]
    fn a_page_that_drew_nothing_reads_on_once() {
        let mut history = HistoryScroll::default();
        history.started(2000);
        assert!(history.read_on(at(1500, 2000), false));
        // The rows drawing again, with no read of its own, reads nothing.
        assert!(!history.read_on(at(1500, 2000), false));
    }

    #[test]
    fn rows_that_do_not_fill_the_box_read_on() {
        let mut history = HistoryScroll::default();
        assert!(history.read_on(at(0, 300), false));
    }

    #[test]
    fn reading_on_stops_at_the_limit_until_the_user_scrolls() {
        let mut history = HistoryScroll::default();
        for _ in 0..UNPROMPTED_PAGES {
            assert!(history.read_on(at(0, 300), false));
            history.counted();
        }
        assert!(!history.read_on(at(0, 300), false));
        history.prompted();
        assert!(history.read_on(at(0, 300), false));
    }
}
