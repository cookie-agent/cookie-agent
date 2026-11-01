//! The root sessions the user marked, each kind in a file of its own
//! under `<data_root>`: `unread.json`, those with a finished turn, an
//! approval, or a question the user has not seen
//! (`docs/site/specs/unread-sessions.md` section 4), and `pinned.json`,
//! those the user pinned (`docs/site/specs/session-pins-and-filters.md`).
//! Each is written through a flushed temporary file renamed over it, like
//! `crons.json`, and kept across restarts. Marking a session writes
//! nothing to its log.

use std::{
    collections::BTreeMap,
    fs, io,
    path::{Path, PathBuf},
    sync::Mutex,
};

use cookie_agent_protocol::SessionId;
use jiff::Timestamp;
use serde::{Deserialize, Serialize};

pub const UNREAD_FILE: &str = "unread.json";
pub const PINNED_FILE: &str = "pinned.json";

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct MarksFile {
    sessions: Vec<MarkEntry>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct MarkEntry {
    session_id: SessionId,
    /// When it was marked.
    since: Timestamp,
}

/// One kind of marked root sessions, and the file that keeps them.
#[derive(Debug)]
pub struct MarkedSessions {
    path: PathBuf,
    /// What the marks are, for messages: `unread sessions`.
    what: &'static str,
    sessions: Mutex<BTreeMap<SessionId, Timestamp>>,
}

impl MarkedSessions {
    /// Reads `<data_root>/<file>`; none yet is no session marked. A file
    /// that does not read is reported and replaced by the next change:
    /// losing the marks is better than a daemon that does not start.
    pub fn open(data_root: &Path, file: &'static str, what: &'static str) -> Self {
        let path = data_root.join(file);
        let sessions = match read(&path) {
            Ok(sessions) => sessions,
            Err(error) => {
                eprintln!(
                    "{what}: {} does not read ({error}); starting with none",
                    path.display()
                );
                BTreeMap::new()
            }
        };
        Self {
            path,
            what,
            sessions: Mutex::new(sessions),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<SessionId, Timestamp>> {
        self.sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub fn contains(&self, id: SessionId) -> bool {
        self.lock().contains_key(&id)
    }

    /// Every marked root session.
    pub fn ids(&self) -> Vec<SessionId> {
        self.lock().keys().copied().collect()
    }

    /// Marks `ids` (since `now`) or clears them, and returns those whose
    /// state changed. The file is written once, when anything changed.
    pub fn set(&self, ids: &[SessionId], marked: bool, now: Timestamp) -> Vec<SessionId> {
        let mut sessions = self.lock();
        let changed = ids
            .iter()
            .copied()
            .filter(|id| {
                if marked {
                    sessions.insert(*id, now).is_none()
                } else {
                    sessions.remove(id).is_some()
                }
            })
            .collect::<Vec<_>>();
        if !changed.is_empty() {
            self.save(&sessions);
        }
        changed
    }

    /// Forgets deleted sessions.
    pub fn forget(&self, ids: impl IntoIterator<Item = SessionId>) {
        let mut sessions = self.lock();
        let before = sessions.len();
        for id in ids {
            sessions.remove(&id);
        }
        if sessions.len() != before {
            self.save(&sessions);
        }
    }

    /// Writes the file; a failure is reported and the state kept in memory.
    fn save(&self, sessions: &BTreeMap<SessionId, Timestamp>) {
        let file = MarksFile {
            sessions: sessions
                .iter()
                .map(|(session_id, since)| MarkEntry {
                    session_id: *session_id,
                    since: *since,
                })
                .collect(),
        };
        if let Err(error) = write(&self.path, &file) {
            eprintln!(
                "{}: {} was not written: {error}",
                self.what,
                self.path.display()
            );
        }
    }
}

fn read(path: &Path) -> io::Result<BTreeMap<SessionId, Timestamp>> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(error) => return Err(error),
    };
    let file: MarksFile = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
    Ok(file
        .sessions
        .into_iter()
        .map(|entry| (entry.session_id, entry.since))
        .collect())
}

/// Replaces the file atomically but without a sync: a crash may lose the
/// latest change, which marks a session read again or unread again, or
/// pins or unpins it again (store locks and durability, section 4.5).
fn write(path: &Path, file: &MarksFile) -> io::Result<()> {
    let directory = path.parent().expect("the data root is a folder");
    fs::create_dir_all(directory)?;
    let bytes = serde_json::to_vec_pretty(file).map_err(io::Error::other)?;
    let name = path
        .file_name()
        .map_or_else(String::new, |name| name.to_string_lossy().into_owned());
    let temporary = directory.join(format!(".{name}.{}.tmp", uuid::Uuid::now_v7()));
    let result = (|| {
        let mut output = fs::File::create(&temporary)?;
        io::Write::write_all(&mut output, &bytes)?;
        drop(output);
        fs::rename(&temporary, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unread_sessions_survive_a_reopen_and_change_only_once() {
        let data = tempfile::tempdir().unwrap();
        let (first, second) = (SessionId::new_v7(), SessionId::new_v7());
        let now = Timestamp::from_second(1_800_000_000).unwrap();
        let unread = MarkedSessions::open(data.path(), UNREAD_FILE, "unread sessions");
        assert_eq!(unread.set(&[first, second], true, now), [first, second]);
        assert!(unread.set(&[first], true, now).is_empty());
        assert_eq!(unread.set(&[second], false, now), [second]);
        assert!(unread.set(&[second], false, now).is_empty());
        let reopened = MarkedSessions::open(data.path(), UNREAD_FILE, "unread sessions");
        assert!(reopened.contains(first) && !reopened.contains(second));
        reopened.forget([first]);
        assert!(!MarkedSessions::open(data.path(), UNREAD_FILE, "unread sessions").contains(first));
    }

    #[test]
    fn each_kind_of_mark_keeps_a_file_of_its_own() {
        let data = tempfile::tempdir().unwrap();
        let id = SessionId::new_v7();
        let now = Timestamp::from_second(1_800_000_000).unwrap();
        let pinned = MarkedSessions::open(data.path(), PINNED_FILE, "pinned sessions");
        assert_eq!(pinned.set(&[id], true, now), [id]);
        assert!(data.path().join(PINNED_FILE).exists());
        assert!(!data.path().join(UNREAD_FILE).exists());
        assert!(MarkedSessions::open(data.path(), PINNED_FILE, "pinned sessions").contains(id));
        assert!(!MarkedSessions::open(data.path(), UNREAD_FILE, "unread sessions").contains(id));
    }

    #[test]
    fn a_file_that_does_not_read_starts_with_none() {
        let data = tempfile::tempdir().unwrap();
        fs::write(
            data.path().join(UNREAD_FILE),
            r#"{"sessions": [], "extra": 1}"#,
        )
        .unwrap();
        let unread = MarkedSessions::open(data.path(), UNREAD_FILE, "unread sessions");
        let id = SessionId::new_v7();
        let now = Timestamp::from_second(1_800_000_000).unwrap();
        assert_eq!(unread.set(&[id], true, now), [id]);
        assert!(MarkedSessions::open(data.path(), UNREAD_FILE, "unread sessions").contains(id));
    }
}
