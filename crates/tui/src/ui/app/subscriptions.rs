//! Per-session `events.subscribe` state on the current connection.
//!
//! Every subscribe costs a paged history replay over the wire, a staged
//! rebuild in the view, and a replacement of the connection's live tail, so
//! the view subscribes each session at most once while that subscription is
//! healthy. Opening, reselecting, and watching a session (and each tree
//! refresh) all go through [`App::subscribe_session`], which coalesces into
//! whatever the session already has.
//!
//! The state lives here rather than in the protocol client because whether a
//! finished subscription is live depends on ownership, which only the view
//! classifies: the daemon answers a session it does not own with a read-only
//! snapshot and registers no tail. The client still owns cursors and replay
//! staging, and runs recovery replays (gaps, failed replays, and the view's
//! own [`App::recover_session`]) on its own; this machine only tracks them.

use super::*;

/// Where one session's subscription stands on this connection. A session
/// without an entry is idle: never subscribed on this connection, or dropped
/// because its recovery failed or the connection closed.
pub(in crate::ui) enum SubscriptionState {
    /// A subscribe this view requested is in flight.
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
        /// Becomes `true` once the request has finished, for callers that
        /// wait on the replay.
        done: tokio::sync::watch::Receiver<bool>,
    },
    /// Replayed and following the live tail (a snapshot, unless `owned`).
    Live { owned: bool },
    /// A replay this view did not request (a recovery, or the client's retry
    /// of a failed subscribe) is bringing the session up to date. It ends in
    /// the session's next `ReplayEnd`, or in `RecoveryFailed`.
    Recovering { owned: bool, resubscribe: bool },
}

/// Why the view asks for a session's subscription.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::ui) enum SubscribeIntent {
    /// Keep the session current: coalesce into any subscription it has.
    Follow,
    /// The user opened the session. As [`Self::Follow`], except that a
    /// finished snapshot, which no tail keeps current, is read again from
    /// the view's cursor.
    Open,
}

pub(in crate::ui) enum SubscriptionOutcome {
    Established,
    ReplayInProgress,
    Failed(String),
}

impl App {
    /// Make sure `session_id` is subscribed on this connection, issuing an
    /// `events.subscribe` only when it has no usable subscription yet.
    pub(in crate::ui) fn subscribe_session(
        &mut self,
        session_id: SessionId,
        intent: SubscribeIntent,
    ) {
        let owned = self.owned_sessions.contains(&session_id);
        match self.subscriptions.get_mut(&session_id) {
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
                return;
            }
            // A snapshot becomes live once the session is owned, and an
            // opened snapshot is refreshed.
            Some(SubscriptionState::Live { owned: false })
                if owned || intent == SubscribeIntent::Open => {}
            Some(_) => return,
        }
        self.start_subscription(session_id, owned);
    }

    fn start_subscription(&mut self, session_id: SessionId, owned: bool) {
        self.next_subscription_attempt = self.next_subscription_attempt.wrapping_add(1);
        let attempt = self.next_subscription_attempt;
        let cursor = self
            .store
            .sessions
            .get(&session_id)
            .map(|state| state.last_seq);
        let (finished, done) = tokio::sync::watch::channel(false);
        self.subscriptions.insert(
            session_id,
            SubscriptionState::Subscribing {
                attempt,
                owned,
                resubscribe: false,
                replay_ended: false,
                done,
            },
        );
        let client = self.client.clone();
        let updates = self.rpc_updates_tx.clone();
        self.spawn_rpc(async move {
            // No outer deadline: the client bounds every replay page itself,
            // and a long history can take several pages.
            let outcome = match client.subscribe_events(session_id, cursor).await {
                Ok(()) => SubscriptionOutcome::Established,
                Err(crate::ClientError::ReplayInProgress) => SubscriptionOutcome::ReplayInProgress,
                Err(error) => SubscriptionOutcome::Failed(error.to_string()),
            };
            let _ = finished.send(true);
            let _ = updates.send(RpcUpdate::SubscriptionFinished {
                session_id,
                attempt,
                outcome,
            });
        });
    }

    pub(super) fn finish_subscription(
        &mut self,
        session_id: SessionId,
        attempt: u64,
        outcome: SubscriptionOutcome,
    ) {
        let Some(&SubscriptionState::Subscribing {
            attempt: current,
            owned,
            resubscribe,
            replay_ended,
            ..
        }) = self.subscriptions.get(&session_id)
        else {
            return;
        };
        if current != attempt {
            return;
        }
        match outcome {
            SubscriptionOutcome::Established if resubscribe => {
                self.start_subscription(session_id, true);
            }
            SubscriptionOutcome::Established => {
                self.subscriptions
                    .insert(session_id, SubscriptionState::Live { owned });
            }
            SubscriptionOutcome::ReplayInProgress => {
                // Another replay (a recovery) holds the session and registers
                // the tail this one would have. It may have been requested
                // before the session was owned, so an owned subscription
                // follows it once it ends.
                let resubscribe = resubscribe || owned;
                if resubscribe && replay_ended {
                    self.start_subscription(session_id, self.owned_sessions.contains(&session_id));
                } else {
                    self.subscriptions.insert(
                        session_id,
                        SubscriptionState::Recovering { owned, resubscribe },
                    );
                }
            }
            SubscriptionOutcome::Failed(error) => {
                self.session_errors.record(&error);
                if self.selected == Some(session_id) {
                    self.status = error;
                }
                // The client retries a failed replay as a recovery of it.
                self.subscriptions.insert(
                    session_id,
                    SubscriptionState::Recovering { owned, resubscribe },
                );
            }
        }
    }

    /// A replay of `session_id` ended (already applied to the store).
    pub(super) fn note_replay_end(&mut self, session_id: SessionId) {
        match self.subscriptions.get_mut(&session_id) {
            Some(SubscriptionState::Subscribing { replay_ended, .. }) => *replay_ended = true,
            Some(&mut SubscriptionState::Recovering { owned, resubscribe }) => {
                if resubscribe {
                    self.start_subscription(session_id, self.owned_sessions.contains(&session_id));
                } else {
                    self.subscriptions
                        .insert(session_id, SubscriptionState::Live { owned });
                }
            }
            Some(SubscriptionState::Live { .. }) | None => {}
        }
    }

    /// Rebuild `session_id` with a full replay: its projection can no longer
    /// be trusted. This is the one deliberate re-subscribe of a session that
    /// is already subscribed.
    pub(in crate::ui) fn recover_session(&mut self, session_id: SessionId) {
        self.client.recover_session(session_id, true);
        // A follow-up the superseded state still owed is kept.
        let resubscribe = matches!(
            self.subscriptions.get(&session_id),
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
        self.subscriptions.insert(
            session_id,
            SubscriptionState::Recovering {
                owned: self.owned_sessions.contains(&session_id),
                resubscribe,
            },
        );
    }

    /// The client gave up recovering `session_id` (every subscription when
    /// `None`): the next open or tree refresh subscribes it afresh.
    pub(super) fn note_recovery_failed(&mut self, session_id: Option<SessionId>) {
        match session_id {
            Some(session_id) => {
                self.subscriptions.remove(&session_id);
            }
            None => self.subscriptions.clear(),
        }
    }

    /// The connection closed; nothing is subscribed on it any more.
    pub(super) fn reset_subscriptions(&mut self) {
        self.subscriptions.clear();
    }

    /// Wait (bounded) for an in-flight subscribe of `session_id` to deliver
    /// its replay. The replay itself is never cancelled.
    pub(super) async fn wait_for_subscription(&mut self, session_id: SessionId) {
        let Some(SubscriptionState::Subscribing { done, .. }) = self.subscriptions.get(&session_id)
        else {
            return;
        };
        let mut done = done.clone();
        if tokio::time::timeout(SELECT_SUBSCRIPTION_WAIT, done.wait_for(|done| *done))
            .await
            .is_err()
        {
            // The replay keeps going without us and lands through the
            // delivery stream; only the wait is bounded.
            self.status = "still loading session history".into();
        }
    }

    #[cfg(test)]
    pub(crate) fn subscription_state_for_test(&self, session_id: SessionId) -> &'static str {
        match self.subscriptions.get(&session_id) {
            None => "idle",
            Some(SubscriptionState::Subscribing { .. }) => "subscribing",
            Some(SubscriptionState::Live { owned: true }) => "live",
            Some(SubscriptionState::Live { owned: false }) => "snapshot",
            Some(SubscriptionState::Recovering { .. }) => "recovering",
        }
    }
}
