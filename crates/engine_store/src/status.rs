//! The daemon's status (`docs/site/specs/daemon-startup.md`): its phase and
//! the tasks it is busy with, which clients see in the handshake and in
//! `daemon.status`.
//!
//! Anything the daemon does that a user would wonder about starts a
//! [`StatusTask`] on the [`StatusBoard`] and drops it when done. The board
//! never branches on what a task is: producers name their tasks, and
//! clients show every task the same way.

use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use cookie_agent_protocol::{DaemonPhase, DaemonStatus, DaemonTask, JsonRpcError, TaskProgress};
use tokio::sync::watch;

/// The daemon's status, shared by everything that reports to it.
#[derive(Clone)]
pub struct StatusBoard {
    inner: Arc<Inner>,
}

struct Inner {
    status: watch::Sender<DaemonStatus>,
    next_id: AtomicU64,
    /// Every status the board held, for a test to read back.
    #[cfg(any(test, feature = "test-support"))]
    history: Option<std::sync::Mutex<Vec<DaemonStatus>>>,
}

impl std::fmt::Debug for StatusBoard {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("StatusBoard")
            .field("status", &*self.inner.status.borrow())
            .finish()
    }
}

impl Default for StatusBoard {
    /// A board in phase `ready`, for an engine nothing watches start.
    fn default() -> Self {
        Self::new(DaemonPhase::Ready)
    }
}

impl StatusBoard {
    #[must_use]
    pub fn new(phase: DaemonPhase) -> Self {
        Self {
            inner: Arc::new(Inner {
                status: watch::Sender::new(DaemonStatus {
                    phase,
                    ..DaemonStatus::default()
                }),
                next_id: AtomicU64::new(1),
                #[cfg(any(test, feature = "test-support"))]
                history: None,
            }),
        }
    }

    /// A board that keeps every status it holds, for [`Self::history`].
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn recording(phase: DaemonPhase) -> Self {
        let status = DaemonStatus {
            phase,
            ..DaemonStatus::default()
        };
        Self {
            inner: Arc::new(Inner {
                history: Some(std::sync::Mutex::new(vec![status.clone()])),
                status: watch::Sender::new(status),
                next_id: AtomicU64::new(1),
            }),
        }
    }

    /// Every status a [`Self::recording`] board held, oldest first.
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn history(&self) -> Vec<DaemonStatus> {
        self.inner
            .history
            .as_ref()
            .map(|history| {
                history
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone()
            })
            .unwrap_or_default()
    }

    /// Changes the status as `change` does, if it says it did.
    fn modify(&self, change: impl FnOnce(&mut DaemonStatus) -> bool) {
        let changed = self.inner.status.send_if_modified(change);
        #[cfg(any(test, feature = "test-support"))]
        if changed && let Some(history) = &self.inner.history {
            history
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(self.status());
        }
        #[cfg(not(any(test, feature = "test-support")))]
        let _ = changed;
    }

    /// The status now.
    #[must_use]
    pub fn status(&self) -> DaemonStatus {
        self.inner.status.borrow().clone()
    }

    /// The phase now.
    #[must_use]
    pub fn phase(&self) -> DaemonPhase {
        self.inner.status.borrow().phase
    }

    /// Every change from now on.
    #[must_use]
    pub fn subscribe(&self) -> watch::Receiver<DaemonStatus> {
        self.inner.status.subscribe()
    }

    /// The daemon is ready.
    pub fn ready(&self) {
        self.set_phase(DaemonPhase::Ready, false);
    }

    /// The daemon is going down; `restart` when it relaunches. A failed
    /// daemon stays failed.
    pub fn stopping(&self, restart: bool) {
        self.modify(|status| {
            if status.phase == DaemonPhase::Failed
                || (status.phase == DaemonPhase::Stopping && status.restart == restart)
            {
                return false;
            }
            status.phase = DaemonPhase::Stopping;
            status.restart = restart;
            true
        });
    }

    /// Startup failed with `error`, a registry error.
    pub fn fail(&self, error: JsonRpcError) {
        self.modify(|status| {
            status.phase = DaemonPhase::Failed;
            status.restart = false;
            status.error = Some(error);
            true
        });
    }

    fn set_phase(&self, phase: DaemonPhase, restart: bool) {
        self.modify(|status| {
            if status.phase == phase && status.restart == restart {
                return false;
            }
            status.phase = phase;
            status.restart = restart;
            true
        });
    }

    /// Starts a task labelled `label`, which ends when the returned handle
    /// is dropped. A `blocking` task is one the daemon is not ready until
    /// it ends.
    #[must_use = "the task ends when its handle is dropped"]
    pub fn start(&self, label: impl Into<String>, blocking: bool) -> StatusTask {
        let id = self
            .inner
            .next_id
            .fetch_add(1, Ordering::Relaxed)
            .to_string();
        let task = DaemonTask {
            id: id.clone(),
            label: label.into(),
            detail: None,
            progress: None,
            blocking,
            started_at: jiff::Timestamp::now(),
        };
        self.modify(|status| {
            status.tasks.push(task);
            true
        });
        StatusTask {
            board: self.clone(),
            id,
        }
    }

    fn update(&self, id: &str, change: impl FnOnce(&mut DaemonTask) -> bool) {
        self.modify(|status| {
            status
                .tasks
                .iter_mut()
                .find(|task| task.id == id)
                .is_some_and(change)
        });
    }
}

/// A task on the [`StatusBoard`], which ends when dropped.
#[derive(Debug)]
pub struct StatusTask {
    board: StatusBoard,
    id: String,
}

impl StatusTask {
    /// Sets how much there is to do, in `unit`s ("trees"), and starts the
    /// count at zero.
    pub fn set_total(&self, total: u64, unit: impl Into<String>) {
        let unit = unit.into();
        self.board.update(&self.id, |task| {
            task.progress = Some(TaskProgress {
                done: 0,
                total: Some(total),
                unit: Some(unit),
            });
            true
        });
    }

    /// Counts `done` more done.
    pub fn advance(&self, done: u64) {
        self.board.update(&self.id, |task| {
            let progress = task.progress.get_or_insert(TaskProgress {
                done: 0,
                total: None,
                unit: None,
            });
            progress.done = progress.done.saturating_add(done);
            true
        });
    }

    /// Says what the task is on now, or nothing.
    pub fn set_detail(&self, detail: Option<String>) {
        self.board.update(&self.id, |task| {
            if task.detail == detail {
                return false;
            }
            task.detail = detail;
            true
        });
    }

    /// Renames the task.
    pub fn set_label(&self, label: impl Into<String>) {
        let label = label.into();
        self.board.update(&self.id, |task| {
            if task.label == label {
                return false;
            }
            task.label = label;
            true
        });
    }
}

impl Drop for StatusTask {
    fn drop(&mut self) {
        self.board.modify(|status| {
            let before = status.tasks.len();
            status.tasks.retain(|task| task.id != self.id);
            status.tasks.len() != before
        });
    }
}

#[cfg(test)]
mod tests {
    use cookie_agent_protocol::{DaemonPhase, errors};

    use super::StatusBoard;

    #[test]
    fn a_task_reports_progress_and_leaves_the_list_when_dropped() {
        let board = StatusBoard::new(DaemonPhase::Starting);
        let mut changes = board.subscribe();
        let task = board.start("Moving sessions", true);
        task.set_total(3, "trees");
        task.advance(2);
        task.set_detail(Some("2026-10".into()));
        let status = changes.borrow_and_update().clone();
        assert_eq!(status.phase, DaemonPhase::Starting);
        let [listed] = status.tasks.as_slice() else {
            panic!("one task: {status:?}");
        };
        assert_eq!(listed.label, "Moving sessions");
        assert!(listed.blocking);
        assert_eq!(listed.detail.as_deref(), Some("2026-10"));
        let progress = listed.progress.as_ref().expect("progress");
        assert_eq!((progress.done, progress.total), (2, Some(3)));
        assert_eq!(progress.unit.as_deref(), Some("trees"));
        drop(task);
        assert!(changes.has_changed().expect("board alive"));
        assert!(board.status().tasks.is_empty());
    }

    #[test]
    fn tasks_keep_their_order_and_ids_stay_distinct() {
        let board = StatusBoard::default();
        let first = board.start("first", false);
        let second = board.start("second", false);
        drop(first);
        let third = board.start("third", false);
        let labels = board
            .status()
            .tasks
            .iter()
            .map(|task| task.label.clone())
            .collect::<Vec<_>>();
        assert_eq!(labels, ["second", "third"]);
        let status = board.status();
        assert_ne!(status.tasks[0].id, status.tasks[1].id);
        drop((second, third));
    }

    #[test]
    fn a_failed_daemon_stays_failed_when_it_stops() {
        let board = StatusBoard::new(DaemonPhase::Starting);
        board.fail(errors::DAEMON_START_FAILED.error().detail("disk").build());
        board.stopping(false);
        let status = board.status();
        assert_eq!(status.phase, DaemonPhase::Failed);
        assert!(status.error.is_some());
    }

    #[test]
    fn stopping_for_a_restart_is_marked() {
        let board = StatusBoard::default();
        board.stopping(true);
        let status = board.status();
        assert_eq!(status.phase, DaemonPhase::Stopping);
        assert!(status.restart);
    }
}
