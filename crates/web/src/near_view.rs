//! Fills a transcript row's code and media as it nears the view. Opening a
//! session draws all its rows; highlighting and fetching for every one of them
//! would freeze the first paint and read every image of the session, and most
//! of those rows are never scrolled to.

use std::cell::RefCell;
use std::rc::Rc;

use wasm_bindgen::{JsCast as _, closure::Closure};
use web_sys::{IntersectionObserver, IntersectionObserverEntry, IntersectionObserverInit};

/// How far from the view a row starts filling: about two screens, so that it
/// is ready before scrolling reaches it.
const MARGIN: &str = "1600px 0px";

/// Watches the rows of one scrolling transcript.
struct NearView {
    observer: IntersectionObserver,
    root: web_sys::Element,
    _callback: Closure<dyn Fn(js_sys::Array, IntersectionObserver)>,
}

thread_local! {
    /// The transcript on the page: the page shows one at a time.
    static CURRENT: RefCell<Option<Rc<NearView>>> = const { RefCell::new(None) };
}

/// Starts watching for `root`, the scrolling transcript, replacing any before.
/// `fill` runs on each watched element as it comes near, once per [`watch`].
pub fn start(root: &web_sys::Element, fill: impl Fn(&web_sys::Element) + 'static) {
    let callback = Closure::<dyn Fn(js_sys::Array, IntersectionObserver)>::new(
        move |entries: js_sys::Array, observer: IntersectionObserver| {
            for entry in entries.iter() {
                let Ok(entry) = entry.dyn_into::<IntersectionObserverEntry>() else {
                    continue;
                };
                if entry.is_intersecting() {
                    let target = entry.target();
                    observer.unobserve(&target);
                    fill(&target);
                }
            }
        },
    );
    let options = IntersectionObserverInit::new();
    options.set_root(Some(root));
    options.set_root_margin(MARGIN);
    let Ok(observer) =
        IntersectionObserver::new_with_options(callback.as_ref().unchecked_ref(), &options)
    else {
        return;
    };
    let view = Rc::new(NearView {
        observer,
        root: root.clone(),
        _callback: callback,
    });
    if let Some(before) = CURRENT.with(|current| current.borrow_mut().replace(view)) {
        before.observer.disconnect();
    }
}

/// Stops watching for `root`, if it is the transcript watched.
pub fn stop(root: &web_sys::Element) {
    let stopped = CURRENT.with(|current| {
        let mut current = current.borrow_mut();
        if current.as_ref().is_some_and(|view| view.root == *root) {
            current.take()
        } else {
            None
        }
    });
    if let Some(view) = stopped {
        view.observer.disconnect();
    }
}

/// Has `element` filled when it comes near, at once if it is near already.
/// Returns false when no transcript on the page holds it, for the caller to
/// fill it now.
pub fn watch(element: &web_sys::Element) -> bool {
    CURRENT.with(|current| match current.borrow().as_ref() {
        Some(view) if view.root.contains(Some(element)) => {
            view.observer.observe(element);
            true
        }
        _ => false,
    })
}
