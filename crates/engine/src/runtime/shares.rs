//! The engine's share link requests (`docs/site/specs/share-links.md`): the
//! client methods `shares.list`, `share.create`, `share.update`,
//! `share.revoke`, and `shares.revoke_all`, what the daemon's `/s/` route
//! asks of a link, and the links' removal with their assets.

use cookie_agent_protocol::{
    Asset, AssetStore, RunId, SHARE_DEFAULT_LABEL, SessionId, SessionOrigin, SessionShare,
    SessionShareCreateParams, SessionShareResult, SessionShareUpdateParams, Share,
    ShareCreateParams, ShareCreateResult, ShareCreated, ShareId, ShareRevokeParams,
    ShareRevokeResult, ShareUpdateParams, ShareUpdateResult, SharesListParams, SharesListResult,
    SharesRevokeAllParams, SharesRevokeAllResult, is_share_label,
};

use super::{Engine, EngineError};
use crate::{
    assets::AssetError,
    shares::{ShareError, ShareRecord, ShareTarget, new_share_id},
};

/// A live link the `/s/` route serves, with its asset as it is now.
#[derive(Clone, Debug)]
pub struct OpenShare {
    pub share: Share,
    pub asset: Asset,
}

/// A live conversation share as its guest connections read it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GuestLink {
    pub session: SessionId,
    pub label: String,
    pub can_send: bool,
    pub expires_at: Option<jiff::Timestamp>,
}

/// What a live link opens, for the `/s/` route.
#[derive(Clone, Debug)]
pub enum OpenedLink {
    Asset(Box<OpenShare>),
    Session(GuestLink),
}

/// One guest connected through a link. It is counted until dropped.
pub struct GuestPresence {
    engine: Engine,
    id: ShareId,
}

impl Drop for GuestPresence {
    fn drop(&mut self) {
        self.engine.inner.shares.guest_left(&self.id);
    }
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

    fn share(record: &ShareRecord, asset: &Asset, base: Option<&str>) -> Option<Share> {
        let ShareTarget::Asset {
            store,
            asset: id,
            title,
        } = &record.target
        else {
            return None;
        };
        Some(Share {
            id: record.id.clone(),
            path: format!("{}{}", cookie_agent_protocol::SHARE_PATH_PREFIX, record.id),
            url: base.map(|base| cookie_agent_protocol::share_url(base, &record.id)),
            store: store.clone(),
            asset: *id,
            title: title.clone(),
            media_type: asset.media_type.clone(),
            byte_length: asset.byte_length,
            created: record.created.clone(),
            expires_at: record.expires_at,
            views: record.views,
            last_viewed: record.last_viewed,
        })
    }

    /// `record` as a conversation share, `None` when it is not one or its
    /// session is gone. Blocking.
    fn session_share(&self, record: &ShareRecord, base: Option<&str>) -> Option<SessionShare> {
        let ShareTarget::Session {
            session,
            label,
            can_send,
        } = &record.target
        else {
            return None;
        };
        let meta = self.session_meta(*session).ok()?;
        Some(SessionShare {
            id: record.id.clone(),
            path: format!("{}{}", cookie_agent_protocol::SHARE_PATH_PREFIX, record.id),
            url: base.map(|base| cookie_agent_protocol::share_url(base, &record.id)),
            session_id: *session,
            title: meta.title.map(|title| title.as_str().to_owned()),
            label: label.clone(),
            can_send: *can_send,
            created: record.created.clone(),
            expires_at: record.expires_at,
            views: record.views,
            last_viewed: record.last_viewed,
            connected: self.inner.shares.guests(&record.id),
        })
    }

    /// The asset `record` shows, `None` when it is gone. Blocking.
    fn shared_asset(&self, record: &ShareRecord) -> Result<Option<Asset>, EngineError> {
        let ShareTarget::Asset { store, asset, .. } = &record.target else {
            return Ok(None);
        };
        match self.inner.assets.get(store, *asset) {
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
            let mut sessions = Vec::new();
            let mut gone = Vec::new();
            for record in records {
                if matches!(record.target, ShareTarget::Session { .. }) {
                    // A session's links go when it is deleted, so one that
                    // cannot be read now is only left out.
                    if params.asset.is_none()
                        && let Some(share) = engine.session_share(&record, base.as_deref())
                        && params
                            .session
                            .is_none_or(|session| session == share.session_id)
                    {
                        sessions.push(share);
                    }
                    continue;
                }
                if params.session.is_some()
                    || params
                        .asset
                        .is_some_and(|asset| record.asset() != Some(asset))
                {
                    continue;
                }
                match engine.shared_asset(&record)? {
                    Some(asset) => shares.extend(Self::share(&record, &asset, base.as_deref())),
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
                sessions,
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
            let share = Self::share(&record, &asset, base.as_deref())
                .ok_or(ShareError::OtherKind(record.id))?;
            return Ok(ShareCreateResult {
                share,
                created: false,
            });
        }
        if params.expiry.at().is_some_and(|at| at <= now) {
            return Err(ShareError::PastExpiry.into());
        }
        let record = ShareRecord {
            id: new_share_id()?,
            target: ShareTarget::Asset {
                store: asset.store.clone(),
                asset: asset.id,
                title: asset.title.clone(),
            },
            created,
            expires_at: params.expiry.at(),
            views: 0,
            last_viewed: None,
        };
        self.inner.shares.insert(record.clone())?;
        let share = Self::share(&record, &asset, base.as_deref())
            .ok_or_else(|| ShareError::OtherKind(record.id.clone()))?;
        Ok(ShareCreateResult {
            share,
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
            let current = engine
                .inner
                .shares
                .get(&params.id, now)?
                .ok_or_else(|| ShareError::NotFound(params.id.clone()))?;
            if !matches!(current.target, ShareTarget::Asset { .. }) {
                return Err(ShareError::OtherKind(params.id).into());
            }
            let record = engine.inner.shares.update(&params.id, now, |record| {
                if let (Some(new), ShareTarget::Asset { title, .. }) =
                    (params.title, &mut record.target)
                {
                    *title = new;
                }
                if let Some(expiry) = &params.expiry {
                    record.expires_at = expiry.at();
                }
            })?;
            let Some(asset) = engine.shared_asset(&record)? else {
                engine.inner.shares.remove(|gone| gone.id == record.id)?;
                return Err(ShareError::NotFound(record.id).into());
            };
            let share = Self::share(&record, &asset, engine.share_base().as_deref())
                .ok_or_else(|| ShareError::OtherKind(record.id.clone()))?;
            Ok(ShareUpdateResult { share })
        })
        .await
    }

    /// `session_share.create`, the user's: no rule is checked.
    pub async fn create_session_share(
        &self,
        params: SessionShareCreateParams,
    ) -> Result<SessionShareResult, EngineError> {
        self.shares_blocking(move |engine| {
            if !engine.shares_enabled() {
                return Err(ShareError::Disabled.into());
            }
            let now = jiff::Timestamp::now();
            if params.expiry.at().is_some_and(|at| at <= now) {
                return Err(ShareError::PastExpiry.into());
            }
            let label = params
                .label
                .unwrap_or_else(|| SHARE_DEFAULT_LABEL.to_owned());
            if !is_share_label(&label) {
                return Err(ShareError::InvalidLabel.into());
            }
            let meta = engine.session_meta(params.session_id)?;
            if !matches!(meta.origin, SessionOrigin::Root) {
                return Err(ShareError::NotRoot(params.session_id).into());
            }
            let record = ShareRecord {
                id: new_share_id()?,
                target: ShareTarget::Session {
                    session: params.session_id,
                    label,
                    can_send: params.can_send,
                },
                created: ShareCreated {
                    at: now,
                    session: None,
                    agent: None,
                },
                expires_at: params.expiry.at(),
                views: 0,
                last_viewed: None,
            };
            engine.inner.shares.insert(record.clone())?;
            let share = engine
                .session_share(&record, engine.share_base().as_deref())
                .ok_or_else(|| ShareError::NotFound(record.id.clone()))?;
            Ok(SessionShareResult { share })
        })
        .await
    }

    /// `session_share.update`.
    pub async fn update_session_share(
        &self,
        params: SessionShareUpdateParams,
    ) -> Result<SessionShareResult, EngineError> {
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
            if params
                .label
                .as_deref()
                .is_some_and(|label| !is_share_label(label))
            {
                return Err(ShareError::InvalidLabel.into());
            }
            let current = engine
                .inner
                .shares
                .get(&params.id, now)?
                .ok_or_else(|| ShareError::NotFound(params.id.clone()))?;
            if !matches!(current.target, ShareTarget::Session { .. }) {
                return Err(ShareError::OtherKind(params.id).into());
            }
            let record = engine.inner.shares.update(&params.id, now, |record| {
                if let ShareTarget::Session {
                    label, can_send, ..
                } = &mut record.target
                {
                    if let Some(new) = params.label {
                        *label = new;
                    }
                    if let Some(new) = params.can_send {
                        *can_send = new;
                    }
                }
                if let Some(expiry) = &params.expiry {
                    record.expires_at = expiry.at();
                }
            })?;
            let share = engine
                .session_share(&record, engine.share_base().as_deref())
                .ok_or_else(|| ShareError::NotFound(record.id.clone()))?;
            Ok(SessionShareResult { share })
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

    /// What the live link `id` opens, for the `/s/` route; `None` when links
    /// are off, or the link or what it opens is gone.
    pub async fn open_share(&self, id: ShareId) -> Result<Option<OpenedLink>, EngineError> {
        if !self.shares_enabled() {
            return Ok(None);
        }
        self.shares_blocking(move |engine| {
            let Some(record) = engine.inner.shares.get(&id, jiff::Timestamp::now())? else {
                return Ok(None);
            };
            if let Some(link) = guest_link(&record) {
                return Ok(engine
                    .session_meta(link.session)
                    .is_ok()
                    .then_some(OpenedLink::Session(link)));
            }
            let Some(asset) = engine.shared_asset(&record)? else {
                return Ok(None);
            };
            let Some(share) = Self::share(&record, &asset, engine.share_base().as_deref()) else {
                return Ok(None);
            };
            Ok(Some(OpenedLink::Asset(Box::new(OpenShare {
                share,
                asset,
            }))))
        })
        .await
    }

    /// The live conversation share `id` as it is now; `None` when links are
    /// off or it is gone. A guest connection asks before each thing it does,
    /// so a change to the link applies at once.
    pub async fn guest_link(&self, id: ShareId) -> Result<Option<GuestLink>, EngineError> {
        if !self.shares_enabled() {
            return Ok(None);
        }
        self.shares_blocking(move |engine| {
            Ok(engine
                .inner
                .shares
                .get(&id, jiff::Timestamp::now())?
                .as_ref()
                .and_then(guest_link))
        })
        .await
    }

    /// Counts a guest connected through link `id` until the result drops.
    #[must_use]
    pub fn guest_presence(&self, id: ShareId) -> GuestPresence {
        self.inner.shares.guest_joined(&id, jiff::Timestamp::now());
        GuestPresence {
            engine: self.clone(),
            id,
        }
    }

    /// Whether `session` is `root` or a delegated session of its tree.
    pub fn session_in_tree(&self, root: SessionId, session: SessionId) -> bool {
        if root == session {
            return true;
        }
        self.session_meta(session).is_ok_and(|meta| {
            matches!(
                meta.origin,
                SessionOrigin::Delegated { root_session_id, .. } if root_session_id == root
            )
        })
    }

    /// The session the active run `run` belongs to.
    #[must_use]
    pub fn session_of_active_run(&self, run: RunId) -> Option<SessionId> {
        self.inner
            .sessions
            .active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&run)
            .map(|active| active.session)
    }

    /// The whole content of a shared asset; `None` when it changed or went
    /// since it was opened.
    pub async fn read_shared_asset(&self, asset: &Asset) -> Result<Option<Vec<u8>>, EngineError> {
        self.read_shared_asset_part(asset, 0, asset.byte_length)
            .await
    }

    /// [`Self::read_shared_asset`], up to `length` bytes from `offset`.
    pub async fn read_shared_asset_part(
        &self,
        asset: &Asset,
        offset: u64,
        length: u64,
    ) -> Result<Option<Vec<u8>>, EngineError> {
        let (store, id, sha256) = (asset.store.clone(), asset.id, asset.sha256.clone());
        self.assets_blocking(
            move |_, stores| match stores.read(&store, id, offset, length) {
                Ok((bytes, now)) if now.sha256 == sha256 => Ok(Some(bytes)),
                Ok(_) | Err(AssetError::NotFound(_)) => Ok(None),
                Err(error) => Err(error.into()),
            },
        )
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
        let session = match store {
            AssetStore::Session { session } => Some(*session),
            _ => None,
        };
        self.inner
            .shares
            .remove(|record| match &record.target {
                ShareTarget::Asset { store: own, .. } => own == store,
                ShareTarget::Session { session: own, .. } => Some(*own) == session,
            })
            .unwrap_or_else(|error| {
                eprintln!("cookie-agent: share links: {error}");
                0
            })
    }

    /// Removes the links of the asset `asset`. Blocking.
    pub(crate) fn forget_shares_of_asset(&self, asset: cookie_agent_protocol::AssetId) {
        if let Err(error) = self
            .inner
            .shares
            .remove(|record| record.asset() == Some(asset))
        {
            eprintln!("cookie-agent: share links: {error}");
        }
    }
}

/// `record` as its guest connections read it, when it is a conversation
/// share.
fn guest_link(record: &ShareRecord) -> Option<GuestLink> {
    match &record.target {
        ShareTarget::Session {
            session,
            label,
            can_send,
        } => Some(GuestLink {
            session: *session,
            label: label.clone(),
            can_send: *can_send,
            expires_at: record.expires_at,
        }),
        ShareTarget::Asset { .. } => None,
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
