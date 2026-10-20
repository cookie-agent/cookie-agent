//! Which sessions a client keeps subscribed, and when it tries again.
//!
//! A session with a live tail cannot be evicted by the daemon, so a client
//! that browses many sessions keeps only the trees it viewed most recently
//! subscribed ([`RECENT_SUBSCRIBED_TREES`]) and releases the settled
//! sessions of the others: a session with a run, or with input whose start
//! or return the view must still see, is released once it settles. The
//! client owns the clock and the requests; this only decides.

use std::{collections::VecDeque, time::Duration};

use cookie_agent_protocol::SessionId;

use crate::SessionState;

/// Trees whose sessions stay subscribed after the view moves on.
pub const RECENT_SUBSCRIBED_TREES: usize = 8;

/// Wait before resubscribing a viewed session whose recovery gave up.
pub const SUBSCRIPTION_RETRY_DELAY: Duration = Duration::from_secs(2);

/// First wait before redialing a dropped daemon; doubles per failure up to
/// [`RECONNECT_MAX_DELAY`].
pub const RECONNECT_FIRST_DELAY: Duration = Duration::from_millis(500);

/// The longest wait between redials of a dropped daemon.
pub const RECONNECT_MAX_DELAY: Duration = Duration::from_secs(10);

/// The wait before the redial after one that waited `delay` and failed.
pub fn next_reconnect_delay(delay: Duration) -> Duration {
    (delay * 2).min(RECONNECT_MAX_DELAY)
}

/// The roots of the trees viewed most recently, newest first.
#[derive(Clone, Debug, Default)]
pub struct RecentTrees(VecDeque<SessionId>);

impl RecentTrees {
    /// Whether the tree rooted at `root` is one of the recent ones.
    pub fn contains(&self, root: SessionId) -> bool {
        self.0.contains(&root)
    }

    /// Records `root` as the most recently viewed tree and returns the
    /// roots this pushed out of the recent set, oldest last: their settled
    /// sessions are released.
    pub fn note_viewed(&mut self, root: SessionId) -> Vec<SessionId> {
        self.0.retain(|recent| *recent != root);
        self.0.push_front(root);
        let mut evicted = Vec::new();
        while self.0.len() > RECENT_SUBSCRIBED_TREES {
            let Some(oldest) = self.0.pop_back() else {
                break;
            };
            evicted.push(oldest);
        }
        evicted
    }
}

impl SessionState {
    /// Whether nothing in the session is still owed to its view: no run,
    /// and no pending or queued input whose start or return it must see.
    pub fn is_settled(&self) -> bool {
        self.active_run.is_none() && self.pending_inputs.is_empty() && self.queued_inputs.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use cookie_agent_protocol::SessionId;

    use super::{RECENT_SUBSCRIBED_TREES, RECONNECT_MAX_DELAY, RecentTrees, next_reconnect_delay};

    #[test]
    fn viewing_more_trees_than_are_kept_evicts_the_least_recent() {
        let mut recent = RecentTrees::default();
        let roots = (0..=RECENT_SUBSCRIBED_TREES)
            .map(|_| SessionId::new_v7())
            .collect::<Vec<_>>();
        for root in &roots[..RECENT_SUBSCRIBED_TREES] {
            assert!(recent.note_viewed(*root).is_empty());
        }
        // Viewing a kept tree again makes it the newest.
        assert!(recent.note_viewed(roots[0]).is_empty());
        assert_eq!(
            recent.note_viewed(roots[RECENT_SUBSCRIBED_TREES]),
            vec![roots[1]]
        );
        assert!(!recent.contains(roots[1]));
        assert!(recent.contains(roots[0]));
    }

    #[test]
    fn redials_back_off_up_to_the_longest_wait() {
        assert_eq!(
            next_reconnect_delay(Duration::from_millis(700)),
            Duration::from_millis(1400)
        );
        assert_eq!(
            next_reconnect_delay(RECONNECT_MAX_DELAY),
            RECONNECT_MAX_DELAY
        );
    }
}
