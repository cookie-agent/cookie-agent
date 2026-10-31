//! Test seams of `runtime/cron/fire.rs`.

use crate::runtime::*;

impl crate::runtime::Engine {
    pub(crate) fn install_cron_settle_hook(
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
            .cron_settle_hook
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) =
            Some(std::sync::Arc::new(crate::runtime::PagingRaceHook {
                reached: Mutex::new(Some(reached)),
                release: std::sync::Arc::clone(&release),
            }));
        (receiver, release)
    }

    pub(in crate::runtime) async fn wait_for_cron_settle_hook(&self) {
        let hook = self
            .inner
            .test_hooks
            .cron_settle_hook
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        if let Some(hook) = hook {
            if let Some(reached) = hook
                .reached
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .take()
            {
                let _ = reached.send(());
            }
            hook.release.notified().await;
        }
    }
}
