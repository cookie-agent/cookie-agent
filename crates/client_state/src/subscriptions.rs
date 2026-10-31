//! Which sessions a client keeps subscribed, and when it tries again.
//!
//! A session with a live tail cannot be evicted by the daemon, so a client
//! that browses many sessions keeps only the trees it viewed most recently
//! subscribed ([`RECENT_SUBSCRIBED_TREES`]) and releases the settled
//! sessions of the others: a session with a run, or with input whose start
//! or return the view must still see, is released once it settles. The
//! client owns the clock and the requests; this only decides.
//!
//! [`Subscriptions`] is the per-connection state machine of every session's
//! `events.subscribe`. Every subscribe costs a paged history replay, a
//! staged rebuild in the view, and a replacement of the connection's live
//! tail, so a session is subscribed at most once while that subscription is
//! healthy. Whether a finished subscription is live depends on ownership,
//! which the client classifies: the daemon answers a session it does not own
//! with a read-only snapshot and registers no tail. The protocol client owns
//! cursors and replay staging and runs recovery replays on its own; this
//! machine only tracks them. Its methods return the [`Subscribe`] requests
//! the client must send, and the client reports how each ended.

use std::{
    collections::{HashMap, VecDeque},
    time::Duration,
};

use cookie_agent_protocol::SessionId;

use crate::SessionState;

/// Trees whose sessions stay subscribed after the view moves on.
pub const RECENT_SUBSCRIBED_TREES: usize = 8;

/// Wait before resubscribing a viewed session whose recovery gave up.
pub const SUBSCRIPTION_RETRY_DELAY: Duration = Duration::from_secs(2);

/// The waits before each redial of a dropped daemon: 500 ms, doubling per
/// failure up to 10 s.
pub const RECONNECT_BACKOFF: cookie_agent_identity::Backoff =
    cookie_agent_identity::Backoff::new(Duration::from_millis(500), Duration::from_secs(10));

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

/// Where one session's subscription stands on this connection. A session
/// without one is idle: never subscribed on this connection, or dropped
/// because its recovery failed or the connection closed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SubscriptionState {
    /// A subscribe this client requested is in flight.
    Subscribing {
        attempt: u64,
        /// The session was classified as owned when the request was made, so
        /// its final page registers a live tail.
        owned: bool,
        /// Ownership arrived after the request was made: its reply may be a
        /// tail-less snapshot, so subscribe once more when it finishes.
        resubscribe: bool,
        /// A replay of this session ended while the request was in flight.
        /// Only consulted when the request found another replay running.
        replay_ended: bool,
    },
    /// Replayed and following the live tail (a snapshot, unless `owned`).
    Live { owned: bool },
    /// A replay this client did not request (a recovery, or the protocol
    /// client's retry of a failed subscribe) is bringing the session up to
    /// date. It ends in the session's next `ReplayEnd`, or in
    /// `RecoveryFailed`.
    Recovering { owned: bool, resubscribe: bool },
}

/// Why the client asks for a session's subscription.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SubscribeIntent {
    /// Keep the session current: coalesce into any subscription it has.
    Follow,
    /// The user opened the session. As [`Self::Follow`], except that a
    /// finished snapshot, which no tail keeps current, is read again from
    /// the view's cursor.
    Open,
}

/// How a requested subscribe ended.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SubscriptionOutcome {
    Established,
    /// Another replay of the session was running.
    ReplayInProgress,
    Failed(String),
}

/// An `events.subscribe` the client must send for `session_id`: from the
/// view's cursor, or from the start when `from_scratch`. Its outcome goes
/// back to [`Subscriptions::finish`] with `attempt`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Subscribe {
    pub session_id: SessionId,
    pub attempt: u64,
    pub from_scratch: bool,
}

/// Every session's subscription on one connection, and the recently viewed
/// trees whose sessions stay subscribed.
#[derive(Clone, Debug, Default)]
pub struct Subscriptions {
    states: HashMap<SessionId, SubscriptionState>,
    next_attempt: u64,
    recent_trees: RecentTrees,
}

impl Subscriptions {
    /// `session_id`'s subscription, `None` while it is idle.
    pub fn state(&self, session_id: SessionId) -> Option<SubscriptionState> {
        self.states.get(&session_id).copied()
    }

    /// The attempt number of the latest subscribe this machine asked for;
    /// every request takes the next.
    pub fn last_attempt(&self) -> u64 {
        self.next_attempt
    }

    /// Whether `session_id` has a subscription in any state.
    pub fn contains(&self, session_id: SessionId) -> bool {
        self.states.contains_key(&session_id)
    }

    /// Makes sure `session_id` is subscribed, returning the request to send
    /// only when it has no usable subscription yet. `owned` is whether the
    /// client classified the session as owned.
    pub fn subscribe(
        &mut self,
        session_id: SessionId,
        owned: bool,
        intent: SubscribeIntent,
    ) -> Option<Subscribe> {
        match self.states.get_mut(&session_id) {
            None => {}
            Some(
                SubscriptionState::Subscribing {
                    owned: false,
                    resubscribe,
                    ..
                }
                | SubscriptionState::Recovering {
                    owned: false,
                    resubscribe,
                },
            ) if owned => {
                *resubscribe = true;
                return None;
            }
            // A snapshot becomes live once the session is owned, and an
            // opened snapshot is refreshed.
            Some(SubscriptionState::Live { owned: false })
                if owned || intent == SubscribeIntent::Open => {}
            Some(_) => return None,
        }
        Some(self.start(session_id, owned, false))
    }

    /// Subscribes `session_id` with a full replay rather than from the
    /// view's cursor, whatever it has: the rebuilt projection replaces the
    /// current one when the replay ends.
    pub fn subscribe_from_scratch(&mut self, session_id: SessionId, owned: bool) -> Subscribe {
        self.start(session_id, owned, true)
    }

    fn start(&mut self, session_id: SessionId, owned: bool, from_scratch: bool) -> Subscribe {
        self.next_attempt = self.next_attempt.wrapping_add(1);
        let attempt = self.next_attempt;
        self.states.insert(
            session_id,
            SubscriptionState::Subscribing {
                attempt,
                owned,
                resubscribe: false,
                replay_ended: false,
            },
        );
        Subscribe {
            session_id,
            attempt,
            from_scratch,
        }
    }

    /// The subscribe `attempt` of `session_id` ended with `outcome`;
    /// `owned_now` is the session's ownership now. Returns a follow-up
    /// subscribe to send, if any. A stale attempt changes nothing.
    pub fn finish(
        &mut self,
        session_id: SessionId,
        attempt: u64,
        outcome: &SubscriptionOutcome,
        owned_now: bool,
    ) -> Option<Subscribe> {
        let Some(&SubscriptionState::Subscribing {
            attempt: current,
            owned,
            resubscribe,
            replay_ended,
        }) = self.states.get(&session_id)
        else {
            return None;
        };
        if current != attempt {
            return None;
        }
        match outcome {
            SubscriptionOutcome::Established if resubscribe => {
                Some(self.start(session_id, true, false))
            }
            SubscriptionOutcome::Established => {
                self.states
                    .insert(session_id, SubscriptionState::Live { owned });
                None
            }
            SubscriptionOutcome::ReplayInProgress => {
                // Another replay (a recovery) holds the session and registers
                // the tail this one would have. It may have been requested
                // before the session was owned, so an owned subscription
                // follows it once it ends.
                let resubscribe = resubscribe || owned;
                if resubscribe && replay_ended {
                    Some(self.start(session_id, owned_now, false))
                } else {
                    self.states.insert(
                        session_id,
                        SubscriptionState::Recovering { owned, resubscribe },
                    );
                    None
                }
            }
            SubscriptionOutcome::Failed(_) => {
                // The protocol client retries a failed replay as a recovery
                // of it.
                self.states.insert(
                    session_id,
                    SubscriptionState::Recovering { owned, resubscribe },
                );
                None
            }
        }
    }

    /// A replay of `session_id` ended (already applied to the store);
    /// `owned_now` is its ownership now. Returns a follow-up subscribe, if
    /// a recovery owed one.
    pub fn note_replay_end(&mut self, session_id: SessionId, owned_now: bool) -> Option<Subscribe> {
        match self.states.get_mut(&session_id) {
            Some(SubscriptionState::Subscribing { replay_ended, .. }) => {
                *replay_ended = true;
                None
            }
            Some(&mut SubscriptionState::Recovering { owned, resubscribe }) => {
                if resubscribe {
                    Some(self.start(session_id, owned_now, false))
                } else {
                    self.states
                        .insert(session_id, SubscriptionState::Live { owned });
                    None
                }
            }
            Some(SubscriptionState::Live { .. }) | None => None,
        }
    }

    /// The client asked the protocol client to rebuild `session_id` with a
    /// full replay: its subscription is recovering, keeping a follow-up the
    /// superseded state still owed.
    pub fn recover(&mut self, session_id: SessionId, owned: bool) {
        let resubscribe = matches!(
            self.states.get(&session_id),
            Some(
                SubscriptionState::Subscribing {
                    resubscribe: true,
                    ..
                } | SubscriptionState::Recovering {
                    resubscribe: true,
                    ..
                }
            )
        );
        self.states.insert(
            session_id,
            SubscriptionState::Recovering { owned, resubscribe },
        );
    }

    /// The protocol client gave up recovering `session_id` (every
    /// subscription when `None`): the next open subscribes it afresh.
    /// Returns the viewed session when it is among them: the client calls
    /// [`Self::retry`] for it after [`SUBSCRIPTION_RETRY_DELAY`], once the
    /// connection had time to settle. Other sessions resubscribe when next
    /// opened.
    pub fn recovery_failed(
        &mut self,
        session_id: Option<SessionId>,
        viewed: Option<SessionId>,
    ) -> Option<SessionId> {
        match session_id {
            Some(session_id) => {
                self.states.remove(&session_id);
            }
            None => self.states.clear(),
        }
        viewed.filter(|viewed| session_id.is_none_or(|session_id| session_id == *viewed))
    }

    /// The retry timer [`Self::recovery_failed`] asked for ran out for
    /// `session_id`: the from-scratch subscribe to send when it is still
    /// the viewed session (`viewed`) and nothing subscribed it meanwhile.
    pub fn retry(
        &mut self,
        session_id: SessionId,
        viewed: Option<SessionId>,
        owned: bool,
    ) -> Option<Subscribe> {
        (viewed == Some(session_id) && !self.contains(session_id))
            .then(|| self.subscribe_from_scratch(session_id, owned))
    }

    /// Ends `session_id`'s subscription. Returns whether it had one, so the
    /// client sends `events.unsubscribe`.
    pub fn release(&mut self, session_id: SessionId) -> bool {
        self.states.remove(&session_id).is_some()
    }

    /// Whether the tree rooted at `root` was viewed recently.
    pub fn is_recent_tree(&self, root: SessionId) -> bool {
        self.recent_trees.contains(root)
    }

    /// Records `root` as the most recently viewed tree. Returns the
    /// sessions released because their tree left the recent set and they
    /// are settled, as `root_of` and `settled` tell; the client sends
    /// `events.unsubscribe` for each. Sessions still running are released
    /// once their run ends.
    pub fn note_viewed_tree(
        &mut self,
        root: SessionId,
        root_of: impl Fn(SessionId) -> SessionId,
        settled: impl Fn(SessionId) -> bool,
    ) -> Vec<SessionId> {
        let mut released = Vec::new();
        for evicted in self.recent_trees.note_viewed(root) {
            let mut sessions = self
                .states
                .keys()
                .copied()
                .filter(|session_id| root_of(*session_id) == evicted && settled(*session_id))
                .collect::<Vec<_>>();
            sessions.sort();
            for session_id in sessions {
                if self.release(session_id) {
                    released.push(session_id);
                }
            }
        }
        released
    }

    /// The connection closed: nothing is subscribed on it any more.
    pub fn reset(&mut self) {
        self.states.clear();
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
    use cookie_agent_protocol::SessionId;

    use super::{
        RECENT_SUBSCRIBED_TREES, RecentTrees, Subscribe, SubscribeIntent, SubscriptionOutcome,
        SubscriptionState, Subscriptions,
    };

    fn subscribe(session_id: SessionId, attempt: u64, from_scratch: bool) -> Option<Subscribe> {
        Some(Subscribe {
            session_id,
            attempt,
            from_scratch,
        })
    }

    #[test]
    fn a_session_is_subscribed_once_while_its_subscription_is_healthy() {
        let mut subscriptions = Subscriptions::default();
        let session = SessionId::new_v7();
        assert_eq!(
            subscriptions.subscribe(session, true, SubscribeIntent::Follow),
            subscribe(session, 1, false)
        );
        assert_eq!(
            subscriptions.subscribe(session, true, SubscribeIntent::Open),
            None
        );
        // A stale attempt changes nothing.
        assert_eq!(
            subscriptions.finish(session, 7, &SubscriptionOutcome::Established, true),
            None
        );
        assert!(matches!(
            subscriptions.state(session),
            Some(SubscriptionState::Subscribing { attempt: 1, .. })
        ));
        assert_eq!(
            subscriptions.finish(session, 1, &SubscriptionOutcome::Established, true),
            None
        );
        assert_eq!(
            subscriptions.state(session),
            Some(SubscriptionState::Live { owned: true })
        );
        assert_eq!(
            subscriptions.subscribe(session, true, SubscribeIntent::Open),
            None
        );
        assert!(subscriptions.release(session));
        assert!(!subscriptions.release(session));
        assert_eq!(subscriptions.last_attempt(), 1);
    }

    #[test]
    fn a_snapshot_is_reread_when_opened_and_goes_live_once_owned() {
        let mut subscriptions = Subscriptions::default();
        let session = SessionId::new_v7();
        subscriptions.subscribe(session, false, SubscribeIntent::Follow);
        subscriptions.finish(session, 1, &SubscriptionOutcome::Established, false);
        assert_eq!(
            subscriptions.state(session),
            Some(SubscriptionState::Live { owned: false })
        );
        assert_eq!(
            subscriptions.subscribe(session, false, SubscribeIntent::Follow),
            None
        );
        assert_eq!(
            subscriptions.subscribe(session, false, SubscribeIntent::Open),
            subscribe(session, 2, false)
        );
        // Ownership arriving mid-request owes one more subscribe.
        assert_eq!(
            subscriptions.subscribe(session, true, SubscribeIntent::Follow),
            None
        );
        assert_eq!(
            subscriptions.finish(session, 2, &SubscriptionOutcome::Established, true),
            subscribe(session, 3, false)
        );
        subscriptions.finish(session, 3, &SubscriptionOutcome::Established, true);
        assert_eq!(
            subscriptions.state(session),
            Some(SubscriptionState::Live { owned: true })
        );
    }

    #[test]
    fn a_running_replay_or_failure_leaves_the_session_recovering() {
        let mut subscriptions = Subscriptions::default();
        let session = SessionId::new_v7();
        subscriptions.subscribe(session, true, SubscribeIntent::Follow);
        // An owned subscription that found another replay follows it.
        assert_eq!(
            subscriptions.finish(session, 1, &SubscriptionOutcome::ReplayInProgress, true),
            None
        );
        assert_eq!(
            subscriptions.state(session),
            Some(SubscriptionState::Recovering {
                owned: true,
                resubscribe: true
            })
        );
        assert_eq!(
            subscriptions.note_replay_end(session, true),
            subscribe(session, 2, false)
        );
        // When the replay already ended, the follow-up goes at once.
        let other = SessionId::new_v7();
        subscriptions.subscribe(other, true, SubscribeIntent::Follow);
        assert_eq!(subscriptions.note_replay_end(other, true), None);
        assert_eq!(
            subscriptions.finish(other, 3, &SubscriptionOutcome::ReplayInProgress, true),
            subscribe(other, 4, false)
        );
        // A failure is retried by the protocol client as a recovery, which
        // ends live.
        let failed = SessionId::new_v7();
        subscriptions.subscribe(failed, false, SubscribeIntent::Follow);
        subscriptions.finish(
            failed,
            5,
            &SubscriptionOutcome::Failed("closed".into()),
            false,
        );
        assert_eq!(subscriptions.note_replay_end(failed, false), None);
        assert_eq!(
            subscriptions.state(failed),
            Some(SubscriptionState::Live { owned: false })
        );
        // A recovery that gives up leaves the session idle.
        subscriptions.recover(failed, false);
        assert_eq!(subscriptions.recovery_failed(Some(failed), None), None);
        assert_eq!(subscriptions.state(failed), None);
        subscriptions.recovery_failed(None, None);
        assert!(!subscriptions.contains(session));
        assert_eq!(
            subscriptions.subscribe_from_scratch(session, true),
            Subscribe {
                session_id: session,
                attempt: 6,
                from_scratch: true
            }
        );
    }

    #[test]
    fn a_failed_recovery_retries_the_viewed_session_while_it_stays_unsubscribed() {
        let viewed = SessionId::new_v7();
        let other = SessionId::new_v7();
        let mut subscriptions = Subscriptions::default();
        subscriptions.subscribe(viewed, true, SubscribeIntent::Open);
        subscriptions.subscribe(other, true, SubscribeIntent::Follow);

        // Another session's failure retries nothing; it resubscribes when
        // next opened.
        assert_eq!(
            subscriptions.recovery_failed(Some(other), Some(viewed)),
            None
        );
        assert!(!subscriptions.contains(other));
        assert_eq!(subscriptions.recovery_failed(Some(other), None), None);
        // The viewed session's own failure, or every session's, retries it.
        assert_eq!(
            subscriptions.recovery_failed(Some(viewed), Some(viewed)),
            Some(viewed)
        );
        assert_eq!(
            subscriptions.recovery_failed(None, Some(viewed)),
            Some(viewed)
        );

        // When the timer runs out the view moved on: nothing to send.
        assert_eq!(subscriptions.retry(viewed, Some(other), true), None);
        assert_eq!(subscriptions.retry(viewed, None, true), None);
        // Still viewed and unsubscribed: a full replay.
        let retry = subscriptions
            .retry(viewed, Some(viewed), false)
            .expect("the viewed session resubscribes");
        assert!(retry.from_scratch);
        assert_eq!(retry.session_id, viewed);
        assert_eq!(
            subscriptions.state(viewed),
            Some(SubscriptionState::Subscribing {
                attempt: retry.attempt,
                owned: false,
                resubscribe: false,
                replay_ended: false,
            })
        );
        // Subscribed meanwhile (here by the retry itself): no second one.
        assert_eq!(subscriptions.retry(viewed, Some(viewed), false), None);
    }

    #[test]
    fn trees_pushed_out_of_the_recent_set_release_their_settled_sessions() {
        let mut subscriptions = Subscriptions::default();
        let roots = (0..=RECENT_SUBSCRIBED_TREES)
            .map(|_| SessionId::new_v7())
            .collect::<Vec<_>>();
        let busy = SessionId::new_v7();
        for root in &roots[..RECENT_SUBSCRIBED_TREES] {
            subscriptions.subscribe(*root, true, SubscribeIntent::Follow);
            assert!(
                subscriptions
                    .note_viewed_tree(*root, |session| session, |_| true)
                    .is_empty()
            );
        }
        subscriptions.subscribe(busy, true, SubscribeIntent::Follow);
        let oldest = roots[0];
        let root_of = |session: SessionId| if session == busy { oldest } else { session };
        let released =
            subscriptions.note_viewed_tree(roots[RECENT_SUBSCRIBED_TREES], root_of, |session| {
                session != busy
            });
        assert_eq!(released, vec![oldest]);
        assert!(subscriptions.contains(busy), "a running session stays");
        assert!(!subscriptions.is_recent_tree(oldest));
        subscriptions.reset();
        assert!(!subscriptions.contains(busy));
    }

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
}
