//! The Assets tab's reads and changes (`docs/site/specs/assets.md` section
//! 8.3): a store's assets (`assets.list`), renaming (`asset.rename`), and
//! deleting (`asset.delete`). `assets.changed` reads the listing again.

use cookie_agent_protocol::{AssetId, AssetRenameParams, AssetStore};
use leptos::{prelude::*, task::spawn_local};

use super::Workspace;

impl Workspace {
    /// The store the tab shows first: the open session's project's, or
    /// the shared store of sessions in no project.
    pub fn default_asset_store(self) -> AssetStore {
        let project = self.selected.get_untracked().and_then(|selected| {
            self.sessions.with_untracked(|sessions| {
                sessions
                    .iter()
                    .find(|meta| meta.session_id == selected)
                    .and_then(|meta| meta.project.clone())
            })
        });
        match project {
            Some(project) => AssetStore::Project { project },
            None => AssetStore::Shared,
        }
    }

    /// Opens the tab: on the open session's project the first time.
    pub(super) fn open_assets(self) {
        if self.asset_store.get_untracked().is_none() {
            self.asset_store.set(Some(self.default_asset_store()));
        }
        self.poll_assets();
    }

    /// Shows `store`'s assets.
    pub fn choose_asset_store(self, store: AssetStore) {
        self.asset_store.set(Some(store));
        self.asset_list.set(None);
        self.asset_open.set(None);
        self.asset_renaming.set(None);
        self.asset_deleting.set(None);
        self.poll_assets();
    }

    /// Reads the shown store's assets.
    pub(super) fn poll_assets(self) {
        let read = self.begin_read(|reads| &mut reads.assets);
        let (Some(client), Some(store)) = (self.client(), self.asset_store.get_untracked()) else {
            return;
        };
        spawn_local(async move {
            let result = client
                .list_assets(store)
                .await
                .map(|result| result.assets)
                .map_err(|error| crate::errors::describe_full(&error));
            if self.read_is_current(|reads| &mut reads.assets, read) {
                self.asset_list.set(Some(result));
            }
        });
    }

    /// Gives an asset of `store` the title `title`.
    pub fn rename_asset(self, store: AssetStore, asset: AssetId, title: String) {
        let Some(client) = self.client() else {
            return;
        };
        let title = title.trim().to_owned();
        if !cookie_agent_protocol::is_asset_title(&title) {
            self.status.set(format!(
                "a title is 1 to {} characters on one line",
                cookie_agent_protocol::ASSET_TITLE_MAX_CHARS
            ));
            return;
        }
        spawn_local(async move {
            match client
                .rename_asset(AssetRenameParams {
                    store,
                    asset,
                    title,
                })
                .await
            {
                Ok(_) => {
                    self.asset_renaming.set(None);
                    self.poll_assets();
                }
                Err(error) => self.status.set(crate::errors::describe(&error)),
            }
        });
    }

    /// Deletes an asset of `store`.
    pub fn delete_asset(self, store: AssetStore, asset: AssetId) {
        let Some(client) = self.client() else {
            return;
        };
        spawn_local(async move {
            match client.delete_asset(store, asset).await {
                Ok(_) => {
                    self.asset_deleting.set(None);
                    if self.asset_open.get_untracked() == Some(asset) {
                        self.asset_open.set(None);
                    }
                    self.poll_assets();
                }
                Err(error) => self.status.set(crate::errors::describe(&error)),
            }
        });
    }
}
