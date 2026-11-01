//! When a session log's records reach the disk.
//!
//! A record is written to the file as it is appended, so readers in this
//! process see it at once, but only a commit point is synced before it takes
//! effect (`docs/site/specs/store-locks-and-durability.md`, section 3). The
//! sync runs after the store's locks are released, on a duplicate of the
//! writer's descriptor: `fdatasync` covers everything written to the file so
//! far, whoever wrote it, so one sync serves every commit point written
//! before it started, and a commit point written while a sync is in flight
//! waits for it to end and then runs the next one.

use std::{
    fs::File,
    io,
    path::{Path, PathBuf},
    sync::{Arc, Condvar, Mutex, MutexGuard},
};

use super::{EventLogError, fsync_directory};

/// One record written to a log, numbered from the log's first write in this
/// process. A commit point's append returns it, and
/// [`super::EventLog::sync_through`] waits until a sync covers it. Reverts
/// do not reuse these numbers, unlike sequences.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct Commit(u64);

/// The sync state of one log: what was written, what a finished sync
/// covers, and the descriptor syncs run on.
#[derive(Debug)]
pub(super) struct Durability {
    path: PathBuf,
    state: Mutex<State>,
    /// Signalled whenever a sync ends.
    synced: Condvar,
}

#[derive(Debug, Default)]
struct State {
    /// Records written so far.
    written: u64,
    /// `written` when the last successful sync started.
    synced: u64,
    /// `written` when the last sync, successful or not, started: what no
    /// longer waits on a sync.
    settled: u64,
    /// A sync is in flight; a commit point written meanwhile waits for it.
    syncing: bool,
    /// A duplicate of the open writer's descriptor.
    file: Option<Arc<File>>,
    /// The log's directory, synced with the next sync after the writer
    /// created the file.
    directory: Option<PathBuf>,
    #[cfg(any(test, feature = "test-support"))]
    hook: Option<SyncHook>,
    #[cfg(any(test, feature = "test-support"))]
    syncs: usize,
}

#[cfg(any(test, feature = "test-support"))]
#[derive(Debug)]
pub(super) struct SyncHook {
    pub(super) reached: std::sync::mpsc::Sender<()>,
    pub(super) release: std::sync::mpsc::Receiver<()>,
}

impl Durability {
    pub(super) fn new(path: &Path) -> Self {
        Self {
            path: path.to_owned(),
            state: Mutex::default(),
            synced: Condvar::new(),
        }
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// A writer opened `file`; later syncs run on a duplicate of its
    /// descriptor, which stays valid if the writer closes meanwhile. When
    /// the writer `created` the file, the next sync also syncs its directory.
    pub(super) fn attach(&self, file: &File, created: bool) -> io::Result<()> {
        let duplicate = Arc::new(file.try_clone()?);
        let mut state = self.state();
        state.file = Some(duplicate);
        if created {
            state.directory = self.path.parent().map(Path::to_owned);
        }
        Ok(())
    }

    /// Counts one record written, returning its number and whether it has to
    /// sync whatever it is: the file is new and its directory entry is not
    /// durable yet.
    pub(super) fn written(&self) -> (Commit, bool) {
        let mut state = self.state();
        state.written += 1;
        (Commit(state.written), state.directory.is_some())
    }

    /// The last record written.
    pub(super) fn latest(&self) -> Commit {
        Commit(self.state().written)
    }

    /// Whether `commit` waits on no sync any more: one that covers it ended,
    /// successfully or not.
    pub(super) fn is_settled(&self, commit: Commit) -> bool {
        self.state().settled >= commit.0
    }

    /// Returns once a sync that covers `commit` succeeded, running it when no
    /// other sync is in flight. A sync in flight that started before
    /// `commit` was written is waited out and followed by the next one,
    /// which covers every record written by then. A failed sync's error goes
    /// to the caller that ran it; the others waiting try again.
    pub(super) fn sync_through(&self, commit: Commit) -> Result<(), EventLogError> {
        let mut state = self.state();
        loop {
            if state.synced >= commit.0 {
                return Ok(());
            }
            if state.syncing {
                state = self
                    .synced
                    .wait(state)
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                continue;
            }
            state.syncing = true;
            let target = state.written;
            let file = state.file.clone();
            let directory = state.directory.take();
            #[cfg(any(test, feature = "test-support"))]
            let hook = state.hook.take();
            drop(state);
            #[cfg(any(test, feature = "test-support"))]
            if let Some(hook) = hook {
                let _ = hook.reached.send(());
                let _ = hook.release.recv();
            }
            let result = self.sync(file.as_deref(), directory.as_deref());
            state = self.state();
            state.syncing = false;
            state.settled = state.settled.max(target);
            #[cfg(any(test, feature = "test-support"))]
            {
                state.syncs += usize::from(file.is_some());
            }
            match &result {
                Ok(()) => state.synced = state.synced.max(target),
                Err(_) => {
                    if state.directory.is_none() {
                        state.directory = directory;
                    }
                }
            }
            self.synced.notify_all();
            result?;
        }
    }

    fn sync(&self, file: Option<&File>, directory: Option<&Path>) -> Result<(), EventLogError> {
        if let Some(file) = file {
            #[cfg(any(test, feature = "test-support"))]
            super::EVENT_LOG_SYNCS.with(|syncs| syncs.set(syncs.get() + 1));
            crate::timing::time(
                crate::timing::Meter::LogSync,
                || self.path.display().to_string(),
                || file.sync_data(),
            )
            .map_err(|source| EventLogError::Io {
                path: self.path.clone(),
                source,
            })?;
        }
        if let Some(directory) = directory {
            fsync_directory(directory)?;
        }
        Ok(())
    }

    /// The writer closes: syncs, on its own descriptor, whatever no sync has
    /// covered yet. A failure has no caller to reach; those records are what
    /// a crash may drop anyway, and a commit point among them is still
    /// synced by the append waiting on it.
    pub(super) fn close(&self, file: &File) {
        let mut state = self.state();
        if state.synced >= state.written {
            return;
        }
        let target = state.written;
        let directory = state.directory.take();
        drop(state);
        let result = self.sync(Some(file), directory.as_deref());
        let mut state = self.state();
        #[cfg(any(test, feature = "test-support"))]
        {
            state.syncs += 1;
        }
        match result {
            Ok(()) => {
                state.synced = state.synced.max(target);
                state.settled = state.settled.max(target);
            }
            Err(_) => {
                if state.directory.is_none() {
                    state.directory = directory;
                }
            }
        }
        self.synced.notify_all();
    }

    /// The file was cut and synced whole (a revert): everything written so
    /// far is as durable as the cut.
    pub(super) fn cut(&self) {
        let mut state = self.state();
        state.synced = state.written;
        state.settled = state.written;
        self.synced.notify_all();
    }

    #[cfg(any(test, feature = "test-support"))]
    pub(super) fn install_hook(&self, hook: SyncHook) {
        self.state().hook = Some(hook);
    }

    /// Test-only: how many times this log was synced.
    #[cfg(any(test, feature = "test-support"))]
    pub(super) fn syncs(&self) -> usize {
        self.state().syncs
    }
}
