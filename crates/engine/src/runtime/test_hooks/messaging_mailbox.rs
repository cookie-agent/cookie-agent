//! Test seams of `runtime/messaging/mailbox.rs`.

use crate::runtime::*;

impl crate::runtime::Engine {
    pub(crate) fn install_prompt_before_claim_hook_for_test(
        &self,
    ) -> (oneshot::Receiver<()>, std::sync::Arc<tokio::sync::Notify>) {
        let (reached, receiver) = oneshot::channel();
        let release = std::sync::Arc::new(tokio::sync::Notify::new());
        *self
            .inner
            .test_hooks
            .prompt_before_claim_hook
            .lock()
            .expect("before-claim hook lock") =
            Some(std::sync::Arc::new(crate::runtime::PromptSnapshotHook {
                reached: std::sync::Mutex::new(Some(reached)),
                release: release.clone(),
            }));
        (receiver, release)
    }

    pub(crate) fn install_prompt_snapshot_hook_for_test(
        &self,
    ) -> (oneshot::Receiver<()>, std::sync::Arc<tokio::sync::Notify>) {
        let (reached, receiver) = oneshot::channel();
        let release = std::sync::Arc::new(tokio::sync::Notify::new());
        *self
            .inner
            .test_hooks
            .prompt_snapshot_hook
            .lock()
            .expect("snapshot hook lock") =
            Some(std::sync::Arc::new(crate::runtime::PromptSnapshotHook {
                reached: std::sync::Mutex::new(Some(reached)),
                release: release.clone(),
            }));
        (receiver, release)
    }

    pub(crate) async fn reconcile_producers_for_test(
        &self,
        session: SessionId,
    ) -> Result<(), EngineError> {
        self.reconcile_producers(session).await
    }

    pub(crate) fn compaction_reserved_for_test(&self, session: SessionId) -> bool {
        self.inner
            .compaction
            .in_progress
            .lock()
            .expect("compaction reservation lock poisoned")
            .contains(&session)
    }

    pub(crate) fn run_active_for_test(&self, run: RunId) -> bool {
        self.inner
            .sessions
            .active
            .lock()
            .expect("active runs lock poisoned")
            .contains_key(&run)
    }

    pub(crate) fn actor_resident_for_test(&self, session: SessionId) -> bool {
        self.inner
            .sessions
            .actors
            .lock()
            .expect("actor registry lock poisoned")
            .contains_key(&session)
    }

    pub(crate) fn install_compaction_execution_hook_for_test(
        &self,
    ) -> (oneshot::Receiver<()>, std::sync::Arc<tokio::sync::Notify>) {
        let (reached, receiver) = oneshot::channel();
        let release = std::sync::Arc::new(tokio::sync::Notify::new());
        *self
            .inner
            .test_hooks
            .compaction_execution_hook
            .lock()
            .expect("compaction execution hook lock poisoned") =
            Some(std::sync::Arc::new(PagingRaceHook {
                reached: std::sync::Mutex::new(Some(reached)),
                release: std::sync::Arc::clone(&release),
            }));
        (receiver, release)
    }

    pub(crate) async fn enqueue_compact_without_residency_for_test(
        &self,
        session: SessionId,
    ) -> Result<
        oneshot::Receiver<Result<cookie_agent_protocol::SessionCompactResult, EngineError>>,
        EngineError,
    > {
        let actor = self
            .inner
            .sessions
            .actors
            .lock()
            .expect("actor registry lock poisoned")
            .get(&session)
            .cloned()
            .ok_or(EngineError::MissingActor(session))?;
        let (reply, receiver) = oneshot::channel();
        let (command, _queued) = SessionCommand::call(true, move |engine| async move {
            engine.spawn_compaction(
                session,
                None,
                crate::runtime::event_origin("engine:auto-compact"),
                reply,
            );
            Ok(())
        });
        actor
            .send(command)
            .await
            .map_err(|_| EngineError::ActorStopped)?;
        Ok(receiver)
    }
}
