//! A pass over a log's records (`docs/site/specs/forever-sessions.md` section
//! 3.3), for a reader whose answer can lie anywhere in the log: one record at a
//! time, keeping nothing, so that its memory does not depend on the log's length.
//!
//! Its time does: a pass is a read of the log, once a log no longer holds its
//! events. So a reader keeps only what it needs, and a question asked every turn
//! belongs in a fold ([`super::LogState`]).

use std::sync::Arc;

use cookie_agent_protocol::{
    EventsView, StoredEvent,
    display_view::{self, OlderPageReader, TailWindowReader, Window},
};

use super::{EventLogError, EventSnapshot};

/// The records of a log as it stood when the pass was asked for, in order. It
/// holds no lock of the log, and may be read for as long as it takes.
///
/// Each record is fallible: it is read when it is asked for, so damage in old
/// records is found here (section 11, decision 1). Nothing else may be assumed
/// of where the records come from: while a log holds all its events this reads
/// them from there, and it is the only way to read past a log's window.
#[derive(Debug)]
pub struct History {
    events: EventSnapshot,
    next: usize,
    end: usize,
}

/// Where a view cache starts: [`History::view_cache_start`].
pub type ViewCacheStart = Option<(Arc<StoredEvent>, Vec<Arc<StoredEvent>>)>;

/// An older page, read by [`History::older_page`].
#[derive(Debug)]
pub struct OlderPage {
    pub window: Window,
    /// [`display_view::state_events`] of every record of the pass, when asked
    /// for: the header of what starts where the page ends.
    pub header_at_end: Option<Vec<Arc<StoredEvent>>>,
}

impl History {
    /// The records of `events`, in order, from position `start` to `end`.
    pub(super) fn new(events: EventSnapshot, start: usize, end: usize) -> Self {
        Self {
            events,
            next: start,
            end,
        }
    }

    /// The records of this pass, when the log holds them all: a window need
    /// measure only what fits, walking back from the end, and a pass must
    /// measure every record to know that (section 2: a log not yet windowed
    /// costs nothing).
    fn held(&self) -> Option<&[Arc<StoredEvent>]> {
        Some(&self.events[self.next..self.end])
    }

    /// The newest-first window of this pass, which covers a whole log.
    pub fn tail_window(self, view: EventsView, budget: usize) -> Result<Window, EventLogError> {
        let Some(events) = self.held() else {
            return self.read_tail_window(view, budget);
        };
        let window = display_view::tail_window(events, view, budget);
        #[cfg(any(test, feature = "test-support"))]
        assert_eq!(
            self.read_tail_window(view, budget)?,
            window,
            "a window read as a pass differs"
        );
        Ok(window)
    }

    fn read_tail_window(self, view: EventsView, budget: usize) -> Result<Window, EventLogError> {
        let mut reader = TailWindowReader::new(view, budget);
        for event in self {
            reader.observe(event?);
        }
        Ok(reader.window())
    }

    /// Where a view cache of this pass, which covers a whole log, starts: the
    /// record, with the header of what precedes it. `None` when it all fits.
    pub fn view_cache_start(self, budget: usize) -> Result<ViewCacheStart, EventLogError> {
        let Some(events) = self.held() else {
            return self.read_view_cache_start(budget);
        };
        let start = display_view::cache_start(events, budget).map(|start| {
            (
                Arc::clone(&events[start]),
                display_view::state_events(&events[..start]),
            )
        });
        #[cfg(any(test, feature = "test-support"))]
        assert_eq!(
            self.read_view_cache_start(budget)?,
            start,
            "a view cache's start read as a pass differs"
        );
        Ok(start)
    }

    fn read_view_cache_start(self, budget: usize) -> Result<ViewCacheStart, EventLogError> {
        let mut reader = TailWindowReader::new(EventsView::Display, budget);
        for event in self {
            reader.observe(event?);
        }
        Ok(reader.cache_start())
    }

    /// The page ending where this pass does, which starts at the log's start.
    pub fn older_page(
        self,
        view: EventsView,
        budget: usize,
        want_header_at_end: bool,
    ) -> Result<OlderPage, EventLogError> {
        let Some(events) = self.held() else {
            return self.read_older_page(view, budget, want_header_at_end);
        };
        let before = events.last().map_or(0, |event| event.seq + 1);
        let page = OlderPage {
            window: display_view::older_page(events, before, view, budget),
            header_at_end: want_header_at_end.then(|| display_view::state_events(events)),
        };
        #[cfg(any(test, feature = "test-support"))]
        {
            let read = self.read_older_page(view, budget, want_header_at_end)?;
            assert_eq!(read.window, page.window, "a page read as a pass differs");
            assert_eq!(
                read.header_at_end, page.header_at_end,
                "a page's header read as a pass differs"
            );
        }
        Ok(page)
    }

    fn read_older_page(
        self,
        view: EventsView,
        budget: usize,
        want_header_at_end: bool,
    ) -> Result<OlderPage, EventLogError> {
        let mut reader = OlderPageReader::new(view, budget);
        for event in self {
            reader.observe(event?);
        }
        let header_at_end = want_header_at_end.then(|| reader.header_at_end());
        Ok(OlderPage {
            window: reader.finish(),
            header_at_end,
        })
    }

    /// The first answer `find` gives, stopping there.
    pub fn find_map_ok<T>(
        self,
        mut find: impl FnMut(&StoredEvent) -> Option<T>,
    ) -> Result<Option<T>, EventLogError> {
        for event in self {
            if let Some(found) = find(&*event?) {
                return Ok(Some(found));
            }
        }
        Ok(None)
    }

    /// The last answer `find` gives: the one a scan from the log's tip would
    /// meet first. It reads to the end, keeping only the latest answer.
    pub fn last_map_ok<T>(
        self,
        mut find: impl FnMut(&StoredEvent) -> Option<T>,
    ) -> Result<Option<T>, EventLogError> {
        let mut latest = None;
        for event in self {
            if let Some(found) = find(&*event?) {
                latest = Some(found);
            }
        }
        Ok(latest)
    }

    /// Calls `visit` with every record.
    pub fn for_each_ok(
        self,
        mut visit: impl FnMut(&Arc<StoredEvent>),
    ) -> Result<(), EventLogError> {
        for event in self {
            visit(&event?);
        }
        Ok(())
    }
}

impl Iterator for History {
    type Item = Result<Arc<StoredEvent>, EventLogError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.next >= self.end {
            return None;
        }
        let event = Arc::clone(&self.events[self.next]);
        self.next += 1;
        Some(Ok(event))
    }
}
