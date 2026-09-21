//! Test-only observation hooks installed on the engine's shared `Inner`.
//!
//! The whole module is `#![cfg(test)]`; it exists so the production `Inner`
//! carries a single `test_hooks` field instead of two dozen `#[cfg(test)]`
//! ones.
#![cfg(test)]

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU64},
};

use std::sync::mpsc as std_mpsc;

use cookie_agent_protocol::RunId;
use tokio::sync::{mpsc, oneshot};

pub(crate) struct PromptSnapshotHook {
    pub(crate) reached: Mutex<Option<oneshot::Sender<()>>>,
    pub(crate) release: Arc<tokio::sync::Notify>,
}

pub(crate) struct PagingRaceHook {
    pub(crate) reached: Mutex<Option<oneshot::Sender<()>>>,
    pub(crate) release: Arc<tokio::sync::Notify>,
}

pub(crate) struct ToolProgressAppendBlock {
    pub(crate) reached: Arc<tokio::sync::Notify>,
    pub(crate) release: tokio::sync::Notify,
}

pub(crate) struct ReadOnlyReopenHook {
    pub(crate) reached: Mutex<Option<oneshot::Sender<()>>>,
    pub(crate) release: Mutex<std_mpsc::Receiver<()>>,
}

pub(crate) struct ApprovalEvaluationHook {
    pub(crate) reached: Mutex<Option<oneshot::Sender<()>>>,
    pub(crate) release: tokio::sync::Notify,
}

#[derive(Clone, Copy, Default, Eq, PartialEq)]
pub(crate) enum ModelRetrySleepMode {
    #[default]
    Real,
    Immediate,
    Blocked,
}

#[derive(Default)]
pub(crate) struct ModelRetrySleepHook {
    pub(crate) mode: Mutex<ModelRetrySleepMode>,
    pub(crate) delays: Mutex<Vec<std::time::Duration>>,
    pub(crate) reached: tokio::sync::Notify,
}

impl ModelRetrySleepHook {
    pub(crate) fn set_mode(&self, mode: ModelRetrySleepMode) {
        *self
            .mode
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = mode;
    }

    pub(crate) fn delays(&self) -> Vec<std::time::Duration> {
        self.delays
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    pub(crate) async fn wait_until_reached(&self, count: usize) {
        loop {
            let reached = self.reached.notified();
            if self.delays().len() >= count {
                return;
            }
            reached.await;
        }
    }

    pub(crate) async fn sleep(&self, delay: std::time::Duration) -> bool {
        let mode = *self
            .mode
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if mode == ModelRetrySleepMode::Real {
            return false;
        }
        self.delays
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(delay);
        self.reached.notify_waiters();
        match mode {
            ModelRetrySleepMode::Real => false,
            ModelRetrySleepMode::Immediate => true,
            ModelRetrySleepMode::Blocked => std::future::pending().await,
        }
    }
}

pub(crate) struct AdmissionConfirmationHook {
    pub(crate) reached: mpsc::UnboundedSender<()>,
    pub(crate) release: Arc<tokio::sync::Barrier>,
}

pub(crate) struct ResumeAdmissionHook {
    pub(crate) reached: Mutex<Option<oneshot::Sender<()>>>,
    pub(crate) release: Arc<tokio::sync::Notify>,
}

pub(crate) struct AdmissionBlockingHook {
    pub(crate) reached: std_mpsc::Sender<()>,
    pub(crate) release: std_mpsc::Receiver<()>,
}

#[derive(Clone)]
pub(crate) struct AbandonedSweepHook {
    pub(crate) reached: mpsc::UnboundedSender<()>,
    pub(crate) captured: mpsc::UnboundedSender<Vec<RunId>>,
    pub(crate) release: Arc<tokio::sync::Notify>,
}

#[derive(Default)]
pub(crate) struct TestHooks {
    pub(crate) prompt_snapshot_hook: Mutex<Option<Arc<PromptSnapshotHook>>>,
    pub(crate) prompt_before_claim_hook: Mutex<Option<Arc<PromptSnapshotHook>>>,
    pub(crate) janitor_before_barrier_hook: Mutex<Option<Arc<PagingRaceHook>>>,
    pub(crate) compaction_execution_hook: Mutex<Option<Arc<PagingRaceHook>>>,
    pub(crate) read_only_reopen_hook: Mutex<Option<ReadOnlyReopenHook>>,
    pub(crate) approval_evaluation_hook: Mutex<Option<Arc<ApprovalEvaluationHook>>>,
    pub(crate) model_retry_sleep_hook: ModelRetrySleepHook,
    pub(crate) pending_approval_ready: tokio::sync::Notify,
    pub(crate) admission_confirmation_hook: Mutex<Option<Arc<AdmissionConfirmationHook>>>,
    pub(crate) resume_admission_hook: Mutex<Option<Arc<ResumeAdmissionHook>>>,
    pub(crate) resume_attachment_hook: Mutex<Option<Arc<ResumeAdmissionHook>>>,
    pub(crate) skill_fork_reservation_hook: Mutex<Option<Arc<PagingRaceHook>>>,
    pub(crate) producer_wake_hook: Mutex<Option<Arc<PagingRaceHook>>>,
    pub(crate) delegation_reservation_hook: Mutex<Option<Arc<PagingRaceHook>>>,
    pub(crate) resume_rollback_hook: Mutex<Option<Arc<ResumeAdmissionHook>>>,
    pub(crate) admission_blocking_hook: Mutex<Option<AdmissionBlockingHook>>,
    pub(crate) abandoned_sweep_hook: Mutex<Option<AbandonedSweepHook>>,
    pub(crate) plugin_diagnostic_append_block: Mutex<Option<Arc<tokio::sync::Notify>>>,
    pub(crate) tool_progress_append_block: Mutex<Option<Arc<ToolProgressAppendBlock>>>,
    pub(crate) publication_failure: AtomicBool,
    pub(crate) delegate_start_failures: AtomicU64,
    pub(crate) delegate_start_failure_observed: tokio::sync::Notify,
    pub(crate) delegate_terminal_append_failures: AtomicU64,
    pub(crate) run_setup_append_failures: AtomicU64,
    pub(crate) resume_monitor_failures: AtomicU64,
    pub(crate) adoption_reconcile_failures: AtomicU64,
}
