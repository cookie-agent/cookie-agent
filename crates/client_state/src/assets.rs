//! Assets in clients (`docs/site/specs/assets.md`): the shelves a
//! transcript shows the assets its figures showed on (section 8.1), the
//! links into the web page the TUI prints (section 9), and how the web's
//! Assets tab sorts out what it lists (section 8.3).

use std::collections::HashMap;

use cookie_agent_protocol::{AssetBlock, AssetHandle, DisplayBlock, Open, SessionId, ToolCallId};

use crate::{AssistantChild, SessionState, TranscriptItem, transcript_mode};

/// The assets a run's `figure` calls showed, under the first text the run
/// wrote after them: the message that comes out of their work.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Shelf {
    /// The child of its block the shelf shows after; `None` after the
    /// block's last, under its work, while no text followed the calls yet.
    pub after: Option<usize>,
    /// The assets, once each, in the order first shown.
    pub entries: Vec<ShelfEntry>,
}

/// One asset on a shelf.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ShelfEntry {
    /// The call that showed it last.
    pub call_id: ToolCallId,
    /// The asset as that call showed it.
    pub block: AssetBlock,
    /// The caption of the call that first showed it, on the first asset
    /// that call showed.
    pub caption: Option<String>,
}

impl Shelf {
    /// Hashes what the shelf shows, for a client's cache keys.
    pub fn hash_shown(&self, hasher: &mut impl std::hash::Hasher) {
        use std::hash::Hash as _;
        self.after.hash(hasher);
        for entry in &self.entries {
            entry.call_id.hash(hasher);
            entry.block.asset.hash(hasher);
            entry.block.sha256.hash(hasher);
            entry.block.title.hash(hasher);
            entry.caption.hash(hasher);
        }
    }

    /// The first entry's call, which names the shelf while it moves from
    /// under the work to under the text that follows.
    #[must_use]
    pub fn first_call(&self) -> Option<&ToolCallId> {
        self.entries.first().map(|entry| &entry.call_id)
    }
}

/// The shelves of `state`'s transcript, under the ID of the block each
/// shows in. A run's blocks are one stretch however far apart they are; a
/// block of no run is a stretch of its own. The text a shelf goes under is
/// any but [`transcript_mode::filler`], with the texts right after it in
/// its block.
#[must_use]
pub fn shelves(state: &SessionState) -> HashMap<u64, Vec<Shelf>> {
    let mut stretches: Vec<Vec<(u64, &[AssistantChild])>> = Vec::new();
    let mut runs = HashMap::new();
    for item in &state.transcript {
        let TranscriptItem::Assistant { id, children, .. } = item else {
            continue;
        };
        let at = match state.assistant_runs.get(id) {
            Some(run) => *runs.entry(*run).or_insert_with(|| {
                stretches.push(Vec::new());
                stretches.len() - 1
            }),
            None => {
                stretches.push(Vec::new());
                stretches.len() - 1
            }
        };
        stretches[at].push((*id, children.as_slice()));
    }
    let mut shelves: HashMap<u64, Vec<Shelf>> = HashMap::new();
    for blocks in stretches {
        let mut entries: Vec<ShelfEntry> = Vec::new();
        for (id, children) in &blocks {
            let mut index = 0;
            while index < children.len() {
                match &children[index] {
                    AssistantChild::Tool { call_id } => shelve(state, call_id, &mut entries),
                    AssistantChild::Text { markdown, .. }
                        if !entries.is_empty() && !transcript_mode::filler(markdown.as_str()) =>
                    {
                        while matches!(children.get(index + 1), Some(AssistantChild::Text { .. })) {
                            index += 1;
                        }
                        shelves.entry(*id).or_default().push(Shelf {
                            after: Some(index),
                            entries: std::mem::take(&mut entries),
                        });
                    }
                    _ => {}
                }
                index += 1;
            }
        }
        if let Some((id, _)) = blocks.last()
            && !entries.is_empty()
        {
            shelves.entry(*id).or_default().push(Shelf {
                after: None,
                entries,
            });
        }
    }
    shelves
}

/// Adds what the call `call_id` showed, when it is a figure, to `entries`.
fn shelve(state: &SessionState, call_id: &ToolCallId, entries: &mut Vec<ShelfEntry>) {
    if !transcript_mode::is_figure(state, call_id) {
        return;
    }
    let Some(tool) = state.tools.get(call_id) else {
        return;
    };
    let mut caption = tool.shown_display().iter().find_map(|block| match block {
        Open::Known(DisplayBlock::Text { text, .. }) if !text.trim().is_empty() => {
            Some(text.clone())
        }
        _ => None,
    });
    for block in tool.shown_display() {
        let Open::Known(DisplayBlock::Asset(block)) = block else {
            continue;
        };
        match entries
            .iter_mut()
            .find(|entry| entry.block.asset == block.asset)
        {
            // Shown again: where it first showed, as the latest call shows it.
            Some(entry) => {
                entry.call_id = *call_id;
                entry.block = block.clone();
                if entry.caption.is_none() {
                    entry.caption = caption.take();
                }
            }
            None => entries.push(ShelfEntry {
                call_id: *call_id,
                block: block.clone(),
                caption: caption.take(),
            }),
        }
    }
}

/// The web page's link that opens `session` and shows its figure of the
/// asset `handle`, on the page at `page_url`. The link carries no token: a
/// browser that holds one connects, and one that does not asks for it.
#[must_use]
pub fn asset_link(page_url: &str, session: SessionId, handle: &AssetHandle) -> String {
    let page = page_url.split('#').next().unwrap_or(page_url);
    let separator = if page.ends_with('/') { "" } else { "/" };
    format!("{page}{separator}#session={session}&asset={handle}")
}

/// What kind of file an asset is, as the Assets tab groups and filters
/// them.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum AssetKind {
    Image,
    /// An HTML page.
    Page,
    /// A Markdown document.
    Document,
    /// Video or audio.
    Media,
    Other,
}

impl AssetKind {
    /// Every kind, in the order the tab's filter lists them.
    pub const ALL: [Self; 5] = [
        Self::Image,
        Self::Page,
        Self::Document,
        Self::Media,
        Self::Other,
    ];

    /// The kind of a file of `media_type`.
    #[must_use]
    pub fn of(media_type: &str) -> Self {
        if media_type.starts_with("image/") {
            Self::Image
        } else if media_type == "text/html" {
            Self::Page
        } else if media_type == "text/markdown" {
            Self::Document
        } else if media_type.starts_with("video/") || media_type.starts_with("audio/") {
            Self::Media
        } else {
            Self::Other
        }
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Image => "Images",
            Self::Page => "Pages",
            Self::Document => "Documents",
            Self::Media => "Video and audio",
            Self::Other => "Other files",
        }
    }
}

/// The assets of `assets` the tab shows: of `kind` when one is chosen,
/// whose title or file name holds `query` (any case), in the order given.
#[must_use]
pub fn assets_matching<'a>(
    assets: &'a [cookie_agent_protocol::Asset],
    query: &str,
    kind: Option<AssetKind>,
) -> Vec<&'a cookie_agent_protocol::Asset> {
    let query = query.trim().to_lowercase();
    assets
        .iter()
        .filter(|asset| kind.is_none_or(|kind| AssetKind::of(&asset.media_type) == kind))
        .filter(|asset| {
            query.is_empty()
                || asset.title.to_lowercase().contains(&query)
                || asset
                    .filename
                    .as_deref()
                    .is_some_and(|name| name.to_lowercase().contains(&query))
        })
        .collect()
}

/// The bytes `assets` take together.
#[must_use]
pub fn total_bytes(assets: &[cookie_agent_protocol::Asset]) -> u64 {
    assets.iter().map(|asset| asset.byte_length).sum()
}

#[cfg(test)]
mod listing_tests {
    use cookie_agent_protocol::{Asset, AssetHandle, AssetId, AssetStamp, AssetStore};

    use super::*;

    fn asset(title: &str, filename: &str, media_type: &str, byte_length: u64) -> Asset {
        let stamp = AssetStamp {
            at: jiff::Timestamp::UNIX_EPOCH,
            session: SessionId::new_v7(),
            agent: None,
        };
        Asset {
            id: AssetId::new_v7(),
            handle: AssetHandle::random(),
            store: AssetStore::Shared,
            title: title.into(),
            media_type: media_type.into(),
            byte_length,
            sha256: "a".repeat(64),
            filename: Some(filename.into()),
            created: stamp.clone(),
            updated: stamp,
        }
    }

    #[test]
    fn the_tab_filters_by_kind_and_searches_titles_and_file_names() {
        let assets = vec![
            asset("Loss curves", "loss.html", "text/html", 10),
            asset("Weekly report", "report.md", "text/markdown", 20),
            asset("Plot", "accuracy.png", "image/png", 30),
            asset("Data", "data.csv", "text/plain", 40),
        ];
        let titles = |shown: Vec<&Asset>| {
            shown
                .iter()
                .map(|asset| asset.title.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(titles(assets_matching(&assets, "", None)).len(), 4);
        assert_eq!(
            titles(assets_matching(&assets, "", Some(AssetKind::Page))),
            ["Loss curves"]
        );
        assert_eq!(
            titles(assets_matching(&assets, "REPORT", None)),
            ["Weekly report"]
        );
        assert_eq!(titles(assets_matching(&assets, "accuracy", None)), ["Plot"]);
        assert!(assets_matching(&assets, "report", Some(AssetKind::Image)).is_empty());
        assert_eq!(total_bytes(&assets), 100);
        assert_eq!(AssetKind::of("image/svg+xml"), AssetKind::Image);
        assert_eq!(AssetKind::of("audio/mpeg"), AssetKind::Media);
        assert_eq!(AssetKind::of("application/pdf"), AssetKind::Other);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_link_opens_the_session_and_its_asset_on_the_page() {
        let session = SessionId::new_v7();
        let handle = AssetHandle::new("asset_3f9a1c2e").unwrap();
        assert_eq!(
            asset_link("http://127.0.0.1:7419/", session, &handle),
            format!("http://127.0.0.1:7419/#session={session}&asset=asset_3f9a1c2e")
        );
        // A page URL without a trailing slash, or with a fragment, gets one
        // and loses it.
        assert_eq!(
            asset_link("https://cookie.example", session, &handle),
            format!("https://cookie.example/#session={session}&asset=asset_3f9a1c2e")
        );
        assert_eq!(
            asset_link("https://cookie.example/app/#token=x", session, &handle),
            format!("https://cookie.example/app/#session={session}&asset=asset_3f9a1c2e")
        );
    }
}
