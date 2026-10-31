//! Test seams of `runtime/sessions/residency.rs`.

use std::time::Duration;

use jiff::Timestamp;

use crate::runtime::delegation::DelegationState;
use crate::runtime::*;
use cookie_agent_protocol::*;

impl crate::runtime::Engine {
    pub(crate) async fn evict_idle_subagents_for_test(
        &self,
        cap: usize,
        idle_after: Duration,
    ) -> Result<Vec<SessionId>, EngineError> {
        self.evict_idle_subagents_with(cap, idle_after).await
    }

    pub(crate) fn set_delegation_queued_for_test(&self, session_id: SessionId, queued: bool) {
        if let Some(record) = self
            .inner
            .delegation
            .by_session
            .lock()
            .expect("delegation registry lock poisoned")
            .get_mut(&session_id)
        {
            record.state = if queued {
                DelegationState::Queued
            } else {
                DelegationState::Finished(SessionStatus::Completed)
            };
        }
    }

    pub(crate) fn set_notification_sent_for_test(&self, session_id: SessionId, sent: bool) {
        if let Some(record) = self
            .inner
            .delegation
            .by_session
            .lock()
            .expect("delegation registry lock poisoned")
            .get_mut(&session_id)
        {
            record.notification_sent = sent;
        }
    }

    pub(crate) fn subagent_eviction_eligible_for_test(
        &self,
        session_id: SessionId,
        idle_after: Duration,
    ) -> bool {
        self.inner
            .store
            .get_resident(session_id)
            .is_some_and(|session| {
                self.subagent_eviction_ended_at(&session, Timestamp::now(), idle_after)
                    .is_some()
            })
    }

    pub(crate) fn delegation_finished_for_test(&self, session_id: SessionId) -> bool {
        self.inner
            .delegation
            .by_session
            .lock()
            .expect("delegation registry lock poisoned")
            .get(&session_id)
            .is_some_and(|record| matches!(record.state, DelegationState::Finished(_)))
    }

    pub(crate) fn delegation_state_for_test(&self, session_id: SessionId) -> String {
        self.inner
            .delegation
            .by_session
            .lock()
            .expect("delegation registry lock poisoned")
            .get(&session_id)
            .map_or_else(|| "missing".into(), |record| format!("{:?}", record.state))
    }

    pub(crate) fn install_janitor_before_barrier_hook_for_test(
        &self,
    ) -> (
        tokio::sync::oneshot::Receiver<()>,
        std::sync::Arc<tokio::sync::Notify>,
    ) {
        let (reached, receiver) = tokio::sync::oneshot::channel();
        let release = std::sync::Arc::new(tokio::sync::Notify::new());
        *self
            .inner
            .test_hooks
            .janitor_before_barrier_hook
            .lock()
            .expect("janitor barrier hook lock poisoned") =
            Some(std::sync::Arc::new(PagingRaceHook {
                reached: std::sync::Mutex::new(Some(reached)),
                release: std::sync::Arc::clone(&release),
            }));
        (receiver, release)
    }

    pub(crate) fn install_acp_eviction_close_hook(
        &self,
    ) -> (
        tokio::sync::oneshot::Receiver<()>,
        std::sync::Arc<tokio::sync::Notify>,
    ) {
        let (reached, receiver) = tokio::sync::oneshot::channel();
        let release = std::sync::Arc::new(tokio::sync::Notify::new());
        *self
            .inner
            .test_hooks
            .acp_eviction_close_hook
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) =
            Some(std::sync::Arc::new(PagingRaceHook {
                reached: std::sync::Mutex::new(Some(reached)),
                release: std::sync::Arc::clone(&release),
            }));
        (receiver, release)
    }
}
