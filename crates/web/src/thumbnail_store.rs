//! The thumbnails the Assets tab has shown, kept across page loads in the
//! browser's Cache Storage by the digest of the content they show
//! (`docs/site/specs/asset-thumbnails.md` section 5). A content's thumbnail
//! never changes, so the tab asks the daemon only about content it has not
//! seen. Content that never has a thumbnail is kept too, as an empty entry;
//! content whose thumbnail may come later (`retry_later`) is not.
//!
//! Cache Storage exists only in a secure context, HTTPS or `localhost`;
//! elsewhere nothing is kept across loads. The page's service worker still
//! caches nothing: this cache holds thumbnails only, and the page reads it.

use std::cell::{Cell, RefCell};

use leptos::task::spawn_local;
use wasm_bindgen::{JsCast as _, JsValue};
use wasm_bindgen_futures::JsFuture;

/// The cache's name. A new layout of its entries takes a new name.
const CACHE_NAME: &str = "cookie-asset-thumbnails-1";
/// The most entries kept: past it, the oldest tenth goes.
const MAX_ENTRIES: u32 = 1000;
/// The type of an entry that says its content has no thumbnail.
const NONE_TYPE: &str = "application/x-cookie-no-thumbnail";

/// What the browser keeps of a content.
pub enum Kept {
    Thumbnail(web_sys::Blob),
    /// It never has a thumbnail.
    None,
}

thread_local! {
    /// The cache once opened; `None` before, or when the browser has none.
    static CACHE: RefCell<Option<web_sys::Cache>> = const { RefCell::new(None) };
    /// The browser has no Cache Storage here, or it failed.
    static UNAVAILABLE: Cell<bool> = const { Cell::new(false) };
    /// How many entries the cache holds, as far as this page knows.
    static ENTRIES: Cell<u32> = const { Cell::new(0) };
}

/// The request an entry is kept under.
fn key(sha256: &str) -> String {
    format!("cookie-asset-thumbnail/{sha256}")
}

/// The cache, opened and trimmed the first time.
async fn cache() -> Option<web_sys::Cache> {
    if let Some(cache) = CACHE.with(|cache| cache.borrow().clone()) {
        return Some(cache);
    }
    if UNAVAILABLE.with(Cell::get) {
        return None;
    }
    let opened = async {
        let storage = web_sys::window()
            .ok_or_else(|| JsValue::from_str("no window"))?
            .caches()?;
        let cache: web_sys::Cache = JsFuture::from(storage.open(CACHE_NAME)).await?.dyn_into()?;
        Ok::<_, JsValue>(cache)
    }
    .await;
    let Ok(cache) = opened else {
        UNAVAILABLE.with(|unavailable| unavailable.set(true));
        return None;
    };
    // Another read may have opened it meanwhile; either serves.
    if CACHE.with(|kept| kept.borrow().is_none()) {
        CACHE.with(|kept| *kept.borrow_mut() = Some(cache.clone()));
        trim(&cache).await;
    }
    Some(cache)
}

/// Deletes the oldest entries once there are more than [`MAX_ENTRIES`],
/// down to nine tenths of it, and counts what is left.
async fn trim(cache: &web_sys::Cache) {
    let Ok(keys) = JsFuture::from(cache.keys()).await else {
        return;
    };
    let keys = js_sys::Array::from(&keys);
    let count = keys.length();
    let keep = MAX_ENTRIES / 10 * 9;
    let mut left = count;
    if count > MAX_ENTRIES {
        // Kept in the order they were put.
        for request in keys.iter().take((count - keep) as usize) {
            let Some(url) = js_sys::Reflect::get(&request, &"url".into())
                .ok()
                .and_then(|url| url.as_string())
            else {
                continue;
            };
            if JsFuture::from(cache.delete_with_str(&url)).await.is_ok() {
                left -= 1;
            }
        }
    }
    ENTRIES.with(|entries| entries.set(left));
}

/// What the browser keeps of the content `sha256`.
pub async fn get(sha256: &str) -> Option<Kept> {
    let cache = cache().await?;
    let found = JsFuture::from(cache.match_with_str(&key(sha256)))
        .await
        .ok()?;
    let response: web_sys::Response = found.dyn_into().ok()?;
    let blob: web_sys::Blob = JsFuture::from(response.blob().ok()?)
        .await
        .ok()?
        .dyn_into()
        .ok()?;
    Some(if blob.type_() == NONE_TYPE {
        Kept::None
    } else {
        Kept::Thumbnail(blob)
    })
}

/// Keeps `blob`, the thumbnail of the content `sha256`.
pub fn keep_thumbnail(sha256: String, blob: web_sys::Blob) {
    spawn_local(async move { put(&sha256, &blob).await });
}

/// Keeps that the content `sha256` never has a thumbnail.
pub fn keep_none(sha256: String) {
    let options = web_sys::BlobPropertyBag::new();
    options.set_type(NONE_TYPE);
    let Ok(blob) =
        web_sys::Blob::new_with_str_sequence_and_options(&js_sys::Array::new(), &options)
    else {
        return;
    };
    spawn_local(async move { put(&sha256, &blob).await });
}

async fn put(sha256: &str, blob: &web_sys::Blob) {
    let Some(cache) = cache().await else {
        return;
    };
    let Ok(response) = web_sys::Response::new_with_opt_blob(Some(blob)) else {
        return;
    };
    if JsFuture::from(cache.put_with_str(&key(sha256), &response))
        .await
        .is_err()
    {
        return;
    }
    let entries = ENTRIES.with(|entries| {
        entries.set(entries.get().saturating_add(1));
        entries.get()
    });
    if entries > MAX_ENTRIES {
        trim(&cache).await;
    }
}
