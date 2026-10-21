//! The few browser services the page uses: its address, storage, history,
//! the colour scheme, visibility, and installing the page as an app. Everything here is a thin call into
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
    set_theme_color(if dark { THEME_DARK } else { THEME_LIGHT });
}

/// The browser's colour around the page (`theme-color`) in the light and
/// the dark theme: the palette's cream and night, as in `index.html` and
/// the manifest.
const THEME_LIGHT: &str = "#fbf4e6";
const THEME_DARK: &str = "#201c16";

/// Sets every `theme-color` of the page to `colour`, so the browser's bar,
/// or an installed app's title bar, follows the user's theme rather than
/// only the system's.
fn set_theme_color(colour: &str) {
    let Some(metas) = window()
        .and_then(|window| window.document())
        .and_then(|document| document.query_selector_all("meta[name=theme-color]").ok())
    else {
        return;
    };
    for index in 0..metas.length() {
        if let Some(meta) = metas
            .item(index)
            .and_then(|node| node.dyn_into::<web_sys::Element>().ok())
        {
            let _ = meta.set_attribute("content", colour);
        }
    }
}

/// `window[name]`, `undefined` outside a browser.
fn window_value(name: &str) -> wasm_bindgen::JsValue {
    window()
        .and_then(|window| js_sys::Reflect::get(&window, &name.into()).ok())
        .unwrap_or(wasm_bindgen::JsValue::UNDEFINED)
}

/// Whether the page is a secure context (HTTPS, or `localhost` and
/// `127.0.0.1`), which installing it and its service worker need.
pub fn is_secure_context() -> bool {
    window().is_some_and(|window| window.is_secure_context())
}

/// Registers the page's service worker, `sw.js` beside it, which caches
/// nothing (it makes the page installable). Only a secure page can; the
/// browser checks it for a new build on every load, past its HTTP cache.
pub fn register_service_worker() {
    let Some(window) = window().filter(web_sys::Window::is_secure_context) else {
        return;
    };
    let navigator = window.navigator();
    // A private window may have no service workers.
    if !js_sys::Reflect::has(&navigator, &"serviceWorker".into()).unwrap_or(false) {
        return;
    }
    let options = web_sys::RegistrationOptions::new();
    options.set_update_via_cache(web_sys::ServiceWorkerUpdateViaCache::None);
    let registered = navigator
        .service_worker()
        .register_with_options("sw.js", &options);
    wasm_bindgen_futures::spawn_local(async move {
        let _ = wasm_bindgen_futures::JsFuture::from(registered).await;
    });
}

/// Whether the page runs as an installed app: in a standalone window, or
/// from an iOS home screen (`navigator.standalone`).
pub fn is_standalone() -> bool {
    matches(STANDALONE_QUERY)
        || window()
            .and_then(|window| js_sys::Reflect::get(&window.navigator(), &"standalone".into()).ok())
            .is_some_and(|standalone| standalone.is_truthy())
}

/// The media query an installed app's window matches.
pub const STANDALONE_QUERY: &str = "(display-mode: standalone)";

/// The browser's `navigator.userAgent` and `navigator.maxTouchPoints`.
pub fn user_agent() -> (String, i32) {
    window().map_or_else(Default::default, |window| {
        let navigator = window.navigator();
        (
            navigator.user_agent().unwrap_or_default(),
            navigator.max_touch_points(),
        )
    })
}

/// Whether the browser handed the page an install prompt it has not used
/// yet; `index.html` keeps it as `window.cookieInstallPrompt`.
pub fn install_prompt_ready() -> bool {
    window_value("cookieInstallPrompt").is_object()
}

/// Whether the browser said it installed the page during this visit.
pub fn app_installed() -> bool {
    window_value("cookieInstalled").is_truthy()
}

/// Calls `changed` whenever the browser offers to install the page or
/// installs it, for as long as the page lives.
pub fn on_install_change(changed: impl Fn() + 'static) {
    let Some(window) = window() else {
        return;
    };
    let listener = Closure::<dyn Fn()>::new(changed);
    let _ = window
        .add_event_listener_with_callback("cookie-install", listener.as_ref().unchecked_ref());
    listener.forget();
}

/// Opens the browser's install prompt, which it offers once: call it from
/// the click that asks for it. Resolves to whether the user installed the
/// page; `None` without a prompt.
pub fn prompt_install() -> Option<impl std::future::Future<Output = bool>> {
    let window = window()?;
    let event = window_value("cookieInstallPrompt");
    if !event.is_object() {
        return None;
    }
    let _ = js_sys::Reflect::set(
        &window,
        &"cookieInstallPrompt".into(),
        &wasm_bindgen::JsValue::NULL,
    );
    let prompt = js_sys::Reflect::get(&event, &"prompt".into())
        .ok()?
        .dyn_into::<js_sys::Function>()
        .ok()?;
    prompt.call0(&event).ok()?;
    let choice = js_sys::Reflect::get(&event, &"userChoice".into())
        .ok()
        .and_then(|choice| choice.dyn_into::<js_sys::Promise>().ok());
    Some(async move {
        let Some(choice) = choice else {
            return false;
        };
        wasm_bindgen_futures::JsFuture::from(choice)
            .await
            .ok()
            .and_then(|choice| js_sys::Reflect::get(&choice, &"outcome".into()).ok())
            .and_then(|outcome| outcome.as_string())
            .is_some_and(|outcome| outcome == "accepted")
    })
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

/// The media query a phone matches: narrower than Tailwind's `sm`.
pub const PHONE_QUERY: &str = "(max-width: 39.99rem)";

/// The media query a desktop matches: Tailwind's `lg` and wider, where the
/// sessions sidebar is pinned beside the transcript.
pub const DESKTOP_QUERY: &str = "(min-width: 64rem)";

fn media_query(query: &str) -> Option<web_sys::MediaQueryList> {
    window()?.match_media(query).ok().flatten()
}

/// Whether the page matches the media `query` now.
pub fn matches(query: &str) -> bool {
    media_query(query).is_some_and(|list| list.matches())
}

/// Calls `changed` with whether the page matches `query` whenever that
/// changes, for as long as the page lives.
pub fn on_media_change(query: &str, changed: impl Fn(bool) + 'static) {
    let Some(list) = media_query(query) else {
        return;
    };
    let watched = list.clone();
    let listener = Closure::<dyn Fn()>::new(move || changed(watched.matches()));
    let _ = list.add_event_listener_with_callback("change", listener.as_ref().unchecked_ref());
    listener.forget();
}

/// Adds a history entry for the page as it is, carrying `depth`: how many
/// of the page's layers are open with it. Browser Back then returns to the
/// entry below instead of leaving the page.
pub fn push_history(depth: usize) {
    if let Some(history) = window().and_then(|window| window.history().ok()) {
        let _ = history.push_state(&wasm_bindgen::JsValue::from(depth as f64), "");
    }
}

/// Goes `steps` entries back in the history.
pub fn history_back(steps: usize) {
    if let Some(history) = window().and_then(|window| window.history().ok()) {
        let _ = history.go_with_delta(-(steps as i32));
    }
}

/// Calls `arrived` with the depth of the history entry the browser moved
/// to (0 for one the page did not add), for as long as the page lives.
pub fn on_history_move(arrived: impl Fn(usize) + 'static) {
    let Some(window) = window() else {
        return;
    };
    let listener =
        Closure::<dyn Fn(web_sys::PopStateEvent)>::new(move |event: web_sys::PopStateEvent| {
            let depth = event.state().as_f64().map_or(0, |depth| depth as usize);
            arrived(depth);
        });
    let _ = window.add_event_listener_with_callback("popstate", listener.as_ref().unchecked_ref());
    listener.forget();
}
