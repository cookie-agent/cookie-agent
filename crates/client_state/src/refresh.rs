//! Keeping a listing current when its reads overlap. The daemon answers a
//! connection's requests concurrently, so two reads of one listing can
//! land in either order: the older answer must not replace the newer.

/// The reads of one listing: only the answer to the newest read started is
/// installed, whatever order the answers land in. A value a notification
/// carried supersedes every read on its way, which may predate it.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Latest {
    started: u64,
}

/// One read of a [`Latest`] listing.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Ticket(u64);

impl Latest {
    /// Starts a read; every read started before it is now stale.
    pub fn begin(&mut self) -> Ticket {
        self.started = self.started.wrapping_add(1);
        Ticket(self.started)
    }

    /// Whether `ticket`'s read is the newest started: its answer may be
    /// installed.
    #[must_use]
    pub fn is_current(&self, ticket: Ticket) -> bool {
        self.started == ticket.0
    }

    /// A newer value than any read on its way could bring was installed:
    /// their answers are dropped.
    pub fn supersede(&mut self) {
        self.started = self.started.wrapping_add(1);
    }
}

#[cfg(test)]
mod tests {
    use super::Latest;

    #[test]
    fn an_older_answer_landing_last_is_dropped() {
        let mut latest = Latest::default();
        let first = latest.begin();
        let second = latest.begin();
        // The second answer lands first and is installed; the first, which
        // may have been read before a change the second saw, is not.
        assert!(latest.is_current(second));
        assert!(!latest.is_current(first));
    }

    #[test]
    fn a_lone_read_is_installed() {
        let mut latest = Latest::default();
        let only = latest.begin();
        assert!(latest.is_current(only));
        assert!(latest.is_current(only));
    }

    #[test]
    fn a_pushed_value_supersedes_the_reads_on_their_way() {
        let mut latest = Latest::default();
        let before = latest.begin();
        latest.supersede();
        assert!(!latest.is_current(before));
        // A read started after the push is current again.
        let after = latest.begin();
        assert!(latest.is_current(after));
    }
}
