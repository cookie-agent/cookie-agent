//! Test seams of `runtime/sessions.rs`.

impl crate::runtime::Engine {
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
