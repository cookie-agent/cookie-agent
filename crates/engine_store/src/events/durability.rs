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
    collections::HashSet,
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
    /// Blobs records written since the last sync name, synced before the
    /// log, with their folder, so a synced record never names a blob a
    /// crash could lose.
    blobs: Vec<PathBuf>,
    /// The folder of `blobs`.
    blob_directory: Option<PathBuf>,
    /// The blobs a sync of this log covered, which a later record naming
    /// them again does not sync again.
    synced_blobs: HashSet<PathBuf>,
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

    /// A record about to be written names the blobs at `blobs`, in the
    /// folder `directory`, each with whether this write just stored it: the
    /// sync that covers the record syncs them first. One stored earlier
    /// that a sync of this log covered already is not synced again.
    pub(super) fn names_blobs(
        &self,
        directory: &Path,
        blobs: impl IntoIterator<Item = (PathBuf, bool)>,
    ) {
        let mut state = self.state();
        let mut any = false;
        for (blob, stored) in blobs {
            if stored {
                state.synced_blobs.remove(&blob);
            } else if state.synced_blobs.contains(&blob) {
                continue;
            }
            if !state.blobs.contains(&blob) {
                state.blobs.push(blob);
            }
            any = true;
        }
        if any {
            state.blob_directory = Some(directory.to_owned());
        }
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
            let blobs = Blobs::take(&mut state);
            #[cfg(any(test, feature = "test-support"))]
            let hook = state.hook.take();
            drop(state);
            #[cfg(any(test, feature = "test-support"))]
            if let Some(hook) = hook {
                let _ = hook.reached.send(());
                let _ = hook.release.recv();
            }
            let result = self.sync(file.as_deref(), directory.as_deref(), &blobs);
            state = self.state();
            state.syncing = false;
            state.settled = state.settled.max(target);
            #[cfg(any(test, feature = "test-support"))]
            {
                state.syncs += usize::from(file.is_some());
            }
            match &result {
                Ok(()) => {
                    state.synced = state.synced.max(target);
                    blobs.synced(&mut state);
                }
                Err(_) => {
                    if state.directory.is_none() {
                        state.directory = directory;
                    }
                    blobs.restore(&mut state);
                }
            }
            self.synced.notify_all();
            result?;
        }
    }

    fn sync(
        &self,
        file: Option<&File>,
        directory: Option<&Path>,
        blobs: &Blobs,
    ) -> Result<(), EventLogError> {
        blobs.sync()?;
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
    /// covered yet, then lets go of the duplicate descriptor, which on
    /// Windows would keep the file and its session folder from being deleted.
    /// A failure has no caller to reach; those records are what a crash may
    /// drop anyway, and a commit point among them is still synced by the
    /// append waiting on it, so the duplicate stays for that sync.
    pub(super) fn close(&self, file: &File) {
        let mut state = self.state();
        if state.synced >= state.written {
            state.file = None;
            return;
        }
        let target = state.written;
        let directory = state.directory.take();
        let blobs = Blobs::take(&mut state);
        drop(state);
        let result = self.sync(Some(file), directory.as_deref(), &blobs);
        let mut state = self.state();
        #[cfg(any(test, feature = "test-support"))]
        {
            state.syncs += 1;
        }
        match result {
            Ok(()) => {
                state.synced = state.synced.max(target);
                state.settled = state.settled.max(target);
                blobs.synced(&mut state);
                if state.synced >= state.written {
                    state.file = None;
                }
            }
            Err(_) => {
                if state.directory.is_none() {
                    state.directory = directory;
                }
                blobs.restore(&mut state);
            }
        }
        self.synced.notify_all();
    }

    /// The file was cut and synced whole (a revert): everything written so
    /// far is as durable as the cut, once the blobs its records name are.
    pub(super) fn cut(&self) -> Result<(), EventLogError> {
        let blobs = Blobs::take(&mut self.state());
        let result = blobs.sync();
        let mut state = self.state();
        match result {
            Ok(()) => blobs.synced(&mut state),
            Err(error) => {
                blobs.restore(&mut state);
                return Err(error);
            }
        }
        state.synced = state.written;
        state.settled = state.written;
        self.synced.notify_all();
        Ok(())
    }

    #[cfg(any(test, feature = "test-support"))]
    pub(super) fn install_hook(&self, hook: SyncHook) {
        self.state().hook = Some(hook);
    }

    /// Test-only: the blobs the next sync syncs first.
    #[cfg(test)]
    pub(super) fn state_blobs_for_test(&self) -> Vec<PathBuf> {
        self.state().blobs.clone()
    }

    /// Test-only: whether a sync of this log covered the blob at `path`.
    #[cfg(test)]
    pub(super) fn synced_blob_for_test(&self, path: &Path) -> bool {
        self.state().synced_blobs.contains(path)
    }

    /// Test-only: how many times this log was synced.
    #[cfg(any(test, feature = "test-support"))]
    pub(super) fn syncs(&self) -> usize {
        self.state().syncs
    }
}

/// The blobs one sync covers, taken from the state as it starts.
struct Blobs {
    paths: Vec<PathBuf>,
    directory: Option<PathBuf>,
}

impl Blobs {
    fn take(state: &mut State) -> Self {
        Self {
            paths: std::mem::take(&mut state.blobs),
            directory: state.blob_directory.take(),
        }
    }

    /// Syncs each blob, then their folder.
    fn sync(&self) -> Result<(), EventLogError> {
        for path in &self.paths {
            super::blobs::sync(path).map_err(|source| EventLogError::Io {
                path: path.clone(),
                source,
            })?;
        }
        match &self.directory {
            Some(directory) => fsync_directory(directory),
            None => Ok(()),
        }
    }

    fn synced(self, state: &mut State) {
        state.synced_blobs.extend(self.paths);
    }

    /// A failed sync leaves them for the next one.
    fn restore(self, state: &mut State) {
        let mut paths = self.paths;
        paths.append(&mut state.blobs);
        state.blobs = paths;
        if state.blob_directory.is_none() {
            state.blob_directory = self.directory;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;

    use super::Durability;

    /// Closing the writer lets go of the duplicate descriptor once nothing is
    /// left to sync, so a deletion right after a run can remove the session's
    /// folder on Windows, where an open descriptor keeps it.
    #[test]
    fn closing_lets_go_of_the_duplicate_descriptor() {
        let directory = tempfile::tempdir().expect("a directory");
        let path = directory.path().join("events.jsonl");
        let mut file = std::fs::File::create(&path).expect("a log");
        let durability = Durability::new(&path);
        durability.attach(&file, false).expect("a duplicate");
        file.write_all(b"{}\n").expect("a record");
        let (commit, _) = durability.written();
        durability.sync_through(commit).expect("a sync");
        assert!(
            durability.state().file.is_some(),
            "kept while the writer is open"
        );
        durability.close(&file);
        assert!(durability.state().file.is_none(), "nothing left to sync");

        // A record no sync covered yet is synced on close, then let go too.
        durability.attach(&file, false).expect("a duplicate");
        file.write_all(b"{}\n").expect("a record");
        let _ = durability.written();
        durability.close(&file);
        assert!(durability.state().file.is_none(), "synced on close");
        assert!(durability.is_settled(durability.latest()));
    }
}
