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

/// Where the browser remembers the tab's layout: `list`, or the grid.
pub const ASSET_LAYOUT_KEY: &str = "cookie.assets_layout";

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
        self.asset_page.set(None);
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

    /// Answers the viewed session's attachment `key` (`turn`, `content`,
    /// `ordinal`) that waits for the user.
    pub fn decide_attachment(self, key: (u64, u32, u32), allow: bool) {
        let (Some(client), Some(session_id)) = (self.client(), self.selected.get_untracked())
        else {
            return;
        };
        spawn_local(async move {
            if let Err(error) = client
                .decide_attachment(cookie_agent_protocol::AttachmentDecideParams {
                    session_id,
                    model_turn_seq: key.0,
                    content_index: key.1,
                    ordinal: key.2,
                    allow,
                })
                .await
            {
                self.status.set(crate::errors::describe(&error));
            }
        });
    }

    /// Makes an asset of the viewed session's attached file `key`, kept in
    /// `scope`, and says its handle.
    pub fn save_attachment(self, key: (u64, u32, u32), scope: cookie_agent_protocol::AssetScope) {
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
                    scope,
                })
                .await
            {
                Ok(saved) => self.status.set(format!(
                    "saved as {}, {}",
                    saved.asset.handle,
                    match scope {
                        cookie_agent_protocol::AssetScope::Project => "kept for the project",
                        cookie_agent_protocol::AssetScope::Session => "kept for this session",
                    }
                )),
                Err(error) => self.status.set(crate::errors::describe(&error)),
            }
        });
    }

    /// Shows the tab as a list, or as a grid, and remembers it.
    pub fn choose_asset_layout(self, list: bool) {
        self.asset_list_layout.set(list);
        crate::browser::store(ASSET_LAYOUT_KEY, if list { "list" } else { "grid" });
    }

    /// Takes the deleted `asset` out of the media viewer: the next file
    /// shows in its place, and the viewer closes when it was the last.
    fn forget_viewed(self, asset: AssetId) {
        let showing = self.media_viewer.with_untracked(|viewing| {
            viewing
                .as_ref()
                .is_some_and(|viewing| viewing.asset.as_ref().is_some_and(|(_, id)| *id == asset))
        });
        let at = self.media_group.with_untracked(|group| {
            group
                .iter()
                .position(|viewing| viewing.asset.as_ref().is_some_and(|(_, id)| *id == asset))
        });
        if let Some(at) = at {
            self.media_group.update(|group| {
                group.remove(at);
            });
        }
        if showing {
            let next = self.media_group.with_untracked(|group| {
                group
                    .get(at.unwrap_or(0).min(group.len().saturating_sub(1)))
                    .cloned()
            });
            self.media_viewer.set(next);
        }
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
                Ok(renamed) => {
                    self.asset_renaming.set(None);
                    self.asset_page.update(|page| {
                        if let Some(page) = page.as_mut().filter(|page| page.id == asset) {
                            page.title = renamed.asset.title.clone();
                        }
                    });
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
                    if self
                        .asset_page
                        .with_untracked(|page| page.as_ref().is_some_and(|page| page.id == asset))
                    {
                        self.asset_page.set(None);
                    }
                    self.forget_viewed(asset);
                    self.poll_assets();
                }
                Err(error) => self.status.set(crate::errors::describe(&error)),
            }
        });
    }
}
