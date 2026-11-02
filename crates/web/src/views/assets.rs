//! The Assets tab (`docs/site/specs/assets.md` section 8.3): a project's
//! assets, or those of sessions in no project, newest first; filtered by
//! kind and searched by title, each opened as a figure shows it, renamed,
//! downloaded, or deleted. A session's own assets are never listed here:
//! they belong to its conversation.

use cookie_agent_client_state::assets::{AssetKind, assets_matching, total_bytes};
use cookie_agent_protocol::{Asset, AssetStore, human_bytes};
use leptos::prelude::*;

use super::{BUTTON, ERROR, NOTE, empty_state, loading};
use crate::{icons::Glyph, workspace::Workspace};

const SELECT: &str = "min-h-11 min-w-0 rounded-md border border-crust bg-cream px-2 text-sm hover:border-caramel focus:border-caramel focus:outline-none dark:border-smoke dark:bg-night dark:hover:border-maple dark:focus:border-maple";
const ROW_BUTTON: &str = "inline-flex min-h-9 items-center gap-1 rounded-md px-2 text-xs hover:bg-toasted active:bg-crust dark:hover:bg-ember dark:active:bg-smoke";

/// A kind's icon.
fn kind_icon(kind: AssetKind) -> crate::icons::Icon {
    match kind {
        AssetKind::Image => icondata::FaImageSolid,
        AssetKind::Page => icondata::FaCodeSolid,
        AssetKind::Document => icondata::FaFileLinesSolid,
        AssetKind::Media => icondata::FaFilmSolid,
        AssetKind::Other => icondata::FaFileSolid,
    }
}

/// The `<select>` value of a store.
fn store_value(store: &AssetStore) -> String {
    match store {
        AssetStore::Project { project } => project.to_string(),
        AssetStore::Shared | AssetStore::Session { .. } => "shared".to_owned(),
    }
}

pub fn assets_page(workspace: Workspace) -> impl IntoView {
    let store = move || workspace.asset_store.get();
    let choose = move |value: String| {
        let chosen = if value == "shared" {
            AssetStore::Shared
        } else {
            match value.parse() {
                Ok(project) => AssetStore::Project { project },
                Err(_) => return,
            }
        };
        workspace.choose_asset_store(chosen);
    };
    let options = move || {
        let current = store().map(|store| store_value(&store)).unwrap_or_default();
        let projects = workspace.projects.get();
        let shared = current == "shared";
        let mut options = vec![
            view! {
                <option value="shared" selected=shared>"No project"</option>
            }
            .into_any(),
        ];
        options.extend(projects.into_iter().map(|project| {
            let value = project.id.to_string();
            let selected = value == current;
            view! { <option value=value selected=selected>{project.name.to_string()}</option> }
                .into_any()
        }));
        options.collect_view()
    };
    let kind_button = move |kind: Option<AssetKind>| {
        let chosen = move || workspace.asset_kind.get() == kind;
        let label = kind.map_or("All", AssetKind::label);
        view! {
            <button
                type="button"
                role="radio"
                class=move || format!(
                    "inline-flex min-h-9 items-center rounded-full border px-3 text-sm {}",
                    if chosen() {
                        "border-espresso bg-espresso text-cream dark:border-latte dark:bg-latte dark:text-night"
                    } else {
                        "border-crust hover:bg-toasted active:bg-crust dark:border-smoke dark:hover:bg-ember dark:active:bg-smoke"
                    },
                )
                aria-checked=move || chosen().to_string()
                data-asset-kind=label
                on:click=move |_| workspace.asset_kind.set(kind)
            >
                {label}
            </button>
        }
    };
    let list = move || {
        match workspace.asset_list.get() {
        None => loading("Reading the assets…").into_any(),
        Some(Err(message)) => view! { <p class=ERROR>{message}</p> }.into_any(),
        Some(Ok(assets)) if assets.is_empty() => empty_state(
            icondata::FaImagesSolid,
            "No assets here yet. An agent saves one for the project with write_asset and scope project.",
        )
        .into_any(),
        Some(Ok(assets)) => {
            let query = workspace.asset_query.get();
            let shown = assets_matching(&assets, &query, workspace.asset_kind.get())
                .into_iter()
                .cloned()
                .collect::<Vec<_>>();
            if shown.is_empty() {
                return empty_state(icondata::FaMagnifyingGlassSolid, "No assets match.")
                    .into_any();
            }
            let total = view! {
                <p class=NOTE data-assets-total>
                    {format!(
                        "{} asset{} · {}",
                        assets.len(),
                        if assets.len() == 1 { "" } else { "s" },
                        human_bytes(total_bytes(&assets))
                    )}
                </p>
            };
            view! {
                {total}
                <div class="flex flex-col">
                    {shown.into_iter().map(|asset| asset_row(workspace, asset)).collect_view()}
                </div>
            }
            .into_any()
        }
    }
    };
    // The figure of the asset opened, filled once it is drawn.
    Effect::new(move |_| {
        if workspace.asset_open.get().is_some() {
            request_animation_frame(move || {
                if let Some(root) = document().get_element_by_id("assets-page") {
                    crate::assets::show_within(&root, workspace);
                }
            });
        }
    });
    view! {
        <div class="flex flex-col gap-2" id="assets-page" data-assets-page>
            <p class=NOTE>
                "Files agents saved for a project, which every session of the project can use. What an agent saves for its conversation alone stays in that conversation."
            </p>
            <div class="flex flex-wrap items-center gap-2">
                <label class="flex min-w-0 items-center gap-2 text-sm">
                    "Project"
                    <select
                        id="assets-store"
                        class=SELECT
                        on:change=move |event| choose(event_target_value(&event))
                    >
                        {options}
                    </select>
                </label>
            </div>
            <div class="flex flex-wrap gap-1.5" role="radiogroup" aria-label="Kind">
                {kind_button(None)}
                {AssetKind::ALL.into_iter().map(|kind| kind_button(Some(kind))).collect_view()}
            </div>
            <label class="relative flex items-center">
                <Glyph icon=icondata::FaMagnifyingGlassSolid class="pointer-events-none absolute left-3 text-xs text-ink dark:text-sand" />
                <input
                    type="text"
                    role="searchbox"
                    id="assets-search"
                    class="min-h-11 w-full min-w-0 rounded-md border border-crust bg-cream pr-3 pl-9 text-sm text-espresso hover:border-caramel focus:border-caramel focus:outline-none dark:border-smoke dark:bg-night dark:text-latte dark:hover:border-maple dark:focus:border-maple"
                    aria-label="Search assets by title or file name"
                    placeholder="Search by title or file name"
                    prop:value=move || workspace.asset_query.get_untracked()
                    on:input=move |event| workspace.asset_query.set(event_target_value(&event))
                />
            </label>
            {list}
        </div>
    }
}

/// Who last saved `asset`, and when: the session's title when the page
/// lists it.
fn saved_by(
    workspace: Workspace,
    asset: &Asset,
) -> (String, Option<cookie_agent_protocol::SessionId>) {
    let session = asset.updated.session;
    let title = workspace.sessions.with_untracked(|sessions| {
        sessions
            .iter()
            .find(|meta| meta.session_id == session)
            .map(|meta| {
                meta.title.as_ref().map_or_else(
                    || meta.short_id.clone().unwrap_or_else(|| session.to_string()),
                    |title| title.as_str().to_owned(),
                )
            })
    });
    let age = cookie_agent_protocol::memory_age(asset.updated.at, jiff::Timestamp::now());
    match title {
        Some(title) => (format!("saved by {title}, {age} ago"), Some(session)),
        None => (format!("saved by another session, {age} ago"), None),
    }
}

/// One asset: its kind, title, and size; who saved it and when; open,
/// rename, download, and delete.
fn asset_row(workspace: Workspace, asset: Asset) -> impl IntoView {
    let kind = AssetKind::of(&asset.media_type);
    let id = asset.id;
    let store = asset.store.clone();
    let (saved, session) = saved_by(workspace, &asset);
    let open = move || workspace.asset_open.get() == Some(id);
    let renaming = move || {
        workspace
            .asset_renaming
            .with(|renaming| renaming.as_ref().is_some_and(|(asset, _)| *asset == id))
    };
    let deleting = move || workspace.asset_deleting.get() == Some(id);
    let block = asset.block();
    let title = asset.title.clone();
    let rename_store = store.clone();
    let delete_store = store.clone();
    let download_asset = asset.clone();
    let saved_link = match session {
        Some(session) => view! {
            <button
                type="button"
                class=format!("{NOTE} underline decoration-dotted underline-offset-2 hover:text-espresso dark:hover:text-latte")
                data-asset-session=session.to_string()
                on:click=move |_| {
                    workspace.open_session(session);
                    workspace.open_view(crate::layout::View::Chat, None);
                }
            >
                {saved.clone()}
            </button>
        }
        .into_any(),
        None => view! { <span class=NOTE>{saved.clone()}</span> }.into_any(),
    };
    view! {
        <div class="flex flex-col gap-1 border-b border-toasted py-2 dark:border-ember" data-asset-row=asset.handle.to_string()>
            <div class="flex min-w-0 flex-wrap items-center gap-2">
                <Glyph icon=kind_icon(kind) class="text-caramel dark:text-maple" />
                <span class="min-w-0 flex-1 truncate text-sm font-bold" data-asset-row-title>{title.clone()}</span>
                <span class=NOTE>{format!("{} · {}", asset.media_type, human_bytes(asset.byte_length))}</span>
            </div>
            <div class="flex min-w-0 flex-wrap items-center gap-1">
                {saved_link}
                <span class="flex-1"></span>
                <button
                    type="button"
                    class=ROW_BUTTON
                    data-asset-action="open"
                    aria-expanded=move || open().to_string()
                    on:click=move |_| workspace.asset_open.set(if open() { None } else { Some(id) })
                >
                    <Glyph icon=icondata::FaEyeSolid />
                    {move || if open() { "Close" } else { "Open" }}
                </button>
                <button
                    type="button"
                    class=ROW_BUTTON
                    data-asset-action="rename"
                    on:click={
                        let title = title.clone();
                        move |_| workspace.asset_renaming.set(Some((id, title.clone())))
                    }
                >
                    <Glyph icon=icondata::FaPenSolid />
                    "Rename"
                </button>
                <button
                    type="button"
                    class=ROW_BUTTON
                    data-asset-action="download"
                    on:click=move |_| crate::assets::download(workspace, download_asset.clone())
                >
                    <Glyph icon=icondata::FaDownloadSolid />
                    "Download"
                </button>
                <button
                    type="button"
                    class=ROW_BUTTON
                    data-asset-action="delete"
                    on:click=move |_| workspace.asset_deleting.set(Some(id))
                >
                    <Glyph icon=icondata::FaTrashSolid />
                    "Delete"
                </button>
            </div>
            <Show when=renaming>
                {
                    let rename_store = rename_store.clone();
                    view! {
                        <form
                            class="flex flex-wrap items-center gap-2"
                            data-asset-rename
                            on:submit={
                                let rename_store = rename_store.clone();
                                move |event: leptos::ev::SubmitEvent| {
                                    event.prevent_default();
                                    let title = workspace
                                        .asset_renaming
                                        .get_untracked()
                                        .map(|(_, title)| title)
                                        .unwrap_or_default();
                                    workspace.rename_asset(rename_store.clone(), id, title);
                                }
                            }
                        >
                            <input
                                type="text"
                                id="asset-rename-title"
                                class="min-h-11 min-w-0 flex-1 rounded-md border border-crust bg-cream px-3 text-sm dark:border-smoke dark:bg-night"
                                maxlength="200"
                                aria-label="The asset's title"
                                prop:value=move || {
                                    workspace
                                        .asset_renaming
                                        .with(|renaming| renaming.as_ref().map(|(_, title)| title.clone()))
                                        .unwrap_or_default()
                                }
                                on:input=move |event| {
                                    let title = event_target_value(&event);
                                    workspace.asset_renaming.update(|renaming| {
                                        if let Some((_, current)) = renaming {
                                            *current = title;
                                        }
                                    });
                                }
                            />
                            <button type="submit" class=BUTTON id="asset-rename-save">"Save"</button>
                            <button type="button" class=BUTTON on:click=move |_| workspace.asset_renaming.set(None)>
                                "Cancel"
                            </button>
                        </form>
                    }
                }
            </Show>
            <Show when=deleting>
                {
                    let delete_store = delete_store.clone();
                    let title = title.clone();
                    view! {
                        <div class="flex flex-wrap items-center gap-2" data-asset-delete-confirm>
                            <p class="text-sm">
                                {format!("Delete {title}? Every place it was shown will say it was deleted.")}
                            </p>
                            <button
                                type="button"
                                class=format!("{BUTTON} border-red-700 text-red-800 dark:border-red-400 dark:text-red-300")
                                id="asset-delete-confirm"
                                on:click={
                                    let delete_store = delete_store.clone();
                                    move |_| workspace.delete_asset(delete_store.clone(), id)
                                }
                            >
                                "Delete"
                            </button>
                            <button type="button" class=BUTTON on:click=move |_| workspace.asset_deleting.set(None)>
                                "Keep"
                            </button>
                        </div>
                    }
                }
            </Show>
            <Show when=open>
                <div inner_html=crate::render::blocks::asset_html(&block) data-asset-preview></div>
            </Show>
        </div>
    }
}
