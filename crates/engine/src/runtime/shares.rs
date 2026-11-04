//! The engine's share link requests (`docs/site/specs/share-links.md`): the
//! client methods `shares.list`, `share.create`, `share.update`,
//! `share.revoke`, and `shares.revoke_all`, what the daemon's `/s/` route
//! asks of a link, and the links' removal with their assets.

use cookie_agent_protocol::{
    Asset, AssetStore, Share, ShareCreateParams, ShareCreateResult, ShareCreated, ShareId,
    ShareRevokeParams, ShareRevokeResult, ShareUpdateParams, ShareUpdateResult, SharesListParams,
    SharesListResult, SharesRevokeAllParams, SharesRevokeAllResult,
};

use super::{Engine, EngineError};
use crate::{
    assets::AssetError,
    shares::{ShareError, ShareRecord, new_share_id},
};

/// A live link the `/s/` route serves, with its asset as it is now.
#[derive(Clone, Debug)]
pub struct OpenShare {
    pub share: Share,
    pub asset: Asset,
}

impl Engine {
    /// Whether the daemon serves share links (`[share] enabled`).
    #[must_use]
    pub fn shares_enabled(&self) -> bool {
        self.inner.config.load().runtime.share.enabled
    }

    /// The base links are built on: `[share] url`, else `[server]
    /// page_url`.
    #[must_use]
    pub fn share_base(&self) -> Option<String> {
        let config = self.inner.config.load();
        config
            .runtime
            .share
            .url
            .clone()
            .or_else(|| config.runtime.server.page_url.clone())
    }

    /// Signals every change to the links but view counts.
    #[must_use]
    pub fn subscribe_share_changes(&self) -> tokio::sync::watch::Receiver<u64> {
        self.inner.shares.subscribe()
    }

    fn share(record: &ShareRecord, asset: &Asset, base: Option<&str>) -> Share {
        Share {
            id: record.id.clone(),
            path: format!("{}{}", cookie_agent_protocol::SHARE_PATH_PREFIX, record.id),
            url: base.map(|base| cookie_agent_protocol::share_url(base, &record.id)),
            store: record.store.clone(),
            asset: record.asset,
            title: record.title.clone(),
            media_type: asset.media_type.clone(),
            byte_length: asset.byte_length,
            created: record.created.clone(),
            expires_at: record.expires_at,
            views: record.views,
            last_viewed: record.last_viewed,
        }
    }

    /// The asset `record` shows, `None` when it is gone. Blocking.
    fn shared_asset(&self, record: &ShareRecord) -> Result<Option<Asset>, EngineError> {
        match self.inner.assets.get(&record.store, record.asset) {
            Ok(asset) => Ok(Some(asset)),
            Err(AssetError::NotFound(_)) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    /// Runs `call` off the async workers.
    async fn shares_blocking<T: Send + 'static>(
        &self,
        call: impl FnOnce(&Engine) -> Result<T, EngineError> + Send + 'static,
    ) -> Result<T, EngineError> {
        let engine = self.clone();
        tokio::task::spawn_blocking(move || call(&engine))
            .await
            .map_err(|error| ShareError::Store(format!("the request did not finish: {error}")))?
    }

    /// `shares.list`: the live links, newest first. A link whose asset is
    /// gone is removed here.
    pub async fn list_shares(
        &self,
        params: SharesListParams,
    ) -> Result<SharesListResult, EngineError> {
        self.shares_blocking(move |engine| {
            let base = engine.share_base();
            let records = engine.inner.shares.list(jiff::Timestamp::now())?;
            let mut shares = Vec::new();
            let mut gone = Vec::new();
            for record in records {
                if params.asset.is_some_and(|asset| asset != record.asset) {
                    continue;
                }
                match engine.shared_asset(&record)? {
                    Some(asset) => shares.push(Self::share(&record, &asset, base.as_deref())),
                    None => gone.push(record.id),
                }
            }
            if !gone.is_empty() {
                engine
                    .inner
                    .shares
                    .remove(|record| gone.contains(&record.id))?;
            }
            Ok(SharesListResult {
                shares,
                enabled: engine.shares_enabled(),
            })
        })
        .await
    }

    /// `share.create`, the user's: no rule is checked.
    pub async fn create_share(
        &self,
        params: ShareCreateParams,
    ) -> Result<ShareCreateResult, EngineError> {
        let created = ShareCreated {
            at: jiff::Timestamp::now(),
            session: None,
            agent: None,
        };
        self.shares_blocking(move |engine| engine.make_share(params, created))
            .await
    }

    /// Makes a link for `params`, or with `reuse` returns the asset's
    /// newest live one. Blocking.
    pub(crate) fn make_share(
        &self,
        params: ShareCreateParams,
        created: ShareCreated,
    ) -> Result<ShareCreateResult, EngineError> {
        if !self.shares_enabled() {
            return Err(ShareError::Disabled.into());
        }
        let asset = self.inner.assets.get(&params.store, params.asset)?;
        let base = self.share_base();
        let now = jiff::Timestamp::now();
        if params.reuse
            && let Some(record) = self.inner.shares.newest_of(asset.id, now)?
        {
            return Ok(ShareCreateResult {
                share: Self::share(&record, &asset, base.as_deref()),
                created: false,
            });
        }
        if params.expiry.at().is_some_and(|at| at <= now) {
            return Err(ShareError::PastExpiry.into());
        }
        let record = ShareRecord {
            id: new_share_id()?,
            store: asset.store.clone(),
            asset: asset.id,
            title: asset.title.clone(),
            created,
            expires_at: params.expiry.at(),
            views: 0,
            last_viewed: None,
        };
        self.inner.shares.insert(record.clone())?;
        Ok(ShareCreateResult {
            share: Self::share(&record, &asset, base.as_deref()),
            created: true,
        })
    }

    /// `share.update`.
    pub async fn update_share(
        &self,
        params: ShareUpdateParams,
    ) -> Result<ShareUpdateResult, EngineError> {
        self.shares_blocking(move |engine| {
            let now = jiff::Timestamp::now();
            if params
                .expiry
                .as_ref()
                .and_then(cookie_agent_protocol::ShareExpiry::at)
                .is_some_and(|at| at <= now)
            {
                return Err(ShareError::PastExpiry.into());
            }
            let record = engine.inner.shares.update(&params.id, now, |record| {
                if let Some(title) = params.title {
                    record.title = title;
                }
                if let Some(expiry) = &params.expiry {
                    record.expires_at = expiry.at();
                }
            })?;
            let Some(asset) = engine.shared_asset(&record)? else {
                engine.inner.shares.remove(|gone| gone.id == record.id)?;
                return Err(ShareError::NotFound(record.id).into());
            };
            Ok(ShareUpdateResult {
                share: Self::share(&record, &asset, engine.share_base().as_deref()),
            })
        })
        .await
    }

    /// `share.revoke`.
    pub async fn revoke_share(
        &self,
        params: ShareRevokeParams,
    ) -> Result<ShareRevokeResult, EngineError> {
        self.shares_blocking(move |engine| {
            let id = params.id;
            if engine.inner.shares.remove(|record| record.id == id)? == 0 {
                return Err(ShareError::NotFound(id).into());
            }
            Ok(ShareRevokeResult {})
        })
        .await
    }

    /// `shares.revoke_all`.
    pub async fn revoke_all_shares(
        &self,
        _params: SharesRevokeAllParams,
    ) -> Result<SharesRevokeAllResult, EngineError> {
        self.shares_blocking(|engine| {
            let now = jiff::Timestamp::now();
            Ok(SharesRevokeAllResult {
                revoked: engine.inner.shares.remove(|record| record.live(now))?,
            })
        })
        .await
    }

    /// The live link `id` and its asset, for the `/s/` route; `None` when
    /// links are off, or the link or its asset is gone.
    pub async fn open_share(&self, id: ShareId) -> Result<Option<OpenShare>, EngineError> {
        if !self.shares_enabled() {
            return Ok(None);
        }
        self.shares_blocking(move |engine| {
            let Some(record) = engine.inner.shares.get(&id, jiff::Timestamp::now())? else {
                return Ok(None);
            };
            let Some(asset) = engine.shared_asset(&record)? else {
                return Ok(None);
            };
            Ok(Some(OpenShare {
                share: Self::share(&record, &asset, engine.share_base().as_deref()),
                asset,
            }))
        })
        .await
    }

    /// The whole content of a shared asset; `None` when it changed or went
    /// since it was opened.
    pub async fn read_shared_asset(&self, asset: &Asset) -> Result<Option<Vec<u8>>, EngineError> {
        let (store, id, total, sha256) = (
            asset.store.clone(),
            asset.id,
            asset.byte_length,
            asset.sha256.clone(),
        );
        self.assets_blocking(move |_, stores| match stores.read(&store, id, 0, total) {
            Ok((bytes, now)) if now.sha256 == sha256 => Ok(Some(bytes)),
            Ok(_) | Err(AssetError::NotFound(_)) => Ok(None),
            Err(error) => Err(error.into()),
        })
        .await
    }

    /// Counts a view of the landing page of link `id`.
    pub fn count_share_view(&self, id: &ShareId) {
        self.inner.shares.viewed(id, jiff::Timestamp::now());
    }

    /// Writes counted views and removes expired links. Blocking.
    pub(crate) fn flush_shares(&self) {
        if let Err(error) = self.inner.shares.flush(jiff::Timestamp::now()) {
            eprintln!("cookie-agent: share links: {error}");
        }
    }

    /// Removes the links of the assets of `store`, as its owner's deletion
    /// does, and says how many. Blocking.
    pub(crate) fn forget_shares_of_store(&self, store: &AssetStore) -> u64 {
        self.inner
            .shares
            .remove(|record| &record.store == store)
            .unwrap_or_else(|error| {
                eprintln!("cookie-agent: share links: {error}");
                0
            })
    }

    /// Removes the links of the asset `asset`. Blocking.
    pub(crate) fn forget_shares_of_asset(&self, asset: cookie_agent_protocol::AssetId) {
        if let Err(error) = self.inner.shares.remove(|record| record.asset == asset) {
            eprintln!("cookie-agent: share links: {error}");
        }
    }
}

#[cfg(any(test, feature = "test-support"))]
impl Engine {
    /// Saves `bytes` as a new asset of `store`, as `write_asset` would, for
    /// tests outside the engine. Blocking.
    pub fn write_asset_for_test(
        &self,
        store: &AssetStore,
        title: &str,
        filename: Option<&str>,
        bytes: Vec<u8>,
    ) -> Asset {
        let media_type = crate::assets::media_type(filename.unwrap_or_default(), &bytes);
        let write = crate::assets::AssetWrite {
            replace: None,
            title: Some(title.to_owned()),
            filename: filename.map(str::to_owned),
            media_type: media_type.to_owned(),
            bytes,
            stamp: cookie_agent_protocol::AssetStamp {
                at: jiff::Timestamp::now(),
                session: cookie_agent_protocol::SessionId::new_v7(),
                agent: None,
            },
        };
        self.inner
            .assets
            .write(store, write, &std::collections::HashSet::new())
            .expect("the test asset is written")
            .0
    }

    /// Changes the running configuration in place, for tests outside the
    /// engine.
    pub fn apply_config_for_test(
        &self,
        change: impl FnOnce(&mut cookie_agent_config::LoadedConfiguration),
    ) {
        let mut config = (**self.inner.config.load()).clone();
        change(&mut config);
        self.inner.config.store(std::sync::Arc::new(config));
    }
}
