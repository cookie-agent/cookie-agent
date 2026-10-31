//! Test seams of `runtime/delegation.rs`.

use crate::runtime::delegation::DelegationState;
use crate::runtime::*;
use cookie_agent_protocol::*;

impl crate::runtime::Engine {
    /// Holds the next delegation between its child's run going live and the
    /// delegation recording the run's id, until the returned notify fires.
    pub(crate) fn install_delegate_run_unrecorded_hook_for_test(
        &self,
    ) -> (oneshot::Receiver<()>, Arc<tokio::sync::Notify>) {
        let (reached, receiver) = oneshot::channel();
        let release = Arc::new(tokio::sync::Notify::new());
        *self
            .inner
            .test_hooks
            .delegate_run_unrecorded_hook
            .lock()
            .expect("delegate run hook lock") =
            Some(Arc::new(crate::runtime::test_hooks::PromptSnapshotHook {
                reached: Mutex::new(Some(reached)),
                release: release.clone(),
            }));
        (receiver, release)
    }

    /// Holds the next delegated child's run start right after its
    /// `RunStarted` is appended, until the returned notify fires.
    pub(crate) fn install_delegate_child_run_started_hook_for_test(
        &self,
    ) -> (oneshot::Receiver<()>, Arc<tokio::sync::Notify>) {
        let (reached, receiver) = oneshot::channel();
        let release = Arc::new(tokio::sync::Notify::new());
        *self
            .inner
            .test_hooks
            .delegate_child_run_started_hook
            .lock()
            .expect("child run started hook lock") =
            Some(Arc::new(crate::runtime::test_hooks::PromptSnapshotHook {
                reached: Mutex::new(Some(reached)),
                release: release.clone(),
            }));
        (receiver, release)
    }

    pub(crate) fn running_background_delegations_for_test(
        &self,
        root_session_id: SessionId,
    ) -> usize {
        self.inner
            .delegation
            .by_session
            .lock()
            .expect("delegation registry")
            .values()
            .filter(|record| {
                record.root_session_id == root_session_id
                    && record.counts_slot
                    && record.state == DelegationState::Running
            })
            .count()
    }

    pub(crate) fn delegation_queue_contains(
        &self,
        child_session_id: SessionId,
    ) -> Result<bool, EngineError> {
        Ok(self
            .inner
            .delegation
            .queue
            .lock()
            .map_err(|_| EngineError::ActorStopped)?
            .contains(&child_session_id))
    }

    pub(crate) async fn wait_for_delegation_reconciliation_for_test(&self) {
        while self
            .inner
            .delegation
            .reconciliation_running
            .load(Ordering::Acquire)
            || self
                .inner
                .delegation
                .reconciliation_requested
                .load(Ordering::Acquire)
        {
            tokio::task::yield_now().await;
        }
    }

    pub(crate) fn delegation_registry_snapshot(
        &self,
        child_session_id: SessionId,
    ) -> Result<(InvocationId, Option<SessionStatus>, bool), EngineError> {
        self.inner
            .delegation
            .by_session
            .lock()
            .map_err(|_| EngineError::ActorStopped)?
            .get(&child_session_id)
            .map(|record| {
                (
                    record.invocation_id,
                    match record.state {
                        DelegationState::Finished(status) => Some(status),
                        _ => None,
                    },
                    record.counts_slot,
                )
            })
            .ok_or_else(|| EngineError::ToolFailed("subagent registry entry is missing".into()))
    }

    pub(crate) fn install_resume_admission_hook(
        &self,
    ) -> (tokio::sync::oneshot::Receiver<()>, Arc<tokio::sync::Notify>) {
        let (reached, receiver) = tokio::sync::oneshot::channel();
        let release = Arc::new(tokio::sync::Notify::new());
        *self
            .inner
            .test_hooks
            .resume_admission_hook
            .lock()
            .expect("resume admission hook lock poisoned") =
            Some(Arc::new(crate::runtime::ResumeAdmissionHook {
                reached: std::sync::Mutex::new(Some(reached)),
                release: Arc::clone(&release),
            }));
        (receiver, release)
    }

    pub(crate) fn install_skill_fork_reservation_hook(
        &self,
    ) -> (oneshot::Receiver<()>, Arc<tokio::sync::Notify>) {
        let (reached, receiver) = oneshot::channel();
        let release = Arc::new(tokio::sync::Notify::new());
        *self
            .inner
            .test_hooks
            .skill_fork_reservation_hook
            .lock()
            .expect("skill fork reservation hook lock poisoned") =
            Some(Arc::new(crate::runtime::PagingRaceHook {
                reached: std::sync::Mutex::new(Some(reached)),
                release: Arc::clone(&release),
            }));
        (receiver, release)
    }

    pub(crate) fn install_delegation_reservation_hook(
        &self,
    ) -> (oneshot::Receiver<()>, Arc<tokio::sync::Notify>) {
        let (reached, receiver) = oneshot::channel();
        let release = Arc::new(tokio::sync::Notify::new());
        *self
            .inner
            .test_hooks
            .delegation_reservation_hook
            .lock()
            .expect("delegation reservation hook lock poisoned") =
            Some(Arc::new(crate::runtime::PagingRaceHook {
                reached: std::sync::Mutex::new(Some(reached)),
                release: Arc::clone(&release),
            }));
        (receiver, release)
    }

    pub(crate) fn install_resume_attachment_hook(
        &self,
    ) -> (tokio::sync::oneshot::Receiver<()>, Arc<tokio::sync::Notify>) {
        self.install_resume_test_hook(&self.inner.test_hooks.resume_attachment_hook)
    }

    pub(crate) fn install_resume_rollback_hook(
        &self,
    ) -> (tokio::sync::oneshot::Receiver<()>, Arc<tokio::sync::Notify>) {
        self.install_resume_test_hook(&self.inner.test_hooks.resume_rollback_hook)
    }

    pub(in crate::runtime) fn install_resume_test_hook(
        &self,
        slot: &std::sync::Mutex<Option<Arc<crate::runtime::ResumeAdmissionHook>>>,
    ) -> (tokio::sync::oneshot::Receiver<()>, Arc<tokio::sync::Notify>) {
        let (reached, receiver) = tokio::sync::oneshot::channel();
        let release = Arc::new(tokio::sync::Notify::new());
        *slot.lock().expect("resume test hook lock poisoned") =
            Some(Arc::new(crate::runtime::ResumeAdmissionHook {
                reached: std::sync::Mutex::new(Some(reached)),
                release: Arc::clone(&release),
            }));
        (receiver, release)
    }

    pub(in crate::runtime) async fn wait_for_resume_test_hook(
        &self,
        slot: &std::sync::Mutex<Option<Arc<crate::runtime::ResumeAdmissionHook>>>,
    ) {
        let hook = slot.lock().expect("resume test hook lock poisoned").clone();
        if let Some(hook) = hook {
            if let Some(reached) = hook
                .reached
                .lock()
                .expect("resume test hook reached lock poisoned")
                .take()
            {
                let _ = reached.send(());
            }
            hook.release.notified().await;
        }
    }

    pub(crate) fn set_delegation_slot_ownership(
        &self,
        child_session_id: SessionId,
        background: bool,
        counts_slot: bool,
    ) -> Result<(), EngineError> {
        let mut records = self
            .inner
            .delegation
            .by_session
            .lock()
            .map_err(|_| EngineError::ActorStopped)?;
        let record = records
            .get_mut(&child_session_id)
            .ok_or_else(|| EngineError::ToolFailed("subagent registry entry is missing".into()))?;
        record.background = background;
        record.counts_slot = counts_slot;
        Ok(())
    }

    /// Simulates a corrupted foreground record by registering a real delegation
    /// producer and writing it onto the child's record. The registry rebuild is
    /// expected to strip it again and retire the registration.
    pub(crate) async fn plant_foreground_delegation_producer_for_test(
        &self,
        child_session_id: SessionId,
    ) -> Result<ProducerId, EngineError> {
        let (parent_session_id, invocation_id) = {
            let records = self
                .inner
                .delegation
                .by_session
                .lock()
                .map_err(|_| EngineError::ActorStopped)?;
            let record = records.get(&child_session_id).ok_or_else(|| {
                EngineError::ToolFailed("subagent registry entry is missing".into())
            })?;
            (record.parent_session_id, record.invocation_id)
        };
        let mut guard = self
            .register_background_delegation_producer(parent_session_id, invocation_id)
            .await?;
        let producer_id = guard.producer_id;
        {
            let mut records = self
                .inner
                .delegation
                .by_session
                .lock()
                .map_err(|_| EngineError::ActorStopped)?;
            let record = records
                .get_mut(&child_session_id)
                .filter(|record| record.invocation_id == invocation_id)
                .ok_or_else(|| {
                    EngineError::ToolFailed("subagent registry entry is missing".into())
                })?;
            record.background = false;
            record.producer_id = Some(producer_id);
        }
        guard.handoff();
        Ok(producer_id)
    }

    pub(crate) fn rebuild_delegation_registry_for_test(
        &self,
        root: SessionId,
    ) -> Result<(), EngineError> {
        self.rebuild_delegation_registry(root, true)
    }

    pub(crate) fn delegation_producer_ownership_for_test(
        &self,
        child_session_id: SessionId,
    ) -> Result<(bool, Option<ProducerId>), EngineError> {
        self.inner
            .delegation
            .by_session
            .lock()
            .map_err(|_| EngineError::ActorStopped)?
            .get(&child_session_id)
            .map(|record| (record.background, record.producer_id))
            .ok_or_else(|| EngineError::ToolFailed("subagent registry entry is missing".into()))
    }
}
