//! The Assets tab's reads and changes (`docs/site/specs/assets.md` section
//! 8.3): the assets its filter lists (`assets.search`), renaming
//! (`asset.rename`), and deleting (`asset.delete`). `assets.changed` lists
//! them again, a moment after the last of a burst.

use cookie_agent_client_state::assets::AssetFilter;
use cookie_agent_protocol::{AssetId, AssetRenameParams, AssetStore};
use leptos::{prelude::*, task::spawn_local};

use super::Workspace;

/// How many rows the tab shows at first, and how many more each *Show
/// more* adds.
pub const ASSET_ROWS: usize = 100;

/// How long the tab waits after an `assets.changed` before it lists again,
/// so a burst of writes lists once.
const CHANGED_SETTLE: std::time::Duration = std::time::Duration::from_millis(300);

impl Workspace {
    /// Opens the tab, listing its assets.
    pub(super) fn open_assets(self) {
        self.assets_opened.set(true);
        self.poll_assets();
    }

    /// Lists what `filter` chooses.
    pub fn choose_asset_filter(self, filter: AssetFilter) {
        if self.asset_filter.get_untracked() == filter {
            return;
        }
        self.asset_filter.set(filter);
        self.asset_list.set(None);
        self.asset_rows.set(ASSET_ROWS);
        self.asset_open.set(None);
        self.asset_renaming.set(None);
        self.asset_deleting.set(None);
        self.poll_assets();
    }

    /// Lists the tab's assets again once no other `assets.changed` came
    /// for a moment.
    pub(super) fn assets_changed_later(self) {
        let mark = self.assets_changed_at.get_value().wrapping_add(1);
        self.assets_changed_at.set_value(mark);
        spawn_local(async move {
            gloo_timers::future::sleep(CHANGED_SETTLE).await;
            if self.assets_changed_at.try_get_value() == Some(mark) {
                self.poll_assets();
            }
        });
    }

    /// Reads the assets the tab's filter lists.
    pub(super) fn poll_assets(self) {
        let read = self.begin_read(|reads| &mut reads.assets);
        let Some(client) = self.client() else {
            return;
        };
        let params = self.asset_filter.get_untracked().params();
        spawn_local(async move {
            let result = client
                .search_assets(params)
                .await
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
