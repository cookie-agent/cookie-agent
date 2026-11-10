//! The asset stores (`docs/site/specs/assets.md` section 4): files agents
//! write for the user, kept by a session tree, a project, or the shared
//! store of sessions in no project.
//!
//! Every store is a folder holding `index.json` (every asset's fields),
//! `blobs/<sha256>`, and `thumbnails/<sha256>.jpg` or `.png`, the
//! thumbnails `asset.thumbnail` made of a content
//! (`docs/site/specs/asset-thumbnails.md`):
//!
//! - a session tree's in `<data_root>/session-assets/<root-id>/`, apart
//!   from the sessions, so listing that folder finds every tree that has
//!   assets without reading a session; the tree's deletion removes it;
//! - a project's in `<data_root>/projects/<id>/assets/`, so it goes with
//!   the project;
//! - the shared store in `<data_root>/assets/`.
//!
//! A store's index is kept in memory once read, and read again only when
//! its file's modification time or length moved, so listing many stores
//! costs a `stat` each.
//!
//! **The index is the only reference to a blob.** Session logs name an
//! asset by its ID, never by its blob, so a blob is in use exactly when an
//! index entry names its digest: replacing or deleting an asset rewrites the
//! index, then deletes the old blob unless another entry still names it. A
//! crash between the two leaves a blob no entry names, which the store's
//! first open in a process removes, with half-written temporaries. A
//! thumbnail goes with its blob.
//!
//! Every change and every read of a store holds that store's lock, so a
//! read never sees a blob deleted under it. Files are replaced by renaming
//! a flushed temporary file over them. The lock is the process's: two
//! daemons sharing a data root are not serialized, as for crons.

use std::{
    collections::{HashMap, HashSet},
    fs, io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use cookie_agent_protocol::{
    Asset, AssetHandle, AssetId, AssetStamp, AssetStore, ProjectId, SessionId,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

#[cfg(test)]
mod tests;
pub(crate) mod thumbnail;

/// The folder of a project's store, and of the shared store.
pub(crate) const ASSETS_DIR: &str = "assets";
/// The folder of the session trees' stores, one per root.
pub(crate) const SESSION_ASSETS_DIR: &str = "session-assets";
const INDEX_FILE: &str = "index.json";
const BLOBS_DIR: &str = "blobs";
const THUMBNAILS_DIR: &str = "thumbnails";
const INDEX_VERSION: u32 = 1;

/// An asset request was refused or failed.
#[derive(Debug, thiserror::Error)]
pub enum AssetError {
    /// The store holds no asset with this ID.
    #[error("there is no asset {0} in that store")]
    NotFound(AssetId),
    /// The request named a store that does not exist, such as a session
    /// store of a session that is not a root.
    #[error("{0}")]
    InvalidStore(String),
    /// Reading or writing the store's files failed.
    #[error("the asset store failed: {0}")]
    Store(String),
}

fn store_error(path: &Path) -> impl FnOnce(io::Error) -> AssetError + '_ {
    move |error| AssetError::Store(format!("{}: {error}", path.display()))
}

/// `index.json`.
#[derive(Debug, Default, Deserialize, Serialize)]
struct IndexFile {
    version: u32,
    #[serde(default)]
    assets: Vec<Record>,
}

/// One asset as its store's index keeps it: an [`Asset`] without its
/// store, which the folder says.
#[derive(Clone, Debug, Deserialize, Serialize)]
struct Record {
    id: AssetId,
    handle: AssetHandle,
    title: String,
    media_type: String,
    byte_length: u64,
    sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    filename: Option<String>,
    created: AssetStamp,
    updated: AssetStamp,
}

impl Record {
    fn asset(&self, store: &AssetStore) -> Asset {
        Asset {
            id: self.id,
            handle: self.handle.clone(),
            store: store.clone(),
            title: self.title.clone(),
            media_type: self.media_type.clone(),
            byte_length: self.byte_length,
            sha256: self.sha256.clone(),
            filename: self.filename.clone(),
            created: self.created.clone(),
            updated: self.updated.clone(),
        }
    }
}

/// What `asset.thumbnail` starts from: the asset, where its content is,
/// and the thumbnail kept of that content, if any.
pub(crate) struct ThumbnailSource {
    pub(crate) asset: Asset,
    pub(crate) blob: PathBuf,
    pub(crate) kept: Option<thumbnail::Picture>,
}

/// What a write stores: new content for a new asset, or for the asset
/// `replace`.
#[derive(Debug)]
pub(crate) struct AssetWrite {
    pub(crate) replace: Option<AssetId>,
    /// The title; a new asset without one is titled by its file name, and a
    /// replaced one keeps its title.
    pub(crate) title: Option<String>,
    pub(crate) filename: Option<String>,
    pub(crate) media_type: String,
    pub(crate) bytes: Vec<u8>,
    pub(crate) stamp: AssetStamp,
}

/// Every store of a data root.
/// How many stores' indexes stay in memory.
const MAX_INDEXES_HELD: usize = 64;

pub(crate) struct AssetStores {
    data_root: PathBuf,
    locks: Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>,
    /// Stores whose leftovers this process removed.
    swept: Mutex<HashSet<PathBuf>>,
    /// Each store's index as last read, with the stamp of its file then.
    indexes: Mutex<HashMap<PathBuf, (IndexStamp, Arc<IndexFile>)>>,
    changes: tokio::sync::watch::Sender<u64>,
    /// How many thumbnails are drawn at once.
    drawing: tokio::sync::Semaphore,
    /// The contents, by digest, that did not draw: asked again, they have
    /// no thumbnail at once.
    undrawable: Mutex<HashSet<String>>,
}

/// How many thumbnails are drawn at once: a list's worth of cards asking
/// together must not start a tool for each.
const DRAWING_AT_ONCE: usize = 2;
/// How many contents that did not draw are remembered.
const MAX_UNDRAWABLE_HELD: usize = 4096;

/// How long after its last write an index's stamp is not trusted: a
/// filesystem's clock can be coarse enough that a second write of the
/// same length right after the first keeps the first one's time.
const SETTLE_TIME: std::time::Duration = std::time::Duration::from_secs(2);

/// What says an index file changed: its modification time and length.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct IndexStamp {
    modified: Option<std::time::SystemTime>,
    length: u64,
}

impl IndexStamp {
    /// The stamp of the index at `path`; `None` when there is none.
    fn of(path: &Path) -> Result<Option<Self>, AssetError> {
        match fs::metadata(path) {
            Ok(metadata) => Ok(Some(Self {
                modified: metadata.modified().ok(),
                length: metadata.len(),
            })),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(store_error(path)(error)),
        }
    }

    /// Whether a later write must change this stamp: the file was last
    /// written long enough ago that its time cannot be a later write's.
    fn settled(self) -> bool {
        self.modified.is_some_and(|modified| {
            std::time::SystemTime::now()
                .duration_since(modified)
                .is_ok_and(|age| age >= SETTLE_TIME)
        })
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl AssetStores {
    pub(crate) fn new(data_root: &Path) -> Self {
        Self {
            data_root: data_root.to_owned(),
            locks: Mutex::default(),
            swept: Mutex::default(),
            indexes: Mutex::default(),
            changes: tokio::sync::watch::channel(0).0,
            drawing: tokio::sync::Semaphore::new(DRAWING_AT_ONCE),
            undrawable: Mutex::default(),
        }
    }

    /// A turn to draw a thumbnail.
    pub(crate) async fn draw_turn(&self) -> Option<tokio::sync::SemaphorePermit<'_>> {
        self.drawing.acquire().await.ok()
    }

    /// Whether the content `sha256` did not draw before.
    pub(crate) fn undrawable(&self, sha256: &str) -> bool {
        lock(&self.undrawable).contains(sha256)
    }

    /// Remembers that the content `sha256` does not draw.
    pub(crate) fn mark_undrawable(&self, sha256: &str) {
        let mut undrawable = lock(&self.undrawable);
        if undrawable.len() >= MAX_UNDRAWABLE_HELD {
            undrawable.clear();
        }
        undrawable.insert(sha256.to_owned());
    }

    /// Signals every change this process makes to any store.
    pub(crate) fn subscribe(&self) -> tokio::sync::watch::Receiver<u64> {
        self.changes.subscribe()
    }

    fn changed(&self) {
        self.changes
            .send_modify(|count| *count = count.wrapping_add(1));
    }

    /// The folder of `store`, whether or not it exists.
    pub(crate) fn dir(&self, store: &AssetStore) -> PathBuf {
        match store {
            AssetStore::Session { session } => self.session_dir(*session),
            AssetStore::Project { project } => self.project_dir(project),
            AssetStore::Shared => self.data_root.join(ASSETS_DIR),
        }
    }

    fn session_dir(&self, root: SessionId) -> PathBuf {
        self.data_root
            .join(SESSION_ASSETS_DIR)
            .join(root.to_string())
    }

    /// The roots of the session trees that have a store: the folders of
    /// `session-assets/`, read without touching any session.
    pub(crate) fn session_roots(&self) -> Result<Vec<SessionId>, AssetError> {
        let dir = self.data_root.join(SESSION_ASSETS_DIR);
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(store_error(&dir)(error)),
        };
        let mut roots = entries
            .flatten()
            .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
            .filter_map(|entry| entry.file_name().to_str()?.parse().ok())
            .collect::<Vec<SessionId>>();
        roots.sort();
        Ok(roots)
    }

    /// Removes the store `store` whole, as its owner's deletion does, and
    /// says how many assets it held.
    pub(crate) fn remove(&self, store: &AssetStore) -> Result<u64, AssetError> {
        let removed = self.locked(store, |dir| {
            let count = read_index(dir)?.assets.len() as u64;
            match fs::remove_dir_all(dir) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(store_error(dir)(error)),
            }
            lock(&self.indexes).remove(dir);
            Ok(count)
        })?;
        if removed > 0 {
            self.changed();
        }
        Ok(removed)
    }

    /// The store's index, from memory while its file has not changed
    /// since it was read; one written moments ago is read again. Called
    /// under the store's lock.
    fn index(&self, dir: &Path) -> Result<Arc<IndexFile>, AssetError> {
        let stamp = IndexStamp::of(&dir.join(INDEX_FILE))?;
        if let Some(stamp) = stamp
            && let Some((cached, index)) = lock(&self.indexes).get(dir)
            && *cached == stamp
        {
            return Ok(Arc::clone(index));
        }
        let index = Arc::new(read_index(dir)?);
        match stamp {
            Some(stamp) if stamp.settled() => {
                let mut indexes = lock(&self.indexes);
                // Each session tree has a store, and an index is read again
                // cheaply: only the ones in use stay for long.
                if indexes.len() >= MAX_INDEXES_HELD {
                    indexes.clear();
                }
                indexes.insert(dir.to_owned(), (stamp, Arc::clone(&index)));
            }
            _ => {
                lock(&self.indexes).remove(dir);
            }
        }
        Ok(index)
    }

    /// Writes `index` as the store's, and forgets the one in memory.
    fn write_index(&self, dir: &Path, index: &IndexFile) -> Result<(), AssetError> {
        lock(&self.indexes).remove(dir);
        write_index(dir, index)
    }

    fn project_dir(&self, project: &ProjectId) -> PathBuf {
        self.data_root
            .join(crate::projects::PROJECTS_DIR)
            .join(project.as_str())
            .join(ASSETS_DIR)
    }

    /// Holds `store`'s lock for `work`, after removing the store's
    /// leftovers once per process.
    fn locked<T>(
        &self,
        store: &AssetStore,
        work: impl FnOnce(&Path) -> Result<T, AssetError>,
    ) -> Result<T, AssetError> {
        let dir = self.dir(store);
        let mutex = Arc::clone(lock(&self.locks).entry(dir.clone()).or_default());
        let _guard = lock(&mutex);
        if lock(&self.swept).insert(dir.clone())
            && let Err(error) = sweep(&dir)
        {
            eprintln!(
                "cookie-agent: leftovers in the asset store {} stay: {error}",
                dir.display()
            );
        }
        work(&dir)
    }

    /// The store's assets, newest first.
    pub(crate) fn list(&self, store: &AssetStore) -> Result<Vec<Asset>, AssetError> {
        self.locked(store, |dir| {
            let mut assets = self
                .index(dir)?
                .assets
                .iter()
                .map(|record| record.asset(store))
                .collect::<Vec<_>>();
            assets.sort_by_key(|asset| std::cmp::Reverse(asset.updated.at));
            Ok(assets)
        })
    }

    /// How many assets the store holds.
    pub(crate) fn count(&self, store: &AssetStore) -> Result<u64, AssetError> {
        self.locked(store, |dir| Ok(self.index(dir)?.assets.len() as u64))
    }

    pub(crate) fn get(&self, store: &AssetStore, id: AssetId) -> Result<Asset, AssetError> {
        self.locked(store, |dir| {
            self.index(dir)?
                .assets
                .iter()
                .find(|record| record.id == id)
                .map(|record| record.asset(store))
                .ok_or(AssetError::NotFound(id))
        })
    }

    /// Up to `length` bytes of the asset's content from `offset`, with the
    /// asset they belong to.
    pub(crate) fn read(
        &self,
        store: &AssetStore,
        id: AssetId,
        offset: u64,
        length: u64,
    ) -> Result<(Vec<u8>, Asset), AssetError> {
        self.locked(store, |dir| {
            let index = self.index(dir)?;
            let record = index
                .assets
                .iter()
                .find(|record| record.id == id)
                .ok_or(AssetError::NotFound(id))?;
            let path = dir.join(BLOBS_DIR).join(&record.sha256);
            let bytes = read_range(&path, offset, length).map_err(store_error(&path))?;
            Ok((bytes, record.asset(store)))
        })
    }

    /// Stores `write`: a new asset, whose handle is none of `avoid` nor
    /// any the store has, or new content for `write.replace`. Returns the
    /// asset and whether it is new.
    pub(crate) fn write(
        &self,
        store: &AssetStore,
        write: AssetWrite,
        avoid: &HashSet<AssetHandle>,
    ) -> Result<(Asset, bool), AssetError> {
        let written = self.locked(store, |dir| {
            let mut index = read_index(dir)?;
            let sha256 = hex(&Sha256::digest(&write.bytes));
            let blobs = dir.join(BLOBS_DIR);
            fs::create_dir_all(&blobs).map_err(store_error(&blobs))?;
            let blob = blobs.join(&sha256);
            if fs::metadata(&blob).map(|metadata| metadata.len()).ok()
                != Some(write.bytes.len() as u64)
            {
                write_atomic(&blob, &write.bytes)?;
            }
            let byte_length = write.bytes.len() as u64;
            let (position, old, new) = match write.replace {
                Some(id) => {
                    let position = index
                        .assets
                        .iter()
                        .position(|record| record.id == id)
                        .ok_or(AssetError::NotFound(id))?;
                    let record = &mut index.assets[position];
                    let old = std::mem::replace(&mut record.sha256, sha256);
                    if let Some(title) = write.title {
                        record.title = title;
                    }
                    record.media_type = write.media_type;
                    record.byte_length = byte_length;
                    if write.filename.is_some() {
                        record.filename = write.filename;
                    }
                    record.updated = write.stamp;
                    (position, Some(old), false)
                }
                None => {
                    let taken = index
                        .assets
                        .iter()
                        .map(|record| record.handle.clone())
                        .collect::<HashSet<_>>();
                    let handle = loop {
                        let handle = AssetHandle::random();
                        if !taken.contains(&handle) && !avoid.contains(&handle) {
                            break handle;
                        }
                    };
                    let title = write
                        .title
                        .or_else(|| write.filename.clone())
                        .unwrap_or_else(|| handle.to_string());
                    index.assets.push(Record {
                        id: AssetId::new_v7(),
                        handle,
                        title,
                        media_type: write.media_type,
                        byte_length,
                        sha256,
                        filename: write.filename,
                        created: write.stamp.clone(),
                        updated: write.stamp,
                    });
                    (index.assets.len() - 1, None, true)
                }
            };
            self.write_index(dir, &index)?;
            if let Some(old) = old {
                release_blob(dir, &index, &old);
            }
            Ok((index.assets[position].asset(store), new))
        })?;
        self.changed();
        Ok(written)
    }

    /// Gives the asset `id` the title `title`.
    pub(crate) fn rename(
        &self,
        store: &AssetStore,
        id: AssetId,
        title: String,
    ) -> Result<Asset, AssetError> {
        let renamed = self.locked(store, |dir| {
            let mut index = read_index(dir)?;
            let record = index
                .assets
                .iter_mut()
                .find(|record| record.id == id)
                .ok_or(AssetError::NotFound(id))?;
            record.title = title;
            let asset = record.asset(store);
            self.write_index(dir, &index)?;
            Ok(asset)
        })?;
        self.changed();
        Ok(renamed)
    }

    /// Deletes the asset `id`, and its blob unless another asset holds the
    /// same bytes. Returns what it was.
    pub(crate) fn delete(&self, store: &AssetStore, id: AssetId) -> Result<Asset, AssetError> {
        let deleted = self.locked(store, |dir| {
            let mut index = read_index(dir)?;
            let position = index
                .assets
                .iter()
                .position(|record| record.id == id)
                .ok_or(AssetError::NotFound(id))?;
            let record = index.assets.remove(position);
            self.write_index(dir, &index)?;
            release_blob(dir, &index, &record.sha256);
            Ok(record.asset(store))
        })?;
        self.changed();
        Ok(deleted)
    }

    /// The asset `id`, the path of its blob, and the thumbnail kept of its
    /// content. The blob is read outside the lock: content replaced
    /// meanwhile deletes it, and reading it then fails.
    pub(crate) fn thumbnail_source(
        &self,
        store: &AssetStore,
        id: AssetId,
    ) -> Result<ThumbnailSource, AssetError> {
        self.locked(store, |dir| {
            let index = self.index(dir)?;
            let record = index
                .assets
                .iter()
                .find(|record| record.id == id)
                .ok_or(AssetError::NotFound(id))?;
            let kept = thumbnail::Format::ALL.into_iter().find_map(|format| {
                let path = thumbnail_path(dir, &record.sha256, format);
                let bytes = fs::read(path).ok()?;
                thumbnail::Picture::of(bytes, format)
            });
            Ok(ThumbnailSource {
                asset: record.asset(store),
                blob: dir.join(BLOBS_DIR).join(&record.sha256),
                kept,
            })
        })
    }

    /// Keeps `picture`, a thumbnail of the content `sha256`, while an
    /// asset of the store still has that content.
    pub(crate) fn keep_thumbnail(
        &self,
        store: &AssetStore,
        sha256: &str,
        picture: &thumbnail::Picture,
    ) -> Result<(), AssetError> {
        self.locked(store, |dir| {
            if !self
                .index(dir)?
                .assets
                .iter()
                .any(|record| record.sha256 == sha256)
            {
                return Ok(());
            }
            write_atomic(&thumbnail_path(dir, sha256, picture.format), &picture.bytes)
        })
    }

    /// Tells clients that stores changed outside a write here, such as a
    /// project's or a tree's deleted with it.
    pub(crate) fn announce(&self) {
        self.changed();
    }
}

/// The store's index; an empty one when the store has none yet.
fn read_index(dir: &Path) -> Result<IndexFile, AssetError> {
    let path = dir.join(INDEX_FILE);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(IndexFile {
                version: INDEX_VERSION,
                assets: Vec::new(),
            });
        }
        Err(error) => return Err(store_error(&path)(error)),
    };
    let index: IndexFile = serde_json::from_slice(&bytes)
        .map_err(|error| AssetError::Store(format!("{} does not read: {error}", path.display())))?;
    if index.version != INDEX_VERSION {
        return Err(AssetError::Store(format!(
            "{} has version {}, which this build does not read",
            path.display(),
            index.version
        )));
    }
    Ok(index)
}

fn write_index(dir: &Path, index: &IndexFile) -> Result<(), AssetError> {
    let bytes = serde_json::to_vec_pretty(index)
        .map_err(|error| AssetError::Store(format!("the index does not encode: {error}")))?;
    write_atomic(&dir.join(INDEX_FILE), &bytes)
}

/// Deletes the blob `sha256` unless an entry of `index` still names it.
/// A blob that cannot be deleted is left for the next sweep.
fn release_blob(dir: &Path, index: &IndexFile, sha256: &str) {
    if index.assets.iter().any(|record| record.sha256 == sha256) {
        return;
    }
    let thumbnails = thumbnail::Format::ALL
        .into_iter()
        .map(|format| thumbnail_path(dir, sha256, format));
    for path in std::iter::once(dir.join(BLOBS_DIR).join(sha256)).chain(thumbnails) {
        if let Err(error) = fs::remove_file(&path)
            && error.kind() != io::ErrorKind::NotFound
        {
            eprintln!(
                "cookie-agent: the unused asset file {} stays: {error}",
                path.display()
            );
        }
    }
}

/// Where the store at `dir` keeps its thumbnail of the content `sha256`.
fn thumbnail_path(dir: &Path, sha256: &str, format: thumbnail::Format) -> PathBuf {
    dir.join(THUMBNAILS_DIR)
        .join(format!("{sha256}.{}", format.extension()))
}

/// Whether `name`, a file of the thumbnails folder, is the thumbnail of a
/// content in `named`.
fn names_thumbnail(name: &str, named: &HashSet<&str>) -> bool {
    name.split_once('.').is_some_and(|(sha256, extension)| {
        named.contains(sha256)
            && thumbnail::Format::ALL
                .into_iter()
                .any(|format| format.extension() == extension)
    })
}

/// Removes the blobs and thumbnails no entry names and every half-written
/// temporary of the store at `dir`. A store whose index does not read is left alone.
fn sweep(dir: &Path) -> Result<(), AssetError> {
    if !dir.is_dir() {
        return Ok(());
    }
    let index = read_index(dir)?;
    let named = index
        .assets
        .iter()
        .map(|record| record.sha256.as_str())
        .collect::<HashSet<_>>();
    let blobs = dir.join(BLOBS_DIR);
    let thumbnails = dir.join(THUMBNAILS_DIR);
    for folder in [dir.to_owned(), blobs.clone(), thumbnails.clone()] {
        let Ok(entries) = fs::read_dir(&folder) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let temporary = name.starts_with('.') && name.ends_with(".tmp");
            let unnamed = !temporary
                && if folder == blobs {
                    !named.contains(name.as_ref())
                } else if folder == thumbnails {
                    !names_thumbnail(&name, &named)
                } else {
                    false
                };
            if temporary || unnamed {
                let path = entry.path();
                fs::remove_file(&path).map_err(store_error(&path))?;
            }
        }
    }
    Ok(())
}

fn read_range(path: &Path, offset: u64, length: u64) -> io::Result<Vec<u8>> {
    use std::io::{Read as _, Seek as _};

    let mut file = fs::File::open(path)?;
    file.seek(io::SeekFrom::Start(offset))?;
    let mut bytes = Vec::new();
    file.take(length).read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// Writes `bytes` to `path` through a flushed temporary file in the same
/// folder, renamed over it.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), AssetError> {
    let directory = path.parent().expect("asset files have a folder");
    fs::create_dir_all(directory).map_err(store_error(directory))?;
    let name = path
        .file_name()
        .expect("asset files have a name")
        .to_string_lossy();
    let temporary = directory.join(format!(".{name}.{}.tmp", uuid::Uuid::now_v7()));
    let result = (|| {
        let mut output = fs::File::create(&temporary)?;
        io::Write::write_all(&mut output, bytes)?;
        output.sync_all()?;
        drop(output);
        fs::rename(&temporary, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result.map_err(store_error(path))?;
    #[cfg(unix)]
    if let Ok(folder) = fs::File::open(directory) {
        let _ = folder.sync_all();
    }
    Ok(())
}

fn hex(digest: &[u8]) -> String {
    use std::fmt::Write as _;

    digest.iter().fold(String::new(), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
        out
    })
}

/// The media type of an asset written from `filename` with content
/// `bytes` (section 7): what the content starts like, then SVG, HTML, and
/// Markdown by their extensions when the content is UTF-8 text, then plain
/// text, then `application/octet-stream`.
pub(crate) fn media_type(filename: &str, bytes: &[u8]) -> &'static str {
    let path = Path::new(filename);
    if let Some((media_type, _)) =
        cookie_agent_protocol::media_sniff::recognize(path, bytes, bytes.len() as u64)
    {
        return media_type;
    }
    let Some(text) = std::str::from_utf8(bytes)
        .ok()
        .filter(|text| !text.contains('\0'))
    else {
        return "application/octet-stream";
    };
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    match extension.as_str() {
        "svg" if looks_like_svg(text) => "image/svg+xml",
        "html" | "htm" => "text/html",
        "md" | "markdown" => "text/markdown",
        _ => "text/plain",
    }
}

/// Whether `text` has an `<svg` root near its start, after any XML
/// declaration, comments, and doctype.
fn looks_like_svg(text: &str) -> bool {
    let head = text
        .char_indices()
        .nth(4096)
        .map_or(text, |(end, _)| &text[..end]);
    head.to_ascii_lowercase().contains("<svg")
}
