//! The Assets tab (`docs/site/specs/assets.md` section 8.3): the assets of
//! every project and session, newest first, narrowed by a filter (a
//! project, or none; a level, session or project) like the session list's,
//! by kind, and by a search of titles and file names; each opened as a
//! figure shows it, renamed, downloaded, or deleted.

use cookie_agent_client_state::assets::{
    AssetFilter, AssetKind, AssetPlace, asset_place, assets_matching, total_bytes,
};
use cookie_agent_protocol::{Asset, AssetProjectFilter, human_bytes};
use leptos::prelude::*;

use super::{BUTTON, ERROR, NOTE, empty_state, loading};
use crate::{icons::Glyph, workspace::Workspace};

const ROW_BUTTON: &str = "inline-flex min-h-9 items-center gap-1 rounded-md px-2 text-xs hover:bg-toasted active:bg-crust dark:hover:bg-ember dark:active:bg-smoke";
const CHIP: &str =
    "inline-flex min-h-9 max-w-full items-center gap-1.5 rounded-full border px-3 text-sm";
const CHIP_ON: &str =
    "border-espresso bg-espresso text-cream dark:border-latte dark:bg-latte dark:text-night";
const CHIP_OFF: &str = "border-crust hover:bg-toasted active:bg-crust dark:border-smoke dark:hover:bg-ember dark:active:bg-smoke";

/// A choice of the filter or the kinds: a chip, filled while chosen.
fn chip(
    label: String,
    chosen: Signal<bool>,
    choose: impl Fn() + Send + Sync + 'static,
    data: &'static str,
) -> impl IntoView {
    let value = label.clone();
    view! {
        <button
            type="button"
            role="radio"
            class=move || format!("{CHIP} {}", if chosen.get() { CHIP_ON } else { CHIP_OFF })
            aria-checked=move || chosen.get().to_string()
            data-asset-filter=data
            data-value=value
            on:click=move |_| choose()
        >
            <Show when=move || chosen.get()>
                <Glyph icon=icondata::FaCheckSolid class="text-xs" />
            </Show>
            <span class="truncate">{label}</span>
        </button>
    }
}

/// The filter: whose assets (every project's, no project's, or one
/// project's), and of which level.
fn filter_panel(workspace: Workspace) -> impl IntoView {
    let heading = "text-xs font-bold text-ink dark:text-sand";
    let project_chip =
        move |label: String, project: Option<AssetProjectFilter>, data: &'static str| {
            let wanted = project.clone();
            chip(
                label,
                Signal::derive(move || {
                    workspace
                        .asset_filter
                        .with(|filter| filter.project == wanted)
                }),
                move || {
                    let mut filter = workspace.asset_filter.get_untracked();
                    filter.project = project.clone();
                    workspace.choose_asset_filter(filter);
                },
                data,
            )
        };
    let projects = move || {
        workspace
            .projects
            .get()
            .into_iter()
            .map(|project| {
                project_chip(
                    project.name.to_string(),
                    Some(AssetProjectFilter::Project {
                        project: project.id.clone(),
                    }),
                    "project",
                )
            })
            .collect_view()
    };
    view! {
        <div
            id="assets-filter"
            role="dialog"
            aria-label="Filter assets"
            class="flex flex-col gap-3 rounded-lg border border-toasted bg-cream p-3 dark:border-ember dark:bg-night"
        >
            <div class="flex flex-col gap-1.5">
                <span class=heading>"Project"</span>
                <div class="flex flex-wrap gap-1.5" role="radiogroup" aria-label="Project">
                    {project_chip("All projects".into(), None, "project")}
                    {project_chip("No project".into(), Some(AssetProjectFilter::NoProject), "project")}
                    {projects}
                </div>
            </div>
            <div class="flex flex-col gap-1.5">
                <span class=heading>"Level"</span>
                <div class="flex flex-wrap gap-1.5" role="radiogroup" aria-label="Level">
                    {AssetFilter::LEVELS
                        .into_iter()
                        .map(|scope| {
                            chip(
                                AssetFilter::level_label(scope).to_owned(),
                                Signal::derive(move || workspace.asset_filter.with(|filter| filter.scope == scope)),
                                move || {
                                    let mut filter = workspace.asset_filter.get_untracked();
                                    filter.scope = scope;
                                    workspace.choose_asset_filter(filter);
                                },
                                "level",
                            )
                        })
                        .collect_view()}
                </div>
            </div>
        </div>
    }
}

/// What the filter lists, in words: "All projects · All levels".
fn filter_summary(workspace: Workspace) -> String {
    let filter = workspace.asset_filter.get();
    let project = match &filter.project {
        None => "All projects".to_owned(),
        Some(AssetProjectFilter::NoProject) => "No project".to_owned(),
        Some(AssetProjectFilter::Project { project }) => workspace
            .projects
            .with(|projects| {
                projects
                    .iter()
                    .find(|known| known.id == *project)
                    .map(|known| known.name.to_string())
            })
            .unwrap_or_else(|| project.to_string()),
    };
    format!("{project} · {}", AssetFilter::level_label(filter.scope))
}

pub fn assets_page(workspace: Workspace) -> impl IntoView {
    let kind_button = move |kind: Option<AssetKind>| {
        let label = kind.map_or("All", AssetKind::label);
        let chosen = move || workspace.asset_kind.get() == kind;
        view! {
            <button
                type="button"
                role="radio"
                class=move || format!("{CHIP} {}", if chosen() { CHIP_ON } else { CHIP_OFF })
                aria-checked=move || chosen().to_string()
                data-asset-kind=label
                on:click=move |_| workspace.asset_kind.set(kind)
            >
                {label}
            </button>
        }
    };
    let project_name = move |project: &cookie_agent_protocol::ProjectId| {
        workspace.projects.with_untracked(|projects| {
            projects
                .iter()
                .find(|known| known.id == *project)
                .map(|known| known.name.to_string())
        })
    };
    let list = move || match workspace.asset_list.get() {
        None => loading("Reading the assets…").into_any(),
        Some(Err(message)) => view! { <p class=ERROR>{message}</p> }.into_any(),
        Some(Ok(found)) if found.assets.is_empty() => empty_state(
            icondata::FaImagesSolid,
            "No assets here yet. Agents save them with write_asset.",
        )
        .into_any(),
        Some(Ok(found)) => {
            let query = workspace.asset_query.get();
            let shown = assets_matching(&found.assets, &query, workspace.asset_kind.get())
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
                        shown.len(),
                        if shown.len() == 1 { "" } else { "s" },
                        human_bytes(total_bytes(&shown))
                    )}
                </p>
            };
            let rows = workspace.asset_rows.get();
            let more = shown.len().saturating_sub(rows);
            let more = (more > 0).then(|| {
                view! {
                    <button
                        type="button"
                        class=BUTTON
                        id="assets-more"
                        on:click=move |_| {
                            workspace
                                .asset_rows
                                .update(|rows| *rows += crate::workspace::ASSET_ROWS);
                        }
                    >
                        {format!("Show more ({more} more)")}
                    </button>
                }
            });
            view! {
                {total}
                <div class="flex flex-col">
                    {shown
                        .into_iter()
                        .take(rows)
                        .map(|asset| {
                            let place = asset_place(&asset, &found.sessions, project_name);
                            asset_row(workspace, asset, place)
                        })
                        .collect_view()}
                </div>
                {more}
            }
            .into_any()
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
    let filter_open = move || workspace.asset_filter_open.get();
    let narrowed = move || workspace.asset_filter.with(AssetFilter::changes) > 0;
    view! {
        <div class="flex flex-col gap-2" id="assets-page" data-assets-page>
            <p class=NOTE>
                "Files agents saved: a session's own, and those kept for a project, which every session of the project can use."
            </p>
            <div class="flex min-w-0 items-center gap-2">
                <button
                    type="button"
                    class=format!("{BUTTON} relative")
                    id="assets-filter-toggle"
                    aria-label="Filter assets"
                    title="Filter assets"
                    aria-haspopup="dialog"
                    aria-expanded=move || filter_open().to_string()
                    aria-controls="assets-filter"
                    on:click=move |_| workspace.asset_filter_open.update(|open| *open = !*open)
                >
                    <Glyph icon=icondata::FaFilterSolid />
                    // A dot while the filter narrows the list.
                    <Show when=narrowed>
                        <span class="absolute top-1.5 right-1.5 h-2 w-2 rounded-full bg-caramel ring-2 ring-cream dark:bg-maple dark:ring-night" data-filter-dot></span>
                    </Show>
                </button>
                <span class=format!("min-w-0 flex-1 truncate {NOTE}") data-assets-summary>
                    {move || filter_summary(workspace)}
                </span>
            </div>
            <Show when=filter_open>
                {filter_panel(workspace)}
            </Show>
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
                    on:input=move |event| {
                        workspace.asset_query.set(event_target_value(&event));
                        workspace.asset_rows.set(crate::workspace::ASSET_ROWS);
                    }
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
fn asset_row(workspace: Workspace, asset: Asset, place: AssetPlace) -> impl IntoView {
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
    // Where it lives: its project, and a session's own its session.
    let level = if place.session.is_some() {
        "session"
    } else {
        "project"
    };
    let tree = place.session.map(|(session, title)| {
        // Untitled, as the session list names it.
        let title = title.unwrap_or_else(|| {
            workspace.sessions.with_untracked(|sessions| {
                sessions
                    .iter()
                    .find(|meta| meta.session_id == session)
                    .map_or_else(|| "Untitled session".to_owned(), super::session_label)
            })
        });
        view! {
            <span class=NOTE>"·"</span>
            <button
                type="button"
                class=format!("{NOTE} min-w-0 truncate underline decoration-dotted underline-offset-2 hover:text-espresso dark:hover:text-latte")
                data-asset-tree=session.to_string()
                title="Open the session"
                on:click=move |_| {
                    workspace.open_session(session);
                    workspace.open_view(crate::layout::View::Chat, None);
                }
            >
                {title}
            </button>
        }
    });
    view! {
        <div class="flex flex-col gap-1 border-b border-toasted py-2 dark:border-ember" data-asset-row=asset.handle.to_string()>
            <div class="flex min-w-0 flex-wrap items-center gap-2">
                <Glyph icon=crate::icons::asset_icon(kind) class="text-caramel dark:text-maple" />
                <span class="min-w-0 flex-1 truncate text-sm font-bold" data-asset-row-title>{title.clone()}</span>
                <span class=NOTE>{format!("{} · {}", asset.media_type, human_bytes(asset.byte_length))}</span>
            </div>
            <div class="flex min-w-0 items-center gap-1" data-asset-place=level>
                <Glyph
                    icon=if level == "session" { icondata::FaCommentsSolid } else { icondata::FaFolderSolid }
                    class="text-xs text-ink dark:text-sand"
                />
                <span class=format!("{NOTE} shrink-0") data-asset-project>{place.project}</span>
                {tree}
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
