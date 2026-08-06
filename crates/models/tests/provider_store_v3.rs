#![cfg(unix)]

use std::{
    collections::BTreeMap,
    fs,
    os::unix::fs::PermissionsExt as _,
    sync::{Arc, Barrier},
    thread,
};

use cookie_agent_identity::{
    AuthFieldName, AuthMethodId, CatalogRevision, ProviderId, ProviderModelId,
    ProviderSetupRecipeId, RecipeCompilerVersion, RuntimeRevision, SafeCode, SetupFieldId,
};
use cookie_agent_models::{
    SafeSetupValue, Sha256Digest,
    provider_store::{
        ClientConnectId, ClientRequestId, ConnectMutation, ConnectProposal, DisconnectMutation,
        DisconnectProposal, ProviderAuthValues, ProviderConnectionGeneration, ProviderStore,
        ProviderStoreError, ProviderStoreMutation, SafePolicyString, SafePolicyValue,
        StoredModelOverrideProjection, StoredProviderPolicyProjection,
    },
};
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use tempfile::TempDir;

fn revision<T, E: std::fmt::Debug>(
    label: &str,
    constructor: impl FnOnce(String) -> Result<T, E>,
) -> T {
    let digest = Sha256Digest::new(format!(
        "{:064x}",
        label.bytes().map(u64::from).sum::<u64>()
    ))
    .unwrap();
    constructor(format!("sha256:{}", digest.as_str())).unwrap()
}

fn catalog_revision() -> CatalogRevision {
    revision("catalog", CatalogRevision::new)
}

fn runtime_revision() -> RuntimeRevision {
    revision("runtime", RuntimeRevision::new)
}

fn private_store(temporary: &TempDir) -> ProviderStore {
    fs::set_permissions(temporary.path(), fs::Permissions::from_mode(0o700)).unwrap();
    ProviderStore::open(temporary.path().join("providers")).unwrap()
}

fn read_store(store: &ProviderStore) -> Value {
    serde_json::from_slice(&fs::read(store.path().join("store-v3.json")).unwrap()).unwrap()
}

fn digest_jcs(value: &Value) -> String {
    let mut bytes = Vec::new();
    write_test_jcs(value, &mut bytes);
    format!("{:x}", Sha256::digest(bytes))
}

fn write_test_jcs(value: &Value, output: &mut Vec<u8>) {
    match value {
        Value::Null => output.extend_from_slice(b"null"),
        Value::Bool(true) => output.extend_from_slice(b"true"),
        Value::Bool(false) => output.extend_from_slice(b"false"),
        Value::Number(number) => output.extend_from_slice(number.to_string().as_bytes()),
        Value::String(value) => {
            output.extend_from_slice(serde_json::to_string(value).unwrap().as_bytes());
        }
        Value::Array(values) => {
            output.push(b'[');
            for (index, value) in values.iter().enumerate() {
                if index > 0 {
                    output.push(b',');
                }
                write_test_jcs(value, output);
            }
            output.push(b']');
        }
        Value::Object(values) => {
            let mut entries = values.iter().collect::<Vec<_>>();
            entries.sort_by(|(left, _), (right, _)| left.encode_utf16().cmp(right.encode_utf16()));
            output.push(b'{');
            for (index, (key, value)) in entries.into_iter().enumerate() {
                if index > 0 {
                    output.push(b',');
                }
                output.extend_from_slice(serde_json::to_string(key).unwrap().as_bytes());
                output.push(b':');
                write_test_jcs(value, output);
            }
            output.push(b'}');
        }
    }
}

fn revision_projection(mut state: Value) -> Value {
    let root = state.as_object_mut().unwrap();
    root.remove("store_revision");
    for receipt in root
        .get_mut("connect_receipts")
        .unwrap()
        .as_object_mut()
        .unwrap()
        .values_mut()
    {
        let receipt = receipt.as_object_mut().unwrap();
        remove_receipt_revisions(receipt.get_mut("result").unwrap());
    }
    for receipt in root
        .get_mut("disconnect_receipts")
        .unwrap()
        .as_object_mut()
        .unwrap()
        .values_mut()
    {
        remove_receipt_revisions(receipt.get_mut("result").unwrap());
    }
    state
}

fn remove_receipt_revisions(result: &mut Value) {
    let receipt = result
        .as_object_mut()
        .unwrap()
        .get_mut("durable_receipt")
        .unwrap()
        .as_object_mut()
        .unwrap();
    receipt.remove("store_revision");
    receipt.remove("provider_state_revision");
}

fn policy(catalog_revision: &CatalogRevision, provider: &str) -> StoredProviderPolicyProjection {
    StoredProviderPolicyProjection {
        catalog_revision: catalog_revision.clone(),
        family_id: SafePolicyString::new(format!("{provider}.v1")).unwrap(),
        setup_recipe: ProviderSetupRecipeId::new(format!("{provider}.setup.v1")).unwrap(),
        adapter_id: SafePolicyString::new(format!("{provider}.protocol.v1")).unwrap(),
        compiler_version: RecipeCompilerVersion::new("family-registry.v1").unwrap(),
        default_endpoint_identity: SafePolicyString::new(format!("https://{provider}.example/v1"))
            .unwrap(),
        package_claim: SafePolicyString::new(format!("@reviewed/{provider}")).unwrap(),
        source_record_digest: Sha256Digest::new(format!(
            "{:064x}",
            provider.bytes().map(u64::from).sum::<u64>()
        ))
        .unwrap(),
        recipe_fingerprint: Sha256Digest::new(format!(
            "{:x}",
            Sha256::digest(format!("test/provider-recipe:{provider}"))
        ))
        .unwrap(),
        model_overrides: BTreeMap::new(),
    }
}

fn connect_request(
    snapshot: &cookie_agent_models::provider_store::ProviderStoreSnapshot,
    id: &str,
    provider: &str,
    secret: &str,
) -> ConnectMutation {
    let catalog_revision = catalog_revision();
    ConnectMutation {
        client_connect_id: ClientConnectId::new(id).unwrap(),
        provider_id: ProviderId::new(provider).unwrap(),
        expected_catalog_revision: catalog_revision.clone(),
        expectation: snapshot.expectation(),
        setup_values: BTreeMap::from([(
            SetupFieldId::new("region").unwrap(),
            SafeSetupValue::Code(SafeCode::new("us-test-1").unwrap()),
        )]),
        auth_method: AuthMethodId::new("bearer-api-key-v1").unwrap(),
        auth_values: ProviderAuthValues::new(BTreeMap::from([(
            AuthFieldName::new("api_key").unwrap(),
            secret.to_owned(),
        )]))
        .unwrap(),
        policy: policy(&catalog_revision, provider),
    }
}

fn commit_connect(store: &ProviderStore, request: &ConnectMutation) -> ProviderStoreMutation {
    let transaction = store.begin_transaction().unwrap();
    let proposal = match transaction
        .propose_connect(request, &catalog_revision())
        .unwrap()
    {
        ConnectProposal::Proposed(proposal) => proposal,
        ConnectProposal::Replay(_) => panic!("new connect unexpectedly replayed"),
    };
    transaction.commit(*proposal).unwrap().mutation
}

#[test]
fn connect_upserts_rotates_replays_and_redacts_secrets() {
    let temporary = TempDir::new().unwrap();
    let store = private_store(&temporary);
    let initial = store.load().unwrap();
    let first_request = connect_request(&initial, "connect-1", "openai", "never-print-one");
    assert!(!format!("{first_request:?}").contains("never-print-one"));

    let transaction = store.begin_transaction().unwrap();
    let proposal = match transaction
        .propose_connect(&first_request, &catalog_revision())
        .unwrap()
    {
        ConnectProposal::Proposed(proposal) => proposal,
        ConnectProposal::Replay(_) => unreachable!(),
    };
    let proposed = proposal.snapshot();
    let proposed_receipt = proposal.mutation().durable_receipt().clone();
    assert_eq!(proposed.generation().get(), 2);
    assert_eq!(proposed.store_revision(), &proposed_receipt.store_revision);
    assert_eq!(
        proposed
            .provider(&ProviderId::new("openai").unwrap())
            .unwrap()
            .connection_generation
            .get(),
        1
    );
    assert!(!format!("{proposal:?}{proposed:?}").contains("never-print-one"));
    let committed = transaction.commit(*proposal).unwrap();
    assert_eq!(
        committed.snapshot.store_revision(),
        &proposed_receipt.store_revision
    );

    let loaded = store.load().unwrap();
    let connection = loaded
        .provider(&ProviderId::new("openai").unwrap())
        .unwrap();
    assert_eq!(
        connection.credential(&AuthFieldName::new("api_key").unwrap()),
        Some("never-print-one")
    );
    assert!(matches!(
        connection
            .setup_values
            .get(&SetupFieldId::new("region").unwrap()),
        Some(SafeSetupValue::Code(value)) if value.as_str() == "us-test-1"
    ));
    assert_eq!(
        fs::metadata(store.path()).unwrap().permissions().mode() & 0o777,
        0o700
    );
    for name in ["store-v3.lock", "store-v3.json"] {
        assert_eq!(
            fs::metadata(store.path().join(name))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    let restarted = ProviderStore::open(store.path()).unwrap();
    let replay = restarted
        .begin_transaction()
        .unwrap()
        .propose_connect(&first_request, &catalog_revision())
        .unwrap();
    match replay {
        ConnectProposal::Replay(mutation) => {
            assert_eq!(mutation.durable_receipt(), &proposed_receipt)
        }
        ConnectProposal::Proposed(_) => panic!("same payload did not replay"),
    }

    let mut conflict = first_request.clone();
    conflict.auth_values = ProviderAuthValues::new(BTreeMap::from([(
        AuthFieldName::new("api_key").unwrap(),
        "never-print-two".to_owned(),
    )]))
    .unwrap();
    assert!(matches!(
        store
            .begin_transaction()
            .unwrap()
            .propose_connect(&conflict, &catalog_revision()),
        Err(ProviderStoreError::IdempotencyConflict)
    ));

    let current = store.load().unwrap();
    let rotation = connect_request(&current, "connect-2", "openai", "rotated-secret");
    commit_connect(&store, &rotation);
    let rotated = store.load().unwrap();
    let connection = rotated
        .provider(&ProviderId::new("openai").unwrap())
        .unwrap();
    assert_eq!(connection.connection_generation.get(), 2);
    assert_eq!(
        connection.credential(&AuthFieldName::new("api_key").unwrap()),
        Some("rotated-secret")
    );
}

#[test]
fn complete_connect_parameters_are_reorder_equivalent_and_changes_conflict() {
    let first_temp = TempDir::new().unwrap();
    let first_store = private_store(&first_temp);
    let first_snapshot = first_store.load().unwrap();
    let mut first = connect_request(&first_snapshot, "ordered", "openai", "one");
    first.setup_values.insert(
        SetupFieldId::new("project").unwrap(),
        SafeSetupValue::Code(SafeCode::new("project-a").unwrap()),
    );
    first.auth_values = ProviderAuthValues::new(BTreeMap::from([
        (AuthFieldName::new("api_key").unwrap(), "one".to_owned()),
        (
            AuthFieldName::new("session_token").unwrap(),
            "two".to_owned(),
        ),
    ]))
    .unwrap();
    commit_connect(&first_store, &first);
    let first_digest = read_store(&first_store)["connect_receipts"]["ordered"]["payload_digest"]
        .as_str()
        .unwrap()
        .to_owned();

    let second_temp = TempDir::new().unwrap();
    let second_store = private_store(&second_temp);
    let second_snapshot = second_store.load().unwrap();
    let mut setup = BTreeMap::new();
    setup.insert(
        SetupFieldId::new("project").unwrap(),
        SafeSetupValue::Code(SafeCode::new("project-a").unwrap()),
    );
    setup.insert(
        SetupFieldId::new("region").unwrap(),
        SafeSetupValue::Code(SafeCode::new("us-test-1").unwrap()),
    );
    let mut auth = BTreeMap::new();
    auth.insert(
        AuthFieldName::new("session_token").unwrap(),
        "two".to_owned(),
    );
    auth.insert(AuthFieldName::new("api_key").unwrap(), "one".to_owned());
    let mut reordered = connect_request(&second_snapshot, "ordered", "openai", "unused");
    reordered.setup_values = setup;
    reordered.auth_values = ProviderAuthValues::new(auth).unwrap();
    commit_connect(&second_store, &reordered);
    let second_digest = read_store(&second_store)["connect_receipts"]["ordered"]["payload_digest"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(first_digest, second_digest);

    let restarted = ProviderStore::open(first_store.path()).unwrap();
    assert!(matches!(
        restarted
            .begin_transaction()
            .unwrap()
            .propose_connect(&reordered, &catalog_revision()),
        Ok(ConnectProposal::Replay(_))
    ));

    let mut omitted_auth = first.clone();
    omitted_auth.auth_values = ProviderAuthValues::new(BTreeMap::from([(
        AuthFieldName::new("api_key").unwrap(),
        "one".to_owned(),
    )]))
    .unwrap();
    let mut omitted_setup = first.clone();
    omitted_setup
        .setup_values
        .remove(&SetupFieldId::new("project").unwrap());
    let mut changed_method = first.clone();
    changed_method.auth_method = AuthMethodId::new("oauth-access-token-v1").unwrap();
    let mut changed_recipe = first.clone();
    changed_recipe.policy.recipe_fingerprint =
        Sha256Digest::new(format!("{:x}", Sha256::digest(b"changed recipe"))).unwrap();
    assert_eq!(
        changed_recipe.policy.source_record_digest,
        first.policy.source_record_digest
    );
    for (label, changed) in [
        ("auth", &omitted_auth),
        ("setup", &omitted_setup),
        ("method", &changed_method),
        ("recipe", &changed_recipe),
    ] {
        let result = first_store
            .begin_transaction()
            .unwrap()
            .propose_connect(changed, &catalog_revision());
        assert!(
            matches!(result, Err(ProviderStoreError::IdempotencyConflict)),
            "{label}: {result:?}"
        );
    }
}

#[test]
fn disconnect_removes_setup_and_credentials_and_absence_is_durable() {
    let temporary = TempDir::new().unwrap();
    let store = private_store(&temporary);
    let initial = store.load().unwrap();
    commit_connect(
        &store,
        &connect_request(&initial, "connect", "openai", "secret"),
    );
    let connected = store.load().unwrap();
    let provider = ProviderId::new("openai").unwrap();
    let request = DisconnectMutation {
        client_request_id: ClientRequestId::new("disconnect-1").unwrap(),
        provider_id: provider.clone(),
        expected_runtime_revision: runtime_revision(),
        expected_provider_state_revision: connected.provider_state_revision(),
        expected_store_generation: connected.generation(),
        expected_store_revision: connected.store_revision().clone(),
        expected_connection_generation: Some(
            connected.provider(&provider).unwrap().connection_generation,
        ),
    };
    let transaction = store.begin_transaction().unwrap();
    let proposal = match transaction
        .propose_disconnect(&request, &runtime_revision())
        .unwrap()
    {
        DisconnectProposal::Proposed(proposal) => proposal,
        DisconnectProposal::Replay(_) => unreachable!(),
    };
    assert!(proposal.snapshot().provider(&provider).is_none());
    let receipt = proposal.mutation().durable_receipt().clone();
    transaction.commit(*proposal).unwrap();
    let disconnected = store.load().unwrap();
    assert!(disconnected.provider(&provider).is_none());

    let restarted = ProviderStore::open(store.path()).unwrap();
    match restarted
        .begin_transaction()
        .unwrap()
        .propose_disconnect(&request, &runtime_revision())
        .unwrap()
    {
        DisconnectProposal::Replay(mutation) => assert_eq!(mutation.durable_receipt(), &receipt),
        DisconnectProposal::Proposed(_) => panic!("disconnect did not replay"),
    }

    let mut conflict = request.clone();
    conflict.provider_id = ProviderId::new("anthropic").unwrap();
    assert!(matches!(
        store
            .begin_transaction()
            .unwrap()
            .propose_disconnect(&conflict, &runtime_revision()),
        Err(ProviderStoreError::IdempotencyConflict)
    ));
    let mut omitted_generation = request.clone();
    omitted_generation.expected_connection_generation = None;
    let mut changed_runtime = request.clone();
    changed_runtime.expected_runtime_revision = revision("changed-runtime", RuntimeRevision::new);
    for changed in [&omitted_generation, &changed_runtime] {
        assert!(matches!(
            store
                .begin_transaction()
                .unwrap()
                .propose_disconnect(changed, &runtime_revision()),
            Err(ProviderStoreError::IdempotencyConflict)
        ));
    }

    let absent = store.load().unwrap();
    let absent_request = DisconnectMutation {
        client_request_id: ClientRequestId::new("disconnect-absent").unwrap(),
        provider_id: ProviderId::new("anthropic").unwrap(),
        expected_runtime_revision: runtime_revision(),
        expected_provider_state_revision: absent.provider_state_revision(),
        expected_store_generation: absent.generation(),
        expected_store_revision: absent.store_revision().clone(),
        expected_connection_generation: None,
    };
    let transaction = store.begin_transaction().unwrap();
    let proposal = match transaction
        .propose_disconnect(&absent_request, &runtime_revision())
        .unwrap()
    {
        DisconnectProposal::Proposed(proposal) => proposal,
        DisconnectProposal::Replay(_) => unreachable!(),
    };
    assert_eq!(
        proposal.snapshot().generation().get(),
        absent.generation().get() + 1
    );
    transaction.commit(*proposal).unwrap();
}

#[test]
fn parameter_digests_are_sha256_of_complete_rfc8785_jcs() {
    let temporary = TempDir::new().unwrap();
    let store = private_store(&temporary);
    let initial = store.load().unwrap();
    let connect = connect_request(&initial, "connect-jcs", "openai", "a\0b\\c\"d");
    commit_connect(&store, &connect);
    let disk = read_store(&store);
    let expected_connect = serde_json::json!({
        "client_connect_id": "connect-jcs",
        "provider_id": "openai",
        "expected_catalog_revision": catalog_revision(),
        "setup_values": {"region": "us-test-1"},
        "auth_method": "bearer-api-key-v1",
        "auth_values": {"api_key": "a\0b\\c\"d"},
        "policy": serde_json::to_value(&connect.policy).unwrap(),
    });
    assert_eq!(
        disk["connect_receipts"]["connect-jcs"]["payload_digest"],
        digest_jcs(&expected_connect)
    );
    let mut changed_connect_id = expected_connect.clone();
    changed_connect_id["client_connect_id"] = Value::String("connect-other".into());
    assert_ne!(
        digest_jcs(&changed_connect_id),
        disk["connect_receipts"]["connect-jcs"]["payload_digest"]
    );

    let connected = store.load().unwrap();
    let provider = ProviderId::new("openai").unwrap();
    let disconnect = DisconnectMutation {
        client_request_id: ClientRequestId::new("disconnect-jcs").unwrap(),
        provider_id: provider.clone(),
        expected_runtime_revision: runtime_revision(),
        expected_provider_state_revision: connected.provider_state_revision(),
        expected_store_generation: connected.generation(),
        expected_store_revision: connected.store_revision().clone(),
        expected_connection_generation: Some(
            connected.provider(&provider).unwrap().connection_generation,
        ),
    };
    let expected_disconnect = serde_json::json!({
        "client_request_id": "disconnect-jcs",
        "provider_id": "openai",
        "expected_runtime_revision": disconnect.expected_runtime_revision.clone(),
        "expected_provider_state_revision": disconnect.expected_provider_state_revision.clone(),
        "expected_connection_generation": 1,
    });
    let transaction = store.begin_transaction().unwrap();
    let proposal = match transaction
        .propose_disconnect(&disconnect, &runtime_revision())
        .unwrap()
    {
        DisconnectProposal::Proposed(proposal) => proposal,
        DisconnectProposal::Replay(_) => unreachable!(),
    };
    transaction.commit(*proposal).unwrap();
    let disk = read_store(&store);
    assert_eq!(
        disk["disconnect_receipts"]["disconnect-jcs"]["payload_digest"],
        digest_jcs(&expected_disconnect)
    );

    let mut changed_id_payload = expected_disconnect;
    changed_id_payload["client_request_id"] = Value::String("disconnect-other".into());
    assert_ne!(
        digest_jcs(&changed_id_payload),
        disk["disconnect_receipts"]["disconnect-jcs"]["payload_digest"]
    );
}

#[test]
fn revision_is_deterministic_jcs_of_the_exact_durable_projection() {
    let temporary = TempDir::new().unwrap();
    let store = private_store(&temporary);
    let initial = store.load().unwrap();
    commit_connect(
        &store,
        &connect_request(&initial, "connect", "openai", "revision-covered-secret"),
    );
    let disk = read_store(&store);
    let expected = format!("sha256:{}", digest_jcs(&revision_projection(disk.clone())));
    assert_eq!(disk["store_revision"], expected);

    let object = disk.as_object().unwrap();
    let reordered = format!(
        "{{\"store_revision\":{},\"schema_version\":{},\"providers\":{},\"generation\":{},\"disconnect_receipts\":{},\"connect_receipts\":{}}}",
        serde_json::to_string(&object["store_revision"]).unwrap(),
        serde_json::to_string(&object["schema_version"]).unwrap(),
        serde_json::to_string(&object["providers"]).unwrap(),
        serde_json::to_string(&object["generation"]).unwrap(),
        serde_json::to_string(&object["disconnect_receipts"]).unwrap(),
        serde_json::to_string(&object["connect_receipts"]).unwrap(),
    );
    fs::write(store.path().join("store-v3.json"), reordered).unwrap();
    fs::set_permissions(
        store.path().join("store-v3.json"),
        fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    assert_eq!(store.load().unwrap().store_revision().as_str(), expected);

    let mut tampered = read_store(&store);
    tampered["providers"]["openai"]["auth_values"]["api_key"] =
        Value::String("tampered-secret".into());
    fs::write(
        store.path().join("store-v3.json"),
        serde_json::to_vec(&tampered).unwrap(),
    )
    .unwrap();
    assert!(matches!(
        store.load(),
        Err(ProviderStoreError::InvalidStore)
    ));
}

#[test]
fn adversarial_payloads_do_not_alias_and_jcs_uses_utf16_property_order() {
    let secrets = ["a\0b", "a\\u0000b", "é", "e\u{301}"];
    let isolated_digests = secrets
        .map(|secret| {
            let temporary = TempDir::new().unwrap();
            let store = private_store(&temporary);
            let snapshot = store.load().unwrap();
            commit_connect(
                &store,
                &connect_request(&snapshot, "same-id", "openai", secret),
            );
            read_store(&store)["connect_receipts"]["same-id"]["payload_digest"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .into_iter()
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(isolated_digests.len(), secrets.len());

    let temporary = TempDir::new().unwrap();
    let store = private_store(&temporary);
    let mut snapshot = store.load().unwrap();
    for (index, secret) in secrets.into_iter().enumerate() {
        let mut request =
            connect_request(&snapshot, &format!("adversarial-{index}"), "openai", secret);
        request.policy.model_overrides.insert(
            ProviderModelId::new("model").unwrap(),
            StoredModelOverrideProjection {
                metadata: BTreeMap::from([
                    ("\u{e000}".into(), SafePolicyValue::Bool(false)),
                    ("\u{10000}".into(), SafePolicyValue::Bool(true)),
                ]),
            },
        );
        commit_connect(&store, &request);
        snapshot = store.load().unwrap();
    }
    let disk = read_store(&store);
    let digests = disk["connect_receipts"]
        .as_object()
        .unwrap()
        .values()
        .map(|receipt| receipt["payload_digest"].as_str().unwrap())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(digests.len(), 4);

    let bytes = fs::read_to_string(store.path().join("store-v3.json")).unwrap();
    assert!(bytes.find('\u{10000}').unwrap() < bytes.find('\u{e000}').unwrap());

    let current = store.load().unwrap();
    let mut collision_attempt = connect_request(&current, "adversarial-0", "openai", "a\\u0000b");
    collision_attempt.policy = policy(&catalog_revision(), "openai");
    assert!(matches!(
        store
            .begin_transaction()
            .unwrap()
            .propose_connect(&collision_attempt, &catalog_revision()),
        Err(ProviderStoreError::IdempotencyConflict)
    ));
}

#[test]
fn stale_revisions_generations_and_custom_providers_are_typed() {
    let temporary = TempDir::new().unwrap();
    let store = private_store(&temporary);
    let initial = store.load().unwrap();
    let mut stale = connect_request(&initial, "connect", "openai", "secret");
    stale.expectation.generation =
        cookie_agent_models::provider_store::ProviderStoreGeneration::new(2).unwrap();
    assert!(matches!(
        store
            .begin_transaction()
            .unwrap()
            .propose_connect(&stale, &catalog_revision()),
        Err(ProviderStoreError::StoreGenerationConflict)
    ));

    let custom = connect_request(&initial, "custom", "custom.local", "secret");
    assert!(matches!(
        store
            .begin_transaction()
            .unwrap()
            .propose_connect(&custom, &catalog_revision()),
        Err(ProviderStoreError::CustomProviderForbidden)
    ));

    let disconnect = DisconnectMutation {
        client_request_id: ClientRequestId::new("stale").unwrap(),
        provider_id: ProviderId::new("openai").unwrap(),
        expected_runtime_revision: runtime_revision(),
        expected_provider_state_revision: initial.provider_state_revision(),
        expected_store_generation: initial.generation(),
        expected_store_revision: initial.store_revision().clone(),
        expected_connection_generation: Some(ProviderConnectionGeneration::new(1).unwrap()),
    };
    assert!(matches!(
        store
            .begin_transaction()
            .unwrap()
            .propose_disconnect(&disconnect, &runtime_revision()),
        Err(ProviderStoreError::StaleConnectionGeneration)
    ));
}

#[test]
fn cross_process_generation_change_is_detected_without_clobbering() {
    let temporary = TempDir::new().unwrap();
    let first = private_store(&temporary);
    let second = ProviderStore::open(first.path()).unwrap();
    let initial = first.load().unwrap();
    assert!(
        first
            .reload_if_changed(initial.generation())
            .unwrap()
            .is_none()
    );

    commit_connect(
        &second,
        &connect_request(&initial, "connect", "openai", "secret"),
    );
    let changed = first
        .reload_if_changed(initial.generation())
        .unwrap()
        .unwrap();
    assert_eq!(changed.generation().get(), initial.generation().get() + 1);
    assert!(
        changed
            .provider(&ProviderId::new("openai").unwrap())
            .is_some()
    );

    let stale_request = connect_request(&initial, "stale", "anthropic", "secret");
    assert!(matches!(
        first
            .begin_transaction()
            .unwrap()
            .propose_connect(&stale_request, &catalog_revision()),
        Err(ProviderStoreError::StoreGenerationConflict)
    ));
}

#[test]
fn concurrent_independent_transactions_retry_without_lost_updates() {
    let temporary = TempDir::new().unwrap();
    let root = temporary.path().join("providers");
    let initial_store = private_store(&temporary);
    let initial = initial_store.load().unwrap();
    let barrier = Arc::new(Barrier::new(2));
    let handles = ["openai", "anthropic"].map(|provider| {
        let root = root.clone();
        let barrier = Arc::clone(&barrier);
        let initial = initial.clone();
        thread::spawn(move || {
            let store = ProviderStore::open(root).unwrap();
            barrier.wait();
            let mut snapshot = initial;
            loop {
                let request = connect_request(
                    &snapshot,
                    &format!("connect-{provider}"),
                    provider,
                    "secret",
                );
                let transaction = store.begin_transaction().unwrap();
                match transaction.propose_connect(&request, &catalog_revision()) {
                    Ok(ConnectProposal::Proposed(proposal)) => {
                        transaction.commit(*proposal).unwrap();
                        break;
                    }
                    Err(ProviderStoreError::StoreGenerationConflict) => {
                        drop(transaction);
                        snapshot = store.load().unwrap();
                    }
                    other => panic!("unexpected concurrent outcome: {other:?}"),
                }
            }
        })
    });
    for handle in handles {
        handle.join().unwrap();
    }
    let loaded = initial_store.load().unwrap();
    assert_eq!(loaded.providers().len(), 2);
    assert_eq!(loaded.generation().get(), 3);
}

#[test]
fn partial_duplicate_unversioned_and_prior_store_state_are_rejected() {
    let cases = [
        ("store-v3.json", b"{".as_slice(), "invalid"),
        (
            "store-v3.json",
            br#"{"schema_version":3,"schema_version":3}"#,
            "invalid",
        ),
        ("store-v3.json", br#"{"providers":{}}"#, "unversioned"),
        ("store-v3.json", br#"{"schema_version":2}"#, "legacy"),
        ("store-v2.json", b"x".as_slice(), "legacy"),
        ("store-v1.json", b"x".as_slice(), "legacy"),
        ("store.json", b"x".as_slice(), "unversioned"),
    ];
    for (name, body, expected) in cases {
        let temporary = TempDir::new().unwrap();
        let store = private_store(&temporary);
        store.load().unwrap();
        fs::write(store.path().join(name), body).unwrap();
        fs::set_permissions(store.path().join(name), fs::Permissions::from_mode(0o600)).unwrap();
        let error = store.load().unwrap_err();
        assert!(match expected {
            "legacy" => matches!(error, ProviderStoreError::LegacyStoreVersion),
            "unversioned" => matches!(error, ProviderStoreError::UnversionedStore),
            _ => matches!(error, ProviderStoreError::InvalidStore),
        });
    }
}

#[test]
fn recipe_fingerprint_is_required_and_revision_protected_in_schema3() {
    for remove in [true, false] {
        let temporary = TempDir::new().unwrap();
        let store = private_store(&temporary);
        let snapshot = store.load().unwrap();
        let request = connect_request(&snapshot, "recipe-fingerprint", "openai", "secret");
        let transaction = store.begin_transaction().unwrap();
        let ConnectProposal::Proposed(proposal) = transaction
            .propose_connect(&request, &request.expected_catalog_revision)
            .unwrap()
        else {
            panic!("new request must propose");
        };
        transaction.commit(*proposal).unwrap();

        let mut document = read_store(&store);
        let policy = document["providers"]["openai"]["policy"]
            .as_object_mut()
            .unwrap();
        if remove {
            policy.remove("recipe_fingerprint");
        } else {
            policy.insert(
                "recipe_fingerprint".to_owned(),
                Value::String("f".repeat(64)),
            );
        }
        let path = store.path().join("store-v3.json");
        fs::write(&path, serde_json::to_vec(&document).unwrap()).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(matches!(
            store.load(),
            Err(ProviderStoreError::InvalidStore)
        ));
    }
}

#[test]
fn lock_replacement_race_prevents_proposal_commit() {
    let temporary = TempDir::new().unwrap();
    let store = private_store(&temporary);
    let initial = store.load().unwrap();
    let transaction = store.begin_transaction().unwrap();
    let proposal = match transaction
        .propose_connect(
            &connect_request(&initial, "connect", "openai", "secret"),
            &catalog_revision(),
        )
        .unwrap()
    {
        ConnectProposal::Proposed(proposal) => proposal,
        ConnectProposal::Replay(_) => unreachable!(),
    };
    fs::rename(
        store.path().join("store-v3.lock"),
        store.path().join("displaced.lock"),
    )
    .unwrap();
    fs::write(store.path().join("store-v3.lock"), b"").unwrap();
    fs::set_permissions(
        store.path().join("store-v3.lock"),
        fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    assert!(matches!(
        transaction.commit(*proposal),
        Err(ProviderStoreError::Storage(_))
    ));
    assert!(!store.path().join("store-v3.json").exists());
}

#[test]
fn secret_values_are_absent_from_debug_and_error_text() {
    let temporary = TempDir::new().unwrap();
    let store = private_store(&temporary);
    let initial = store.load().unwrap();
    let request = connect_request(&initial, "connect", "openai", "body-free-secret");
    let transaction = store.begin_transaction().unwrap();
    let proposal = match transaction
        .propose_connect(&request, &catalog_revision())
        .unwrap()
    {
        ConnectProposal::Proposed(proposal) => proposal,
        ConnectProposal::Replay(_) => unreachable!(),
    };
    let all_debug = format!(
        "{request:?}{transaction:?}{proposal:?}{:?}",
        proposal.snapshot()
    );
    assert!(!all_debug.contains("body-free-secret"));
    let mut conflict = request;
    conflict.auth_values = ProviderAuthValues::new(BTreeMap::from([(
        AuthFieldName::new("api_key").unwrap(),
        "other-secret".to_owned(),
    )]))
    .unwrap();
    transaction.commit(*proposal).unwrap();
    let error = store
        .begin_transaction()
        .unwrap()
        .propose_connect(&conflict, &catalog_revision())
        .unwrap_err();
    assert!(!format!("{error:?} {error}").contains("secret"));
}

#[test]
fn setup_and_auth_field_shapes_are_bounded_and_normalized() {
    assert!(
        ProviderAuthValues::new(BTreeMap::from([(
            AuthFieldName::new("api_key").unwrap(),
            String::new(),
        )]))
        .is_err()
    );
    assert!(SafePolicyString::new("${env:SECRET}").is_err());
    assert!(SafeCode::new("managed").is_ok());
}
