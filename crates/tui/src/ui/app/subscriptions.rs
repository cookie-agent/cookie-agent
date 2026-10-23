//! Per-session `events.subscribe` state on the current connection.
//!
//! Every subscribe costs a paged history replay over the wire, a staged
//! rebuild in the view, and a replacement of the connection's live tail, so
//! the view subscribes each session at most once while that subscription is
//! healthy. Opening, reselecting, and watching a session (and each tree
//! refresh) all go through [`App::subscribe_session`], which coalesces into
//! whatever the session already has.
//!
//! The state machine is `cookie_agent_client_state::subscriptions`, shared
//! with the web client; this module sends the requests it returns and feeds
//! their outcomes back. Whether a finished subscription is live depends on
//! ownership, which the view classifies.

use super::*;

use cookie_agent_client_state::subscriptions::{
    RECONNECT_FIRST_DELAY, SUBSCRIPTION_RETRY_DELAY, Subscribe, SubscriptionState,
    next_reconnect_delay,
};

impl App {
    /// Make sure `session_id` is subscribed on this connection, issuing an
    /// `events.subscribe` only when it has no usable subscription yet.
    pub(in crate::ui) fn subscribe_session(
        &mut self,
        session_id: SessionId,
        intent: SubscribeIntent,
    ) {
        let owned = self.owned_sessions.contains(&session_id);
        if let Some(subscribe) = self.subscriptions.subscribe(session_id, owned, intent) {
            self.send_subscribe(subscribe);
        }
    }

    /// Subscribe `session_id` with a full replay rather than from the view's
    /// cursor. The rebuilt projection replaces the current one when the
    /// replay ends, and output streaming now arrives whole: the view may
    /// have missed deltas while it had no subscription.
    pub(in crate::ui) fn resubscribe_from_scratch(&mut self, session_id: SessionId) {
        let owned = self.owned_sessions.contains(&session_id);
        let subscribe = self.subscriptions.subscribe_from_scratch(session_id, owned);
        self.send_subscribe(subscribe);
    }

    fn send_subscribe(&mut self, subscribe: Subscribe) {
        let Subscribe {
            session_id,
            attempt,
            from_scratch,
        } = subscribe;
        let cursor = if from_scratch {
            None
        } else {
            self.store
                .sessions
                .get(&session_id)
                .map(|state| state.last_seq)
        };
        let (finished, done) = tokio::sync::watch::channel(false);
        self.subscription_done.insert(session_id, done);
        let client = self.client.clone();
        let updates = self.rpc_updates_tx.clone();
        self.spawn_rpc(async move {
            // No outer deadline: the client bounds every replay page itself,
            // and a long history can take several pages.
            let outcome = match client.subscribe_events(session_id, cursor).await {
                Ok(()) => {
                    if let Ok(listed) = client.list_background(session_id).await {
                        let _ = updates.send(RpcUpdate::BackgroundCalls {
                            session_id,
                            calls: listed.calls,
                        });
                    }
                    SubscriptionOutcome::Established
                }
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
        let current = matches!(
            self.subscriptions.state(session_id),
            Some(SubscriptionState::Subscribing { attempt: current, .. }) if current == attempt
        );
        if current && let SubscriptionOutcome::Failed(error) = &outcome {
            self.session_errors.record(error);
            if self.selected == Some(session_id) {
                self.status = error.clone();
            }
        }
        let owned = self.owned_sessions.contains(&session_id);
        if let Some(subscribe) = self
            .subscriptions
            .finish(session_id, attempt, &outcome, owned)
        {
            self.send_subscribe(subscribe);
        }
    }

    /// A replay of `session_id` ended (already applied to the store).
    pub(super) fn note_replay_end(&mut self, session_id: SessionId) {
        let owned = self.owned_sessions.contains(&session_id);
        if let Some(subscribe) = self.subscriptions.note_replay_end(session_id, owned) {
            self.send_subscribe(subscribe);
        }
    }

    /// Rebuild `session_id` with a full replay: its projection can no longer
    /// be trusted. This is the one deliberate re-subscribe of a session that
    /// is already subscribed.
    pub(in crate::ui) fn recover_session(&mut self, session_id: SessionId) {
        self.client.recover_session(session_id, true);
        let owned = self.owned_sessions.contains(&session_id);
        self.subscriptions.recover(session_id, owned);
    }

    /// The client gave up recovering `session_id` (every subscription when
    /// `None`): the next open or tree refresh subscribes it afresh.
    pub(super) fn note_recovery_failed(&mut self, session_id: Option<SessionId>) {
        self.subscriptions.recovery_failed(session_id);
    }

    /// Ends this connection's live tail for `session_id`, keeping its
    /// projection: reopening it later catches up from its cursor. A session
    /// with a tail cannot be evicted by the daemon, so a TUI that has browsed
    /// many sessions releases the ones it no longer shows.
    pub(super) fn release_subscription(&mut self, session_id: SessionId) {
        if self.subscriptions.release(session_id) {
            self.send_unsubscribe(session_id);
        }
    }

    fn send_unsubscribe(&mut self, session_id: SessionId) {
        let client = self.client.clone();
        self.spawn_rpc(async move {
            // Best effort: a failure leaves a tail the connection's end drops.
            let _ = client.unsubscribe_events(session_id).await;
        });
    }

    /// Whether `session_id` belongs to a recently viewed tree, whose
    /// sessions keep their subscriptions.
    pub(super) fn is_recently_viewed(&self, session_id: SessionId) -> bool {
        self.subscriptions
            .is_recent_tree(self.permission_mode_root(session_id))
    }

    /// Whether nothing in `session_id` is still owed to this view: no run, and
    /// no pending or queued input whose start or return it must see.
    pub(super) fn is_settled(&self, session_id: SessionId) -> bool {
        self.store
            .sessions
            .get(&session_id)
            .is_none_or(crate::state::SessionState::is_settled)
    }

    /// Records `root` as the most recently viewed tree. The tree this pushes
    /// out of the recent set has its settled sessions released; ones still
    /// running are released when their run ends.
    pub(super) fn note_viewed_tree(&mut self, root: SessionId) {
        let mut subscriptions = std::mem::take(&mut self.subscriptions);
        let released = subscriptions.note_viewed_tree(
            root,
            |session_id| self.permission_mode_root(session_id),
            |session_id| self.is_settled(session_id),
        );
        self.subscriptions = subscriptions;
        for session_id in released {
            self.send_unsubscribe(session_id);
        }
    }

    /// The connection closed; nothing is subscribed on it any more.
    pub(super) fn reset_subscriptions(&mut self) {
        self.subscriptions.reset();
        self.subscription_done.clear();
    }

    /// Starts redialing the daemon after the connection dropped, when this
    /// TUI has a reconnector. Returns whether a reconnect is under way.
    pub(in crate::ui) fn begin_reconnect(&mut self) -> bool {
        let Some(reconnector) = self.reconnector.clone() else {
            return false;
        };
        self.reconnecting = true;
        self.reset_subscriptions();
        self.status = "daemon disconnected — reconnecting…".into();
        let updates = self.rpc_updates_tx.clone();
        self.spawn_rpc(async move {
            let mut delay = RECONNECT_FIRST_DELAY;
            loop {
                tokio::time::sleep(delay).await;
                if updates.is_closed() {
                    return;
                }
                match reconnector().await {
                    Ok(client) => {
                        let _ = updates.send(RpcUpdate::Reconnected { client });
                        return;
                    }
                    Err(error) => {
                        let _ = updates.send(RpcUpdate::Notice(format!(
                            "daemon disconnected — reconnecting ({error})"
                        )));
                        delay = next_reconnect_delay(delay);
                    }
                }
            }
        });
        true
    }

    /// Adopts the redialed connection and rebuilds what the old one fed:
    /// the viewed session from a full replay (which also brings back output
    /// streaming now), the session list, the runtime, approvals, the
    /// permission mode, and the Agents tree's subscriptions.
    pub(in crate::ui) async fn finish_reconnect(&mut self, client: crate::Client) {
        self.deliveries = client.subscribe_deliveries();
        self.client = client;
        self.reconnecting = false;
        self.reset_restart_after_reconnect();
        self.reset_subscriptions();
        self.status = "reconnected to the daemon".into();
        self.request_runtime_resync();
        self.refresh_lists().await;
        if let Some(selected) = self.selected {
            self.resubscribe_from_scratch(selected);
            self.refresh_approvals(selected);
            self.refresh_permission_mode_for_session(selected);
        }
        self.refresh_tree_background();
    }

    /// After the client gave up recovering a session, tries the viewed one
    /// again with a fresh subscription once the connection had time to
    /// settle. Other sessions resubscribe when next opened.
    pub(super) fn schedule_subscription_retry(&mut self, session_id: Option<SessionId>) {
        let Some(selected) = self.selected else {
            return;
        };
        if session_id.is_some_and(|session_id| session_id != selected) {
            return;
        }
        let updates = self.rpc_updates_tx.clone();
        self.spawn_rpc(async move {
            tokio::time::sleep(SUBSCRIPTION_RETRY_DELAY).await;
            let _ = updates.send(RpcUpdate::RetrySubscription {
                session_id: selected,
            });
        });
    }

    /// Wait (bounded) for an in-flight subscribe of `session_id` to deliver
    /// its replay. The replay itself is never cancelled.
    pub(super) async fn wait_for_subscription(&mut self, session_id: SessionId) {
        if !matches!(
            self.subscriptions.state(session_id),
            Some(SubscriptionState::Subscribing { .. })
        ) {
            return;
        }
        let Some(mut done) = self.subscription_done.get(&session_id).cloned() else {
            return;
        };
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
        match self.subscriptions.state(session_id) {
            None => "idle",
            Some(SubscriptionState::Subscribing { .. }) => "subscribing",
            Some(SubscriptionState::Live { owned: true }) => "live",
            Some(SubscriptionState::Live { owned: false }) => "snapshot",
            Some(SubscriptionState::Recovering { .. }) => "recovering",
        }
    }
}
