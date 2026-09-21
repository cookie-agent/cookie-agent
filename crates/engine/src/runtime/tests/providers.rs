#[cfg(unix)]
use std::os::unix::fs::PermissionsExt as _;

use std::{collections::BTreeMap, fs, sync::Arc};

use cookie_agent_models::{
    ModelManager,
    catalog::{
        CatalogAvailability, CatalogProviderEntry, CatalogQuarantineEntry, CatalogQuarantineReason,
        CatalogSource,
    },
    provider_store::{ClientRequestId as StoreClientRequestId, ProviderStore},
};

use cookie_agent_protocol::{
    AgentId, CatalogRevision, ClientConnectId, ClientRequestId, EventPayload, ModelSelection,
    ProviderConnectParams, ProviderCredentialValues, ProviderDisconnectParams, ProviderId,
    RunSelection, RunStartParams, RuntimeChangeReason, SetupFieldId,
};

use crate::{Engine, EngineError, EngineOptions, ToolProvider};

use super::support::*;

#[test]
fn provider_registration_rejects_duplicate_provenance_ids() {
    let fixture = fixture();
    let duplicate_options = EngineOptions {
        data_dir: fixture._directory.path().join("duplicate-provider-data"),
        cwd: fixture._directory.path().to_owned(),
        config: fixture.config.clone(),
        model_manager: Arc::clone(&fixture.manager),
        tools: vec![
            Arc::new(TestPromptProvider::new("test.duplicate", Vec::new())),
            Arc::new(TestPromptProvider::new("test.duplicate", Vec::new())),
        ],
        model_snapshot_directory: None,
    };
    let startup_error = match Engine::open(duplicate_options) {
        Ok(_) => panic!("duplicate startup provider ID must fail"),
        Err(error) => error,
    };
    assert!(matches!(startup_error, EngineError::ToolFailed(_)));
    assert!(
        startup_error
            .to_string()
            .contains("tool provider ID `test.duplicate` is already registered")
    );

    let provider: Arc<dyn ToolProvider> =
        Arc::new(TestPromptProvider::new("test.runtime", Vec::new()));
    fixture
        .engine
        .try_register_tool_provider(Arc::clone(&provider))
        .expect("first provider registration");
    let duplicate_error = fixture
        .engine
        .try_register_tool_provider(provider)
        .expect_err("duplicate runtime provider ID must fail");
    assert!(matches!(duplicate_error, EngineError::ToolFailed(_)));
    assert!(
        duplicate_error
            .to_string()
            .contains("tool provider ID `test.runtime` is already registered")
    );

    for reserved_id in ["mcp", "plugin"] {
        let error = fixture
            .engine
            .try_register_tool_provider(Arc::new(TestPromptProvider::new(reserved_id, Vec::new())))
            .expect_err("startup provider ID must remain reserved");
        assert!(matches!(error, EngineError::ToolFailed(_)));
        assert!(
            error.to_string().contains(&format!(
                "tool provider ID `{reserved_id}` is already registered"
            )),
            "{error}"
        );
    }
}

#[cfg(unix)]
#[test]
fn shared_project_cwd_creates_and_reopens_model_manifests() {
    let fixture = fixture();
    let workspace = fixture._directory.path().join("shared-workspace");
    fs::create_dir(&workspace).expect("shared workspace");
    fs::set_permissions(&workspace, fs::Permissions::from_mode(0o775))
        .expect("shared workspace mode");
    let data_dir = fixture._directory.path().join("shared-data");
    let snapshots = fixture._directory.path().join("model-snapshots");

    let engine = Engine::open(EngineOptions {
        data_dir: data_dir.clone(),
        cwd: workspace.clone(),
        config: fixture.config.clone(),
        model_manager: Arc::clone(&fixture.manager),
        tools: Vec::new(),
        model_snapshot_directory: Some(snapshots.clone()),
    })
    .expect("engine in shared workspace");
    let revision = engine
        .runtime_snapshot()
        .expect("runtime snapshot")
        .snapshot
        .model_revision;
    drop(engine);

    assert_eq!(
        fs::metadata(&snapshots).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert!(fs::read_dir(&snapshots).unwrap().any(|entry| {
        entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .ends_with(".json")
    }));

    let reopened = Engine::open(EngineOptions {
        data_dir,
        cwd: workspace,
        config: fixture.config.clone(),
        model_manager: Arc::clone(&fixture.manager),
        tools: Vec::new(),
        model_snapshot_directory: Some(snapshots),
    })
    .expect("reopened engine in shared workspace");
    assert_eq!(
        reopened
            .runtime_snapshot()
            .expect("reopened runtime snapshot")
            .snapshot
            .model_revision,
        revision
    );
}

#[test]
fn absent_disconnect_commits_once_and_replay_publishes_nothing() {
    let fixture = fixture();
    let initial = fixture
        .engine
        .runtime_snapshot()
        .expect("runtime snapshot")
        .snapshot;
    let mut notifications = fixture.engine.subscribe_runtime_changes();
    let request = ProviderDisconnectParams {
        provider_id: ProviderId::new("openai").expect("provider ID"),
        expected_runtime_revision: initial.runtime_revision.clone(),
        expected_provider_state_revision: initial.provider_state_revision.clone(),
        expected_connection_generation: None,
        client_request_id: ClientRequestId::new("absent-disconnect").expect("request ID"),
    };
    let first = fixture
        .engine
        .disconnect_provider(request.clone())
        .expect("first disconnect");
    assert!(!first.replayed);
    let changed = notifications.try_recv().expect("runtime notification");
    assert_eq!(
        changed.reasons,
        vec![RuntimeChangeReason::ProviderDisconnected]
    );
    assert_eq!(changed.previous_revision, Some(initial.runtime_revision));

    let replay = fixture
        .engine
        .disconnect_provider(request)
        .expect("disconnect replay");
    assert!(replay.replayed);
    assert!(notifications.try_recv().is_err());
    assert_eq!(
        replay.runtime.snapshot.runtime_revision,
        first.runtime.snapshot.runtime_revision
    );
}

#[test]
fn disconnect_replay_survives_a_clean_engine_restart() {
    let fixture = fixture();
    let initial = fixture.engine.runtime_snapshot().expect("runtime").snapshot;
    let request = ProviderDisconnectParams {
        provider_id: ProviderId::new("openai").expect("provider ID"),
        expected_runtime_revision: initial.runtime_revision,
        expected_provider_state_revision: initial.provider_state_revision,
        expected_connection_generation: None,
        client_request_id: ClientRequestId::new("restart-disconnect").expect("request ID"),
    };
    let first = fixture
        .engine
        .disconnect_provider(request.clone())
        .expect("first disconnect");
    let reopened = reopen_engine(&fixture);
    let mut notifications = reopened.subscribe_runtime_changes();
    let replay = reopened
        .disconnect_provider(request)
        .expect("restart replay");
    assert!(replay.replayed);
    assert_eq!(replay.durable_receipt, first.durable_receipt);
    assert!(notifications.try_recv().is_err());
}

#[tokio::test]
async fn global_bedrock_connection_executes_cross_workspace_and_disconnect_preserves_frozen_run() {
    let temporary = private_tempdir();
    let workspace_one = temporary.path().join("workspace-one");
    let workspace_two = temporary.path().join("workspace-two");
    let config_one = empty_provider_workspace(&workspace_one);
    let config_two = empty_provider_workspace(&workspace_two);
    assert!(config_one.runtime.providers.is_empty());
    assert!(config_two.runtime.providers.is_empty());
    let data = temporary.path().join("data");
    let provider_store = temporary.path().join("global-provider-store");
    let catalog = bedrock_catalog();
    let (engine_one, _) = open_workspace_engine(
        &workspace_one,
        &data,
        &provider_store,
        Arc::clone(&catalog),
        config_one.clone(),
    );
    let initial = engine_one
        .runtime_snapshot()
        .expect("initial runtime")
        .snapshot;
    assert!(initial.models.is_empty());
    let auth_values: ProviderCredentialValues = serde_json::from_value(serde_json::json!({
        "access_key_id":"bedrock-access",
        "secret_access_key":"bedrock-secret",
        "session_token":"bedrock-session"
    }))
    .expect("credential values");
    let connected = engine_one
        .connect_provider(ProviderConnectParams {
            provider_id: ProviderId::new("amazon-bedrock").expect("provider ID"),
            expected_catalog_revision: catalog.revision.clone(),
            setup_values: BTreeMap::from([(
                SetupFieldId::new("region").expect("setup field"),
                cookie_agent_protocol::SafeSetupValue::String(
                    cookie_agent_protocol::BoundedSetupString::new("us-east-1").expect("region"),
                ),
            )]),
            auth_method: cookie_agent_protocol::AuthMethodId::new("aws-sigv4-credentials-v1")
                .expect("auth method"),
            auth_values,
            client_connect_id: ClientConnectId::new("global-bedrock-connect").expect("connect ID"),
        })
        .expect("connect Bedrock");
    assert_eq!(
        connected.effective_auth_source,
        cookie_agent_protocol::EffectiveAuthSource::ProviderStore
    );
    assert_eq!(connected.runtime.models.len(), 1);
    assert!(
        connected
            .runtime
            .agents
            .iter()
            .any(|agent| agent.runnable_as_root)
    );

    let (engine_two, manager_two) = open_workspace_engine(
        &workspace_two,
        &data,
        &provider_store,
        Arc::clone(&catalog),
        config_two.clone(),
    );
    let second = engine_two
        .runtime_snapshot()
        .expect("second runtime")
        .snapshot;
    assert_eq!(second.models.len(), 1);
    assert_eq!(
        second.providers[0].effective_auth_state,
        cookie_agent_protocol::EffectiveAuthState::ProviderStore
    );
    let selection = RunSelection {
        agent: AgentId::new("primary").expect("agent ID"),
        model: ModelSelection {
            model: "amazon-bedrock/anthropic.claude-3-5-sonnet-20241022-v2:0"
                .parse()
                .expect("model key"),
            variant: None,
        },
        preset: None,
    };
    manager_two
        .current()
        .resolve(&selection.model)
        .expect("cross-workspace executable constructor");
    let session = engine_two
        .create_session(selection.clone())
        .expect("session");
    let run = engine_two
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: session.session_id,
                client_run_id: cookie_agent_protocol::ClientRunId::new("frozen-bedrock-run")
                    .expect("run ID"),
                selection,
                input: "hold frozen Bedrock semantics".to_owned(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("accepted run");
    let frozen = engine_two
        .inner
        .store
        .get(session.session_id)
        .expect("session projection")
        .log
        .events()
        .into_iter()
        .find_map(|event| match event.payload {
            EventPayload::RunStarted {
                selected_suffix, ..
            } if event.run_id == Some(run.run_id) => Some(selected_suffix),
            _ => None,
        })
        .expect("frozen suffix");
    let connection = second.providers[0]
        .durable_connection
        .as_ref()
        .expect("durable connection");
    let disconnect_request = ProviderDisconnectParams {
        provider_id: ProviderId::new("amazon-bedrock").expect("provider ID"),
        expected_runtime_revision: second.runtime_revision,
        expected_provider_state_revision: second.provider_state_revision,
        expected_connection_generation: Some(connection.connection_generation),
        client_request_id: ClientRequestId::new("global-bedrock-disconnect")
            .expect("disconnect ID"),
    };
    let disconnected = engine_two
        .disconnect_provider(disconnect_request.clone())
        .expect("disconnect Bedrock");
    assert!(!disconnected.replayed);
    assert!(disconnected.runtime.snapshot.models.is_empty());
    assert_eq!(
        disconnected.runtime.snapshot.providers[0].effective_auth_state,
        cookie_agent_protocol::EffectiveAuthState::Unavailable
    );
    assert!(
        engine_one
            .runtime_snapshot()
            .expect("workspace one reload")
            .snapshot
            .models
            .is_empty()
    );
    let readable = engine_two
        .get_session(session.session_id)
        .expect("readable session");
    assert_eq!(readable.manifest_revision, frozen[0].manifest_revision);
    let still_frozen = engine_two
        .inner
        .store
        .get(session.session_id)
        .expect("session after disconnect")
        .log
        .events()
        .into_iter()
        .find_map(|event| match event.payload {
            EventPayload::RunStarted {
                selected_suffix, ..
            } if event.run_id == Some(run.run_id) => Some(selected_suffix),
            _ => None,
        })
        .expect("frozen suffix after disconnect");
    assert_eq!(still_frozen, frozen);
    engine_one.shutdown().await;
    engine_two.shutdown().await;

    let (reopened_two, _) =
        open_workspace_engine(&workspace_two, &data, &provider_store, catalog, config_two);
    let replay = reopened_two
        .disconnect_provider(disconnect_request)
        .expect("disconnect replay after restart");
    assert!(replay.replayed);
    assert!(reopened_two.get_session(session.session_id).is_ok());
    reopened_two.shutdown().await;
}

#[test]
fn catalog_refresh_publishes_one_coherent_reasoned_snapshot() {
    let fixture = fixture();
    let before = fixture.engine.current_runtime();
    let mut refreshed = (**before.models.catalog()).clone();
    refreshed.revision =
        CatalogRevision::new(format!("sha256:{}", "2".repeat(64))).expect("catalog revision");
    refreshed.source = CatalogSource::Network;
    refreshed.state.availability = CatalogAvailability::Ready;
    let mut notifications = fixture.engine.subscribe_runtime_changes();
    let result = fixture
        .engine
        .refresh_catalog(Arc::new(refreshed))
        .expect("catalog refresh");
    let changed = notifications.try_recv().expect("refresh notification");
    assert_eq!(changed.reasons, vec![RuntimeChangeReason::CatalogRefreshed]);
    assert_eq!(
        changed.previous_revision,
        Some(before.result.snapshot.runtime_revision.clone())
    );
    assert_eq!(changed.snapshot, result.snapshot);
    assert_eq!(fixture.engine.current_runtime().result, result);
}

#[test]
fn parser_quarantine_is_counted_and_changes_the_global_digest() {
    let fixture = fixture();
    let before = fixture.engine.runtime_snapshot().expect("runtime").snapshot;
    let mut catalog = (**fixture.manager.current().catalog()).clone();
    catalog.revision =
        CatalogRevision::new(format!("sha256:{}", "1".repeat(64))).expect("catalog revision");
    catalog.source = CatalogSource::Network;
    catalog.state.availability = CatalogAvailability::Ready;
    let provider_id = ProviderId::new("broken-provider").expect("provider ID");
    catalog.providers.insert(
        provider_id.clone(),
        CatalogProviderEntry {
            id: provider_id.clone(),
            record: None,
            quarantine: Some(CatalogQuarantineReason::InvalidCatalogProviderRecord),
        },
    );
    catalog.quarantine.push(CatalogQuarantineEntry {
        provider_id: Some(provider_id.to_string()),
        model_id: None,
        canonical_model_id: None,
        reason: CatalogQuarantineReason::InvalidCatalogProviderRecord,
    });

    let refreshed = fixture
        .engine
        .refresh_catalog(Arc::new(catalog))
        .expect("parser quarantine refresh")
        .snapshot;
    assert_eq!(refreshed.catalog_state.provider_quarantine_count, 1);
    assert_eq!(refreshed.catalog_state.model_quarantine_count, 0);
    assert_ne!(
        refreshed.catalog_state.quarantine_digest,
        before.catalog_state.quarantine_digest
    );
    let provider = refreshed
        .providers
        .iter()
        .find(|provider| provider.id == provider_id)
        .expect("quarantined provider descriptor");
    assert_eq!(
        provider.support.state,
        cookie_agent_protocol::ProviderSupportState::Quarantined
    );
    assert!(refreshed.catalog_state.provider_quarantine_count >= 1);
}

#[test]
fn registry_provider_drift_counts_but_unsupported_provider_does_not() {
    let fixture = fixture();
    let mut drifted = (*bedrock_catalog()).clone();
    drifted.revision =
        CatalogRevision::new(format!("sha256:{}", "2".repeat(64))).expect("catalog revision");
    let provider = drifted
        .providers
        .get_mut(&ProviderId::new("amazon-bedrock").expect("provider ID"))
        .expect("Bedrock provider")
        .record
        .as_mut()
        .expect("Bedrock record");
    provider.shape = Some("unexpected".to_owned());
    let drifted = fixture
        .engine
        .refresh_catalog(Arc::new(drifted))
        .expect("provider drift refresh")
        .snapshot;
    assert_eq!(drifted.catalog_state.provider_quarantine_count, 0);
    assert_eq!(drifted.catalog_state.model_quarantine_count, 0);
    assert_eq!(
        drifted.providers[0].support.state,
        cookie_agent_protocol::ProviderSupportState::Supported
    );
    assert_eq!(
        drifted.providers[0]
            .support
            .reason
            .as_ref()
            .map(cookie_agent_protocol::SafeCode::as_str),
        None
    );

    let mut unsupported = (*bedrock_catalog()).clone();
    unsupported.revision =
        CatalogRevision::new(format!("sha256:{}", "3".repeat(64))).expect("catalog revision");
    let old_id = ProviderId::new("amazon-bedrock").expect("provider ID");
    let unknown_id = ProviderId::new("unknown-provider").expect("provider ID");
    let mut entry = unsupported
        .providers
        .remove(&old_id)
        .expect("provider entry");
    entry.id = unknown_id.clone();
    let record = entry.record.as_mut().expect("provider record");
    record.id = unknown_id.clone();
    record.npm = "@example/unknown-provider".to_owned();
    record.environment.clear();
    unsupported.providers.insert(unknown_id, entry);
    let unsupported = fixture
        .engine
        .refresh_catalog(Arc::new(unsupported))
        .expect("unsupported provider refresh")
        .snapshot;
    assert_eq!(unsupported.catalog_state.provider_quarantine_count, 0);
    assert_eq!(unsupported.catalog_state.model_quarantine_count, 0);
    assert_eq!(
        unsupported.providers[0].support.state,
        cookie_agent_protocol::ProviderSupportState::Unsupported
    );
}

#[test]
fn registry_model_shape_drift_is_counted_with_exact_model_identity() {
    let fixture = fixture();
    let mut catalog = (*bedrock_catalog()).clone();
    catalog.revision =
        CatalogRevision::new(format!("sha256:{}", "4".repeat(64))).expect("catalog revision");
    let provider = catalog
        .providers
        .get_mut(&ProviderId::new("amazon-bedrock").expect("provider ID"))
        .expect("provider")
        .record
        .as_mut()
        .expect("provider record");
    provider
        .models
        .values_mut()
        .next()
        .expect("model")
        .record
        .as_mut()
        .expect("model record")
        .shape = Some("unexpected".to_owned());
    let snapshot = fixture
        .engine
        .refresh_catalog(Arc::new(catalog))
        .expect("model drift refresh")
        .snapshot;
    assert_eq!(snapshot.catalog_state.provider_quarantine_count, 0);
    assert_eq!(snapshot.catalog_state.model_quarantine_count, 0);
    assert!(snapshot.models.is_empty());
}

#[test]
fn nested_endpoint_placeholders_project_setup_and_secret_classification() {
    let fixture = fixture();
    let mut catalog = (*bedrock_catalog()).clone();
    let provider = catalog
        .providers
        .values_mut()
        .next()
        .unwrap()
        .record
        .as_mut()
        .unwrap();
    provider.models.values_mut().next().unwrap().record.as_mut().unwrap().provider = Some(
        cookie_agent_models::catalog::CatalogModelProviderMetadata {
            npm: Some("@ai-sdk/anthropic".to_owned()),
            api: Some("https://${AZURE_COGNITIVE_SERVICES_RESOURCE_NAME}.example/${SERVICE_TOKEN}/anthropic/v1".to_owned()),
            shape: None,
        },
    );
    let snapshot = fixture
        .engine
        .refresh_catalog(Arc::new(catalog))
        .expect("nested placeholder refresh")
        .snapshot;
    let fields = &snapshot.providers[0].setup_fields;
    assert!(fields.iter().any(|field| {
        field.id.as_str() == "azure_cognitive_services_resource_name" && field.safe_to_project
    }));
    assert!(
        fields
            .iter()
            .any(|field| { field.id.as_str() == "service_token" && !field.safe_to_project })
    );
}

#[test]
fn combined_quarantine_digest_is_order_independent_and_notifications_are_coherent() {
    let fixture = fixture();
    let mut catalog = (*bedrock_catalog()).clone();
    catalog.revision =
        CatalogRevision::new(format!("sha256:{}", "5".repeat(64))).expect("catalog revision");
    catalog
        .providers
        .get_mut(&ProviderId::new("amazon-bedrock").expect("provider ID"))
        .expect("provider")
        .record
        .as_mut()
        .expect("provider record")
        .models
        .values_mut()
        .next()
        .expect("model")
        .record
        .as_mut()
        .expect("model record")
        .shape = Some("unexpected".to_owned());
    let parser_provider = ProviderId::new("parser-broken").expect("provider ID");
    catalog.providers.insert(
        parser_provider.clone(),
        CatalogProviderEntry {
            id: parser_provider.clone(),
            record: None,
            quarantine: Some(CatalogQuarantineReason::InvalidCatalogProviderRecord),
        },
    );
    catalog.quarantine = vec![
        CatalogQuarantineEntry {
            provider_id: Some(parser_provider.to_string()),
            model_id: None,
            canonical_model_id: None,
            reason: CatalogQuarantineReason::InvalidCatalogProviderRecord,
        },
        CatalogQuarantineEntry {
            provider_id: Some("amazon-bedrock".to_owned()),
            model_id: Some("parser-model".to_owned()),
            canonical_model_id: None,
            reason: CatalogQuarantineReason::InvalidCatalogModelRecord,
        },
        CatalogQuarantineEntry {
            provider_id: None,
            model_id: None,
            canonical_model_id: Some("canonical-model".to_owned()),
            reason: CatalogQuarantineReason::InvalidCanonicalModelRecord,
        },
    ];
    let mut notifications = fixture.engine.subscribe_runtime_changes();
    let first = fixture
        .engine
        .refresh_catalog(Arc::new(catalog.clone()))
        .expect("combined refresh")
        .snapshot;
    let first_notification = notifications.try_recv().expect("first notification");
    assert_eq!(first_notification.snapshot, first);
    assert_eq!(first.catalog_state.provider_quarantine_count, 1);
    assert_eq!(first.catalog_state.model_quarantine_count, 2);

    catalog.revision =
        CatalogRevision::new(format!("sha256:{}", "6".repeat(64))).expect("catalog revision");
    catalog.quarantine.reverse();
    let reordered = fixture
        .engine
        .refresh_catalog(Arc::new(catalog.clone()))
        .expect("reordered refresh")
        .snapshot;
    let reordered_notification = notifications.try_recv().expect("reordered notification");
    assert_eq!(reordered_notification.snapshot, reordered);
    assert_eq!(
        reordered.catalog_state.quarantine_digest,
        first.catalog_state.quarantine_digest
    );
    assert_eq!(
        reordered.catalog_state.provider_quarantine_count,
        first.catalog_state.provider_quarantine_count
    );
    assert_eq!(
        reordered.catalog_state.model_quarantine_count,
        first.catalog_state.model_quarantine_count
    );

    catalog.revision =
        CatalogRevision::new(format!("sha256:{}", "7".repeat(64))).expect("catalog revision");
    catalog.quarantine.pop();
    let changed = fixture
        .engine
        .refresh_catalog(Arc::new(catalog))
        .expect("changed quarantine refresh")
        .snapshot;
    let changed_notification = notifications.try_recv().expect("changed notification");
    assert_eq!(changed_notification.snapshot, changed);
    assert_ne!(
        changed.catalog_state.quarantine_digest,
        reordered.catalog_state.quarantine_digest
    );
}

#[test]
fn failed_publication_preparation_commits_nothing_and_publishes_nothing() {
    use std::sync::atomic::Ordering;

    let fixture = fixture();
    let initial = fixture.engine.runtime_snapshot().expect("runtime").snapshot;
    let initial_generation = fixture.manager.current().store().generation();
    let mut notifications = fixture.engine.subscribe_runtime_changes();
    fixture
        .engine
        .inner
        .test_hooks
        .publication_failure
        .store(true, Ordering::Release);
    let result = fixture
        .engine
        .disconnect_provider(ProviderDisconnectParams {
            provider_id: ProviderId::new("openai").expect("provider ID"),
            expected_runtime_revision: initial.runtime_revision,
            expected_provider_state_revision: initial.provider_state_revision,
            expected_connection_generation: None,
            client_request_id: ClientRequestId::new("failed-publication").expect("request ID"),
        });
    assert!(matches!(result, Err(EngineError::ModelManager(_))));
    assert_eq!(
        fixture.manager.current().store().generation(),
        initial_generation
    );
    assert!(notifications.try_recv().is_err());
}

#[test]
fn corrupt_matching_manifest_rejects_reopen() {
    let fixture = fixture();
    let runtime = fixture.engine.current_runtime();
    let revision = runtime
        .current_manifest
        .revision
        .as_str()
        .strip_prefix("sha256:")
        .expect("manifest revision");
    let path = fixture
        ._directory
        .path()
        .join("model-snapshots")
        .join(format!("{revision}.json"));
    fs::write(&path, b"{\"schema_version\":1}\n").expect("corrupt manifest");
    let reopened = Engine::open(EngineOptions {
        data_dir: fixture._directory.path().join("other-data"),
        cwd: fixture._directory.path().to_owned(),
        config: fixture.config,
        model_manager: fixture.manager,
        tools: Vec::new(),
        model_snapshot_directory: Some(fixture._directory.path().join("model-snapshots")),
    });
    assert!(matches!(reopened, Err(EngineError::Manifest(_))));
}

#[test]
fn external_store_generation_is_reloaded_before_discovery() {
    let fixture = fixture();
    let current = fixture.manager.current();
    let external = ModelManager::new(
        current.authored().clone(),
        Arc::clone(current.catalog()),
        ProviderStore::open(fixture._directory.path().join("provider-store"))
            .expect("second provider store"),
    )
    .expect("second manager");
    let external_current = external.current();
    external
        .disconnect(
            cookie_agent_models::ProviderDisconnectRequest {
                provider_id: ProviderId::new("openai").expect("provider ID"),
                expected_runtime_revision: external_current.runtime_revision().clone(),
                expected_provider_state_revision: external_current.provider_state_revision(),
                expected_connection_generation: None,
                client_request_id: StoreClientRequestId::new("external-disconnect")
                    .expect("request ID"),
            },
            |_, _| Ok(()),
        )
        .expect("external mutation");
    let mut notifications = fixture.engine.subscribe_runtime_changes();
    let before = fixture
        .engine
        .current_runtime()
        .result
        .snapshot
        .runtime_revision
        .clone();
    let after = fixture
        .engine
        .runtime_snapshot()
        .expect("reloaded snapshot")
        .snapshot;
    assert_ne!(before, after.runtime_revision);
    let changed = notifications.try_recv().expect("reload notification");
    assert_eq!(
        changed.reasons,
        vec![
            RuntimeChangeReason::ProviderStoreChanged,
            RuntimeChangeReason::ProviderStoreReloaded,
        ]
    );
    assert_eq!(changed.previous_revision, Some(before));
    assert_eq!(changed.snapshot.runtime_revision, after.runtime_revision);
}

#[test]
fn engine_attempt_resolution_uses_the_published_executable_handle() {
    let (fixture, selection) = custom_fixture();
    let runtime = fixture.engine.current_runtime();
    let binding = crate::model_snapshots::binding_for_selection(
        &runtime.current_manifest,
        &runtime.models,
        &selection.model,
    )
    .expect("frozen binding");
    let expected = runtime
        .models
        .resolve(&selection.model)
        .expect("published executable");
    let resolved = crate::policy::resolve_model(&binding, &runtime).expect("engine resolution");
    assert!(Arc::ptr_eq(expected.model(), resolved.model()));
}

#[tokio::test]
async fn accepted_root_run_keeps_its_exact_manifest_binding_after_runtime_change() {
    let (fixture, selection) = custom_fixture();
    let session = fixture
        .engine
        .create_session(selection.clone())
        .expect("session");
    let run = fixture
        .engine
        .start_run(
            RunStartParams {
                reset_fallback: false,
                session_id: session.session_id,
                client_run_id: cookie_agent_protocol::ClientRunId::new("immutable-run")
                    .expect("run ID"),
                selection,
                input: "hello".to_owned(),
            },
            cookie_agent_protocol::EventOrigin::new("client:test").unwrap(),
        )
        .await
        .expect("run");
    let (before, _) = fixture
        .engine
        .subscribe(session.session_id, None)
        .await
        .expect("events");
    let frozen = before
        .events
        .iter()
        .find_map(|event| match &event.payload {
            cookie_agent_protocol::EventPayload::RunStarted {
                selected_suffix, ..
            } if event.run_id == Some(run.run_id) => Some(selected_suffix.clone()),
            _ => None,
        })
        .expect("frozen suffix");
    let runtime = fixture.engine.runtime_snapshot().expect("runtime").snapshot;
    fixture
        .engine
        .disconnect_provider(ProviderDisconnectParams {
            provider_id: ProviderId::new("openai").expect("provider ID"),
            expected_runtime_revision: runtime.runtime_revision,
            expected_provider_state_revision: runtime.provider_state_revision,
            expected_connection_generation: None,
            client_request_id: ClientRequestId::new("immutability-change").expect("request ID"),
        })
        .expect("runtime mutation");
    let (after, _) = fixture
        .engine
        .subscribe(session.session_id, None)
        .await
        .expect("events after change");
    let still_frozen = after
        .events
        .iter()
        .find_map(|event| match &event.payload {
            cookie_agent_protocol::EventPayload::RunStarted {
                selected_suffix, ..
            } if event.run_id == Some(run.run_id) => Some(selected_suffix.clone()),
            _ => None,
        })
        .expect("frozen suffix after change");
    assert_eq!(frozen, still_frozen);
    assert_eq!(frozen[0].manifest_revision, session.manifest_revision);
}
