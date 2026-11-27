//! Fills a transcript row's code and media as it nears the view. Opening a
//! session draws all its rows; highlighting and fetching for every one of them
//! would freeze the first paint and read every image of the session, and most
//! of those rows are never scrolled to. Within a row that came near, a card
//! that reads its file whole (a picture, a video, a player) waits again until
//! the card itself is within a screen of the view
//! (`docs/site/specs/attachment-handling.md` section 5.1).

use std::cell::RefCell;
use std::rc::Rc;

use wasm_bindgen::{JsCast as _, closure::Closure};
use web_sys::{IntersectionObserver, IntersectionObserverEntry, IntersectionObserverInit};

/// How far from the view a row starts filling: about two screens, so that it
/// is ready before scrolling reaches it.
const MARGIN: &str = "1600px 0px";

/// How far from the view a card starts reading its file: one screen.
const CARD_MARGIN: &str = "100% 0px";

type Callback = Closure<dyn Fn(js_sys::Array, IntersectionObserver)>;

/// Watches the rows, and the cards, of one scrolling transcript.
struct NearView {
    observer: IntersectionObserver,
    cards: IntersectionObserver,
    root: web_sys::Element,
    _callbacks: (Callback, Callback),
}

thread_local! {
    /// The transcript on the page: the page shows one at a time.
    static CURRENT: RefCell<Option<Rc<NearView>>> = const { RefCell::new(None) };
}

/// An observer of `root` with `margin` that runs `fill` on each element
/// it watches as it comes near, once.
fn observer(
    root: &web_sys::Element,
    margin: &str,
    fill: impl Fn(&web_sys::Element) + 'static,
) -> Option<(IntersectionObserver, Callback)> {
    let callback = Callback::new(
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
    options.set_root_margin(margin);
    let observer =
        IntersectionObserver::new_with_options(callback.as_ref().unchecked_ref(), &options).ok()?;
    Some((observer, callback))
}

/// Starts watching for `root`, the scrolling transcript, replacing any before.
/// `fill` runs on each watched row as it comes near, once per [`watch`];
/// `fill_card` on each watched card, once per [`watch_card`].
pub fn start(
    root: &web_sys::Element,
    fill: impl Fn(&web_sys::Element) + 'static,
    fill_card: impl Fn(&web_sys::Element) + 'static,
) {
    let (Some((observer, rows)), Some((cards, card_callback))) = (
        observer(root, MARGIN, fill),
        observer(root, CARD_MARGIN, fill_card),
    ) else {
        return;
    };
    let view = Rc::new(NearView {
        observer,
        cards,
        root: root.clone(),
        _callbacks: (rows, card_callback),
    });
    if let Some(before) = CURRENT.with(|current| current.borrow_mut().replace(view)) {
        before.observer.disconnect();
        before.cards.disconnect();
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
        view.cards.disconnect();
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

/// [`watch`] for a card, which fills within a screen of the view.
pub fn watch_card(element: &web_sys::Element) -> bool {
    CURRENT.with(|current| match current.borrow().as_ref() {
        Some(view) if view.root.contains(Some(element)) => {
            view.cards.observe(element);
            true
        }
        _ => false,
    })
}
