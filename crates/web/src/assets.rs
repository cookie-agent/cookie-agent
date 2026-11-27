//! Assets on the page (`docs/site/specs/assets.md` section 8): each asset
//! placeholder (`data-asset`) is filled with the asset as it is now, read
//! with `asset.get` and `asset.read`, in the way its `data-asset-view`
//! names:
//!
//! - `figure`, an `asset` block shown whole: images, SVG, video, and audio
//!   as the media blocks show them, a Markdown document rendered by the
//!   transcript's own renderer and cut to 480 px until *Show all*, an HTML
//!   page in a sandboxed frame of `viewer.html` (section 8.2), which gets
//!   the page by `postMessage` with a token and reports its height back
//!   with it, anything else as a download card;
//! - a reply's cards (`crate::render::attachments`): `thumb` a picture or
//!   a video's first frame, whole when it is alone, `page` a live
//!   thumbnail, `doc` the first lines, `audio` a player, and `file` only
//!   the asset's state;
//! - the side panel's `panel` (the asset whole, the page filling it) and
//!   `source` (its text);
//! - the Assets tab's `tile`: the thumbnail the daemon makes
//!   (`asset.thumbnail`, `docs/site/specs/asset-thumbnails.md`), kept by
//!   [`crate::thumbnail_store`] across page loads, or the kind's icon when
//!   it makes none; the tab never reads a whole file to draw a card.
//!
//! A `doc` card of an asset reads only the start of it.
//!
//! `assets.changed` makes every one shown read its asset again: one whose
//! content changed is filled anew and says when it was updated, one deleted
//! says so.

use std::{cell::RefCell, collections::HashMap};

use cookie_agent_protocol::{Asset, AssetId, AssetStore};
use leptos::{
    prelude::{GetUntracked as _, Set as _, Update as _, WithUntracked as _},
    task::spawn_local,
};
use wasm_bindgen::{JsCast as _, JsValue, closure::Closure};

use crate::{
    blob_cache::{BUDGET_BYTES, BlobCache, url_on_page},
    render::escape,
    workspace::Workspace,
};

/// The most bytes of a Markdown document shown inline.
const MAX_DOCUMENT_BYTES: usize = 1024 * 1024;
/// How much of a document its card shows.
const EXCERPT_CHARS: usize = 1500;
/// How much of a document its card reads: [`EXCERPT_CHARS`] of any text.
const EXCERPT_BYTES: u64 = 8 * 1024;
/// How many bytes of thumbnails stay in memory.
const THUMBNAIL_BUDGET_BYTES: u64 = 16 * 1024 * 1024;
/// An HTML frame's height before its page reports one, and its bounds.
const FRAME_START: f64 = 480.0;
const FRAME_MIN: f64 = 120.0;
/// The share of the window a frame may grow to.
const FRAME_MAX_SHARE: f64 = 0.8;
/// Height changes smaller than this are ignored, so a page sized in `vh`
/// cannot oscillate.
const FRAME_TOLERANCE: f64 = 4.0;
const NOTE: &str = "text-xs text-muted";
const BUTTON: &str =
    "inline-flex min-h-8 items-center gap-1 rounded-md px-2 text-xs hover:bg-hover active:bg-press";

thread_local! {
    /// What was read of each content, by its digest: a `Blob` URL, and the
    /// text of a document or page, kept while it fits the cache.
    static CONTENTS: RefCell<BlobCache<Content>> = RefCell::new(BlobCache::new(BUDGET_BYTES));
    /// The HTML frames shown, by their token.
    static FRAMES: RefCell<HashMap<String, web_sys::Element>> = RefCell::new(HashMap::new());
    static LISTENING: RefCell<bool> = const { RefCell::new(false) };
    /// pdf.js and its worker's code, read once from the page's own files
    /// for the PDF viewer (`docs/site/specs/asset-opening.md` section 6.2).
    static PDF_LIBRARY: RefCell<Option<(web_sys::Blob, web_sys::Blob)>> = const { RefCell::new(None) };
    /// The `Blob` URL of each content's thumbnail, by its digest, kept
    /// while it fits.
    static THUMBNAILS: RefCell<BlobCache<String>> = RefCell::new(BlobCache::new(THUMBNAIL_BUDGET_BYTES));
    /// The contents the daemon made no thumbnail of, by digest.
    static NO_THUMBNAIL: RefCell<std::collections::HashSet<String>> = RefCell::new(std::collections::HashSet::new());
}

/// Where the page keeps pdf.js and its worker's code.
const PDF_LIBRARY_PATH: &str = "pdf.min.mjs";
const PDF_WORKER_PATH: &str = "pdf.worker.min.mjs";
/// The PDF viewer's file, beside the page.
const PDF_VIEWER: &str = "pdf-viewer.html";

#[derive(Clone)]
struct Content {
    url: String,
    /// Where a folder page's relative references resolve: the folder's base
    /// with the main file's own folder, read over HTTP.
    base: Option<String>,
    /// The file's own URL over HTTP, for downloads: a file's, or a
    /// folder's zip.
    download: Option<String>,
    /// The bytes the URL names, to send a PDF to its viewer: the page's
    /// policy does not let it read its own `blob:` URLs back.
    blob: web_sys::Blob,
    text: Option<std::rc::Rc<str>>,
    media_type: String,
}

/// What a placeholder names: an asset, or a file an attachment stored in
/// the session's artifacts; what it was when shown; and how it shows.
#[derive(Clone)]
struct Placeholder {
    source: Source,
    shown_sha: String,
    title: String,
    view: String,
    /// A stored folder's main file and files, from its card.
    folder: Option<cookie_agent_protocol::FolderInfo>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Source {
    /// An asset, shown as it is now.
    Asset { store: AssetStore, asset: AssetId },
    /// A stored file, which never changes.
    Artifact {
        sha256: String,
        media_type: String,
        name: String,
    },
}

fn placeholder(element: &web_sys::Element) -> Option<Placeholder> {
    let source = if element.get_attribute("data-asset")? == "artifact" {
        Source::Artifact {
            sha256: element.get_attribute("data-artifact-sha")?,
            media_type: element.get_attribute("data-asset-type").unwrap_or_default(),
            name: element.get_attribute("data-asset-name").unwrap_or_default(),
        }
    } else {
        Source::Asset {
            store: serde_json::from_str(&element.get_attribute("data-asset-store")?).ok()?,
            asset: element.get_attribute("data-asset")?.parse().ok()?,
        }
    };
    Some(Placeholder {
        source,
        shown_sha: element.get_attribute("data-asset-sha").unwrap_or_default(),
        title: element
            .get_attribute("data-asset-title")
            .unwrap_or_default(),
        view: element
            .get_attribute("data-asset-view")
            .unwrap_or_else(|| "figure".to_owned()),
        folder: element.get_attribute("data-asset-main").map(|main| {
            cookie_agent_protocol::FolderInfo {
                main,
                files: element
                    .get_attribute("data-asset-folder")
                    .and_then(|files| files.parse().ok())
                    .unwrap_or(0),
                bytes: element
                    .get_attribute("data-artifact-bytes")
                    .and_then(|bytes| bytes.parse().ok())
                    .unwrap_or(0),
            }
        }),
    })
}

impl Placeholder {
    /// The stored file as the card code reads an asset: its title, type,
    /// name, and content; it is never replaced.
    fn artifact_asset(&self) -> Option<Asset> {
        let Source::Artifact {
            sha256,
            media_type,
            name,
        } = &self.source
        else {
            return None;
        };
        let stamp = cookie_agent_protocol::AssetStamp {
            at: jiff::Timestamp::now(),
            session: cookie_agent_protocol::SessionId::new_v7(),
            agent: None,
        };
        Some(Asset {
            id: AssetId::new_v7(),
            handle: cookie_agent_protocol::AssetHandle::new("asset_00000000").ok()?,
            store: AssetStore::Shared,
            title: self.title.clone(),
            media_type: media_type.clone(),
            byte_length: 0,
            sha256: sha256.clone(),
            filename: Some(name.clone()).filter(|name| !name.is_empty()),
            created: stamp.clone(),
            updated: stamp,
            folder: self.folder.clone(),
        })
    }
}

/// An asset open in the side panel: which, and what it was when the card
/// that opened it showed it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OpenAsset {
    pub source: Source,
    /// The asset's handle; empty for a stored file.
    pub handle: String,
    pub title: String,
    pub media_type: String,
    pub shown_sha: String,
    /// It is a folder, whose files the side panel lists.
    pub folder: bool,
}

impl OpenAsset {
    /// The asset, or stored file, the placeholder `element` shows.
    pub fn of(element: &web_sys::Element) -> Option<Self> {
        let shown = placeholder(element)?;
        Some(Self {
            source: shown.source,
            handle: element
                .get_attribute("data-asset-handle")
                .unwrap_or_default(),
            title: shown.title,
            media_type: element.get_attribute("data-asset-type").unwrap_or_default(),
            shown_sha: shown.shown_sha,
            folder: element.has_attribute("data-asset-folder"),
        })
    }

    /// Whether it has text to show as its source: a page, a document, or
    /// a text file.
    pub fn has_source(&self) -> bool {
        is_text(&self.media_type)
    }

    /// Whether it shows only as its source: a text file that is not a
    /// page or a document (`docs/site/specs/asset-opening.md` section 5.2).
    pub fn source_only(&self) -> bool {
        is_text(&self.media_type)
            && self.media_type != "text/html"
            && self.media_type != "text/markdown"
    }

    /// The asset `asset` as it is now, to open on its page.
    pub fn of_asset(asset: &Asset) -> Self {
        Self {
            source: Source::Asset {
                store: asset.store.clone(),
                asset: asset.id,
            },
            handle: asset.handle.to_string(),
            title: asset.title.clone(),
            media_type: asset.media_type.clone(),
            shown_sha: asset.sha256.clone(),
            folder: asset.folder.is_some(),
        }
    }

    /// The side panel's placeholder of it: shown whole, its `source`, or a
    /// folder's `files`.
    pub fn placeholder_html(&self, source: bool) -> String {
        self.placeholder_view(if source { "source" } else { "panel" })
    }

    /// The side panel's placeholder showing it as `view`.
    pub fn placeholder_view(&self, view: &str) -> String {
        let named = match &self.source {
            Source::Asset { store, asset } => format!(
                "data-asset=\"{}\" data-asset-store=\"{}\" data-asset-handle=\"{}\"",
                escape(&asset.to_string()),
                escape(&serde_json::to_string(store).unwrap_or_default()),
                escape(&self.handle),
            ),
            Source::Artifact { sha256, name, .. } => format!(
                "data-asset=\"artifact\" data-artifact-sha=\"{}\" data-asset-name=\"{}\"",
                escape(sha256),
                escape(name),
            ),
        };
        format!(
            "<div class=\"flex min-h-full flex-col\" {named} data-asset-sha=\"{}\" data-asset-title=\"{}\" data-asset-type=\"{}\" data-asset-view=\"{view}\"><div class=\"flex min-h-0 flex-1 flex-col\" data-asset-body><span class=\"p-3 {NOTE}\">Loading…</span></div></div>",
            escape(&self.shown_sha),
            escape(&self.title),
            escape(&self.media_type),
        )
    }
}

/// Whether a file of `media_type` is text to show as its source: a page, a
/// document, any `text/*`, JSON, XML, or YAML.
pub fn is_text(media_type: &str) -> bool {
    media_type.starts_with("text/")
        || matches!(
            media_type,
            "application/json" | "application/xml" | "application/yaml" | "application/x-yaml"
        )
        || media_type.ends_with("+json")
        || media_type.ends_with("+xml") && media_type != "image/svg+xml"
}

/// The Assets tab's thumbnail of `asset`: a placeholder the tab fills once
/// its card comes into view, with a Markdown document's first lines, or
/// the thumbnail the daemon makes of anything else; it shows the kind's
/// icon and the file's type until then, and when there is none.
pub fn tab_thumbnail_html(asset: &Asset) -> String {
    let media = asset.media_type.as_str();
    let (view, body) = if media == "text/markdown" {
        (
            "doc",
            "<span class=\"asset-excerpt absolute inset-0 block overflow-hidden bg-surface px-3 pt-2.5 text-xs text-muted\" data-asset-body></span>".to_owned(),
        )
    } else {
        (
            "tile",
            format!(
                "<span class=\"absolute inset-0\" data-asset-body>{}</span>",
                icon_html(asset)
            ),
        )
    };
    format!(
        "<span class=\"absolute inset-0\" data-asset=\"{}\" data-asset-store=\"{}\" data-asset-sha=\"{}\" data-asset-handle=\"{}\" data-asset-title=\"{}\" data-asset-type=\"{}\" data-asset-view=\"{view}\">{body}</span>",
        escape(&asset.id.to_string()),
        escape(&serde_json::to_string(&asset.store).unwrap_or_default()),
        escape(&asset.sha256),
        escape(asset.handle.as_str()),
        escape(&asset.title),
        escape(media),
    )
}

/// A card's icon: the kind's, and the file's type.
fn icon_html(asset: &Asset) -> String {
    let label =
        crate::render::attachments::type_label_of(asset.filename.as_deref(), &asset.media_type);
    let hue = crate::badges::hue_class(Some(cookie_agent_client_state::badges::badge_hue(&label)));
    format!(
        "<span class=\"absolute inset-0 flex flex-col items-center justify-center gap-2 text-muted\" data-asset-icon>{}<span class=\"rounded-md border px-1.5 py-0.5 font-mono text-[0.6875rem] font-bold {hue}\">{}</span></span>",
        crate::icons::svg(
            crate::icons::asset_icon(cookie_agent_client_state::assets::AssetKind::of(
                &asset.media_type
            )),
            "text-3xl"
        ),
        escape(&label),
    )
}

/// The media viewer's file for `asset`, a picture or a video: its URL when
/// its content was read already, else empty, for the viewer to read it
/// when it shows it (`docs/site/specs/asset-opening.md` section 4).
pub fn viewing_of(asset: &Asset) -> crate::media::Viewing {
    let url = CONTENTS
        .with(|contents| contents.borrow_mut().get(&asset.sha256))
        .map(|content| content.url)
        .unwrap_or_default();
    crate::media::Viewing {
        url,
        media_type: asset.media_type.clone(),
        alt: asset.title.clone(),
        handle: Some(asset.handle.to_string()),
        asset: Some((asset.store.clone(), asset.id)),
        stored: None,
        call: None,
    }
}

/// Reads the asset of `viewing`, a file of the media viewer not read yet,
/// and shows it in the viewer's deck, and in the viewer while it still
/// shows it.
pub fn load_viewing(viewing: crate::media::Viewing, workspace: Workspace) {
    if viewing.asset.is_none() && viewing.stored.is_none() {
        return;
    }
    spawn_local(async move {
        let read = async {
            if let Some((sha256, name)) = viewing.stored.clone() {
                // A stored file, from a work row's strip.
                let shown = Placeholder {
                    source: Source::Artifact {
                        sha256,
                        media_type: viewing.media_type.clone(),
                        name,
                    },
                    shown_sha: String::new(),
                    title: viewing.alt.clone(),
                    view: "thumb".into(),
                    folder: None,
                };
                let asset = shown
                    .artifact_asset()
                    .ok_or_else(|| "it names no file".to_owned())?;
                return content(&asset, &shown.source, workspace).await;
            }
            let (store, id) = viewing.asset.clone().ok_or_else(String::new)?;
            let asset = workspace
                .get_asset(store.clone(), id)
                .await?
                .ok_or_else(|| "that asset was deleted".to_owned())?;
            content(&asset, &Source::Asset { store, asset: id }, workspace).await
        }
        .await;
        let showing = workspace
            .media_viewer
            .with_untracked(|shown| shown.as_ref() == Some(&viewing));
        match read {
            Ok(content) => {
                let filled = crate::media::Viewing {
                    url: content.url,
                    media_type: content.media_type,
                    ..viewing.clone()
                };
                workspace.media_group.update(|group| {
                    for shown in group.iter_mut().filter(|shown| **shown == viewing) {
                        *shown = filled.clone();
                    }
                });
                if showing {
                    workspace.media_viewer.set(Some(filled));
                }
            }
            Err(error) => {
                workspace.status.set(error);
                if showing {
                    workspace.media_viewer.set(None);
                }
            }
        }
    });
}

/// The assets with a live link among `shares`.
fn shared_ids(shares: &[cookie_agent_protocol::Share]) -> std::collections::HashSet<String> {
    shares.iter().map(|share| share.asset.to_string()).collect()
}

/// Marks every asset card on the page whose asset has a live link among
/// `shares` with `data-shared`, which keeps its share button shown, and
/// unmarks the rest.
pub fn mark_shared(shares: &[cookie_agent_protocol::Share]) {
    let Some(document) = web_sys::window().and_then(|window| window.document()) else {
        return;
    };
    let shared = shared_ids(shares);
    let Ok(cards) = document.query_selector_all("[data-asset]") else {
        return;
    };
    for index in 0..cards.length() {
        if let Some(card) = cards
            .item(index)
            .and_then(|node| node.dyn_into::<web_sys::Element>().ok())
        {
            mark_card(&card, &shared);
        }
    }
}

fn mark_card(card: &web_sys::Element, shared: &std::collections::HashSet<String>) {
    let is_shared = card
        .get_attribute("data-asset")
        .is_some_and(|asset| shared.contains(&asset));
    if is_shared {
        let _ = card.set_attribute("data-shared", "");
    } else {
        let _ = card.remove_attribute("data-shared");
    }
}

/// Fills every asset placeholder under `root` not filled yet. A card that
/// reads its file whole, a picture's, a video's, or a player's, waits in the
/// transcript until it is within a screen of the view (`data-near`); a
/// file's chip reads nothing.
pub fn show_within(root: &web_sys::Element, workspace: Workspace) {
    listen();
    show_forwards(root, workspace);
    let Ok(blocks) = root.query_selector_all("[data-asset]:not([data-shown]):not([data-near])")
    else {
        return;
    };
    for index in 0..blocks.length() {
        let Some(element) = blocks
            .item(index)
            .and_then(|node| node.dyn_into::<web_sys::Element>().ok())
        else {
            continue;
        };
        let reads_whole = matches!(
            element.get_attribute("data-asset-view").as_deref(),
            Some("thumb" | "audio" | "seen")
        );
        if reads_whole && crate::near_view::watch_card(&element) {
            let _ = element.set_attribute("data-near", "");
            continue;
        }
        start_fill(element, workspace);
    }
}

/// Fills the card `element` [`show_within`] left waiting, now near.
pub fn fill_card(element: &web_sys::Element, workspace: Workspace) {
    let _ = element.remove_attribute("data-near");
    if !element.has_attribute("data-shown") {
        start_fill(element.clone(), workspace);
    }
}

/// Starts filling the placeholder `element`.
fn start_fill(element: web_sys::Element, workspace: Workspace) {
    let Some(shown) = placeholder(&element) else {
        return;
    };
    let _ = element.set_attribute("data-shown", "loading");
    let shared = leptos::prelude::WithUntracked::with_untracked(&workspace.share_list, |list| {
        list.as_ref()
            .and_then(|list| list.as_ref().ok())
            .map(|list| shared_ids(&list.shares))
            .unwrap_or_default()
    });
    mark_card(&element, &shared);
    spawn_local(fill(element, shown, workspace));
}

/// Brings the newest card of the asset `handle` into view and opens it as
/// a click does, as a `#…&asset=` link asks; else its newest figure, shown
/// full screen when it can be. It waits up to 30 s for the session's
/// transcript to show it.
pub fn reveal(handle: String, workspace: Workspace) {
    spawn_local(async move {
        let handle = handle.replace(['"', '\\'], "");
        for _ in 0..120 {
            gloo_timers::future::sleep(std::time::Duration::from_millis(250)).await;
            if let Some(card) = card_of(&handle) {
                card.scroll_into_view();
                open_card(&card, workspace);
                return;
            }
            let Some(figure) = last(&format!(
                "#transcript [data-asset-handle=\"{handle}\"][data-asset-view=\"figure\"][data-shown=\"yes\"]"
            )) else {
                continue;
            };
            figure.scroll_into_view();
            if figure
                .query_selector("[data-action='asset-full']")
                .ok()
                .flatten()
                .is_some()
            {
                full_screen(&figure);
            } else if let Ok(Some(media)) = figure.query_selector(".media-open")
                && let Some(media) = media.dyn_ref::<web_sys::HtmlElement>()
            {
                media.click();
            }
            return;
        }
    });
}

/// The last element on the page `selector` finds.
fn last(selector: &str) -> Option<web_sys::Element> {
    let document = web_sys::window()?.document()?;
    let found = document.query_selector_all(selector).ok()?;
    found
        .item(found.length().checked_sub(1)?)
        .and_then(|node| node.dyn_into::<web_sys::Element>().ok())
}

/// What finds a shelf card once filled, or waiting to come near.
const SETTLED: &str = ":is([data-shown]:not([data-shown=\"loading\"]), [data-near])";

/// The newest shelf card of the asset `handle`, once filled.
fn card_of(handle: &str) -> Option<web_sys::Element> {
    last(&format!(
        "#transcript [data-attach-row] [data-asset][data-asset-handle=\"{}\"]{SETTLED}",
        handle.replace(['"', '\\'], "")
    ))
}

/// Runs a click on an asset's control, `target` the element whose
/// `data-action` it is and `clicked` the element clicked:
///
/// - a figure's: `asset-full` shows it over the page and back,
///   `asset-reload` runs its page again, `asset-show-all` uncuts its
///   document;
/// - a card's: `asset-panel` opens it in the side panel, `asset-deck` opens
///   the media viewer at the card, to step through its gallery's pictures
///   and videos, `asset-download`
///   saves it, `asset-copy` copies its handle;
/// - `asset-chip`, a handle in a reply, opens the newest card of its asset.
pub fn act(
    target: &web_sys::Element,
    clicked: Option<&web_sys::Element>,
    action: &str,
    workspace: Workspace,
) {
    let card = || target.closest("[data-asset]").ok().flatten();
    match action {
        "asset-full" => {
            if let Some(figure) = card() {
                full_screen(&figure);
            }
        }
        "asset-reload" => {
            if let Some(figure) = card()
                && let Ok(Some(frame)) = figure.query_selector("[data-asset-frame]")
            {
                // Loading the viewer again sends it the page again.
                let _ = frame.set_attribute("src", "viewer.html");
            }
        }
        "asset-show-all" => {
            if let Some(figure) = card()
                && let Ok(Some(document)) = figure.query_selector("[data-asset-document]")
            {
                uncut(&document);
            }
            target.remove();
        }
        "asset-panel" => {
            if let Some(card) = card() {
                open_card(&card, workspace);
            }
        }
        "asset-deck" => {
            if let Ok(Some(card)) = clicked.unwrap_or(target).closest("[data-index]") {
                open_deck(&card, workspace);
            }
        }
        "asset-seen" => {
            if let Ok(Some(tile)) = target.closest("[data-seen-tile]") {
                open_seen(&tile, workspace);
            }
        }
        // The `+N` tile opens the first file it stands for.
        "asset-seen-more" => {
            let first = target
                .get_attribute("data-seen-from")
                .and_then(|from| from.parse::<u32>().ok())
                .zip(target.closest("[data-seen-strip]").ok().flatten())
                .and_then(|(from, strip)| {
                    strip
                        .query_selector_all(":scope > [data-seen-tile]")
                        .ok()?
                        .item(from)?
                        .dyn_into::<web_sys::Element>()
                        .ok()
                });
            if let Some(tile) = first {
                open_seen(&tile, workspace);
            }
        }
        "asset-download" => {
            if let Some(card) = card()
                && let Some(shown) = placeholder(&card)
            {
                download_source(workspace, shown.source, shown.title);
            }
        }
        "attachment-allow" | "attachment-deny" => {
            if let Some(key) = attach_key(target) {
                workspace.decide_attachment(key, action == "attachment-allow");
            }
        }
        // Saving asks where first: this session's assets or the project's.
        "attachment-save" => {
            if let Some(key) = target.get_attribute("data-attach-key") {
                target.set_inner_html(&format!(
                    "<span class=\"flex items-center gap-1\"><span role=\"button\" tabindex=\"0\" class=\"{BUTTON}\" data-action=\"attachment-save-session\" data-attach-key=\"{key}\">Save for this session</span><span role=\"button\" tabindex=\"0\" class=\"{BUTTON}\" data-action=\"attachment-save-project\" data-attach-key=\"{key}\">for the project</span></span>",
                    key = escape(&key),
                ));
                let _ = target.set_attribute("data-action", "attachment-saving");
                let _ = target.class_list().remove_1("opacity-0");
            }
        }
        "attachment-save-session" | "attachment-save-project" => {
            if let Some(key) = attach_key(target) {
                let scope = if action == "attachment-save-project" {
                    cookie_agent_protocol::AssetScope::Project
                } else {
                    cookie_agent_protocol::AssetScope::Session
                };
                workspace.save_attachment(key, scope);
                if let Ok(Some(corner)) = target.closest("[data-action='attachment-saving']") {
                    corner.remove();
                }
            }
        }
        "asset-copy" => {
            if let Some(handle) = target.get_attribute("data-asset-handle") {
                copy_handle(handle, workspace);
            }
        }
        "asset-share" => {
            if let Some(card) = card()
                && let Some(Placeholder {
                    source: Source::Asset { store, asset },
                    title,
                    ..
                }) = placeholder(&card)
            {
                // The title the card shows now, which follows a rename.
                let title = card
                    .query_selector("[data-asset-title]")
                    .ok()
                    .flatten()
                    .and_then(|title| title.text_content())
                    .filter(|shown| !shown.trim().is_empty())
                    .unwrap_or(title);
                workspace.open_share_dialog(store, asset, title);
            }
        }
        "attachment-share" => {
            if let Some(key) = attach_key(target) {
                workspace.share_attachment(key);
            }
        }
        "asset-chip" => match target
            .get_attribute("data-asset-handle")
            .and_then(|handle| card_of(&handle))
        {
            Some(card) => open_card(&card, workspace),
            None => workspace.status.set("that asset is not shown here".into()),
        },
        _ => {}
    }
}

/// Opens the card `card` as a click on it does: a picture's or video's in
/// the media viewer, a page or a document in the side panel; any other
/// comes into view. A picture still waiting to come near is brought into
/// view and read first.
fn open_card(card: &web_sys::Element, workspace: Workspace) {
    if card.get_attribute("data-shown").as_deref() == Some("deleted") {
        workspace.status.set("that asset was deleted".into());
        return;
    }
    match card.get_attribute("data-asset-view").as_deref() {
        Some("thumb") if card.get_attribute("data-shown").as_deref() == Some("yes") => {
            open_deck(card, workspace);
        }
        Some("thumb") => {
            let options = web_sys::ScrollIntoViewOptions::new();
            options.set_block(web_sys::ScrollLogicalPosition::Center);
            card.scroll_into_view_with_scroll_into_view_options(&options);
            fill_card(card, workspace);
            let card = card.clone();
            spawn_local(async move {
                for _ in 0..40 {
                    gloo_timers::future::sleep(std::time::Duration::from_millis(250)).await;
                    match card.get_attribute("data-shown").as_deref() {
                        Some("yes") => return open_deck(&card, workspace),
                        Some("loading") => {}
                        _ => return,
                    }
                }
            });
        }
        Some("page" | "doc") => workspace.asset_panel.set(OpenAsset::of(card)),
        _ => {
            let options = web_sys::ScrollIntoViewOptions::new();
            options.set_block(web_sys::ScrollLogicalPosition::Center);
            card.scroll_into_view_with_scroll_into_view_options(&options);
        }
    }
}

/// Opens the media viewer at the picture or video card `card`, to step
/// through every one filled of its message: its row's in the transcript,
/// its gallery's elsewhere, in the order they show.
fn open_deck(card: &web_sys::Element, workspace: Workspace) {
    let scope = card
        .closest(".transcript-item")
        .ok()
        .flatten()
        .or_else(|| card.closest("[data-deck]").ok().flatten())
        .unwrap_or_else(|| card.clone());
    // A card that shows a thumbnail reads its file whole only now.
    let unread = scope
        .query_selector_all("[data-asset][data-index][data-thumbnail-only]")
        .ok()
        .map(|cards| {
            (0..cards.length())
                .filter_map(|at| cards.item(at)?.dyn_into::<web_sys::Element>().ok())
                .filter(|each| {
                    each.get_attribute("data-asset-current").is_some_and(|sha| {
                        CONTENTS.with(|contents| contents.borrow_mut().get(&sha).is_none())
                    })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if unread.is_empty() {
        return show_deck(card, &scope, workspace);
    }
    workspace.status.set("opening…".into());
    let card = card.clone();
    spawn_local(async move {
        for each in &unread {
            let Some(shown) = placeholder(each) else {
                continue;
            };
            let Some(asset) = shown.artifact_asset() else {
                continue;
            };
            if let Err(error) = content(&asset, &shown.source, workspace).await {
                workspace
                    .status
                    .set(format!("the file could not be read: {error}"));
                return;
            }
        }
        workspace.status.set(String::new());
        show_deck(&card, &scope, workspace);
    });
}

/// [`open_deck`] once every card of `scope` was read.
fn show_deck(card: &web_sys::Element, scope: &web_sys::Element, workspace: Workspace) {
    let Ok(cards) = scope.query_selector_all("[data-asset][data-index]") else {
        return;
    };
    let mut group = Vec::new();
    let mut start = None;
    for at in 0..cards.length() {
        let Some(each) = cards
            .item(at)
            .and_then(|node| node.dyn_into::<web_sys::Element>().ok())
        else {
            continue;
        };
        let content = each
            .get_attribute("data-asset-current")
            .filter(|_| each.get_attribute("data-shown").as_deref() == Some("yes"))
            .and_then(|sha| CONTENTS.with(|contents| contents.borrow_mut().get(&sha)));
        let Some(content) = content else {
            continue;
        };
        if each == *card {
            start = Some(group.len());
        }
        group.push(crate::media::Viewing {
            url: content.url,
            media_type: content.media_type,
            alt: each.get_attribute("data-asset-title").unwrap_or_default(),
            handle: each.get_attribute("data-asset-handle"),
            asset: match placeholder(&each).map(|shown| shown.source) {
                Some(Source::Asset { store, asset }) => Some((store, asset)),
                _ => None,
            },
            stored: None,
            call: None,
        });
    }
    let Some(first) = start.and_then(|start| group.get(start).cloned()) else {
        workspace.status.set("the picture is still loading".into());
        return;
    };
    workspace.media_group.set(group);
    workspace.media_viewer.set(Some(first));
}

/// Opens the thumbnail `tile` of a work row's strip
/// (`docs/site/specs/folded-media.md` section 4.1): a PDF beside the chat,
/// a picture or video in the media viewer, which steps through every
/// picture and video of the strip, those the `+N` tile stands for too. One
/// not read whole yet is read as the viewer comes to it.
fn open_seen(tile: &web_sys::Element, workspace: Workspace) {
    if tile.get_attribute("data-asset-type").as_deref() == Some("application/pdf") {
        workspace.asset_panel.set(OpenAsset::of(tile));
        return;
    }
    let Some(strip) = tile.closest("[data-seen-strip]").ok().flatten() else {
        return;
    };
    let block_key = strip.get_attribute("data-block-key").unwrap_or_default();
    let Ok(tiles) = strip.query_selector_all(":scope > [data-seen-tile][data-index]") else {
        return;
    };
    let mut group = Vec::new();
    let mut start = None;
    for at in 0..tiles.length() {
        let Some(each) = tiles
            .item(at)
            .and_then(|node| node.dyn_into::<web_sys::Element>().ok())
        else {
            continue;
        };
        let attribute = |name: &str| each.get_attribute(name).unwrap_or_default();
        let sha256 = attribute("data-artifact-sha");
        let url = CONTENTS
            .with(|contents| contents.borrow_mut().get(&sha256))
            .map(|content| content.url)
            .unwrap_or_default();
        if each == *tile {
            start = Some(group.len());
        }
        group.push(crate::media::Viewing {
            url,
            media_type: attribute("data-asset-type"),
            alt: attribute("data-asset-title"),
            handle: None,
            asset: None,
            stored: Some((sha256, attribute("data-asset-name"))),
            call: Some(crate::media::SeenCall {
                header: attribute("data-seen-header"),
                call_id: attribute("data-seen-call"),
                block_key: block_key.clone(),
                not_shown: each.get_attribute("data-seen-not-shown"),
            }),
        });
    }
    let Some(first) = start.and_then(|start| group.get(start).cloned()) else {
        return;
    };
    workspace.media_group.set(group);
    workspace.media_viewer.set(Some(first));
}

/// The media viewer's *Show call*: closes the viewer, opens the work row
/// `block_key` when it is folded, and brings the row of the call `call_id`
/// into view, flashing it once.
pub fn show_call(workspace: Workspace, block_key: String, call_id: String) {
    workspace.media_viewer.set(None);
    let open = workspace.selected.get_untracked().is_some_and(|session| {
        workspace.expanded.with_untracked(|expanded| {
            expanded
                .get(&session)
                .is_some_and(|expanded| expanded.block(&block_key))
        })
    });
    if !open {
        workspace.toggle_expanded(block_key);
    }
    spawn_local(async move {
        let call_id = call_id.replace(['"', '\\'], "");
        let Some(document) = web_sys::window().and_then(|window| window.document()) else {
            return;
        };
        // The fold draws its rows at the next frames.
        for _ in 0..40 {
            gloo_timers::future::sleep(std::time::Duration::from_millis(50)).await;
            let Ok(Some(row)) =
                document.query_selector(&format!("#transcript [data-tool=\"{call_id}\"]"))
            else {
                continue;
            };
            let options = web_sys::ScrollIntoViewOptions::new();
            options.set_block(web_sys::ScrollLogicalPosition::Center);
            row.scroll_into_view_with_scroll_into_view_options(&options);
            // Again from its start, when it flashed before.
            let _ = row.class_list().remove_1("seen-flash");
            if let Some(row) = row.dyn_ref::<web_sys::HtmlElement>() {
                let _ = row.offset_width();
            }
            let _ = row.class_list().add_1("seen-flash");
            return;
        }
    });
}

/// The narrowest screen whose strips show the `+N` tile; a narrower one,
/// a phone's, scrolls them sideways instead.
const SEEN_FIT_FROM: &str = "(min-width: 640px)";

/// Fits every work row strip under `within` to its one line
/// (`docs/site/specs/folded-media.md` section 4): the thumbnails that do
/// not fit hide, and the `+N` tile says how many; the newest stay in view
/// while the work goes on, the first once it ended. On a phone the strip
/// scrolls sideways, at its newest while the work goes on.
pub fn fit_seen(within: &web_sys::Element) {
    let Ok(strips) = within.query_selector_all("[data-seen-strip]") else {
        return;
    };
    let fits_tile = web_sys::window()
        .and_then(|window| window.match_media(SEEN_FIT_FROM).ok().flatten())
        .is_some_and(|query| query.matches());
    for at in 0..strips.length() {
        if let Some(strip) = strips
            .item(at)
            .and_then(|node| node.dyn_into::<web_sys::HtmlElement>().ok())
        {
            fit_strip(&strip, fits_tile);
        }
    }
}

/// [`fit_seen`] of the one strip `strip`.
fn fit_strip(strip: &web_sys::HtmlElement, fits_tile: bool) {
    /// The strip's gap, and the `+N` tile's room.
    const GAP: i32 = 8;
    const MORE: i32 = 56;
    let Ok(list) = strip.query_selector_all(":scope > [data-seen-tile]") else {
        return;
    };
    let tiles = (0..list.length())
        .filter_map(|at| list.item(at)?.dyn_into::<web_sys::HtmlElement>().ok())
        .collect::<Vec<_>>();
    let Some(more) = strip
        .query_selector(":scope > [data-seen-more]")
        .ok()
        .flatten()
        .and_then(|more| more.dyn_into::<web_sys::HtmlElement>().ok())
    else {
        return;
    };
    for tile in &tiles {
        tile.set_hidden(false);
    }
    more.set_hidden(true);
    let live = strip.has_attribute("data-seen-live");
    let room = strip.client_width();
    if !fits_tile || room <= 0 {
        if live {
            strip.set_scroll_left(strip.scroll_width());
        }
        return;
    }
    let widths = tiles
        .iter()
        .map(|tile| tile.offset_width())
        .collect::<Vec<_>>();
    let all = widths.iter().sum::<i32>() + GAP * (widths.len() as i32 - 1).max(0);
    if all <= room {
        return;
    }
    // As many as fit beside the tile: from the newest back while the work
    // goes on, from the first once it ended.
    let mut used = MORE;
    let mut shown = 0;
    let order: Box<dyn Iterator<Item = &i32>> = if live {
        Box::new(widths.iter().rev())
    } else {
        Box::new(widths.iter())
    };
    for width in order {
        if used + GAP + width > room {
            break;
        }
        used += GAP + width;
        shown += 1;
    }
    let hidden = tiles.len() - shown;
    let hidden_range = if live { 0..hidden } else { shown..tiles.len() };
    for tile in &tiles[hidden_range.clone()] {
        tile.set_hidden(true);
    }
    more.set_text_content(Some(&format!("+{hidden}")));
    let _ = more.set_attribute("data-seen-from", &hidden_range.start.to_string());
    let _ = more.set_attribute(
        "aria-label",
        &format!("{hidden} more; open the first of them"),
    );
    more.set_hidden(false);
}

/// Fills the thumbnail `element` of a work row's strip: the daemon's
/// thumbnail of a video, a PDF, or a picture over 1 MiB, else the picture
/// itself; a PDF without one shows its icon and name, a video its icon.
/// The strip fits again once the picture is drawn, at its own shape.
async fn fill_seen(
    element: &web_sys::Element,
    asset: &Asset,
    source: &Source,
    workspace: Workspace,
) {
    let media_type = asset.media_type.as_str();
    let pdf = media_type == "application/pdf";
    let video = media_type.starts_with("video/");
    let url = if thumbnail_first(element, source) || pdf {
        artifact_thumbnail_url(&asset.sha256, workspace).await
    } else {
        None
    };
    let url = match url {
        Some(url) => Some(url),
        None if pdf => {
            let target = body(element);
            let _ = target.set_attribute(
                "class",
                "flex h-16 w-28 flex-col items-center justify-center gap-1 px-2 text-muted",
            );
            target.set_inner_html(&format!(
                "{}<span class=\"w-full truncate text-center text-[0.6875rem]\">{}</span>",
                crate::icons::svg(icondata::LuFileText, "text-xl"),
                escape(&asset.title)
            ));
            // Its icon and name say it is a PDF.
            if let Ok(Some(badge)) = element.query_selector("[data-seen-pdf]") {
                badge.remove();
            }
            None
        }
        // A video is never read whole for its strip.
        None if video => None,
        None => match content(asset, source, workspace).await {
            Ok(content) => Some(content.url),
            Err(_) => {
                let _ = element.set_attribute("data-shown", "failed");
                return;
            }
        },
    };
    let _ = element.set_attribute("data-asset-current", &asset.sha256);
    let _ = element.set_attribute("data-shown", "yes");
    let Some(url) = url else {
        return;
    };
    let target = body(element);
    let _ = target.set_attribute("class", "block h-16");
    let svg_ground = if media_type == "image/svg+xml" {
        " bg-white"
    } else {
        ""
    };
    target.set_inner_html(&format!(
        "<img src=\"{}\" alt=\"\" draggable=\"false\" class=\"pointer-events-none block h-16 w-auto min-w-8 max-w-32 object-cover{svg_ground}\">",
        escape(&url)
    ));
    // A video's first frame plays.
    if let Ok(Some(play)) = element.query_selector("[data-seen-play]")
        && let Some(play) = play.dyn_ref::<web_sys::HtmlElement>()
    {
        play.set_hidden(false);
    }
    // Drawn, the picture has its width: the strip fits to it.
    if let Ok(Some(image)) = target.query_selector("img")
        && let Ok(image) = image.dyn_into::<web_sys::HtmlImageElement>()
    {
        let _ = wasm_bindgen_futures::JsFuture::from(image.decode()).await;
    }
    if let Some(strip) = element
        .closest("[data-seen-strip]")
        .ok()
        .flatten()
        .and_then(|strip| strip.parent_element())
    {
        fit_seen(&strip);
    }
}

/// The attachment `target` names, `turn:content:ordinal`.
fn attach_key(target: &web_sys::Element) -> Option<(u64, u32, u32)> {
    let key = target.get_attribute("data-attach-key")?;
    let mut parts = key.split(':');
    Some((
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
    ))
}

/// Brings the newest card of the stored file `sha256` into view and opens
/// it as a click does, as a `#…&attachment=` link asks; it waits up to 30 s
/// for the session's transcript to show it.
pub fn reveal_attachment(sha256: String, workspace: Workspace) {
    spawn_local(async move {
        let sha256 = sha256.replace(['"', '\\'], "");
        for _ in 0..120 {
            gloo_timers::future::sleep(std::time::Duration::from_millis(250)).await;
            if let Some(card) = last(&format!(
                "#transcript [data-attach-row] [data-artifact-sha=\"{sha256}\"]{SETTLED}"
            )) {
                card.scroll_into_view();
                open_card(&card, workspace);
                return;
            }
        }
    });
}

/// Copies the handle `handle`, for the user to give an agent; where the
/// browser cannot copy, the status line says it.
pub fn copy_handle(handle: String, workspace: Workspace) {
    spawn_local(async move {
        let copied = crate::browser::copy_text(&handle).await;
        workspace.status.set(if copied {
            format!("copied {handle}")
        } else {
            format!("copying needs a secure page; the handle is {handle}")
        });
    });
}

/// Saves what `source` names as a file: an asset as it is now, or a
/// stored file.
pub fn download_source(workspace: Workspace, source: Source, title: String) {
    if let Source::Artifact { .. } = &source {
        let shown = Placeholder {
            source: source.clone(),
            shown_sha: String::new(),
            title,
            view: "file".into(),
            folder: None,
        };
        if let Some(asset) = shown.artifact_asset() {
            download_from(workspace, asset, source);
        }
        return;
    }
    let Source::Asset { store, asset } = source else {
        return;
    };
    spawn_local(async move {
        match workspace.get_asset(store, asset).await {
            Ok(Some(asset)) => download(workspace, asset),
            Ok(None) => workspace.status.set("that asset was deleted".into()),
            Err(error) => workspace
                .status
                .set(format!("the asset could not be read: {error}")),
        }
    });
}

/// Reads every shown figure's asset again, as `assets.changed` asks.
pub fn refresh(workspace: Workspace) {
    let Some(document) = web_sys::window().and_then(|window| window.document()) else {
        return;
    };
    let Ok(blocks) = document.query_selector_all("[data-asset][data-shown]") else {
        return;
    };
    for index in 0..blocks.length() {
        let Some(element) = blocks
            .item(index)
            .and_then(|node| node.dyn_into::<web_sys::Element>().ok())
        else {
            continue;
        };
        // A stored file never changes.
        let Some(shown) = placeholder(&element) else {
            continue;
        };
        let Source::Asset { store, asset } = shown.source.clone() else {
            continue;
        };
        let current = element
            .get_attribute("data-asset-current")
            .unwrap_or_default();
        spawn_local(async move {
            match workspace.get_asset(store, asset).await {
                Ok(Some(asset)) if asset.sha256 == current => {
                    // The same content: only its title may have changed.
                    set_head(&element, &asset, &shown);
                }
                Ok(_) => {
                    let _ = element.set_attribute("data-shown", "loading");
                    fill(element, shown, workspace).await;
                }
                Err(_) => {}
            }
        });
    }
}

async fn fill(element: web_sys::Element, shown: Placeholder, workspace: Workspace) {
    if shown.view == "tile" {
        if let Source::Asset { store, asset } = &shown.source {
            fill_tile(&element, &shown, store.clone(), *asset, workspace).await;
        }
        return;
    }
    let figure = shown.view == "figure";
    let failed = |element: &web_sys::Element, error: String| {
        body(element).set_text_content(Some(&format!("the asset could not be read: {error}")));
        let _ = element.set_attribute("data-shown", "failed");
    };
    let asset = match &shown.source {
        Source::Artifact { .. } => match shown.artifact_asset() {
            Some(asset) => asset,
            None => return failed(&element, "it names no file".into()),
        },
        Source::Asset { store, asset } => match workspace.get_asset(store.clone(), *asset).await {
            Ok(Some(asset)) => asset,
            Ok(None) if figure => {
                gone(&element, &shown);
                return;
            }
            Ok(None) => {
                card_gone(&element);
                return;
            }
            Err(error) => return failed(&element, error.to_string()),
        },
    };
    if shown.view == "seen" {
        return fill_seen(&element, &asset, &shown.source, workspace).await;
    }
    // A file's chip shows what the asset is, not what it holds; a stored
    // PDF's, its first page.
    if shown.view == "file" {
        let _ = element.set_attribute("data-asset-current", &asset.sha256);
        card_state(&element, &asset, &shown);
        let _ = element.set_attribute("data-shown", "yes");
        if matches!(&shown.source, Source::Artifact { media_type, .. } if media_type == "application/pdf")
            && let Some(url) = artifact_thumbnail_url(&asset.sha256, workspace).await
            && let Ok(Some(badge)) = element.query_selector("[data-file-badge]")
        {
            badge.set_inner_html(&format!(
                "<img src=\"{}\" alt=\"\" draggable=\"false\" class=\"h-full w-full rounded-lg object-cover object-top\" data-artifact-thumbnail>",
                escape(&url)
            ));
        }
        return;
    }
    // A stored picture over 1 MiB, or a video, shows the daemon's thumbnail
    // and is read whole only to be viewed
    // (`docs/site/specs/attachment-handling.md` section 5.1).
    if shown.view == "thumb"
        && thumbnail_first(&element, &shown.source)
        && let Some(url) = artifact_thumbnail_url(&asset.sha256, workspace).await
    {
        let _ = element.set_attribute("data-asset-current", &asset.sha256);
        let _ = element.set_attribute("data-thumbnail-only", "");
        let target = body(&element);
        target.set_inner_html("");
        let _ = target.remove_attribute("data-shown");
        thumb_at(&target, &url, &asset.media_type, &asset.title, true);
        card_state(&element, &asset, &shown);
        let _ = element.set_attribute("data-shown", "yes");
        return;
    }
    if shown.view == "doc"
        && let Source::Asset { store, asset: id } = &shown.source
    {
        let start = match workspace
            .read_asset_start(store.clone(), *id, EXCERPT_BYTES)
            .await
        {
            Ok(Some(start)) => start,
            Ok(None) => return card_gone(&element),
            Err(error) => return failed(&element, error),
        };
        let _ = element.set_attribute("data-asset-current", &asset.sha256);
        let target = body(&element);
        target.set_inner_html("");
        excerpt(&target, text_start(&start));
        card_state(&element, &asset, &shown);
        let _ = element.set_attribute("data-shown", "yes");
        return;
    }
    if shown.view == "files" {
        files(&element, &asset, &shown, workspace).await;
        return;
    }
    let content = match content(&asset, &shown.source, workspace).await {
        Ok(content) => content,
        Err(error) => return failed(&element, error),
    };
    let _ = element.set_attribute("data-asset-current", &asset.sha256);
    let target = body(&element);
    target.set_inner_html("");
    let _ = target.remove_attribute("data-shown");
    match shown.view.as_str() {
        "thumb" => thumb(&target, &content, &asset),
        "page" => match &content.text {
            Some(text) => frame(
                &target,
                text,
                Some(Fixed::Thumbnail),
                content.base.as_deref(),
            ),
            None => target.set_text_content(Some(&asset.title)),
        },
        "doc" => match &content.text {
            Some(text) => excerpt(&target, text),
            None => target.set_text_content(Some("too large to show")),
        },
        "audio" => crate::media::show(&target, &content.url, &content.media_type, &asset.title),
        "panel" => panel(&target, &content, &asset),
        "source" => {
            let text = content
                .text
                .as_deref()
                .unwrap_or("This file has no text to show.");
            target.set_inner_html(&format!(
                "<pre class=\"m-0 p-3 font-mono text-xs whitespace-pre-wrap wrap-anywhere\" data-asset-source>{}</pre>",
                escape(text)
            ));
        }
        _ => {
            set_head(&element, &asset, &shown);
            figure_body(&target, &content, &asset);
        }
    }
    if !figure {
        card_state(&element, &asset, &shown);
    }
    let _ = element.set_attribute("data-shown", "yes");
}

/// A folder's files, each opening in a tab of its own when the folder is
/// served over HTTP (`docs/site/specs/folder-artifacts.md` section 5).
async fn files(
    element: &web_sys::Element,
    asset: &Asset,
    shown: &Placeholder,
    workspace: Workspace,
) {
    let manifest = match &shown.source {
        Source::Asset { store, asset } => workspace.asset_manifest(store.clone(), *asset).await,
        Source::Artifact { sha256, .. } => workspace.artifact_manifest(sha256.clone()).await,
    };
    let target = body(element);
    let manifest = match manifest {
        Ok(manifest) => manifest,
        Err(error) => {
            target.set_text_content(Some(&format!("its files could not be read: {error}")));
            let _ = element.set_attribute("data-shown", "failed");
            return;
        }
    };
    let http = http_content(asset, &shown.source, workspace).await;
    let rows = manifest
        .entries
        .iter()
        .map(|entry| {
            let name = match &http {
                Some(http) => format!(
                    "<a class=\"truncate font-mono text-accent hover:underline\" href=\"{}{}\" target=\"_blank\" rel=\"noopener noreferrer\">{}</a>",
                    escape(&http.url),
                    escape(&encode_path(&entry.path)),
                    escape(&entry.path)
                ),
                None => format!("<span class=\"truncate font-mono\">{}</span>", escape(&entry.path)),
            };
            let main = if entry.path == manifest.main {
                "<span class=\"rounded-full bg-sunken px-1.5 text-[0.625rem] text-muted\">main</span>"
            } else {
                ""
            };
            format!(
                "<li class=\"flex min-w-0 items-center gap-2 py-1\" data-folder-file>{name}{main}<span class=\"ml-auto shrink-0 {NOTE}\">{} · {}</span></li>",
                escape(&cookie_agent_client_state::media_type_word(&entry.media_type)),
                escape(&cookie_agent_protocol::human_bytes(entry.byte_length)),
            )
        })
        .collect::<String>();
    target.set_inner_html(&format!(
        "<ul class=\"m-0 list-none divide-y divide-line p-3 text-xs\" data-folder-files>{rows}</ul>"
    ));
    let _ = element.set_attribute("data-shown", "yes");
}

/// The text `bytes` start with, up to a character they cut.
fn text_start(bytes: &[u8]) -> &str {
    match std::str::from_utf8(bytes) {
        Ok(text) => text,
        Err(error) => std::str::from_utf8(&bytes[..error.valid_up_to()]).unwrap_or_default(),
    }
}

/// A tab card's thumbnail (`docs/site/specs/asset-thumbnails.md`): the
/// daemon's picture, or the icon the card shows already when there is
/// none.
async fn fill_tile(
    element: &web_sys::Element,
    shown: &Placeholder,
    store: AssetStore,
    asset: AssetId,
    workspace: Workspace,
) {
    let (sha256, url) = match known_thumbnail(&shown.shown_sha).await {
        Some(url) => (shown.shown_sha.clone(), url),
        None => {
            let result = match workspace.asset_thumbnail(store, asset).await {
                Ok(Some(result)) => result,
                Ok(None) => return card_gone(element),
                Err(_) => {
                    let _ = element.set_attribute("data-shown", "failed");
                    return;
                }
            };
            let url = match result.thumbnail {
                Some(thumbnail) => daemon_thumbnail(&result.sha256, &thumbnail),
                None => {
                    NO_THUMBNAIL.with(|none| none.borrow_mut().insert(result.sha256.clone()));
                    if !result.retry_later {
                        crate::thumbnail_store::keep_none(result.sha256.clone());
                    }
                    None
                }
            };
            (result.sha256, url)
        }
    };
    let _ = element.set_attribute("data-asset-current", &sha256);
    let Some(url) = url else {
        let _ = element.set_attribute("data-shown", "icon");
        return;
    };
    let media_type = element.get_attribute("data-asset-type").unwrap_or_default();
    let placed = if media_type == "application/pdf" {
        " object-top"
    } else {
        ""
    };
    let play = if media_type.starts_with("video/") {
        format!(
            "<span class=\"pointer-events-none absolute inset-0 flex items-center justify-center\">{}</span>",
            crate::icons::svg(icondata::LuCirclePlay, "text-4xl text-white/90 drop-shadow")
        )
    } else {
        String::new()
    };
    body(element).set_inner_html(&format!(
        "<img src=\"{}\" alt=\"{}\" draggable=\"false\" class=\"pointer-events-none absolute inset-0 h-full w-full object-cover{placed}\" data-asset-thumbnail>{play}",
        escape(&url),
        escape(&shown.title),
    ));
    let _ = element.set_attribute("data-shown", "yes");
}

/// The size from which a stored picture shows the daemon's thumbnail
/// rather than its bytes.
const THUMBNAIL_FROM_BYTES: u64 = 1024 * 1024;

/// Whether the card `element` of a stored file `source` shows the daemon's
/// thumbnail first: a video, or a picture over [`THUMBNAIL_FROM_BYTES`].
fn thumbnail_first(element: &web_sys::Element, source: &Source) -> bool {
    let Source::Artifact { media_type, .. } = source else {
        return false;
    };
    let bytes = element
        .get_attribute("data-artifact-bytes")
        .and_then(|bytes| bytes.parse::<u64>().ok())
        .unwrap_or(0);
    media_type.starts_with("video/")
        || (media_type.starts_with("image/")
            && media_type != "image/svg+xml"
            && bytes > THUMBNAIL_FROM_BYTES)
}

/// The `Blob` URL of the thumbnail of the viewed session's stored file
/// `sha256`, as this page or the browser keeps it, else as
/// `artifact.thumbnail` draws it; `None` when there is none, for the card to
/// read the file instead.
async fn artifact_thumbnail_url(sha256: &str, workspace: Workspace) -> Option<String> {
    if let Some(known) = known_thumbnail(sha256).await {
        return known;
    }
    let result = workspace
        .artifact_thumbnail(sha256.to_owned())
        .await
        .ok()
        .flatten()?;
    match result.thumbnail {
        Some(thumbnail) => daemon_thumbnail(&result.sha256, &thumbnail),
        None => {
            if !result.retry_later {
                NO_THUMBNAIL.with(|none| none.borrow_mut().insert(result.sha256.clone()));
                crate::thumbnail_store::keep_none(result.sha256);
            }
            None
        }
    }
}

/// What this page or the browser knows of the content `sha256`'s
/// thumbnail: `Some(Some(url))` its `Blob` URL, `Some(None)` that it has
/// none, `None` nothing yet.
async fn known_thumbnail(sha256: &str) -> Option<Option<String>> {
    if let Some(url) = THUMBNAILS.with(|thumbnails| thumbnails.borrow_mut().get(sha256)) {
        return Some(Some(url));
    }
    if NO_THUMBNAIL.with(|none| none.borrow().contains(sha256)) {
        return Some(None);
    }
    match crate::thumbnail_store::get(sha256).await? {
        crate::thumbnail_store::Kept::Thumbnail(blob) => {
            let url = web_sys::Url::create_object_url_with_blob(&blob).ok()?;
            Some(Some(remember_thumbnail(sha256, url, &blob)))
        }
        crate::thumbnail_store::Kept::None => {
            NO_THUMBNAIL.with(|none| none.borrow_mut().insert(sha256.to_owned()));
            Some(None)
        }
    }
}

/// The `Blob` URL of the daemon's `thumbnail` of the content `sha256`,
/// kept in this page and in the browser.
fn daemon_thumbnail(
    sha256: &str,
    thumbnail: &cookie_agent_protocol::AssetThumbnail,
) -> Option<String> {
    use base64::Engine as _;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(&thumbnail.data)
        .ok()?;
    let (blob, url) = blob_url(&bytes, &thumbnail.media_type).ok()?;
    crate::thumbnail_store::keep_thumbnail(sha256.to_owned(), blob.clone());
    Some(remember_thumbnail(sha256, url, &blob))
}

/// Keeps `url`, the `Blob` URL of `blob`, the content `sha256`'s
/// thumbnail, in this page while it fits, and gives it back.
fn remember_thumbnail(sha256: &str, url: String, blob: &web_sys::Blob) -> String {
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let bytes = blob.size() as u64;
    let left = THUMBNAILS.with(|thumbnails| {
        thumbnails
            .borrow_mut()
            .insert(sha256.to_owned(), url.clone(), bytes, |url| {
                url_on_page(url)
            })
    });
    for left in left {
        let _ = web_sys::Url::revoke_object_url(&left);
    }
    url
}

/// A figure's body: the asset whole, as its type shows.
fn figure_body(target: &web_sys::Element, content: &Content, asset: &Asset) {
    let media = &content.media_type;
    if media.starts_with("image/") || media.starts_with("video/") || media.starts_with("audio/") {
        crate::media::show(target, &content.url, media, &asset.title);
    } else if media == "text/markdown" {
        match &content.text {
            Some(text) if text.len() <= MAX_DOCUMENT_BYTES => {
                document(target, text);
                resolve_relative(target, content.base.as_deref());
            }
            _ => download_card(target, asset, &content.url, Some("too large to show")),
        }
    } else if media == "text/html" {
        match &content.text {
            Some(text) => frame(target, text, None, content.base.as_deref()),
            None => download_card(target, asset, &content.url, None),
        }
    } else {
        download_card(target, asset, &content.url, None);
    }
}

/// The side panel's body: a page filling it, a whole document, media as
/// the transcript shows it, or a download.
fn panel(target: &web_sys::Element, content: &Content, asset: &Asset) {
    match (content.media_type.as_str(), &content.text) {
        ("text/html", Some(text)) => {
            frame(target, text, Some(Fixed::Filling), content.base.as_deref())
        }
        ("application/pdf", _) => pdf_frame(target, content),
        ("text/markdown", Some(text)) => {
            target.set_inner_html(&format!(
                "<div class=\"markdown mx-auto w-full max-w-3xl p-4\" data-asset-document>{}</div>",
                crate::render::markdown::markdown_html(text)
            ));
            crate::highlight::highlight_within(target);
            resolve_relative(target, content.base.as_deref());
        }
        _ => {
            let _ = target.set_attribute("class", "flex min-h-0 flex-1 flex-col items-center p-3");
            figure_body(target, content, asset);
        }
    }
}

/// A gallery card's picture: the image, or a video's first frame, filling
/// its frame; alone in its gallery, whole at its own shape.
fn thumb(target: &web_sys::Element, content: &Content, asset: &Asset) {
    thumb_at(
        target,
        &content.url,
        &content.media_type,
        &asset.title,
        false,
    );
}

/// [`thumb`] of the picture at `url` of a file of `media_type` titled
/// `title`; `still` draws a video's or a PDF's thumbnail, a picture, with a
/// video's play mark over it.
fn thumb_at(target: &web_sys::Element, url: &str, media_type: &str, title: &str, still: bool) {
    let video = media_type.starts_with("video/");
    let alone = target
        .closest("[data-gallery='1']")
        .ok()
        .flatten()
        .is_some();
    // A user's, a tool's, or a model's files show small.
    let compact = target.closest("[data-compact]").ok().flatten().is_some();
    let size = if alone && compact {
        "block max-h-48 w-auto max-w-full object-contain"
    } else if alone {
        "block max-h-96 w-auto max-w-full object-contain"
    } else if media_type == "image/svg+xml" {
        "h-full w-full object-contain"
    } else {
        "h-full w-full object-cover"
    };
    let svg_ground = if media_type == "image/svg+xml" {
        " bg-white"
    } else {
        ""
    };
    let play = format!(
        "<span class=\"pointer-events-none absolute inset-0 flex items-center justify-center\">{}</span>",
        crate::icons::svg(icondata::LuCirclePlay, "text-4xl text-white/90 drop-shadow")
    );
    let picture = if video && still {
        format!(
            "<img src=\"{}\" alt=\"{}\" draggable=\"false\" class=\"pointer-events-none {size}\" data-artifact-thumbnail>{play}",
            escape(url),
            escape(title)
        )
    } else if video {
        format!(
            "<video preload=\"metadata\" muted playsinline src=\"{}\" class=\"pointer-events-none {size}\"></video><span class=\"pointer-events-none absolute inset-0 flex items-center justify-center\">{}</span>",
            escape(url),
            crate::icons::svg(icondata::LuCirclePlay, "text-4xl text-white/90 drop-shadow")
        )
    } else {
        let marked = if still {
            " data-artifact-thumbnail"
        } else {
            ""
        };
        format!(
            "<img src=\"{}\" alt=\"{}\" draggable=\"false\" class=\"pointer-events-none {size}{svg_ground}\"{marked}>",
            escape(url),
            escape(title)
        )
    };
    target.set_inner_html(&picture);
    let _ = target.set_attribute(
        "class",
        if alone {
            "relative flex justify-center"
        } else {
            "absolute inset-0"
        },
    );
}

/// A document card's first lines.
fn excerpt(target: &web_sys::Element, text: &str) {
    let cut = text
        .char_indices()
        .nth(EXCERPT_CHARS)
        .map_or(text, |(at, _)| &text[..at]);
    target.set_inner_html(&format!(
        "<div class=\"markdown pointer-events-none\">{}</div><div class=\"pointer-events-none absolute inset-x-0 bottom-0 h-8 bg-linear-to-t from-surface\"></div>",
        crate::render::markdown::markdown_html(cut)
    ));
}

/// A card's title as the asset's is now, and a mark when it was replaced
/// since the card showed it.
fn card_state(element: &web_sys::Element, asset: &Asset, shown: &Placeholder) {
    if shown.view != "file"
        && let Ok(Some(title)) = element.query_selector("[data-asset-title]")
    {
        title.set_text_content(Some(&asset.title));
    }
    let _ = element.set_attribute("data-asset-title", &asset.title);
    if let Ok(Some(mark)) = element.query_selector(":scope > [data-asset-updated]") {
        mark.remove();
    }
    if asset.sha256 != shown.shown_sha {
        let _ = element.insert_adjacent_html(
            "beforeend",
            &format!(
                "<span class=\"pointer-events-none absolute top-1.5 left-1.5 z-20 rounded-full bg-accent px-1.5 text-[0.6875rem] text-on-accent\" title=\"updated {} ago\" data-asset-updated>updated</span>",
                escape(&cookie_agent_protocol::memory_age(
                    asset.updated.at,
                    jiff::Timestamp::now()
                ))
            ),
        );
    }
}

/// A card whose asset is gone: it says so and opens nothing.
fn card_gone(element: &web_sys::Element) {
    if element
        .query_selector(":scope > [data-asset-deleted]")
        .ok()
        .flatten()
        .is_none()
    {
        let _ = element.insert_adjacent_html(
            "beforeend",
            "<span class=\"pointer-events-none absolute top-1.5 left-1.5 z-20 rounded-full bg-muted px-1.5 text-[0.6875rem] text-on-accent\" data-asset-deleted>deleted</span>",
        );
    }
    if let Ok(Some(mark)) = element.query_selector(":scope > [data-asset-updated]") {
        mark.remove();
    }
    let _ = element.set_attribute("data-shown", "deleted");
}

/// The figure's body, where the asset shows.
fn body(element: &web_sys::Element) -> web_sys::Element {
    element
        .query_selector("[data-asset-body]")
        .ok()
        .flatten()
        .unwrap_or_else(|| element.clone())
}

/// The figure's head: the asset's title, when it was updated since it was
/// shown, and its buttons.
fn set_head(element: &web_sys::Element, asset: &Asset, shown: &Placeholder) {
    let Some(head) = element.query_selector("[data-asset-head]").ok().flatten() else {
        return;
    };
    let updated = if asset.sha256 != shown.shown_sha {
        format!(
            "<span class=\"{NOTE} italic\" data-asset-updated>updated {} ago</span>",
            escape(&cookie_agent_protocol::memory_age(
                asset.updated.at,
                jiff::Timestamp::now()
            ))
        )
    } else {
        String::new()
    };
    let viewable = asset.media_type == "text/html" || asset.media_type == "text/markdown";
    let full = if viewable {
        format!(
            "<button type=\"button\" data-action=\"asset-full\" class=\"{BUTTON}\" title=\"Full screen\">{}<span class=\"sr-only sm:not-sr-only\">Full screen</span></button>",
            crate::icons::svg(icondata::LuMaximize2, "")
        )
    } else {
        String::new()
    };
    let reload = if asset.media_type == "text/html" {
        format!(
            "<button type=\"button\" data-action=\"asset-reload\" class=\"{BUTTON}\" title=\"Run the page again\">{}<span class=\"sr-only sm:not-sr-only\">Reload</span></button>",
            crate::icons::svg(icondata::LuRedo2, "")
        )
    } else {
        String::new()
    };
    let download = CONTENTS.with(|contents| {
        contents.borrow_mut().get(&asset.sha256).map(|content| {
            format!(
                "<a download=\"{}\" href=\"{}\" class=\"{BUTTON}\" title=\"Download\">{}<span class=\"sr-only sm:not-sr-only\">Download</span></a>",
                escape(&download_name(asset)),
                escape(content.download.as_deref().unwrap_or(&content.url)),
                crate::icons::svg(icondata::LuDownload, "")
            )
        })
    });
    head.set_inner_html(&format!(
        "<span class=\"min-w-0 flex-1 truncate text-sm font-bold\" data-asset-title>{}</span>{updated}<span class=\"flex shrink-0 items-center gap-0.5\">{full}{reload}{}</span>",
        escape(&asset.title),
        download.unwrap_or_default()
    ));
}

/// A figure whose asset is gone.
fn gone(element: &web_sys::Element, shown: &Placeholder) {
    if let Some(head) = element.query_selector("[data-asset-head]").ok().flatten() {
        head.set_inner_html(&format!(
            "<span class=\"min-w-0 flex-1 truncate text-sm font-bold line-through opacity-70\">{}</span>",
            escape(&shown.title)
        ));
    }
    body(element).set_inner_html(&format!(
        "<span class=\"{NOTE} italic\" data-asset-deleted>This asset was deleted.</span>"
    ));
    let _ = element.set_attribute("data-shown", "deleted");
}

fn download_name(asset: &Asset) -> String {
    asset
        .filename
        .clone()
        .unwrap_or_else(|| asset.handle.to_string())
}

fn download_card(target: &web_sys::Element, asset: &Asset, url: &str, note: Option<&str>) {
    let note = note
        .map(|note| format!(" · {}", escape(note)))
        .unwrap_or_default();
    target.set_inner_html(&format!(
        "<a download=\"{}\" href=\"{}\" class=\"flex items-center gap-3 rounded-xl p-3 hover:bg-hover bg-raised shadow-[0_0_0_1px_var(--color-line),0_1px_2px_rgb(0_0_0/0.03)]\" data-asset-download>{}<span class=\"flex min-w-0 flex-col\"><span class=\"truncate text-sm\">{}</span><span class=\"{NOTE}\">{} · {}{note}</span></span></a>",
        escape(&download_name(asset)),
        escape(url),
        crate::icons::svg(icondata::LuFileDown, "text-2xl text-accent"),
        escape(&download_name(asset)),
        escape(&asset.media_type),
        escape(&cookie_agent_protocol::human_bytes(asset.byte_length)),
    ));
}

/// A Markdown document, cut to 480 px until *Show all*; one that fits
/// shows whole, without the button.
fn document(target: &web_sys::Element, text: &str) {
    target.set_inner_html(&format!(
        "<div class=\"asset-document markdown relative max-h-[480px] overflow-hidden\" data-asset-document data-cut=\"true\">{}<div class=\"asset-fade pointer-events-none absolute inset-x-0 bottom-0 h-16 bg-linear-to-t from-surface\"></div></div><button type=\"button\" data-action=\"asset-show-all\" class=\"{BUTTON} mt-1\">Show all</button>",
        crate::render::markdown::markdown_html(text)
    ));
    crate::highlight::highlight_within(target);
    if let Ok(Some(document)) = target.query_selector("[data-asset-document]")
        && document.scroll_height() <= document.client_height()
        && let Ok(Some(button)) = target.query_selector("[data-action='asset-show-all']")
    {
        uncut(&document);
        button.remove();
    }
}

/// Points a rendered document's relative image sources and links at its
/// folder `base`; without one they stay as they are.
fn resolve_relative(target: &web_sys::Element, base: Option<&str>) {
    let Some(base) = base else {
        return;
    };
    let relative = |value: &str| {
        !(value.is_empty()
            || value.starts_with('#')
            || value.starts_with('/')
            || value.contains(':'))
    };
    for (selector, attribute) in [
        ("img[src]", "src"),
        ("a[href]", "href"),
        ("video[src]", "src"),
    ] {
        let Ok(found) = target.query_selector_all(selector) else {
            continue;
        };
        for index in 0..found.length() {
            let Some(element) = found
                .item(index)
                .and_then(|node| node.dyn_into::<web_sys::Element>().ok())
            else {
                continue;
            };
            let Some(value) = element.get_attribute(attribute) else {
                continue;
            };
            if relative(&value) {
                let _ = element.set_attribute(attribute, &format!("{base}{value}"));
                if attribute == "href" {
                    let _ = element.set_attribute("target", "_blank");
                    let _ = element.set_attribute("rel", "noopener noreferrer");
                }
            }
        }
    }
}

/// Shows a cut document whole.
fn uncut(document: &web_sys::Element) {
    let _ = document
        .class_list()
        .remove_2("max-h-[480px]", "overflow-hidden");
    let _ = document.set_attribute("data-cut", "false");
    if let Ok(Some(fade)) = document.query_selector(".asset-fade") {
        fade.remove();
    }
}

/// A frame whose size its page does not set.
#[derive(Clone, Copy)]
enum Fixed {
    /// A card's thumbnail: the page laid out at 960 px wide, scaled to
    /// the card's width, taking no input.
    Thumbnail,
    /// Filling the side panel.
    Filling,
}

/// An HTML page in a frame of `viewer.html`, which gets the page once it
/// loads, with this frame's token.
fn frame(
    target: &web_sys::Element,
    text: &std::rc::Rc<str>,
    fixed: Option<Fixed>,
    base: Option<&str>,
) {
    let Some(document) = target.owner_document() else {
        return;
    };
    let Ok(iframe) = document.create_element("iframe") else {
        return;
    };
    let token = uuid::Uuid::now_v7().simple().to_string();
    let start = format!("height: {FRAME_START}px");
    let thumbnail = thumbnail_style(target);
    let (class, style) = match fixed {
        None => (
            "block w-full rounded border border-line bg-white",
            start.as_str(),
        ),
        Some(Fixed::Thumbnail) => (
            "pointer-events-none block border-0 bg-white",
            thumbnail.as_str(),
        ),
        Some(Fixed::Filling) => (
            "block min-h-0 w-full flex-1 border-0 bg-white",
            "height: 100%",
        ),
    };
    for (name, value) in [
        ("sandbox", "allow-scripts allow-popups"),
        ("referrerpolicy", "no-referrer"),
        ("src", "viewer.html"),
        ("title", "HTML page"),
        ("data-asset-frame", token.as_str()),
        ("class", class),
        ("style", style),
    ] {
        let _ = iframe.set_attribute(name, value);
    }
    if let Some(fixed) = fixed {
        let _ = iframe.set_attribute("data-fixed", "");
        if matches!(fixed, Fixed::Thumbnail) {
            let _ = iframe.set_attribute("tabindex", "-1");
            let _ = iframe.set_attribute("aria-hidden", "true");
            let _ = iframe.set_attribute("loading", "lazy");
        }
    }
    let html = std::rc::Rc::clone(text);
    let sent = token.clone();
    let frame = iframe.clone();
    let base = base.map(str::to_owned);
    let on_load = Closure::<dyn Fn()>::new(move || {
        send_page(&frame, &html, &sent, base.as_deref());
    });
    let _ = iframe.add_event_listener_with_callback("load", on_load.as_ref().unchecked_ref());
    on_load.forget();
    FRAMES.with(|frames| frames.borrow_mut().insert(token, iframe.clone()));
    let _ = target.append_child(&iframe);
}

/// A thumbnail frame's size: the page laid out 960 px wide, scaled to fill
/// `target`, its card's frame (a quarter before the card is laid out).
fn thumbnail_style(target: &web_sys::Element) -> String {
    const LAYOUT_WIDTH: f64 = 960.0;
    let (width, height) = (
        f64::from(target.client_width()),
        f64::from(target.client_height()),
    );
    let scale = if width > 0.0 {
        width / LAYOUT_WIDTH
    } else {
        0.25
    };
    let height = if height > 0.0 { height / scale } else { 528.0 };
    format!(
        "width: {LAYOUT_WIDTH}px; height: {height:.0}px; transform: scale({scale:.4}); transform-origin: 0 0"
    )
}

/// A PDF in a frame of `pdf-viewer.html`, the asset page's reader: every
/// page, with its bar. The frame gets the PDF and pdf.js once it loads,
/// with this frame's token.
fn pdf_frame(target: &web_sys::Element, content: &Content) {
    let Some(document) = target.owner_document() else {
        return;
    };
    let Ok(iframe) = document.create_element("iframe") else {
        return;
    };
    let token = uuid::Uuid::now_v7().simple().to_string();
    for (name, value) in [
        ("sandbox", "allow-scripts allow-popups"),
        ("referrerpolicy", "no-referrer"),
        ("src", PDF_VIEWER),
        ("title", "PDF"),
        ("data-pdf-frame", token.as_str()),
        ("class", "block min-h-0 w-full flex-1 border-0"),
        ("style", "height: 100%"),
    ] {
        let _ = iframe.set_attribute(name, value);
    }
    let pdf = content.blob.clone();
    let sent = token.clone();
    let frame = iframe.clone();
    let on_load = Closure::<dyn Fn()>::new(move || {
        spawn_local(send_pdf(frame.clone(), pdf.clone(), sent.clone()));
    });
    let _ = iframe.add_event_listener_with_callback("load", on_load.as_ref().unchecked_ref());
    on_load.forget();
    FRAMES.with(|frames| frames.borrow_mut().insert(token, iframe.clone()));
    let _ = target.append_child(&iframe);
}

/// A file of the page's own origin as a `Blob`.
async fn fetch_blob(url: &str) -> Result<web_sys::Blob, JsValue> {
    let window = web_sys::window().ok_or_else(|| JsValue::from_str("no window"))?;
    let response: web_sys::Response =
        wasm_bindgen_futures::JsFuture::from(window.fetch_with_str(url))
            .await?
            .dyn_into()?;
    if !response.ok() {
        return Err(JsValue::from_str(&format!("{url}: {}", response.status())));
    }
    wasm_bindgen_futures::JsFuture::from(response.blob()?)
        .await?
        .dyn_into()
}

/// pdf.js and its worker's code, read the first time a PDF is shown.
async fn pdf_library() -> Result<(web_sys::Blob, web_sys::Blob), JsValue> {
    if let Some(library) = PDF_LIBRARY.with(|library| library.borrow().clone()) {
        return Ok(library);
    }
    let library = (
        fetch_blob(PDF_LIBRARY_PATH).await?,
        fetch_blob(PDF_WORKER_PATH).await?,
    );
    PDF_LIBRARY.with(|kept| *kept.borrow_mut() = Some(library.clone()));
    Ok(library)
}

/// Sends the PDF `pdf` and pdf.js to the frame `frame`, with `token`.
async fn send_pdf(frame: web_sys::Element, pdf: web_sys::Blob, token: String) {
    let sent = async {
        let (library, worker) = pdf_library().await?;
        let window = content_window(&frame).ok_or_else(|| JsValue::from_str("no frame"))?;
        let dark = web_sys::window()
            .and_then(|window| window.document())
            .and_then(|document| document.document_element())
            .is_some_and(|root| root.class_list().contains("dark"));
        let message = js_sys::Object::new();
        for (name, value) in [
            ("type", JsValue::from_str("cookie-pdf")),
            ("token", JsValue::from_str(&token)),
            ("dark", JsValue::from_bool(dark)),
            ("pdf", pdf.into()),
            ("library", library.into()),
            ("worker", worker.into()),
        ] {
            js_sys::Reflect::set(&message, &name.into(), &value)?;
        }
        let post: js_sys::Function =
            js_sys::Reflect::get(&window, &"postMessage".into())?.dyn_into()?;
        post.call2(&window, &message, &"*".into())?;
        Ok::<(), JsValue>(())
    }
    .await;
    if sent.is_err() {
        frame.set_outer_html(&format!(
            "<span class=\"p-3 {NOTE}\">the PDF viewer could not be loaded</span>"
        ));
    }
}

fn send_page(frame: &web_sys::Element, html: &str, token: &str, base: Option<&str>) {
    let Some(window) = content_window(frame) else {
        return;
    };
    let message = js_sys::Object::new();
    let _ = js_sys::Reflect::set(&message, &"type".into(), &"cookie-asset-page".into());
    let _ = js_sys::Reflect::set(&message, &"token".into(), &token.into());
    let _ = js_sys::Reflect::set(&message, &"html".into(), &html.into());
    // A folder's page: its relative references resolve to its folder.
    if let Some(base) = base {
        let _ = js_sys::Reflect::set(&message, &"base".into(), &base.into());
    }
    let post = js_sys::Reflect::get(&window, &"postMessage".into())
        .ok()
        .and_then(|post| post.dyn_into::<js_sys::Function>().ok());
    if let Some(post) = post {
        let _ = post.call2(&window, &message, &"*".into());
    }
}

/// A frame's window, as a plain object: cross-origin, so only messages
/// reach it.
fn content_window(frame: &web_sys::Element) -> Option<JsValue> {
    js_sys::Reflect::get(frame, &"contentWindow".into())
        .ok()
        .filter(|window| !window.is_null() && !window.is_undefined())
}

/// Listens, once per page, for the height the frames' pages report. A
/// report counts only from the frame its token names.
fn listen() {
    if LISTENING.with(|listening| std::mem::replace(&mut *listening.borrow_mut(), true)) {
        return;
    }
    let Some(window) = web_sys::window() else {
        return;
    };
    let on_message =
        Closure::<dyn Fn(web_sys::MessageEvent)>::new(|event: web_sys::MessageEvent| {
            let data = event.data();
            let text = |name: &str| {
                js_sys::Reflect::get(&data, &name.into())
                    .ok()
                    .and_then(|value| value.as_string())
            };
            // A page's height.
            if text("type").as_deref() != Some("cookie-asset-height") {
                return;
            }
            let Some(token) = text("token") else {
                return;
            };
            let Some(height) = js_sys::Reflect::get(&data, &"height".into())
                .ok()
                .and_then(|value| value.as_f64())
                .filter(|number| number.is_finite() && *number > 0.0)
            else {
                return;
            };
            let Some(frame) = FRAMES.with(|frames| frames.borrow().get(&token).cloned()) else {
                return;
            };
            let source = event.source().map(JsValue::from);
            let from_frame = match (source, content_window(&frame)) {
                (Some(source), Some(window)) => js_sys::Object::is(&source, &window),
                _ => false,
            };
            // Each frame speaks only of its own kind.
            if !from_frame {
                return;
            }
            if frame.has_attribute("data-asset-frame") {
                size_frame(&frame, height);
            }
        });
    let _ = window.add_event_listener_with_callback("message", on_message.as_ref().unchecked_ref());
    on_message.forget();
}

/// The height a frame takes for content `reported` pixels tall in a window
/// `window_height` tall, or `None` when it stays at `current`.
pub fn frame_height(reported: f64, current: f64, window_height: f64) -> Option<f64> {
    let most = (window_height * FRAME_MAX_SHARE).max(FRAME_MIN);
    let height = reported.clamp(FRAME_MIN, most).round();
    ((height - current).abs() >= FRAME_TOLERANCE).then_some(height)
}

fn size_frame(frame: &web_sys::Element, reported: f64) {
    // A frame shown full screen fills it, and a fixed one keeps its size.
    if frame.has_attribute("data-fixed") || frame.closest("#asset-full").ok().flatten().is_some() {
        return;
    }
    let Some(element) = frame.dyn_ref::<web_sys::HtmlElement>() else {
        return;
    };
    let current = f64::from(element.offset_height());
    let window_height = web_sys::window()
        .and_then(|window| window.inner_height().ok())
        .and_then(|height| height.as_f64())
        .unwrap_or(800.0);
    if let Some(height) = frame_height(reported, current, window_height) {
        let _ = element
            .style()
            .set_property("height", &format!("{height}px"));
    }
}

/// Shows `figure`'s asset over the whole page: an HTML page in a frame of
/// its own, which runs it again, or the whole document. *Close* or Escape
/// ends it.
fn full_screen(figure: &web_sys::Element) {
    let Some(document) = figure.owner_document() else {
        return;
    };
    let Some(sha256) = figure.get_attribute("data-asset-current") else {
        return;
    };
    let Some(content) = CONTENTS.with(|contents| contents.borrow_mut().get(&sha256)) else {
        return;
    };
    let Some(text) = content.text.clone() else {
        return;
    };
    close_full_screen(&document);
    let title = figure
        .query_selector("[data-asset-title]")
        .ok()
        .flatten()
        .and_then(|title| title.text_content())
        .unwrap_or_default();
    let Ok(overlay) = document.create_element("div") else {
        return;
    };
    let _ = overlay.set_attribute("id", "asset-full");
    let _ = overlay.set_attribute("role", "dialog");
    let _ = overlay.set_attribute("aria-label", &title);
    let _ = overlay.set_attribute(
        "class",
        "fixed inset-0 z-50 flex flex-col gap-2 bg-surface p-3 text-muted",
    );
    overlay.set_inner_html(&format!(
        "<div class=\"flex min-w-0 items-center gap-2\"><span class=\"min-w-0 flex-1 truncate text-sm font-bold\">{}</span><button type=\"button\" id=\"asset-full-close\" class=\"{BUTTON}\" title=\"Close (Escape)\">{}<span>Close</span></button></div><div class=\"min-h-0 flex-1 overflow-auto\" data-asset-body></div>",
        escape(&title),
        crate::icons::svg(icondata::LuX, "")
    ));
    let body = body(&overlay);
    if content.media_type == "text/html" {
        frame(&body, &text, None, content.base.as_deref());
        if let Ok(Some(iframe)) = body.query_selector("iframe")
            && let Some(iframe) = iframe.dyn_ref::<web_sys::HtmlElement>()
        {
            let _ = iframe.style().set_property("height", "100%");
        }
    } else {
        body.set_inner_html(&format!(
            "<div class=\"markdown mx-auto max-w-3xl\">{}</div>",
            crate::render::markdown::markdown_html(&text)
        ));
        crate::highlight::highlight_within(&body);
    }
    let close = Closure::<dyn Fn()>::new(move || {
        if let Some(document) = web_sys::window().and_then(|window| window.document()) {
            close_full_screen(&document);
        }
    });
    if let Ok(Some(button)) = overlay.query_selector("#asset-full-close") {
        let _ = button.add_event_listener_with_callback("click", close.as_ref().unchecked_ref());
    }
    close.forget();
    let on_key = Closure::<dyn Fn(web_sys::KeyboardEvent)>::new(|event: web_sys::KeyboardEvent| {
        if event.key() == "Escape"
            && let Some(document) = web_sys::window().and_then(|window| window.document())
        {
            close_full_screen(&document);
        }
    });
    let _ = overlay.add_event_listener_with_callback("keydown", on_key.as_ref().unchecked_ref());
    on_key.forget();
    let _ = overlay.set_attribute("tabindex", "-1");
    if let Some(root) = document.body() {
        let _ = root.append_child(&overlay);
    }
    if let Some(overlay) = overlay.dyn_ref::<web_sys::HtmlElement>() {
        let _ = overlay.focus();
    }
}

fn close_full_screen(document: &web_sys::Document) {
    if let Some(overlay) = document.get_element_by_id("asset-full") {
        if let Ok(Some(frame)) = overlay.query_selector("[data-asset-frame]")
            && let Some(token) = frame.get_attribute("data-asset-frame")
        {
            FRAMES.with(|frames| frames.borrow_mut().remove(&token));
        }
        overlay.remove();
    }
}

/// Reads `asset`'s content whole, once per digest, as a `Blob` URL, with
/// its text when it is a document or a page.
async fn content(asset: &Asset, source: &Source, workspace: Workspace) -> Result<Content, String> {
    if let Some(content) = CONTENTS.with(|contents| contents.borrow_mut().get(&asset.sha256)) {
        return Ok(content);
    }
    let http = http_content(asset, source, workspace).await;
    let shown_type = match source {
        Source::Asset { .. } => asset.media_type.clone(),
        Source::Artifact { media_type, .. } => media_type.clone(),
    };
    // Video and audio play from their URL, which seeks with ranges, rather
    // than read whole first.
    if let Some(http) = &http
        && http.main.is_none()
        && (shown_type.starts_with("video/") || shown_type.starts_with("audio/"))
    {
        return Ok(Content {
            url: http.url.clone(),
            base: None,
            download: Some(http.download(&download_name(asset))),
            blob: web_sys::Blob::new().map_err(|_| "the asset could not be shown".to_owned())?,
            text: None,
            media_type: shown_type,
        });
    }
    let fetched = match &http {
        Some(http) => match fetch_bytes(&http.shown()).await {
            Ok(bytes) => Some(bytes),
            Err(_) => {
                HTTP_FAILED.with(|failed| failed.set(true));
                None
            }
        },
        None => None,
    };
    let http = http.filter(|_| fetched.is_some());
    let (bytes, sha256, media_type) = match (fetched, source) {
        (Some(bytes), _) => (bytes, asset.sha256.clone(), shown_type),
        (None, Source::Asset { store, asset }) => {
            workspace.read_asset_bytes(store.clone(), *asset).await?
        }
        // A stored file's type is what its attachment found it to be.
        (
            None,
            Source::Artifact {
                sha256, media_type, ..
            },
        ) => {
            let (bytes, _) = workspace.read_media(sha256.clone()).await?;
            (bytes, sha256.clone(), media_type.clone())
        }
    };
    // A page's text whatever its size; another text file's up to what a
    // document shows.
    let text = (media_type == "text/html"
        || (is_text(&media_type) && bytes.len() <= MAX_DOCUMENT_BYTES))
        .then(|| String::from_utf8(bytes.clone()).ok())
        .flatten()
        .map(std::rc::Rc::from);
    let (blob, url) =
        blob_url(&bytes, &media_type).map_err(|_| "the asset could not be shown".to_owned())?;
    let content = Content {
        url,
        base: http.as_ref().and_then(HttpContent::base),
        download: http
            .as_ref()
            .map(|http| http.download(&download_name(asset))),
        blob,
        text,
        media_type,
    };
    let left = CONTENTS.with(|contents| {
        contents.borrow_mut().insert(
            sha256,
            content.clone(),
            (bytes.len() + content.text.as_ref().map_or(0, |text| text.len())) as u64,
            |content| url_on_page(&content.url),
        )
    });
    for left in left {
        let _ = web_sys::Url::revoke_object_url(&left.url);
    }
    Ok(content)
}

thread_local! {
    /// Whether HTTP failed on this connection; it is tried again on the
    /// next (`docs/site/specs/folder-artifacts.md` section 6.6).
    static HTTP_FAILED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// The URLs `content.urls` gave on this connection, by content.
    static HTTP_URLS: RefCell<HashMap<String, Option<HttpContent>>> = RefCell::new(HashMap::new());
}

/// Where content is served over HTTP.
#[derive(Clone, Debug)]
struct HttpContent {
    /// A file's URL, or a folder's base (ending in `/`).
    url: String,
    /// A folder's main file.
    main: Option<String>,
}

impl HttpContent {
    /// The URL of the file a card shows: the file, or a folder's main file.
    fn shown(&self) -> String {
        match &self.main {
            Some(main) => format!("{}{}", self.url, encode_path(main)),
            None => self.url.clone(),
        }
    }

    /// Where the shown file's relative references resolve, for a folder.
    fn base(&self) -> Option<String> {
        let main = self.main.as_ref()?;
        let folder = main.rsplit_once('/').map_or("", |(folder, _)| folder);
        Some(if folder.is_empty() {
            self.url.clone()
        } else {
            format!("{}{}/", self.url, encode_path(folder))
        })
    }

    /// The URL that downloads it: a file as an attachment, a folder as a
    /// zip named `name`.
    fn download(&self, name: &str) -> String {
        match &self.main {
            Some(_) => format!(
                "{}.zip?name={}",
                self.url.trim_end_matches('/'),
                js_sys::encode_uri_component(name)
            ),
            None => format!("{}?download", self.url),
        }
    }
}

/// `path` with each segment escaped for a URL.
fn encode_path(path: &str) -> String {
    path.split('/')
        .map(|part| String::from(js_sys::encode_uri_component(part)))
        .collect::<Vec<_>>()
        .join("/")
}

/// A store as the content route names it: `s_<root>`, `p_<project>`, or
/// `shared`.
fn store_label(store: &AssetStore) -> String {
    match store {
        AssetStore::Session { session } => format!("s_{session}"),
        AssetStore::Project { project } => format!("p_{}", project.as_str()),
        AssetStore::Shared => "shared".to_owned(),
    }
}

/// Points the web forwards' frames and links below `root` at the daemon's
/// address: the page's own for the user, a guest's share for a guest.
fn show_forwards(root: &web_sys::Element, workspace: Workspace) {
    let Ok(found) = root.query_selector_all("[data-forward-src]:not([data-forward-shown])") else {
        return;
    };
    let base = workspace.share_page_base.get_untracked();
    for index in 0..found.length() {
        let Some(element) = found
            .item(index)
            .and_then(|node| node.dyn_into::<web_sys::Element>().ok())
        else {
            continue;
        };
        let Some(path) = element.get_attribute("data-forward-src") else {
            continue;
        };
        let url = format!("{base}{path}");
        let member = if element.tag_name().eq_ignore_ascii_case("a") {
            "href"
        } else {
            "src"
        };
        let _ = element.set_attribute(member, &url);
        let _ = element.set_attribute("data-forward-shown", "");
    }
}

/// Forgets what HTTP did on the last connection: a new one tries it again.
pub fn reset_http() {
    HTTP_FAILED.with(|failed| failed.set(false));
    HTTP_URLS.with(|urls| urls.borrow_mut().clear());
}

/// Where `asset`, read from `source`, is served over HTTP; `None` once HTTP
/// failed on this connection, for a guest, or when the daemon has no URL.
async fn http_content(asset: &Asset, source: &Source, workspace: Workspace) -> Option<HttpContent> {
    if HTTP_FAILED.with(std::cell::Cell::get) {
        return None;
    }
    let key = format!("{source:?}:{}", asset.sha256);
    if let Some(known) = HTTP_URLS.with(|urls| urls.borrow().get(&key).cloned()) {
        return known;
    }
    let base = workspace.share_page_base.get_untracked();
    if base.is_empty() {
        return None;
    }
    // A guest reaches the daemon below its share, which serves the shared
    // session's content without a key (`docs/site/specs/folder-artifacts.md`
    // section 6.5).
    if let Some(guest) = workspace.guest.get_untracked() {
        let label = match source {
            Source::Asset { store, .. } => store_label(store),
            Source::Artifact { .. } => format!("a_{}", guest.session_id),
        };
        let main = asset.folder.as_ref().map(|folder| folder.main.clone());
        let name = asset.filename.clone().unwrap_or_else(|| "file".to_owned());
        let url = match &main {
            Some(_) => format!("{base}f/{label}/{}/", asset.sha256),
            None => format!("{base}f/{label}/{}/{}", asset.sha256, encode_path(&name)),
        };
        return Some(HttpContent { url, main });
    }
    let target = match source {
        Source::Asset { store, asset } => cookie_agent_protocol::ContentTarget::Asset {
            store: store.clone(),
            asset: *asset,
        },
        Source::Artifact { sha256, name, .. } => cookie_agent_protocol::ContentTarget::Artifact {
            session_id: workspace.selected.get_untracked()?,
            sha256: sha256.clone(),
            name: Some(name.clone()).filter(|name| !name.is_empty()),
        },
    };
    let found = workspace
        .content_urls(vec![target])
        .await
        .ok()?
        .into_iter()
        .next()
        .flatten()
        // An asset written since names other content: read it as it is.
        .filter(|url| url.sha256 == asset.sha256)
        .map(|url| HttpContent {
            url: format!("{base}{}", url.path),
            main: url.main,
        });
    HTTP_URLS.with(|urls| urls.borrow_mut().insert(key, found.clone()));
    found
}

/// The bytes at `url`, read over HTTP.
async fn fetch_bytes(url: &str) -> Result<Vec<u8>, JsValue> {
    let blob = fetch_blob(url).await?;
    let buffer = wasm_bindgen_futures::JsFuture::from(blob.array_buffer()).await?;
    Ok(js_sys::Uint8Array::new(&buffer).to_vec())
}

/// A `Blob` of `bytes` and its URL.
fn blob_url(bytes: &[u8], media_type: &str) -> Result<(web_sys::Blob, String), JsValue> {
    let array = js_sys::Uint8Array::from(bytes);
    let parts = js_sys::Array::of1(&array);
    let options = web_sys::BlobPropertyBag::new();
    // An HTML page's blob is a download only, never a page of this origin.
    options.set_type(if media_type == "text/html" {
        "application/octet-stream"
    } else {
        media_type
    });
    let blob = web_sys::Blob::new_with_u8_array_sequence_and_options(&parts, &options)?;
    let url = web_sys::Url::create_object_url_with_blob(&blob)?;
    Ok((blob, url))
}

/// Saves `asset`'s current content as a file: read, then handed to the
/// browser's download.
pub fn download(workspace: Workspace, asset: Asset) {
    let source = Source::Asset {
        store: asset.store.clone(),
        asset: asset.id,
    };
    download_from(workspace, asset, source);
}

/// Saves `asset`, read from `source`, as a file.
fn download_from(workspace: Workspace, asset: Asset, source: Source) {
    spawn_local(async move {
        let url = match CONTENTS.with(|contents| contents.borrow_mut().get(&asset.sha256)) {
            Some(content) => content.download.unwrap_or(content.url),
            None => match content(&asset, &source, workspace).await {
                Ok(content) => content.download.unwrap_or(content.url),
                Err(error) => {
                    workspace
                        .status
                        .set(format!("{} could not be read: {error}", asset.title));
                    return;
                }
            },
        };
        let Some(document) = web_sys::window().and_then(|window| window.document()) else {
            return;
        };
        let Ok(link) = document.create_element("a") else {
            return;
        };
        let _ = link.set_attribute("href", &url);
        let _ = link.set_attribute("download", &download_name(&asset));
        if let Some(link) = link.dyn_ref::<web_sys::HtmlElement>() {
            link.click();
        }
    });
}

#[cfg(test)]
mod tests {
    use super::frame_height;

    #[test]
    fn a_frame_follows_its_page_within_bounds_and_ignores_jitter() {
        // Within bounds it takes the reported height.
        assert_eq!(frame_height(600.0, 480.0, 1000.0), Some(600.0));
        // Never under the minimum, never over 80% of the window.
        assert_eq!(frame_height(10.0, 480.0, 1000.0), Some(120.0));
        assert_eq!(frame_height(5000.0, 480.0, 1000.0), Some(800.0));
        // A change of a few pixels is jitter.
        assert_eq!(frame_height(482.0, 480.0, 1000.0), None);
        assert_eq!(frame_height(476.0, 480.0, 1000.0), Some(476.0));
        // A tiny window still lets the frame be its minimum.
        assert_eq!(frame_height(500.0, 480.0, 100.0), Some(120.0));
    }
}
