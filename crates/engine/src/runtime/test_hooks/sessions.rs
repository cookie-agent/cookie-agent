//! Test seams of `runtime/sessions.rs`.

impl crate::runtime::Engine {
    /// Holds the next `submit_input` that steers between reading the session
    /// and reaching its run.
    pub(crate) fn install_submit_before_steer_hook_for_test(
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
            .submit_before_steer_hook
            .lock()
            .expect("submit before-steer hook lock") =
            Some(std::sync::Arc::new(crate::runtime::PromptSnapshotHook {
                reached: std::sync::Mutex::new(Some(reached)),
                release: release.clone(),
            }));
        (receiver, release)
    }

    /// `submit_input`'s side of [`Self::install_submit_before_steer_hook_for_test`].
    pub(crate) async fn pause_before_submit_steer(&self) {
        let hook = self
            .inner
            .test_hooks
            .submit_before_steer_hook
            .lock()
            .expect("submit before-steer hook lock")
            .take();
        if let Some(hook) = hook {
            if let Some(reached) = hook
                .reached
                .lock()
                .expect("submit before-steer reached lock")
                .take()
            {
                let _ = reached.send(());
            }
            hook.release.notified().await;
        }
    }

    pub(crate) fn install_read_only_reopen_hook_for_test(
        &self,
    ) -> (
        tokio::sync::oneshot::Receiver<()>,
        std::sync::mpsc::Sender<()>,
    ) {
        let (reached, receiver) = tokio::sync::oneshot::channel();
        let (release, release_receiver) = std::sync::mpsc::channel();
        *self
            .inner
            .test_hooks
            .read_only_reopen_hook
            .lock()
            .expect("read-only reopen hook lock poisoned") =
            Some(crate::runtime::ReadOnlyReopenHook {
                reached: std::sync::Mutex::new(Some(reached)),
                release: std::sync::Mutex::new(release_receiver),
            });
        (receiver, release)
    }
}
