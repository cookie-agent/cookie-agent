//! The side panel (`docs/site/specs/assets.md` section 8.1): an HTML page
//! or a Markdown document a shelf's card opened, beside the transcript on a
//! desktop's width and over it, full screen, on narrower windows. Its head
//! has the asset's title and handle, *Preview* and *Source*, *Copy handle*,
//! *Full screen*, *Download*, and *Close* (Escape too). Beside the
//! transcript, a divider on its edge sets how much of the row it takes.

use leptos::{ev, html, prelude::*};

use crate::{
    assets::OpenAsset,
    browser,
    icons::Glyph,
    layout::{ASSET_PANEL_SHARE_KEY, asset_panel_share, parse_asset_panel_share},
    workspace::Workspace,
};

const BUTTON: &str = "inline-flex min-h-9 min-w-9 shrink-0 items-center justify-center gap-1 rounded-md px-2 text-xs hover:bg-hover active:bg-press";
const TAB: &str = "inline-flex min-h-8 items-center rounded-md border px-2 text-xs";
const TAB_ON: &str = "border-fg bg-accent text-on-accent";
const TAB_OFF: &str = "border-line-2 hover:bg-hover";
/// How far an arrow key moves the divider, in CSS pixels.
const DIVIDER_STEP: f64 = 32.0;

/// The panel, open while `workspace.asset_panel` holds an asset. A session
/// opened in its place closes it.
#[component]
pub(crate) fn AssetPanel(workspace: Workspace) -> impl IntoView {
    Effect::new(
        move |previous: Option<Option<cookie_agent_protocol::SessionId>>| {
            let selected = workspace.selected.get();
            if previous.is_some_and(|previous| previous != selected) {
                workspace.asset_panel.set(None);
            }
            selected
        },
    );
    move || {
        workspace
            .asset_panel
            .get()
            .map(|open| view! { <Panel workspace open /> })
    }
}

/// The divider between the transcript and the panel: a drag or the arrow
/// keys set `share`, the part of the row the panel takes, which the browser
/// remembers, and a double click goes back to the default width. Its parent
/// `root` is the panel, whose parent is the row.
#[component]
fn Divider(
    root: NodeRef<html::Aside>,
    share: RwSignal<Option<f64>>,
    dragging: RwSignal<bool>,
) -> impl IntoView {
    let handle = NodeRef::<html::Div>::new();
    // How far right of the panel's edge the press was, so the edge does not
    // jump to the pointer.
    let grab = StoredValue::new(0.0_f64);
    // Puts the panel's left edge where `edge` says, given where it is.
    let place = move |edge: &dyn Fn(f64) -> f64| {
        let Some(panel) = root.get_untracked() else {
            return;
        };
        let Some(row) = panel.parent_element() else {
            return;
        };
        let (row, panel) = (
            row.get_bounding_client_rect(),
            panel.get_bounding_client_rect(),
        );
        let left = edge(panel.left());
        if let Some(next) = asset_panel_share(row.width(), row.right() - left) {
            share.set(Some(next));
        }
    };
    let keep = move || {
        if let Some(share) = share.get_untracked() {
            browser::store(ASSET_PANEL_SHARE_KEY, &share.to_string());
        }
    };
    let on_pointerdown = move |event: ev::PointerEvent| {
        if event.button() != 0 {
            return;
        }
        // No text selection under the drag.
        event.prevent_default();
        if let Some(panel) = root.get_untracked() {
            grab.set_value(f64::from(event.client_x()) - panel.get_bounding_client_rect().left());
        }
        // Moves over the transcript or over a page's frame come here too.
        if let Some(handle) = handle.get_untracked() {
            let _ = handle.set_pointer_capture(event.pointer_id());
        }
        dragging.set(true);
    };
    let on_pointermove = move |event: ev::PointerEvent| {
        if dragging.get_untracked() {
            let left = f64::from(event.client_x()) - grab.get_value();
            place(&|_| left);
        }
    };
    let on_pointerup = move |_: ev::PointerEvent| {
        if dragging.get_untracked() {
            dragging.set(false);
            keep();
        }
    };
    let on_keydown = move |event: ev::KeyboardEvent| {
        let by = match event.key().as_str() {
            "ArrowLeft" => -DIVIDER_STEP,
            "ArrowRight" => DIVIDER_STEP,
            _ => return,
        };
        event.prevent_default();
        place(&|left| left + by);
        keep();
    };
    view! {
        <div
            node_ref=handle
            id="asset-panel-divider"
            role="separator"
            aria-orientation="vertical"
            aria-label="Resize the panel"
            aria-valuenow=move || share.get().map(|share| (share * 100.0).round().to_string())
            tabindex="0"
            title="Drag to resize, double-click to reset"
            class="group absolute inset-y-0 -left-px z-10 hidden w-2 cursor-col-resize touch-none outline-none lg:block"
            on:pointerdown=on_pointerdown
            on:pointermove=on_pointermove
            on:pointerup=on_pointerup
            on:pointercancel=on_pointerup
            on:dblclick=move |_| {
                share.set(None);
                browser::forget(ASSET_PANEL_SHARE_KEY);
            }
            on:keydown=on_keydown
        >
            <div
                class="h-full w-0.5 group-hover:bg-accent group-focus-visible:bg-accent"
                class:bg-accent=move || dragging.get()
            ></div>
        </div>
    }
}

#[component]
fn Panel(workspace: Workspace, open: OpenAsset) -> impl IntoView {
    let source = RwSignal::new(false);
    let full = RwSignal::new(false);
    let body = NodeRef::<html::Div>::new();
    let root = NodeRef::<html::Aside>::new();
    let share = RwSignal::new(
        browser::load(ASSET_PANEL_SHARE_KEY).and_then(|stored| parse_asset_panel_share(&stored)),
    );
    let dragging = RwSignal::new(false);
    let close = move || workspace.asset_panel.set(None);
    {
        let open = open.clone();
        Effect::new(move |_| {
            let source = source.get();
            let Some(element) = body.get() else {
                return;
            };
            element.set_inner_html(&open.placeholder_html(source));
            crate::assets::show_within(&element, workspace);
        });
    }
    // The focus goes to the panel, for Escape.
    Effect::new(move |_| {
        if let Some(root) = root.get() {
            let _ = root.focus();
        }
    });
    let has_source = open.has_source();
    let title = open.title.clone();
    let label = title.clone();
    let handle = open.handle.clone();
    let copy = handle.clone();
    let (origin, file_title) = (open.source.clone(), open.title.clone());
    let copyable = !handle.is_empty();
    let shareable = match &open.source {
        crate::assets::Source::Asset { store, asset } => Some((store.clone(), *asset)),
        crate::assets::Source::Artifact { .. } => None,
    };
    let share_title = open.title.clone();
    let tab = move |is_source: bool| {
        move || {
            format!(
                "{TAB} {}",
                if source.get() == is_source {
                    TAB_ON
                } else {
                    TAB_OFF
                }
            )
        }
    };
    let class = move || {
        if full.get() {
            "fixed inset-0 z-50 flex flex-col bg-surface-2 outline-none"
        } else {
            "fixed inset-0 z-40 flex flex-col bg-surface-2 outline-none lg:relative lg:z-auto lg:w-[clamp(320px,var(--asset-panel-width,min(48rem,48%)),calc(100%-320px))] lg:shrink-0 lg:border-l lg:border-line"
        }
    };
    view! {
        <aside
            node_ref=root
            id="asset-panel"
            role="dialog"
            aria-label=label
            tabindex="-1"
            class=class
            style=move || {
                share
                    .get()
                    .map(|share| format!("--asset-panel-width:{:.2}%", share * 100.0))
                    .unwrap_or_default()
            }
            data-full=move || full.get().to_string()
            on:keydown=move |event: leptos::ev::KeyboardEvent| {
                if event.key() == "Escape" {
                    event.prevent_default();
                    if full.get_untracked() {
                        full.set(false);
                    } else {
                        close();
                    }
                }
            }
        >
            <Show when=move || !full.get()>
                <Divider root share dragging />
            </Show>
            <div class="flex min-w-0 shrink-0 flex-wrap items-center gap-1 border-b border-line px-2 py-1.5">
                <span class="flex min-w-0 flex-1 flex-col px-1">
                    <span class="truncate text-sm font-bold" id="asset-panel-title">{title}</span>
                    <span class="truncate font-mono text-[0.6875rem] text-muted select-all" id="asset-panel-handle">
                        {handle}
                    </span>
                </span>
                <Show when=move || has_source>
                    <span class="flex shrink-0 gap-1" role="tablist">
                        <button
                            type="button"
                            role="tab"
                            id="asset-panel-preview"
                            class=tab(false)
                            aria-selected=move || (!source.get()).to_string()
                            on:click=move |_| source.set(false)
                        >
                            "Preview"
                        </button>
                        <button
                            type="button"
                            role="tab"
                            id="asset-panel-source"
                            class=tab(true)
                            aria-selected=move || source.get().to_string()
                            on:click=move |_| source.set(true)
                        >
                            "Source"
                        </button>
                    </span>
                </Show>
                // A stored file has no handle to copy.
                <Show when=move || copyable>
                    <button
                        type="button"
                        id="asset-panel-copy"
                        class=BUTTON
                        title="Copy the handle"
                        aria-label="Copy the handle"
                        on:click={
                            let copy = copy.clone();
                            move |_| crate::assets::copy_handle(copy.clone(), workspace)
                        }
                    >
                        <Glyph icon=icondata::LuCopy />
                    </button>
                </Show>
                {shareable.map(|(store, asset)| {
                    view! {
                        <Show when=move || workspace.has_shares.get()>
                            <button
                                type="button"
                                id="asset-panel-share"
                                class=BUTTON
                                title="Share link"
                                aria-label="Share link"
                                on:click={
                                    let (store, title) = (store.clone(), share_title.clone());
                                    move |_| workspace.open_share_dialog(store.clone(), asset, title.clone())
                                }
                            >
                                <Glyph icon=icondata::LuShare2 />
                            </button>
                        </Show>
                    }
                })}
                <button
                    type="button"
                    id="asset-panel-full"
                    class=BUTTON
                    title=move || if full.get() { "Beside the chat" } else { "Full screen" }
                    aria-label=move || if full.get() { "Beside the chat" } else { "Full screen" }
                    aria-pressed=move || full.get().to_string()
                    on:click=move |_| full.update(|full| *full = !*full)
                >
                    <Glyph icon=Signal::derive(move || {
                        if full.get() {
                            icondata::LuMinimize2
                        } else {
                            icondata::LuMaximize2
                        }
                    }) />
                </button>
                <button
                    type="button"
                    id="asset-panel-download"
                    class=BUTTON
                    title="Download"
                    aria-label="Download"
                    on:click=move |_| crate::assets::download_source(workspace, origin.clone(), file_title.clone())
                >
                    <Glyph icon=icondata::LuDownload />
                </button>
                <button
                    type="button"
                    id="asset-panel-close"
                    class=BUTTON
                    title="Close (Esc)"
                    aria-label="Close"
                    on:click=move |_| close()
                >
                    <Glyph icon=icondata::LuX />
                </button>
            </div>
            // A page's frame would otherwise take the pointer from the drag.
            <div
                node_ref=body
                class="flex min-h-0 flex-1 flex-col overflow-auto"
                class:pointer-events-none=move || dragging.get()
                id="asset-panel-body"
            ></div>
        </aside>
    }
}
