//! Assets (`docs/site/specs/assets.md`, client protocol 1.67.0): files that
//! agents write for the user, kept by a session tree or a project and shown
//! in the conversation as figures.
//!
//! Agents write and show assets with the `assets` tool group; clients list,
//! read, rename, and delete them with the `asset.*` methods here and follow
//! `assets.changed`. A figure is an [`crate::DisplayBlock::Asset`] block.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{AgentId, AssetHandle, AssetId, ProjectId, SessionId, SessionTitle, human_bytes};

pub const ASSETS_LIST_METHOD: &str = "assets.list";
pub const ASSETS_SEARCH_METHOD: &str = "assets.search";
pub const ASSET_GET_METHOD: &str = "asset.get";
pub const ASSET_READ_METHOD: &str = "asset.read";
pub const ASSET_THUMBNAIL_METHOD: &str = "asset.thumbnail";
pub const ASSET_RENAME_METHOD: &str = "asset.rename";
pub const ASSET_DELETE_METHOD: &str = "asset.delete";
pub const ASSETS_CHANGED_METHOD: &str = "assets.changed";

/// The most bytes one asset holds.
pub const ASSET_MAX_BYTES: u64 = 25 * 1024 * 1024;
/// The most characters an asset's title has.
pub const ASSET_TITLE_MAX_CHARS: usize = 200;
/// The most characters a figure's caption has.
pub const ASSET_CAPTION_MAX_CHARS: usize = 300;
/// The most assets one `show_asset` call shows.
pub const ASSET_SHOW_MAX: usize = 12;
/// The most bytes one `asset.read` returns.
pub const ASSET_CHUNK_BYTES: u64 = 1024 * 1024;
/// The longest side of an `asset.thumbnail` picture, in pixels.
pub const ASSET_THUMBNAIL_MAX_EDGE: u32 = 768;

/// Where an asset is kept (section 4.1). A store's assets are listed,
/// read, and changed together; an asset never moves between stores.
#[derive(
    Clone, Debug, Deserialize, Eq, Hash, JsonSchema, Ord, PartialEq, PartialOrd, Serialize,
)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AssetStore {
    /// A session tree's own assets (scope `session`), named by the tree's
    /// root. They are deleted with the tree.
    Session { session: SessionId },
    /// A project's assets (scope `project`).
    Project { project: ProjectId },
    /// The assets of every session in no project (scope `project`).
    Shared,
}

impl AssetStore {
    /// The scope an asset of this store has.
    #[must_use]
    pub const fn scope(&self) -> AssetScope {
        match self {
            Self::Session { .. } => AssetScope::Session,
            Self::Project { .. } | Self::Shared => AssetScope::Project,
        }
    }
}

/// Who an asset belongs to: the session tree that wrote it, or every
/// session of its project (section 4.1).
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, Hash, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AssetScope {
    #[default]
    Session,
    Project,
}

impl AssetScope {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Session => "session",
            Self::Project => "project",
        }
    }
}

/// When an asset was written, and by which session and agent.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct AssetStamp {
    pub at: jiff::Timestamp,
    /// The session whose call wrote it. It may have been deleted since.
    pub session: SessionId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<AgentId>,
}

/// One asset as it is now (section 3).
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct Asset {
    pub id: AssetId,
    pub handle: AssetHandle,
    pub store: AssetStore,
    pub title: String,
    /// The media type of the current content, such as `image/png`,
    /// `text/html`, or `text/markdown`.
    pub media_type: String,
    pub byte_length: u64,
    /// Lowercase hex SHA-256 of the current content. It changes when the
    /// asset is written again, so a client knows to read it again.
    pub sha256: String,
    /// The name of the file the asset was last written from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
    pub created: AssetStamp,
    pub updated: AssetStamp,
}

impl Asset {
    /// How the asset reads in one line: its handle, title, type, and size.
    #[must_use]
    pub fn summary(&self) -> String {
        asset_summary(
            &self.handle,
            &self.title,
            &self.media_type,
            self.byte_length,
        )
    }

    /// The asset as a figure shows it.
    #[must_use]
    pub fn block(&self) -> AssetBlock {
        AssetBlock {
            store: self.store.clone(),
            asset: self.id,
            handle: self.handle.clone(),
            title: self.title.clone(),
            media_type: self.media_type.clone(),
            byte_length: self.byte_length,
            sha256: self.sha256.clone(),
            filename: self.filename.clone(),
        }
    }
}

/// `asset_3f9a1c2e "Loss curves" (text/html, 48 KB)`.
#[must_use]
pub fn asset_summary(
    handle: &AssetHandle,
    title: &str,
    media_type: &str,
    byte_length: u64,
) -> String {
    format!(
        "{handle} {title:?} ({media_type}, {})",
        human_bytes(byte_length)
    )
}

/// An asset shown as a figure (section 10): which asset, and what it was
/// when the call showed it. Clients show the asset as it is now, read
/// with `asset.get`, and fall back to these fields when it is gone; a
/// current `sha256` that differs from this one says it was replaced since.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct AssetBlock {
    pub store: AssetStore,
    pub asset: AssetId,
    pub handle: AssetHandle,
    pub title: String,
    pub media_type: String,
    pub byte_length: u64,
    /// Lowercase hex SHA-256 of the content shown.
    pub sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
}

impl AssetBlock {
    /// The block's canonical text: its asset's one-line summary.
    #[must_use]
    pub fn to_text(&self) -> String {
        format!(
            "asset {}",
            asset_summary(
                &self.handle,
                &self.title,
                &self.media_type,
                self.byte_length
            )
        )
    }
}

/// Lists a store's assets, newest first.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AssetsListParams {
    pub store: AssetStore,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct AssetsListResult {
    pub assets: Vec<Asset>,
}

/// Lists the assets of many stores at once, newest first, as the web's
/// Assets tab does (client protocol 1.71.0): those of one project, of the
/// sessions in no project, or of every one, and of one scope or both.
#[derive(Clone, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AssetsSearchParams {
    /// Whose assets; every project's and those of sessions in no project
    /// when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<AssetProjectFilter>,
    /// The session trees' own assets, the projects' kept assets, or both
    /// when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<AssetScope>,
}

/// Which project's assets `assets.search` lists: a session tree's own by
/// the project of its root.
#[derive(Clone, Debug, Deserialize, Eq, Hash, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AssetProjectFilter {
    Project {
        project: ProjectId,
    },
    /// Sessions in no project, and their shared store.
    NoProject,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct AssetsSearchResult {
    pub assets: Vec<Asset>,
    /// The root of each session store the assets come from, to name it.
    pub sessions: Vec<AssetSession>,
}

/// A session tree whose own assets `assets.search` listed.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct AssetSession {
    /// The tree's root.
    pub session: SessionId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<SessionTitle>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<ProjectId>,
}

/// One asset as it is now.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AssetGetParams {
    pub store: AssetStore,
    pub asset: AssetId,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct AssetGetResult {
    pub asset: Asset,
}

/// Reads part of an asset's current content, as `artifact.read` reads an
/// artifact.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AssetReadParams {
    pub store: AssetStore,
    pub asset: AssetId,
    /// At most `total_bytes`; reading at `total_bytes` returns no data.
    pub offset: u64,
    #[schemars(range(min = 1, max = ASSET_CHUNK_BYTES))]
    pub length: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct AssetReadResult {
    /// Base64 of the bytes read: fewer than `length` at the end.
    pub data: String,
    pub total_bytes: u64,
    pub media_type: String,
    /// The content the bytes come from. A read whose `sha256` differs from
    /// an earlier one's read content written in between: start again.
    pub sha256: String,
}

/// A small picture of an asset's current content, made by the daemon
/// (client protocol 1.93.0, `docs/site/specs/asset-thumbnails.md`): a
/// picture scaled down, a video's first frame, or a PDF's first page.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AssetThumbnailParams {
    pub store: AssetStore,
    pub asset: AssetId,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct AssetThumbnailResult {
    /// The content the thumbnail shows.
    pub sha256: String,
    /// Absent when the daemon makes none for this content: a kind it does
    /// not draw, a file it cannot decode, or a video or PDF without the
    /// tool that draws it on the daemon's `PATH`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thumbnail: Option<AssetThumbnail>,
}

/// A thumbnail: a JPEG, or a PNG for a picture with transparency, whose
/// longer side is at most [`ASSET_THUMBNAIL_MAX_EDGE`] pixels.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct AssetThumbnail {
    pub media_type: String,
    /// Base64 of the picture.
    pub data: String,
    pub width: u32,
    pub height: u32,
}

/// Gives an asset a new title.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AssetRenameParams {
    pub store: AssetStore,
    pub asset: AssetId,
    #[schemars(length(min = 1, max = ASSET_TITLE_MAX_CHARS))]
    pub title: String,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct AssetRenameResult {
    pub asset: Asset,
}

/// Deletes an asset. Figures of it then show it was deleted.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AssetDeleteParams {
    pub store: AssetStore,
    pub asset: AssetId,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct AssetDeleteResult {}

/// An asset was written, renamed, or deleted through this daemon; refetch
/// what shows assets.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
pub struct AssetsChangedNotification {}

/// Whether `title` is a title an asset may have: 1 to
/// [`ASSET_TITLE_MAX_CHARS`] characters, on one line, with no control
/// characters.
#[must_use]
pub fn is_asset_title(title: &str) -> bool {
    let count = title.chars().count();
    (1..=ASSET_TITLE_MAX_CHARS).contains(&count)
        && !title.chars().any(char::is_control)
        && !title.trim().is_empty()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn handles_are_asset_and_eight_hex_digits() {
        let handle = AssetHandle::random();
        assert!(AssetHandle::new(handle.as_str()).is_ok(), "{handle}");
        assert!(AssetHandle::new("asset_3f9a1c2e").is_ok());
        for invalid in [
            "asset_3F9A1C2E",
            "asset_3f9a1c2",
            "asset_3f9a1c2eff",
            "assets_3f9a1c2e",
            "3f9a1c2e",
        ] {
            assert!(AssetHandle::new(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn a_store_is_tagged_by_its_kind() {
        let session = SessionId::new_v7();
        let store = AssetStore::Session { session };
        assert_eq!(
            serde_json::to_value(&store).unwrap(),
            json!({"type": "session", "session": session})
        );
        assert_eq!(
            serde_json::from_value::<AssetStore>(json!({"type": "shared"})).unwrap(),
            AssetStore::Shared
        );
        assert_eq!(store.scope(), AssetScope::Session);
        assert_eq!(AssetStore::Shared.scope(), AssetScope::Project);
    }

    #[test]
    fn titles_are_one_short_line() {
        assert!(is_asset_title("Loss curves"));
        assert!(is_asset_title(&"t".repeat(ASSET_TITLE_MAX_CHARS)));
        assert!(!is_asset_title(&"t".repeat(ASSET_TITLE_MAX_CHARS + 1)));
        assert!(!is_asset_title(""));
        assert!(!is_asset_title("   "));
        assert!(!is_asset_title("two\nlines"));
    }
}
