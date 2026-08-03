#![cfg(unix)]

use std::{
    collections::BTreeMap,
    env, fs,
    os::unix::fs::{MetadataExt as _, PermissionsExt as _},
    path::Path,
    sync::{
        Arc, Mutex, MutexGuard, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    thread,
};

use cookie_agent_models::{
    Catalog, ConfiguredModel, CredentialConnectRequest, CredentialStore, CredentialStoreError,
    ModelSetManager, ModelSetManagerError,
};
use tempfile::TempDir;

fn private_temp() -> TempDir {
    let temp = TempDir::new().unwrap();
    fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o700)).unwrap();
    temp
}

struct ProcessEnvironment {
    _guard: MutexGuard<'static, ()>,
    prior_home: Option<std::ffi::OsString>,
    prior_umask: libc::mode_t,
}

impl ProcessEnvironment {
    fn new(home: &Path, umask: libc::mode_t) -> Self {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        let guard = LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let prior_home = env::var_os("HOME");
        unsafe { env::set_var("HOME", home) };
        let prior_umask = unsafe { libc::umask(umask) };
        Self {
            _guard: guard,
            prior_home,
            prior_umask,
        }
    }
}

impl Drop for ProcessEnvironment {
    fn drop(&mut self) {
        unsafe {
            libc::umask(self.prior_umask);
            match self.prior_home.take() {
                Some(home) => env::set_var("HOME", home),
                None => env::remove_var("HOME"),
            }
        }
    }
}

fn request(
    catalog: &Catalog,
    id: &str,
    provider_id: &str,
    secret: &str,
) -> CredentialConnectRequest {
    let field = catalog.providers()[provider_id].credential_fields[0].clone();
    CredentialConnectRequest {
        client_connect_id: id.into(),
        provider_id: provider_id.into(),
        catalog_revision: catalog.revision().into(),
        credentials: BTreeMap::from([(field, secret.into())]),
    }
}

#[test]
fn credential_store_standard_startup_creates_private_layout() {
    let home = private_temp();
    let _environment = ProcessEnvironment::new(home.path(), 0o002);

    CredentialStore::standard().unwrap().snapshot().unwrap();

    assert_eq!(
        fs::metadata(home.path().join(".local/share/cookie_agent"))
            .unwrap()
            .mode()
            & 0o777,
        0o700
    );
    assert_eq!(
        fs::metadata(home.path().join(".local/share/cookie_agent/credentials"))
            .unwrap()
            .mode()
            & 0o777,
        0o700
    );
}

#[test]
fn credential_store_is_private_durable_and_idempotent_without_receipt_secrets() {
    let temp = private_temp();
    let root = temp.path().join("credentials");
    let store = CredentialStore::new(root.clone());
    let catalog = Catalog::embedded().unwrap();
    let request = request(&catalog, "connect-one", "anthropic", "never-in-receipt");
    let first = store
        .connect_with(&request, |_| Ok(("sha256:one".into(), ())))
        .unwrap();
    assert!(!first.replayed);
    let replay: cookie_agent_models::CredentialConnectOutcome<()> = store
        .connect_with(&request, |_| {
            panic!("idempotent replay must not revalidate")
        })
        .unwrap();
    assert!(replay.replayed);
    assert_eq!(first.receipt, replay.receipt);

    let conflict = CredentialConnectRequest {
        credentials: BTreeMap::from([(
            catalog.providers()["anthropic"].credential_fields[0].clone(),
            "changed".into(),
        )]),
        ..request
    };
    assert!(matches!(
        store.connect_with(&conflict, |_| Ok(("unused".into(), ()))),
        Err(CredentialStoreError::IdempotencyConflict)
    ));

    let root_metadata = fs::metadata(&root).unwrap();
    let store_path = root.join("store-v1.json");
    let lock_path = root.join("store-v1.lock");
    assert_eq!(root_metadata.mode() & 0o777, 0o700);
    assert_eq!(fs::metadata(&store_path).unwrap().mode() & 0o777, 0o600);
    assert_eq!(fs::metadata(&lock_path).unwrap().mode() & 0o777, 0o600);
    let document: serde_json::Value =
        serde_json::from_slice(&fs::read(store_path).unwrap()).unwrap();
    let receipt_text = serde_json::to_string(&document["receipts"]).unwrap();
    assert!(!receipt_text.contains("never-in-receipt"));
    assert_eq!(
        document["connections"]["anthropic"]["credentials"]
            .as_object()
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn credential_store_rejects_symlinks_and_concurrent_updates_do_not_clobber() {
    let temp = private_temp();
    let target = temp.path().join("target");
    fs::create_dir(&target).unwrap();
    let linked = temp.path().join("linked");
    std::os::unix::fs::symlink(&target, &linked).unwrap();
    assert!(matches!(
        CredentialStore::new(linked).snapshot(),
        Err(CredentialStoreError::UnsafePath)
    ));

    let catalog = Arc::new(Catalog::embedded().unwrap());
    let store = CredentialStore::new(temp.path().join("real"));
    let threads = ["anthropic", "cohere"].map(|provider| {
        let catalog = Arc::clone(&catalog);
        let store = store.clone();
        thread::spawn(move || {
            let request = request(&catalog, provider, provider, "secret");
            store
                .connect_with(&request, |_| Ok((format!("sha256:{provider}"), ())))
                .unwrap();
        })
    });
    for thread in threads {
        thread.join().unwrap();
    }
    let snapshot = store.snapshot().unwrap();
    assert_eq!(snapshot.connections().len(), 2);
}

#[test]
fn credential_store_rejects_symlinked_anchor_and_managed_ancestor_components() {
    let temp = private_temp();
    let real_anchor = temp.path().join("real-anchor");
    fs::create_dir(&real_anchor).unwrap();
    fs::set_permissions(&real_anchor, fs::Permissions::from_mode(0o700)).unwrap();
    let linked_anchor = temp.path().join("linked-anchor");
    std::os::unix::fs::symlink(&real_anchor, &linked_anchor).unwrap();
    assert!(matches!(
        CredentialStore::new_in(linked_anchor, "credentials".into()).snapshot(),
        Err(CredentialStoreError::UnsafePath)
    ));

    let target = temp.path().join("target");
    fs::create_dir(&target).unwrap();
    fs::set_permissions(&target, fs::Permissions::from_mode(0o700)).unwrap();
    std::os::unix::fs::symlink(&target, temp.path().join("managed")).unwrap();
    assert!(matches!(
        CredentialStore::new_in(temp.path().to_owned(), "managed/credentials".into()).snapshot(),
        Err(CredentialStoreError::UnsafePath)
    ));
    assert_eq!(fs::read_dir(target).unwrap().count(), 0);
}

#[test]
fn credential_store_ancestor_replacement_race_never_redirects_writes() {
    let temp = private_temp();
    let store = CredentialStore::new_in(temp.path().to_owned(), "managed/credentials".into());
    store.snapshot().unwrap();

    let managed = temp.path().join("managed");
    let parked = temp.path().join("managed-parked");
    let target = temp.path().join("attacker-target");
    fs::create_dir(&target).unwrap();
    fs::set_permissions(&target, fs::Permissions::from_mode(0o700)).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let attacker_stop = Arc::clone(&stop);
    let attacker = thread::spawn(move || {
        while !attacker_stop.load(Ordering::Relaxed) {
            if fs::rename(&managed, &parked).is_ok() {
                let _ = std::os::unix::fs::symlink(&target, &managed);
                thread::yield_now();
                let _ = fs::remove_file(&managed);
                let _ = fs::rename(&parked, &managed);
            } else {
                thread::yield_now();
            }
        }
        let _ = fs::remove_file(&managed);
        if parked.exists() {
            let _ = fs::rename(&parked, &managed);
        }
        (managed, target)
    });

    for index in 0..32 {
        let request = CredentialConnectRequest {
            client_connect_id: format!("race-{index}"),
            provider_id: "race-provider".into(),
            catalog_revision: "sha256:race".into(),
            credentials: BTreeMap::from([("RACE_KEY".into(), "race-secret".into())]),
        };
        match store.connect_with(&request, |_| Ok((format!("sha256:{index}"), ()))) {
            Ok(_) | Err(CredentialStoreError::UnsafePath | CredentialStoreError::Io(_)) => {}
            Err(error) => panic!("unexpected fail-closed race result: {error}"),
        }
    }
    stop.store(true, Ordering::Relaxed);
    let (_managed, target) = attacker.join().unwrap();
    assert_eq!(fs::read_dir(target).unwrap().count(), 0);
}

#[test]
fn manager_publishes_atomically_preserves_revision_on_secret_rotation_and_resolves_frozen() {
    let temp = private_temp();
    let catalog = Arc::new(Catalog::embedded().unwrap());
    let manager = Arc::new(
        ModelSetManager::new(
            BTreeMap::new(),
            Arc::clone(&catalog),
            CredentialStore::new(temp.path().join("manager")),
        )
        .unwrap(),
    );
    let before = manager.current();
    assert_eq!(before.model_set().aliases().len(), 0);

    manager
        .connect(&request(&catalog, "first", "anthropic", "secret-one"))
        .unwrap();
    let connected = manager.current();
    let alias = connected
        .model_set()
        .aliases()
        .find(|alias| alias.starts_with("anthropic/"))
        .unwrap()
        .to_owned();
    let frozen = connected.model_set().freeze(&alias).unwrap();
    let first_adapter = connected.model_set().get(&alias).unwrap().clone();
    let first_revision = connected.revision().to_owned();

    manager
        .connect(&request(&catalog, "add-cohere", "cohere", "cohere-secret"))
        .unwrap();
    let with_cohere = manager.current();
    assert_ne!(with_cohere.revision(), first_revision);
    let current_before_rotation = with_cohere.model_set().get(&alias).unwrap();
    let rebound_before_rotation = manager.resolve_frozen(&frozen).unwrap();
    assert!(Arc::ptr_eq(
        rebound_before_rotation.model(),
        current_before_rotation.model()
    ));
    assert!(!Arc::ptr_eq(
        rebound_before_rotation.model(),
        first_adapter.model()
    ));
    let latest_frozen = with_cohere.model_set().freeze(&alias).unwrap();
    let latest_revision = with_cohere.revision().to_owned();

    manager
        .connect(&request(&catalog, "rotate", "anthropic", "secret-two"))
        .unwrap();
    let rotated = manager.current();
    assert_eq!(rotated.revision(), latest_revision);
    let rotated_current = rotated.model_set().get(&alias).unwrap();
    let rebound_after_rotation = manager.resolve_frozen(&frozen).unwrap();
    assert!(Arc::ptr_eq(
        rebound_after_rotation.model(),
        rotated_current.model()
    ));
    assert!(!Arc::ptr_eq(
        rebound_after_rotation.model(),
        current_before_rotation.model()
    ));
    assert!(Arc::ptr_eq(
        manager.resolve_frozen(&latest_frozen).unwrap().model(),
        rotated_current.model()
    ));

    let rotated_adapter = rotated_current.clone();
    let refreshed = manager.refresh().unwrap();
    assert_eq!(refreshed.revision(), latest_revision);
    let refreshed_current = refreshed.model_set().get(&alias).unwrap();
    assert!(Arc::ptr_eq(
        manager.resolve_frozen(&frozen).unwrap().model(),
        refreshed_current.model()
    ));
    assert!(!Arc::ptr_eq(
        rotated_adapter.model(),
        refreshed_current.model()
    ));
}

#[test]
fn manager_restart_revokes_obsolete_fingerprints_but_keeps_current_behavior_binding() {
    let temp = private_temp();
    let catalog = Arc::new(Catalog::embedded().unwrap());
    let store = CredentialStore::new(temp.path().join("restart"));
    let manager =
        ModelSetManager::new(BTreeMap::new(), Arc::clone(&catalog), store.clone()).unwrap();
    manager
        .connect(&request(
            &catalog,
            "restart-first",
            "anthropic",
            "secret-one",
        ))
        .unwrap();
    let first = manager.current();
    let alias = first
        .model_set()
        .aliases()
        .find(|alias| alias.starts_with("anthropic/"))
        .unwrap()
        .to_owned();
    let obsolete = first.model_set().freeze(&alias).unwrap();
    manager
        .connect(&request(
            &catalog,
            "restart-add-cohere",
            "cohere",
            "cohere-secret",
        ))
        .unwrap();
    let latest = manager.current().model_set().freeze(&alias).unwrap();
    manager
        .connect(&request(
            &catalog,
            "restart-rotate",
            "anthropic",
            "secret-two",
        ))
        .unwrap();
    let obsolete_bytes = serde_json::to_vec(&obsolete).unwrap();
    let latest_bytes = serde_json::to_vec(&latest).unwrap();
    drop(manager);

    let restarted = ModelSetManager::new(BTreeMap::new(), Arc::clone(&catalog), store).unwrap();
    let obsolete: cookie_agent_models::FrozenModelBinding =
        serde_json::from_slice(&obsolete_bytes).unwrap();
    assert!(matches!(
        restarted.resolve_frozen(&obsolete),
        Err(ModelSetManagerError::RetainedSnapshotNotFound)
    ));
    let latest: cookie_agent_models::FrozenModelBinding =
        serde_json::from_slice(&latest_bytes).unwrap();
    let resolved = restarted.resolve_frozen(&latest).unwrap();
    assert!(Arc::ptr_eq(
        resolved.model(),
        restarted.current().model_set().get(&alias).unwrap().model()
    ));
}

#[test]
fn manager_rejects_static_catalog_collisions_before_durable_store_or_publication() {
    let configured: ConfiguredModel = toml::from_str(
        r#"
provider_id = "static"
model_id = "static-model"
endpoint = "https://example.test/v1"
adaptor = "openai-compatible"

[auth]
type = "none"

[capabilities]
features = []
cancellation = "local_only"
compaction = "unsupported"

[capabilities.limits]

[capabilities.modalities]
input = ["text"]
output = ["text"]

[capabilities.media]
input = {}

[capabilities.replay]
policy = "never"
capability = "unsupported"
reasoning = false

[settings]
adapter_id = "cookie.static.chat"
system_message_role = "system"
max_tokens_field = "max_tokens"
stream_usage = false
structured_output = "unsupported"
reasoning_field = "none"
"#,
    )
    .unwrap();
    let temp = private_temp();
    let catalog = Arc::new(Catalog::embedded().unwrap());
    let store = CredentialStore::new(temp.path().join("collision"));
    let manager = ModelSetManager::new(
        BTreeMap::from([("anthropic/claude-opus-4-6".into(), configured)]),
        Arc::clone(&catalog),
        store.clone(),
    )
    .unwrap();
    let before = manager.current();
    let error = manager
        .connect(&request(
            &catalog,
            "collision",
            "anthropic",
            "collision-secret",
        ))
        .unwrap_err();
    assert!(matches!(
        error,
        ModelSetManagerError::StaticAliasCollision(_)
    ));
    assert_eq!(manager.current().revision(), before.revision());
    assert!(store.snapshot().unwrap().connections().is_empty());
    assert!(!format!("{error:?}").contains("collision-secret"));
}
