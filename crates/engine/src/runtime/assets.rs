//! The engine's asset requests (`docs/site/specs/assets.md`): the client
//! methods `assets.list`, `asset.get`, `asset.read`, `asset.rename`, and
//! `asset.delete`, and the stores a session's asset tools use.

use std::sync::Arc;

use base64::Engine as _;
use cookie_agent_protocol::{
    Asset, AssetDeleteParams, AssetDeleteResult, AssetGetParams, AssetGetResult, AssetReadParams,
    AssetReadResult, AssetRenameParams, AssetRenameResult, AssetStore, AssetsListParams,
    AssetsListResult, SessionId,
};

use super::{Engine, EngineError};
use crate::assets::{AssetError, AssetStores};

/// The two stores a session's asset tools reach: its tree's, and its
/// project's (or the shared store, for a session in no project).
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SessionAssetStores {
    pub(crate) session: AssetStore,
    pub(crate) project: AssetStore,
}

impl SessionAssetStores {
    /// The store an asset of `scope` goes to.
    pub(crate) fn of(&self, scope: cookie_agent_protocol::AssetScope) -> &AssetStore {
        match scope {
            cookie_agent_protocol::AssetScope::Session => &self.session,
            cookie_agent_protocol::AssetScope::Project => &self.project,
        }
    }
}

impl Engine {
    /// The daemon's `[server] page_url`, which handshakes report.
    #[must_use]
    pub fn page_url(&self) -> Option<String> {
        self.inner.config.load().runtime.server.page_url.clone()
    }

    /// Signals every asset change this daemon makes.
    #[must_use]
    pub fn subscribe_asset_changes(&self) -> tokio::sync::watch::Receiver<u64> {
        self.inner.assets.subscribe()
    }

    /// Runs `call` on the asset stores off the async workers.
    pub(crate) async fn assets_blocking<T: Send + 'static>(
        &self,
        call: impl FnOnce(&Engine, &AssetStores) -> Result<T, EngineError> + Send + 'static,
    ) -> Result<T, EngineError> {
        let engine = self.clone();
        tokio::task::spawn_blocking(move || {
            let stores = Arc::clone(&engine.inner.assets);
            call(&engine, &stores)
        })
        .await
        .map_err(|error| AssetError::Store(format!("the request did not finish: {error}")))?
    }

    /// The stores `session`'s asset tools reach. Blocking: it reads the
    /// session's project.
    pub(crate) fn session_asset_stores(&self, session: SessionId) -> SessionAssetStores {
        let root = self.root_session_of(session);
        let project = match self.session_project(session) {
            Some(project) => AssetStore::Project {
                project: project.id,
            },
            None => AssetStore::Shared,
        };
        SessionAssetStores {
            session: AssetStore::Session { session: root },
            project,
        }
    }

    /// Refuses a store a client names that does not exist: a project the
    /// daemon does not have, or a session store not named by a root whose
    /// folder exists. Blocking.
    fn check_client_store(&self, store: &AssetStore) -> Result<(), AssetError> {
        match store {
            AssetStore::Shared => Ok(()),
            AssetStore::Project { project } => {
                if self.inner.projects.exists(project) {
                    Ok(())
                } else {
                    Err(AssetError::InvalidStore(format!(
                        "there is no project {project}"
                    )))
                }
            }
            AssetStore::Session { session } => {
                let root_dir = self.inner.store.root_dir(*session);
                if root_dir.is_dir() {
                    Ok(())
                } else {
                    Err(AssetError::InvalidStore(format!(
                        "{session} is not a root session of this daemon"
                    )))
                }
            }
        }
    }

    /// `assets.list`.
    pub async fn list_assets(
        &self,
        params: AssetsListParams,
    ) -> Result<AssetsListResult, EngineError> {
        self.assets_blocking(move |engine, stores| {
            engine.check_client_store(&params.store)?;
            Ok(AssetsListResult {
                assets: stores.list(&params.store)?,
            })
        })
        .await
    }

    /// `asset.get`.
    pub async fn get_asset(&self, params: AssetGetParams) -> Result<AssetGetResult, EngineError> {
        self.assets_blocking(move |engine, stores| {
            engine.check_client_store(&params.store)?;
            Ok(AssetGetResult {
                asset: stores.get(&params.store, params.asset)?,
            })
        })
        .await
    }

    /// `asset.read`.
    pub async fn read_asset(
        &self,
        params: AssetReadParams,
    ) -> Result<AssetReadResult, EngineError> {
        self.assets_blocking(move |engine, stores| {
            engine.check_client_store(&params.store)?;
            let length = params.length.min(cookie_agent_protocol::ASSET_CHUNK_BYTES);
            let (bytes, asset) = stores.read(&params.store, params.asset, params.offset, length)?;
            Ok(AssetReadResult {
                data: base64::engine::general_purpose::STANDARD.encode(bytes),
                total_bytes: asset.byte_length,
                media_type: asset.media_type,
                sha256: asset.sha256,
            })
        })
        .await
    }

    /// `asset.rename`.
    pub async fn rename_asset(
        &self,
        params: AssetRenameParams,
    ) -> Result<AssetRenameResult, EngineError> {
        self.assets_blocking(move |engine, stores| {
            engine.check_client_store(&params.store)?;
            Ok(AssetRenameResult {
                asset: stores.rename(&params.store, params.asset, params.title)?,
            })
        })
        .await
    }

    /// `asset.delete`.
    pub async fn delete_asset(
        &self,
        params: AssetDeleteParams,
    ) -> Result<AssetDeleteResult, EngineError> {
        self.assets_blocking(move |engine, stores| {
            engine.check_client_store(&params.store)?;
            stores.delete(&params.store, params.asset)?;
            Ok(AssetDeleteResult {})
        })
        .await
    }

    /// Every asset `session`'s tools can reach, the tree's first and then
    /// the project's, each newest first. Blocking.
    pub(crate) fn session_assets(
        &self,
        stores: &SessionAssetStores,
    ) -> Result<Vec<Asset>, AssetError> {
        let mut assets = self.inner.assets.list(&stores.session)?;
        assets.extend(self.inner.assets.list(&stores.project)?);
        Ok(assets)
    }
}
