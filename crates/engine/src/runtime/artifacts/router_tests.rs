use std::{collections::HashMap, fs, path::Path, sync::Arc, time::Duration};

use cookie_agent_protocol::SessionId;

use super::{ArtifactRouter, SHARED_ARTIFACTS_DIR};

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
        !fixture
            .workdir
            .path()
            .join(SHARED_ARTIFACTS_DIR)
            .join(&first)
            .exists(),
        "a routed write never uses the shared store"
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
fn unrouted_writes_fall_back_to_the_shared_store() {
    let workdir = tempfile::tempdir().expect("workdir");
    let router = ArtifactRouter::open(workdir.path().to_path_buf()).expect("router");
    let (_, digest) = router
        .retain(SessionId::new_v7(), b"orphaned")
        .expect("orphan retain");
    assert!(
        workdir
            .path()
            .join(SHARED_ARTIFACTS_DIR)
            .join(&digest)
            .is_file()
    );
    assert!(!workdir.path().join("sessions").exists());
}

#[test]
fn reads_find_content_another_tree_stored() {
    let fixture = placement();
    let (reference, digest) = fixture
        .router
        .retain(fixture.children[0], b"cross-tree")
        .expect("retain");
    // A reopened router has no write index: the directory-name scan must find it.
    let reopened =
        ArtifactRouter::open(fixture.workdir.path().to_path_buf()).expect("reopened router");
    let page = reopened
        .read_paged(&digest, 0, 10)
        .expect("read content stored by another tree");
    assert_eq!(page.content, "cross-tree");
    assert_eq!(reference.uri, format!("artifact://sha256/{digest}"));
    assert!(
        reopened
            .read_paged(&"f".repeat(64), 0, 1)
            .unwrap_err()
            .to_string()
            .contains("artifact missing")
    );
}

#[test]
fn collection_retains_content_another_tree_references() {
    let fixture = placement();
    let (_, digest) = fixture
        .router
        .retain(fixture.children[0], b"referenced-by-other-tree")
        .expect("retain");
    let blob = tree_blob(fixture.workdir.path(), fixture.roots[0], &digest);
    let log = write_root_log(fixture.workdir.path(), fixture.roots[1], &[&digest]);

    // First sweep records the cross-reference in the ledger.
    fixture.router.note_tree_loaded(fixture.roots[0]);
    let report = fixture
        .router
        .collect_garbage(Duration::ZERO)
        .expect("sweep");
    assert_eq!(report.deleted, 0);
    assert!(blob.is_file());

    // The referencing log disappearing must not retroactively free the blob.
    drop(fixture.router);
    fs::remove_file(log).expect("remove referencing log");
    let reopened =
        ArtifactRouter::open(fixture.workdir.path().to_path_buf()).expect("reopened router");
    reopened.note_tree_loaded(fixture.roots[0]);
    let report = reopened
        .collect_garbage(Duration::ZERO)
        .expect("second sweep");
    assert_eq!(
        report.deleted, 0,
        "the ledger keeps the foreign reference alive"
    );
    assert!(blob.is_file());
}

#[test]
fn collection_only_sweeps_loaded_trees() {
    let fixture = placement();
    let (_, owned) = fixture
        .router
        .retain(fixture.roots[0], b"unreferenced-in-tree")
        .expect("retain");
    let blob = tree_blob(fixture.workdir.path(), fixture.roots[0], &owned);
    fs::write(
        fixture
            .workdir
            .path()
            .join(SHARED_ARTIFACTS_DIR)
            .join("b".repeat(64)),
        b"unreferenced-shared",
    )
    .expect("shared blob");

    let report = fixture
        .router
        .collect_garbage(Duration::ZERO)
        .expect("sweep without loading the tree");
    assert!(blob.is_file(), "an unloaded tree is never collected (§5.2)");
    assert_eq!(report.deleted, 1, "only the shared store was swept");

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
fn a_resident_unflushed_child_append_aborts_the_sweep() {
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
        "the live set is incomplete, so the sweep must not run at all"
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
    // The referencing log belongs to the *other* tree and uses an escape, so
    // the scanner has to decode it before it can recognise the reference.
    let directory = tree_dir(fixture.workdir.path(), fixture.roots[1]);
    fs::create_dir_all(&directory).expect("tree directory");
    fs::write(
        directory.join(crate::session::EVENTS_FILE),
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
    assert!(
        fixture
            .router
            .cross_ref_ledger()
            .contains(&(digest.clone(), fixture.roots[1])),
        "the escaped reference is also recorded as a cross-tree one"
    );
    assert!(
        fs::read_to_string(fixture.workdir.path().join(super::CROSS_REFS_FILE))
            .expect("ledger")
            .contains(&digest),
        "the ledger on disk names the escaped digest"
    );
}

/// §5.2: a sweep may only delete what it has *proven* unreferenced. If the
/// child logs of a loaded tree cannot be listed, liveness is unproven and
/// the whole sweep must abort without unlinking anything.
#[test]
fn collection_aborts_without_deleting_when_a_loaded_tree_cannot_be_listed() {
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

    // A sweep that would delete an expired shared blob if it ran at all.
    let shared_blob = fixture
        .workdir
        .path()
        .join(SHARED_ARTIFACTS_DIR)
        .join("d".repeat(64));
    fs::write(&shared_blob, b"expired and unreferenced").expect("shared blob");

    // Inject the enumeration failure: listing `subagents/` now errors with
    // something that is not "no such directory".
    fs::remove_dir_all(&subagents).expect("remove subagents");
    fs::write(&subagents, b"not a directory").expect("subagents becomes a file");
    let error = fixture
        .router
        .collect_garbage(Duration::ZERO)
        .expect_err("an incomplete live set must abort the sweep");
    assert_ne!(
        error.kind(),
        std::io::ErrorKind::NotFound,
        "only an absent directory may mean \"no children\": {error}"
    );
    assert!(
        blob.is_file(),
        "the sole referencer of the blob was hidden from the sweep"
    );
    assert!(
        shared_blob.is_file(),
        "the sweep deleted despite an unproven live set"
    );
}

/// §5.2: the same rule one level up. If the sessions root cannot be listed,
/// nothing is known to be live, which is not the same as nothing being live.
#[test]
fn collection_aborts_without_deleting_when_the_sessions_root_cannot_be_listed() {
    let fixture = placement();
    let (_, digest) = fixture
        .router
        .retain(fixture.roots[0], b"referenced-by-a-root-log")
        .expect("retain");
    let blob = tree_blob(fixture.workdir.path(), fixture.roots[0], &digest);
    write_root_log(fixture.workdir.path(), fixture.roots[0], &[&digest]);
    let shared_blob = fixture
        .workdir
        .path()
        .join(SHARED_ARTIFACTS_DIR)
        .join("e".repeat(64));
    fs::write(&shared_blob, b"expired and unreferenced").expect("shared blob");

    // A router whose sessions root is a plain file: enumeration fails with
    // `NotADirectory`, and treating that as "no sessions" would free both
    // blobs below.
    let blocked = fixture.workdir.path().join("not-a-sessions-root");
    fs::write(&blocked, b"").expect("blocking file");
    let broken = ArtifactRouter::open_layout(
        fixture.workdir.path().to_path_buf(),
        fixture.workdir.path().join(SHARED_ARTIFACTS_DIR),
        blocked,
    )
    .expect("router over an unlistable sessions root");
    broken.note_tree_loaded(fixture.roots[0]);

    let error = broken
        .collect_garbage(Duration::ZERO)
        .expect_err("an unlistable sessions root must abort the sweep");
    assert_ne!(
        error.kind(),
        std::io::ErrorKind::NotFound,
        "only an absent sessions root may mean \"no sessions\": {error}"
    );
    assert!(
        blob.is_file(),
        "a tree blob was freed on an unknown live set"
    );
    assert!(
        shared_blob.is_file(),
        "the shared store was swept on an unknown live set"
    );

    // Sanity: the same fixture with a readable sessions root does collect
    // the unreferenced shared blob.
    let healthy =
        ArtifactRouter::open(fixture.workdir.path().to_path_buf()).expect("healthy router");
    healthy.note_tree_loaded(fixture.roots[0]);
    let report = healthy.collect_garbage(Duration::ZERO).expect("sweep");
    assert_eq!(report.deleted, 1);
    assert!(blob.is_file());
}

/// B2: a lost ledger append must never leave the durable ledger short of a
/// reference this process already proved — the next sweep rewrites it.
#[test]
fn a_lost_ledger_append_is_rewritten_by_the_next_sweep() {
    let fixture = placement();
    let (_, digest) = fixture
        .router
        .retain(fixture.roots[0], b"foreign-but-for-a-lost-append")
        .expect("retain");
    let blob = tree_blob(fixture.workdir.path(), fixture.roots[0], &digest);
    write_root_log(fixture.workdir.path(), fixture.roots[1], &[&digest]);
    fixture.router.note_tree_loaded(fixture.roots[0]);

    fixture.router.fail_next_ledger_append();
    fixture
        .router
        .collect_garbage(Duration::ZERO)
        .expect("sweep with a lost append");
    let ledger_path = fixture.workdir.path().join(super::CROSS_REFS_FILE);
    assert!(
        !fs::read_to_string(&ledger_path)
            .unwrap_or_default()
            .contains(&digest),
        "the injected failure must actually lose the entry"
    );

    // The next sweep repairs the file wholesale, so a later process cannot
    // see a ledger missing a reference this one proved.
    fixture
        .router
        .collect_garbage(Duration::ZERO)
        .expect("repairing sweep");
    let text = fs::read_to_string(&ledger_path).expect("rebuilt ledger");
    assert!(
        text.contains(&digest),
        "the ledger was not rebuilt after the lost append: {text}"
    );
    assert!(blob.is_file());

    // A reopened router reads the rebuilt file back into its live set.
    drop(fixture.router);
    let reopened =
        ArtifactRouter::open(fixture.workdir.path().to_path_buf()).expect("reopened router");
    reopened.note_tree_loaded(fixture.roots[0]);
    let live = reopened
        .live_references_and_unprovable()
        .expect("live set")
        .0;
    assert!(
        live.contains(&digest),
        "the durable ledger no longer carries the reference back"
    );
    assert!(blob.is_file());
}
