//! Assets in clients (`docs/site/specs/assets.md`): the assets replies show
//! with `<attach asset="…"/>` (`docs/site/specs/message-attachments.md`),
//! the links into the web page the TUI prints (section 9), and how the web's
//! Assets tab sorts out what it lists (section 8.3).

use std::collections::HashMap;

use cookie_agent_protocol::{AssetBlock, AssetHandle, SessionId};

use crate::SessionState;

/// Every asset an `<attach asset="…"/>` tag of `state` showed, by handle,
/// as the latest showed it: what a reply's handles name, as chips.
#[must_use]
pub fn shown_assets(state: &SessionState) -> HashMap<String, AssetBlock> {
    let mut shown = HashMap::new();
    for attachments in state.attachments.values() {
        for attachment in attachments.values() {
            if let cookie_agent_protocol::AttachmentOutcome::Shown { asset } = &attachment.outcome {
                shown.insert(asset.handle.to_string(), asset.clone());
            }
        }
    }
    shown
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

/// The web page's link that opens `session` and the file an attachment of
/// it stored, by its `sha256`, on the page at `page_url`
/// (`docs/site/specs/message-attachments.md` section 6).
#[must_use]
pub fn attachment_link(page_url: &str, session: SessionId, sha256: &str) -> String {
    let page = page_url.split('#').next().unwrap_or(page_url);
    let separator = if page.ends_with('/') { "" } else { "/" };
    format!("{page}{separator}#session={session}&attachment={sha256}")
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

/// What the Assets tab lists (section 8.3): whose assets, every project's
/// when `project` is `None`, and of which level, both when `scope` is
/// `None`. Kind and search narrow it further in the page.
#[derive(Clone, Debug, Default, Eq, Hash, PartialEq)]
pub struct AssetFilter {
    pub project: Option<cookie_agent_protocol::AssetProjectFilter>,
    pub scope: Option<cookie_agent_protocol::AssetScope>,
}

impl AssetFilter {
    /// The levels the filter offers, in order.
    pub const LEVELS: [Option<cookie_agent_protocol::AssetScope>; 3] = [
        None,
        Some(cookie_agent_protocol::AssetScope::Session),
        Some(cookie_agent_protocol::AssetScope::Project),
    ];

    /// The `assets.search` that lists it.
    #[must_use]
    pub fn params(&self) -> cookie_agent_protocol::AssetsSearchParams {
        cookie_agent_protocol::AssetsSearchParams {
            project: self.project.clone(),
            scope: self.scope,
        }
    }

    /// How many of its choices narrow the list, for the filter's dot.
    #[must_use]
    pub fn changes(&self) -> usize {
        usize::from(self.project.is_some()) + usize::from(self.scope.is_some())
    }

    /// A level's name in the filter.
    #[must_use]
    pub const fn level_label(scope: Option<cookie_agent_protocol::AssetScope>) -> &'static str {
        match scope {
            None => "All levels",
            Some(cookie_agent_protocol::AssetScope::Session) => "Session",
            Some(cookie_agent_protocol::AssetScope::Project) => "Project",
        }
    }
}

/// Where an asset lives, as its row says: its project's name, or "No
/// project", and for a session tree's own asset the tree's root and its
/// title, when it has one.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AssetPlace {
    pub project: String,
    pub session: Option<(SessionId, Option<String>)>,
}

/// Where `asset` lives: a project store by its project, the shared store
/// as no project, a session store by its tree in `sessions`, the tree's
/// project named by `project_name`.
#[must_use]
pub fn asset_place(
    asset: &cookie_agent_protocol::Asset,
    sessions: &[cookie_agent_protocol::AssetSession],
    project_name: impl Fn(&cookie_agent_protocol::ProjectId) -> Option<String>,
) -> AssetPlace {
    use cookie_agent_protocol::AssetStore;
    let project_label = |project: Option<&cookie_agent_protocol::ProjectId>| {
        project
            .and_then(&project_name)
            .unwrap_or_else(|| "No project".to_owned())
    };
    match &asset.store {
        AssetStore::Project { project } => AssetPlace {
            project: project_label(Some(project)),
            session: None,
        },
        AssetStore::Shared => AssetPlace {
            project: project_label(None),
            session: None,
        },
        AssetStore::Session { session } => {
            let tree = sessions.iter().find(|tree| tree.session == *session);
            AssetPlace {
                project: project_label(tree.and_then(|tree| tree.project.as_ref())),
                session: Some((
                    *session,
                    tree.and_then(|tree| tree.title.as_ref())
                        .map(|title| title.as_str().to_owned()),
                )),
            }
        }
    }
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
    fn the_filter_asks_for_what_it_narrows_to_and_counts_its_changes() {
        use cookie_agent_protocol::{AssetProjectFilter, AssetScope};
        let everything = AssetFilter::default();
        assert_eq!(everything.params(), Default::default());
        assert_eq!(everything.changes(), 0);
        let narrowed = AssetFilter {
            project: Some(AssetProjectFilter::NoProject),
            scope: Some(AssetScope::Session),
        };
        let params = narrowed.params();
        assert_eq!(params.project, Some(AssetProjectFilter::NoProject));
        assert_eq!(params.scope, Some(AssetScope::Session));
        assert_eq!(narrowed.changes(), 2);
    }

    #[test]
    fn a_row_says_the_project_and_the_session_an_asset_lives_in() {
        use cookie_agent_protocol::{AssetSession, ProjectId, SessionTitle};
        let project = ProjectId::new("0192f0c4-7d6e-7a1b-9c2d-3e4f5a6b7c8d").unwrap();
        let named = |id: &ProjectId| (*id == project).then(|| "Grasping".to_owned());
        let mut kept = asset("Report", "report.md", "text/markdown", 1);
        kept.store = AssetStore::Project {
            project: project.clone(),
        };
        assert_eq!(
            asset_place(&kept, &[], named),
            AssetPlace {
                project: "Grasping".into(),
                session: None
            }
        );
        let shared = asset("Report", "report.md", "text/markdown", 1);
        assert_eq!(asset_place(&shared, &[], named).project, "No project");
        let root = SessionId::new_v7();
        let mut own = asset("Plot", "plot.png", "image/png", 1);
        own.store = AssetStore::Session { session: root };
        let trees = [AssetSession {
            session: root,
            title: Some(SessionTitle::new("Loss sweep").unwrap()),
            project: Some(project.clone()),
        }];
        assert_eq!(
            asset_place(&own, &trees, named),
            AssetPlace {
                project: "Grasping".into(),
                session: Some((root, Some("Loss sweep".into())))
            }
        );
        // A tree the search did not name still says it is a session's.
        assert_eq!(asset_place(&own, &[], named).session, Some((root, None)));
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
