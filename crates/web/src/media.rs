//! Media display blocks shown inline: their artifact is read from the daemon
//! (`artifact.read`) and shown through a `Blob` URL, kept for the page's
//! life so a re-rendered block shows it again at once.

use std::{cell::RefCell, collections::HashMap};

use leptos::task::spawn_local;
use wasm_bindgen::JsValue;

use crate::workspace::Workspace;

thread_local! {
    static URLS: RefCell<HashMap<String, (String, String)>> = RefCell::new(HashMap::new());
}

/// Fills every media block under `root` that is not shown yet.
pub fn show_within(root: &web_sys::Element, workspace: Workspace) {
    let Ok(blocks) = root.query_selector_all("[data-media]:not([data-shown])") else {
        return;
    };
    for index in 0..blocks.length() {
        let Some(element) = blocks
            .item(index)
            .and_then(|node| wasm_bindgen::JsCast::dyn_into::<web_sys::Element>(node).ok())
        else {
            continue;
        };
        let Some(sha256) = element.get_attribute("data-media") else {
            continue;
        };
        let _ = element.set_attribute("data-shown", "loading");
        let alt = element.get_attribute("data-alt").unwrap_or_default();
        if let Some((url, media_type)) = URLS.with(|urls| urls.borrow().get(&sha256).cloned()) {
            show(&element, &url, &media_type, &alt);
            continue;
        }
        spawn_local(async move {
            match workspace.read_media(sha256.clone()).await {
                Ok((bytes, media_type)) => match blob_url(&bytes, &media_type) {
                    Ok(url) => {
                        URLS.with(|urls| {
                            urls.borrow_mut()
                                .insert(sha256, (url.clone(), media_type.clone()));
                        });
                        show(&element, &url, &media_type, &alt);
                    }
                    Err(_) => element.set_text_content(Some("the media could not be shown")),
                },
                Err(error) => {
                    element
                        .set_text_content(Some(&format!("the media could not be read: {error}")));
                }
            }
        });
    }
}

fn blob_url(bytes: &[u8], media_type: &str) -> Result<String, JsValue> {
    let array = js_sys::Uint8Array::from(bytes);
    let parts = js_sys::Array::of1(&array);
    let options = web_sys::BlobPropertyBag::new();
    options.set_type(media_type);
    let blob = web_sys::Blob::new_with_u8_array_sequence_and_options(&parts, &options)?;
    web_sys::Url::create_object_url_with_blob(&blob)
}

fn show(element: &web_sys::Element, url: &str, media_type: &str, alt: &str) {
    let Some(document) = element.owner_document() else {
        return;
    };
    let (tag, attributes): (&str, &[(&str, &str)]) = if media_type.starts_with("image/") {
        (
            "img",
            &[("alt", alt), ("class", "max-h-96 max-w-full rounded")],
        )
    } else if media_type.starts_with("video/") {
        (
            "video",
            &[("controls", ""), ("class", "max-h-96 max-w-full rounded")],
        )
    } else if media_type.starts_with("audio/") {
        ("audio", &[("controls", "")])
    } else {
        (
            "a",
            &[
                ("download", ""),
                ("class", "text-caramel underline dark:text-maple"),
            ],
        )
    };
    let Ok(shown) = document.create_element(tag) else {
        return;
    };
    let source = if tag == "a" { "href" } else { "src" };
    let _ = shown.set_attribute(source, url);
    for (name, value) in attributes {
        let _ = shown.set_attribute(name, value);
    }
    if tag == "a" {
        shown.set_text_content(Some(if alt.is_empty() { "download" } else { alt }));
    }
    element.set_inner_html("");
    let _ = element.append_child(&shown);
    let _ = element.set_attribute("data-shown", "yes");
}
