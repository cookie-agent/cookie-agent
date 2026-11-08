//! The asset page (`docs/site/specs/asset-opening.md` section 5): an asset
//! that is not a picture or a video, opened from the Assets tab in place of
//! the list. Its head has Back, the title, the handle, where the asset
//! lives, *Preview* and *Source*, steps to the list's other such assets,
//! and its actions; its body is the asset whole, an HTML page filling it.

use cookie_agent_protocol::{Asset, AssetStore};
use leptos::{ev, html, prelude::*};
use wasm_bindgen::JsCast as _;

use crate::{assets::OpenAsset, icons::Glyph, layout::View, workspace::Workspace};

const BUTTON: &str = "inline-flex min-h-9 min-w-9 shrink-0 items-center justify-center gap-1 rounded-md px-2 text-xs text-fg-2 hover:bg-hover hover:text-fg active:bg-press disabled:opacity-35 disabled:hover:bg-transparent";
const TAB: &str = "inline-flex min-h-8 items-center rounded-md px-2.5 text-xs";
const TAB_ON: &str = "bg-sunken text-fg shadow-[0_0_0_1px_var(--color-line)]";
const TAB_OFF: &str = "text-muted hover:text-fg";

/// The page of `workspace.asset_page`, while it holds an asset.
#[component]
pub(crate) fn AssetPage(workspace: Workspace) -> impl IntoView {
    move || {
        workspace
            .asset_page
            .get()
            .map(|asset| view! { <Page workspace asset /> })
    }
}

#[component]
fn Page(workspace: Workspace, asset: Asset) -> impl IntoView {
    let open = OpenAsset::of_asset(&asset);
    let source_only = open.source_only();
    let source = RwSignal::new(source_only);
    let full = RwSignal::new(false);
    let root = NodeRef::<html::Section>::new();
    let body = NodeRef::<html::Div>::new();
    let close = move || workspace.asset_page.set(None);
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
    Effect::new(move |_| {
        if let Some(root) = root.get() {
            let _ = root.focus();
        }
    });
    // The other assets of the list that open on a page, to step through.
    let id = asset.id;
    let deck = Memo::new(move |_| {
        super::assets::shown_assets(workspace)
            .into_iter()
            .filter(|shown| super::assets::on_page(&shown.media_type))
            .collect::<Vec<_>>()
    });
    let at = Memo::new(move |_| deck.with(|deck| deck.iter().position(|shown| shown.id == id)));
    let step = move |by: isize| {
        let next = deck.with_untracked(|deck| {
            at.get_untracked()
                .and_then(|at| at.checked_add_signed(by))
                .and_then(|next| deck.get(next).cloned())
        });
        if next.is_some() {
            workspace.asset_page.set(next);
        }
    };
    let stepping = move || deck.with(|deck| deck.len() > 1) && at.get().is_some();
    // Whether it is the last of them; outside `view!`, whose tags a `>`
    // would end.
    let at_end = move || at.get().is_none_or(|at| at + 1 >= deck.with(Vec::len));
    let reload = move || {
        if let Some(frame) = body
            .get_untracked()
            .and_then(|body| body.query_selector("[data-asset-frame]").ok().flatten())
        {
            // Loading the viewer again sends it the page again.
            let _ = frame.set_attribute("src", "viewer.html");
        }
    };
    let on_keydown = move |event: ev::KeyboardEvent| {
        if event.ctrl_key() || event.meta_key() || event.alt_key() {
            return;
        }
        let in_field = event
            .target()
            .and_then(|target| target.dyn_into::<web_sys::Element>().ok())
            .is_some_and(|target| {
                target
                    .closest("input, textarea, select, [contenteditable], [role=menu]")
                    .ok()
                    .flatten()
                    .is_some()
            });
        let handled = match event.key().as_str() {
            "Escape" if full.get_untracked() => {
                full.set(false);
                true
            }
            "Escape" => {
                close();
                true
            }
            "j" if !in_field => {
                step(1);
                true
            }
            "k" if !in_field => {
                step(-1);
                true
            }
            _ => false,
        };
        if handled {
            event.prevent_default();
        }
    };
    let kind = cookie_agent_client_state::assets::AssetKind::of(&asset.media_type);
    let has_tabs = open.has_source() && !source_only;
    let fills = matches!(
        asset.media_type.as_str(),
        "text/html" | "text/markdown" | "application/pdf"
    );
    let is_html = asset.media_type == "text/html";
    let place = super::assets::place_of(workspace, &asset);
    let age = cookie_agent_protocol::memory_age(asset.updated.at, jiff::Timestamp::now());
    let lives = match place.session {
        Some((session, title)) => view! {
            <button
                type="button"
                class="min-w-0 truncate underline decoration-dotted underline-offset-2 hover:text-fg"
                data-asset-page-session
                on:click=move |_| {
                    workspace.asset_page.set(None);
                    workspace.open_session(session);
                    workspace.open_view(View::Chat, None);
                }
            >
                {title.unwrap_or_else(|| "its session".to_owned())}
            </button>
        }
        .into_any(),
        // The shared store's assets are kept for sessions in no project.
        None if matches!(asset.store, AssetStore::Shared) => place.project.into_any(),
        None => format!("{} project", place.project).into_any(),
    };
    let menu_open = RwSignal::new(None::<()>);
    let items = {
        let asset = asset.clone();
        move || {
            super::assets::menu_items(workspace, &asset, None)
                .into_iter()
                .filter(|item| matches!(item.id, "rename" | "session" | "delete"))
                .collect::<Vec<_>>()
        }
    };
    let menu = super::row_menu::row_menu(
        menu_open,
        (),
        "asset-page".to_owned(),
        asset.title.clone(),
        BUTTON,
        items,
    );
    let handle = asset.handle.to_string();
    let copy = handle.clone();
    let share: Option<(AssetStore, _, String)> =
        Some((asset.store.clone(), asset.id, asset.title.clone()));
    let downloaded = asset.clone();
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
            "fixed inset-0 z-50 flex flex-col bg-surface outline-none"
        } else {
            "flex min-h-0 min-w-0 flex-1 flex-col outline-none"
        }
    };
    view! {
        <section
            node_ref=root
            id="asset-page"
            aria-label=asset.title.clone()
            tabindex="-1"
            class=class
            data-full=move || full.get().to_string()
            on:keydown=on_keydown
        >
            <div class="flex min-w-0 shrink-0 flex-wrap items-center gap-0.5 border-b border-line px-2 py-1.5">
                <button
                    type="button"
                    class=BUTTON
                    id="asset-page-back"
                    aria-label="Back to the assets"
                    title="Back (Esc)"
                    on:click=move |_| close()
                >
                    <Glyph icon=icondata::LuArrowLeft class="text-base" />
                </button>
                <span class="flex min-w-0 flex-[1_1_12rem] flex-col px-1.5">
                    <span class="flex min-w-0 items-center gap-1.5 text-sm font-semibold">
                        <Glyph icon=crate::icons::asset_icon(kind) class="shrink-0 text-accent" />
                        <span class="truncate" id="asset-page-title">{asset.title.clone()}</span>
                    </span>
                    <span class="flex min-w-0 items-center gap-1 text-[0.6875rem] text-muted">
                        <span class="shrink-0 font-mono select-all" id="asset-page-handle">{handle.clone()}</span>
                        <span>"·"</span>
                        {lives}
                        <span class="shrink-0">{format!("· {age} ago")}</span>
                    </span>
                </span>
                <Show when=move || has_tabs>
                    <span class="flex shrink-0 gap-0.5 rounded-lg p-0.5 shadow-[inset_0_0_0_1px_var(--color-line-2)]" role="tablist">
                        <button type="button" role="tab" id="asset-page-preview" class=tab(false) aria-selected=move || (!source.get()).to_string() on:click=move |_| source.set(false)>
                            "Preview"
                        </button>
                        <button type="button" role="tab" id="asset-page-source" class=tab(true) aria-selected=move || source.get().to_string() on:click=move |_| source.set(true)>
                            "Source"
                        </button>
                    </span>
                    <span class="mx-1 hidden h-5 w-px bg-line-2 sm:block"></span>
                </Show>
                <Show when=stepping>
                    <span class="hidden shrink-0 items-center sm:flex">
                        <button
                            type="button"
                            class=BUTTON
                            id="asset-page-previous"
                            aria-label="Previous"
                            title="Previous (k)"
                            disabled=move || at.get() == Some(0)
                            on:click=move |_| step(-1)
                        >
                            <Glyph icon=icondata::LuChevronUp />
                        </button>
                        <span class="px-0.5 font-mono text-[0.6875rem] text-muted tabular-nums" id="asset-page-position">
                            {move || format!("{}/{}", at.get().unwrap_or(0) + 1, deck.with(Vec::len))}
                        </span>
                        <button
                            type="button"
                            class=BUTTON
                            id="asset-page-next"
                            aria-label="Next"
                            title="Next (j)"
                            disabled=at_end
                            on:click=move |_| step(1)
                        >
                            <Glyph icon=icondata::LuChevronDown />
                        </button>
                        <span class="mx-1 h-5 w-px bg-line-2"></span>
                    </span>
                </Show>
                <Show when=move || is_html>
                    <button type="button" class=BUTTON id="asset-page-reload" aria-label="Reload the page" title="Run the page again" on:click=move |_| reload()>
                        <Glyph icon=icondata::LuRotateCw />
                    </button>
                </Show>
                <button
                    type="button"
                    class=BUTTON
                    id="asset-page-copy"
                    aria-label="Copy the handle"
                    title=format!("Copy {handle}")
                    on:click=move |_| crate::assets::copy_handle(copy.clone(), workspace)
                >
                    <Glyph icon=icondata::LuCopy />
                </button>
                {share.map(|(store, id, title)| view! {
                    <Show when=move || workspace.has_shares.get()>
                        <button
                            type="button"
                            class=BUTTON
                            id="asset-page-share"
                            aria-label="Share link"
                            title="Share link"
                            on:click={
                                let (store, title) = (store.clone(), title.clone());
                                move |_| workspace.open_share_dialog(store.clone(), id, title.clone())
                            }
                        >
                            <Glyph icon=icondata::LuShare2 />
                        </button>
                    </Show>
                })}
                <button
                    type="button"
                    class=BUTTON
                    id="asset-page-download"
                    aria-label="Download"
                    title="Download"
                    on:click=move |_| crate::assets::download(workspace, downloaded.clone())
                >
                    <Glyph icon=icondata::LuDownload />
                </button>
                <Show when=move || fills>
                    <button
                        type="button"
                        class=BUTTON
                        id="asset-page-full"
                        aria-label=move || if full.get() { "Leave full screen" } else { "Full screen" }
                        title=move || if full.get() { "Leave full screen (Esc)" } else { "Full screen" }
                        aria-pressed=move || full.get().to_string()
                        on:click=move |_| full.update(|full| *full = !*full)
                    >
                        <Glyph icon=Signal::derive(move || {
                            if full.get() { icondata::LuMinimize2 } else { icondata::LuMaximize2 }
                        }) />
                    </button>
                </Show>
                {menu}
            </div>
            <div class="flex min-h-0 flex-1 flex-col bg-surface-2 sm:p-3">
                <div
                    node_ref=body
                    class="flex min-h-0 flex-1 flex-col overflow-auto bg-raised sm:rounded-xl sm:shadow-[0_0_0_1px_var(--color-line),0_1px_2px_rgb(0_0_0/0.03)]"
                    id="asset-page-body"
                ></div>
            </div>
        </section>
    }
}
