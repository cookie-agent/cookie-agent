use std::{collections::HashMap, fs, path::Path, sync::Arc, time::Duration};

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

fn write_root_log(workdir: &Path, root: SessionId, digests: &[&str]) -> std::path::PathBuf {
    let directory = tree_dir(workdir, root);
    fs::create_dir_all(&directory).expect("tree directory");
    let lines = digests
        .iter()
        .map(|digest| {
            serde_json::json!({
                "payload": {"result": {"reference": format!("artifact://sha256/{digest}")}}
            })
            .to_string()
        })
        .collect::<Vec<_>>()
        .join("\n");
    let path = directory.join(crate::session::EVENTS_FILE);
    fs::write(&path, format!("{lines}\n")).expect("root log");
    path
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
fn copying_into_a_tree_takes_manifests_and_their_streams_along() {
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
        .copy_into_tree(
            source,
            target,
            [manifest_digest.clone(), "f".repeat(64)]
                .into_iter()
                .collect(),
        )
        .expect("copy into the fork's tree");
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

/// Tree-local sessions D4: a tree is collected against its own logs only. A
/// reference from another tree keeps nothing alive, because a tree that needs
/// content holds its own copy.
#[test]
fn collection_ignores_references_from_other_trees() {
    let fixture = placement();
    let (_, digest) = fixture
        .router
        .retain(fixture.children[0], b"referenced-by-other-tree")
        .expect("retain");
    let blob = tree_blob(fixture.workdir.path(), fixture.roots[0], &digest);
    write_root_log(fixture.workdir.path(), fixture.roots[1], &[&digest]);
    fixture.router.note_tree_loaded(fixture.roots[0]);
    let report = fixture
        .router
        .collect_garbage(Duration::ZERO)
        .expect("sweep");
    assert_eq!(report.deleted, 1);
    assert!(!blob.exists());
}

#[test]
fn collection_only_sweeps_loaded_trees() {
    let fixture = placement();
    let (_, owned) = fixture
        .router
        .retain(fixture.roots[0], b"unreferenced-in-tree")
        .expect("retain");
    let blob = tree_blob(fixture.workdir.path(), fixture.roots[0], &owned);

    let report = fixture
        .router
        .collect_garbage(Duration::ZERO)
        .expect("sweep without loading the tree");
    assert!(blob.is_file(), "an unloaded tree is never collected (§5.2)");
    assert_eq!(report.deleted, 0);

    fixture.router.note_tree_loaded(fixture.roots[0]);
    let report = fixture
        .router
        .collect_garbage(Duration::ZERO)
        .expect("sweep after loading");
    assert_eq!(report.deleted, 1);
    assert!(!blob.exists());
}

/// §5.2, review F4: reusing what the one bulk fold harvested is only sound
/// while the store still agrees with that fold, and the store's proof is
/// resident tip **and** durable length. A child resident in this process can
/// hold its newest record in the log writer's buffer: the file on disk stays
/// byte-identical while the log has moved. A sweep that compared lengths
/// alone would call the harvested set current and delete the blob only that
/// buffered record points at.
#[test]
fn a_resident_unflushed_child_append_skips_its_tree() {
    let fixture = placement();
    let (root, child) = (fixture.roots[0], fixture.children[0]);
    let harvested_digest = "a".repeat(64);
    let child_dir = tree_dir(fixture.workdir.path(), root)
        .join(crate::session::SUBAGENTS_DIR)
        .join(child.to_string());
    fs::create_dir_all(&child_dir).expect("child directory");
    let child_log = child_dir.join(crate::session::EVENTS_FILE);
    fs::write(
        &child_log,
        format!(
            "{}\n",
            serde_json::json!({"payload": {"result": {"reference":
                    format!("artifact://sha256/{harvested_digest}")}}})
        ),
    )
    .expect("child log");
    let durable_len = fs::metadata(&child_log).expect("child log size").len();
    let fingerprint = move |resident_tip| crate::session::LogFingerprint {
        resident_tip,
        durable_len,
    };
    fixture.router.note_tree_loaded(root);
    fixture
        .router
        .install_log_fingerprint_probe(Arc::new(move |_session| fingerprint(Some(2))));
    fixture.router.note_tree_live_refs(
        root,
        [harvested_digest.clone()].into_iter().collect(),
        [(child, fingerprint(None))]
            .into_iter()
            .collect::<std::collections::BTreeMap<_, _>>(),
    );
    // Content only a record sitting in the child's writer buffer points at:
    // nothing on disk mentions its digest, and the child's file does not grow.
    let (buffered_reference, buffered_digest) = fixture
        .router
        .retain(child, b"referenced-only-from-the-buffer")
        .expect("retain");
    assert_eq!(
        buffered_reference.uri,
        format!("artifact://sha256/{buffered_digest}")
    );
    let blob = tree_blob(fixture.workdir.path(), root, &buffered_digest);
    assert!(blob.is_file());
    assert_eq!(
        fs::metadata(&child_log).expect("child log size").len(),
        durable_len,
        "the child's newest append is not in the file the sweep can scan"
    );

    let report = fixture
        .router
        .collect_garbage(Duration::ZERO)
        .expect("sweep a tree with a buffered child append");
    assert_eq!(
        (report.deleted, report.retained),
        (0, 0),
        "the tree's live set is incomplete, so it must not be swept"
    );
    assert!(
        blob.is_file(),
        "an artifact a resident child still references must survive the sweep"
    );

    // Control: with nothing buffered, the same harvested set is current and
    // the very same sweep deletes the unreferenced blob.
    let reopened_log = child_log.clone();
    fixture
        .router
        .install_log_fingerprint_probe(Arc::new(move |_session| crate::session::LogFingerprint {
            resident_tip: None,
            durable_len: fs::metadata(&reopened_log).expect("child log size").len(),
        }));
    let report = fixture
        .router
        .collect_garbage(Duration::ZERO)
        .expect("sweep the quiet tree");
    assert_eq!(report.deleted, 1, "a proven-current harvest does collect");
    assert!(!blob.exists());
}

/// A log line whose artifact URI is JSON-escaped. A legal encoding of
/// `artifact://…` that a naive `contains("artifact://")` prefilter misses,
/// which would drop a live reference out of the sweep entirely.
fn escaped_reference_line(digest: &str) -> String {
    format!(r#"{{"payload":{{"result":{{"reference":"\u0061rtifact://sha256/{digest}"}}}}}}"#)
}

/// Same, with the scheme separator escaped instead of the first letter.
fn escaped_separator_line(digest: &str) -> String {
    format!(r#"{{"payload":{{"result":{{"reference":"artifact\u003a//sha256/{digest}"}}}}}}"#)
}

#[test]
fn an_escaped_artifact_uri_is_scanned_like_a_literal_one() {
    let root = tempfile::tempdir().expect("temporary root");
    let escaped = "a".repeat(64);
    let separator = "b".repeat(64);
    let literal = "c".repeat(64);
    let log = root.path().join(crate::session::EVENTS_FILE);
    fs::write(
        &log,
        format!(
            "{}\n{}\n{}\n{{\"payload\":{{\"text\":\"nothing to see\"}}}}\n",
            escaped_reference_line(&escaped),
            escaped_separator_line(&separator),
            serde_json::json!({
                "payload": {"result": {"reference": format!("artifact://sha256/{literal}")}}
            }),
        ),
    )
    .expect("log");

    let found = super::scan_artifact_references_in_log(&log).expect("scan");
    assert_eq!(
        found,
        [escaped, separator, literal]
            .into_iter()
            .collect::<std::collections::HashSet<_>>(),
        "an escaped URI must reach the live set exactly like a literal one"
    );
}

#[test]
fn collection_retains_a_blob_only_an_escaped_uri_references() {
    let fixture = placement();
    let (_, digest) = fixture
        .router
        .retain(fixture.roots[0], b"referenced-through-an-escape")
        .expect("retain");
    let blob = tree_blob(fixture.workdir.path(), fixture.roots[0], &digest);
    // The tree's own log uses an escape, so the scanner has to decode it
    // before it can recognise the reference.
    fs::write(
        tree_dir(fixture.workdir.path(), fixture.roots[0]).join(crate::session::EVENTS_FILE),
        format!("{}\n", escaped_reference_line(&digest)),
    )
    .expect("escaped log");

    fixture.router.note_tree_loaded(fixture.roots[0]);
    let report = fixture
        .router
        .collect_garbage(Duration::ZERO)
        .expect("sweep");
    assert_eq!(report.deleted, 0);
    assert!(blob.is_file(), "an escaped reference is a live reference");
}

/// §5.2 per tree: a sweep may only delete what it has *proven* unreferenced.
/// If the child logs of a loaded tree cannot be listed, that tree's liveness
/// is unproven and it is left untouched, while every other loaded tree is
/// still collected (tree-local D4).
#[test]
fn a_loaded_tree_that_cannot_be_listed_is_skipped_alone() {
    let fixture = placement();
    let (_, digest) = fixture
        .router
        .retain(fixture.roots[0], b"referenced-only-by-a-child")
        .expect("retain");
    let blob = tree_blob(fixture.workdir.path(), fixture.roots[0], &digest);
    write_root_log(fixture.workdir.path(), fixture.roots[0], &[]);
    let subagents =
        tree_dir(fixture.workdir.path(), fixture.roots[0]).join(crate::session::SUBAGENTS_DIR);
    let child = subagents.join(fixture.children[0].to_string());
    fs::create_dir_all(&child).expect("child directory");
    fs::write(
        child.join(crate::session::EVENTS_FILE),
        format!(
            "{}\n",
            serde_json::json!({
                "payload": {"result": {"reference": format!("artifact://sha256/{digest}")}}
            })
        ),
    )
    .expect("child log");
    fixture.router.note_tree_loaded(fixture.roots[0]);
    let report = fixture
        .router
        .collect_garbage(Duration::ZERO)
        .expect("healthy sweep");
    assert_eq!(report.deleted, 0, "the child log keeps the blob alive");
    assert!(blob.is_file());

    // Another loaded tree with an expired, unreferenced blob.
    let (_, other) = fixture
        .router
        .retain(fixture.roots[1], b"expired and unreferenced")
        .expect("retain in the other tree");
    let other_blob = tree_blob(fixture.workdir.path(), fixture.roots[1], &other);
    fixture.router.note_tree_loaded(fixture.roots[1]);

    // Inject the enumeration failure: listing `subagents/` now errors with
    // something that is not "no such directory".
    fs::remove_dir_all(&subagents).expect("remove subagents");
    fs::write(&subagents, b"not a directory").expect("subagents becomes a file");
    let report = fixture
        .router
        .collect_garbage(Duration::ZERO)
        .expect("one unprovable tree does not fail the sweep");
    assert!(
        blob.is_file(),
        "the sole referencer of the blob was hidden from the sweep"
    );
    assert_eq!(report.deleted, 1, "the other tree was still collected");
    assert!(!other_blob.exists());
}
