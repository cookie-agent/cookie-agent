//! `<data_root>/unread.json`: the root sessions with a finished turn, an
//! approval, or a question the user has not seen
//! (`docs/site/specs/unread-sessions.md` section 4). It is written through
//! a flushed temporary file renamed over it, like `crons.json`, and kept
//! across restarts. Reading a session writes nothing to its log.

use std::{
    collections::BTreeMap,
    fs, io,
    path::{Path, PathBuf},
    sync::Mutex,
};

use cookie_agent_protocol::SessionId;
use jiff::Timestamp;
use serde::{Deserialize, Serialize};

pub(crate) const UNREAD_FILE: &str = "unread.json";

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct UnreadFile {
    sessions: Vec<UnreadEntry>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct UnreadEntry {
    session_id: SessionId,
    /// When it became unread.
    since: Timestamp,
}

/// The unread root sessions, and the file that keeps them.
#[derive(Debug)]
pub(crate) struct UnreadSessions {
    path: PathBuf,
    sessions: Mutex<BTreeMap<SessionId, Timestamp>>,
}

impl UnreadSessions {
    /// Reads `<data_root>/unread.json`; none yet is no session unread. A
    /// file that does not read is reported and replaced by the next change:
    /// losing the bubbles is better than a daemon that does not start.
    pub(crate) fn open(data_root: &Path) -> Self {
        let path = data_root.join(UNREAD_FILE);
        let sessions = match read(&path) {
            Ok(sessions) => sessions,
            Err(error) => {
                eprintln!(
                    "unread sessions: {} does not read ({error}); starting with none",
                    path.display()
                );
                BTreeMap::new()
            }
        };
        Self {
            path,
            sessions: Mutex::new(sessions),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<SessionId, Timestamp>> {
        self.sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(crate) fn contains(&self, id: SessionId) -> bool {
        self.lock().contains_key(&id)
    }

    /// Marks `ids` unread (since `now`) or read, and returns those whose
    /// state changed. The file is written once, when anything changed.
    pub(crate) fn set(&self, ids: &[SessionId], unread: bool, now: Timestamp) -> Vec<SessionId> {
        let mut sessions = self.lock();
        let changed = ids
            .iter()
            .copied()
            .filter(|id| {
                if unread {
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
    pub(crate) fn forget(&self, ids: impl IntoIterator<Item = SessionId>) {
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
        let file = UnreadFile {
            sessions: sessions
                .iter()
                .map(|(session_id, since)| UnreadEntry {
                    session_id: *session_id,
                    since: *since,
                })
                .collect(),
        };
        if let Err(error) = write(&self.path, &file) {
            eprintln!(
                "unread sessions: {} was not written: {error}",
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
    let file: UnreadFile = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
    Ok(file
        .sessions
        .into_iter()
        .map(|entry| (entry.session_id, entry.since))
        .collect())
}

fn write(path: &Path, file: &UnreadFile) -> io::Result<()> {
    let directory = path.parent().expect("the data root is a folder");
    fs::create_dir_all(directory)?;
    let bytes = serde_json::to_vec_pretty(file).map_err(io::Error::other)?;
    let temporary = directory.join(format!(".{UNREAD_FILE}.{}.tmp", uuid::Uuid::now_v7()));
    let result = (|| {
        let mut output = fs::File::create(&temporary)?;
        io::Write::write_all(&mut output, &bytes)?;
        output.sync_all()?;
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
        let unread = UnreadSessions::open(data.path());
        assert_eq!(unread.set(&[first, second], true, now), [first, second]);
        assert!(unread.set(&[first], true, now).is_empty());
        assert_eq!(unread.set(&[second], false, now), [second]);
        assert!(unread.set(&[second], false, now).is_empty());
        let reopened = UnreadSessions::open(data.path());
        assert!(reopened.contains(first) && !reopened.contains(second));
        reopened.forget([first]);
        assert!(!UnreadSessions::open(data.path()).contains(first));
    }

    #[test]
    fn a_file_that_does_not_read_starts_with_none() {
        let data = tempfile::tempdir().unwrap();
        fs::write(
            data.path().join(UNREAD_FILE),
            r#"{"sessions": [], "extra": 1}"#,
        )
        .unwrap();
        let unread = UnreadSessions::open(data.path());
        let id = SessionId::new_v7();
        let now = Timestamp::from_second(1_800_000_000).unwrap();
        assert_eq!(unread.set(&[id], true, now), [id]);
        assert!(UnreadSessions::open(data.path()).contains(id));
    }
}
