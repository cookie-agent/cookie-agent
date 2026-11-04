//! Share links (`docs/site/specs/share-links.md` section 7.1): the share
//! dialog an asset's or an attachment's share button opens, the links the
//! Sharing view lists (`shares.list`, again on `shares.changed`), and
//! making, changing, and revoking them.

use cookie_agent_client_state::shares::{
    ShareLifetime, link_address, looks_private, share_address,
};
use cookie_agent_protocol::{
    AssetId, AssetStore, SessionId, SessionShare, SessionShareCreateParams,
    SessionShareUpdateParams, Share, ShareCreateParams, ShareId, ShareUpdateParams,
    SharesListParams,
};
use leptos::{prelude::*, task::spawn_local};

use super::Workspace;

/// What the share dialog is about.
#[derive(Clone, Debug, PartialEq)]
pub struct ShareDialog {
    pub store: AssetStore,
    pub asset: AssetId,
    pub title: String,
    /// The handle of the asset an attachment was just saved as, which the
    /// dialog names.
    pub saved_as: Option<String>,
    /// The link the dialog shows; `None` until the asset has one.
    pub share: Option<Share>,
    /// The expiry a new link gets.
    pub lifetime: ShareLifetime,
    /// A request is on its way.
    pub busy: bool,
    pub error: Option<String>,
}

/// What the conversation share dialog is about.
#[derive(Clone, Debug, PartialEq)]
pub struct SessionShareDialog {
    pub session_id: SessionId,
    pub title: String,
    /// The link the dialog shows; `None` until the session has one.
    pub share: Option<SessionShare>,
    /// Who a new link is for.
    pub label: String,
    /// Whether a new link's guests may act.
    pub can_send: bool,
    /// The expiry a new link gets.
    pub lifetime: ShareLifetime,
    /// A request is on its way.
    pub busy: bool,
    pub error: Option<String>,
}

/// The newest of `session`'s links.
fn newest_of_session(links: &[SessionShare], session: SessionId) -> Option<SessionShare> {
    links
        .iter()
        .filter(|link| link.session_id == session)
        .max_by_key(|link| link.created.at)
        .cloned()
}

impl Workspace {
    /// Lists the links again.
    pub fn refresh_shares(self) {
        if !self.has_shares.get_untracked() {
            return;
        }
        let Some(client) = self.client() else {
            return;
        };
        spawn_local(async move {
            let result = client
                .list_shares(SharesListParams::default())
                .await
                .map_err(|error| crate::errors::describe_full(&error));
            if let Ok(listed) = &result {
                crate::assets::mark_shared(&listed.shares);
                // The dialog follows its link: revoked or changed elsewhere.
                self.share_dialog.update(|dialog| {
                    if let Some(dialog) = dialog
                        && !dialog.busy
                    {
                        dialog.share = newest_of(&listed.shares, dialog.asset);
                    }
                });
                self.session_share_dialog.update(|dialog| {
                    if let Some(dialog) = dialog
                        && !dialog.busy
                    {
                        dialog.share = newest_of_session(&listed.sessions, dialog.session_id);
                    }
                });
            }
            self.share_list.set(Some(result));
        });
    }

    /// The address links are copied as: the link's own URL, else the
    /// daemon's page address before its path.
    pub fn share_address(self, share: &Share) -> String {
        share_address(share, &self.share_page_base.get_untracked())
    }

    /// The address a conversation share is copied as.
    pub fn session_share_address(self, share: &SessionShare) -> String {
        link_address(
            share.url.as_deref(),
            &share.path,
            &self.share_page_base.get_untracked(),
        )
    }

    /// The live links of `session`, as the Sharing view last read them.
    pub fn session_links(self, session: SessionId) -> Vec<SessionShare> {
        self.share_list.with(|list| match list {
            Some(Ok(listed)) => listed
                .sessions
                .iter()
                .filter(|link| link.session_id == session)
                .cloned()
                .collect(),
            _ => Vec::new(),
        })
    }

    /// Opens the conversation share dialog of the viewed session's tree.
    pub fn open_session_share_dialog(self) {
        if let Some(session) = self.selected.get_untracked() {
            self.open_session_share_dialog_of(self.root_of(session));
        }
    }

    /// Opens the conversation share dialog of the root session `session_id`,
    /// with the link it already has.
    pub fn open_session_share_dialog_of(self, session_id: SessionId) {
        self.panel.set(None);
        let title = self.sessions.with_untracked(|sessions| {
            sessions
                .iter()
                .find(|session| session.session_id == session_id)
                .map(crate::views::session_label)
        });
        self.session_share_dialog.set(Some(SessionShareDialog {
            session_id,
            title: title.unwrap_or_else(|| "This conversation".to_owned()),
            share: None,
            label: String::new(),
            can_send: true,
            lifetime: ShareLifetime::default(),
            busy: false,
            error: None,
        }));
        self.refresh_shares();
    }

    fn update_session_share_dialog(self, change: impl FnOnce(&mut SessionShareDialog)) {
        self.session_share_dialog.update(|dialog| {
            if let Some(dialog) = dialog {
                change(dialog);
            }
        });
    }

    /// Makes a link for the dialog's session, beside any it has.
    pub fn create_session_share(self) {
        let (Some(client), Some(dialog)) =
            (self.client(), self.session_share_dialog.get_untracked())
        else {
            return;
        };
        self.update_session_share_dialog(|dialog| {
            dialog.busy = true;
            dialog.error = None;
        });
        let label = dialog.label.trim().to_owned();
        spawn_local(async move {
            let made = client
                .create_session_share(SessionShareCreateParams {
                    session_id: dialog.session_id,
                    label: (!label.is_empty()).then_some(label),
                    can_send: dialog.can_send,
                    expiry: dialog.lifetime.expiry(jiff::Timestamp::now()),
                })
                .await;
            self.update_session_share_dialog(|dialog| {
                dialog.busy = false;
                match made {
                    Ok(made) => dialog.share = Some(made.share),
                    Err(error) => dialog.error = Some(crate::errors::describe(&error)),
                }
            });
            self.refresh_shares();
        });
    }

    /// Changes a conversation share.
    pub fn update_session_share(self, params: SessionShareUpdateParams) {
        let Some(client) = self.client() else {
            return;
        };
        spawn_local(async move {
            match client.update_session_share(params).await {
                Ok(updated) => {
                    self.update_session_share_dialog(|dialog| {
                        if dialog
                            .share
                            .as_ref()
                            .is_some_and(|share| share.id == updated.share.id)
                        {
                            dialog.share = Some(updated.share);
                        }
                    });
                    self.refresh_shares();
                }
                Err(error) => self.status.set(crate::errors::describe(&error)),
            }
        });
    }

    /// Copies a conversation share's address.
    pub fn copy_session_share(self, share: &SessionShare) {
        let address = self.session_share_address(share);
        spawn_local(async move {
            let copied = crate::browser::copy_text(&address).await;
            self.status.set(if copied {
                "link copied".to_owned()
            } else {
                format!("copying needs a secure page; the link is {address}")
            });
        });
    }

    /// Whether links made now probably work only where the daemon's page can
    /// be reached.
    pub fn shares_look_private(self) -> bool {
        match self.share_url.get() {
            Some(base) => looks_private(&base, true),
            None => looks_private(&self.share_page_base.get(), false),
        }
    }

    /// Opens the share dialog for the asset `asset` of `store`, with the
    /// link it already has.
    pub fn open_share_dialog(self, store: AssetStore, asset: AssetId, title: String) {
        self.open_share_dialog_saved(store, asset, title, None);
    }

    fn open_share_dialog_saved(
        self,
        store: AssetStore,
        asset: AssetId,
        title: String,
        saved_as: Option<String>,
    ) {
        if !self.has_shares.get_untracked() {
            self.status
                .set("this daemon has no share links: it needs client protocol 1.86".into());
            return;
        }
        let share = self.share_list.with_untracked(|list| {
            list.as_ref()
                .and_then(|list| list.as_ref().ok())
                .and_then(|list| newest_of(&list.shares, asset))
        });
        self.share_dialog.set(Some(ShareDialog {
            store,
            asset,
            title,
            saved_as,
            share,
            lifetime: ShareLifetime::default(),
            busy: false,
            error: None,
        }));
        self.refresh_shares();
    }

    /// Saves the viewed session's attached file `key` as an asset of the
    /// session, then opens the share dialog for it.
    pub fn share_attachment(self, key: (u64, u32, u32)) {
        let (Some(client), Some(session_id)) = (self.client(), self.selected.get_untracked())
        else {
            return;
        };
        spawn_local(async move {
            match client
                .save_attachment_asset(cookie_agent_protocol::AttachmentSaveAssetParams {
                    session_id,
                    model_turn_seq: key.0,
                    content_index: key.1,
                    ordinal: key.2,
                    scope: cookie_agent_protocol::AssetScope::Session,
                })
                .await
            {
                Ok(saved) => self.open_share_dialog_saved(
                    saved.asset.store,
                    saved.asset.id,
                    saved.asset.title,
                    Some(saved.asset.handle.to_string()),
                ),
                Err(error) => self.status.set(crate::errors::describe(&error)),
            }
        });
    }

    fn dialog_busy(self, busy: bool, error: Option<String>) {
        self.share_dialog.update(|dialog| {
            if let Some(dialog) = dialog {
                dialog.busy = busy;
                dialog.error = error;
            }
        });
    }

    /// Makes the dialog's asset a link: a new one with `fresh`, else the
    /// one it has when it has one.
    pub fn create_share(self, fresh: bool) {
        let Some(client) = self.client() else {
            return;
        };
        let Some(dialog) = self.share_dialog.get_untracked() else {
            return;
        };
        self.dialog_busy(true, None);
        spawn_local(async move {
            let made = client
                .create_share(ShareCreateParams {
                    store: dialog.store.clone(),
                    asset: dialog.asset,
                    expiry: dialog.lifetime.expiry(jiff::Timestamp::now()),
                    reuse: !fresh,
                })
                .await;
            match made {
                Ok(made) => {
                    self.share_dialog.update(|open| {
                        if let Some(open) = open
                            && open.asset == dialog.asset
                        {
                            open.share = Some(made.share);
                            open.busy = false;
                            open.error = None;
                        }
                    });
                    self.refresh_shares();
                }
                Err(error) => self.dialog_busy(false, Some(crate::errors::describe(&error))),
            }
        });
    }

    /// Gives the link `id` the expiry `lifetime`, counted from now.
    pub fn set_share_lifetime(self, id: ShareId, lifetime: ShareLifetime) {
        let Some(client) = self.client() else {
            return;
        };
        spawn_local(async move {
            match client
                .update_share(ShareUpdateParams {
                    id: id.clone(),
                    title: None,
                    expiry: Some(lifetime.expiry(jiff::Timestamp::now())),
                })
                .await
            {
                Ok(updated) => {
                    self.share_dialog.update(|dialog| {
                        if let Some(dialog) = dialog
                            && dialog.share.as_ref().is_some_and(|share| share.id == id)
                        {
                            dialog.share = Some(updated.share);
                            dialog.lifetime = lifetime;
                        }
                    });
                    self.refresh_shares();
                }
                Err(error) => self.status.set(crate::errors::describe(&error)),
            }
        });
    }

    /// Revokes the link `id`.
    pub fn revoke_share(self, id: ShareId) {
        let Some(client) = self.client() else {
            return;
        };
        spawn_local(async move {
            match client.revoke_share(id.clone()).await {
                Ok(_) => {
                    self.share_dialog.update(|dialog| {
                        if let Some(dialog) = dialog
                            && dialog.share.as_ref().is_some_and(|share| share.id == id)
                        {
                            dialog.share = None;
                        }
                    });
                    self.update_session_share_dialog(|dialog| {
                        if dialog.share.as_ref().is_some_and(|share| share.id == id) {
                            dialog.share = None;
                        }
                    });
                    self.status.set("link revoked".into());
                    self.refresh_shares();
                }
                Err(error) => self.status.set(crate::errors::describe(&error)),
            }
        });
    }

    /// Revokes every link.
    pub fn revoke_all_shares(self) {
        let Some(client) = self.client() else {
            return;
        };
        spawn_local(async move {
            match client.revoke_all_shares().await {
                Ok(revoked) => {
                    self.share_revoking_all.set(false);
                    self.status.set(format!(
                        "revoked {} link{}",
                        revoked.revoked,
                        if revoked.revoked == 1 { "" } else { "s" }
                    ));
                    self.refresh_shares();
                }
                Err(error) => self.status.set(crate::errors::describe(&error)),
            }
        });
    }

    /// Copies the link `share` and says so.
    pub fn copy_share(self, share: &Share) {
        let address = self.share_address(share);
        spawn_local(async move {
            let copied = crate::browser::copy_text(&address).await;
            self.status.set(if copied {
                "link copied".to_owned()
            } else {
                format!("copying needs a secure page; the link is {address}")
            });
        });
    }
}

/// The newest live link of `asset` among `shares`.
pub fn newest_of(shares: &[Share], asset: AssetId) -> Option<Share> {
    shares
        .iter()
        .filter(|share| share.asset == asset)
        .max_by_key(|share| share.created.at)
        .cloned()
}
