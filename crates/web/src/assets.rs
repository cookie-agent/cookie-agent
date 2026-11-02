//! Assets shown as figures (`docs/site/specs/assets.md` section 8): each
//! `asset` block of a transcript is a placeholder this fills with the asset
//! as it is now, read with `asset.get` and `asset.read`:
//!
//! - images, SVG, video, and audio as the media blocks show them, opening
//!   in the media viewer;
//! - a Markdown document rendered by the transcript's own renderer, cut to
//!   480 px until *Show all*;
//! - an HTML page in a sandboxed frame of `viewer.html` (section 8.2), which
//!   gets the page by `postMessage` with a token and reports its height
//!   back with it;
//! - anything else as a download card.
//!
//! `assets.changed` makes every shown figure read its asset again: one whose
//! content changed is filled anew and says when it was updated, one deleted
//! says so.

use std::{cell::RefCell, collections::HashMap};

use cookie_agent_protocol::{Asset, AssetId, AssetStore};
use leptos::task::spawn_local;
use wasm_bindgen::{JsCast as _, JsValue, closure::Closure};

use crate::{render::escape, workspace::Workspace};

/// The most bytes of a Markdown document shown inline.
const MAX_DOCUMENT_BYTES: usize = 1024 * 1024;
/// An HTML frame's height before its page reports one, and its bounds.
const FRAME_START: f64 = 480.0;
const FRAME_MIN: f64 = 120.0;
/// The share of the window a frame may grow to.
const FRAME_MAX_SHARE: f64 = 0.8;
/// Height changes smaller than this are ignored, so a page sized in `vh`
/// cannot oscillate.
const FRAME_TOLERANCE: f64 = 4.0;
const NOTE: &str = "text-xs text-ink dark:text-sand";
const BUTTON: &str = "inline-flex min-h-8 items-center gap-1 rounded-md px-2 text-xs hover:bg-toasted active:bg-crust dark:hover:bg-ember dark:active:bg-smoke";

thread_local! {
    /// What was read of each content, by its digest: a `Blob` URL, and the
    /// text of a document or page, kept for the page's life.
    static CONTENTS: RefCell<HashMap<String, Content>> = RefCell::new(HashMap::new());
    /// The HTML frames shown, by their token.
    static FRAMES: RefCell<HashMap<String, web_sys::Element>> = RefCell::new(HashMap::new());
    static LISTENING: RefCell<bool> = const { RefCell::new(false) };
}

#[derive(Clone)]
struct Content {
    url: String,
    text: Option<std::rc::Rc<str>>,
    media_type: String,
}

/// What a placeholder names: the asset, and what it was when shown.
struct Placeholder {
    store: AssetStore,
    asset: AssetId,
    shown_sha: String,
    title: String,
}

fn placeholder(element: &web_sys::Element) -> Option<Placeholder> {
    let store = serde_json::from_str(&element.get_attribute("data-asset-store")?).ok()?;
    Some(Placeholder {
        store,
        asset: element.get_attribute("data-asset")?.parse().ok()?,
        shown_sha: element.get_attribute("data-asset-sha").unwrap_or_default(),
        title: element
            .get_attribute("data-asset-title")
            .unwrap_or_default(),
    })
}

/// Fills every asset placeholder under `root` not filled yet.
pub fn show_within(root: &web_sys::Element, workspace: Workspace) {
    listen();
    let Ok(blocks) = root.query_selector_all("[data-asset]:not([data-shown])") else {
        return;
    };
    for index in 0..blocks.length() {
        let Some(element) = blocks
            .item(index)
            .and_then(|node| node.dyn_into::<web_sys::Element>().ok())
        else {
            continue;
        };
        let Some(shown) = placeholder(&element) else {
            continue;
        };
        let _ = element.set_attribute("data-shown", "loading");
        spawn_local(fill(element, shown, workspace));
    }
}

/// Brings the newest figure of the asset `handle` into view, and shows it
/// full screen when it can be, as a `#…&asset=` link asks; it waits up to
/// 30 s for the session's transcript to show it.
pub fn reveal(handle: String) {
    spawn_local(async move {
        for _ in 0..120 {
            gloo_timers::future::sleep(std::time::Duration::from_millis(250)).await;
            let Some(document) = web_sys::window().and_then(|window| window.document()) else {
                return;
            };
            let selector = format!(
                "#transcript [data-asset-handle=\"{}\"][data-shown=\"yes\"]",
                handle.replace(['"', '\\'], "")
            );
            let Ok(figures) = document.query_selector_all(&selector) else {
                return;
            };
            let Some(figure) = figures
                .item(figures.length().saturating_sub(1))
                .and_then(|node| node.dyn_into::<web_sys::Element>().ok())
            else {
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
        let Some(shown) = placeholder(&element) else {
            continue;
        };
        let current = element
            .get_attribute("data-asset-current")
            .unwrap_or_default();
        spawn_local(async move {
            match workspace.get_asset(shown.store.clone(), shown.asset).await {
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
    let asset = match workspace.get_asset(shown.store.clone(), shown.asset).await {
        Ok(Some(asset)) => asset,
        Ok(None) => {
            gone(&element, &shown);
            return;
        }
        Err(error) => {
            body(&element).set_text_content(Some(&format!("the asset could not be read: {error}")));
            let _ = element.set_attribute("data-shown", "failed");
            return;
        }
    };
    let content = match content(&asset, workspace).await {
        Ok(content) => content,
        Err(error) => {
            body(&element).set_text_content(Some(&format!("the asset could not be read: {error}")));
            let _ = element.set_attribute("data-shown", "failed");
            return;
        }
    };
    let _ = element.set_attribute("data-asset-current", &asset.sha256);
    set_head(&element, &asset, &shown);
    let target = body(&element);
    target.set_inner_html("");
    let _ = target.remove_attribute("data-shown");
    let media = &content.media_type;
    if media.starts_with("image/") || media.starts_with("video/") || media.starts_with("audio/") {
        crate::media::show(&target, &content.url, media, &asset.title);
    } else if media == "text/markdown" {
        match &content.text {
            Some(text) if text.len() <= MAX_DOCUMENT_BYTES => document(&target, text),
            _ => download_card(&target, &asset, &content.url, Some("too large to show")),
        }
    } else if media == "text/html" {
        match &content.text {
            Some(text) => frame(&target, text),
            None => download_card(&target, &asset, &content.url, None),
        }
    } else {
        download_card(&target, &asset, &content.url, None);
    }
    let _ = element.set_attribute("data-shown", "yes");
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
            crate::icons::svg(icondata::FaExpandSolid, "")
        )
    } else {
        String::new()
    };
    let reload = if asset.media_type == "text/html" {
        format!(
            "<button type=\"button\" data-action=\"asset-reload\" class=\"{BUTTON}\" title=\"Run the page again\">{}<span class=\"sr-only sm:not-sr-only\">Reload</span></button>",
            crate::icons::svg(icondata::FaRotateRightSolid, "")
        )
    } else {
        String::new()
    };
    let download = CONTENTS.with(|contents| {
        contents.borrow().get(&asset.sha256).map(|content| {
            format!(
                "<a download=\"{}\" href=\"{}\" class=\"{BUTTON}\" title=\"Download\">{}<span class=\"sr-only sm:not-sr-only\">Download</span></a>",
                escape(&download_name(asset)),
                escape(&content.url),
                crate::icons::svg(icondata::FaDownloadSolid, "")
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
        "<a download=\"{}\" href=\"{}\" class=\"flex items-center gap-3 rounded-md border border-toasted p-3 hover:bg-toasted dark:border-ember dark:hover:bg-ember\" data-asset-download>{}<span class=\"flex min-w-0 flex-col\"><span class=\"truncate text-sm\">{}</span><span class=\"{NOTE}\">{} · {}{note}</span></span></a>",
        escape(&download_name(asset)),
        escape(url),
        crate::icons::svg(icondata::FaFileArrowDownSolid, "text-2xl text-caramel dark:text-maple"),
        escape(&download_name(asset)),
        escape(&asset.media_type),
        escape(&cookie_agent_protocol::human_bytes(asset.byte_length)),
    ));
}

/// A Markdown document, cut to 480 px until *Show all*; one that fits
/// shows whole, without the button.
fn document(target: &web_sys::Element, text: &str) {
    target.set_inner_html(&format!(
        "<div class=\"asset-document markdown relative max-h-[480px] overflow-hidden\" data-asset-document data-cut=\"true\">{}<div class=\"asset-fade pointer-events-none absolute inset-x-0 bottom-0 h-16 bg-linear-to-t from-cream dark:from-night\"></div></div><button type=\"button\" data-action=\"asset-show-all\" class=\"{BUTTON} mt-1\">Show all</button>",
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

/// An HTML page in a frame of `viewer.html`, which gets the page once it
/// loads, with this frame's token.
fn frame(target: &web_sys::Element, text: &std::rc::Rc<str>) {
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
        ("src", "viewer.html"),
        ("title", "HTML page"),
        ("data-asset-frame", token.as_str()),
        (
            "class",
            "block w-full rounded border border-toasted bg-white dark:border-ember",
        ),
        ("style", &format!("height: {FRAME_START}px")),
    ] {
        let _ = iframe.set_attribute(name, value);
    }
    let html = std::rc::Rc::clone(text);
    let sent = token.clone();
    let frame = iframe.clone();
    let on_load = Closure::<dyn Fn()>::new(move || {
        send_page(&frame, &html, &sent);
    });
    let _ = iframe.add_event_listener_with_callback("load", on_load.as_ref().unchecked_ref());
    on_load.forget();
    FRAMES.with(|frames| frames.borrow_mut().insert(token, iframe.clone()));
    let _ = target.append_child(&iframe);
}

fn send_page(frame: &web_sys::Element, html: &str, token: &str) {
    let Some(window) = content_window(frame) else {
        return;
    };
    let message = js_sys::Object::new();
    let _ = js_sys::Reflect::set(&message, &"type".into(), &"cookie-asset-page".into());
    let _ = js_sys::Reflect::set(&message, &"token".into(), &token.into());
    let _ = js_sys::Reflect::set(&message, &"html".into(), &html.into());
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
            if text("type").as_deref() != Some("cookie-asset-height") {
                return;
            }
            let Some(token) = text("token") else {
                return;
            };
            let Some(height) = js_sys::Reflect::get(&data, &"height".into())
                .ok()
                .and_then(|value| value.as_f64())
                .filter(|height| height.is_finite() && *height > 0.0)
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
            if from_frame {
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
    // A frame shown full screen fills it.
    if frame.closest("#asset-full").ok().flatten().is_some() {
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

/// Runs a click on a figure's button: `asset-full` shows the figure over
/// the page and back, `asset-reload` runs its page again, `asset-show-all`
/// uncuts its document.
pub fn act(target: &web_sys::Element, action: &str) {
    let Some(figure) = target.closest("[data-asset]").ok().flatten() else {
        return;
    };
    match action {
        "asset-full" => full_screen(&figure),
        "asset-reload" => {
            if let Ok(Some(frame)) = figure.query_selector("[data-asset-frame]") {
                // Loading the viewer again sends it the page again.
                let _ = frame.set_attribute("src", "viewer.html");
            }
        }
        "asset-show-all" => {
            if let Ok(Some(document)) = figure.query_selector("[data-asset-document]") {
                uncut(&document);
            }
            target.remove();
        }
        _ => {}
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
    let Some(content) = CONTENTS.with(|contents| contents.borrow().get(&sha256).cloned()) else {
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
        "fixed inset-0 z-50 flex flex-col gap-2 bg-cream p-3 text-ink dark:bg-night dark:text-sand",
    );
    overlay.set_inner_html(&format!(
        "<div class=\"flex min-w-0 items-center gap-2\"><span class=\"min-w-0 flex-1 truncate text-sm font-bold\">{}</span><button type=\"button\" id=\"asset-full-close\" class=\"{BUTTON}\" title=\"Close (Escape)\">{}<span>Close</span></button></div><div class=\"min-h-0 flex-1 overflow-auto\" data-asset-body></div>",
        escape(&title),
        crate::icons::svg(icondata::FaXmarkSolid, "")
    ));
    let body = body(&overlay);
    if content.media_type == "text/html" {
        frame(&body, &text);
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
async fn content(asset: &Asset, workspace: Workspace) -> Result<Content, String> {
    if let Some(content) = CONTENTS.with(|contents| contents.borrow().get(&asset.sha256).cloned()) {
        return Ok(content);
    }
    let (bytes, sha256, media_type) = workspace
        .read_asset_bytes(asset.store.clone(), asset.id)
        .await?;
    let text = (media_type == "text/markdown" || media_type == "text/html")
        .then(|| String::from_utf8(bytes.clone()).ok())
        .flatten()
        .map(std::rc::Rc::from);
    let url =
        blob_url(&bytes, &media_type).map_err(|_| "the asset could not be shown".to_owned())?;
    let content = Content {
        url,
        text,
        media_type,
    };
    CONTENTS.with(|contents| contents.borrow_mut().insert(sha256, content.clone()));
    Ok(content)
}

fn blob_url(bytes: &[u8], media_type: &str) -> Result<String, JsValue> {
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
    web_sys::Url::create_object_url_with_blob(&blob)
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
