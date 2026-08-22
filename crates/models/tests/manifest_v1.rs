#![cfg(unix)]

use std::{
    fs,
    os::unix::fs::{MetadataExt as _, PermissionsExt as _, symlink},
};

use cookie_agent_identity::{
    CatalogRevision, ModelRevision, ModelSnapshotRevision, ProviderStateRevision,
    RecipeRegistryRevision,
};
use cookie_agent_models::manifests::{
    ManifestError, ModelSnapshotManifestStore, ModelSnapshotPayloadV1,
};
use sha2::{Digest as _, Sha256};
use tempfile::TempDir;

fn revision<T, E: std::fmt::Debug>(
    label: &str,
    constructor: impl FnOnce(String) -> Result<T, E>,
) -> T {
    constructor(format!("sha256:{:x}", Sha256::digest(label.as_bytes()))).unwrap()
}

fn payload() -> ModelSnapshotPayloadV1 {
    ModelSnapshotPayloadV1 {
        catalog_revision: revision("catalog", CatalogRevision::new),
        recipe_registry_revision: revision("recipes", RecipeRegistryRevision::new),
        provider_state_revision: revision("providers", ProviderStateRevision::new),
        model_revision: revision("models", ModelRevision::new),
        blueprints: Vec::new(),
    }
}

fn private_store(temporary: &TempDir) -> ModelSnapshotManifestStore {
    fs::set_permissions(temporary.path(), fs::Permissions::from_mode(0o700)).unwrap();
    ModelSnapshotManifestStore::open_directory(temporary.path().join("model-snapshots")).unwrap()
}

fn manifest_path(
    store: &ModelSnapshotManifestStore,
    revision: &ModelSnapshotRevision,
) -> std::path::PathBuf {
    store
        .path()
        .join(format!("{}.json", &revision.as_str()["sha256:".len()..]))
}

fn directory_fingerprint(path: &std::path::Path) -> Vec<(String, u32, u64, String)> {
    let mut entries = fs::read_dir(path)
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            let metadata = entry.metadata().unwrap();
            let bytes = if metadata.is_file() {
                fs::read(entry.path()).unwrap()
            } else {
                Vec::new()
            };
            (
                entry.file_name().into_string().unwrap(),
                metadata.permissions().mode() & 0o777,
                metadata.len(),
                format!("{:x}", Sha256::digest(bytes)),
            )
        })
        .collect::<Vec<_>>();
    entries.sort();
    entries
}

#[test]
fn shared_project_anchor_modes_create_and_reopen_private_storage() {
    for mode in [0o775, 0o777] {
        let temporary = TempDir::new().unwrap();
        fs::set_permissions(temporary.path(), fs::Permissions::from_mode(mode)).unwrap();

        let manifest = ModelSnapshotManifestStore::open(temporary.path())
            .unwrap()
            .write(payload())
            .unwrap();
        let project = temporary.path().join(".cookie-agent");
        let snapshots = project.join("model-snapshots");
        assert_eq!(fs::metadata(&project).unwrap().mode() & 0o777, 0o700);
        assert_eq!(fs::metadata(&snapshots).unwrap().mode() & 0o777, 0o700);

        let reopened = ModelSnapshotManifestStore::open(temporary.path()).unwrap();
        assert!(reopened.scan().unwrap().get(&manifest.revision).is_some());
        for entry in fs::read_dir(&snapshots).unwrap() {
            let metadata = entry.unwrap().metadata().unwrap();
            assert_eq!(metadata.mode() & 0o777, 0o600);
            assert_eq!(metadata.nlink(), 1);
        }
    }
}

#[test]
fn symlink_anchors_and_storage_children_are_used() {
    let temporary = TempDir::new().unwrap();
    fs::set_permissions(temporary.path(), fs::Permissions::from_mode(0o777)).unwrap();
    let actual = temporary.path().join("actual");
    fs::create_dir(&actual).unwrap();
    fs::set_permissions(&actual, fs::Permissions::from_mode(0o777)).unwrap();
    let linked = temporary.path().join("linked");
    symlink(&actual, &linked).unwrap();
    ModelSnapshotManifestStore::open(&linked)
        .unwrap()
        .write(payload())
        .unwrap();

    let second = TempDir::new().unwrap();
    let second_project = second.path().join("project");
    let target = second.path().join("target");
    fs::create_dir(&second_project).unwrap();
    fs::create_dir(&target).unwrap();
    symlink(&target, second_project.join(".cookie-agent")).unwrap();
    ModelSnapshotManifestStore::open(&second_project)
        .unwrap()
        .write(payload())
        .unwrap();

    fs::remove_file(second_project.join(".cookie-agent")).unwrap();
    fs::write(second_project.join(".cookie-agent"), b"not a directory").unwrap();
    assert!(matches!(
        ModelSnapshotManifestStore::open(&second_project),
        Err(ManifestError::Storage(_))
    ));
}

#[test]
fn non_private_or_wrong_owner_storage_children_are_used() {
    let temporary = TempDir::new().unwrap();
    fs::set_permissions(temporary.path(), fs::Permissions::from_mode(0o777)).unwrap();
    let project = temporary.path().join(".cookie-agent");
    fs::create_dir(&project).unwrap();
    fs::set_permissions(&project, fs::Permissions::from_mode(0o755)).unwrap();
    ModelSnapshotManifestStore::open(temporary.path()).unwrap();

    fs::set_permissions(&project, fs::Permissions::from_mode(0o700)).unwrap();
    if unsafe { libc::geteuid() } == 0 {
        let path = std::ffi::CString::new(project.as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::chown(path.as_ptr(), 1, 1) }, 0);
        ModelSnapshotManifestStore::open(temporary.path()).unwrap();
    }
}

#[test]
fn non_owned_project_anchor_is_accepted_when_simulation_is_available() {
    let temporary = TempDir::new().unwrap();
    fs::set_permissions(temporary.path(), fs::Permissions::from_mode(0o777)).unwrap();
    if unsafe { libc::geteuid() } == 0 {
        let path = std::ffi::CString::new(temporary.path().as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::chown(path.as_ptr(), 1, 1) }, 0);
        let store = ModelSnapshotManifestStore::open(temporary.path()).unwrap();
        assert_eq!(store.scan().unwrap().len(), 0);
        assert_eq!(fs::metadata(store.path()).unwrap().uid(), unsafe {
            libc::geteuid()
        });
    }
}

#[test]
fn durable_manifest_round_trip_and_jcs_property_reordering_are_equivalent() {
    let temporary = TempDir::new().unwrap();
    let store = private_store(&temporary);
    let manifest = store.write(payload()).unwrap();
    let path = manifest_path(&store, &manifest.revision);
    let payload = &manifest.payload;
    let reordered = format!(
        "{{\"payload\":{{\"model_revision\":\"{}\",\"blueprints\":[],\"provider_state_revision\":\"{}\",\"catalog_revision\":\"{}\",\"recipe_registry_revision\":\"{}\"}},\"revision\":\"{}\",\"schema_version\":1}}",
        payload.model_revision,
        payload.provider_state_revision,
        payload.catalog_revision,
        payload.recipe_registry_revision,
        manifest.revision,
    );
    fs::write(&path, reordered).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    let index = store.scan().unwrap();
    assert_eq!(index.len(), 1);
    assert_eq!(
        index.require(&manifest.revision).unwrap().payload,
        manifest.payload
    );
}

#[test]
fn missing_corrupt_and_revision_mismatched_manifests_fail_closed() {
    let temporary = TempDir::new().unwrap();
    let store = private_store(&temporary);
    let missing = revision("missing", ModelSnapshotRevision::new);
    assert!(matches!(
        store.scan().unwrap().require(&missing),
        Err(ManifestError::MissingModelSnapshotManifest)
    ));

    let manifest = store.write(payload()).unwrap();
    let path = manifest_path(&store, &manifest.revision);
    let mut value: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    value["payload"]["model_revision"] = serde_json::Value::String(
        revision::<ModelRevision, _>("different", ModelRevision::new).into_string(),
    );
    fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    assert!(matches!(
        store.scan(),
        Err(ManifestError::ModelSnapshotDigestMismatch)
    ));

    fs::write(&path, b"{not-json").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    assert!(matches!(
        store.scan(),
        Err(ManifestError::InvalidModelSnapshotManifest)
    ));
}

#[test]
fn matching_manifest_symlink_hardlink_and_wrong_mode_are_used() {
    let temporary = TempDir::new().unwrap();
    let store = private_store(&temporary);
    let manifest = store.write(payload()).unwrap();
    let attack = manifest_path(&store, &manifest.revision);
    let target = store.path().join("target");
    fs::rename(&attack, &target).unwrap();
    symlink(&target, &attack).unwrap();
    assert!(store.scan().unwrap().get(&manifest.revision).is_some());
    fs::remove_file(&attack).unwrap();

    fs::hard_link(&target, &attack).unwrap();
    assert!(store.scan().unwrap().get(&manifest.revision).is_some());

    fs::set_permissions(&attack, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(store.scan().unwrap().get(&manifest.revision).is_some());
}

#[test]
fn floats_unsafe_integers_duplicates_and_old_names_are_current_only() {
    let temporary = TempDir::new().unwrap();
    let store = private_store(&temporary);
    fs::write(store.path().join("snapshot.json"), b"legacy is ignored").unwrap();
    fs::set_permissions(
        store.path().join("snapshot.json"),
        fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    assert!(store.scan().unwrap().is_empty());

    for (name, bytes) in [
        (
            format!("{}.json", "b".repeat(64)),
            br#"{"schema_version":1.0,"revision":"sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","payload":{}}"#.as_slice(),
        ),
        (
            format!("{}.json", "c".repeat(64)),
            br#"{"schema_version":1,"schema_version":1,"revision":"sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc","payload":{}}"#.as_slice(),
        ),
        (
            format!("{}.json", "d".repeat(64)),
            br#"{"schema_version":9007199254740992,"revision":"sha256:dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd","payload":{}}"#.as_slice(),
        ),
    ] {
        let path = store.path().join(name);
        fs::write(&path, bytes).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(matches!(
            store.scan(),
            Err(ManifestError::InvalidModelSnapshotManifest)
        ));
        fs::remove_file(path).unwrap();
    }
}

#[test]
fn preparation_validates_existing_index_before_write_and_returns_resulting_index() {
    let temporary = TempDir::new().unwrap();
    let store = private_store(&temporary);
    let first = store.prepare(payload()).unwrap();
    assert_eq!(first.index.len(), 1);
    assert!(first.index.get(&first.manifest.revision).is_some());

    let malformed = store.path().join(format!("{}.json", "e".repeat(64)));
    fs::write(&malformed, b"{malformed").unwrap();
    fs::set_permissions(&malformed, fs::Permissions::from_mode(0o600)).unwrap();
    let before = directory_fingerprint(store.path());
    let mut missing_payload = payload();
    missing_payload.model_revision = revision("different-models", ModelRevision::new);
    assert!(matches!(
        store.prepare(missing_payload),
        Err(ManifestError::InvalidModelSnapshotManifest)
    ));
    assert_eq!(directory_fingerprint(store.path()), before);
}
