//! Restarting the daemon from a client
//! (`docs/site/specs/engine-configuration.md` section 11).
//!
//! The engine schedules the restart and decides when it is due; the program
//! that runs the engine as a daemon does the restart itself, through the
//! hook it installs with [`Engine::set_restart_hook`]. An engine without one
//! refuses with [`EngineError::RestartUnsupported`].
//!
//! An `idle` restart waits for the active runs and the running background
//! calls, which the shutdown would otherwise end without a report.
//! While it waits, runs keep starting as usual (client starts, queued input,
//! producer wakes, cron fires), and the restart waits for them too. Only once
//! it is due are new runs refused with [`EngineError::Restarting`]; a cron
//! fire due then stays unclaimed for the restarted daemon to catch up.

use std::{
    sync::{
        Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

use cookie_agent_protocol::RestartWhen;
use tokio::sync::{Notify, watch};

use super::{Engine, EngineError};

/// How often a waiting restart counts the active runs.
const IDLE_POLL: Duration = Duration::from_millis(200);

/// What the program running the engine does once a scheduled restart is
/// due: stop the daemon and relaunch it. Called at most once.
pub type RestartHook = Box<dyn FnOnce() + Send>;

/// A scheduled restart, as clients are told about it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RestartStatus {
    pub when: RestartWhen,
    /// The runs still active, which an `idle` restart waits for.
    pub active_runs: u32,
    /// The background calls still running, which an `idle` restart waits
    /// for too.
    pub background_calls: u32,
    /// The restart is due and the daemon is going down.
    pub due: bool,
}

/// What [`Engine::schedule_restart`] did.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RestartScheduled {
    /// `false` when a restart was already scheduled and only its `when`
    /// changed.
    pub scheduled: bool,
    pub active_runs: u32,
    pub background_calls: u32,
}

#[derive(Default)]
pub(crate) struct RestartState {
    supported: AtomicBool,
    hook: Mutex<Option<RestartHook>>,
    /// Set once a restart is scheduled; never cleared, since the daemon goes
    /// down.
    pending: AtomicBool,
    /// Set once the restart is due: run starts are refused. A start counts
    /// itself and the restart decides it is due under this lock, so a start
    /// is either counted before the decision or refused after it.
    due: Mutex<bool>,
    when: Mutex<Option<RestartWhen>>,
    /// Run starts in progress, which a waiting restart counts as active.
    starting: AtomicUsize,
    /// Work that may start a run without being one, such as a run's task
    /// between its end and starting its session's queued input; a waiting
    /// restart is not due while any is held, but does not report them.
    held: AtomicUsize,
    status: watch::Sender<Option<RestartStatus>>,
    wake: Notify,
}

/// Counts a run start in progress until dropped.
pub(super) struct RunStart<'a>(&'a AtomicUsize);

impl Drop for RunStart<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Keeps a waiting restart from being due until dropped; see
/// [`Engine::hold_restart`].
pub(super) struct RestartHold(Engine);

impl Drop for RestartHold {
    fn drop(&mut self) {
        self.0.inner.restart.held.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Engine {
    /// Lets clients restart the daemon this engine runs in: `hook` is
    /// called once a scheduled restart is due, and must stop the daemon
    /// (shutting this engine down) and relaunch it.
    pub fn set_restart_hook(&self, hook: RestartHook) {
        let restart = &self.inner.restart;
        *restart
            .hook
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(hook);
        restart.supported.store(true, Ordering::Release);
    }

    /// Whether a restart is scheduled.
    #[must_use]
    pub fn restart_pending(&self) -> bool {
        self.inner.restart.pending.load(Ordering::SeqCst)
    }

    /// Whether a scheduled restart is due: new runs are refused.
    pub(crate) fn restart_due(&self) -> bool {
        *self
            .inner
            .restart
            .due
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The scheduled restart, `None` until one is; it changes as the active
    /// runs do and once the restart is due.
    #[must_use]
    pub fn subscribe_restart(&self) -> watch::Receiver<Option<RestartStatus>> {
        self.inner.restart.status.subscribe()
    }

    /// Schedules a restart `when` asked. A second call changes the `when`
    /// of the scheduled one; `now` wins over `idle`.
    pub fn schedule_restart(&self, when: RestartWhen) -> Result<RestartScheduled, EngineError> {
        let restart = &self.inner.restart;
        if !restart.supported.load(Ordering::Acquire) {
            return Err(EngineError::RestartUnsupported);
        }
        let when = match when {
            RestartWhen::Now => RestartWhen::Now,
            RestartWhen::Idle | RestartWhen::Unknown => RestartWhen::Idle,
        };
        let scheduled = {
            let mut current = restart
                .when
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let scheduled = current.is_none();
            if current.is_none_or(|current| current == RestartWhen::Idle) {
                *current = Some(when);
            }
            scheduled
        };
        restart.pending.store(true, Ordering::SeqCst);
        let active_runs = self.restart_active_runs();
        let background_calls = self.restart_background_calls();
        if scheduled {
            let engine = self.clone();
            let task = async move { engine.wait_for_restart().await };
            match &self.inner.runtime {
                Some(runtime) => drop(runtime.spawn(task)),
                None => drop(tokio::spawn(task)),
            }
        } else {
            restart.wake.notify_one();
        }
        Ok(RestartScheduled {
            scheduled,
            active_runs,
            background_calls,
        })
    }

    /// Counts a run start, or refuses it once a scheduled restart is due.
    pub(super) fn begin_run_start(&self) -> Result<RunStart<'_>, EngineError> {
        let restart = &self.inner.restart;
        let due = restart
            .due
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if *due {
            return Err(EngineError::Restarting);
        }
        restart.starting.fetch_add(1, Ordering::SeqCst);
        Ok(RunStart(&restart.starting))
    }

    /// Keeps a waiting restart from being due while work that may start a
    /// run is under way, so the restart does not fall in the gap between a
    /// run and the one it leads to. A run started after the restart is due
    /// is refused all the same.
    pub(super) fn hold_restart(&self) -> RestartHold {
        self.inner.restart.held.fetch_add(1, Ordering::SeqCst);
        RestartHold(self.clone())
    }

    fn restart_active_runs(&self) -> u32 {
        let active = self
            .inner
            .sessions
            .active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .len();
        let starting = self.inner.restart.starting.load(Ordering::SeqCst);
        u32::try_from(active + starting).unwrap_or(u32::MAX)
    }

    /// The background calls of every session that have not committed their
    /// result yet.
    fn restart_background_calls(&self) -> u32 {
        u32::try_from(self.inner.background.pending_count() + self.acp_background_count())
            .unwrap_or(u32::MAX)
    }

    /// Publishes the restart's progress until it is due, then calls the
    /// hook.
    async fn wait_for_restart(self) {
        let restart = &self.inner.restart;
        loop {
            let when = restart
                .when
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .unwrap_or(RestartWhen::Idle);
            let (active_runs, background_calls, due) = {
                let mut gate = restart
                    .due
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                let active_runs = self.restart_active_runs();
                let background_calls = self.restart_background_calls();
                *gate = when == RestartWhen::Now
                    || (active_runs == 0
                        && background_calls == 0
                        && restart.held.load(Ordering::SeqCst) == 0);
                (active_runs, background_calls, *gate)
            };
            let status = RestartStatus {
                when,
                active_runs,
                background_calls,
                due,
            };
            restart.status.send_if_modified(|current| {
                let changed = *current != Some(status);
                *current = Some(status);
                changed
            });
            if due {
                break;
            }
            tokio::select! {
                () = tokio::time::sleep(IDLE_POLL) => {}
                () = restart.wake.notified() => {}
            }
        }
        let hook = restart
            .hook
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        if let Some(hook) = hook {
            hook();
        }
    }
}
