#![cfg(unix)]

use std::{collections::BTreeMap, fs, os::unix::fs::PermissionsExt as _, sync::Arc, thread};

use cookie_agent_identity::{ModelSelection, ProviderId};
use cookie_agent_models::{
    Catalog, CredentialConnectRequest, CredentialStore, CredentialStoreError,
    MODELS_DEV_ARTIFACT_SHA256, ModelSetManager, ModelsDevProvider, ProviderDefinition,
};
use tempfile::TempDir;

fn request(id: &str, provider: &str, secret: &str) -> CredentialConnectRequest {
    CredentialConnectRequest {
        client_connect_id: id.into(),
        provider_id: ProviderId::new(provider).unwrap(),
        catalog_revision: "sha256:test".into(),
        credentials: BTreeMap::from([("API_KEY".into(), secret.into())]),
    }
}

#[test]
fn credential_store_is_private_idempotent_and_redacted() {
    let temp = TempDir::new().unwrap();
    fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let store = CredentialStore::new(temp.path().join("credentials"));
    let request = CredentialConnectRequest {
        client_connect_id: "connect-one".into(),
        provider_id: ProviderId::new("openai").unwrap(),
        catalog_revision: "sha256:test".into(),
        credentials: BTreeMap::from([("OPENAI_API_KEY".into(), "never-print".into())]),
    };
    let first = store
        .connect_with(&request, |_| Ok(("sha256:model".into(), ())))
        .unwrap();
    let replay: cookie_agent_models::CredentialConnectOutcome<()> = store
        .connect_with(&request, |_| panic!("replay must not validate"))
        .unwrap();
    assert!(replay.replayed);
    assert_eq!(first.receipt, replay.receipt);
    assert!(!format!("{request:?}{:?}", store.snapshot().unwrap()).contains("never-print"));
    assert_eq!(
        fs::metadata(store.root()).unwrap().permissions().mode() & 0o777,
        0o700
    );

    let conflict = CredentialConnectRequest {
        credentials: BTreeMap::from([("OPENAI_API_KEY".into(), "changed".into())]),
        ..request
    };
    assert!(matches!(
        store.connect_with(&conflict, |_| Ok(("x".into(), ()))),
        Err(CredentialStoreError::IdempotencyConflict)
    ));
}

#[test]
fn credential_rotation_and_same_fingerprint_publication_retain_exact_executable_snapshot() {
    let temp = TempDir::new().unwrap();
    fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let catalog = Arc::new(Catalog::embedded().unwrap());
    let provider: ModelsDevProvider = toml::from_str(&format!(
        r#"
catalog_revision = "sha256:{MODELS_DEV_ARTIFACT_SHA256}"
auth = {{ type = "credential_store" }}
[models."gpt-5.6-sol"]
"#
    ))
    .unwrap();
    let providers = BTreeMap::from([(
        ProviderId::new("openai").unwrap(),
        ProviderDefinition::ModelsDev(provider),
    )]);
    let manager = ModelSetManager::new(
        providers,
        Arc::clone(&catalog),
        CredentialStore::new(temp.path().join("manager")),
    )
    .unwrap();
    assert!(
        !manager
            .current()
            .model_set()
            .entries()
            .next()
            .unwrap()
            .1
            .is_available()
    );
    assert!(manager.current().model_set().descriptors().is_empty());
    let request = |id: &str, secret: &str| CredentialConnectRequest {
        client_connect_id: id.into(),
        provider_id: ProviderId::new("openai").unwrap(),
        catalog_revision: format!("sha256:{MODELS_DEV_ARTIFACT_SHA256}"),
        credentials: BTreeMap::from([("OPENAI_API_KEY".into(), secret.into())]),
    };
    manager.connect(&request("first", "one")).unwrap();
    let connected = manager.current();
    assert!(
        connected
            .model_set()
            .entries()
            .next()
            .unwrap()
            .1
            .is_available()
    );
    assert_eq!(
        connected
            .model_set()
            .descriptors()
            .into_iter()
            .map(|descriptor| descriptor.key.to_string())
            .collect::<Vec<_>>(),
        ["openai/gpt-5.6-sol"]
    );
    let revision = connected.revision().to_owned();
    let model = connected.model_set().entries().next().unwrap().0.clone();
    let selection = ModelSelection {
        model,
        variant: Some("high".parse().unwrap()),
    };
    let binding = connected.model_set().freeze(&selection).unwrap();
    let before_rotation = connected.resolve(&binding).unwrap();
    let retained = Arc::downgrade(&connected);
    manager.connect(&request("rotate", "two")).unwrap();
    assert_eq!(manager.current().revision(), revision);
    let after_rotation = connected.resolve(&binding).unwrap();
    assert!(Arc::ptr_eq(before_rotation.model(), after_rotation.model()));
    let rotated = manager.current().resolve(&binding).unwrap();
    assert!(!Arc::ptr_eq(before_rotation.model(), rotated.model()));

    let published = manager.refresh().unwrap();
    assert_eq!(published.revision(), revision);
    assert!(!Arc::ptr_eq(&published, &connected));
    assert!(Arc::ptr_eq(
        connected.resolve(&binding).unwrap().model(),
        before_rotation.model()
    ));
    drop(connected);
    assert!(retained.upgrade().is_some());
}

#[test]
fn connect_retains_preexisting_session_model_snapshot_and_binding() {
    let temp = TempDir::new().unwrap();
    fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let catalog = Arc::new(Catalog::embedded().unwrap());
    let openai: ModelsDevProvider = toml::from_str(&format!(
        r#"
catalog_revision = "sha256:{MODELS_DEV_ARTIFACT_SHA256}"
auth = {{ type = "credential_store" }}
[models."gpt-5.6-sol"]
"#
    ))
    .unwrap();
    let explicit: ProviderDefinition = serde_json::from_value(serde_json::json!({
        "source": "explicit",
        "endpoint": "https://example.test/v1",
        "adaptor": "openai-compatible",
        "auth": {"type": "none"},
        "models": {
            "model": {
                "display_name": "Model",
                "capabilities": {
                    "input": ["text"], "output": ["text"],
                    "context_tokens": 8192, "output_tokens": 2048,
                    "tool_calling": true, "parallel_tool_calls": false,
                    "structured_output": false, "reasoning": false,
                    "temperature": true, "top_p": true, "seed": true,
                    "native_replay": "unsupported",
                    "native_compaction": "unsupported",
                    "cancellation": "local_only", "media": {}
                }
            }
        }
    }))
    .unwrap();
    let providers = BTreeMap::from([
        (
            ProviderId::new("openai").unwrap(),
            ProviderDefinition::ModelsDev(openai),
        ),
        (ProviderId::new("test").unwrap(), explicit),
    ]);
    let manager = ModelSetManager::new(
        providers,
        catalog,
        CredentialStore::new(temp.path().join("retained")),
    )
    .unwrap();
    let initial = manager.current();
    let selection = ModelSelection {
        model: "test/model".parse().unwrap(),
        variant: None,
    };
    let binding = initial.model_set().freeze(&selection).unwrap();
    manager
        .connect(&CredentialConnectRequest {
            client_connect_id: "connect-openai".into(),
            provider_id: ProviderId::new("openai").unwrap(),
            catalog_revision: format!("sha256:{MODELS_DEV_ARTIFACT_SHA256}"),
            credentials: BTreeMap::from([("OPENAI_API_KEY".into(), "secret".into())]),
        })
        .unwrap();
    assert_ne!(manager.current().revision(), initial.revision());
    assert!(
        manager
            .snapshot(initial.model_set().fingerprint())
            .is_some()
    );
    assert_eq!(initial.resolve(&binding).unwrap().selection(), &selection);
}

#[test]
fn credential_store_rejects_links_weak_modes_and_unexpected_types() {
    let temp = TempDir::new().unwrap();
    fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let target = temp.path().join("target");
    fs::create_dir(&target).unwrap();
    fs::set_permissions(&target, fs::Permissions::from_mode(0o700)).unwrap();
    let linked = temp.path().join("linked");
    std::os::unix::fs::symlink(&target, &linked).unwrap();
    assert!(matches!(
        CredentialStore::new(linked).snapshot(),
        Err(CredentialStoreError::UnsafePath)
    ));

    let store = CredentialStore::new(temp.path().join("real"));
    store
        .connect_with(&request("one", "provider", "secret"), |_| {
            Ok(("revision".into(), ()))
        })
        .unwrap();
    let state = store.root().join("store-v1.json");
    fs::set_permissions(&state, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(matches!(
        store.snapshot(),
        Err(CredentialStoreError::UnsafePath)
    ));
    fs::set_permissions(&state, fs::Permissions::from_mode(0o600)).unwrap();
    fs::hard_link(&state, store.root().join("hardlink.json")).unwrap();
    assert!(matches!(
        store.snapshot(),
        Err(CredentialStoreError::UnsafePath)
    ));
}

#[test]
fn concurrent_credential_transactions_do_not_clobber_providers() {
    let temp = TempDir::new().unwrap();
    fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let store = CredentialStore::new(temp.path().join("concurrent"));
    let threads = ["first", "second"].map(|provider| {
        let store = store.clone();
        thread::spawn(move || {
            store
                .connect_with(&request(provider, provider, "secret"), |_| {
                    Ok((provider.into(), ()))
                })
                .unwrap();
        })
    });
    for thread in threads {
        thread.join().unwrap();
    }
    let debug = format!("{:?}", store.snapshot().unwrap());
    assert!(debug.contains("first"));
    assert!(debug.contains("second"));
    assert!(!debug.contains("secret"));
}
