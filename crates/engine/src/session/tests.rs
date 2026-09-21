use std::{
    collections::BTreeMap,
    fs,
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc,
    },
    thread,
};

#[cfg(unix)]
use std::{
    ffi::OsString,
    os::unix::{
        ffi::{OsStrExt, OsStringExt},
        fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt, symlink},
    },
};

use cookie_agent_config::{ModelPricing, PicoUsdPerMillion, PricingConfig};
use cookie_agent_protocol::{
    AgentId, AgentMode, AgentRevision, AttemptId, CatalogRevision, ClientRunId, EventPayload,
    InternalAgentBackend, InternalAgentFailure, InternalAgentInvocationId, InternalAgentKind,
    InternalAgentRunId, InvocationId, ModelFinishReason, ModelRevision, PersistedModelTurn,
    ProviderStateRevision, RecipeRegistryRevision, RunId, RuntimeRevision, SafeCode,
    SafeDisplayText, SafeErrorMessage, SafeInternalAgentCall, SafeInternalAgentResult, SessionId,
    SessionOrigin, SessionPermissionOverlay, SessionTitle, SessionTitleChange, Sha256Digest,
    ToolCallId, Usage,
};

use crate::ownership::owner_lock_path;

use super::{
    EVENTS_FILE, IndexedChild, SESSION_META_FILE, SESSIONS_ROOT_DIR, SUBAGENT_INDEX_FILE,
    SUBAGENT_INDEX_VERSION, SUBAGENTS_DIR, SessionError, SessionStore, SessionSummary,
    SubagentIndex, TREE_LOAD_RACES, TreeLoadObserver, TreeLoadProducts, TreeLoadStatus, meta_path,
    projection,
};

#[cfg(unix)]
use super::{LAYOUT_MARKER_FILE, WORKDIR_CWD_FILE};

/// The v2 work-dir store for `cwd` (what a freshly opened store creates).
fn workdir_dir(data_root: &Path, cwd: &Path) -> std::path::PathBuf {
    data_root
        .join(SESSIONS_ROOT_DIR)
        .join(SessionStore::workdir_key(cwd))
}

#[cfg(unix)]
fn cwd_file(data_root: &Path, cwd: &Path) -> std::path::PathBuf {
    workdir_dir(data_root, cwd).join(WORKDIR_CWD_FILE)
}

fn private_tempdir() -> tempfile::TempDir {
    let directory = tempfile::tempdir().expect("temporary root");
    #[cfg(unix)]
    {
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700))
            .expect("private temporary root");
    }
    #[cfg(windows)]
    {
        fs::remove_dir(directory.path()).expect("remove ordinary temp directory");
        cookie_agent_models::secure_store::SecureDirectory::open(directory.path())
            .expect("private temporary root");
    }
    directory
}

fn create_private_test_dir_all(path: &Path) {
    #[cfg(unix)]
    {
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true).mode(0o700);
        builder.create(path).expect("private test directory");
    }
    #[cfg(windows)]
    cookie_agent_models::secure_store::create_windows_private_dir_all(path)
        .expect("private test directory");
}

fn write_private_test_file(path: &Path, contents: impl AsRef<[u8]>) {
    #[cfg(unix)]
    {
        use std::io::Write as _;

        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .expect("private test file");
        file.write_all(contents.as_ref())
            .expect("write private test file");
    }
    #[cfg(windows)]
    {
        use std::io::Write as _;

        let mut file = cookie_agent_models::secure_store::create_windows_private_file(path)
            .expect("private test file");
        file.write_all(contents.as_ref())
            .expect("write private test file");
    }
}

fn persist_test_session(store: &SessionStore) -> SessionId {
    persist_test_session_with_origin(store, SessionOrigin::Root)
}

/// Creates and durably publishes a session carrying `origin`. Delegated
/// origins land under the root's `subagents/` directory (§2.2).
fn persist_test_session_with_origin(store: &SessionStore, origin: SessionOrigin) -> SessionId {
    let session_id = SessionId::new_v7();
    let agent = crate::test_support::agent_snapshot("test", AgentMode::Primary);
    let selection = crate::test_support::run_selection("test");
    let binding = agent.fallback_chain[0].clone();
    let revision = |label: char| format!("sha256:{}", label.to_string().repeat(64));
    let runtime_revision = RuntimeRevision::new(revision('1')).unwrap();
    let catalog_revision = CatalogRevision::new(revision('2')).unwrap();
    let provider_state_revision = ProviderStateRevision::new(revision('3')).unwrap();
    let model_revision = ModelRevision::new(revision('4')).unwrap();
    let agent_revision = AgentRevision::new(revision('5')).unwrap();
    let recipe_registry_revision = RecipeRegistryRevision::new(revision('6')).unwrap();
    store
        .create(
            session_id,
            cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
            EventPayload::SessionCreated {
                short_id: None,
                origin,
                cwd_identity: cookie_agent_protocol::CwdIdentity::new("workspace:test").unwrap(),
                creation_selection: selection.clone(),
                creation_agent: Box::new(agent.clone()),
                runtime_revision: runtime_revision.clone(),
                catalog_revision: catalog_revision.clone(),
                provider_state_revision: provider_state_revision.clone(),
                model_revision: model_revision.clone(),
                agent_revision: agent_revision.clone(),
                recipe_registry_revision: recipe_registry_revision.clone(),
                manifest_revision: binding.manifest_revision.clone(),
            },
        )
        .unwrap();
    let run_id = RunId::new_v7();
    store
        .append(
            session_id,
            Some(run_id),
            cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
            EventPayload::RunStarted {
                client_run_id: ClientRunId::new("private-session-test").unwrap(),
                selection,
                agent: Box::new(agent),
                runtime_revision,
                catalog_revision,
                provider_state_revision,
                model_revision,
                agent_revision,
                recipe_registry_revision,
                manifest_revision: binding.manifest_revision.clone(),
                selected_suffix: vec![binding],
                internal_agents: Vec::new(),
                input_through_seq: 1,
            },
        )
        .unwrap();
    store
        .append(
            session_id,
            Some(run_id),
            cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
            EventPayload::UserInputSubmitted {
                input: "persist me".into(),
            },
        )
        .unwrap();
    session_id
}

fn create_buffered_test_session(store: &SessionStore) -> SessionId {
    let session_id = SessionId::new_v7();
    let agent = crate::test_support::agent_snapshot("test", AgentMode::Primary);
    let selection = crate::test_support::run_selection("test");
    let binding = agent.fallback_chain[0].clone();
    let revision = |label: char| format!("sha256:{}", label.to_string().repeat(64));
    store
        .create(
            session_id,
            cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
            EventPayload::SessionCreated {
                short_id: None,
                origin: SessionOrigin::Root,
                cwd_identity: cookie_agent_protocol::CwdIdentity::new("workspace:test").unwrap(),
                creation_selection: selection,
                creation_agent: Box::new(agent),
                runtime_revision: RuntimeRevision::new(revision('1')).unwrap(),
                catalog_revision: CatalogRevision::new(revision('2')).unwrap(),
                provider_state_revision: ProviderStateRevision::new(revision('3')).unwrap(),
                model_revision: ModelRevision::new(revision('4')).unwrap(),
                agent_revision: AgentRevision::new(revision('5')).unwrap(),
                recipe_registry_revision: RecipeRegistryRevision::new(revision('6')).unwrap(),
                manifest_revision: binding.manifest_revision,
            },
        )
        .expect("create buffered session");
    session_id
}

#[test]
fn ownership_is_acquired_on_write_open_and_released_with_the_store() {
    let temporary = private_tempdir();
    let cwd = temporary.path().join("workspace");
    create_private_test_dir_all(&cwd);
    let data = temporary.path().join("data");
    let owner = SessionStore::open(&data, &cwd).expect("owner store");
    let session_id = persist_test_session(&owner);
    assert!(owner_lock_path(&owner.session_dir(session_id)).is_file());
    let stale_log = owner.get(session_id).expect("owned projection").log;
    let (authorized, release_append) = stale_log.install_append_authorization_hook_for_test();
    let appending = thread::spawn(move || {
        stale_log.append(
            None,
            cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
            EventPayload::SessionReverted { through_seq: 1 },
        )
    });
    authorized
        .recv()
        .expect("append passed initial authorization");

    assert_eq!(Arc::strong_count(&owner), 1);
    drop(owner);
    let observer = SessionStore::open(&data, &cwd).expect("observer store");
    observer
        .open_for_write(session_id)
        .expect("adopt after owner drops");
    assert!(observer.is_owned(session_id));
    release_append.send(()).expect("release stale append");
    assert!(matches!(
        appending.join().expect("stale append thread"),
        Err(crate::events::EventLogError::ReadOnly(_))
    ));
}

#[test]
fn ownership_release_does_not_wait_for_store_drop() {
    let temporary = private_tempdir();
    let cwd = temporary.path().join("workspace");
    create_private_test_dir_all(&cwd);
    let data = temporary.path().join("data");
    let owner = SessionStore::open(&data, &cwd).expect("owner store");
    let session_id = persist_test_session(&owner);
    let stale_log = owner.get(session_id).expect("owned projection").log;

    owner.release_ownership();
    owner.release_ownership();

    assert!(!owner.is_owned(session_id));
    assert!(matches!(
        owner.append(
            session_id,
            None,
            cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
            EventPayload::SessionReverted { through_seq: 1 },
        ),
        Err(SessionError::StoreClosed)
    ));
    assert!(matches!(
        stale_log.append(
            None,
            cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
            EventPayload::SessionReverted { through_seq: 1 },
        ),
        Err(crate::events::EventLogError::ReadOnly(_))
    ));
    let observer = SessionStore::open(&data, &cwd).expect("observer store");
    observer
        .open_for_write(session_id)
        .expect("adopt after explicit ownership release");
}

#[test]
fn ownership_release_waits_for_an_append_that_already_won_serialization() {
    let temporary = private_tempdir();
    let cwd = temporary.path().join("workspace");
    create_private_test_dir_all(&cwd);
    let data = temporary.path().join("data");
    let owner = SessionStore::open(&data, &cwd).expect("owner store");
    let session_id = persist_test_session(&owner);
    let log = owner.get(session_id).expect("owned projection").log;
    let (authorized, release_append) = log.install_append_authorization_hook_for_test();
    let append_store = Arc::clone(&owner);
    let appending = thread::spawn(move || {
        append_store.append(
            session_id,
            None,
            cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
            EventPayload::SessionReverted { through_seq: 1 },
        )
    });
    authorized.recv().expect("append passed authorization");

    let release_store = Arc::clone(&owner);
    let (released, release_observed) = mpsc::channel();
    let releasing = thread::spawn(move || {
        release_store.release_ownership();
        released.send(()).expect("report ownership release");
    });
    assert!(matches!(
        release_observed.recv_timeout(std::time::Duration::from_millis(50)),
        Err(mpsc::RecvTimeoutError::Timeout)
    ));
    let observer = SessionStore::open(&data, &cwd).expect("observer store");
    assert!(matches!(
        observer.open_for_write(session_id),
        Err(SessionError::SessionLocked(id)) if id == session_id
    ));

    release_append.send(()).expect("release append");
    appending
        .join()
        .expect("append thread")
        .expect("append wins");
    release_observed
        .recv_timeout(std::time::Duration::from_secs(1))
        .expect("ownership releases after append");
    releasing.join().expect("release thread");
    observer
        .open_for_write(session_id)
        .expect("adopt after append and release");
}

#[test]
fn eviction_retains_ownership_and_reopens_for_the_owner() {
    let temporary = private_tempdir();
    let cwd = temporary.path().join("workspace");
    create_private_test_dir_all(&cwd);
    let data = temporary.path().join("data");
    let owner = SessionStore::open(&data, &cwd).expect("owner store");
    let session_id = persist_test_session(&owner);
    assert!(owner.evict(session_id).expect("evict owned session"));
    assert!(!owner.is_resident(session_id));

    let observer = SessionStore::open(&data, &cwd).expect("observer store");
    let snapshot = observer.get(session_id).expect("read-only snapshot");
    assert!(matches!(
        snapshot.log.append(
            None,
            cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
            EventPayload::SessionReverted { through_seq: 1 },
        ),
        Err(crate::events::EventLogError::ReadOnly(_))
    ));
    assert!(matches!(
        observer.open_for_write(session_id),
        Err(SessionError::SessionLocked(id)) if id == session_id
    ));
    owner
        .open_for_write(session_id)
        .expect("owner reopens after eviction");
}

#[test]
fn foreign_snapshot_preserves_torn_tail_until_owned_adoption() {
    let temporary = private_tempdir();
    let cwd = temporary.path().join("workspace");
    create_private_test_dir_all(&cwd);
    let data = temporary.path().join("data");
    let owner = SessionStore::open(&data, &cwd).expect("owner store");
    let session_id = persist_test_session(&owner);
    let event_path = owner.session_dir(session_id).join("events.jsonl");
    let mut bytes = fs::read(&event_path).expect("read event log");
    bytes.extend_from_slice(b"{\"torn\"");
    fs::write(&event_path, &bytes).expect("write torn tail");

    let observer = SessionStore::open(&data, &cwd).expect("observer store");
    observer.get(session_id).expect("read foreign snapshot");
    assert_eq!(fs::read(&event_path).expect("tail remains"), bytes);
    assert!(matches!(
        observer.open_for_write(session_id),
        Err(SessionError::SessionLocked(id)) if id == session_id
    ));

    drop(owner);
    observer.open_for_write(session_id).expect("adopt torn log");
    assert_ne!(fs::read(&event_path).expect("tail truncated"), bytes);
    assert!(
        fs::read(&event_path)
            .expect("read repaired log")
            .ends_with(b"\n")
    );
}

#[test]
fn failed_adoption_is_unobservable_and_retryable() {
    let temporary = private_tempdir();
    let cwd = temporary.path().join("workspace");
    create_private_test_dir_all(&cwd);
    let data = temporary.path().join("data");
    let owner = SessionStore::open(&data, &cwd).expect("owner store");
    let session_id = persist_test_session(&owner);
    assert_eq!(Arc::strong_count(&owner), 1);
    drop(owner);

    let first = SessionStore::open(&data, &cwd).expect("first adopter");
    assert_eq!(
        first.begin_write(session_id).unwrap(),
        super::WriteOpen::Adopting
    );
    assert!(!first.is_owned(session_id));
    assert!(matches!(
        first.append(
            session_id,
            None,
            cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
            EventPayload::SessionReverted { through_seq: 1 },
        ),
        Err(SessionError::SessionLocked(id)) if id == session_id
    ));
    first.rollback_adoption(session_id);

    let second = SessionStore::open(&data, &cwd).expect("second adopter");
    assert_eq!(
        second.begin_write(session_id).unwrap(),
        super::WriteOpen::Adopting
    );
    second.commit_adoption(session_id).expect("commit retry");
    assert!(second.is_owned(session_id));
}

#[test]
fn concurrent_adoption_has_one_winner() {
    let temporary = private_tempdir();
    let cwd = temporary.path().join("workspace");
    create_private_test_dir_all(&cwd);
    let data = temporary.path().join("data");
    let owner = SessionStore::open(&data, &cwd).expect("owner store");
    let session_id = persist_test_session(&owner);
    drop(owner);
    let stores = [
        SessionStore::open(&data, &cwd).expect("first contender"),
        SessionStore::open(&data, &cwd).expect("second contender"),
    ];
    let barrier = Arc::new(std::sync::Barrier::new(3));
    let threads = stores.clone().map(|store| {
        let barrier = Arc::clone(&barrier);
        thread::spawn(move || {
            barrier.wait();
            match store.begin_write(session_id) {
                Ok(super::WriteOpen::Adopting) => {
                    store.commit_adoption(session_id).expect("commit winner");
                    true
                }
                Err(SessionError::SessionLocked(id)) if id == session_id => false,
                result => panic!("unexpected adoption result: {result:?}"),
            }
        })
    });
    barrier.wait();
    let winners = threads
        .into_iter()
        .map(|thread| usize::from(thread.join().expect("adoption contender")))
        .sum::<usize>();
    assert_eq!(winners, 1);
}

/// Every ownership-lock artifact under `directory`, in *either* platform
/// layout. Both are searched on both platforms on purpose: the point of the
/// tree-scoped lock is that a child leaves neither behind.
fn owner_lock_artifacts(directory: &Path) -> Vec<std::path::PathBuf> {
    fn walk(directory: &Path, found: &mut Vec<std::path::PathBuf>) {
        let Ok(entries) = fs::read_dir(directory) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, found);
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if name == "owner.lock" || name.ends_with(".owner.lock") {
                found.push(path);
            }
        }
    }
    let mut found = Vec::new();
    walk(directory, &mut found);
    found.sort();
    found
}

/// Creates a root with a child and a grandchild, all published.
fn persist_test_tree(store: &SessionStore) -> (SessionId, SessionId, SessionId) {
    let root = persist_test_session(store);
    let child = persist_test_session_with_origin(store, delegated_origin(root, root, 1));
    let grandchild = persist_test_session_with_origin(store, delegated_origin(root, child, 2));
    (root, child, grandchild)
}

/// (a) Locks are per tree. Publishing children under an owned root adds no
/// lock anywhere below it — no `owner.lock` and no `<id>.owner.lock` — and the
/// whole work dir still holds exactly the root's one lock.
#[test]
fn publishing_a_child_writes_no_lock_under_the_root() {
    let temporary = private_tempdir();
    let cwd = temporary.path().join("workspace");
    create_private_test_dir_all(&cwd);
    let data = temporary.path().join("data");
    let store = SessionStore::open(&data, &cwd).expect("owner store");
    let (root, child, grandchild) = persist_test_tree(&store);

    let root_lock = owner_lock_path(&store.session_dir(root));
    assert!(root_lock.is_file(), "the tree keeps the root's lock");
    assert!(
        owner_lock_artifacts(
            &store
                .workdir_dir_path()
                .join(root.to_string())
                .join(SUBAGENTS_DIR)
        )
        .is_empty(),
        "no lock of any layout is filed under subagents/"
    );
    assert_eq!(
        owner_lock_artifacts(store.workdir_dir_path()),
        vec![root_lock],
        "the tree is guarded by exactly one lock"
    );
    assert!(store.is_owned(child) && store.is_owned(grandchild));
}

/// (b) A foreign root locks its whole tree: another store may neither write a
/// child of it nor adopt the root, though reading stays legal.
#[test]
fn a_foreign_root_refuses_every_session_in_its_tree() {
    let temporary = private_tempdir();
    let cwd = temporary.path().join("workspace");
    create_private_test_dir_all(&cwd);
    let data = temporary.path().join("data");
    let owner = SessionStore::open(&data, &cwd).expect("owner store");
    let (root, child, grandchild) = persist_test_tree(&owner);

    let observer = SessionStore::open(&data, &cwd).expect("observer store");
    observer
        .get(child)
        .expect("reading a foreign child is legal");
    for id in [child, grandchild, root] {
        assert!(
            matches!(observer.open_for_write(id), Err(SessionError::SessionLocked(locked)) if locked == id),
            "session {id} of a foreign tree must not be writable"
        );
        assert!(!observer.is_owned(id));
    }
}

/// (c) Adopting a child takes the *root's* lock, once, for the whole tree.
/// That does not make the root writable — it reconciles through its own
/// adoption — and that adoption takes no second lock.
#[test]
fn adopting_a_child_takes_the_root_lock_for_the_whole_tree() {
    let temporary = private_tempdir();
    let cwd = temporary.path().join("workspace");
    create_private_test_dir_all(&cwd);
    let data = temporary.path().join("data");
    let owner = SessionStore::open(&data, &cwd).expect("owner store");
    let (root, child, grandchild) = persist_test_tree(&owner);
    assert_eq!(Arc::strong_count(&owner), 1);
    drop(owner);

    let adopter = SessionStore::open(&data, &cwd).expect("adopter store");
    adopter.open_for_write(child).expect("adopt the child");
    let root_lock = owner_lock_path(&adopter.session_dir(root));
    assert_eq!(
        owner_lock_artifacts(adopter.workdir_dir_path()),
        vec![root_lock.clone()],
        "adopting through a child takes the root's lock and nothing else"
    );
    assert!(adopter.is_owned(child));
    assert!(
        !adopter.is_owned(root),
        "holding the tree is not adopting the root"
    );

    adopter
        .open_for_write(root)
        .expect("adopt the root of a tree already held");
    assert!(adopter.is_owned(root));
    adopter
        .open_for_write(grandchild)
        .expect("adopt a sibling branch of the same tree");
    assert_eq!(
        owner_lock_artifacts(adopter.workdir_dir_path()),
        vec![root_lock],
        "no adoption in a held tree takes a second lock"
    );
}

/// (d) Dropping the owning store releases the whole tree, so another store can
/// adopt any session in it.
#[test]
fn dropping_the_owning_store_frees_the_whole_tree() {
    let temporary = private_tempdir();
    let cwd = temporary.path().join("workspace");
    create_private_test_dir_all(&cwd);
    let data = temporary.path().join("data");
    let owner = SessionStore::open(&data, &cwd).expect("owner store");
    let (root, child, _) = persist_test_tree(&owner);

    let observer = SessionStore::open(&data, &cwd).expect("observer store");
    assert!(matches!(
        observer.open_for_write(child),
        Err(SessionError::SessionLocked(id)) if id == child
    ));

    assert_eq!(Arc::strong_count(&owner), 1);
    drop(owner);

    observer
        .open_for_write(child)
        .expect("adopt the child of a released tree");
    observer
        .open_for_write(root)
        .expect("adopt the root of a released tree");
    assert!(observer.is_owned(child) && observer.is_owned(root));
}

/// (e) Per-child locks an older build wrote are never consulted, and the tree's
/// load pass removes them.
#[test]
fn legacy_child_lock_files_are_swept_and_never_consulted() {
    let temporary = private_tempdir();
    let cwd = temporary.path().join("workspace");
    create_private_test_dir_all(&cwd);
    let data = temporary.path().join("data");
    let owner = SessionStore::open(&data, &cwd).expect("owner store");
    let (root, child, _) = persist_test_tree(&owner);
    let subagents = owner
        .workdir_dir_path()
        .join(root.to_string())
        .join(SUBAGENTS_DIR);
    assert_eq!(Arc::strong_count(&owner), 1);
    drop(owner);

    // Both layouts an older build could have left behind, planted together.
    let legacy_inside = subagents.join(child.to_string()).join("owner.lock");
    let legacy_sidecar = subagents.join(format!("{child}.owner.lock"));
    write_private_test_file(&legacy_inside, []);
    write_private_test_file(&legacy_sidecar, []);

    let store = SessionStore::open(&data, &cwd).expect("cold store");
    store.load_tree(root).expect("complete the tree");

    assert!(!legacy_inside.exists(), "the load swept the legacy lock");
    assert!(
        !legacy_sidecar.exists(),
        "the load swept the legacy sidecar"
    );
    store
        .open_for_write(child)
        .expect("a legacy child lock never gated adoption");
    assert_eq!(
        owner_lock_artifacts(store.workdir_dir_path()),
        vec![owner_lock_path(&store.session_dir(root))],
        "only the root's lock survives"
    );
}

#[test]
fn buffered_publish_is_locked_before_the_directory_becomes_visible() {
    let temporary = private_tempdir();
    let cwd = temporary.path().join("workspace");
    create_private_test_dir_all(&cwd);
    let data = temporary.path().join("data");
    let creator = SessionStore::open(&data, &cwd).expect("creator store");
    let session_id = create_buffered_test_session(&creator);
    let (reached, release) = creator.install_publish_hook_for_test();
    let publishing = {
        let creator = Arc::clone(&creator);
        thread::spawn(move || creator.persist_buffered_session(session_id))
    };
    assert_eq!(
        reached.recv().expect("publisher acquired ownership lock"),
        session_id
    );

    let observer = SessionStore::open(&data, &cwd).expect("observer store");
    assert!(matches!(observer.get(session_id), Err(SessionError::Missing(id)) if id == session_id));
    release.send(()).expect("release publisher");
    publishing
        .join()
        .expect("publisher thread")
        .expect("publish session");
    assert!(matches!(
        observer.open_for_write(session_id),
        Err(SessionError::SessionLocked(id)) if id == session_id
    ));
}

#[test]
fn fork_publish_is_locked_before_the_directory_becomes_visible() {
    let temporary = private_tempdir();
    let cwd = temporary.path().join("workspace");
    create_private_test_dir_all(&cwd);
    let data = temporary.path().join("data");
    let creator = SessionStore::open(&data, &cwd).expect("creator store");
    let source_id = persist_test_session(&creator);
    let through_seq = creator
        .get(source_id)
        .expect("source projection")
        .log
        .events()
        .into_iter()
        .find(|event| matches!(event.payload, EventPayload::UserInputSubmitted { .. }))
        .expect("source user input")
        .seq;
    let (reached, release) = creator.install_publish_hook_for_test();
    let publishing = {
        let creator = Arc::clone(&creator);
        thread::spawn(move || {
            creator.fork(
                source_id,
                through_seq,
                cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
            )
        })
    };
    let fork_id = reached.recv().expect("fork acquired ownership lock");

    let observer = SessionStore::open(&data, &cwd).expect("observer store");
    assert!(matches!(observer.get(fork_id), Err(SessionError::Missing(id)) if id == fork_id));
    release.send(()).expect("release fork publisher");
    assert_eq!(
        publishing
            .join()
            .expect("fork publisher thread")
            .expect("publish fork"),
        fork_id
    );
    assert!(matches!(
        observer.open_for_write(fork_id),
        Err(SessionError::SessionLocked(id)) if id == fork_id
    ));
}

#[test]
fn metadata_cache_reads_never_observe_partial_replacements() {
    let temporary = private_tempdir();
    let cwd = temporary.path().join("workspace");
    create_private_test_dir_all(&cwd);
    let data = temporary.path().join("data");
    let store = SessionStore::open(&data, &cwd).expect("session store");
    let session_id = persist_test_session(&store);
    let session_dir = store.session_dir(session_id);
    let cache_path = meta_path(&session_dir);
    let event_path = session_dir.join("events.jsonl");
    let meta = store.get(session_id).expect("projection").meta;
    // The replacement window is short, so a low iteration count hides a
    // non-atomic replace. Windows CI reproduced the spurious `NotFound` within a
    // few hundred rewrites; 2,000 keeps the reader inside the window long enough
    // to fail loudly rather than flake.
    const REPLACEMENTS: usize = 2_000;
    let writer = thread::spawn({
        let cache_path = cache_path.clone();
        let meta = meta.clone();
        move || {
            for _ in 0..REPLACEMENTS {
                super::write_cache(&cache_path, &meta).expect("replace metadata cache");
            }
        }
    });
    for _ in 0..REPLACEMENTS {
        match super::read_cache(&cache_path, &event_path) {
            Ok(read) => assert_eq!(read.session_id, session_id),
            Err(SessionError::Io { path, source })
                if source.kind() == std::io::ErrorKind::NotFound =>
            {
                panic!(
                    "metadata cache at {} vanished mid-replacement: {source}",
                    path.display()
                )
            }
            Err(error) => panic!("read of a replaced metadata cache failed: {error}"),
        }
    }
    writer.join().expect("metadata writer");
}

/// A root directory without a `metadata` cache is a transient discovery state,
/// not a fault: `<root>/subagents/` exists on its own whenever a child publishes
/// before its root, and the same shape appears mid-merge in
/// `publish_prepared_dir`. Listing must skip it and pick the root up once its
/// metadata lands.
#[test]
fn bare_root_scaffolds_are_skipped_until_their_metadata_exists() {
    let temporary = private_tempdir();
    let cwd = temporary.path().join("workspace");
    create_private_test_dir_all(&cwd);
    let data = temporary.path().join("data");
    let owner = SessionStore::open(&data, &cwd).expect("owner store");
    let session_id = persist_test_session(&owner);
    let session_dir = owner.session_dir(session_id);
    drop(owner);

    // A root whose only content is the child scaffold, never published further.
    let orphan_id = SessionId::new_v7();
    let orphan_dir = workdir_dir(&data, &cwd).join(orphan_id.to_string());
    create_private_test_dir_all(&orphan_dir.join(SUBAGENTS_DIR));

    // The published root, reduced to the same scaffold shape.
    let cache_path = meta_path(&session_dir);
    let withheld = fs::read(&cache_path).expect("published metadata cache");
    fs::remove_file(&cache_path).expect("withhold the metadata cache");
    create_private_test_dir_all(&session_dir.join(SUBAGENTS_DIR));

    let scaffolded = SessionStore::open(&data, &cwd).expect("store opens over bare scaffolds");
    let listed = scaffolded
        .all_summaries()
        .into_iter()
        .map(|summary| summary.meta.session_id)
        .collect::<Vec<_>>();
    assert!(
        !listed.contains(&session_id) && !listed.contains(&orphan_id),
        "bare scaffolds must not be listed: {listed:?}"
    );
    assert!(
        scaffolded.get(orphan_id).is_err(),
        "a scaffold without an event log is not a loadable session"
    );
    drop(scaffolded);

    write_private_test_file(&cache_path, &withheld);
    let restored = SessionStore::open(&data, &cwd).expect("store reopens");
    assert!(
        restored
            .all_summaries()
            .into_iter()
            .any(|summary| summary.meta.session_id == session_id),
        "a root is discovered once its metadata cache exists"
    );
}

#[test]
fn discovery_does_not_reread_known_evicted_metadata() {
    let temporary = private_tempdir();
    let cwd = temporary.path().join("workspace");
    create_private_test_dir_all(&cwd);
    let data = temporary.path().join("data");
    let owner = SessionStore::open(&data, &cwd).expect("owner store");
    let session_id = persist_test_session(&owner);
    drop(owner);

    let observer = SessionStore::open(&data, &cwd).expect("observer store");
    let cached = observer.summary(session_id).expect("cached summary");
    assert!(!observer.is_resident(session_id));
    let mut replacement = cached.meta.clone();
    replacement.title = Some(
        cookie_agent_protocol::SessionTitle::new("changed on disk").expect("replacement title"),
    );
    super::write_cache(
        &observer
            .meta_cache_path(session_id)
            .expect("metadata cache path"),
        &replacement,
    )
    .expect("replace metadata cache");

    let rediscovered = observer
        .all_summaries()
        .into_iter()
        .find(|summary| summary.meta.session_id == session_id)
        .expect("rediscovered summary");
    assert_eq!(rediscovered.meta.title, cached.meta.title);
}

/// Builds a delegated origin filed under `root`, nested below `parent`.
fn delegated_origin(root: SessionId, parent: SessionId, depth: u32) -> SessionOrigin {
    SessionOrigin::Delegated {
        root_session_id: root,
        parent_session_id: parent,
        parent_run_id: RunId::new_v7(),
        parent_tool_call_id: ToolCallId::new_v7(),
        invocation_id: InvocationId::new_v7(),
        depth,
    }
}

fn test_user_input_seq(store: &SessionStore, id: SessionId) -> u64 {
    store
        .get(id)
        .expect("session")
        .log
        .events()
        .into_iter()
        .find(|event| matches!(event.payload, EventPayload::UserInputSubmitted { .. }))
        .expect("user input event")
        .seq
}

/// §8.2 #4: startup discovery is root-only. With every child event log made
/// unreadable, a reopened store still lists the whole tree because it reads
/// root `metadata` caches plus each root's `subagents/index.json`.
#[cfg(unix)]
#[test]
fn startup_discovery_never_reads_child_logs() {
    use std::os::unix::fs::PermissionsExt;

    let temporary = private_tempdir();
    let cwd = temporary.path().join("workspace");
    create_private_test_dir_all(&cwd);
    let data = temporary.path().join("data");
    let store = SessionStore::open(&data, &cwd).expect("owner store");
    let root = persist_test_session(&store);
    let child = persist_test_session_with_origin(&store, delegated_origin(root, root, 1));
    let grandchild = persist_test_session_with_origin(&store, delegated_origin(root, child, 2));

    // Both descendants sit one level under the root, whatever their depth.
    let root_dir = store.session_dir(root);
    let child_dir = store.session_dir(child);
    let grandchild_dir = store.session_dir(grandchild);
    assert_eq!(
        child_dir,
        root_dir.join(SUBAGENTS_DIR).join(child.to_string())
    );
    assert_eq!(
        grandchild_dir,
        root_dir.join(SUBAGENTS_DIR).join(grandchild.to_string())
    );
    let expected = store.all_summaries();
    assert_eq!(expected.len(), 3);
    drop(store);

    for directory in [&child_dir, &grandchild_dir] {
        fs::set_permissions(
            directory.join("events.jsonl"),
            fs::Permissions::from_mode(0o000),
        )
        .expect("unreadable child log");
    }

    let observer = SessionStore::open(&data, &cwd).expect("cold observer store");
    // (a) Opening the store, and every startup bookkeeping pass it runs, reads
    // no child event log at all: the counts are of `events.jsonl` opens, not
    // of load attempts, so nothing can hide behind a cached flag.
    for child in [child, grandchild] {
        assert_eq!(observer.log_open_count(child), 0, "startup read {child}");
    }
    assert_eq!(observer.root_snapshots().len(), 1, "only the root log");
    assert_eq!(observer.all_summaries().len(), 3, "summaries from caches");
    for child in [child, grandchild] {
        assert_eq!(
            observer.log_open_count(child),
            0,
            "startup passes read no child log"
        );
    }
    let discovered = observer.all_summaries();
    assert_eq!(
        discovered.len(),
        3,
        "index.json pre-populates child summaries"
    );
    for summary in &expected {
        let found = discovered
            .iter()
            .find(|found| found.meta.session_id == summary.meta.session_id)
            .expect("discovered summary");
        assert_eq!(found.meta, summary.meta);
    }
    assert!(!observer.is_resident(child));
    // Listing *is* a tree use, so it triggers the load and reports the
    // failure instead of serving the pre-load cache (review L14).
    assert!(observer.children(root).is_err());
    assert!(observer.children(child).is_err());
    assert!(
        !observer.is_tree_loaded(root),
        "an unreadable child must not report a loaded tree"
    );
    assert!(
        observer.log_open_count(child) > 0,
        "a listing that cannot complete the tree attempts the child log"
    );
    assert_eq!(
        observer.root_snapshots().len(),
        1,
        "the root log is read by the startup pass only"
    );

    // A child log is only needed where the tree is actually assembled, and
    // there an unreadable child fails closed (§3.2.2).
    assert!(observer.tree(root).is_err());
    for directory in [&child_dir, &grandchild_dir] {
        fs::set_permissions(
            directory.join("events.jsonl"),
            fs::Permissions::from_mode(0o600),
        )
        .expect("readable child log");
    }
    let tree = observer.tree(root).expect("tree after a lazy load");
    assert_eq!(
        observer.log_open_count(grandchild),
        1,
        "the first completed fold reads each child exactly once"
    );
    assert_eq!(tree.session.session_id, root);
    assert_eq!(tree.children.len(), 1);
    assert_eq!(tree.children[0].session.session_id, child);
    assert_eq!(tree.children[0].children[0].session.session_id, grandchild);
}

/// §8.2 #5: opening a root reads each child log exactly once, leaves the
/// children evicted, and never re-reads them for later queries.
#[cfg(unix)]
#[test]
fn tree_load_reads_each_child_once_and_leaves_them_evicted() {
    use std::os::unix::fs::PermissionsExt;

    let temporary = private_tempdir();
    let cwd = temporary.path().join("workspace");
    create_private_test_dir_all(&cwd);
    let data = temporary.path().join("data");
    let owner = SessionStore::open(&data, &cwd).expect("owner store");
    let root = persist_test_session(&owner);
    let first = persist_test_session_with_origin(&owner, delegated_origin(root, root, 1));
    let second = persist_test_session_with_origin(&owner, delegated_origin(root, root, 1));
    drop(owner);

    let store = SessionStore::open(&data, &cwd).expect("cold store");
    assert!(
        !store.is_tree_loaded(root),
        "a cold store has no loaded trees"
    );
    for child in [first, second] {
        assert_eq!(store.log_open_count(child), 0, "opening reads no child");
    }
    store.get(root).expect("open the root");
    assert!(store.is_tree_loaded(root), "opening a root loads its tree");
    for child in [first, second] {
        // (b) exactly one fold per child, counted at the log itself.
        assert_eq!(store.log_open_count(child), 1, "one fold of {child}");
        assert!(!store.is_resident(child), "children stay out of residency");
        assert!(store.session_exists(child));
    }
    assert_eq!(store.children(root).expect("children").len(), 2);

    // Child logs go unreadable: everything the tree offers is already
    // cached, so further queries keep working and no second load happens.
    for child in [first, second] {
        fs::set_permissions(
            store.session_dir(child).join("events.jsonl"),
            fs::Permissions::from_mode(0o000),
        )
        .expect("unreadable child log");
    }
    assert_eq!(store.children(root).expect("children").len(), 2);
    assert_eq!(store.tree(root).expect("cached tree").children.len(), 2);
    assert_eq!(store.get(root).expect("root again").meta.session_id, root);
    assert_eq!(store.tree(root).expect("tree again").children.len(), 2);
    // (c) further uses of the loaded tree open no child log at all: were the
    // logs still readable this would be indistinguishable from a re-read.
    for child in [first, second] {
        assert_eq!(
            store.log_open_count(child),
            1,
            "the pass runs once for {child}"
        );
    }
}

/// §8.2 #6: addressing a child directly locates it by directory, loads its
/// root's tree first, then serves the child.
#[test]
fn direct_address_child_loads_its_tree_first() {
    let temporary = private_tempdir();
    let cwd = temporary.path().join("workspace");
    create_private_test_dir_all(&cwd);
    let data = temporary.path().join("data");
    let owner = SessionStore::open(&data, &cwd).expect("owner store");
    let root = persist_test_session(&owner);
    let child = persist_test_session_with_origin(&owner, delegated_origin(root, root, 1));
    drop(owner);

    let store = SessionStore::open(&data, &cwd).expect("cold store");
    // Force placement discovery instead of serving from the summary cache.
    let index = store
        .session_dir(root)
        .join(SUBAGENTS_DIR)
        .join(SUBAGENT_INDEX_FILE);
    fs::remove_file(&index).expect("remove child index");
    assert!(!store.is_tree_loaded(root));

    let projection = store.get(child).expect("direct child access");
    assert_eq!(projection.meta.session_id, child);
    assert!(
        store.is_tree_loaded(root),
        "the child's tree completed first"
    );
    assert_eq!(
        store.log_open_count(child),
        2,
        "one bulk fold, plus the read that serves the requested child itself"
    );
    assert!(!store.is_resident(child));
    assert!(index.is_file(), "the load rebuilt the child summary cache");
}

/// Review F2: `summary` and `fork` are the two paths that can answer a child
/// from memory alone — a cached/seeded summary, or a placement copy — and
/// both of those answers are exactly what a bulk pass also produces. Neither
/// may be served, nor a fork filed, before the tree is complete.
#[test]
fn cached_summary_and_fork_do_not_bypass_the_tree_load() {
    let temporary = private_tempdir();
    let cwd = temporary.path().join("workspace");
    create_private_test_dir_all(&cwd);
    let data = temporary.path().join("data");
    let owner = SessionStore::open(&data, &cwd).expect("owner store");
    let root = persist_test_session(&owner);
    let child = persist_test_session_with_origin(&owner, delegated_origin(root, root, 1));
    let mut seeded = owner.summary(child).expect("child summary");
    // Content the durable index can carry and a fold cannot invent.
    seeded.meta.title = Some(SessionTitle::new("seeded by the durable index").expect("test title"));
    drop(owner);

    fs::write(
        workdir_dir(&data, &cwd)
            .join(root.to_string())
            .join(SUBAGENTS_DIR)
            .join(SUBAGENT_INDEX_FILE),
        serde_json::to_vec(&SubagentIndex {
            version: SUBAGENT_INDEX_VERSION,
            children: vec![IndexedChild {
                summary: seeded,
                terminal_runs: BTreeMap::new(),
            }],
        })
        .expect("encode index"),
    )
    .expect("forge the index");

    let store = SessionStore::open(&data, &cwd).expect("cold store");
    assert!(
        !store.is_tree_loaded(root),
        "a cold store has completed nothing"
    );
    let served = store.summary(child).expect("child summary");
    assert_ne!(
        served.meta.title.as_ref().map(SessionTitle::as_str),
        Some("seeded by the durable index"),
        "the seeded summary is pre-load data and must not be what a load produced"
    );
    assert!(
        store.is_tree_loaded(root),
        "finishing the tree came before the memory-only answer"
    );
    assert_eq!(
        store.log_open_count(child),
        1,
        "and it cost the one read+fold §3.3 allows, not a second read"
    );

    // Same shape from a second cold store: forking a directly-addressed child
    // must not file it into a tree that was never flattened.
    let verifier = SessionStore::open(&data, &cwd).expect("verifier store");
    assert!(!verifier.is_tree_loaded(root));
    let fork_id = verifier
        .fork(child, test_user_input_seq(&verifier, child), test_origin())
        .expect("fork the child");
    assert!(
        verifier.is_tree_loaded(root),
        "the fork completed its source's tree before taking `mutation`"
    );
    assert!(
        verifier.tree_members(root).contains(&fork_id),
        "and filed the result into that tree"
    );
}

/// Observer recording what each completed load delivered, so a test can see
/// the products the engine would have folded in — and can reject them.
#[derive(Default)]
struct CapturingObserver {
    deliveries: Mutex<Vec<(SessionId, Vec<String>, usize)>>,
    reject: AtomicBool,
}

impl TreeLoadObserver for CapturingObserver {
    fn tree_loaded(
        &self,
        products: Arc<TreeLoadProducts>,
    ) -> Result<(), crate::runtime::EngineError> {
        if self.reject.load(Ordering::SeqCst) {
            return Err(crate::runtime::EngineError::ActorStopped);
        }
        let mut references = products.artifact_refs.iter().cloned().collect::<Vec<_>>();
        references.sort();
        self.deliveries.lock().expect("observer lock").push((
            products.root,
            references,
            products.grants.len(),
        ));
        Ok(())
    }
}

fn test_origin() -> cookie_agent_protocol::EventOrigin {
    cookie_agent_protocol::EventOrigin::new("engine:test").expect("static origin is valid")
}

/// One child's `events.jsonl` open count, for tests that create children in a
/// loop.
fn child_log_opens(store: &SessionStore, child: SessionId) -> usize {
    store.log_open_count(child)
}

/// (d) Concurrent triggers on one root coalesce: one fold per child and one
/// delivery of the products, no matter how many threads asked.
#[test]
fn concurrent_tree_load_triggers_fold_each_child_once() {
    let temporary = private_tempdir();
    let cwd = temporary.path().join("workspace");
    create_private_test_dir_all(&cwd);
    let data = temporary.path().join("data");
    let owner = SessionStore::open(&data, &cwd).expect("owner store");
    let root = persist_test_session(&owner);
    let children = (0..4)
        .map(|_| persist_test_session_with_origin(&owner, delegated_origin(root, root, 1)))
        .collect::<Vec<_>>();
    drop(owner);

    let store = SessionStore::open(&data, &cwd).expect("cold store");
    let observer = Arc::new(CapturingObserver::default());
    store
        .set_tree_load_observer(Arc::clone(&observer) as Arc<dyn TreeLoadObserver>)
        .expect("install observer");
    let threads = (0..8)
        .map(|_| {
            let store = Arc::clone(&store);
            thread::spawn(move || store.load_tree(root).expect("load the tree"))
        })
        .collect::<Vec<_>>();
    for thread in threads {
        thread.join().expect("loader thread");
    }
    for child in &children {
        assert_eq!(
            child_log_opens(&store, *child),
            1,
            "eight triggers folded {child} exactly once"
        );
    }
    assert_eq!(
        observer.deliveries.lock().expect("observer lock").len(),
        1,
        "one completed load delivers its products once"
    );
}

/// (e) The install check is real: every load pass verifies its fold against the
/// per-child fingerprints it read, and a fold that still agrees is published on
/// the first install — never retried. A verification that spuriously reported
/// staleness (a fingerprint derived from anything the fold itself perturbs)
/// would burn the retry budget here. Review L1.
#[test]
fn tree_load_verifies_its_fold_and_publishes_without_retrying() {
    let temporary = private_tempdir();
    let cwd = temporary.path().join("workspace");
    create_private_test_dir_all(&cwd);
    let data = temporary.path().join("data");
    let owner = SessionStore::open(&data, &cwd).expect("owner store");
    let root = persist_test_session(&owner);
    let child = persist_test_session_with_origin(&owner, delegated_origin(root, root, 1));
    drop(owner);

    let store = SessionStore::open(&data, &cwd).expect("cold store");
    let observer = Arc::new(CapturingObserver::default());
    store
        .set_tree_load_observer(Arc::clone(&observer) as Arc<dyn TreeLoadObserver>)
        .expect("install observer");
    let passes = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&passes);
    store.install_tree_load_read_hook_for_test(move |_| {
        counted.fetch_add(1, Ordering::SeqCst);
    });

    store.get(root).expect("the uncontended load publishes");
    assert_eq!(
        passes.load(Ordering::SeqCst),
        1,
        "one read pass: the fold verified clean and was installed"
    );
    assert_eq!(store.log_open_count(child), 1, "the child was folded once");
    assert_eq!(
        observer.deliveries.lock().expect("observer lock").len(),
        1,
        "one delivery, in the completion path the install owns"
    );
}

/// Observer that keeps whole product sets, so a test can assert on *what* a
/// fold published instead of only on how many times the pass ran.
#[derive(Default)]
struct ProductsObserver {
    deliveries: Mutex<Vec<Arc<TreeLoadProducts>>>,
}

impl TreeLoadObserver for ProductsObserver {
    fn tree_loaded(
        &self,
        products: Arc<TreeLoadProducts>,
    ) -> Result<(), crate::runtime::EngineError> {
        self.deliveries
            .lock()
            .expect("observer lock")
            .push(products);
        Ok(())
    }
}

/// A committed user title: the cheapest append whose effect is visible in the
/// summary a load publishes, and one that a fold-ignored payload would not be.
fn append_title(store: &SessionStore, id: SessionId, title: &str) {
    store
        .append(
            id,
            None,
            test_origin(),
            EventPayload::SessionTitleCommitted {
                change: SessionTitleChange::UserSet {
                    title: SessionTitle::new(title).expect("test title"),
                    client_rename_id: cookie_agent_protocol::ClientRenameId::new(format!(
                        "rename-{}",
                        SessionId::new_v7()
                    ))
                    .expect("test rename id"),
                },
                input_through_seq: 1,
            },
        )
        .expect("commit the title");
}

/// (e′) Review L1, gate F1: the per-child fingerprints are load-bearing, not
/// decorative. A real append through the store's own write path, landing in
/// the window between the read phase and the install, makes that fold stale.
/// The pass re-reads, publishes the fresh fold, and the stale one never
/// marks the tree loaded.
#[test]
fn stale_child_append_causes_refold() {
    let temporary = private_tempdir();
    let cwd = temporary.path().join("workspace");
    create_private_test_dir_all(&cwd);
    let data = temporary.path().join("data");
    let owner = SessionStore::open(&data, &cwd).expect("owner store");
    let root = persist_test_session(&owner);
    let child = persist_test_session_with_origin(&owner, delegated_origin(root, root, 1));
    drop(owner);

    let store = SessionStore::open(&data, &cwd).expect("cold store");
    let observer = Arc::new(ProductsObserver::default());
    store
        .set_tree_load_observer(Arc::clone(&observer) as Arc<dyn TreeLoadObserver>)
        .expect("install observer");

    let passes = Arc::new(AtomicUsize::new(0));
    // What the load state looked like when the re-fold began.
    let loaded_at_refold = Arc::new(AtomicBool::new(true));
    let driver = Arc::downgrade(&store);
    let counted = Arc::clone(&passes);
    let recorded = Arc::clone(&loaded_at_refold);
    store.install_tree_load_read_hook_for_test(move |root| {
        let Some(store) = driver.upgrade() else {
            return;
        };
        if counted.fetch_add(1, Ordering::SeqCst) == 0 {
            // Move the log this fold already fingerprinted, for real.
            store.open_for_write(child).expect("adopt the child");
            append_title(&store, child, "appended during the read phase");
            // Keep the appended record in the log's writer: the retry must be
            // attributable to this append alone, not to a background sync.
            store
                .get_resident(child)
                .expect("resident child")
                .log
                .pause_background_sync_for_test();
        } else {
            recorded.store(store.is_tree_loaded(root), Ordering::SeqCst);
        }
    });

    store.get(root).expect("the retried load publishes");
    assert_eq!(
        passes.load(Ordering::SeqCst),
        2,
        "one stale fold, then the re-fold that installed"
    );
    assert!(
        !loaded_at_refold.load(Ordering::SeqCst),
        "`loaded` was never published from the fold that lost the race"
    );
    assert!(store.is_tree_loaded(root), "the fresh fold completed");
    let deliveries = observer.deliveries.lock().expect("observer lock").clone();
    assert_eq!(deliveries.len(), 1, "one publish, from the fresh fold");
    let published = deliveries[0]
        .summaries
        .iter()
        .find(|summary| summary.meta.session_id == child)
        .expect("the child was published");
    assert_eq!(
        published.meta.title.as_ref().map(SessionTitle::as_str),
        Some("appended during the read phase"),
        "the published state contains the event the stale fold missed"
    );
    assert_eq!(
        child_log_opens(&store, child),
        1,
        "the retry folded the child's resident projection, not a second read of its log"
    );
}

/// (e″) Review L1, gate F1: when every unlocked fold loses the race the pass
/// falls back to folding with `mutation` held, and a fold that is still
/// unprovable there is *reported*, never published: nothing is installed,
/// nothing is delivered, and the root stays retryable.
///
/// How this is forced, honestly: the writer is the test hook, which appends
/// from the load's own thread and so re-enters `mutation`. That is the only
/// deterministic way to move a log under that lock — an in-process writer on
/// another thread blocks on it, so in production the condition this branch
/// guards against is a writer outside the process. The test therefore pins
/// the fallback's fail-closed behaviour, not a claim about which writer
/// caused it.
#[test]
fn a_fold_that_always_loses_the_race_reports_the_tree_contended() {
    let temporary = private_tempdir();
    let cwd = temporary.path().join("workspace");
    create_private_test_dir_all(&cwd);
    let data = temporary.path().join("data");
    let owner = SessionStore::open(&data, &cwd).expect("owner store");
    let root = persist_test_session(&owner);
    let child = persist_test_session_with_origin(&owner, delegated_origin(root, root, 1));
    drop(owner);

    let store = SessionStore::open(&data, &cwd).expect("cold store");
    let observer = Arc::new(ProductsObserver::default());
    store
        .set_tree_load_observer(Arc::clone(&observer) as Arc<dyn TreeLoadObserver>)
        .expect("install observer");

    let passes = Arc::new(AtomicUsize::new(0));
    let driver = Arc::downgrade(&store);
    let counted = Arc::clone(&passes);
    store.install_tree_load_read_hook_for_test(move |_| {
        let Some(store) = driver.upgrade() else {
            return;
        };
        if !store.is_owned(child) {
            store.open_for_write(child).expect("adopt the child");
            store
                .get_resident(child)
                .expect("resident child")
                .log
                .pause_background_sync_for_test();
        }
        // Append after every read phase, so no fold's fingerprints survive
        // to its install — including the one taken with `mutation` held.
        append_title(
            &store,
            child,
            &format!("racing append {}", SessionId::new_v7()),
        );
        counted.fetch_add(1, Ordering::SeqCst);
    });

    let error = store
        .load_tree(root)
        .expect_err("a fold that cannot be proven must not publish");
    assert!(
        matches!(error, SessionError::TreeContended(contended) if contended == root),
        "the budget-exhausted pass reports the tree, got {error:?}"
    );
    assert_eq!(
        passes.load(Ordering::SeqCst),
        TREE_LOAD_RACES + 1,
        "every unlocked fold was retried, then one pass with `mutation` held"
    );
    assert!(
        !store.is_tree_loaded(root),
        "an unprovable fold installs nothing, and never marks the tree loaded"
    );
    assert_eq!(
        store.tree_load_status(root),
        TreeLoadStatus::Unloaded,
        "the root stays joinable and retryable after the failure"
    );
    assert!(
        observer
            .deliveries
            .lock()
            .expect("observer lock")
            .is_empty(),
        "no products were delivered from an unproven fold"
    );
}

/// Review L2 + L3: an observer rejection leaves the tree retryable with its
/// products still queued, so the next trigger applies them without a second
/// fold, and each product set is claimed exactly once.
#[test]
fn rejected_tree_load_stays_retryable_and_redelivers_its_products() {
    let temporary = private_tempdir();
    let cwd = temporary.path().join("workspace");
    create_private_test_dir_all(&cwd);
    let data = temporary.path().join("data");
    let owner = SessionStore::open(&data, &cwd).expect("owner store");
    let root = persist_test_session(&owner);
    let child = persist_test_session_with_origin(&owner, delegated_origin(root, root, 1));
    drop(owner);

    let store = SessionStore::open(&data, &cwd).expect("cold store");
    let observer = Arc::new(CapturingObserver::default());
    observer.reject.store(true, Ordering::SeqCst);
    store
        .set_tree_load_observer(Arc::clone(&observer) as Arc<dyn TreeLoadObserver>)
        .expect("install observer");
    assert!(
        matches!(store.load_tree(root), Err(SessionError::TreeRejected(_))),
        "a rejected load fails the access that triggered it"
    );
    assert!(
        !store.is_tree_loaded(root),
        "a rejected load never marks the tree loaded"
    );
    assert_eq!(store.tree_load_status(root), TreeLoadStatus::Pending);

    observer.reject.store(false, Ordering::SeqCst);
    store.load_tree(root).expect("the retry delivers");
    assert!(store.is_tree_loaded(root));
    assert_eq!(child_log_opens(&store, child), 1, "no second fold");
    assert_eq!(
        observer.deliveries.lock().expect("observer lock").len(),
        1,
        "the queued products were claimed exactly once"
    );
}

/// Review L3 / D5: a load that finished before the engine installed its hook
/// is not lost — installing the observer delivers everything queued.
#[test]
fn loads_completed_without_an_observer_are_delivered_when_it_arrives() {
    let temporary = private_tempdir();
    let cwd = temporary.path().join("workspace");
    create_private_test_dir_all(&cwd);
    let data = temporary.path().join("data");
    let owner = SessionStore::open(&data, &cwd).expect("owner store");
    let root = persist_test_session(&owner);
    let child = persist_test_session_with_origin(&owner, delegated_origin(root, root, 1));
    drop(owner);

    let store = SessionStore::open(&data, &cwd).expect("cold store");
    store
        .load_tree(root)
        .expect("a load can complete with no observer installed");
    assert!(store.is_tree_loaded(root), "the store-side pass completed");
    let observer = Arc::new(CapturingObserver::default());
    store
        .set_tree_load_observer(Arc::clone(&observer) as Arc<dyn TreeLoadObserver>)
        .expect("install observer");
    assert_eq!(
        observer.deliveries.lock().expect("observer lock").clone(),
        vec![(root, vec![], 0)],
        "the queued products reached the late observer once"
    );
    assert_eq!(
        child_log_opens(&store, child),
        1,
        "and were never re-folded"
    );
}

/// Review F5: `Loaded` may not be observable while a load's products are
/// still unapplied. A pass that completed with no hook installed leaves them
/// *owed*, so an access on that tree is refused until the hook that arrives
/// later has taken them — and it takes them from the queued fold, never from
/// a second one.
#[test]
fn a_loaded_tree_owes_its_products_until_the_hook_has_taken_them() {
    let temporary = private_tempdir();
    let cwd = temporary.path().join("workspace");
    create_private_test_dir_all(&cwd);
    let data = temporary.path().join("data");
    let owner = SessionStore::open(&data, &cwd).expect("owner store");
    let root = persist_test_session(&owner);
    let child = persist_test_session_with_origin(&owner, delegated_origin(root, root, 1));
    drop(owner);

    let store = SessionStore::open(&data, &cwd).expect("cold store");
    store
        .load_tree(root)
        .expect("a store-side pass completes with no hook installed");
    assert!(
        store.is_tree_loaded(root),
        "the durable install is in place"
    );
    assert!(
        store
            .pending_loads
            .lock()
            .expect("pending load lock")
            .queued
            .contains_key(&root),
        "and its products were never applied"
    );

    // The hook arrives and refuses what it was handed: the tree is loaded,
    // the products are not applied.
    let observer = Arc::new(CapturingObserver::default());
    observer.reject.store(true, Ordering::SeqCst);
    assert!(
        matches!(
            store.set_tree_load_observer(Arc::clone(&observer) as Arc<dyn TreeLoadObserver>),
            Err(SessionError::TreeRejected(_))
        ),
        "a refused delivery fails the install that triggered it"
    );
    assert_eq!(store.tree_load_status(root), TreeLoadStatus::Loaded);
    assert!(
        observer
            .deliveries
            .lock()
            .expect("observer lock")
            .is_empty(),
        "nothing has been applied yet"
    );

    // Serving this tree would answer from state the engine never received.
    assert!(
        matches!(store.get(root), Err(SessionError::TreeRejected(_)),),
        "an access on a loaded-but-unapplied tree fails closed"
    );
    assert!(
        observer
            .deliveries
            .lock()
            .expect("observer lock")
            .is_empty(),
        "the refused delivery applied nothing"
    );

    observer.reject.store(false, Ordering::SeqCst);
    let projection = store
        .get(root)
        .expect("the owed products are delivered, then served");
    assert_eq!(projection.meta.session_id, root);
    assert_eq!(
        observer.deliveries.lock().expect("observer lock").len(),
        1,
        "the queued products reached the hook exactly once"
    );
    assert!(
        !store
            .pending_loads
            .lock()
            .expect("pending load lock")
            .queued
            .contains_key(&root),
        "and nothing is owed any more"
    );
    assert_eq!(
        child_log_opens(&store, child),
        1,
        "delivering them never re-folded the child (§3.3)"
    );
}

/// Observer that answers `parent_run_facts` the way the delegation registry
/// does — from inside a load's delivery — and records what that cost.
struct FactsAtDelivery {
    store: std::sync::Weak<SessionStore>,
    parent: SessionId,
    /// `(child log opens so far, facts resolved)` at delivery time.
    seen: Mutex<Vec<(usize, bool)>>,
}

impl TreeLoadObserver for FactsAtDelivery {
    fn tree_loaded(
        &self,
        _products: Arc<TreeLoadProducts>,
    ) -> Result<(), crate::runtime::EngineError> {
        let store = self
            .store
            .upgrade()
            .expect("the store outlives its own observer");
        let resolved = store
            .parent_run_facts(self.parent)
            .expect("registry lookup")
            .is_some();
        self.seen
            .lock()
            .expect("delivery probe lock")
            .push((store.log_open_count(self.parent), resolved));
        Ok(())
    }
}

/// §4.1.3, review F3: a delegation-parent's facts can never be paid for with
/// a cold child log read. Only that tree's bulk pass folds child logs, and it
/// carries the facts with it — including into the delivery where the registry
/// is rebuilt, which is where a second, differently-timed fold used to happen.
#[test]
fn parent_facts_of_an_unloaded_child_never_open_its_log() {
    let temporary = private_tempdir();
    let cwd = temporary.path().join("workspace");
    create_private_test_dir_all(&cwd);
    let data = temporary.path().join("data");
    let owner = SessionStore::open(&data, &cwd).expect("owner store");
    let root = persist_test_session(&owner);
    // `child` is itself a delegation parent: the registry asks it for facts.
    let child = persist_test_session_with_origin(&owner, delegated_origin(root, root, 1));
    let grandchild = persist_test_session_with_origin(&owner, delegated_origin(root, child, 2));
    drop(owner);

    let store = SessionStore::open(&data, &cwd).expect("cold store");
    assert_eq!(store.log_open_count(child), 0, "a cold store read nothing");
    assert!(
        store.parent_run_facts(child).expect("resolvable").is_none(),
        "facts for an unloaded tree are reported as not knowable yet"
    );
    assert_eq!(
        store.log_open_count(child),
        0,
        "and answering that way opened no child log (§3.3)"
    );

    let observer = Arc::new(FactsAtDelivery {
        store: Arc::downgrade(&store),
        parent: child,
        seen: Mutex::new(Vec::new()),
    });
    store
        .set_tree_load_observer(Arc::clone(&observer) as Arc<dyn TreeLoadObserver>)
        .expect("install observer");
    store.load_tree(root).expect("the tree load runs");

    let seen = observer.seen.lock().expect("delivery probe lock").clone();
    assert_eq!(
        seen,
        vec![(1, true)],
        "delivery resolved the facts from the fold it already made"
    );
    for descendant in [child, grandchild] {
        assert_eq!(
            store.log_open_count(descendant),
            1,
            "{descendant} was folded exactly once by the bulk pass (§3.3)"
        );
    }
    assert!(
        store
            .parent_run_facts(child)
            .expect("facts after the load")
            .is_some(),
        "and they stay resolvable afterwards"
    );
    assert_eq!(
        store.log_open_count(child),
        1,
        "resolving them opened nothing"
    );
    // The root is the one parent the pass cannot know about: its own log is
    // still the answer, and that read is legal.
    assert!(
        store.parent_run_facts(root).expect("root facts").is_some(),
        "a root's facts still fall back to the root's log"
    );
}

/// Review L9: a hostile `subagents/index.json` is dropped, not trusted — the
/// duplicate, a child with no directory, and another root filed as a child
/// none of them become live, and none of them move a real root's placement.
#[test]
fn subagent_index_entries_are_validated_before_they_are_trusted() {
    let temporary = private_tempdir();
    let cwd = temporary.path().join("workspace");
    create_private_test_dir_all(&cwd);
    let data = temporary.path().join("data");
    let owner = SessionStore::open(&data, &cwd).expect("owner store");
    let root = persist_test_session(&owner);
    let child = persist_test_session_with_origin(&owner, delegated_origin(root, root, 1));
    let other_root = persist_test_session(&owner);
    let child_summary = owner.summary(child).expect("child summary");
    let other_summary = owner.summary(other_root).expect("other root summary");
    drop(owner);

    let phantom = SessionId::new_v7();
    let entry = |summary: SessionSummary| IndexedChild {
        summary,
        terminal_runs: BTreeMap::new(),
    };
    let forged = SubagentIndex {
        version: SUBAGENT_INDEX_VERSION,
        children: vec![
            entry(child_summary.clone()),
            entry(child_summary.clone()),
            entry(SessionSummary {
                meta: cookie_agent_protocol::SessionMeta {
                    session_id: phantom,
                    ..child_summary.meta.clone()
                },
                usage: None,
                usage_rollup: Default::default(),
                agent_usage: BTreeMap::new(),
            }),
            entry(other_summary),
        ],
    };
    let index_path = workdir_dir(&data, &cwd)
        .join(root.to_string())
        .join(SUBAGENTS_DIR)
        .join(SUBAGENT_INDEX_FILE);
    fs::write(
        &index_path,
        serde_json::to_vec(&forged).expect("encode forged index"),
    )
    .expect("forge the index");

    let store = SessionStore::open(&data, &cwd).expect("cold store");
    assert_eq!(
        store
            .children(root)
            .expect("children")
            .into_iter()
            .map(|listed| listed.session_id)
            .collect::<Vec<_>>(),
        vec![child],
        "duplicates, phantoms and foreign roots are ignored"
    );
    assert!(
        !store.session_exists(phantom),
        "an index cannot make a nonexistent session live"
    );
    assert_eq!(
        store.root_of(other_root).expect("placement"),
        other_root,
        "an index entry cannot re-home a real root under another tree"
    );
    assert_eq!(store.root_of(child).expect("placement"), root);
}

/// Review L10: the durable index names a child only once that child's
/// directory is published, so a crash before the publish leaves no entry
/// pointing at a directory that does not exist.
#[test]
fn the_durable_index_names_only_published_children() {
    let temporary = private_tempdir();
    let cwd = temporary.path().join("workspace");
    create_private_test_dir_all(&cwd);
    let data = temporary.path().join("data");
    let store = SessionStore::open(&data, &cwd).expect("store");
    let root = persist_test_session(&store);
    let child = create_buffered_child(&store, root);
    let index_path = store
        .session_dir(root)
        .join(SUBAGENTS_DIR)
        .join(SUBAGENT_INDEX_FILE);
    let child_dir = workdir_dir(&data, &cwd)
        .join(root.to_string())
        .join(SUBAGENTS_DIR)
        .join(child.to_string());
    assert!(!child_dir.exists(), "a buffered child has no directory");

    let indexed = |store: &SessionStore| {
        store
            .read_subagent_index(root)
            .map(|index| {
                index
                    .children
                    .into_iter()
                    .map(|entry| entry.summary.meta.session_id)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    };
    assert_eq!(
        indexed(&store),
        Vec::new(),
        "an unpublished child is not named by the durable index"
    );
    // A tree load in that window must not smuggle it into the cache either.
    store.children(root).expect("listing the root tree");
    assert_eq!(
        indexed(&store),
        Vec::new(),
        "the load's cache rewrite skips children with no directory"
    );

    store
        .persist_buffered_session(child)
        .expect("publish the child");
    assert!(child_dir.is_dir(), "the child directory is published");
    assert_eq!(
        indexed(&store),
        vec![child],
        "the published child lands in the durable index"
    );
    assert!(index_path.is_file());
}

/// Creates a delegated child that stays buffered (published nowhere).
fn create_buffered_child(store: &SessionStore, root: SessionId) -> SessionId {
    let session_id = SessionId::new_v7();
    store
        .create(
            session_id,
            test_origin(),
            EventPayload::SessionCreated {
                short_id: None,
                origin: delegated_origin(root, root, 1),
                cwd_identity: cookie_agent_protocol::CwdIdentity::new("workspace:test")
                    .expect("static identity is valid"),
                creation_selection: crate::test_support::run_selection("test"),
                creation_agent: Box::new(crate::test_support::agent_snapshot(
                    "test",
                    AgentMode::Primary,
                )),
                runtime_revision: RuntimeRevision::new(format!("sha256:{}", "1".repeat(64)))
                    .expect("static revision is valid"),
                catalog_revision: CatalogRevision::new(format!("sha256:{}", "2".repeat(64)))
                    .expect("static revision is valid"),
                provider_state_revision: ProviderStateRevision::new(format!(
                    "sha256:{}",
                    "3".repeat(64)
                ))
                .expect("static revision is valid"),
                model_revision: ModelRevision::new(format!("sha256:{}", "4".repeat(64)))
                    .expect("static revision is valid"),
                agent_revision: AgentRevision::new(format!("sha256:{}", "5".repeat(64)))
                    .expect("static revision is valid"),
                recipe_registry_revision: RecipeRegistryRevision::new(format!(
                    "sha256:{}",
                    "6".repeat(64)
                ))
                .expect("static revision is valid"),
                manifest_revision: cookie_agent_protocol::ModelSnapshotRevision::new(format!(
                    "sha256:{}",
                    "7".repeat(64)
                ))
                .expect("static revision is valid"),
            },
        )
        .expect("create a buffered child");
    session_id
}

/// Review L11 / D6: publishing merges onto a directory that is *exactly* the
/// `subagents` scaffold. Anything else — an empty directory, or a name the
/// prepared files would have to overwrite — fails closed instead of replacing
/// bytes another writer owns.
#[test]
fn publishing_refuses_a_destination_that_is_not_the_scaffold() {
    let temporary = private_tempdir();
    let cwd = temporary.path().join("workspace");
    create_private_test_dir_all(&cwd);
    let data = temporary.path().join("data");
    let store = SessionStore::open(&data, &cwd).expect("store");
    let session_id = create_buffered_test_session(&store);
    let final_dir = store.session_dir(session_id);

    // An empty directory is not a scaffold: `.all()` on an empty listing must
    // not be read as "only the scaffold lives here".
    fs::create_dir_all(&final_dir).expect("pre-create the destination");
    assert!(
        matches!(
            store.persist_buffered_session(session_id),
            Err(SessionError::SessionLocked(id)) if id == session_id
        ),
        "an empty directory is not a scaffold"
    );
    fs::remove_dir_all(&final_dir).expect("clear the destination");

    // A foreign file beside the scaffold means somebody else owns it, and the
    // prepared `events.jsonl` must not be renamed over it.
    fs::create_dir_all(final_dir.join(SUBAGENTS_DIR)).expect("scaffold");
    write_private_test_file(&final_dir.join(EVENTS_FILE), "someone else's log");
    assert!(
        matches!(
            store.persist_buffered_session(session_id),
            Err(SessionError::SessionLocked(id)) if id == session_id
        ),
        "a destination collision is rejected, never renamed over"
    );
    assert_eq!(
        fs::read_to_string(final_dir.join(EVENTS_FILE)).expect("foreign log"),
        "someone else's log",
        "the rejected publish left the foreign bytes alone"
    );
}

/// D6, the case the scaffold exists for: a root whose child was filed while
/// the root was still buffered merges its prepared files into that scaffold.
#[test]
fn publishing_merges_onto_a_bare_subagents_scaffold() {
    let temporary = private_tempdir();
    let cwd = temporary.path().join("workspace");
    create_private_test_dir_all(&cwd);
    let data = temporary.path().join("data");
    let store = SessionStore::open(&data, &cwd).expect("store");
    let root = create_buffered_test_session(&store);
    // Filing a child under an unpublished root brings the scaffold into being.
    let child = persist_test_session_with_origin(&store, delegated_origin(root, root, 1));
    let root_dir = store.session_dir(root);
    assert!(
        root_dir.join(SUBAGENTS_DIR).is_dir(),
        "the child created the root scaffold"
    );
    assert!(
        !root_dir.join(EVENTS_FILE).is_file(),
        "the root itself is not published yet"
    );

    store.persist_buffered_session(root).expect("publish root");
    assert!(
        root_dir.join(EVENTS_FILE).is_file(),
        "the merged log is durable"
    );
    assert!(
        root_dir.join(SESSION_META_FILE).is_file(),
        "the merged metadata cache is durable"
    );
    assert!(
        root_dir
            .join(SUBAGENTS_DIR)
            .join(child.to_string())
            .is_dir(),
        "the scaffold child survived the merge"
    );
    assert_eq!(store.children(root).expect("children").len(), 1);
    assert_eq!(store.root_of(child).expect("placement"), root);
}

/// D8 / review L12: seeding from `subagents/index.json` drops the derived
/// per-session `usage` while keeping the durable rollups.
#[test]
fn seeded_child_summaries_drop_derived_usage() {
    let temporary = private_tempdir();
    let cwd = temporary.path().join("workspace");
    create_private_test_dir_all(&cwd);
    let data = temporary.path().join("data");
    let owner = SessionStore::open(&data, &cwd).expect("owner store");
    let root = persist_test_session(&owner);
    let child = persist_test_session_with_origin(&owner, delegated_origin(root, root, 1));
    let mut summary = owner.summary(child).expect("child summary");
    summary.usage = Some(Usage {
        input_tokens: Some(4_321),
        ..Default::default()
    });
    summary.usage_rollup.input_tokens = 99;
    drop(owner);

    fs::write(
        workdir_dir(&data, &cwd)
            .join(root.to_string())
            .join(SUBAGENTS_DIR)
            .join(SUBAGENT_INDEX_FILE),
        serde_json::to_vec(&SubagentIndex {
            version: SUBAGENT_INDEX_VERSION,
            children: vec![IndexedChild {
                summary,
                terminal_runs: BTreeMap::new(),
            }],
        })
        .expect("encode index"),
    )
    .expect("forge the index");

    let store = SessionStore::open(&data, &cwd).expect("cold store");
    let seeded = store.summary(child).expect("seeded summary");
    assert!(
        seeded.usage.is_none(),
        "a cold child must not serve derived usage"
    );
    assert_eq!(
        seeded.usage_rollup.input_tokens, 99,
        "the durable rollup is still served"
    );
    assert_eq!(
        store
            .children(root)
            .expect("children")
            .into_iter()
            .next()
            .expect("listed child")
            .usage,
        None,
        "and neither does the listing"
    );
}

/// §8.2 #11: a fork inherits the source's tree, so fork-of-root is published
/// into the work dir and fork-of-child into the same root's `subagents/`.
#[test]
fn fork_placement_follows_the_source_tree() {
    let temporary = private_tempdir();
    let cwd = temporary.path().join("workspace");
    create_private_test_dir_all(&cwd);
    let data = temporary.path().join("data");
    let store = SessionStore::open(&data, &cwd).expect("session store");
    let root = persist_test_session(&store);
    let child = persist_test_session_with_origin(&store, delegated_origin(root, root, 1));
    let origin = cookie_agent_protocol::EventOrigin::new("client:test").unwrap();

    let root_fork = store
        .fork(root, test_user_input_seq(&store, root), origin.clone())
        .expect("fork of root");
    let child_fork = store
        .fork(child, test_user_input_seq(&store, child), origin)
        .expect("fork of child");

    let root_dir = store.session_dir(root);
    assert_eq!(
        store.session_dir(root_fork).parent(),
        Some(store.workdir_dir.as_path())
    );
    assert_eq!(
        store.session_dir(child_fork).parent(),
        Some(root_dir.join(SUBAGENTS_DIR).as_path())
    );
    assert!(matches!(
        store
            .get(child_fork)
            .expect("forked child")
            .meta
            .origin,
        SessionOrigin::Delegated {
            root_session_id,
            parent_session_id,
            ..
        } if root_session_id == root && parent_session_id == root
    ));
    let index = store.tree_members(root);
    assert!(index.contains(&child));
    assert!(index.contains(&child_fork));
    assert!(
        !store
            .children(root)
            .expect("children")
            .into_iter()
            .any(|listed| listed.session_id == root_fork)
    );
    assert!(root_dir.join(SUBAGENTS_DIR).join("index.json").is_file());
}

/// §8.2 #12: `subagents/index.json` is a cache. Corrupt or missing content
/// never fails a startup, and placement discovery rebuilds it.
#[test]
fn subagent_index_corruption_is_rebuilt_not_fatal() {
    let temporary = private_tempdir();
    let cwd = temporary.path().join("workspace");
    create_private_test_dir_all(&cwd);
    let data = temporary.path().join("data");
    let store = SessionStore::open(&data, &cwd).expect("owner store");
    let root = persist_test_session(&store);
    let child = persist_test_session_with_origin(&store, delegated_origin(root, root, 1));
    let index_path = store
        .session_dir(root)
        .join(SUBAGENTS_DIR)
        .join(SUBAGENT_INDEX_FILE);
    assert!(index_path.is_file(), "index written on child creation");
    drop(store);

    for corruption in ["not json at all", "{\"version\": 99, \"children\": []}"] {
        fs::write(&index_path, corruption).expect("corrupt index");
        let observer = SessionStore::open(&data, &cwd).expect("cold open with corrupt index");
        let listed = observer
            .children(root)
            .expect("children")
            .into_iter()
            .map(|child| child.session_id)
            .collect::<Vec<_>>();
        // The directory scan re-adopts the filed child and rebuilds the cache.
        assert!(listed.contains(&child), "placement rescans filed children");
        let rebuilt: serde_json::Value =
            serde_json::from_slice(&fs::read(&index_path).expect("rebuilt index"))
                .expect("valid index json");
        assert_eq!(
            rebuilt["version"],
            serde_json::json!(SUBAGENT_INDEX_VERSION)
        );
        assert_eq!(rebuilt["children"].as_array().expect("children").len(), 1);
        drop(observer);
    }

    fs::remove_file(&index_path).expect("remove index");
    let observer = SessionStore::open(&data, &cwd).expect("cold open with missing index");
    assert!(
        observer
            .children(root)
            .expect("children")
            .into_iter()
            .any(|listed| listed.session_id == child)
    );
}

fn append_pending_test_delta(
    store: &SessionStore,
    session_id: SessionId,
    text: &str,
) -> (
    Arc<crate::events::EventLog>,
    cookie_agent_protocol::StoredEvent,
) {
    let projection = store.get(session_id).expect("session projection");
    let (run_id, resolved_model, prompt_fingerprint) = projection
        .log
        .events()
        .iter()
        .find_map(|event| match &event.payload {
            EventPayload::RunStarted {
                agent,
                selected_suffix,
                ..
            } => Some((
                event.run_id.expect("run id"),
                crate::model_history::wire_model(selected_suffix.first().expect("selected model")),
                agent.prompt_fingerprint.clone(),
            )),
            _ => None,
        })
        .expect("run event");
    let attempt_id = AttemptId::new_v7();
    store
        .append(
            session_id,
            Some(run_id),
            cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
            EventPayload::ModelAttemptStarted {
                attempt_id,
                attempt_ordinal: 1,
                fallback_index: 0,
                retry_ordinal: 0,
                resolved_model,
                prompt_fingerprint,
            },
        )
        .expect("start attempt");
    let log = store.get(session_id).expect("session projection").log;
    log.pause_background_sync_for_test();
    let delta = store
        .append(
            session_id,
            Some(run_id),
            cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
            EventPayload::TextDelta {
                attempt_id,
                text: text.into(),
            },
        )
        .expect("append buffered delta");
    (log, delta)
}

#[test]
fn eviction_waits_for_pending_stream_records_to_sync() {
    let temporary = private_tempdir();
    let cwd = temporary.path().join("workspace");
    create_private_test_dir_all(&cwd);
    let store = SessionStore::open(&temporary.path().join("data"), &cwd).unwrap();
    let session_id = persist_test_session(&store);
    let (log, _) = append_pending_test_delta(&store, session_id, "durable before eviction");
    let (sync_reached, release_sync) = log.install_sync_hook_for_test();
    let (eviction_done, eviction_result) = mpsc::channel();
    let evicting = {
        let store = store.clone();
        thread::spawn(move || {
            eviction_done
                .send(store.evict(session_id))
                .expect("report eviction result");
        })
    };

    sync_reached.recv().expect("eviction reached pending sync");
    assert!(matches!(
        eviction_result.try_recv(),
        Err(mpsc::TryRecvError::Empty)
    ));
    release_sync.send(()).expect("release eviction sync");
    assert!(
        eviction_result
            .recv()
            .expect("receive eviction result")
            .expect("evict session")
    );
    evicting.join().expect("eviction thread");

    let durable = crate::events::load_jsonl::<cookie_agent_protocol::StoredEvent>(
        &store.session_dir(session_id).join("events.jsonl"),
    )
    .expect("read evicted event log");
    assert!(durable.iter().any(|event| matches!(
        &event.payload,
        EventPayload::TextDelta { text, .. } if text == "durable before eviction"
    )));
}

#[test]
fn fork_flushes_pending_source_records_before_copying() {
    let temporary = private_tempdir();
    let cwd = temporary.path().join("workspace");
    create_private_test_dir_all(&cwd);
    let store = SessionStore::open(&temporary.path().join("data"), &cwd).unwrap();
    let session_id = persist_test_session(&store);
    let (log, delta) = append_pending_test_delta(&store, session_id, "copied after sync");
    assert!(log.writer_is_open_for_test());
    let delta_seq = delta.seq;
    let run_id = delta.run_id.expect("delta run id");
    let EventPayload::TextDelta { attempt_id, .. } = delta.payload else {
        panic!("pending event is a text delta")
    };
    let (sync_reached, release_sync) = log.install_sync_hook_for_test();
    let (fork_done, fork_result) = mpsc::channel();
    let forking = {
        let store = store.clone();
        thread::spawn(move || {
            fork_done
                .send(store.fork(
                    session_id,
                    delta_seq,
                    cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
                ))
                .expect("report fork result");
        })
    };

    sync_reached.recv().expect("fork reached source sync");
    assert!(matches!(
        fork_result.try_recv(),
        Err(mpsc::TryRecvError::Empty)
    ));
    release_sync.send(()).expect("release fork sync");
    let fork_id = fork_result
        .recv()
        .expect("receive fork result")
        .expect("fork session");
    forking.join().expect("fork thread");
    assert!(!log.writer_is_open_for_test());

    let copied = store.get(fork_id).expect("fork projection").log.events();
    assert!(copied.iter().any(|event| matches!(
        &event.payload,
        EventPayload::TextDelta { text, .. } if text == "copied after sync"
    )));
    store
        .append(
            session_id,
            Some(run_id),
            cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
            EventPayload::ReasoningDelta {
                attempt_id,
                text: "reopened after fork".into(),
            },
        )
        .expect("append after suspended fork source");
    assert!(log.writer_is_open_for_test());
    log.flush().expect("flush reopened source writer");
}

#[cfg(unix)]
#[test]
fn unix_buffered_session_is_private_at_creation() {
    let temporary = private_tempdir();
    let cwd = temporary.path().join("workspace");
    create_private_test_dir_all(&cwd);
    let data = temporary.path().join("data");
    let store = SessionStore::open(&data, &cwd).unwrap();
    let session_id = persist_test_session(&store);
    let workdir = store.workdir_dir_path();
    let session = store.session_dir(session_id);

    for path in [
        data.clone(),
        data.join(SESSIONS_ROOT_DIR),
        workdir.to_owned(),
        session.clone(),
    ] {
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }
    for path in [
        workdir.join(WORKDIR_CWD_FILE),
        workdir.join(LAYOUT_MARKER_FILE),
        session.join("events.jsonl"),
        session.join(SESSION_META_FILE),
    ] {
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}

#[cfg(unix)]
#[test]
fn unix_session_reuses_preexisting_loose_modes() {
    let temporary = private_tempdir();
    let cwd = temporary.path().join("workspace");
    create_private_test_dir_all(&cwd);
    let data = temporary.path().join("data");
    let store = SessionStore::open(&data, &cwd).unwrap();
    let session_id = persist_test_session(&store);
    let workdir = store.workdir_dir_path().to_owned();
    let session = store.session_dir(session_id);
    let directories = [
        data.clone(),
        data.join(SESSIONS_ROOT_DIR),
        workdir.clone(),
        session.clone(),
    ];
    let files = [
        workdir.join(WORKDIR_CWD_FILE),
        workdir.join(LAYOUT_MARKER_FILE),
        session.join("events.jsonl"),
        session.join(SESSION_META_FILE),
    ];
    for path in &directories {
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    for path in &files {
        fs::set_permissions(path, fs::Permissions::from_mode(0o644)).unwrap();
    }
    drop(store);

    let reopened = SessionStore::open(&data, &cwd).unwrap();
    reopened.get(session_id).expect("loose existing session");
    for path in directories {
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o755
        );
    }
    for path in files {
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o644
        );
    }
}

// Requires Unix symlink semantics and exact raw path bytes.
#[cfg(unix)]
#[test]
fn canonical_aliases_share_the_existing_workdir_and_record_canonical_bytes() {
    let temp = tempfile::tempdir().expect("temp");
    let real = temp.path().join("real");
    let alias = temp.path().join("alias");
    let data = temp.path().join("data");
    fs::create_dir(&real).expect("real cwd");
    symlink(&real, &alias).expect("alias");

    let real_store = SessionStore::open(&data, &real).expect("real store");
    let alias_store = SessionStore::open(&data, &alias).expect("alias store");
    assert_eq!(
        real_store.workdir_dir_path(),
        alias_store.workdir_dir_path()
    );
    assert_eq!(
        fs::read(cwd_file(&data, &alias)).expect("cwd bytes"),
        real.canonicalize()
            .expect("canonical")
            .as_os_str()
            .as_bytes()
    );
}

// Requires constructing and comparing non-UTF8 Unix path bytes.
#[cfg(unix)]
#[test]
fn non_utf8_cwd_round_trips_exact_bytes() {
    let temp = tempfile::tempdir().expect("temp");
    let cwd = temp
        .path()
        .join(OsString::from_vec(b"project-\xfe\xff".to_vec()));
    let data = temp.path().join("data");
    fs::create_dir(&cwd).expect("cwd");

    SessionStore::open(&data, &cwd).expect("store");
    assert_eq!(
        fs::read(cwd_file(&data, &cwd)).expect("cwd bytes"),
        cwd.canonicalize()
            .expect("canonical")
            .as_os_str()
            .as_bytes()
    );
}

// Existing state is reused without permission repair.
#[cfg(unix)]
#[test]
fn cwd_file_is_private_at_creation_and_loose_mode_is_reused() {
    let temp = tempfile::tempdir().expect("temp");
    let data = temp.path().join("data");
    SessionStore::open(&data, temp.path()).expect("store");
    let path = cwd_file(&data, temp.path());
    assert_eq!(
        fs::metadata(&path).expect("metadata").mode() & 0o7777,
        0o600
    );

    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).expect("loosen mode");
    SessionStore::open(&data, temp.path()).expect("reopen");
    assert_eq!(fs::metadata(path).expect("metadata").mode() & 0o7777, 0o644);
}

// Verifies atomic replacement using Unix inode identity and mode bits.
#[cfg(unix)]
#[test]
fn stale_cwd_file_is_replaced_atomically() {
    let temp = tempfile::tempdir().expect("temp");
    let data = temp.path().join("data");
    SessionStore::open(&data, temp.path()).expect("store");
    let path = cwd_file(&data, temp.path());
    fs::write(&path, b"stale project path").expect("stale file");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("mode");
    let stale_inode = fs::metadata(&path).expect("stale metadata").ino();

    SessionStore::open(&data, temp.path()).expect("refresh");
    assert_ne!(
        fs::metadata(&path).expect("new metadata").ino(),
        stale_inode
    );
    assert_eq!(
        fs::read(&path).expect("cwd bytes"),
        temp.path()
            .canonicalize()
            .expect("canonical")
            .as_os_str()
            .as_bytes()
    );
    let workdir = path.parent().expect("workdir");
    assert!(
        fs::read_dir(workdir)
            .expect("workdir entries")
            .filter_map(Result::ok)
            .all(|entry| !entry.file_name().to_string_lossy().starts_with(".cwd."))
    );
}

// Verifies retention using Unix inode identity.
#[cfg(unix)]
#[test]
fn correct_cwd_file_is_retained_on_reopen() {
    let temp = tempfile::tempdir().expect("temp");
    let data = temp.path().join("data");
    SessionStore::open(&data, temp.path()).expect("store");
    let path = cwd_file(&data, temp.path());
    let inode = fs::metadata(&path).expect("metadata").ino();

    SessionStore::open(&data, temp.path()).expect("reopen");
    assert_eq!(fs::metadata(path).expect("metadata").ino(), inode);
}

#[test]
fn replayed_stamps_keep_footer_and_session_cost_equal_across_pricing_changes() {
    let temp = private_tempdir();
    let path = temp.path().join("events.jsonl");
    let session_id = SessionId::new_v7();
    let run_id = RunId::new_v7();
    let attempt_id = AttemptId::new_v7();
    let agent = crate::test_support::agent_snapshot("test", AgentMode::Primary);
    let selection = crate::test_support::run_selection("test");
    let binding = agent.fallback_chain[0].clone();
    let resolved_model = crate::model_history::wire_model(&binding);
    let model_key = resolved_model.selection.model.clone();
    let runtime_revision = RuntimeRevision::new(format!("sha256:{}", "1".repeat(64))).unwrap();
    let catalog_revision = CatalogRevision::new(format!("sha256:{}", "2".repeat(64))).unwrap();
    let provider_state_revision =
        ProviderStateRevision::new(format!("sha256:{}", "3".repeat(64))).unwrap();
    let model_revision = ModelRevision::new(format!("sha256:{}", "4".repeat(64))).unwrap();
    let agent_revision = AgentRevision::new(format!("sha256:{}", "5".repeat(64))).unwrap();
    let recipe_registry_revision =
        RecipeRegistryRevision::new(format!("sha256:{}", "6".repeat(64))).unwrap();
    let log = crate::events::EventLog::create(
        path.clone(),
        session_id,
        cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
        EventPayload::SessionCreated {
            short_id: None,
            origin: SessionOrigin::Root,
            cwd_identity: cookie_agent_protocol::CwdIdentity::new("workspace:test").unwrap(),
            creation_selection: selection.clone(),
            creation_agent: Box::new(agent.clone()),
            runtime_revision: runtime_revision.clone(),
            catalog_revision: catalog_revision.clone(),
            provider_state_revision: provider_state_revision.clone(),
            model_revision: model_revision.clone(),
            agent_revision: agent_revision.clone(),
            recipe_registry_revision: recipe_registry_revision.clone(),
            manifest_revision: binding.manifest_revision.clone(),
        },
    )
    .unwrap();
    log.append(
        Some(run_id),
        cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
        EventPayload::RunStarted {
            client_run_id: ClientRunId::new("usage-replay").unwrap(),
            selection,
            agent: Box::new(agent.clone()),
            runtime_revision,
            catalog_revision,
            provider_state_revision,
            model_revision,
            agent_revision,
            recipe_registry_revision,
            manifest_revision: binding.manifest_revision.clone(),
            selected_suffix: vec![binding],
            internal_agents: Vec::new(),
            input_through_seq: 1,
        },
    )
    .unwrap();
    log.append(
        Some(run_id),
        cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
        EventPayload::UserInputSubmitted {
            input: "question".into(),
        },
    )
    .unwrap();
    log.append(
        Some(run_id),
        cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
        EventPayload::ModelAttemptStarted {
            attempt_id,
            attempt_ordinal: 1,
            fallback_index: 0,
            retry_ordinal: 0,
            resolved_model: resolved_model.clone(),
            prompt_fingerprint: agent.prompt_fingerprint.clone(),
        },
    )
    .unwrap();
    let usage = Usage {
        input_tokens: Some(120),
        input_tokens_no_cache: Some(70),
        input_tokens_cache_read: Some(40),
        input_tokens_cache_write: Some(10),
        output_tokens: Some(30),
        ..Usage::default()
    };
    log.append(
        Some(run_id),
        cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
        EventPayload::ModelTurnCommitted {
            attempt_id,
            model_turn_seq: 1,
            resolved_model: resolved_model.clone(),
            input_through_seq: 1,
            turn: PersistedModelTurn {
                content: Vec::new(),
                provider_options: BTreeMap::new(),
                finish_reason: ModelFinishReason::Stop,
                usage: usage.clone(),
                response_metadata: BTreeMap::new(),
                provider_metadata: BTreeMap::new(),
                native_replay: None,
            },
            warnings: Vec::new(),
        },
    )
    .unwrap();
    let usage_event = log
        .append(
            Some(run_id),
            cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
            EventPayload::ModelUsageRecorded {
                model_turn_seq: 1,
                agent_id: agent.agent.clone(),
                resolved_model,
                usage,
                estimated_cost_pico_usd: Some(123_456_789_000),
            },
        )
        .unwrap();
    let through_seq = usage_event.seq;
    drop(log);

    let source_json = fs::read_to_string(&path).unwrap();
    let rewrite = |session_id: SessionId, stamp: Option<u64>| {
        source_json
            .lines()
            .map(|line| {
                let mut value: serde_json::Value = serde_json::from_str(line).unwrap();
                value["session_id"] = serde_json::json!(session_id);
                if value["payload"]["type"] == "model_usage_recorded" {
                    let payload = value["payload"].as_object_mut().unwrap();
                    payload.insert(
                        "estimated_cost_pico_usd".into(),
                        serde_json::to_value(stamp).unwrap(),
                    );
                }
                serde_json::to_string(&value).unwrap()
            })
            .collect::<Vec<_>>()
            .join("\n")
            + "\n"
    };
    let reopened = crate::events::EventLog::open(path, session_id).unwrap();
    // The TUI footer reducer sums these same durable pico-USD stamps.
    let footer_pico_usd = reopened
        .events()
        .iter()
        .filter_map(|event| match event.payload {
            EventPayload::ModelUsageRecorded {
                estimated_cost_pico_usd,
                ..
            } => estimated_cost_pico_usd,
            _ => None,
        })
        .sum::<u64>();
    let rebuilt = projection(reopened).unwrap();
    assert_eq!(rebuilt.usage_rollup.request_count, 1);
    assert_eq!(rebuilt.usage_rollup.input_tokens, 120);
    assert_eq!(rebuilt.usage_rollup.output_tokens, 30);
    assert_eq!(rebuilt.usage_rollup.cache_read_tokens, 40);
    assert_eq!(rebuilt.usage_rollup.cache_write_tokens, 10);
    assert_eq!(rebuilt.agent_usage[&agent.agent].request_count, 1);
    let changed_pricing = PricingConfig {
        models: BTreeMap::from([(
            model_key.clone(),
            ModelPricing {
                input_per_million_usd: Some(PicoUsdPerMillion::from_decimal_str("999").unwrap()),
                output_per_million_usd: Some(PicoUsdPerMillion::from_decimal_str("999").unwrap()),
                ..ModelPricing::default()
            },
        )]),
    };
    let expected = Some(footer_pico_usd as f64 / 1_000_000_000_000.0);
    assert_eq!(
        crate::usage::with_pricing(
            rebuilt.usage_rollup.clone(),
            &PricingConfig::default(),
            &BTreeMap::new(),
        )
        .estimated_cost_usd,
        expected
    );

    let cwd = temp.path().join("fork-cwd");
    let data = temp.path().join("fork-data");
    create_private_test_dir_all(&cwd);
    let seed = SessionStore::open(&data, &cwd).unwrap();
    let sessions_dir = seed.workdir_dir_path().to_owned();
    drop(seed);
    let stamped_source = SessionId::new_v7();
    let unpriced_source = SessionId::new_v7();
    for (source_id, contents) in [
        (
            stamped_source,
            rewrite(stamped_source, Some(123_456_789_000)),
        ),
        (unpriced_source, rewrite(unpriced_source, None)),
    ] {
        let directory = sessions_dir.join(source_id.to_string());
        create_private_test_dir_all(&directory);
        write_private_test_file(&directory.join("events.jsonl"), contents);
    }
    let store = SessionStore::open(&data, &cwd).unwrap();
    let stamped_fork = store
        .fork(
            stamped_source,
            through_seq,
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .unwrap();
    let unpriced_fork = store
        .fork(
            unpriced_source,
            through_seq,
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .unwrap();
    assert_eq!(
        crate::usage::with_pricing(
            store.get(stamped_fork).unwrap().usage_rollup,
            &changed_pricing,
            &BTreeMap::new(),
        )
        .estimated_cost_usd,
        expected
    );
    assert_eq!(
        crate::usage::with_pricing(
            store.get(unpriced_fork).unwrap().usage_rollup,
            &changed_pricing,
            &BTreeMap::new(),
        )
        .estimated_cost_usd,
        None
    );
    assert_eq!(
        crate::usage::with_pricing(rebuilt.usage_rollup, &changed_pricing, &BTreeMap::new(),)
            .estimated_cost_usd,
        expected
    );
}

#[test]
fn internal_usage_is_once_per_fallback_phase_and_all_kinds_survive_reopen() {
    let temp = private_tempdir();
    let path = temp.path().join("internal-usage-events.jsonl");
    let session_id = SessionId::new_v7();
    let run_id = RunId::new_v7();
    let owner = crate::test_support::agent_snapshot("test", AgentMode::Primary);
    let selection = crate::test_support::run_selection("test");
    let binding = owner.fallback_chain[0].clone();
    let resolved_model = crate::model_history::wire_model(&binding);
    let fallback_model =
        crate::model_history::wire_model(&crate::test_support::model_binding_named("fallback-one"));
    let revision = |value: char| format!("sha256:{}", value.to_string().repeat(64));
    let runtime_revision = RuntimeRevision::new(revision('1')).unwrap();
    let catalog_revision = CatalogRevision::new(revision('2')).unwrap();
    let provider_state_revision = ProviderStateRevision::new(revision('3')).unwrap();
    let model_revision = ModelRevision::new(revision('4')).unwrap();
    let agent_revision = AgentRevision::new(revision('5')).unwrap();
    let recipe_registry_revision = RecipeRegistryRevision::new(revision('6')).unwrap();
    let log = crate::events::EventLog::create(
        path.clone(),
        session_id,
        cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
        EventPayload::SessionCreated {
            short_id: None,
            origin: SessionOrigin::Root,
            cwd_identity: cookie_agent_protocol::CwdIdentity::new("workspace:test").unwrap(),
            creation_selection: selection.clone(),
            creation_agent: Box::new(owner.clone()),
            runtime_revision: runtime_revision.clone(),
            catalog_revision: catalog_revision.clone(),
            provider_state_revision: provider_state_revision.clone(),
            model_revision: model_revision.clone(),
            agent_revision: agent_revision.clone(),
            recipe_registry_revision: recipe_registry_revision.clone(),
            manifest_revision: binding.manifest_revision.clone(),
        },
    )
    .unwrap();
    log.append(
        Some(run_id),
        cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
        EventPayload::RunStarted {
            client_run_id: ClientRunId::new("internal-usage-replay").unwrap(),
            selection,
            agent: Box::new(owner),
            runtime_revision,
            catalog_revision,
            provider_state_revision,
            model_revision,
            agent_revision,
            recipe_registry_revision,
            manifest_revision: binding.manifest_revision.clone(),
            selected_suffix: vec![binding],
            internal_agents: Vec::new(),
            input_through_seq: 1,
        },
    )
    .unwrap();

    let kinds = [
        (
            InternalAgentKind::Approval,
            cookie_agent_config::BUILT_IN_APPROVAL_AGENT_ID,
        ),
        (
            InternalAgentKind::ContextCompaction,
            cookie_agent_config::BUILT_IN_COMPACTION_AGENT_ID,
        ),
        (
            InternalAgentKind::SessionTitle,
            cookie_agent_config::BUILT_IN_TITLE_AGENT_ID,
        ),
    ];
    for (index, (kind, agent_name)) in kinds.into_iter().enumerate() {
        let invocation_id = InternalAgentInvocationId::new_v7();
        let internal_run_id = InternalAgentRunId::new_v7();
        let agent_id = AgentId::new(agent_name).unwrap();
        log.append(
            Some(run_id),
            cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
            EventPayload::InternalAgentStarted {
                invocation_id,
                internal_run_id,
                kind,
                backend: InternalAgentBackend::Model {
                    resolved_model: resolved_model.clone(),
                },
                call: SafeInternalAgentCall {
                    name: SafeCode::new("internal").unwrap(),
                    input_summary: SafeDisplayText::new("bounded input").unwrap(),
                    input_digest: Sha256Digest::of_bytes(b"input"),
                },
            },
        )
        .unwrap();
        let usage = Usage {
            input_tokens: Some(100 + index as u64),
            input_tokens_cache_read: Some(0),
            output_tokens: Some(10),
            output_tokens_reasoning: Some(0),
            ..Usage::default()
        };
        log.append(
            Some(run_id),
            cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
            EventPayload::InternalAgentUsageRecorded {
                internal_run_id,
                kind,
                agent_id: agent_id.clone(),
                resolved_model: resolved_model.clone(),
                usage: usage.clone(),
                estimated_cost_pico_usd: None,
            },
        )
        .unwrap();
        if index == 0 {
            assert!(
                log.append(
                    Some(run_id),
                    cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
                    EventPayload::InternalAgentUsageRecorded {
                        internal_run_id,
                        kind,
                        agent_id: agent_id.clone(),
                        resolved_model: resolved_model.clone(),
                        usage: usage.clone(),
                        estimated_cost_pico_usd: None,
                    },
                )
                .is_err()
            );
            let failure = || InternalAgentFailure {
                code: SafeCode::new("fallback").unwrap(),
                message: SafeErrorMessage::new("test fallback").unwrap(),
                retryable: true,
                model_error: None,
            };
            log.append(
                Some(run_id),
                cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
                EventPayload::InternalAgentFallback {
                    invocation_id,
                    internal_run_id,
                    kind,
                    from: InternalAgentBackend::Model {
                        resolved_model: resolved_model.clone(),
                    },
                    to: InternalAgentBackend::Model {
                        resolved_model: fallback_model.clone(),
                    },
                    failure: failure(),
                    attempts: 1,
                },
            )
            .unwrap();
            log.append(
                Some(run_id),
                cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
                EventPayload::InternalAgentUsageRecorded {
                    internal_run_id,
                    kind,
                    agent_id: agent_id.clone(),
                    resolved_model: fallback_model.clone(),
                    usage: usage.clone(),
                    estimated_cost_pico_usd: None,
                },
            )
            .unwrap();
            log.append(
                Some(run_id),
                cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
                EventPayload::InternalAgentFallback {
                    invocation_id,
                    internal_run_id,
                    kind,
                    from: InternalAgentBackend::Model {
                        resolved_model: fallback_model.clone(),
                    },
                    to: InternalAgentBackend::Model {
                        resolved_model: resolved_model.clone(),
                    },
                    failure: failure(),
                    attempts: 2,
                },
            )
            .unwrap();
            log.append(
                Some(run_id),
                cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
                EventPayload::InternalAgentUsageRecorded {
                    internal_run_id,
                    kind,
                    agent_id: agent_id.clone(),
                    resolved_model: resolved_model.clone(),
                    usage,
                    estimated_cost_pico_usd: None,
                },
            )
            .unwrap();
        }
        log.append(
            Some(run_id),
            cookie_agent_protocol::EventOrigin::new("engine:test").unwrap(),
            EventPayload::InternalAgentCompleted {
                invocation_id,
                internal_run_id,
                kind,
                result: SafeInternalAgentResult {
                    output_summary: SafeDisplayText::new("validated output").unwrap(),
                    output_digest: Sha256Digest::of_bytes(b"output"),
                },
            },
        )
        .unwrap();
    }
    drop(log);

    let raw = fs::read_to_string(&path).unwrap();
    assert_eq!(
        raw.lines()
            .filter(|line| {
                let value: serde_json::Value = serde_json::from_str(line).unwrap();
                value["payload"]["type"] == "internal_agent_usage_recorded"
                    && value["payload"]["estimated_cost_pico_usd"].is_null()
            })
            .count(),
        5
    );
    let reopened = crate::events::EventLog::open(path, session_id).unwrap();
    let rebuilt = projection(reopened).unwrap();
    assert_eq!(rebuilt.usage_rollup.request_count, 5);
    assert_eq!(rebuilt.usage_rollup.input_tokens, 503);
    for (kind, agent_name) in kinds {
        assert_eq!(
            rebuilt.agent_usage[&AgentId::new(agent_name).unwrap()].request_count,
            if kind == InternalAgentKind::Approval {
                3
            } else {
                1
            }
        );
    }
    let rate = PicoUsdPerMillion::from_decimal_str("1").unwrap();
    let pricing = PricingConfig {
        models: BTreeMap::from([
            (
                resolved_model.selection.model,
                ModelPricing {
                    input_per_million_usd: Some(rate),
                    output_per_million_usd: Some(rate),
                    ..ModelPricing::default()
                },
            ),
            (
                fallback_model.selection.model,
                ModelPricing {
                    input_per_million_usd: Some(rate),
                    output_per_million_usd: Some(rate),
                    ..ModelPricing::default()
                },
            ),
        ]),
    };
    assert_eq!(
        crate::usage::with_pricing(rebuilt.usage_rollup, &pricing, &BTreeMap::new())
            .estimated_cost_usd,
        None
    );
}

struct XorShift(u64);

impl XorShift {
    fn next(&mut self) -> u64 {
        let mut value = self.0;
        value ^= value << 13;
        value ^= value >> 7;
        value ^= value << 17;
        self.0 = value;
        value
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

fn fuzz_origin() -> cookie_agent_protocol::EventOrigin {
    cookie_agent_protocol::EventOrigin::new("engine:fuzz").expect("fuzz origin")
}

/// Extracts the run id, resolved model, agent id, prompt fingerprint, and a
/// reusable RunStarted payload from a session created by
/// `persist_test_session`.
fn fuzz_scaffolding(
    store: &SessionStore,
    session_id: SessionId,
) -> (
    RunId,
    cookie_agent_protocol::ResolvedModelRef,
    AgentId,
    Sha256Digest,
    EventPayload,
) {
    let projection = store.get(session_id).expect("session projection");
    projection
        .log
        .event_snapshot()
        .iter()
        .find_map(|event| match &event.payload {
            EventPayload::RunStarted {
                agent,
                selected_suffix,
                ..
            } => Some((
                event.run_id.expect("run id"),
                crate::model_history::wire_model(selected_suffix.first().expect("model")),
                agent.agent.clone(),
                agent.prompt_fingerprint.clone(),
                event.payload.clone(),
            )),
            _ => None,
        })
        .expect("run started event")
}

fn fuzz_tool_owner(turn_seq: u64, label: &str) -> cookie_agent_protocol::AssistantToolCallRef {
    cookie_agent_protocol::AssistantToolCallRef {
        model_turn_seq: turn_seq,
        content_index: 0,
        model_call_id: cookie_agent_protocol::ModelCallId::new(label).expect("model call id"),
        provider_item_id: None,
    }
}

#[test]
fn fold_ignored_appends_do_not_rebuild_projection() {
    let temporary = private_tempdir();
    let cwd = temporary.path().join("workspace");
    create_private_test_dir_all(&cwd);
    let store = SessionStore::open(&temporary.path().join("data"), &cwd).unwrap();
    let session_id = persist_test_session(&store);
    let (run_id, resolved_model, _, prompt_fingerprint, _) = fuzz_scaffolding(&store, session_id);
    let attempt_id = AttemptId::new_v7();
    store
        .append(
            session_id,
            Some(run_id),
            fuzz_origin(),
            EventPayload::ModelAttemptStarted {
                attempt_id,
                attempt_ordinal: 1,
                fallback_index: 0,
                retry_ordinal: 0,
                resolved_model,
                prompt_fingerprint,
            },
        )
        .expect("start attempt");
    let before = super::projection_fold_count();
    for text in ["delta one", "delta two"] {
        store
            .append(
                session_id,
                Some(run_id),
                fuzz_origin(),
                EventPayload::TextDelta {
                    attempt_id,
                    text: text.into(),
                },
            )
            .expect("text delta");
    }
    store
        .append(
            session_id,
            Some(run_id),
            fuzz_origin(),
            EventPayload::ReasoningDelta {
                attempt_id,
                text: "thinking".into(),
            },
        )
        .expect("reasoning delta");
    assert_eq!(
        super::projection_fold_count(),
        before,
        "fold-ignored appends must not rebuild the projection"
    );
    store
        .append(
            session_id,
            Some(run_id),
            fuzz_origin(),
            EventPayload::RunCompleted { final_text: None },
        )
        .expect("complete run");
    assert_eq!(
        super::projection_fold_count(),
        before + 1,
        "fold-consumed payloads rebuild exactly once"
    );
}

#[test]
fn incremental_projection_matches_full_fold_across_random_event_streams() {
    for seed in [7_u64, 42, 0x5EED_5EED, 999_331] {
        run_projection_fuzz(seed, 400);
    }
}

/// Drives a pseudo-random event stream through a real SessionStore. The
/// `append_with_mode` test assertion re-folds the log after every append
/// and compares it against the resident projection, so each step is a
/// differential check; this test additionally pins that full folds happen
/// exactly on fold-consumed payloads.
fn run_projection_fuzz(seed: u64, steps: usize) {
    let temporary = private_tempdir();
    let cwd = temporary.path().join("workspace");
    create_private_test_dir_all(&cwd);
    let store = SessionStore::open(&temporary.path().join("data"), &cwd).unwrap();
    let session_id = persist_test_session(&store);
    let (mut run_id, resolved_model, agent_id, prompt_fingerprint, run_started) =
        fuzz_scaffolding(&store, session_id);
    let mut attempt_id = AttemptId::new_v7();
    store
        .append(
            session_id,
            Some(run_id),
            fuzz_origin(),
            EventPayload::ModelAttemptStarted {
                attempt_id,
                attempt_ordinal: 1,
                fallback_index: 0,
                retry_ordinal: 0,
                resolved_model: resolved_model.clone(),
                prompt_fingerprint: prompt_fingerprint.clone(),
            },
        )
        .expect("start attempt");

    let mut rng = XorShift(seed | 1);
    let mut next_turn_seq = 1_u64;
    let mut next_attempt_ordinal = 2_u32;
    let mut callable_owners: Vec<cookie_agent_protocol::AssistantToolCallRef> = Vec::new();
    let mut open_tools: Vec<(ToolCallId, cookie_agent_protocol::AssistantToolCallRef)> = Vec::new();
    let mut consumed_appends = 0_u64;
    let folds_before = super::projection_fold_count();
    let append =
        |store: &SessionStore, run: Option<RunId>, payload: EventPayload, consumed: &mut u64| {
            if super::fold_consumed(&payload) {
                *consumed += 1;
            }
            store
                .append(session_id, run, fuzz_origin(), payload)
                .expect("fuzz append");
        };

    for step in 0..steps {
        let roll = rng.below(100);
        match roll {
            // ~45%: streaming text deltas (fold-ignored).
            0..=44 => append(
                &store,
                Some(run_id),
                EventPayload::TextDelta {
                    attempt_id,
                    text: format!("delta-{seed}-{step}"),
                },
                &mut consumed_appends,
            ),
            // ~15%: reasoning deltas (fold-ignored).
            45..=59 => append(
                &store,
                Some(run_id),
                EventPayload::ReasoningDelta {
                    attempt_id,
                    text: format!("reasoning-{seed}-{step}"),
                },
                &mut consumed_appends,
            ),
            // ~15%: tool progress on an open call (fold-ignored).
            60..=74 => {
                if let Some((tool_call_id, _)) = open_tools.first() {
                    append(
                        &store,
                        Some(run_id),
                        EventPayload::ToolCallProgress {
                            tool_call_id: *tool_call_id,
                            message: SafeDisplayText::new("progress").expect("progress"),
                            display: None,
                        },
                        &mut consumed_appends,
                    );
                } else {
                    append(
                        &store,
                        Some(run_id),
                        EventPayload::TextDelta {
                            attempt_id,
                            text: format!("fallback-{seed}-{step}"),
                        },
                        &mut consumed_appends,
                    );
                }
            }
            // ~5%: committed model turn carrying one tool-call part, plus
            // its usage record (consumed). The part gives later tool starts
            // a valid owner.
            75..=79 => {
                let turn_seq = next_turn_seq;
                next_turn_seq += 1;
                let owner = fuzz_tool_owner(turn_seq, &format!("fuzz-mc-{turn_seq}"));
                append(
                    &store,
                    Some(run_id),
                    EventPayload::ModelTurnCommitted {
                        attempt_id,
                        model_turn_seq: turn_seq,
                        resolved_model: resolved_model.clone(),
                        input_through_seq: 1,
                        turn: PersistedModelTurn {
                            content: vec![
                                cookie_agent_protocol::PersistedAssistantPart::ToolCall {
                                    id: owner.model_call_id.clone(),
                                    provider_item_id: None,
                                    name: SafeCode::new("fuzz_tool").expect("tool name"),
                                    input: serde_json::json!({}),
                                    raw_input: None,
                                    metadata: None,
                                },
                            ],
                            provider_options: BTreeMap::new(),
                            finish_reason: ModelFinishReason::ToolCalls,
                            usage: Usage::default(),
                            response_metadata: BTreeMap::new(),
                            provider_metadata: BTreeMap::new(),
                            native_replay: None,
                        },
                        warnings: Vec::new(),
                    },
                    &mut consumed_appends,
                );
                callable_owners.push(owner);
                append(
                    &store,
                    Some(run_id),
                    EventPayload::ModelUsageRecorded {
                        model_turn_seq: turn_seq,
                        agent_id: agent_id.clone(),
                        resolved_model: resolved_model.clone(),
                        usage: Usage::default(),
                        estimated_cost_pico_usd: None,
                    },
                    &mut consumed_appends,
                );
                // A committed turn is terminal for its attempt; stream the
                // next deltas under a fresh attempt.
                attempt_id = AttemptId::new_v7();
                store
                    .append(
                        session_id,
                        Some(run_id),
                        fuzz_origin(),
                        EventPayload::ModelAttemptStarted {
                            attempt_id,
                            attempt_ordinal: next_attempt_ordinal,
                            fallback_index: 0,
                            retry_ordinal: 0,
                            resolved_model: resolved_model.clone(),
                            prompt_fingerprint: prompt_fingerprint.clone(),
                        },
                    )
                    .expect("start attempt");
                next_attempt_ordinal += 1;
            }
            // ~5%: tool call start against a committed tool-call owner
            // (consumed).
            80..=84 => {
                if let Some(owner) = callable_owners.pop() {
                    let tool_call_id = ToolCallId::new_v7();
                    append(
                        &store,
                        Some(run_id),
                        EventPayload::ToolCallStarted {
                            start: cookie_agent_protocol::ToolCallStart {
                                output: Default::default(),
                                tool_call_id,
                                owner: owner.clone(),
                                presentation: cookie_agent_protocol::ToolCallPresentation {
                                    title: SafeDisplayText::new("fuzz tool").expect("title"),
                                    primary_argument: None,
                                },
                                operation_fingerprint: serde_json::from_value(serde_json::json!({
                                    "digest": Sha256Digest::of_bytes(b"fuzz operation")
                                }))
                                .expect("operation fingerprint"),
                            },
                        },
                        &mut consumed_appends,
                    );
                    open_tools.push((tool_call_id, owner));
                } else {
                    append(
                        &store,
                        Some(run_id),
                        EventPayload::TextDelta {
                            attempt_id,
                            text: format!("pre-tool-{seed}-{step}"),
                        },
                        &mut consumed_appends,
                    );
                }
            }
            // ~5%: tool call termination (consumed).
            85..=89 => {
                if let Some((tool_call_id, owner)) = open_tools.pop() {
                    append(
                        &store,
                        Some(run_id),
                        EventPayload::ToolCallTerminated {
                            termination: cookie_agent_protocol::ToolCallTermination {
                                tool_call_id,
                                owner,
                                outcome: cookie_agent_protocol::ToolTerminationOutcome::Failed,
                                result: None,
                                error: Some(cookie_agent_protocol::SafeToolError {
                                    code: SafeCode::new("fuzz_failed").expect("code"),
                                    message: SafeErrorMessage::new("fuzz failed").expect("message"),
                                }),
                            },
                        },
                        &mut consumed_appends,
                    );
                } else {
                    append(
                        &store,
                        Some(run_id),
                        EventPayload::ReasoningDelta {
                            attempt_id,
                            text: format!("no-tool-{seed}-{step}"),
                        },
                        &mut consumed_appends,
                    );
                }
            }
            // ~3%: titles, overlays, user input (all consumed).
            90..=92 => match rng.below(3) {
                0 => append(
                    &store,
                    None,
                    EventPayload::SessionTitleCommitted {
                        change: SessionTitleChange::UserSet {
                            title: SessionTitle::new(format!("fuzz title {step}")).expect("title"),
                            client_rename_id: cookie_agent_protocol::ClientRenameId::new(format!(
                                "fuzz-rename-{seed}-{step}"
                            ))
                            .expect("rename id"),
                        },
                        input_through_seq: 1,
                    },
                    &mut consumed_appends,
                ),
                1 => append(
                    &store,
                    None,
                    EventPayload::SessionPermissionOverlaySet {
                        overlay: SessionPermissionOverlay::default(),
                    },
                    &mut consumed_appends,
                ),
                _ => append(
                    &store,
                    Some(run_id),
                    EventPayload::UserInputSubmitted {
                        input: format!("follow-up {step}"),
                    },
                    &mut consumed_appends,
                ),
            },
            // ~2%: complete the run and start a fresh one (consumed).
            // Tool owners are per-run state; model turn sequences are
            // session-global and stay contiguous across runs and reverts.
            93..=94 => {
                append(
                    &store,
                    Some(run_id),
                    EventPayload::RunCompleted { final_text: None },
                    &mut consumed_appends,
                );
                run_id = RunId::new_v7();
                attempt_id = AttemptId::new_v7();
                next_attempt_ordinal = 2;
                callable_owners.clear();
                open_tools.clear();
                append(
                    &store,
                    Some(run_id),
                    run_started.clone(),
                    &mut consumed_appends,
                );
                store
                    .append(
                        session_id,
                        Some(run_id),
                        fuzz_origin(),
                        EventPayload::ModelAttemptStarted {
                            attempt_id,
                            attempt_ordinal: 1,
                            fallback_index: 0,
                            retry_ordinal: 0,
                            resolved_model: resolved_model.clone(),
                            prompt_fingerprint: prompt_fingerprint.clone(),
                        },
                    )
                    .expect("start attempt");
            }
            // ~2%: revert to the creation event (consumed). Hidden turns
            // invalidate any owners committed before the revert.
            95..=96 => {
                callable_owners.clear();
                open_tools.clear();
                append(
                    &store,
                    None,
                    EventPayload::SessionReverted { through_seq: 1 },
                    &mut consumed_appends,
                );
            }
            // ~3%: user input on the current run (consumed).
            _ => append(
                &store,
                Some(run_id),
                EventPayload::UserInputSubmitted {
                    input: format!("input {seed}-{step}"),
                },
                &mut consumed_appends,
            ),
        }
    }

    assert_eq!(
        super::projection_fold_count() - folds_before,
        consumed_appends,
        "seed {seed}: full folds happen exactly on fold-consumed payloads"
    );
}
