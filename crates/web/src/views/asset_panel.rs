//! The side panel (`docs/site/specs/assets.md` section 8.1): an HTML page
//! or a Markdown document a shelf's card opened, beside the transcript on a
//! desktop's width and over it, full screen, on narrower windows. Its head
//! has the asset's title and handle, *Preview* and *Source*, *Copy handle*,
//! *Full screen*, *Download*, and *Close* (Escape too).

use leptos::{html, prelude::*};

use crate::{assets::OpenAsset, icons::Glyph, workspace::Workspace};

const BUTTON: &str = "inline-flex min-h-9 min-w-9 shrink-0 items-center justify-center gap-1 rounded-md px-2 text-xs hover:bg-toasted active:bg-crust dark:hover:bg-ember dark:active:bg-smoke";
const TAB: &str = "inline-flex min-h-8 items-center rounded-md border px-2 text-xs";
const TAB_ON: &str =
    "border-espresso bg-espresso text-cream dark:border-maple dark:bg-maple dark:text-night";
const TAB_OFF: &str = "border-crust hover:bg-toasted dark:border-smoke dark:hover:bg-ember";

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

#[component]
fn Panel(workspace: Workspace, open: OpenAsset) -> impl IntoView {
    let source = RwSignal::new(false);
    let full = RwSignal::new(false);
    let body = NodeRef::<html::Div>::new();
    let root = NodeRef::<html::Aside>::new();
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
    let (store, asset) = (open.store.clone(), open.asset);
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
            "fixed inset-0 z-50 flex flex-col bg-parchment outline-none dark:bg-char"
        } else {
            "fixed inset-0 z-40 flex flex-col bg-parchment outline-none lg:static lg:z-auto lg:w-[min(48rem,48%)] lg:shrink-0 lg:border-l lg:border-toasted dark:bg-char lg:dark:border-ember"
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
            <div class="flex min-w-0 shrink-0 flex-wrap items-center gap-1 border-b border-toasted px-2 py-1.5 dark:border-ember">
                <span class="flex min-w-0 flex-1 flex-col px-1">
                    <span class="truncate text-sm font-bold" id="asset-panel-title">{title}</span>
                    <span class="truncate font-mono text-[0.6875rem] text-ink select-all dark:text-sand" id="asset-panel-handle">
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
                <button
                    type="button"
                    id="asset-panel-copy"
                    class=BUTTON
                    title="Copy the handle"
                    aria-label="Copy the handle"
                    on:click=move |_| crate::assets::copy_handle(copy.clone(), workspace)
                >
                    <Glyph icon=icondata::FaCopyRegular />
                </button>
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
                            icondata::FaCompressSolid
                        } else {
                            icondata::FaExpandSolid
                        }
                    }) />
                </button>
                <button
                    type="button"
                    id="asset-panel-download"
                    class=BUTTON
                    title="Download"
                    aria-label="Download"
                    on:click=move |_| crate::assets::download_stored(workspace, store.clone(), asset)
                >
                    <Glyph icon=icondata::FaDownloadSolid />
                </button>
                <button
                    type="button"
                    id="asset-panel-close"
                    class=BUTTON
                    title="Close (Esc)"
                    aria-label="Close"
                    on:click=move |_| close()
                >
                    <Glyph icon=icondata::FaXmarkSolid />
                </button>
            </div>
            <div node_ref=body class="flex min-h-0 flex-1 flex-col overflow-auto" id="asset-panel-body"></div>
        </aside>
    }
}
