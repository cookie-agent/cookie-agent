//! Composing the daemon: its catalog at startup and the hourly refresh.

use super::*;

#[tokio::test]
async fn empty_startup_uses_injected_offline_catalog_and_composes_before_frontend() {
    let temporary = tempfile::tempdir().unwrap();
    private_directory(temporary.path());
    let workspace = temporary.path().join("workspace");
    let cache_anchor = temporary.path().join("cache-anchor");
    let provider_store = temporary.path().join("provider-store");
    let data = temporary.path().join("data");
    write_empty_config(&workspace);
    private_directory(&cache_anchor);
    let fetches = Arc::new(AtomicUsize::new(0));
    let mut runtime = compose_isolated(
        &workspace,
        || {
            Ok(OfflineTransport {
                fetches: Arc::clone(&fetches),
                body: unsupported_catalog(),
            })
        },
        |transport| CatalogManager::in_directory(transport, &cache_anchor, "catalog"),
        || ProviderStore::open(&provider_store),
        || Ok(data),
    )
    .await
    .unwrap();
    // Startup itself never waits on the network.
    assert_eq!(fetches.load(Ordering::SeqCst), 0);
    await_initial_catalog_refresh(&runtime).await;
    assert_eq!(fetches.load(Ordering::SeqCst), 1);
    let snapshot = runtime.engine.runtime_snapshot().unwrap();
    assert!(snapshot.snapshot.models.is_empty());
    // The catalog's one provider and the built-in ChatGPT sign-in.
    assert_eq!(snapshot.snapshot.providers.len(), 2);
    assert!(
        snapshot
            .snapshot
            .providers
            .iter()
            .any(|provider| provider.id.as_str() == "chatgpt"
                && !provider.sign_in_methods.is_empty())
    );
    runtime.server.shutdown();
    runtime.stop_catalog_refresh().await;
    runtime.engine.shutdown().await;
}

#[tokio::test]
async fn leftover_schema_field_fails_before_catalog_or_provider_store_open() {
    let temporary = tempfile::tempdir().unwrap();
    private_directory(temporary.path());
    let workspace = temporary.path().join("workspace");
    write_empty_config(&workspace);
    std::fs::write(
        workspace.join(".cookie-agent/config.toml"),
        "schema_version = 10\n",
    )
    .unwrap();
    let fetches = Arc::new(AtomicUsize::new(0));
    let catalog_opens = Arc::new(AtomicUsize::new(0));
    let provider_opens = Arc::new(AtomicUsize::new(0));
    let catalog_anchor = temporary.path().to_owned();
    let provider_path = temporary.path().join("providers");
    let data_path = temporary.path().join("data");
    let result = compose_isolated(
        &workspace,
        || {
            Ok(OfflineTransport {
                fetches: Arc::clone(&fetches),
                body: unsupported_catalog(),
            })
        },
        {
            let catalog_opens = Arc::clone(&catalog_opens);
            move |transport| {
                catalog_opens.fetch_add(1, Ordering::SeqCst);
                CatalogManager::in_directory(transport, catalog_anchor, "catalog")
            }
        },
        {
            let provider_opens = Arc::clone(&provider_opens);
            move || {
                provider_opens.fetch_add(1, Ordering::SeqCst);
                ProviderStore::open(provider_path)
            }
        },
        || Ok(data_path),
    )
    .await;
    assert!(result.is_err());
    assert_eq!(catalog_opens.load(Ordering::SeqCst), 0);
    assert_eq!(provider_opens.load(Ordering::SeqCst), 0);
    assert_eq!(fetches.load(Ordering::SeqCst), 0);
}

#[tokio::test(start_paused = true)]
async fn hourly_catalog_refresh_skips_noops_publishes_fallback_once_and_cancels() {
    let temporary = tempfile::tempdir().unwrap();
    private_directory(temporary.path());
    let workspace = temporary.path().join("workspace");
    let cache_anchor = temporary.path().join("cache-anchor");
    write_empty_config(&workspace);
    private_directory(&cache_anchor);
    let fetches = Arc::new(AtomicUsize::new(0));
    let steps = Arc::new(Mutex::new(VecDeque::from([
        CatalogStep::Body(unsupported_catalog()),
        CatalogStep::NotModified,
        CatalogStep::Fail,
        CatalogStep::NotModified,
        CatalogStep::NotModified,
    ])));
    let mut runtime = compose_isolated(
        &workspace,
        || {
            Ok(ScriptedTransport {
                fetches: Arc::clone(&fetches),
                steps: Arc::clone(&steps),
            })
        },
        |transport| CatalogManager::in_directory(transport, &cache_anchor, "catalog"),
        || ProviderStore::open(temporary.path().join("providers")),
        || Ok(temporary.path().join("data")),
    )
    .await
    .unwrap();
    let mut changes = runtime.engine.subscribe_runtime_changes();
    // The first refresh runs right after startup, replacing the bundled catalog.
    await_initial_catalog_refresh(&runtime).await;
    catalog_refresh_settled().await;
    assert_eq!(fetches.load(Ordering::SeqCst), 1);
    let changed = changes.recv().await.unwrap();
    assert_eq!(
        changed.reasons,
        vec![cookie_agent_protocol::RuntimeChangeReason::CatalogRefreshed]
    );

    tokio::time::advance(CATALOG_REFRESH_INTERVAL).await;
    tokio::task::yield_now().await;
    assert_eq!(fetches.load(Ordering::SeqCst), 2);
    assert!(matches!(
        changes.try_recv(),
        Err(tokio::sync::broadcast::error::TryRecvError::Empty)
    ));

    tokio::time::advance(CATALOG_REFRESH_INTERVAL).await;
    tokio::task::yield_now().await;
    let changed = changes.recv().await.unwrap();
    assert_eq!(
        changed.reasons,
        vec![cookie_agent_protocol::RuntimeChangeReason::CatalogFallback]
    );
    catalog_refresh_settled().await;

    tokio::time::advance(CATALOG_REFRESH_INTERVAL).await;
    tokio::task::yield_now().await;
    let changed = changes.recv().await.unwrap();
    assert_eq!(
        changed.reasons,
        vec![cookie_agent_protocol::RuntimeChangeReason::CatalogRefreshed]
    );
    catalog_refresh_settled().await;

    tokio::time::advance(CATALOG_REFRESH_INTERVAL).await;
    tokio::task::yield_now().await;
    assert!(matches!(
        changes.try_recv(),
        Err(tokio::sync::broadcast::error::TryRecvError::Empty)
    ));
    let before_shutdown = fetches.load(Ordering::SeqCst);
    runtime.stop_catalog_refresh().await;
    tokio::time::advance(CATALOG_REFRESH_INTERVAL).await;
    tokio::task::yield_now().await;
    assert_eq!(fetches.load(Ordering::SeqCst), before_shutdown);
    runtime.server.shutdown();
    runtime.engine.shutdown().await;
}
