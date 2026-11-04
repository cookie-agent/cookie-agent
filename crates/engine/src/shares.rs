//! The daemon's share links (`docs/site/specs/share-links.md` sections 3
//! and 4, `docs/site/specs/conversation-shares.md` section 3): one record
//! per link in `<data_root>/shares.json`, naming the asset it shows or the
//! session it opens. No bytes are copied: every request reads the asset
//! store or the session.
//!
//! Writes are atomic and serialized by one lock. View counts are kept in
//! memory and written by [`ShareStore::flush`], which the engine's janitor
//! calls every minute and shutdown calls once.

use std::{
    collections::HashMap,
    fs, io,
    path::{Path, PathBuf},
    sync::Mutex,
};

use cookie_agent_protocol::{AssetId, AssetStore, SessionId, ShareCreated, ShareId};
use serde::{Deserialize, Serialize};

/// The store's file under the data root.
pub(crate) const SHARES_FILE: &str = "shares.json";
const FILE_VERSION: u32 = 1;

/// A share request was refused or failed.
#[derive(Debug, thiserror::Error)]
pub enum ShareError {
    #[error("there is no share link {0}")]
    NotFound(ShareId),
    #[error("share links are turned off ([share] enabled = false)")]
    Disabled,
    #[error("the expiry is already past")]
    PastExpiry,
    #[error("session {0} is a delegated session, which cannot be shared by itself")]
    NotRoot(SessionId),
    #[error("a share link's label is one line of at most 30 bytes")]
    InvalidLabel,
    #[error("share link {0} is not of this kind")]
    OtherKind(ShareId),
    #[error("the share store failed: {0}")]
    Store(String),
}

/// One link as the store keeps it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct ShareRecord {
    pub(crate) id: ShareId,
    #[serde(flatten)]
    pub(crate) target: ShareTarget,
    pub(crate) created: ShareCreated,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) expires_at: Option<jiff::Timestamp>,
    #[serde(default)]
    pub(crate) views: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) last_viewed: Option<jiff::Timestamp>,
}

/// What a link opens. The fields are the record's own, so a file written
/// before conversation shares reads as asset links.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(untagged)]
pub(crate) enum ShareTarget {
    Asset {
        store: AssetStore,
        asset: AssetId,
        title: String,
    },
    Session {
        session: SessionId,
        label: String,
        can_send: bool,
    },
}

impl ShareRecord {
    /// Whether the link still works at `now`.
    pub(crate) fn live(&self, now: jiff::Timestamp) -> bool {
        self.expires_at.is_none_or(|at| at > now)
    }

    /// The asset this link shows, when it shows one.
    pub(crate) fn asset(&self) -> Option<AssetId> {
        match &self.target {
            ShareTarget::Asset { asset, .. } => Some(*asset),
            ShareTarget::Session { .. } => None,
        }
    }
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct ShareFile {
    version: u32,
    #[serde(default)]
    shares: Vec<ShareRecord>,
}

#[derive(Default)]
struct State {
    /// `None` until the file is read.
    records: Option<Vec<ShareRecord>>,
    /// Views counted since the file was last written.
    views_dirty: bool,
    /// The guests connected now, by link. Nothing of it is kept.
    guests: HashMap<ShareId, u32>,
}

/// Every share link of a data root.
pub(crate) struct ShareStore {
    path: PathBuf,
    state: Mutex<State>,
    changes: tokio::sync::watch::Sender<u64>,
}

/// A new link ID: 128 bits from the OS's random source, base64url.
pub(crate) fn new_share_id() -> Result<ShareId, ShareError> {
    use base64::Engine as _;

    let mut bytes = [0_u8; 16];
    getrandom::getrandom(&mut bytes)
        .map_err(|error| ShareError::Store(format!("no random bytes: {error}")))?;
    let id = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
    ShareId::new(id).map_err(|error| ShareError::Store(error.to_string()))
}

impl ShareStore {
    pub(crate) fn new(data_root: &Path) -> Self {
        Self {
            path: data_root.join(SHARES_FILE),
            state: Mutex::default(),
            changes: tokio::sync::watch::channel(0).0,
        }
    }

    /// Signals every change to the links but view counts.
    pub(crate) fn subscribe(&self) -> tokio::sync::watch::Receiver<u64> {
        self.changes.subscribe()
    }

    fn changed(&self) {
        self.changes
            .send_modify(|count| *count = count.wrapping_add(1));
    }

    /// Runs `work` on the records, read from the file the first time, and
    /// writes them back when it returns `true` with its value.
    fn with<T>(
        &self,
        work: impl FnOnce(&mut Vec<ShareRecord>) -> (T, bool),
    ) -> Result<T, ShareError> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.records.is_none() {
            state.records = Some(read_file(&self.path)?);
        }
        let records = state.records.as_mut().expect("read above");
        let (value, write) = work(records);
        if write {
            write_file(&self.path, records)?;
            state.views_dirty = false;
            drop(state);
            self.changed();
        }
        Ok(value)
    }

    /// The live links, newest first; expired ones are removed.
    pub(crate) fn list(&self, now: jiff::Timestamp) -> Result<Vec<ShareRecord>, ShareError> {
        self.with(|records| {
            let before = records.len();
            records.retain(|record| record.live(now));
            let mut live = records.clone();
            live.sort_by_key(|record| std::cmp::Reverse(record.created.at));
            (live, records.len() != before)
        })
    }

    /// The live link `id`.
    pub(crate) fn get(
        &self,
        id: &ShareId,
        now: jiff::Timestamp,
    ) -> Result<Option<ShareRecord>, ShareError> {
        self.with(|records| {
            (
                records
                    .iter()
                    .find(|record| &record.id == id && record.live(now))
                    .cloned(),
                false,
            )
        })
    }

    /// The newest live link of `asset`.
    pub(crate) fn newest_of(
        &self,
        asset: AssetId,
        now: jiff::Timestamp,
    ) -> Result<Option<ShareRecord>, ShareError> {
        self.with(|records| {
            (
                records
                    .iter()
                    .filter(|record| record.asset() == Some(asset) && record.live(now))
                    .max_by_key(|record| record.created.at)
                    .cloned(),
                false,
            )
        })
    }

    /// Keeps `record`, a new link.
    pub(crate) fn insert(&self, record: ShareRecord) -> Result<(), ShareError> {
        self.with(|records| {
            records.push(record);
            ((), true)
        })
    }

    /// Changes link `id` with `change`, and returns it as it is then.
    pub(crate) fn update(
        &self,
        id: &ShareId,
        now: jiff::Timestamp,
        change: impl FnOnce(&mut ShareRecord),
    ) -> Result<ShareRecord, ShareError> {
        self.with(|records| {
            match records
                .iter_mut()
                .find(|record| &record.id == id && record.live(now))
            {
                Some(record) => {
                    change(record);
                    (Ok(record.clone()), true)
                }
                None => (Err(ShareError::NotFound(id.clone())), false),
            }
        })?
    }

    /// Removes every link `remove` picks, and says how many.
    pub(crate) fn remove(
        &self,
        mut remove: impl FnMut(&ShareRecord) -> bool,
    ) -> Result<u64, ShareError> {
        self.with(|records| {
            let before = records.len();
            records.retain(|record| !remove(record));
            let removed = (before - records.len()) as u64;
            (removed, removed > 0)
        })
    }

    /// Counts a view of link `id`, kept in memory until [`Self::flush`].
    pub(crate) fn viewed(&self, id: &ShareId, now: jiff::Timestamp) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(record) = state
            .records
            .as_mut()
            .and_then(|records| records.iter_mut().find(|record| &record.id == id))
        {
            record.views = record.views.saturating_add(1);
            record.last_viewed = Some(now);
            state.views_dirty = true;
        }
    }

    /// Counts a guest connecting through link `id`: a view, and one more
    /// connected until [`Self::guest_left`].
    pub(crate) fn guest_joined(&self, id: &ShareId, now: jiff::Timestamp) {
        self.viewed(id, now);
        *self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .guests
            .entry(id.clone())
            .or_insert(0) += 1;
        self.changed();
    }

    /// A guest of link `id` went away.
    pub(crate) fn guest_left(&self, id: &ShareId) {
        {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(count) = state.guests.get_mut(id) {
                *count = count.saturating_sub(1);
                if *count == 0 {
                    state.guests.remove(id);
                }
            }
        }
        self.changed();
    }

    /// How many guests are connected through link `id` now.
    pub(crate) fn guests(&self, id: &ShareId) -> u32 {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .guests
            .get(id)
            .copied()
            .unwrap_or(0)
    }

    /// Writes the view counts counted since the last write, and removes
    /// expired links.
    pub(crate) fn flush(&self, now: jiff::Timestamp) -> Result<(), ShareError> {
        let dirty = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .views_dirty;
        self.with(|records| {
            let before = records.len();
            records.retain(|record| record.live(now));
            ((), dirty || records.len() != before)
        })
    }
}

fn store_error(path: &Path) -> impl FnOnce(io::Error) -> ShareError + '_ {
    move |error| ShareError::Store(format!("{}: {error}", path.display()))
}

fn read_file(path: &Path) -> Result<Vec<ShareRecord>, ShareError> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(store_error(path)(error)),
    };
    let file: ShareFile = serde_json::from_str(&text)
        .map_err(|error| ShareError::Store(format!("{}: {error}", path.display())))?;
    if file.version != FILE_VERSION {
        return Err(ShareError::Store(format!(
            "{}: version {} is not {FILE_VERSION}",
            path.display(),
            file.version
        )));
    }
    Ok(file.shares)
}

/// Writes the records through a flushed temporary file renamed over the
/// store's, readable by the user alone: the IDs are the links' secrets.
fn write_file(path: &Path, records: &[ShareRecord]) -> Result<(), ShareError> {
    #[derive(Serialize)]
    struct Borrowed<'a> {
        version: u32,
        shares: &'a [ShareRecord],
    }
    let bytes = serde_json::to_vec_pretty(&Borrowed {
        version: FILE_VERSION,
        shares: records,
    })
    .map_err(|error| ShareError::Store(error.to_string()))?;
    let directory = path.parent().expect("the share file has a folder");
    fs::create_dir_all(directory).map_err(store_error(directory))?;
    let temporary = directory.join(format!(".{SHARES_FILE}.{}.tmp", uuid::Uuid::now_v7()));
    let result = (|| {
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let mut output = options.open(&temporary)?;
        io::Write::write_all(&mut output, &bytes)?;
        output.sync_all()?;
        drop(output);
        fs::rename(&temporary, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result.map_err(store_error(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(id: &str, asset: AssetId, at: &str, expires_at: Option<&str>) -> ShareRecord {
        ShareRecord {
            id: ShareId::new(id).unwrap(),
            target: ShareTarget::Asset {
                store: AssetStore::Session {
                    session: SessionId::new_v7(),
                },
                asset,
                title: "Loss curves".into(),
            },
            created: ShareCreated {
                at: at.parse().unwrap(),
                session: None,
                agent: None,
            },
            expires_at: expires_at.map(|at| at.parse().unwrap()),
            views: 0,
            last_viewed: None,
        }
    }

    fn at(time: &str) -> jiff::Timestamp {
        time.parse().unwrap()
    }

    #[test]
    fn ids_are_random_base64url() {
        let first = new_share_id().unwrap();
        let second = new_share_id().unwrap();
        assert_eq!(first.as_str().len(), ShareId::LEN);
        assert_ne!(first, second);
    }

    #[test]
    fn links_survive_a_new_store_and_expire_at_their_time() {
        let dir = tempfile::tempdir().unwrap();
        let asset = AssetId::new_v7();
        let store = ShareStore::new(dir.path());
        store
            .insert(record(
                "aaaaaaaaaaaaaaaaaaaaaa",
                asset,
                "2026-11-01T00:00:00Z",
                Some("2026-11-08T00:00:00Z"),
            ))
            .unwrap();
        store
            .insert(record(
                "bbbbbbbbbbbbbbbbbbbbbb",
                asset,
                "2026-11-02T00:00:00Z",
                None,
            ))
            .unwrap();
        let reopened = ShareStore::new(dir.path());
        let before = reopened.list(at("2026-11-07T23:59:59Z")).unwrap();
        assert_eq!(
            before
                .iter()
                .map(|record| record.id.as_str())
                .collect::<Vec<_>>(),
            ["bbbbbbbbbbbbbbbbbbbbbb", "aaaaaaaaaaaaaaaaaaaaaa"]
        );
        let id = ShareId::new("aaaaaaaaaaaaaaaaaaaaaa").unwrap();
        assert!(
            reopened
                .get(&id, at("2026-11-08T00:00:00Z"))
                .unwrap()
                .is_none(),
            "a link is gone at its expiry"
        );
        assert_eq!(reopened.list(at("2026-11-08T00:00:00Z")).unwrap().len(), 1);
        // The expired link was removed from the file too.
        let text = fs::read_to_string(dir.path().join(SHARES_FILE)).unwrap();
        assert!(!text.contains("aaaaaaaaaaaaaaaaaaaaaa"));
    }

    #[test]
    fn views_are_written_on_flush() {
        let dir = tempfile::tempdir().unwrap();
        let store = ShareStore::new(dir.path());
        let id = ShareId::new("cccccccccccccccccccccc").unwrap();
        store
            .insert(record(
                id.as_str(),
                AssetId::new_v7(),
                "2026-11-01T00:00:00Z",
                None,
            ))
            .unwrap();
        store.viewed(&id, at("2026-11-02T00:00:00Z"));
        store.viewed(&id, at("2026-11-03T00:00:00Z"));
        assert_eq!(
            ShareStore::new(dir.path())
                .get(&id, at("2026-11-03T00:00:00Z"))
                .unwrap()
                .unwrap()
                .views,
            0,
            "views are in memory until flushed"
        );
        store.flush(at("2026-11-03T00:00:00Z")).unwrap();
        let read = ShareStore::new(dir.path())
            .get(&id, at("2026-11-03T00:00:00Z"))
            .unwrap()
            .unwrap();
        assert_eq!(read.views, 2);
        assert_eq!(read.last_viewed, Some(at("2026-11-03T00:00:00Z")));
    }

    #[test]
    fn remove_says_how_many_and_newest_of_picks_the_latest_live_link() {
        let dir = tempfile::tempdir().unwrap();
        let store = ShareStore::new(dir.path());
        let kept = AssetId::new_v7();
        let gone = AssetId::new_v7();
        for (id, asset, created) in [
            ("dddddddddddddddddddddd", kept, "2026-11-01T00:00:00Z"),
            ("eeeeeeeeeeeeeeeeeeeeee", kept, "2026-11-02T00:00:00Z"),
            ("ffffffffffffffffffffff", gone, "2026-11-03T00:00:00Z"),
        ] {
            store.insert(record(id, asset, created, None)).unwrap();
        }
        let now = at("2026-11-04T00:00:00Z");
        assert_eq!(
            store.newest_of(kept, now).unwrap().unwrap().id.as_str(),
            "eeeeeeeeeeeeeeeeeeeeee"
        );
        assert_eq!(
            store.remove(|record| record.asset() == Some(gone)).unwrap(),
            1
        );
        assert!(store.newest_of(gone, now).unwrap().is_none());
        assert_eq!(store.list(now).unwrap().len(), 2);
    }

    #[test]
    fn a_file_of_asset_links_reads_and_both_kinds_survive_a_new_store() {
        let dir = tempfile::tempdir().unwrap();
        let session = SessionId::new_v7();
        let asset = AssetId::new_v7();
        // As 1.86.0 wrote it: the asset's fields at the record's top level.
        fs::write(
            dir.path().join(SHARES_FILE),
            serde_json::json!({
                "version": 1,
                "shares": [{
                    "id": "hhhhhhhhhhhhhhhhhhhhhh",
                    "store": AssetStore::Session { session },
                    "asset": asset,
                    "title": "Loss curves",
                    "created": { "at": "2026-11-01T00:00:00Z" },
                }],
            })
            .to_string(),
        )
        .unwrap();
        let now = at("2026-11-02T00:00:00Z");
        let store = ShareStore::new(dir.path());
        let guest_link = ShareId::new("iiiiiiiiiiiiiiiiiiiiii").unwrap();
        store
            .insert(ShareRecord {
                id: guest_link.clone(),
                target: ShareTarget::Session {
                    session,
                    label: "Alice".into(),
                    can_send: false,
                },
                created: ShareCreated {
                    at: now,
                    session: None,
                    agent: None,
                },
                expires_at: None,
                views: 0,
                last_viewed: None,
            })
            .unwrap();
        let again = ShareStore::new(dir.path());
        assert_eq!(again.list(now).unwrap().len(), 2);
        assert!(matches!(
            again.get(&guest_link, now).unwrap().unwrap().target,
            ShareTarget::Session {
                can_send: false,
                ..
            }
        ));
    }

    #[test]
    fn a_guest_counts_as_a_view_and_while_connected() {
        let dir = tempfile::tempdir().unwrap();
        let store = ShareStore::new(dir.path());
        let id = ShareId::new("jjjjjjjjjjjjjjjjjjjjjj").unwrap();
        store
            .insert(record(
                id.as_str(),
                AssetId::new_v7(),
                "2026-11-01T00:00:00Z",
                None,
            ))
            .unwrap();
        let now = at("2026-11-02T00:00:00Z");
        store.guest_joined(&id, now);
        store.guest_joined(&id, now);
        assert_eq!(store.guests(&id), 2);
        store.guest_left(&id);
        assert_eq!(store.guests(&id), 1);
        store.guest_left(&id);
        store.guest_left(&id);
        assert_eq!(store.guests(&id), 0);
        assert_eq!(store.get(&id, now).unwrap().unwrap().views, 2);
    }

    #[cfg(unix)]
    #[test]
    fn the_file_is_the_users_alone() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().unwrap();
        let store = ShareStore::new(dir.path());
        store
            .insert(record(
                "gggggggggggggggggggggg",
                AssetId::new_v7(),
                "2026-11-01T00:00:00Z",
                None,
            ))
            .unwrap();
        let mode = fs::metadata(dir.path().join(SHARES_FILE))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}
