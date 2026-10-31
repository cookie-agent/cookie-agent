//! Test seams of `runtime.rs`.

use crate::runtime::*;

impl crate::runtime::Engine {
    pub(crate) fn block_plugin_diagnostic_appends_for_test(&self) {
        *self
            .inner
            .test_hooks
            .plugin_diagnostic_append_block
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) =
            Some(Arc::new(tokio::sync::Notify::new()));
    }

    pub(crate) fn pending_plugin_diagnostic_keys_for_test(&self) -> usize {
        self.inner.plugin_diagnostics.accumulator.key_count()
    }
}
