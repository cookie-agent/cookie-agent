//! Code highlighting with highlight.js, shipped in the bundle
//! (`highlight.min.js`, no CDN) and loaded the first time a code block that
//! names its language is on the page.

use std::cell::Cell;

use wasm_bindgen::{JsCast as _, JsValue, closure::Closure};

thread_local! {
    static LOADING: Cell<bool> = const { Cell::new(false) };
}

const PENDING: &str = "pre code[class*=\"language-\"]:not([data-highlighted])";

fn highlighter() -> Option<js_sys::Function> {
    let window = web_sys::window()?;
    let hljs = js_sys::Reflect::get(&window, &JsValue::from_str("hljs")).ok()?;
    if hljs.is_undefined() {
        return None;
    }
    js_sys::Reflect::get(&hljs, &JsValue::from_str("highlightElement"))
        .ok()?
        .dyn_into()
        .ok()
}

/// Highlights the code blocks under `root` that name a language and are not
/// highlighted yet, loading the highlighter first when it is not.
pub fn highlight_within(root: &web_sys::Element) {
    let Ok(pending) = root.query_selector_all(PENDING) else {
        return;
    };
    let is_pending = root.matches(PENDING).unwrap_or(false);
    if pending.length() == 0 && !is_pending {
        return;
    }
    let Some(highlight) = highlighter() else {
        load();
        return;
    };
    if is_pending {
        let _ = highlight.call1(&JsValue::NULL, root);
    }
    for index in 0..pending.length() {
        if let Some(element) = pending.item(index) {
            let _ = highlight.call1(&JsValue::NULL, &element);
        }
    }
}

/// What waited for the highlighter to load: blocks of the transcript as they
/// near the view, as their rows would have been, and any others now.
fn highlight_pending() {
    let Some(pending) = web_sys::window()
        .and_then(|window| window.document())
        .and_then(|document| document.query_selector_all(PENDING).ok())
    else {
        return;
    };
    let Some(highlight) = highlighter() else {
        return;
    };
    for index in 0..pending.length() {
        let Some(block) = pending
            .item(index)
            .and_then(|node| node.dyn_into::<web_sys::Element>().ok())
        else {
            continue;
        };
        if !crate::near_view::watch(&block) {
            let _ = highlight.call1(&JsValue::NULL, &block);
        }
    }
}

/// Adds the highlighter's script once; when it has loaded, the page's pending
/// blocks are highlighted.
fn load() {
    if LOADING.with(|loading| loading.replace(true)) {
        return;
    }
    let Some(document) = web_sys::window().and_then(|window| window.document()) else {
        return;
    };
    let Ok(script) = document.create_element("script") else {
        return;
    };
    let _ = script.set_attribute("src", "highlight.min.js");
    let loaded = Closure::<dyn Fn()>::new(highlight_pending);
    let _ = script.add_event_listener_with_callback("load", loaded.as_ref().unchecked_ref());
    // Fires once, when the script loads, and the page never unloads it.
    loaded.forget();
    if let Some(head) = document.head() {
        let _ = head.append_child(&script);
    }
}
