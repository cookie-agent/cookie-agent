//! Restarting the daemon from a client
//! (`docs/site/specs/engine-configuration.md` section 11).
//!
//! The engine schedules the restart and decides when it is due; the program
//! that runs the engine as a daemon does the restart itself, through the
//! hook it installs with [`Engine::set_restart_hook`]. An engine without one
//! refuses with [`EngineError::RestartUnsupported`].
//!
//! While an `idle` restart waits, new top-level runs are refused with
//! [`EngineError::Restarting`]: client starts, queued input, producer wakes,
//! and cron fires (recorded as skipped). A run's delegated children still
//! start, since they are part of a turn the restart waits for.

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
}

#[derive(Default)]
pub(crate) struct RestartState {
    supported: AtomicBool,
    hook: Mutex<Option<RestartHook>>,
    /// Set once a restart is scheduled; never cleared, since the daemon goes
    /// down.
    pending: AtomicBool,
    when: Mutex<Option<RestartWhen>>,
    /// Run starts in progress, which a waiting restart counts as active.
    starting: AtomicUsize,
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
        })
    }

    /// Counts a run start, or refuses a top-level one while a restart is
    /// scheduled. `top_level` is `false` for a delegated child, which is
    /// part of a turn already running.
    pub(super) fn begin_run_start(&self, top_level: bool) -> Result<RunStart<'_>, EngineError> {
        let restart = &self.inner.restart;
        // Counted before the check: a restart scheduled after it sees this
        // start among the active runs.
        restart.starting.fetch_add(1, Ordering::SeqCst);
        let start = RunStart(&restart.starting);
        if top_level && restart.pending.load(Ordering::SeqCst) {
            return Err(EngineError::Restarting);
        }
        Ok(start)
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
            let active_runs = self.restart_active_runs();
            let due = when == RestartWhen::Now || active_runs == 0;
            let status = RestartStatus {
                when,
                active_runs,
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
