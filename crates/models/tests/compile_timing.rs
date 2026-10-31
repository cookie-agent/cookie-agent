#![cfg(unix)]
//! Manual timing of a full runtime compile against the bundled catalog.
//! Run: cargo test --release -p cookie_agent_models --test compile_timing -- --ignored --nocapture
//! (`test = false` in Cargo.toml: a plain test run neither builds nor links it).

use std::{
    alloc::{GlobalAlloc, Layout, System},
    collections::BTreeMap,
    fs,
    os::unix::fs::PermissionsExt as _,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Instant,
};

use cookie_agent_identity::{AuthFieldName, AuthMethodId, ProviderId};
use cookie_agent_models::{
    catalog::{CatalogManager, CatalogTransport, CatalogTransportFuture, CatalogTransportResponse},
    manager::{ModelManager, ProviderConnectRequest},
    provider_store::{ClientConnectId, ProviderAuthValues, ProviderStore},
    secure_store::SecureDirectory,
};

struct Counting;
static ALLOCS: AtomicUsize = AtomicUsize::new(0);
static BYTES: AtomicUsize = AtomicUsize::new(0);
static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        BYTES.fetch_add(layout.size(), Ordering::Relaxed);
        let live = LIVE.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
        PEAK.fetch_max(live, Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

struct Bundled;
impl CatalogTransport for Bundled {
    fn fetch(&self, etag: Option<String>) -> CatalogTransportFuture<'_> {
        Box::pin(async move {
            Ok(if etag.is_some() {
                CatalogTransportResponse::not_modified()
            } else {
                let mut response = CatalogTransportResponse::from_bytes(
                    200,
                    cookie_agent_models::catalog::MODELS_DEV_BOOTSTRAP.to_vec(),
                );
                response.etag = Some("\"bundled\"".to_owned());
                response
            })
        })
    }
}

fn measure<T>(label: &str, f: impl FnOnce() -> T) -> T {
    let allocs = ALLOCS.load(Ordering::Relaxed);
    let bytes = BYTES.load(Ordering::Relaxed);
    let live = LIVE.load(Ordering::Relaxed);
    PEAK.store(live, Ordering::Relaxed);
    let start = Instant::now();
    let value = f();
    let elapsed = start.elapsed();
    println!(
        "{label:<28} {:>9.2} ms  allocs {:>9}  alloc MB {:>8.1}  peak +MB {:>7.1}  live +MB {:>7.1}",
        elapsed.as_secs_f64() * 1e3,
        ALLOCS.load(Ordering::Relaxed) - allocs,
        (BYTES.load(Ordering::Relaxed) - bytes) as f64 / 1e6,
        (PEAK.load(Ordering::Relaxed) - live) as f64 / 1e6,
        (LIVE.load(Ordering::Relaxed) as f64 - live as f64) / 1e6,
    );
    value
}

fn connect(
    id: &str,
    provider: &str,
    revision: &cookie_agent_identity::CatalogRevision,
) -> ProviderConnectRequest {
    ProviderConnectRequest {
        provider_id: ProviderId::new(provider).unwrap(),
        expected_catalog_revision: revision.clone(),
        setup_values: BTreeMap::new(),
        auth_method: AuthMethodId::new("bearer-api-key-v1").unwrap(),
        auth_values: ProviderAuthValues::new(BTreeMap::from([(
            AuthFieldName::new("api_key").unwrap(),
            "secret".to_owned(),
        )]))
        .unwrap(),
        client_connect_id: ClientConnectId::new(id).unwrap(),
    }
}

#[test]
#[ignore = "manual timing"]
fn time_full_compile_against_bundled_catalog() {
    let temporary = tempfile::tempdir().unwrap();
    fs::set_permissions(temporary.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let catalogs = CatalogManager::new(
        Bundled,
        SecureDirectory::open_in(temporary.path(), "catalog").unwrap(),
    );
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let snapshot = Arc::new(measure("refresh 200 (parse)", || {
        runtime.block_on(catalogs.refresh()).unwrap()
    }));
    let revalidated = measure("refresh 304", || {
        runtime.block_on(catalogs.refresh()).unwrap()
    });
    let cached = measure("load_cached (cache hit)", || {
        catalogs.load_cached().unwrap()
    });
    drop((revalidated, cached));
    let store = ProviderStore::open(temporary.path().join("providers")).unwrap();
    let manager = measure("initial compile", || {
        ModelManager::new(BTreeMap::new(), Arc::clone(&snapshot), store).unwrap()
    });
    let runtime = manager.current();
    println!(
        "providers {} models {}",
        runtime.providers().len(),
        runtime.models().len()
    );
    for (index, provider) in ["openai", "anthropic", "openrouter", "deepseek"]
        .iter()
        .enumerate()
    {
        measure(&format!("connect {provider}"), || {
            manager
                .connect(
                    connect(&format!("c{index}"), provider, &snapshot.revision),
                    |_, _| Ok(()),
                )
                .unwrap()
        });
    }
    let runtime = manager.current();
    println!(
        "available {}",
        runtime
            .models()
            .values()
            .filter(|model| model.model.status
                == cookie_agent_models::compiler::CompiledModelStatus::Available)
            .count()
    );
    measure("reload store (no change)", || {
        manager
            .reload_inputs(BTreeMap::new(), Arc::clone(&snapshot), |_| Ok(()))
            .unwrap()
    });
    measure("manifest payload", || runtime.manifest_payload().unwrap());
}
