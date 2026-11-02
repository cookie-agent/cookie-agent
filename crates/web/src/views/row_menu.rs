//! A row's ⋮ menu: the row's rarer actions as a short list, where more of
//! them can go. A wide window opens it under the ⋮ (over it near the
//! bottom of the screen); a phone shows it as a sheet from the bottom, as
//! the session's sheets are. Escape, a click outside, or an item closes
//! it, and nothing in it presses the row under it.
//!
//! The same menu opens at the pointer on a right-click
//! ([`context_menu`], docs/site/specs/quiet-ink.md, D15), with every
//! action the row has.

use icondata::Icon;
use leptos::{ev, html, prelude::*};
use wasm_bindgen::JsCast as _;

use crate::{
    icons::Glyph,
    layout::{context_place, menu_place},
};

/// An item's height in a wide window (36 px), for placing the menu.
const ITEM_HEIGHT: f64 = 36.0;
/// The menu's least width (`min-w-48`), for keeping it on the screen.
const MENU_WIDTH: f64 = 192.0;
/// The menu's padding and border, top and bottom together.
const MENU_PADDING: f64 = 10.0;

/// One action of a row's menu.
#[derive(Clone)]
pub(super) struct MenuItem {
    /// Its `data-menu-item`.
    pub id: &'static str,
    pub label: &'static str,
    pub icon: Icon,
    /// What it does; the menu has closed by then.
    pub act: Callback<()>,
    /// It destroys something: drawn in the error colour.
    pub danger: bool,
}

/// Whether a right-click should open the page's context menu rather than
/// the browser's: not with Shift held, not over selected text, and not
/// over a link, a field, or code, where the browser's own menu is the
/// useful one.
pub(super) fn wants_context_menu(event: &ev::MouseEvent) -> bool {
    if event.shift_key() {
        return false;
    }
    let selected = window()
        .get_selection()
        .ok()
        .flatten()
        .is_some_and(|selection| !selection.is_collapsed());
    if selected {
        return false;
    }
    let inside_own = event
        .target()
        .and_then(|target| target.dyn_into::<web_sys::Element>().ok())
        .and_then(|element| {
            element
                .closest("a[href], input, textarea, select, code, pre, [contenteditable]")
                .ok()
                .flatten()
        })
        .is_some();
    !inside_own
}

/// A row's `contextmenu` handler: a mouse's right-click opens the row's
/// menu at the pointer in `open`, where [`wants_context_menu`] says so.
pub(super) fn on_context(open: RwSignal<Option<((), f64, f64)>>) -> impl Fn(ev::MouseEvent) + Copy {
    move |event: ev::MouseEvent| {
        if !crate::browser::from_touch(&event) && wants_context_menu(&event) {
            event.prevent_default();
            open.set(Some((
                (),
                f64::from(event.client_x()),
                f64::from(event.client_y()),
            )));
        }
    }
}

/// The menu of `items` opened at the pointer by a right-click on the row
/// `key` names: `open` holds which row's, and where.
pub(super) fn context_menu<K>(
    open: RwSignal<Option<(K, f64, f64)>>,
    key: K,
    title: String,
    items: impl Fn() -> Vec<MenuItem> + Send + Sync + Clone + 'static,
) -> impl IntoView
where
    K: Clone + PartialEq + Send + Sync + 'static,
{
    let place = RwSignal::new(String::new());
    let items = StoredValue::new(items);
    let listed = move || items.with_value(|items| items());
    let title = StoredValue::new(title);
    let at = Memo::new(move |_| {
        open.with(|open| {
            open.as_ref()
                .filter(|(shown, _, _)| *shown == key)
                .map(|(_, x, y)| (*x, *y))
        })
    });
    // Placed when it opens: the CSS variables the ⋮'s menu uses.
    Effect::new(move |_| {
        let Some(point) = at.get() else {
            return;
        };
        let window = window();
        let size = |value: Result<wasm_bindgen::JsValue, _>| {
            value.ok().and_then(|value| value.as_f64()).unwrap_or(0.0)
        };
        let viewport = (size(window.inner_width()), size(window.inner_height()));
        #[allow(clippy::cast_precision_loss)]
        let height = untrack(listed).len() as f64 * ITEM_HEIGHT + MENU_PADDING;
        let at = context_place(point, viewport, height);
        let px = |value: Option<f64>| {
            value.map_or_else(|| "auto".to_owned(), |value| format!("{value}px"))
        };
        place.set(format!(
            "--menu-top: {}; --menu-bottom: {}; --menu-right: {}px",
            px(at.top),
            px(at.bottom),
            at.right,
        ));
    });
    let close = move |_refocus: bool| open.set(None);
    view! {
        <Show when=move || at.with(Option::is_some) && !listed().is_empty()>
            {move || menu(listed(), title.get_value(), place, close)}
        </Show>
    }
}

/// A row's ⋮, styled `class`, and the menu of `items` it opens; no ⋮ while
/// there are none. `open` names the one open menu of the rows sharing it,
/// `key` this one's; `name` is the ⋮'s `data-row-menu`, and `title` heads
/// the phone's sheet and names the menu.
pub(super) fn row_menu<K>(
    open: RwSignal<Option<K>>,
    key: K,
    name: String,
    title: String,
    class: &'static str,
    items: impl Fn() -> Vec<MenuItem> + Send + Sync + Clone + 'static,
) -> impl IntoView
where
    K: Clone + PartialEq + Send + Sync + 'static,
{
    let trigger = NodeRef::<html::Button>::new();
    let is_open = {
        let key = key.clone();
        move || open.with(|open| open.as_ref() == Some(&key))
    };
    // Where the menu stands in a wide window: CSS variables, which a
    // phone's sheet ignores.
    let place = RwSignal::new(String::new());
    let items = StoredValue::new(items);
    let listed = move || items.with_value(|items| items());
    let title = StoredValue::new(title);
    let toggle = {
        let key = key.clone();
        let is_open = is_open.clone();
        move |_| {
            if is_open() {
                open.set(None);
                return;
            }
            let Some(button) = trigger.get_untracked() else {
                return;
            };
            let frame = button.get_bounding_client_rect();
            let window = window();
            let size = |value: Result<wasm_bindgen::JsValue, _>| {
                value.ok().and_then(|value| value.as_f64()).unwrap_or(0.0)
            };
            let viewport = (size(window.inner_width()), size(window.inner_height()));
            #[allow(clippy::cast_precision_loss)]
            let height = listed().len() as f64 * ITEM_HEIGHT + MENU_PADDING;
            // Right-aligned to the ⋮, but never past the window's left
            // edge: the sidebar stands against it.
            let at = menu_place(
                (
                    frame.top(),
                    frame.bottom(),
                    frame.right().max(MENU_WIDTH + 8.0),
                ),
                viewport,
                height,
            );
            let px = |value: Option<f64>| {
                value.map_or_else(|| "auto".to_owned(), |value| format!("{value}px"))
            };
            place.set(format!(
                "--menu-top: {}; --menu-bottom: {}; --menu-right: {}px",
                px(at.top),
                px(at.bottom),
                at.right,
            ));
            open.set(Some(key.clone()));
        }
    };
    let close = move |refocus: bool| {
        open.set(None);
        if refocus && let Some(button) = trigger.get_untracked() {
            let _ = button.focus();
        }
    };
    let has_items = move || !listed().is_empty();
    view! {
        <Show when=has_items>
            <button
                type="button"
                class=class
                aria-label="More actions"
                title="More actions"
                aria-haspopup="menu"
                aria-expanded={
                    let is_open = is_open.clone();
                    move || is_open().to_string()
                }
                data-row-menu=name.clone()
                node_ref=trigger
                on:click=toggle.clone()
            >
                <Glyph icon=icondata::LuEllipsisVertical />
            </button>
            <Show when=is_open.clone()>
                {move || menu(listed(), title.get_value(), place, close)}
            </Show>
        </Show>
    }
}

/// The open menu over a backdrop that closes it.
fn menu(
    items: Vec<MenuItem>,
    title: String,
    place: RwSignal<String>,
    close: impl Fn(bool) + Copy + Send + Sync + 'static,
) -> impl IntoView {
    let list = NodeRef::<html::Div>::new();
    let heading = title.clone();
    // Back closes the menu before anything under it.
    if let Some(workspace) = use_context::<crate::workspace::Workspace>() {
        workspace.open_transient(Callback::new(move |()| close(false)));
        on_cleanup(move || workspace.transient_closed());
    }
    // Escape closes it wherever the focus is; the keyboard starts on the
    // first item.
    let escape = window_event_listener(ev::keydown, move |event| {
        if event.key() == "Escape" {
            event.prevent_default();
            close(true);
        }
    });
    let escape = StoredValue::new_local(Some(escape));
    on_cleanup(move || {
        if let Some(escape) = escape.try_update_value(Option::take).flatten() {
            escape.remove();
        }
    });
    Effect::new(move |_| {
        if let Some(first) = list
            .get()
            .and_then(|list| menu_items(&list).into_iter().next())
        {
            let _ = first.focus();
        }
    });
    // Up and down move between the items; Tab leaves the menu for the ⋮.
    let on_keydown = move |event: ev::KeyboardEvent| {
        let Some(list) = list.get_untracked() else {
            return;
        };
        let items = menu_items(&list);
        let focused = document().active_element();
        let at = items
            .iter()
            .position(|item| focused.as_ref() == Some(item.unchecked_ref()));
        let next = match event.key().as_str() {
            "ArrowDown" => at.map_or(0, |at| (at + 1) % items.len()),
            "ArrowUp" => at.map_or(items.len() - 1, |at| (at + items.len() - 1) % items.len()),
            "Home" => 0,
            "End" => items.len() - 1,
            "Tab" => {
                event.prevent_default();
                close(true);
                return;
            }
            _ => return,
        };
        event.prevent_default();
        if let Some(item) = items.get(next) {
            let _ = item.focus();
        }
    };
    view! {
        <div
            class="fixed inset-0 z-50 compact:bg-scrim"
            data-row-menu-backdrop
            on:click=move |_| close(false)
            // A wheel would scroll the list from under the menu.
            on:wheel=move |_| close(false)
        ></div>
        <div
            role="menu"
            aria-label=title
            class="menu-in fixed top-(--menu-top) right-(--menu-right) bottom-(--menu-bottom) z-50 flex min-w-48 flex-col rounded-xl bg-raised p-1 text-fg shadow-menu ring-1 ring-line compact:top-auto compact:right-0 compact:bottom-0 compact:left-0 compact:min-w-0 compact:rounded-none compact:rounded-t-2xl compact:border-x-0 compact:border-b-0 compact:p-2 compact:pb-[max(0.5rem,env(safe-area-inset-bottom))]"
            style=move || place.get()
            data-row-menu-list
            node_ref=list
            on:keydown=on_keydown
        >
            <span class="mx-auto mb-1 hidden h-1 w-10 rounded-full bg-line-2 compact:block" aria-hidden="true"></span>
            <p class="hidden truncate px-3 pt-1 pb-2 text-sm font-bold compact:block" aria-hidden="true">{heading}</p>
            {items
                .into_iter()
                .map(|item| {
                    let act = item.act;
                    view! {
                        <button
                            type="button"
                            role="menuitem"
                            class=if item.danger {
                                "flex min-h-9 w-full items-center gap-2.5 rounded-md px-2.5 text-left text-[0.8125rem] whitespace-nowrap text-bad outline-none hover:bg-bad-soft focus-visible:bg-bad-soft active:bg-bad-soft compact:min-h-12 compact:text-sm"
                            } else {
                                "flex min-h-9 w-full items-center gap-2.5 rounded-md px-2.5 text-left text-[0.8125rem] whitespace-nowrap outline-none hover:bg-hover focus-visible:bg-hover active:bg-press compact:min-h-12 compact:text-sm"
                            }
                            data-menu-item=item.id
                            on:click=move |_| {
                                close(false);
                                act.run(());
                            }
                        >
                            <Glyph
                                icon=item.icon
                                class=if item.danger { "w-5 justify-center text-bad" } else { "w-5 justify-center text-muted" }
                            />
                            <span>{item.label}</span>
                        </button>
                    }
                })
                .collect_view()}
        </div>
    }
}

/// The menu's items, in order.
fn menu_items(list: &web_sys::HtmlDivElement) -> Vec<web_sys::HtmlElement> {
    let Ok(found) = list.query_selector_all("[role=menuitem]") else {
        return Vec::new();
    };
    (0..found.length())
        .filter_map(|index| found.item(index))
        .filter_map(|node| node.dyn_into::<web_sys::HtmlElement>().ok())
        .collect()
}
