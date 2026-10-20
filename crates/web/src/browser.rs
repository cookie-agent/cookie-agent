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

/// Copies `text` with `navigator.clipboard`, which browsers offer only on
/// a secure page (HTTPS or localhost); `false` when it did not copy.
pub async fn copy_text(text: &str) -> bool {
    let Some(window) = window().filter(web_sys::Window::is_secure_context) else {
        return false;
    };
    let promise = window.navigator().clipboard().write_text(text);
    wasm_bindgen_futures::JsFuture::from(promise).await.is_ok()
}

/// Selects the text of the text field `id`, for the user to copy.
pub fn select_text(id: &str) {
    if let Some(field) = window()
        .and_then(|window| window.document())
        .and_then(|document| document.get_element_by_id(id))
        .and_then(|element| element.dyn_into::<web_sys::HtmlTextAreaElement>().ok())
    {
        let _ = field.focus();
        field.select();
    }
}

/// Keeps `--app-height` on `<html>` at the visual viewport's height, which a
/// phone's on-screen keyboard shrinks while the layout viewport (and
/// `100dvh`) may not, so the composer and an open sheet stay above the
/// keyboard. The page's frame and sheets size themselves from it.
pub fn follow_visual_viewport() {
    let Some(window) = window() else {
        return;
    };
    let Some(viewport) = window.visual_viewport() else {
        return;
    };
    let update = {
        let viewport = viewport.clone();
        let window = window.clone();
        move || {
            let Some(root) = window
                .document()
                .and_then(|document| document.document_element())
                .and_then(|root| root.dyn_into::<web_sys::HtmlElement>().ok())
            else {
                return;
            };
            let _ = root
                .style()
                .set_property("--app-height", &format!("{}px", viewport.height()));
            // A browser that scrolled the page to show the focused field
            // leaves the frame where it was; bring it back.
            if viewport.offset_top() > 0.0 {
                window.scroll_to_with_x_and_y(0.0, 0.0);
            }
        }
    };
    update();
    let listener = Closure::<dyn Fn()>::new(update);
    let _ = viewport.add_event_listener_with_callback("resize", listener.as_ref().unchecked_ref());
    listener.forget();
}
