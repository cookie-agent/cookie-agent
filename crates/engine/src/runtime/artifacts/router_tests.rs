use std::{collections::HashMap, fs, path::Path, sync::Arc};

use cookie_agent_protocol::SessionId;

use super::ArtifactRouter;

struct Placement {
    workdir: tempfile::TempDir,
    router: Arc<ArtifactRouter>,
    roots: [SessionId; 2],
    children: [SessionId; 2],
}

/// Router over two root trees, each with one child, resolved the way the
/// engine resolves them (§5.1).
fn placement() -> Placement {
    let workdir = tempfile::tempdir().expect("workdir");
    let roots = [SessionId::new_v7(), SessionId::new_v7()];
    let children = [SessionId::new_v7(), SessionId::new_v7()];
    let router = ArtifactRouter::open(workdir.path().to_path_buf()).expect("router");
    let mut membership: HashMap<SessionId, SessionId> = HashMap::new();
    membership.insert(children[0], roots[0]);
    membership.insert(children[1], roots[1]);
    router.install_tree_resolver(Arc::new(move |session| {
        Some(membership.get(&session).copied().unwrap_or(session))
    }));
    Placement {
        workdir,
        router,
        roots,
        children,
    }
}

#[test]
fn concurrent_first_writes_share_one_tree_store() {
    let fixture = placement();
    let gate = std::sync::Barrier::new(8);
    let stores = std::thread::scope(|scope| {
        let writers = (0..8)
            .map(|index| {
                let router = &fixture.router;
                let gate = &gate;
                let tree = fixture.roots[0];
                scope.spawn(move || {
                    gate.wait();
                    let store = router.tree_store(tree).expect("initialize tree store");
                    let content = format!("writer {index}");
                    let (_, digest) = store.retain(content.as_bytes()).expect("retain output");
                    assert_eq!(
                        store
                            .read_paged(&digest, 0, 1)
                            .expect("read output")
                            .content,
                        content
                    );
                    store
                })
            })
            .collect::<Vec<_>>();
        writers
            .into_iter()
            .map(|writer| writer.join().unwrap())
            .collect::<Vec<_>>()
    });
    assert!(stores.iter().all(|store| Arc::ptr_eq(store, &stores[0])));
}

fn tree_dir(workdir: &Path, root: SessionId) -> std::path::PathBuf {
    workdir.join(root.to_string())
}

fn tree_blob(workdir: &Path, root: SessionId, digest: &str) -> std::path::PathBuf {
    tree_dir(workdir, root)
        .join(super::ARTIFACTS_DIR)
        .join(digest)
}

#[test]
fn writes_land_in_the_directory_of_the_writing_trees_root() {
    let fixture = placement();
    let (reference, first) = fixture
        .router
        .retain(fixture.children[0], b"child-zero")
        .expect("retain in child tree");
    let (_, second) = fixture
        .router
        .retain(fixture.children[1], b"child-one")
        .expect("retain in the other tree");
    assert_eq!(reference.uri, format!("artifact://sha256/{first}"));
    assert!(tree_blob(fixture.workdir.path(), fixture.roots[0], &first).is_file());
    assert!(tree_blob(fixture.workdir.path(), fixture.roots[1], &second).is_file());
    assert!(
        !fixture.workdir.path().join("artifacts.shared").exists(),
        "no work-dir-wide store exists at all"
    );

    // Identical content is copied into the other tree rather than shared.
    let (_, duplicate) = fixture
        .router
        .retain(fixture.roots[1], b"child-zero")
        .expect("duplicate retain");
    assert_eq!(duplicate, first);
    assert!(tree_blob(fixture.workdir.path(), fixture.roots[1], &duplicate).is_file());
}

#[test]
fn unrouted_writes_land_in_the_sessions_own_tree() {
    let workdir = tempfile::tempdir().expect("workdir");
    let router = ArtifactRouter::open(workdir.path().to_path_buf()).expect("router");
    let session = SessionId::new_v7();
    let (_, digest) = router.retain(session, b"unrouted").expect("retain");
    assert!(tree_blob(workdir.path(), session, &digest).is_file());
    assert!(!workdir.path().join("artifacts.shared").exists());
}

/// Tree-local sessions D1: a read resolves in the reading session's own tree
/// and nowhere else, and reading from a tree that never stored anything does
/// not create its directory.
#[test]
fn reads_resolve_only_in_the_readers_tree() {
    let fixture = placement();
    let (_, digest) = fixture
        .router
        .retain(fixture.children[0], b"tree-zero")
        .expect("retain");
    for reader in [fixture.children[0], fixture.roots[0]] {
        assert_eq!(
            fixture
                .router
                .read_paged(reader, &digest, 0, 10)
                .expect("same tree")
                .content,
            "tree-zero"
        );
    }
    for reader in [fixture.children[1], fixture.roots[1]] {
        assert!(
            fixture
                .router
                .read_paged(reader, &digest, 0, 10)
                .unwrap_err()
                .to_string()
                .contains("artifact missing"),
            "another tree's content never resolves"
        );
    }
    assert!(
        !tree_dir(fixture.workdir.path(), fixture.roots[1]).exists(),
        "a read never creates a tree directory"
    );

    // A fresh router finds a tree's content on disk without any write of its own.
    let reopened =
        ArtifactRouter::open(fixture.workdir.path().to_path_buf()).expect("reopened router");
    assert_eq!(
        reopened
            .read_paged(fixture.roots[0], &digest, 0, 10)
            .expect("read after reopen")
            .content,
        "tree-zero"
    );
}

/// Tree-local sessions D2: content copied into a fork's tree includes what a
/// manifest names, and survives the source tree going away.
#[test]
fn linking_into_a_tree_takes_manifests_and_their_streams_along() {
    let fixture = placement();
    let (source, target) = (fixture.roots[0], fixture.roots[1]);
    let (_, stream) = fixture
        .router
        .retain(source, b"stream bytes\n")
        .expect("stream");
    let manifest = serde_json::json!({
        "streams": [{"name": "stdout", "uri": format!("artifact://sha256/{stream}")}]
    })
    .to_string();
    let (_, manifest_digest) = fixture
        .router
        .retain(source, manifest.as_bytes())
        .expect("manifest");
    fixture
        .router
        .link_into(
            source,
            &tree_dir(fixture.workdir.path(), target).join(super::ARTIFACTS_DIR),
            &[manifest_digest.clone(), "f".repeat(64)]
                .into_iter()
                .collect(),
        )
        .expect("link into the fork's tree");
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        let inode = |root| {
            fs::metadata(tree_blob(fixture.workdir.path(), root, &stream))
                .expect("stream blob")
                .ino()
        };
        assert_eq!(inode(source), inode(target), "one blob, linked twice");
    }
    fs::remove_dir_all(tree_dir(fixture.workdir.path(), source)).expect("drop the source tree");
    let reopened =
        ArtifactRouter::open(fixture.workdir.path().to_path_buf()).expect("reopened router");
    for (digest, content) in [
        (&manifest_digest, manifest.as_str()),
        (&stream, "stream bytes\n"),
    ] {
        assert_eq!(
            reopened
                .read_paged(target, digest, 0, 10)
                .expect("copied content")
                .content,
            content
        );
    }
}
