//! `cookie connect` and `cookie disconnect` against a daemon in this
//! process.

use super::*;

#[test]
fn provider_connect_requires_support_before_submission() {
    ensure_supported(&provider(ProviderSupportState::Supported)).unwrap();
    assert!(ensure_supported(&provider(ProviderSupportState::Unsupported)).is_err());
    assert!(ensure_supported(&provider(ProviderSupportState::Quarantined)).is_err());

    let mut removed = provider(ProviderSupportState::Supported);
    removed.presence = ProviderPresence::Removed;
    ensure_supported(&removed).unwrap();

    removed.support = ProviderSupport {
        state: ProviderSupportState::Unsupported,
        reason: Some(SafeCode::new("removed_without_retained_recipe_match").unwrap()),
    };
    assert!(
        ensure_supported(&removed)
            .unwrap_err()
            .to_string()
            .contains("removed_without_retained_recipe_match")
    );
}

#[test]
fn authored_configuration_is_not_treated_as_connected_when_auth_is_incomplete() {
    let mut authored = provider(ProviderSupportState::Supported);
    authored.configuration = ProviderConfigurationState::Authored;
    authored.effective_auth_state = EffectiveAuthState::Unavailable;
    assert_eq!(
        connect_confirmation_prompt(&authored),
        "Complete setup and authentication for this authored provider? [y/N] "
    );
}

#[test]
fn authored_complete_provider_is_offered_global_store_connect_not_reconnect() {
    let mut authored = provider(ProviderSupportState::Supported);
    authored.configuration = ProviderConfigurationState::Authored;
    authored.effective_auth_state = EffectiveAuthState::AuthoredApiKey;
    authored.durable_connection = None;
    assert_eq!(
        connect_confirmation_prompt(&authored),
        "Connect this provider? [y/N] "
    );
}

#[test]
fn setup_and_auth_prompts_are_separate_and_blank_secret_is_rejected() {
    let provider = provider(ProviderSupportState::Supported);
    let mut public_prompts = Vec::new();
    let setup = collect_setup_values(&provider, |prompt| {
        public_prompts.push(prompt.to_owned());
        Ok(String::new())
    })
    .unwrap();
    assert_eq!(setup.len(), 1);
    assert!(public_prompts[0].contains("Region"));

    let mut secret_prompts = Vec::new();
    let error = collect_auth_values(&provider.auth_methods[0], |prompt| {
        secret_prompts.push(prompt.to_owned());
        Ok(Zeroizing::new(String::new()))
    })
    .unwrap_err();
    assert!(error.to_string().contains("was blank"));
    assert!(secret_prompts[0].contains("secret"));
}

fn unusable_provider() -> ProviderDescriptor {
    let mut provider = provider(ProviderSupportState::Supported);
    provider.configuration = ProviderConfigurationState::Stored;
    provider.documentation_url = Some(SafeDisplayText::new("https://example.test/docs").unwrap());
    provider.environment = vec![
        cookie_agent_protocol::CredentialFieldName::new("OPENAI_ORG").unwrap(),
        cookie_agent_protocol::CredentialFieldName::new("OPENAI_API_KEY").unwrap(),
    ];
    provider.model_counts = cookie_agent_protocol::ProviderModelCounts {
        quarantined: 1,
        unsupported: 1,
        ..Default::default()
    };
    provider.unavailable_models = serde_json::from_value(serde_json::json!([
        {"id": "gpt-q", "display_name": "gpt-q", "kind": "quarantined", "reason": "invalid_catalog_model_record"},
        {"id": "gpt-u", "display_name": "GPT U", "kind": "unsupported", "reason": "unsupported_model_capabilities"}
    ]))
    .unwrap();
    provider
}

#[test]
fn provider_listing_and_details_explain_unavailable_models() {
    let providers = vec![unusable_provider()];
    let mut io = ScriptedConnectIo {
        public: VecDeque::from(["1".to_owned()]),
        ..Default::default()
    };
    let chosen = choose_provider(&providers, None, &mut io).unwrap();
    print_provider_details(chosen, "sha256:catalog", &mut io).unwrap();
    for line in [
        "  1. OpenAI (openai) — supported · models: 0 usable, 1 quarantined, 1 unsupported",
        "Models: 0 usable, 1 quarantined, 1 unsupported",
        "  gpt-q — quarantined: invalid_catalog_model_record",
        "  gpt-u — unsupported: unsupported_model_capabilities",
        "Docs: https://example.test/docs",
        "Config alternative: [providers.openai] source = \"models_dev\", api_key = \"${env:OPENAI_API_KEY}\"",
    ] {
        assert!(
            io.output.iter().any(|output| output == line),
            "missing {line:?}: {:#?}",
            io.output
        );
    }
}

#[test]
fn headless_model_errors_name_the_unavailable_reason() {
    let providers = vec![unusable_provider()];
    assert_eq!(
        cookie_agent::availability::model_unavailable_reason(
            &providers,
            &"openai/gpt-q".parse().unwrap()
        )
        .as_deref(),
        Some("quarantined: invalid_catalog_model_record")
    );
    assert_eq!(
        cookie_agent::availability::model_unavailable_reason(
            &providers,
            &"openai/other".parse().unwrap()
        )
        .as_deref(),
        Some("provider `openai` has no usable models (0 usable, 1 quarantined, 1 unsupported)")
    );
    assert_eq!(
        cookie_agent::availability::unusable_configured_providers(&providers).as_deref(),
        Some(
            "configured providers without usable models: openai (0 usable, 1 quarantined, 1 unsupported)"
        )
    );
    assert!(
        cookie_agent::availability::model_unavailable_reason(
            &providers,
            &"elsewhere/model".parse().unwrap()
        )
        .is_none()
    );
}

#[test]
fn typed_credentials_become_protocol_credentials_and_the_source_is_wiped() {
    let before = SECRET_VALUES_WIPED.load(Ordering::SeqCst);
    let credentials = SecretValues(BTreeMap::from([(
        "api_key".into(),
        "invented-test-placeholder".into(),
    )]))
    .into_credentials()
    .expect("credentials");
    assert_eq!(
        credentials.get(&cookie_agent_protocol::AuthFieldName::new("api_key").unwrap()),
        Some("invented-test-placeholder")
    );
    assert!(SECRET_VALUES_WIPED.load(Ordering::SeqCst) > before);
}

#[tokio::test]
async fn global_setup_and_auth_provider_recomposes_across_two_workspaces() {
    let temporary = tempfile::tempdir().unwrap();
    private_directory(temporary.path());
    let workspace_a = temporary.path().join("workspace-a");
    let workspace_b = temporary.path().join("workspace-b");
    let cache_anchor = temporary.path().join("cache-anchor");
    let provider_path = temporary.path().join("provider-store");
    let data_path = temporary.path().join("data");
    write_config_without_providers(&workspace_a);
    write_config_without_providers(&workspace_b);
    private_directory(&cache_anchor);
    let fetches = Arc::new(AtomicUsize::new(0));

    let seed_catalog = Arc::new(
        CatalogManager::in_directory(
            OfflineTransport {
                fetches: Arc::clone(&fetches),
                body: bedrock_catalog(),
            },
            &cache_anchor,
            "catalog",
        )
        .refresh()
        .await
        .unwrap(),
    );
    let provider_id = ProviderId::new("amazon-bedrock").unwrap();
    let authored = cookie_agent_models::ProviderDefinition::ModelsDev(
        cookie_agent_models::authoring::ModelsDevProvider {
            base_url: None,
            setup: BTreeMap::from([(
                SetupFieldId::new("region").unwrap(),
                cookie_agent_models::SafeSetupValue::String(
                    cookie_agent_models::BoundedSetupString::new("us-test-1").unwrap(),
                ),
            )]),
            api_key: None,
            auth_override: None,
            cache: None,
            headers: BTreeMap::new(),
            model_overrides: BTreeMap::new(),
        },
    );
    let seed_manager = ModelManager::new(
        BTreeMap::from([(provider_id.clone(), authored)]),
        Arc::clone(&seed_catalog),
        ProviderStore::open(&provider_path).unwrap(),
    )
    .unwrap();
    let seed_request = cookie_agent_models::ProviderConnectRequest {
        provider_id: provider_id.clone(),
        expected_catalog_revision: seed_catalog.revision.clone(),
        setup_values: BTreeMap::from([(
            SetupFieldId::new("region").unwrap(),
            cookie_agent_models::SafeSetupValue::String(
                cookie_agent_models::BoundedSetupString::new("us-test-1").unwrap(),
            ),
        )]),
        auth_method: cookie_agent_protocol::AuthMethodId::new("aws-sigv4-credentials-v1").unwrap(),
        auth_values: cookie_agent_models::provider_store::ProviderAuthValues::new(BTreeMap::from(
            [
                (
                    cookie_agent_protocol::AuthFieldName::new("access_key_id").unwrap(),
                    "invented-access-placeholder".into(),
                ),
                (
                    cookie_agent_protocol::AuthFieldName::new("secret_access_key").unwrap(),
                    "invented-secret-placeholder".into(),
                ),
            ],
        ))
        .unwrap(),
        client_connect_id: cookie_agent_models::provider_store::ClientConnectId::new(
            "store-backed-connect-1",
        )
        .unwrap(),
    };
    let seeded = seed_manager
        .connect(seed_request.clone(), |_, _| Ok(()))
        .unwrap();
    assert!(!seeded.replayed);
    assert!(
        seed_manager
            .connect(seed_request, |_, _| Ok(()))
            .unwrap()
            .replayed
    );
    drop(seed_manager);

    let mut runtime = compose_isolated(
        &workspace_a,
        || {
            Ok(OfflineTransport {
                fetches: Arc::clone(&fetches),
                body: bedrock_catalog(),
            })
        },
        |transport| CatalogManager::in_directory(transport, &cache_anchor, "catalog"),
        || ProviderStore::open(&provider_path),
        || Ok(data_path.clone()),
    )
    .await
    .unwrap();
    let snapshot_a = runtime.engine.runtime_snapshot().unwrap().snapshot;
    let provider_a = snapshot_a
        .providers
        .iter()
        .find(|provider| provider.id == provider_id)
        .unwrap();
    assert_eq!(
        provider_a
            .durable_connection
            .as_ref()
            .unwrap()
            .setup_values
            .get(&SetupFieldId::new("region").unwrap())
            .map(setup_value_text),
        Some("us-test-1".into())
    );
    assert_eq!(
        provider_a
            .durable_connection
            .as_ref()
            .unwrap()
            .credential_fields
            .len(),
        2
    );
    runtime.server.shutdown();
    runtime.stop_catalog_refresh().await;
    runtime.engine.shutdown().await;

    let mut recomposed = compose_isolated(
        &workspace_b,
        || {
            Ok(OfflineTransport {
                fetches,
                body: bedrock_catalog(),
            })
        },
        |transport| CatalogManager::in_directory(transport, &cache_anchor, "catalog"),
        || ProviderStore::open(&provider_path),
        || Ok(data_path),
    )
    .await
    .unwrap();
    let snapshot = recomposed.engine.runtime_snapshot().unwrap().snapshot;
    let provider = snapshot
        .providers
        .iter()
        .find(|provider| provider.id.as_str() == "amazon-bedrock")
        .unwrap();
    assert!(provider.durable_connection.is_some());
    assert_eq!(provider.configuration, ProviderConfigurationState::Stored);
    assert_eq!(
        provider
            .durable_connection
            .as_ref()
            .unwrap()
            .credential_fields
            .len(),
        2
    );
    recomposed.server.shutdown();
    recomposed.stop_catalog_refresh().await;
    recomposed.engine.shutdown().await;
}

#[tokio::test]
async fn connect_reconnect_and_disconnect_are_idempotent() {
    let temporary = tempfile::tempdir().unwrap();
    private_directory(temporary.path());
    let workspace = temporary.path().join("workspace");
    let cache_anchor = temporary.path().join("cache-anchor");
    write_empty_config(&workspace);
    private_directory(&cache_anchor);
    let mut runtime = compose_isolated(
        &workspace,
        || {
            Ok(OfflineTransport {
                fetches: Arc::new(AtomicUsize::new(0)),
                body: openai_catalog(),
            })
        },
        |transport| CatalogManager::in_directory(transport, &cache_anchor, "catalog"),
        || ProviderStore::open(temporary.path().join("providers")),
        || Ok(temporary.path().join("data")),
    )
    .await
    .unwrap();
    let snapshot = runtime.engine.runtime_snapshot().unwrap().snapshot;
    let connect_json = serde_json::json!({
        "provider_id": "openai",
        "expected_revision": snapshot.revision,
        "setup_values": {},
        "auth_method": "bearer-api-key-v1",
        "auth_values": {"api_key": "invented-test-placeholder"},
        "client_connect_id": "connect-replay-1"
    });
    let connected = runtime
        .engine
        .connect_provider(serde_json::from_value(connect_json.clone()).unwrap())
        .unwrap();
    assert!(!connected.replayed);
    assert!(
        runtime
            .engine
            .connect_provider(serde_json::from_value(connect_json).unwrap())
            .unwrap()
            .replayed
    );
    let disconnect = ProviderDisconnectParams {
        provider_id: ProviderId::new("openai").unwrap(),
        expected_revision: connected.runtime.revision,
        expected_connection_generation: Some(connected.durable_connection.connection_generation),
        client_request_id: ClientRequestId::new("disconnect-replay-1").unwrap(),
    };
    assert!(
        !runtime
            .engine
            .disconnect_provider(disconnect.clone())
            .unwrap()
            .replayed
    );
    assert!(
        runtime
            .engine
            .disconnect_provider(disconnect)
            .unwrap()
            .replayed
    );
    runtime.server.shutdown();
    runtime.stop_catalog_refresh().await;
    runtime.engine.shutdown().await;
}

/// A catalog refresh between reading the snapshot and connecting is retried
/// against the new revision, unless the provider's form changed with it.
#[tokio::test]
async fn connect_retries_a_catalog_conflict_when_the_provider_form_is_unchanged() {
    let temporary = tempfile::tempdir().unwrap();
    private_directory(temporary.path());
    let workspace = temporary.path().join("workspace");
    let cache_anchor = temporary.path().join("cache-anchor");
    write_empty_config(&workspace);
    private_directory(&cache_anchor);
    let mut runtime = compose_isolated(
        &workspace,
        || {
            Ok(OfflineTransport {
                fetches: Arc::new(AtomicUsize::new(0)),
                body: openai_catalog(),
            })
        },
        |transport| CatalogManager::in_directory(transport, &cache_anchor, "catalog"),
        || ProviderStore::open(temporary.path().join("providers")),
        || Ok(temporary.path().join("data")),
    )
    .await
    .unwrap();
    await_initial_catalog_refresh(&runtime).await;
    let stale = runtime.engine.runtime_snapshot().unwrap().snapshot;
    let provider = stale
        .providers
        .iter()
        .find(|provider| provider.id.as_str() == "openai")
        .unwrap()
        .clone();
    // The same provider under a new catalog revision: a model was renamed.
    let renamed = String::from_utf8(openai_catalog().to_vec())
        .unwrap()
        .replace(r#""name":"GPT Test""#, r#""name":"GPT Renamed""#);
    let refreshed = CatalogManager::in_directory(
        OfflineTransport {
            fetches: Arc::new(AtomicUsize::new(0)),
            body: Arc::new(renamed.into_bytes()),
        },
        &cache_anchor,
        "refreshed-catalog",
    )
    .refresh()
    .await
    .unwrap();
    runtime.engine.refresh_catalog(Arc::new(refreshed)).unwrap();
    assert_ne!(
        runtime.engine.runtime_snapshot().unwrap().snapshot.revision,
        stale.revision
    );

    let (client_stream, server_stream) = cookie_agent_protocol::in_process_pair(32);
    let server_task = tokio::spawn(runtime.server.clone().serve_stream(
        server_stream,
        cookie_agent_protocol::Authentication::Authenticated,
    ));
    let client = Client::connect_stream(client_stream);
    client.handshake().await.unwrap();
    let params = || cookie_agent_protocol::ProviderConnectParams {
        provider_id: provider.id.clone(),
        expected_revision: stale.revision.clone(),
        setup_values: BTreeMap::new(),
        auth_method: cookie_agent_protocol::AuthMethodId::new("bearer-api-key-v1").unwrap(),
        auth_values: serde_json::from_value(serde_json::json!({"api_key": "placeholder"})).unwrap(),
        client_connect_id: ClientConnectId::new(Uuid::now_v7().to_string()).unwrap(),
    };
    // A form that no longer matches the provider is not retried.
    let mut changed = provider.clone();
    changed.auth_methods.clear();
    let error = client
        .connect_provider_retrying(params(), &changed)
        .await
        .expect_err("a changed form is not retried");
    assert_eq!(error.error_code(), Some("catalog_revision_conflict"));
    let connected = client
        .connect_provider_retrying(params(), &provider)
        .await
        .expect("retried against the new catalog");
    assert_eq!(connected.durable_connection.provider_id, provider.id);
    drop(client);
    server_task.await.unwrap().unwrap();
    runtime.server.shutdown();
    runtime.stop_catalog_refresh().await;
    runtime.engine.shutdown().await;
}

#[tokio::test]
async fn cli_reconnects_supported_removed_provider_through_real_server() {
    let temporary = tempfile::tempdir().unwrap();
    private_directory(temporary.path());
    let workspace = temporary.path().join("workspace");
    let cache_anchor = temporary.path().join("cache-anchor");
    write_empty_config(&workspace);
    private_directory(&cache_anchor);
    let mut runtime = compose_isolated(
        &workspace,
        || {
            Ok(OfflineTransport {
                fetches: Arc::new(AtomicUsize::new(0)),
                body: openai_catalog(),
            })
        },
        |transport| CatalogManager::in_directory(transport, &cache_anchor, "catalog"),
        || ProviderStore::open(temporary.path().join("providers")),
        || Ok(temporary.path().join("data")),
    )
    .await
    .unwrap();
    await_initial_catalog_refresh(&runtime).await;
    let initial = runtime.engine.runtime_snapshot().unwrap().snapshot;
    let connected = runtime
        .engine
        .connect_provider(
            serde_json::from_value(serde_json::json!({
                "provider_id": "openai",
                "expected_revision": initial.revision,
                "setup_values": {},
                "auth_method": "bearer-api-key-v1",
                "auth_values": {"api_key": "invented-initial-placeholder"},
                "client_connect_id": "removed-provider-initial-connect"
            }))
            .unwrap(),
        )
        .unwrap();
    let initial_generation = connected.durable_connection.connection_generation;

    let removed = CatalogManager::in_directory(
        OfflineTransport {
            fetches: Arc::new(AtomicUsize::new(0)),
            body: removed_openai_catalog(),
        },
        &cache_anchor,
        "removed-catalog",
    )
    .refresh()
    .await
    .unwrap();
    runtime.engine.refresh_catalog(Arc::new(removed)).unwrap();
    let snapshot = runtime.engine.runtime_snapshot().unwrap().snapshot;
    let removed_provider = snapshot
        .providers
        .iter()
        .find(|provider| provider.id.as_str() == "openai")
        .unwrap();
    assert_eq!(removed_provider.presence, ProviderPresence::Removed);
    assert_eq!(
        removed_provider.support.state,
        ProviderSupportState::Supported
    );

    let before_wipe = SECRET_VALUES_WIPED.load(Ordering::SeqCst);
    let (client_stream, server_stream) = cookie_agent_protocol::in_process_pair(32);
    let server_task = tokio::spawn(runtime.server.clone().serve_stream(
        server_stream,
        cookie_agent_protocol::Authentication::Authenticated,
    ));
    let client = Client::connect_stream(client_stream);
    let mut io = ScriptedConnectIo {
        public: VecDeque::from(["openai".into(), "yes".into()]),
        secrets: VecDeque::from([Zeroizing::new("invented-reconnect-placeholder".into())]),
        output: Vec::new(),
    };
    run_connect_with(&client, None, &mut io).await.unwrap();
    let output = io.output.join("\n");
    assert!(output.contains("openai"));
    assert!(output.contains("Presence: Removed"));
    assert!(output.contains("provider.connect succeeded"));
    assert!(SECRET_VALUES_WIPED.load(Ordering::SeqCst) > before_wipe);
    drop(client);
    server_task.await.unwrap().unwrap();
    let reconnected = runtime
        .engine
        .runtime_snapshot()
        .unwrap()
        .snapshot
        .providers
        .into_iter()
        .find(|provider| provider.id.as_str() == "openai")
        .unwrap()
        .durable_connection
        .unwrap();
    assert!(reconnected.connection_generation > initial_generation);

    let blocked = runtime
        .engine
        .runtime_snapshot()
        .unwrap()
        .snapshot
        .providers;
    assert_eq!(
        blocked
            .iter()
            .find(|provider| provider.id.as_str() == "test")
            .unwrap()
            .support
            .state,
        ProviderSupportState::Supported
    );
    for (provider_id, state) in [("broken", ProviderSupportState::Quarantined)] {
        let descriptor = blocked
            .iter()
            .find(|provider| provider.id.as_str() == provider_id)
            .unwrap();
        assert_eq!(descriptor.support.state, state);
        let reason = descriptor
            .support
            .reason
            .as_ref()
            .unwrap()
            .as_str()
            .to_owned();
        let (client_stream, server_stream) = cookie_agent_protocol::in_process_pair(32);
        let server_task = tokio::spawn(runtime.server.clone().serve_stream(
            server_stream,
            cookie_agent_protocol::Authentication::Authenticated,
        ));
        let client = Client::connect_stream(client_stream);
        let mut io = ScriptedConnectIo::default();
        let error = run_connect_with(&client, Some(provider_id.into()), &mut io)
            .await
            .unwrap_err();
        assert!(error.to_string().contains(&reason), "{error:#}");
        assert!(
            io.output
                .iter()
                .any(|line| line.contains(&format!("Support: {state:?}")))
        );
        assert!(io.secrets.is_empty());
        drop(client);
        server_task.await.unwrap().unwrap();
    }

    runtime.server.shutdown();
    runtime.stop_catalog_refresh().await;
    runtime.engine.shutdown().await;
}
