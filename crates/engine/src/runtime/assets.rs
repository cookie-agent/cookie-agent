//! The engine's asset requests (`docs/site/specs/assets.md`): the client
//! methods `assets.list`, `assets.search`, `asset.get`, `asset.read`,
//! `asset.thumbnail`, `asset.rename`, and `asset.delete`, and the stores a
//! session's asset tools use.

use std::sync::Arc;

use base64::Engine as _;
use cookie_agent_protocol::{
    Asset, AssetDeleteParams, AssetDeleteResult, AssetGetParams, AssetGetResult,
    AssetProjectFilter, AssetReadParams, AssetReadResult, AssetRenameParams, AssetRenameResult,
    AssetScope, AssetSession, AssetStore, AssetThumbnail, AssetThumbnailParams,
    AssetThumbnailResult, AssetsListParams, AssetsListResult, AssetsSearchParams,
    AssetsSearchResult, SessionId,
};

use super::{Engine, EngineError};
use crate::assets::{
    AssetError, AssetStores, ThumbnailSource,
    thumbnail::{self, Drawer, Made},
};

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

    /// `assets.search`: the session trees' own stores, found by listing
    /// `session-assets/` and filtered by their root's project, and the
    /// projects' stores, newest first. No session's log is read, and each
    /// store's index comes from memory while its file is unchanged.
    pub async fn search_assets(
        &self,
        params: AssetsSearchParams,
    ) -> Result<AssetsSearchResult, EngineError> {
        self.assets_blocking(move |engine, stores| {
            let mut result = AssetsSearchResult::default();
            if params.scope != Some(AssetScope::Project) {
                for root in stores.session_roots()? {
                    // A tree deleted by an older build, or not a root.
                    let Some(meta) = engine.inner.store.root_meta(root) else {
                        continue;
                    };
                    let project = meta
                        .project
                        .filter(|project| engine.project_exists(project));
                    let wanted = match &params.project {
                        None => true,
                        Some(AssetProjectFilter::NoProject) => project.is_none(),
                        Some(AssetProjectFilter::Project { project: wanted }) => {
                            project.as_ref() == Some(wanted)
                        }
                    };
                    if !wanted {
                        continue;
                    }
                    let assets = stores.list(&AssetStore::Session { session: root })?;
                    if assets.is_empty() {
                        continue;
                    }
                    result.sessions.push(AssetSession {
                        session: root,
                        title: meta.title,
                        project,
                    });
                    result.assets.extend(assets);
                }
            }
            if params.scope != Some(AssetScope::Session) {
                let kept = match &params.project {
                    None => {
                        let mut projects = engine.project_ids().into_iter().collect::<Vec<_>>();
                        projects.sort();
                        projects
                            .into_iter()
                            .map(|project| AssetStore::Project { project })
                            .chain([AssetStore::Shared])
                            .collect()
                    }
                    Some(AssetProjectFilter::NoProject) => vec![AssetStore::Shared],
                    Some(AssetProjectFilter::Project { project }) => {
                        let store = AssetStore::Project {
                            project: project.clone(),
                        };
                        engine.check_client_store(&store)?;
                        vec![store]
                    }
                };
                for store in kept {
                    result.assets.extend(stores.list(&store)?);
                }
            }
            result
                .assets
                .sort_by_key(|asset| std::cmp::Reverse(asset.updated.at));
            Ok(result)
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
            let part = stores.read(
                &params.store,
                params.asset,
                params.path.as_deref(),
                params.offset,
                length,
            )?;
            Ok(AssetReadResult {
                data: base64::engine::general_purpose::STANDARD.encode(part.bytes),
                total_bytes: part.total_bytes,
                media_type: part.media_type,
                sha256: part.asset.sha256,
            })
        })
        .await
    }

    /// `asset.manifest`.
    pub async fn asset_manifest(
        &self,
        params: cookie_agent_protocol::AssetManifestParams,
    ) -> Result<cookie_agent_protocol::FolderManifestResult, EngineError> {
        self.assets_blocking(move |engine, stores| {
            engine.check_client_store(&params.store)?;
            let (asset, manifest) = stores.manifest(&params.store, params.asset)?;
            Ok(cookie_agent_protocol::FolderManifestResult::of(
                asset.sha256,
                manifest,
            ))
        })
        .await
    }

    /// `asset.thumbnail`: the thumbnail kept of the asset's content, or
    /// one drawn now and kept.
    pub async fn asset_thumbnail(
        &self,
        params: AssetThumbnailParams,
    ) -> Result<AssetThumbnailResult, EngineError> {
        let store = params.store.clone();
        let source = self
            .assets_blocking(move |engine, stores| {
                engine.check_client_store(&params.store)?;
                Ok(stores.thumbnail_source(&params.store, params.asset)?)
            })
            .await?;
        let mut sha256 = source.asset.sha256.clone();
        let made = match source.kept {
            Some(kept) => Made::Picture(kept),
            None => self.draw_thumbnail(store, source, &mut sha256).await,
        };
        let (picture, retry_later) = match made {
            Made::Picture(picture) => (Some(picture), false),
            Made::Cannot => (None, false),
            Made::Unavailable => (None, true),
        };
        Ok(AssetThumbnailResult {
            sha256,
            thumbnail: picture.map(|picture| AssetThumbnail {
                media_type: picture.format.media_type().to_owned(),
                data: base64::engine::general_purpose::STANDARD.encode(&picture.bytes),
                width: picture.width,
                height: picture.height,
            }),
            retry_later,
        })
    }

    /// Draws the thumbnail of `source`, an asset of `store` with none kept,
    /// and keeps it. `sha256` becomes the content drawn, which another
    /// write may have changed while this one waited its turn. A kind
    /// without thumbnails, or content that did not draw before, is
    /// [`Made::Cannot`].
    async fn draw_thumbnail(
        &self,
        store: AssetStore,
        source: ThumbnailSource,
        sha256: &mut String,
    ) -> Made {
        let stores = Arc::clone(&self.inner.assets);
        if Drawer::of(&source.asset.media_type).is_none() || stores.undrawable(sha256) {
            return Made::Cannot;
        }
        let Some(_turn) = stores.draw_turn().await else {
            return Made::Unavailable;
        };
        // Another request may have drawn it while this one waited.
        let id = source.asset.id;
        let Ok(source) = self
            .assets_blocking({
                let store = store.clone();
                move |_, stores| Ok(stores.thumbnail_source(&store, id)?)
            })
            .await
        else {
            return Made::Unavailable;
        };
        sha256.clone_from(&source.asset.sha256);
        if let Some(kept) = source.kept {
            return Made::Picture(kept);
        }
        let Some(drawer) = Drawer::of(&source.asset.media_type) else {
            return Made::Cannot;
        };
        if stores.undrawable(sha256) {
            return Made::Cannot;
        }
        let made = thumbnail::make(source.blob, drawer).await;
        match &made {
            Made::Picture(picture) => {
                let kept = picture.clone();
                let drawn = sha256.clone();
                if let Err(error) = self
                    .assets_blocking(move |_, stores| {
                        Ok(stores.keep_thumbnail(&store, &drawn, &kept)?)
                    })
                    .await
                {
                    eprintln!("cookie-agent: a thumbnail was not kept: {error}");
                }
            }
            Made::Cannot => stores.mark_undrawable(sha256),
            Made::Unavailable => {}
        }
        made
    }

    /// `artifact.thumbnail` (`docs/site/specs/attachment-handling.md`
    /// section 5.1): the thumbnail kept of artifact `sha256` of `session`'s
    /// tree, or one drawn now, by the asset drawer and in its turn, and kept
    /// beside the tree's blobs, which delete it with the blob or the tree.
    pub async fn artifact_thumbnail(
        &self,
        session: SessionId,
        sha256: &str,
    ) -> Result<AssetThumbnailResult, EngineError> {
        use super::artifacts::uploads::MediaError;
        if !super::artifacts::is_digest_name_common(sha256) {
            return Err(
                MediaError::InvalidRequest("a digest is 64 lowercase hex digits".into()).into(),
            );
        }
        self.inner.store.get(session)?;
        let not_found = |reason: &str| MediaError::ArtifactNotFound {
            sha256: sha256.to_owned(),
            reason: reason.to_owned(),
        };
        // The whole blob is verified as it opens, and its head says what
        // it is. A folder shows its main file
        // (`docs/site/specs/folder-artifacts.md` section 4).
        let found = {
            let artifacts = Arc::clone(&self.inner.artifacts);
            let digest = sha256.to_owned();
            super::blocking_io::run(move || {
                let Some(store) = artifacts.read_store(session)? else {
                    return Ok(None);
                };
                let digest = match super::artifacts::read_folder_manifest(&store, &digest) {
                    Ok(Some(manifest)) => manifest.main_entry().sha256.clone(),
                    Ok(None) => digest,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        return Ok(None);
                    }
                    Err(error) => return Err(error),
                };
                let range = match store.read_range(&digest, 0, 1) {
                    Ok(range) => range,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        return Ok(None);
                    }
                    Err(error) => return Err(error),
                };
                let media_type = cookie_agent_runtime_api::media::recognize_media_type(
                    &range.head,
                    range.total_bytes,
                );
                let kept = store.kept_thumbnail(&digest);
                Ok(Some((store, digest, media_type, kept)))
            })
            .await
            .map_err(|error| EngineError::ToolFailed(error.to_string()))?
        };
        let (store, drawn, media_type, kept) = match found {
            Ok(Some(found)) => found,
            Ok(None) => return Err(not_found("missing").into()),
            Err(error) if error.kind() == std::io::ErrorKind::InvalidData => {
                return Err(not_found("its stored copy is damaged").into());
            }
            Err(error) => return Err(error.into()),
        };
        let made = match kept {
            Some(kept) => Made::Picture(kept),
            None => {
                self.draw_artifact_thumbnail(store, &drawn, media_type)
                    .await
            }
        };
        let (picture, retry_later) = match made {
            Made::Picture(picture) => (Some(picture), false),
            Made::Cannot => (None, false),
            Made::Unavailable => (None, true),
        };
        Ok(AssetThumbnailResult {
            sha256: sha256.to_owned(),
            thumbnail: picture.map(|picture| AssetThumbnail {
                media_type: picture.format.media_type().to_owned(),
                data: base64::engine::general_purpose::STANDARD.encode(&picture.bytes),
                width: picture.width,
                height: picture.height,
            }),
            retry_later,
        })
    }

    /// Draws the thumbnail of artifact `sha256` of `store`, content of
    /// `media_type`, and keeps it.
    async fn draw_artifact_thumbnail(
        &self,
        store: Arc<super::artifacts::ArtifactStore>,
        sha256: &str,
        media_type: Option<&'static str>,
    ) -> Made {
        let stores = Arc::clone(&self.inner.assets);
        let Some(drawer) = media_type.and_then(Drawer::of) else {
            return Made::Cannot;
        };
        if stores.undrawable(sha256) {
            return Made::Cannot;
        }
        let Some(_turn) = stores.draw_turn().await else {
            return Made::Unavailable;
        };
        // Another request may have drawn it while this one waited.
        if let Some(kept) = store.kept_thumbnail(sha256) {
            return Made::Picture(kept);
        }
        let made = thumbnail::make(store.blob_file(sha256), drawer).await;
        match &made {
            Made::Picture(picture) => {
                let kept = picture.clone();
                let digest = sha256.to_owned();
                let written =
                    super::blocking_io::run(move || store.keep_thumbnail(&digest, &kept)).await;
                if !matches!(written, Ok(Ok(()))) {
                    eprintln!("cookie-agent: an artifact thumbnail was not kept");
                }
            }
            Made::Cannot => stores.mark_undrawable(sha256),
            Made::Unavailable => {}
        }
        made
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
            engine.forget_shares_of_asset(params.asset);
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
