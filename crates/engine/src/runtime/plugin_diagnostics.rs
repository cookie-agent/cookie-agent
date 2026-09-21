//! Plugin diagnostic accumulation and the drain task that appends batched
//! diagnostics to session logs.

use std::{
    collections::{HashMap, HashSet},
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, Ordering},
    },
};

use cookie_agent_protocol::SessionId;
use tokio::task::JoinHandle;

use super::{Engine, Event, Inner, SessionCommand};

const PLUGIN_DIAGNOSTIC_BATCH_DELAY: std::time::Duration = std::time::Duration::from_millis(100);

pub(super) type PluginDiagnosticKey = (
    SessionId,
    String,
    cookie_agent_protocol::PluginDiagnosticKind,
    String,
);
pub(super) type PluginDiagnosticGroup = (
    SessionId,
    String,
    cookie_agent_protocol::PluginDiagnosticKind,
);

const PLUGIN_DIAGNOSTIC_MESSAGE_CHARS: usize = 200;
const PLUGIN_DIAGNOSTIC_DETAIL_KEYS: usize = 256;
const PLUGIN_DIAGNOSTIC_OVERFLOW_MESSAGE: &str = "(overflow)";
const PLUGIN_DIAGNOSTIC_APPEND_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(250);
#[cfg(not(all(test, windows)))]
pub(super) const PLUGIN_DIAGNOSTIC_SHUTDOWN_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(5);
// Windows CI's coalescing test drains up to 257 synced appends under parallel load.
#[cfg(all(test, windows))]
pub(super) const PLUGIN_DIAGNOSTIC_SHUTDOWN_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(30);

#[derive(Debug, Default)]
pub(super) struct PendingPluginDiagnostics {
    details: HashMap<PluginDiagnosticKey, u64>,
    overflow: HashMap<PluginDiagnosticGroup, u64>,
}

#[derive(Debug, Default)]
pub(crate) struct PluginDiagnosticAccumulator {
    pending: Mutex<PendingPluginDiagnostics>,
    pub(crate) notify: tokio::sync::Notify,
    pub(crate) shutdown: AtomicBool,
    active_plugin: Mutex<Option<String>>,
}

impl PluginDiagnosticAccumulator {
    pub(crate) fn record(&self, key: PluginDiagnosticKey, count: u64) {
        let (session_id, plugin, kind, message) = key;
        let message = normalize_plugin_diagnostic_message(&message);
        let key = (session_id, plugin.clone(), kind, message);
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let total = if pending.details.contains_key(&key)
            || pending.details.len() < PLUGIN_DIAGNOSTIC_DETAIL_KEYS
        {
            pending.details.entry(key).or_default()
        } else {
            pending
                .overflow
                .entry((session_id, plugin, kind))
                .or_default()
        };
        *total = total.saturating_add(count);
        drop(pending);
        self.notify.notify_one();
    }

    pub(super) fn take(&self) -> Vec<(PluginDiagnosticKey, u64)> {
        let pending = std::mem::take(
            &mut *self
                .pending
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        );
        let mut records = pending.details.into_iter().collect::<Vec<_>>();
        records.extend(
            pending
                .overflow
                .into_iter()
                .map(|((session_id, plugin, kind), count)| {
                    (
                        (
                            session_id,
                            plugin,
                            kind,
                            PLUGIN_DIAGNOSTIC_OVERFLOW_MESSAGE.into(),
                        ),
                        count,
                    )
                }),
        );
        records
    }

    pub(super) fn is_empty(&self) -> bool {
        let pending = self
            .pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        pending.details.is_empty() && pending.overflow.is_empty()
    }

    #[cfg(test)]
    pub(crate) fn key_count(&self) -> usize {
        let pending = self
            .pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        pending.details.len() + pending.overflow.len()
    }

    pub(crate) fn offenders(&self) -> HashSet<String> {
        let pending = self
            .pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut offenders = pending
            .details
            .keys()
            .map(|(_, plugin, _, _)| plugin.clone())
            .chain(pending.overflow.keys().map(|(_, plugin, _)| plugin.clone()))
            .collect::<HashSet<_>>();
        drop(pending);
        if let Some(plugin) = self
            .active_plugin
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
        {
            offenders.insert(plugin);
        }
        offenders
    }
}

fn normalize_plugin_diagnostic_message(message: &str) -> String {
    cookie_agent_protocol::diagnostics::sanitize(message, PLUGIN_DIAGNOSTIC_MESSAGE_CHARS)
        .replace(['\n', '\t'], " ")
}

/// Plugin diagnostic accumulation state owned by [`Inner`].
#[derive(Debug, Default)]
pub(crate) struct PluginDiagnosticsState {
    pub(crate) accumulator: Arc<PluginDiagnosticAccumulator>,
    pub(crate) task: Mutex<Option<JoinHandle<()>>>,
}

pub(super) async fn run_plugin_diagnostic_aggregator(
    inner: Weak<Inner>,
    diagnostics: Arc<PluginDiagnosticAccumulator>,
) {
    loop {
        let notified = diagnostics.notify.notified();
        if diagnostics.is_empty() && !diagnostics.shutdown.load(Ordering::Acquire) {
            notified.await;
            continue;
        }
        if !diagnostics.shutdown.load(Ordering::Acquire) {
            tokio::time::sleep(PLUGIN_DIAGNOSTIC_BATCH_DELAY).await;
        }
        let batch = diagnostics.take();
        let Some(inner) = inner.upgrade() else {
            return;
        };
        #[cfg(test)]
        let append_block = inner
            .test_hooks
            .plugin_diagnostic_append_block
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let engine = Engine { inner };
        for ((session_id, plugin, kind, message), count) in batch {
            *diagnostics
                .active_plugin
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(plugin.clone());
            let append = async {
                #[cfg(test)]
                if let Some(block) = &append_block {
                    block.notified().await;
                }
                engine
                    .request(session_id, |reply| SessionCommand::Append {
                        run: None,
                        origin: cookie_agent_protocol::EventOrigin::new("engine:plugin-host")
                            .expect("static event origin is valid"),
                        event: Box::new(Event::PluginDiagnostic {
                            plugin: plugin.clone(),
                            kind,
                            message,
                            count,
                        }),
                        reply,
                    })
                    .await
            };
            if tokio::time::timeout(PLUGIN_DIAGNOSTIC_APPEND_TIMEOUT, append)
                .await
                .is_err()
            {
                engine.inner.plugins.note_offender_diagnostic(
                    &plugin,
                    "plugin diagnostic drain incomplete: append timed out".into(),
                );
            }
            *diagnostics
                .active_plugin
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
        }
        if diagnostics.shutdown.load(Ordering::Acquire) && diagnostics.is_empty() {
            return;
        }
    }
}
