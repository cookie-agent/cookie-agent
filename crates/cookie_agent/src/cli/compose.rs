//! Composing the daemon in this process: configuration, the models.dev
//! catalog and its refresh loop, providers, the engine, and the official
//! agents and skills kept up to date.

use std::{
    env,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::Context as _;
use cookie_agent_engine::{Engine, EngineOptions};
use cookie_agent_models::{
    ModelManager,
    catalog::{CatalogManager, CatalogTransport, HttpCatalogTransport},
    provider_store::ProviderStore,
};
use cookie_agent_protocol::paths;
use cookie_agent_server::Server;
use tokio_util::sync::CancellationToken;

use super::data_dir;

pub(super) const CATALOG_REFRESH_INTERVAL: Duration = Duration::from_secs(60 * 60);

pub(super) enum RunCatalogTransport {
    Http(HttpCatalogTransport),
    #[cfg(debug_assertions)]
    Bundled,
}

impl CatalogTransport for RunCatalogTransport {
    fn fetch(
        &self,
        etag: Option<String>,
    ) -> cookie_agent_models::catalog::CatalogTransportFuture<'_> {
        match self {
            Self::Http(transport) => transport.fetch(etag),
            #[cfg(debug_assertions)]
            Self::Bundled => Box::pin(async {
                Ok(
                    cookie_agent_models::catalog::CatalogTransportResponse::from_bytes(
                        200,
                        cookie_agent_models::catalog::MODELS_DEV_BOOTSTRAP.to_vec(),
                    ),
                )
            }),
        }
    }
}

pub(super) fn run_catalog_transport() -> anyhow::Result<RunCatalogTransport> {
    #[cfg(debug_assertions)]
    if matches!(
        env::var("COOKIE_AGENT_TEST_BUNDLED_CATALOG").as_deref(),
        Ok("1")
    ) {
        return Ok(RunCatalogTransport::Bundled);
    }
    HttpCatalogTransport::new()
        .context("construct fixed catalog transport")
        .map(RunCatalogTransport::Http)
}

pub(super) struct Runtime {
    pub(super) engine: Engine,
    pub(super) server: Arc<Server>,
    pub(super) port: u16,
    /// `[server] token`, when the configuration has one.
    pub(super) token: Option<String>,
    /// The user `config.toml`, where the daemon keeps a token it creates.
    pub(super) config_path: Option<PathBuf>,
    pub(super) catalog_refresh_shutdown: CancellationToken,
    pub(super) catalog_refresh_task: Option<tokio::task::JoinHandle<()>>,
    /// The official skills' refresh loop, stopped with the catalog's.
    pub(super) official_skills_task: Option<tokio::task::JoinHandle<()>>,
    /// The official agents' refresh loop, stopped with the catalog's.
    pub(super) official_agents_task: Option<tokio::task::JoinHandle<()>>,
}

impl Runtime {
    pub(super) async fn stop_catalog_refresh(&mut self) {
        self.catalog_refresh_shutdown.cancel();
        if let Some(task) = self.catalog_refresh_task.take() {
            let _ = task.await;
        }
        if let Some(task) = self.official_skills_task.take() {
            let _ = task.await;
        }
        if let Some(task) = self.official_agents_task.take() {
            let _ = task.await;
        }
    }

    /// Starts keeping the official agents up to date: at once, then every
    /// [`cookie_agent_engine::OFFICIAL_AGENTS_REFRESH`]. A refresh that
    /// changes them reloads the configuration.
    pub(super) fn start_official_agents(&mut self) {
        // Debug builds under test keep off the network unless a test serves
        // its own source.
        #[cfg(debug_assertions)]
        {
            if let Ok(url) = env::var("COOKIE_AGENT_TEST_OFFICIAL_AGENTS_URL") {
                self.engine.set_official_agents_url(url);
            } else if matches!(
                env::var("COOKIE_AGENT_TEST_BUNDLED_CATALOG").as_deref(),
                Ok("1")
            ) {
                return;
            }
        }
        let engine = self.engine.clone();
        let shutdown = self.catalog_refresh_shutdown.clone();
        self.official_agents_task = Some(tokio::spawn(async move {
            loop {
                tokio::select! {
                    () = shutdown.cancelled() => return,
                    _ = engine.refresh_official_agents() => {}
                }
                tokio::select! {
                    () = shutdown.cancelled() => return,
                    () = tokio::time::sleep(cookie_agent_engine::OFFICIAL_AGENTS_REFRESH) => {}
                }
            }
        }));
    }

    /// Starts keeping the official skills up to date: at once, then every
    /// [`cookie_agent_engine::OFFICIAL_SKILLS_REFRESH`]. A refresh that
    /// changes them reloads the daemon's skills from the user roots.
    pub(super) fn start_official_skills(&mut self) {
        let home = paths::home_dir().ok();
        self.engine
            .set_user_skill_roots(cookie_agent_engine::UserSkillRoots {
                shared: home.as_ref().map(|home| home.join(".agents/skills")),
                user: paths::user_data_root().ok().map(|root| root.join("skills")),
            });
        // Debug builds under test keep off the network unless a test serves
        // its own source.
        #[cfg(debug_assertions)]
        {
            if let Ok(url) = env::var("COOKIE_AGENT_TEST_OFFICIAL_SKILLS_URL") {
                self.engine.set_official_skills_url(url);
            } else if matches!(
                env::var("COOKIE_AGENT_TEST_BUNDLED_CATALOG").as_deref(),
                Ok("1")
            ) {
                return;
            }
        }
        self.official_skills_task = Some(tokio::spawn(run_official_skills_loop(
            self.engine.clone(),
            self.catalog_refresh_shutdown.clone(),
            cookie_agent_engine::OFFICIAL_SKILLS_REFRESH,
        )));
    }
}

async fn run_official_skills_loop(engine: Engine, shutdown: CancellationToken, cadence: Duration) {
    loop {
        tokio::select! {
            () = shutdown.cancelled() => return,
            _ = engine.refresh_official_skills() => {}
        }
        tokio::select! {
            () = shutdown.cancelled() => return,
            () = tokio::time::sleep(cadence) => {}
        }
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        self.catalog_refresh_shutdown.cancel();
    }
}

pub(super) async fn compose(workspace: &Path) -> anyhow::Result<Runtime> {
    let mut runtime = compose_with(
        workspace,
        || HttpCatalogTransport::new().context("construct fixed catalog transport"),
        CatalogManager::standard,
        ProviderStore::standard,
        data_dir,
    )
    .await?;
    runtime.start_official_skills();
    runtime.start_official_agents();
    Ok(runtime)
}

/// [`compose`] with the configuration already loaded.
pub(super) async fn compose_loaded(
    workspace: &Path,
    configuration: cookie_agent_config::LoadedConfiguration,
) -> anyhow::Result<Runtime> {
    let mut runtime = compose_with_configuration(
        workspace,
        configuration,
        || HttpCatalogTransport::new().context("construct fixed catalog transport"),
        CatalogManager::standard,
        ProviderStore::standard,
        data_dir,
    )
    .await?;
    runtime.start_official_skills();
    runtime.start_official_agents();
    Ok(runtime)
}

pub(super) async fn compose_with<T: CatalogTransport + 'static>(
    workspace: &Path,
    open_transport: impl FnOnce() -> anyhow::Result<T>,
    open_catalog: impl FnOnce(T) -> CatalogManager<T>,
    open_provider_store: impl FnOnce() -> Result<
        ProviderStore,
        cookie_agent_models::provider_store::ProviderStoreError,
    >,
    open_data_dir: impl FnOnce() -> anyhow::Result<PathBuf>,
) -> anyhow::Result<Runtime> {
    let configuration =
        cookie_agent_config::load().context("load user configuration, agents, and skills")?;
    compose_with_configuration(
        workspace,
        configuration,
        open_transport,
        open_catalog,
        open_provider_store,
        open_data_dir,
    )
    .await
}

pub(super) async fn compose_with_configuration<T: CatalogTransport + 'static>(
    workspace: &Path,
    configuration: cookie_agent_config::LoadedConfiguration,
    open_transport: impl FnOnce() -> anyhow::Result<T>,
    open_catalog: impl FnOnce(T) -> CatalogManager<T>,
    open_provider_store: impl FnOnce() -> Result<
        ProviderStore,
        cookie_agent_models::provider_store::ProviderStoreError,
    >,
    open_data_dir: impl FnOnce() -> anyhow::Result<PathBuf>,
) -> anyhow::Result<Runtime> {
    if configuration.runtime.server.host != "127.0.0.1" {
        anyhow::bail!("server.host must be exactly 127.0.0.1");
    }
    let port = configuration.runtime.server.port;
    let token = configuration.runtime.server.token.clone();
    let config_path = configuration.config_paths.user.clone();

    // Startup never waits on the network: it serves the validated cache (or
    // the bundled catalog) and the refresh loop checks models.dev right away.
    let catalog_manager = open_catalog(open_transport()?);
    let catalog = Arc::new(
        catalog_manager
            .load_cached()
            .context("load cached models.dev catalog")?,
    );
    let provider_store = open_provider_store().context("open provider store 3")?;
    // A sign-in record this build cannot read leaves ChatGPT out rather than
    // the daemon down.
    let chatgpt =
        match cookie_agent_models::chatgpt::ChatGptCredentials::open_beside(&provider_store) {
            Ok(credentials) => Some(cookie_agent_models::manager::ChatGptManagerInputs {
                models: credentials.cached_models(),
                credentials,
            }),
            Err(error) => {
                eprintln!("cookie-agent: ChatGPT sign-in unavailable: {error}");
                None
            }
        };
    let model_manager = Arc::new(
        ModelManager::new_with_chatgpt(
            configuration.runtime.providers.clone(),
            configuration.runtime.headers.clone(),
            catalog,
            provider_store,
            chatgpt,
        )
        .context("compose effective providers and current model runtime")?,
    );
    let engine = Engine::open_async(EngineOptions {
        data_dir: open_data_dir()?,
        cwd: workspace.to_owned(),
        config: configuration,
        model_manager,
        runtimes: cookie_agent_coding_runtime::runtime_options(workspace),
    })
    .await
    .context("open manifests, rehydrate project state, and reconcile engine")?;
    let server = Arc::new(Server::new(engine.clone()));
    let catalog_refresh_shutdown = CancellationToken::new();
    let catalog_refresh_task = tokio::spawn(run_catalog_refresh_loop(
        catalog_manager,
        engine.clone(),
        catalog_refresh_shutdown.clone(),
        CATALOG_REFRESH_INTERVAL,
    ));
    tokio::spawn(engine.clone().run_chatgpt_maintenance(
        catalog_refresh_shutdown.clone(),
        cookie_agent_engine::CHATGPT_MAINTENANCE_INTERVAL,
    ));
    Ok(Runtime {
        engine,
        server,
        port,
        token,
        config_path,
        catalog_refresh_shutdown,
        catalog_refresh_task: Some(catalog_refresh_task),
        official_skills_task: None,
        official_agents_task: None,
    })
}

async fn run_catalog_refresh_loop<T: CatalogTransport + 'static>(
    catalog_manager: CatalogManager<T>,
    engine: Engine,
    shutdown: CancellationToken,
    cadence: Duration,
) {
    // The first check runs immediately, since startup served the cache.
    let mut first = true;
    loop {
        if !std::mem::take(&mut first) {
            tokio::select! {
                () = shutdown.cancelled() => return,
                () = tokio::time::sleep(cadence) => {}
            }
        }
        let refreshed = tokio::select! {
            () = shutdown.cancelled() => return,
            refreshed = catalog_manager.refresh() => refreshed,
        };
        let Ok(catalog) = refreshed else {
            continue;
        };
        if !catalog_publication_changed(&engine, &catalog) {
            continue;
        }
        // Recompiling every model takes seconds in a debug build; on an
        // async worker it would hold up every request that worker serves.
        let refreshing = engine.clone();
        let _ = tokio::task::spawn_blocking(move || refreshing.refresh_catalog(Arc::new(catalog)))
            .await;
    }
}

/// Republishing recompiles every model, so it happens only when the models
/// could differ: a new catalog revision or a change in availability. A server
/// confirming the cached body (source `cache` becoming `network`) is not one.
fn catalog_publication_changed(
    engine: &Engine,
    catalog: &cookie_agent_models::catalog::CatalogSnapshot,
) -> bool {
    let current = engine.current_runtime();
    let current = current.models.catalog();
    current.revision != catalog.revision || current.state.availability != catalog.state.availability
}
