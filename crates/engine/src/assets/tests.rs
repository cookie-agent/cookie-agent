use std::{collections::HashSet, fs, path::Path, sync::Arc};

use cookie_agent_protocol::{AssetHandle, AssetStamp, AssetStore, SessionId};

use super::{AssetError, AssetStores, AssetWrite, BLOBS_DIR, INDEX_FILE, media_type};

fn stores(root: &Path) -> AssetStores {
    AssetStores::new(root)
}

fn stamp() -> AssetStamp {
    AssetStamp {
        at: jiff::Timestamp::now(),
        session: SessionId::new_v7(),
        agent: None,
    }
}

fn new_asset(bytes: &[u8], filename: &str) -> AssetWrite {
    AssetWrite {
        replace: None,
        title: None,
        filename: Some(filename.to_owned()),
        media_type: media_type(filename, bytes).to_owned(),
        bytes: bytes.to_vec(),
        stamp: stamp(),
    }
}

fn blobs(stores: &AssetStores, store: &AssetStore) -> usize {
    fs::read_dir(stores.dir(store).join(BLOBS_DIR))
        .map(|entries| entries.count())
        .unwrap_or(0)
}

#[test]
fn a_write_replaces_an_asset_and_frees_the_content_nothing_else_holds() {
    let root = tempfile::tempdir().unwrap();
    let stores = stores(root.path());
    let store = AssetStore::Shared;
    let none = HashSet::new();
    assert!(stores.list(&store).unwrap().is_empty(), "no store yet");

    let (first, new) = stores
        .write(&store, new_asset(b"<svg></svg>", "plot.svg"), &none)
        .unwrap();
    assert!(new);
    assert_eq!(first.title, "plot.svg", "titled by its file name");
    assert_eq!(first.media_type, "image/svg+xml");
    assert_eq!(blobs(&stores, &store), 1);

    let mut replace = new_asset(b"<svg><rect/></svg>", "plot.svg");
    replace.replace = Some(first.id);
    replace.title = Some("Loss curves".into());
    let (replaced, new) = stores.write(&store, replace, &none).unwrap();
    assert!(!new);
    assert_eq!(replaced.id, first.id);
    assert_eq!(replaced.handle, first.handle, "the handle never changes");
    assert_eq!(replaced.title, "Loss curves");
    assert_ne!(replaced.sha256, first.sha256);
    assert_eq!(replaced.created, first.created);
    assert_eq!(blobs(&stores, &store), 1, "the old content went");

    // Replacing keeps the title when none is given.
    let mut again = new_asset(b"<svg><circle/></svg>", "plot.svg");
    again.replace = Some(first.id);
    let (again, _) = stores.write(&store, again, &none).unwrap();
    assert_eq!(again.title, "Loss curves");

    let (bytes, read) = stores.read(&store, first.id, 6, 6).unwrap();
    assert_eq!(bytes, b"circle");
    assert_eq!(read.sha256, again.sha256);

    let deleted = stores.delete(&store, first.id).unwrap();
    assert_eq!(deleted.id, first.id);
    assert_eq!(blobs(&stores, &store), 0);
    assert!(matches!(
        stores.get(&store, first.id),
        Err(AssetError::NotFound(id)) if id == first.id
    ));
    assert!(matches!(
        stores.delete(&store, first.id),
        Err(AssetError::NotFound(_))
    ));
}

#[test]
fn assets_with_the_same_bytes_share_one_blob_until_both_are_gone() {
    let root = tempfile::tempdir().unwrap();
    let stores = stores(root.path());
    let store = AssetStore::Session {
        session: SessionId::new_v7(),
    };
    let none = HashSet::new();
    let (one, _) = stores
        .write(&store, new_asset(b"# Notes", "a.md"), &none)
        .unwrap();
    let (two, _) = stores
        .write(&store, new_asset(b"# Notes", "b.md"), &none)
        .unwrap();
    assert_ne!(one.handle, two.handle);
    assert_eq!(one.sha256, two.sha256);
    assert_eq!(blobs(&stores, &store), 1);
    stores.delete(&store, one.id).unwrap();
    assert_eq!(blobs(&stores, &store), 1, "the other asset still holds it");
    assert_eq!(stores.read(&store, two.id, 0, 64).unwrap().0, b"# Notes");
    stores.delete(&store, two.id).unwrap();
    assert_eq!(blobs(&stores, &store), 0);
}

#[test]
fn the_first_open_removes_what_a_crash_left_behind() {
    let root = tempfile::tempdir().unwrap();
    let store = AssetStore::Shared;
    let kept = {
        let stores = stores(root.path());
        let (asset, _) = stores
            .write(&store, new_asset(b"kept", "kept.txt"), &HashSet::new())
            .unwrap();
        asset
    };
    let dir = root.path().join("assets");
    // A blob whose index write never happened, and half-written files.
    fs::write(dir.join(BLOBS_DIR).join("0".repeat(64)), b"orphan").unwrap();
    fs::write(dir.join(BLOBS_DIR).join(".abc.tmp"), b"half").unwrap();
    fs::write(dir.join(".index.json.123.tmp"), b"{").unwrap();

    let stores = stores(root.path());
    let listed = stores.list(&store).unwrap();
    assert_eq!(listed.len(), 1);
    let names = fs::read_dir(dir.join(BLOBS_DIR))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    assert_eq!(names, vec![kept.sha256.clone()]);
    assert!(!dir.join(".index.json.123.tmp").exists());
    assert_eq!(stores.read(&store, kept.id, 0, 64).unwrap().0, b"kept");
}

#[test]
fn concurrent_writers_each_land_and_the_last_write_wins() {
    let root = tempfile::tempdir().unwrap();
    let stores = Arc::new(stores(root.path()));
    let store = AssetStore::Shared;
    let (target, _) = stores
        .write(&store, new_asset(b"start", "shared.txt"), &HashSet::new())
        .unwrap();
    let writers = (0..8)
        .map(|index| {
            let stores = Arc::clone(&stores);
            let store = store.clone();
            std::thread::spawn(move || {
                let content = format!("writer {index}");
                stores
                    .write(
                        &store,
                        new_asset(content.as_bytes(), &format!("{index}.txt")),
                        &HashSet::new(),
                    )
                    .unwrap();
                let mut replace = new_asset(content.as_bytes(), "shared.txt");
                replace.replace = Some(target.id);
                stores.write(&store, replace, &HashSet::new()).unwrap();
            })
        })
        .collect::<Vec<_>>();
    for writer in writers {
        writer.join().unwrap();
    }
    let assets = stores.list(&store).unwrap();
    assert_eq!(assets.len(), 9, "every new asset is in the index");
    let handles = assets
        .iter()
        .map(|asset| &asset.handle)
        .collect::<HashSet<_>>();
    assert_eq!(handles.len(), 9, "handles are unique");
    let shared = stores.get(&store, target.id).unwrap();
    let (bytes, _) = stores.read(&store, target.id, 0, 64).unwrap();
    assert!(String::from_utf8(bytes).unwrap().starts_with("writer "));
    // Every blob is named by an entry: the replaced contents went.
    let named = assets
        .iter()
        .map(|asset| &asset.sha256)
        .collect::<HashSet<_>>();
    assert_eq!(blobs(&stores, &store), named.len());
    assert!(named.contains(&shared.sha256));
}

#[test]
fn a_new_asset_avoids_the_handles_it_is_told_to() {
    let root = tempfile::tempdir().unwrap();
    let stores = stores(root.path());
    let store = AssetStore::Shared;
    // Every handle but one is taken, so the store must keep generating.
    let avoid = (0..32)
        .map(|_| AssetHandle::random())
        .collect::<HashSet<_>>();
    let (asset, _) = stores
        .write(&store, new_asset(b"x", "x.txt"), &avoid)
        .unwrap();
    assert!(!avoid.contains(&asset.handle));
}

#[test]
fn an_index_that_does_not_read_is_refused_and_left_alone() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("assets");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join(INDEX_FILE), b"not json").unwrap();
    let stores = stores(root.path());
    let store = AssetStore::Shared;
    assert!(matches!(stores.list(&store), Err(AssetError::Store(_))));
    assert!(matches!(
        stores.write(&store, new_asset(b"x", "x.txt"), &HashSet::new()),
        Err(AssetError::Store(_))
    ));
    assert_eq!(fs::read(dir.join(INDEX_FILE)).unwrap(), b"not json");
}

#[test]
fn stores_live_apart_by_tree_with_their_project_or_in_the_data_root() {
    let root = tempfile::tempdir().unwrap();
    let stores = stores(root.path());
    let session = SessionId::new_v7();
    assert_eq!(
        stores.dir(&AssetStore::Session { session }),
        root.path().join("session-assets").join(session.to_string())
    );
    let project =
        cookie_agent_protocol::ProjectId::new("0192f0c4-7d6e-7a1b-9c2d-3e4f5a6b7c8d").unwrap();
    assert_eq!(
        stores.dir(&AssetStore::Project {
            project: project.clone()
        }),
        root.path()
            .join("projects")
            .join(project.as_str())
            .join("assets")
    );
    assert_eq!(stores.dir(&AssetStore::Shared), root.path().join("assets"));
}

#[test]
fn media_types_come_from_the_content_then_the_extension_of_text() {
    let png = b"\x89PNG\r\n\x1a\n rest";
    assert_eq!(media_type("chart.bin", png), "image/png");
    assert_eq!(
        media_type("diagram.svg", b"<?xml version=\"1.0\"?>\n<svg/>"),
        "image/svg+xml"
    );
    assert_eq!(media_type("diagram.svg", b"not an svg"), "text/plain");
    assert_eq!(
        media_type("page.HTML", b"<!doctype html><p>hi"),
        "text/html"
    );
    assert_eq!(media_type("page.htm", b"<p>hi"), "text/html");
    assert_eq!(media_type("report.md", b"# Report"), "text/markdown");
    assert_eq!(media_type("notes.markdown", b"# Notes"), "text/markdown");
    assert_eq!(media_type("data.csv", b"a,b\n1,2\n"), "text/plain");
    assert_eq!(
        media_type("page.html", b"\x80\x81 binary"),
        "application/octet-stream"
    );
    assert_eq!(media_type("blob", b"a\0b"), "application/octet-stream");
}

#[test]
fn the_trees_with_assets_are_the_folders_of_session_assets() {
    let root = tempfile::tempdir().unwrap();
    let stores = stores(root.path());
    assert!(stores.session_roots().unwrap().is_empty(), "no folder yet");
    let none = HashSet::new();
    let (first, second) = (SessionId::new_v7(), SessionId::new_v7());
    for session in [first, second] {
        stores
            .write(
                &AssetStore::Session { session },
                new_asset(session.to_string().as_bytes(), "note.txt"),
                &none,
            )
            .unwrap();
    }
    // A stray file is not a tree.
    fs::write(root.path().join("session-assets").join("notes.txt"), "x").unwrap();
    let mut expected = vec![first, second];
    expected.sort();
    assert_eq!(stores.session_roots().unwrap(), expected);

    // Removing a tree's store removes its folder and says what it held.
    assert_eq!(
        stores
            .remove(&AssetStore::Session { session: first })
            .unwrap(),
        1
    );
    assert_eq!(stores.session_roots().unwrap(), vec![second]);
    assert!(
        stores
            .list(&AssetStore::Session { session: first })
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        stores
            .remove(&AssetStore::Session { session: first })
            .unwrap(),
        0
    );
}

#[test]
fn an_index_kept_in_memory_follows_writes_from_elsewhere() {
    let root = tempfile::tempdir().unwrap();
    // Two processes sharing a data root: each keeps its own indexes.
    let (here, there) = (stores(root.path()), stores(root.path()));
    let store = AssetStore::Shared;
    let none = HashSet::new();
    let (asset, _) = here
        .write(&store, new_asset(b"one", "one.txt"), &none)
        .unwrap();
    assert_eq!(there.list(&store).unwrap().len(), 1);
    // Kept, the same index answers again.
    assert_eq!(there.get(&store, asset.id).unwrap().title, "one.txt");
    here.rename(&store, asset.id, "Renamed".into()).unwrap();
    assert_eq!(there.get(&store, asset.id).unwrap().title, "Renamed");
    here.write(&store, new_asset(b"two", "two.txt"), &none)
        .unwrap();
    assert_eq!(there.list(&store).unwrap().len(), 2);
    here.delete(&store, asset.id).unwrap();
    assert_eq!(there.list(&store).unwrap().len(), 1);
    // Its own writes it never misses.
    there
        .rename(&store, there.list(&store).unwrap()[0].id, "Mine".into())
        .unwrap();
    assert_eq!(there.list(&store).unwrap()[0].title, "Mine");
}
