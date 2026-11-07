//! A pass over a log's records (`docs/site/specs/forever-sessions.md` section
//! 3.3), for a reader whose answer can lie anywhere in the log: one record at a
//! time, keeping nothing, so that its memory does not depend on the log's length.
//!
//! Its time does: a pass is a read of the log, once a log no longer holds its
//! events. So a reader keeps only what it needs, and a question asked every turn
//! belongs in a fold ([`super::LogState`]).

use std::sync::Arc;

use cookie_agent_protocol::StoredEvent;

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

impl History {
    /// The records of `events`, in order, from position `start` to `end`.
    pub(super) fn new(events: EventSnapshot, start: usize, end: usize) -> Self {
        Self {
            events,
            next: start,
            end,
        }
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
