//! The few browser services the page uses: its address, storage, history,
//! the colour scheme, and visibility. Everything here is a thin call into
//! `web_sys`; it compiles on every target but only works in a browser.

use wasm_bindgen::{JsCast as _, closure::Closure};

fn window() -> Option<web_sys::Window> {
    web_sys::window()
}

/// The page's `location.href`.
pub fn page_href() -> Option<String> {
    window()?.location().href().ok()
}

/// The page's `location.origin`.
pub fn page_origin() -> Option<String> {
    window()?.location().origin().ok()
}

/// The page's `location.hash`, `#` included.
pub fn page_fragment() -> Option<String> {
    window()?.location().hash().ok()
}

/// Replaces the page's fragment in the address bar and history without
/// reloading; an empty `fragment` removes it.
pub fn replace_fragment(fragment: &str) {
    let Some(window) = window() else {
        return;
    };
    let location = window.location();
    let (Ok(path), Ok(search)) = (location.pathname(), location.search()) else {
        return;
    };
    if let Ok(history) = window.history() {
        let _ = history.replace_state_with_url(
            &wasm_bindgen::JsValue::NULL,
            "",
            Some(&format!("{path}{search}{fragment}")),
        );
    }
}

fn local_storage() -> Option<web_sys::Storage> {
    window()?.local_storage().ok().flatten()
}

fn session_storage() -> Option<web_sys::Storage> {
    window()?.session_storage().ok().flatten()
}

/// A value from `localStorage`.
pub fn load(key: &str) -> Option<String> {
    local_storage()?.get_item(key).ok().flatten()
}

/// Keeps a value in `localStorage`; a page without storage keeps nothing.
pub fn store(key: &str, value: &str) {
    if let Some(storage) = local_storage() {
        let _ = storage.set_item(key, value);
    }
}

/// Removes a value from `localStorage`.
pub fn forget(key: &str) {
    if let Some(storage) = local_storage() {
        let _ = storage.remove_item(key);
    }
}

/// A value from this tab's `sessionStorage`.
pub fn load_tab(key: &str) -> Option<String> {
    session_storage()?.get_item(key).ok().flatten()
}

/// Keeps a value in this tab's `sessionStorage`.
pub fn store_tab(key: &str, value: &str) {
    if let Some(storage) = session_storage() {
        let _ = storage.set_item(key, value);
    }
}

fn dark_query() -> Option<web_sys::MediaQueryList> {
    window()?
        .match_media("(prefers-color-scheme: dark)")
        .ok()
        .flatten()
}

/// Whether the system prefers a dark colour scheme.
pub fn system_prefers_dark() -> bool {
    dark_query().is_some_and(|query| query.matches())
}

/// Calls `changed` whenever the system's colour scheme changes, for as long
/// as the page lives.
pub fn on_system_scheme_change(changed: impl Fn() + 'static) {
    let Some(query) = dark_query() else {
        return;
    };
    let listener = Closure::<dyn Fn()>::new(changed);
    let _ = query.add_event_listener_with_callback("change", listener.as_ref().unchecked_ref());
    listener.forget();
}

/// Turns the dark palette on or off: the `dark` class on `<html>`, which
/// Tailwind's `dark:` variant follows.
pub fn set_dark(dark: bool) {
    let Some(root) = window()
        .and_then(|window| window.document())
        .and_then(|document| document.document_element())
    else {
        return;
    };
    let classes = root.class_list();
    let _ = if dark {
        classes.add_1("dark")
    } else {
        classes.remove_1("dark")
    };
}

/// Calls `shown` whenever the page becomes visible again, as a phone's tab
/// does after it was suspended, for as long as the page lives.
pub fn on_visible(shown: impl Fn() + 'static) {
    let Some(document) = window().and_then(|window| window.document()) else {
        return;
    };
    let watched = document.clone();
    let listener = Closure::<dyn Fn()>::new(move || {
        if watched.visibility_state() == web_sys::VisibilityState::Visible {
            shown();
        }
    });
    let _ = document
        .add_event_listener_with_callback("visibilitychange", listener.as_ref().unchecked_ref());
    listener.forget();
}
