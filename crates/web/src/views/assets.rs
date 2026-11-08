//! The Assets tab (`docs/site/specs/asset-opening.md` section 3): the assets
//! of every project and session, newest first, narrowed by a filter (a
//! project, or none; a level, session or project) like the session list's,
//! by kind, and by a search of titles and file names; shown as a grid of
//! cards or a list, whose thumbnails are read once they come into view. A
//! click opens a picture or a video in the media viewer and anything else
//! on its own page; ⋯ or a right-click offers the rest.

use cookie_agent_client_state::assets::{
    AssetFilter, AssetKind, AssetPlace, asset_place, assets_matching, total_bytes,
};
use cookie_agent_protocol::{Asset, AssetProjectFilter, SessionId, human_bytes};
use leptos::{ev, html, prelude::*};
use wasm_bindgen::{JsCast as _, closure::Closure};

use super::{BUTTON, ERROR, NOTE, PRIMARY, empty_state, loading, row_menu::MenuItem};
use crate::{icons::Glyph, layout::View, workspace::Workspace};

const CHIP: &str =
    "inline-flex min-h-9 max-w-full items-center gap-1.5 rounded-full border px-3 text-sm";
const CHIP_ON: &str = "border-fg bg-accent text-on-accent";
const CHIP_OFF: &str = "border-line-2 hover:bg-hover active:bg-press";
/// A card, which holds the box and, outside it, the corner's buttons: the
/// box's lift on hover would otherwise carry the ⋯ menu inside it.
const CARD: &str = "group relative min-w-0 cursor-pointer rounded-xl text-left outline-none focus-visible:ring-2 focus-visible:ring-accent";
const CARD_BOX: &str = "flex h-full flex-col overflow-hidden rounded-xl bg-raised shadow-[0_0_0_1px_var(--color-line),0_1px_2px_rgb(0_0_0/0.03)] transition-[translate,box-shadow] group-hover:-translate-y-px group-hover:shadow-[0_0_0_1px_var(--color-line-2),0_6px_18px_-6px_rgb(0_0_0/0.18)]";
const CORNER: &str = "inline-flex size-8 items-center justify-center rounded-lg bg-raised/90 text-fg shadow-[0_1px_3px_rgb(0_0_0/0.2)] hover:bg-raised";
/// How far below the visible list a card's thumbnail is read ahead.
const READ_AHEAD: &str = "100% 0px";

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
                <Glyph icon=icondata::LuCheck class="text-xs" />
            </Show>
            <span class="truncate">{label}</span>
        </button>
    }
}

/// The filter: whose assets (every project's, no project's, or one
/// project's), and of which level.
fn filter_panel(workspace: Workspace) -> impl IntoView {
    let heading = "text-xs font-bold text-muted";
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
            class="flex flex-col gap-3 rounded-xl p-3 bg-raised shadow-[0_0_0_1px_var(--color-line),0_1px_2px_rgb(0_0_0/0.03)]"
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

/// The assets the tab lists now, as its kind and search narrow them, in
/// its order: every one, not only the rows shown.
pub(super) fn shown_assets(workspace: Workspace) -> Vec<Asset> {
    let query = workspace.asset_query.get();
    let kind = workspace.asset_kind.get();
    workspace.asset_list.with(|list| match list {
        Some(Ok(found)) => assets_matching(&found.assets, &query, kind)
            .into_iter()
            .cloned()
            .collect(),
        _ => Vec::new(),
    })
}

/// Whether the media viewer shows a file of `media_type`.
fn in_viewer(media_type: &str) -> bool {
    media_type.starts_with("image/") || media_type.starts_with("video/")
}

/// Whether an asset of `media_type` opens on its own page.
pub(super) fn on_page(media_type: &str) -> bool {
    !in_viewer(media_type)
}

/// Opens `asset`, one of `shown`: a picture or a video in the media viewer,
/// to step through every picture and video of `shown`; anything else on
/// its page.
fn open(workspace: Workspace, asset: &Asset, shown: &[Asset]) {
    if !in_viewer(&asset.media_type) {
        workspace.asset_page.set(Some(asset.clone()));
        return;
    }
    let group = shown
        .iter()
        .filter(|shown| in_viewer(&shown.media_type))
        .map(crate::assets::viewing_of)
        .collect::<Vec<_>>();
    let start = group.iter().position(|viewing| {
        viewing
            .asset
            .as_ref()
            .is_some_and(|(_, id)| *id == asset.id)
    });
    let first = start.and_then(|start| group.get(start).cloned());
    workspace.media_group.set(group);
    workspace.media_viewer.set(first);
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
    let list_layout = move || workspace.asset_list_layout.get();
    let list = move || match workspace.asset_list.get() {
        None => loading("Reading the assets…").into_any(),
        Some(Err(message)) => view! { <p class=ERROR>{message}</p> }.into_any(),
        Some(Ok(found)) if found.assets.is_empty() => empty_state(
            icondata::LuImages,
            "No assets here yet. Agents save them with write_asset.",
        )
        .into_any(),
        Some(Ok(found)) => {
            let shown = shown_assets(workspace);
            if shown.is_empty() {
                return empty_state(icondata::LuSearch, "No assets match.").into_any();
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
            let shown = std::sync::Arc::new(shown);
            let as_list = list_layout();
            let items = shown
                .iter()
                .take(rows)
                .map(|asset| {
                    let place = asset_place(asset, &found.sessions, project_name);
                    item(
                        workspace,
                        asset.clone(),
                        place,
                        std::sync::Arc::clone(&shown),
                        as_list,
                    )
                })
                .collect_view();
            let class = if as_list {
                "flex flex-col"
            } else {
                "grid grid-cols-2 gap-2.5 sm:grid-cols-[repeat(auto-fill,minmax(13rem,1fr))] sm:gap-3.5"
            };
            view! {
                {total}
                <div class=class data-assets-layout=if as_list { "list" } else { "grid" }>{items}</div>
                {more}
            }
            .into_any()
        }
    };
    let observer = lazy_thumbnails(workspace);
    let filter_open = move || workspace.asset_filter_open.get();
    let narrowed = move || workspace.asset_filter.with(AssetFilter::changes) > 0;
    let layout_button = move |as_list: bool| {
        let (label, icon) = if as_list {
            ("List", icondata::LuList)
        } else {
            ("Grid", icondata::LuLayoutGrid)
        };
        view! {
            <button
                type="button"
                role="radio"
                class=move || {
                    format!(
                        "inline-flex size-8 items-center justify-center rounded-md {}",
                        if list_layout() == as_list {
                            "bg-sunken text-fg shadow-[0_0_0_1px_var(--color-line)]"
                        } else {
                            "text-muted hover:text-fg"
                        }
                    )
                }
                aria-checked=move || (list_layout() == as_list).to_string()
                aria-label=label
                title=label
                data-assets-layout-choice=label
                on:click=move |_| workspace.choose_asset_layout(as_list)
            >
                <Glyph icon=icon />
            </button>
        }
    };
    view! {
        <div class="flex flex-col gap-2" id="assets-page" data-assets-page node_ref=observer>
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
                    <Glyph icon=icondata::LuListFilter />
                    // A dot while the filter narrows the list.
                    <Show when=narrowed>
                        <span class="absolute top-1.5 right-1.5 h-2 w-2 rounded-full bg-accent ring-2 ring-surface" data-filter-dot></span>
                    </Show>
                </button>
                <label class="relative flex min-w-0 flex-1 items-center">
                    <Glyph icon=icondata::LuSearch class="pointer-events-none absolute left-3 text-xs text-muted" />
                    <input
                        type="text"
                        role="searchbox"
                        id="assets-search"
                        class="min-h-11 w-full min-w-0 rounded-lg bg-surface pr-3 pl-9 text-sm text-fg hover:shadow-[inset_0_0_0_1px_var(--color-faint)] focus:shadow-[inset_0_0_0_1px_var(--color-accent),0_0_0_3px_var(--color-accent-soft)] focus:outline-none shadow-[inset_0_0_0_1px_var(--color-line-2)] lg:min-h-9"
                        aria-label="Search assets by title or file name"
                        placeholder="Search by title or file name"
                        prop:value=move || workspace.asset_query.get_untracked()
                        on:input=move |event| {
                            workspace.asset_query.set(event_target_value(&event));
                            workspace.asset_rows.set(crate::workspace::ASSET_ROWS);
                        }
                    />
                </label>
                <span class="inline-flex shrink-0 gap-0.5 rounded-lg p-0.5 shadow-[inset_0_0_0_1px_var(--color-line-2)]" role="radiogroup" aria-label="Layout">
                    {layout_button(false)}
                    {layout_button(true)}
                </span>
            </div>
            <Show when=filter_open>
                {filter_panel(workspace)}
            </Show>
            <div class="flex flex-wrap gap-1.5" role="radiogroup" aria-label="Kind">
                {kind_button(None)}
                {AssetKind::ALL.into_iter().map(|kind| kind_button(Some(kind))).collect_view()}
            </div>
            <span class=format!("min-w-0 truncate {NOTE}") data-assets-summary>
                {move || filter_summary(workspace)}
            </span>
            {list}
        </div>
    }
}

/// Reads each card's thumbnail once it comes within a screen of the view:
/// the returned node is the tab, whose cards it watches as they are drawn.
fn lazy_thumbnails(workspace: Workspace) -> NodeRef<html::Div> {
    let root = NodeRef::<html::Div>::new();
    let observer = StoredValue::new_local(
        None::<(
            web_sys::IntersectionObserver,
            Closure<dyn Fn(js_sys::Array, web_sys::IntersectionObserver)>,
        )>,
    );
    let made = move || {
        if let Some(existing) = observer
            .try_with_value(|made| made.as_ref().map(|(observer, _)| observer.clone()))
            .flatten()
        {
            return Some(existing);
        }
        let seen = Closure::<dyn Fn(js_sys::Array, web_sys::IntersectionObserver)>::new(
            move |entries: js_sys::Array, observer: web_sys::IntersectionObserver| {
                for entry in entries.iter() {
                    let Ok(entry) = entry.dyn_into::<web_sys::IntersectionObserverEntry>() else {
                        continue;
                    };
                    if entry.is_intersecting() {
                        let target = entry.target();
                        observer.unobserve(&target);
                        crate::assets::show_within(&target, workspace);
                    }
                }
            },
        );
        let options = web_sys::IntersectionObserverInit::new();
        options.set_root_margin(READ_AHEAD);
        let made = web_sys::IntersectionObserver::new_with_options(
            seen.as_ref().unchecked_ref(),
            &options,
        )
        .ok()?;
        observer.set_value(Some((made.clone(), seen)));
        Some(made)
    };
    // Each time the cards are drawn anew, the new ones are watched.
    Effect::new(move |_| {
        workspace.asset_list.track();
        workspace.asset_query.track();
        workspace.asset_kind.track();
        workspace.asset_rows.track();
        workspace.asset_list_layout.track();
        request_animation_frame(move || {
            let (Some(root), Some(observer)) = (root.get_untracked(), made()) else {
                return;
            };
            let Ok(thumbs) = root.query_selector_all("[data-thumb]:not([data-watched])") else {
                return;
            };
            for at in 0..thumbs.length() {
                if let Some(thumb) = thumbs
                    .item(at)
                    .and_then(|node| node.dyn_into::<web_sys::Element>().ok())
                {
                    let _ = thumb.set_attribute("data-watched", "");
                    observer.observe(&thumb);
                }
            }
        });
    });
    on_cleanup(move || {
        if let Some(Some((observer, _))) = observer.try_update_value(Option::take) {
            observer.disconnect();
        }
    });
    root
}

/// Where `asset` lives: its project, and a session's own asset's session,
/// named from the tab's listing when it has one.
pub(super) fn place_of(workspace: Workspace, asset: &Asset) -> AssetPlace {
    let project_name = |project: &cookie_agent_protocol::ProjectId| {
        workspace.projects.with_untracked(|projects| {
            projects
                .iter()
                .find(|known| known.id == *project)
                .map(|known| known.name.to_string())
        })
    };
    let mut place = workspace.asset_list.with_untracked(|list| match list {
        Some(Ok(found)) => asset_place(asset, &found.sessions, project_name),
        _ => asset_place(asset, &[], project_name),
    });
    // Untitled, as the session list names it.
    if let Some((session, title @ None)) = &mut place.session {
        *title = Some(session_title(workspace, *session));
    }
    place
}

/// What the session list calls `session`.
fn session_title(workspace: Workspace, session: SessionId) -> String {
    workspace.sessions.with_untracked(|sessions| {
        sessions
            .iter()
            .find(|meta| meta.session_id == session)
            .map_or_else(|| "Untitled session".to_owned(), super::session_label)
    })
}

/// Who last saved `asset`, and when: the session's title when the page
/// lists it.
pub(super) fn saved_by(workspace: Workspace, asset: &Asset) -> (String, Option<SessionId>) {
    let session = asset.updated.session;
    let title = workspace.sessions.with_untracked(|sessions| {
        sessions
            .iter()
            .find(|meta| meta.session_id == session)
            .map(super::session_label)
    });
    let age = cookie_agent_protocol::memory_age(asset.updated.at, jiff::Timestamp::now());
    match title {
        Some(title) => (format!("saved by {title}, {age} ago"), Some(session)),
        None => (format!("saved by another session, {age} ago"), None),
    }
}

/// Opens `session` in the chat.
fn open_session(workspace: Workspace, session: SessionId) {
    workspace.open_session(session);
    workspace.open_view(View::Chat, None);
}

/// The actions of `asset`'s menu, by ⋯ or a right-click.
pub(super) fn menu_items(
    workspace: Workspace,
    asset: &Asset,
    opener: Option<Callback<()>>,
) -> Vec<MenuItem> {
    let mut items = Vec::new();
    if let Some(open) = opener {
        items.push(MenuItem {
            id: "open",
            label: "Open",
            icon: icondata::LuEye,
            act: open,
            danger: false,
        });
    }
    let handle = asset.handle.to_string();
    items.push(MenuItem {
        id: "copy",
        label: "Copy handle",
        icon: icondata::LuCopy,
        act: Callback::new(move |()| crate::assets::copy_handle(handle.clone(), workspace)),
        danger: false,
    });
    let renamed = asset.clone();
    items.push(MenuItem {
        id: "rename",
        label: "Rename",
        icon: icondata::LuPencil,
        act: Callback::new(move |()| {
            workspace
                .asset_renaming
                .set(Some((renamed.clone(), renamed.title.clone())));
        }),
        danger: false,
    });
    let downloaded = asset.clone();
    items.push(MenuItem {
        id: "download",
        label: "Download",
        icon: icondata::LuDownload,
        act: Callback::new(move |()| crate::assets::download(workspace, downloaded.clone())),
        danger: false,
    });
    if workspace.has_shares.get_untracked() {
        let (store, id, title) = (asset.store.clone(), asset.id, asset.title.clone());
        items.push(MenuItem {
            id: "share",
            label: "Share link…",
            icon: icondata::LuShare2,
            act: Callback::new(move |()| {
                workspace.open_share_dialog(store.clone(), id, title.clone());
            }),
            danger: false,
        });
    }
    let session = match &asset.store {
        cookie_agent_protocol::AssetStore::Session { session } => Some(*session),
        _ => saved_by(workspace, asset).1,
    };
    if let Some(session) = session {
        items.push(MenuItem {
            id: "session",
            label: "Open the session",
            icon: icondata::LuMessagesSquare,
            act: Callback::new(move |()| {
                workspace.asset_page.set(None);
                open_session(workspace, session);
            }),
            danger: false,
        });
    }
    let deleted = asset.clone();
    items.push(MenuItem {
        id: "delete",
        label: "Delete…",
        icon: icondata::LuTrash2,
        act: Callback::new(move |()| workspace.asset_deleting.set(Some(deleted.clone()))),
        danger: true,
    });
    items
}

/// One asset, as a card of the grid or a row of the list: its thumbnail,
/// title, and where it lives; a click opens it and ⋯ offers the rest.
fn item(
    workspace: Workspace,
    asset: Asset,
    place: AssetPlace,
    shown: std::sync::Arc<Vec<Asset>>,
    as_list: bool,
) -> impl IntoView {
    let kind = AssetKind::of(&asset.media_type);
    let id = asset.id;
    let shared = move || {
        workspace.share_list.with(|list| {
            list.as_ref()
                .and_then(|list| list.as_ref().ok())
                .is_some_and(|list| list.shares.iter().any(|share| share.asset == id))
        })
    };
    let opened = {
        let (asset, shown) = (asset.clone(), std::sync::Arc::clone(&shown));
        Callback::new(move |()| open(workspace, &asset, &shown))
    };
    let items = {
        let asset = asset.clone();
        move || menu_items(workspace, &asset, Some(opened))
    };
    let menu = super::row_menu::row_menu(
        workspace.asset_menu,
        id,
        asset.handle.to_string(),
        asset.title.clone(),
        if as_list {
            "inline-flex size-9 shrink-0 items-center justify-center rounded-md text-muted hover:bg-hover hover:text-fg"
        } else {
            CORNER
        },
        items.clone(),
    );
    let context =
        super::row_menu::context_menu(workspace.asset_context, id, asset.title.clone(), items);
    let on_contextmenu = move |event: ev::MouseEvent| {
        if !crate::browser::from_touch(&event) && super::row_menu::wants_context_menu(&event) {
            event.prevent_default();
            workspace.asset_context.set(Some((
                id,
                f64::from(event.client_x()),
                f64::from(event.client_y()),
            )));
        }
    };
    let on_keydown = move |event: ev::KeyboardEvent| {
        let on_card = event
            .target()
            .and_then(|target| target.dyn_into::<web_sys::Element>().ok())
            .is_some_and(|target| target.has_attribute("data-asset-row"));
        if on_card && (event.key() == "Enter" || event.key() == " ") {
            event.prevent_default();
            opened.run(());
        }
    };
    let level = if place.session.is_some() {
        "session"
    } else {
        "project"
    };
    let tree = place.session.map(|(session, title)| {
        let title = title.unwrap_or_else(|| session_title(workspace, session));
        view! {
            <span>"·"</span>
            <button
                type="button"
                class="min-w-0 truncate underline decoration-dotted underline-offset-2 hover:text-fg"
                data-asset-tree=session.to_string()
                title="Open the session"
                on:click=move |event| {
                    event.stop_propagation();
                    open_session(workspace, session);
                }
            >
                {title}
            </button>
        }
    });
    let where_ = view! {
        <span class=format!("flex min-w-0 items-center gap-1 {NOTE}") data-asset-place=level>
            <Glyph
                icon=if level == "session" { icondata::LuMessagesSquare } else { icondata::LuFolder }
                class="shrink-0 text-xs"
            />
            <span class="shrink-0" data-asset-project>{place.project}</span>
            {tree}
        </span>
    };
    let title_line = view! {
        <span class="flex min-w-0 items-center gap-1.5">
            <Glyph icon=crate::icons::asset_icon(kind) class="shrink-0 text-[0.8125rem] text-accent" />
            <span class="min-w-0 truncate text-[0.8125rem] font-semibold" data-asset-row-title>{asset.title.clone()}</span>
            <Show when=shared>
                <span class="shrink-0 text-xs text-accent" title="Shared" data-asset-shared>
                    <Glyph icon=icondata::LuShare2 />
                </span>
            </Show>
        </span>
    };
    let label =
        crate::render::attachments::type_label_of(asset.filename.as_deref(), &asset.media_type);
    let age = cookie_agent_protocol::memory_age(asset.updated.at, jiff::Timestamp::now());
    let thumbnail = crate::assets::tab_thumbnail_html(&asset);
    let handle = asset.handle.to_string();
    let copy = handle.clone();
    let row_attrs = (handle.clone(), asset.title.clone());
    if as_list {
        let (saved, _) = saved_by(workspace, &asset);
        view! {
            {context}
            <div
                class="flex min-w-0 cursor-pointer items-center gap-3 rounded-lg border-b border-line px-1.5 py-2 outline-none hover:bg-hover focus-visible:ring-2 focus-visible:ring-accent"
                role="button"
                tabindex="0"
                aria-label=format!("Open {}", row_attrs.1)
                data-asset-row=row_attrs.0
                on:click=move |_| opened.run(())
                on:keydown=on_keydown
                on:contextmenu=on_contextmenu
            >
                <span class="relative aspect-[4/3] w-16 shrink-0 overflow-hidden rounded-md bg-sunken shadow-[0_0_0_1px_var(--color-line)] [&_[data-asset-icon]_span]:hidden [&_[data-asset-icon]_svg]:text-xl" data-thumb inner_html=thumbnail></span>
                <span class="flex min-w-0 flex-1 flex-col gap-0.5">
                    {title_line}
                    {where_}
                </span>
                <span class=format!("hidden shrink-0 text-right sm:block {NOTE}")>
                    {format!("{} · {}", asset.media_type, human_bytes(asset.byte_length))}
                    <br />
                    {saved}
                </span>
                <span class="contents" on:click=|event| event.stop_propagation()>{menu}</span>
            </div>
        }
        .into_any()
    } else {
        view! {
            {context}
            <div
                class=CARD
                role="button"
                tabindex="0"
                aria-label=format!("Open {}", row_attrs.1)
                title=format!("{} · {}", row_attrs.1, row_attrs.0)
                data-asset-row=row_attrs.0
                on:click=move |_| opened.run(())
                on:keydown=on_keydown
                on:contextmenu=on_contextmenu
            >
                <span class=CARD_BOX>
                    <span class="relative block aspect-[4/3] overflow-hidden bg-sunken" data-thumb inner_html=thumbnail></span>
                    <span class="flex min-w-0 flex-col gap-0.5 px-2.5 pt-2 pb-2.5">
                        {title_line}
                        <span class=format!("truncate {NOTE}")>
                            {format!("{label} · {} · {age} ago", human_bytes(asset.byte_length))}
                        </span>
                        {where_}
                    </span>
                </span>
                // Shown while its menu is open, which it holds.
                <span
                    class="absolute top-1.5 right-1.5 z-10 flex gap-1 opacity-0 transition-opacity group-hover:opacity-100 group-focus-within:opacity-100 pointer-coarse:opacity-100"
                    class:opacity-100=move || workspace.asset_menu.get() == Some(id)
                    on:click=|event| event.stop_propagation()
                >
                    <button
                        type="button"
                        class=format!("{CORNER} pointer-coarse:hidden")
                        aria-label=format!("Copy the handle {handle}")
                        title=format!("Copy {handle}")
                        data-asset-action="copy"
                        on:click=move |_| crate::assets::copy_handle(copy.clone(), workspace)
                    >
                        <Glyph icon=icondata::LuCopy />
                    </button>
                    {menu}
                </span>
            </div>
        }
        .into_any()
    }
}

/// The rename and delete dialogs of an asset, from the tab, its page, or
/// the media viewer's Details: over everything, the viewer included.
#[component]
pub(crate) fn AssetDialogs(workspace: Workspace) -> impl IntoView {
    let field = NodeRef::<html::Input>::new();
    Effect::new(move |_| {
        if let Some(field) = field.get() {
            let _ = field.focus();
            field.select();
        }
    });
    // The focus goes back to the viewer, for its keys, when it is open.
    let refocus = move || {
        request_animation_frame(|| {
            if let Some(viewer) = document()
                .get_element_by_id("media-viewer")
                .and_then(|viewer| viewer.dyn_into::<web_sys::HtmlElement>().ok())
            {
                let _ = viewer.focus();
            }
        });
    };
    let close = move || {
        workspace.asset_renaming.set(None);
        workspace.asset_deleting.set(None);
        refocus();
    };
    let open = move || {
        workspace.asset_renaming.with(Option::is_some)
            || workspace.asset_deleting.with(Option::is_some)
    };
    let save = move || {
        if let Some((asset, title)) = workspace.asset_renaming.get_untracked() {
            workspace.rename_asset(asset.store.clone(), asset.id, title);
            refocus();
        }
    };
    let body = move || {
        if workspace.asset_renaming.with(Option::is_some) {
            view! {
                <form
                    class="flex flex-col gap-3"
                    data-asset-rename
                    on:submit=move |event: ev::SubmitEvent| {
                        event.prevent_default();
                        save();
                    }
                >
                    <h2 class="text-[0.9375rem] font-semibold" id="asset-dialog-title">"Rename"</h2>
                    <input
                        type="text"
                        node_ref=field
                        id="asset-rename-title"
                        class="min-h-11 w-full min-w-0 rounded-lg bg-surface px-3 text-sm shadow-[inset_0_0_0_1px_var(--color-line-2)] focus:shadow-[inset_0_0_0_1px_var(--color-accent),0_0_0_3px_var(--color-accent-soft)] focus:outline-none lg:min-h-9"
                        maxlength="200"
                        aria-label="The asset's title"
                        prop:value=move || {
                            workspace
                                .asset_renaming
                                .with_untracked(|renaming| renaming.as_ref().map(|(_, title)| title.clone()))
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
                    <div class="flex justify-end gap-2">
                        <button type="button" class=BUTTON on:click=move |_| close()>"Cancel"</button>
                        <button type="submit" class=PRIMARY id="asset-rename-save">"Save"</button>
                    </div>
                </form>
            }
            .into_any()
        } else {
            let asset = workspace.asset_deleting.get();
            let title = asset
                .as_ref()
                .map(|asset| asset.title.clone())
                .unwrap_or_default();
            view! {
                <div class="flex flex-col gap-3" data-asset-delete-confirm>
                    <h2 class="text-[0.9375rem] font-semibold" id="asset-dialog-title">"Delete the asset?"</h2>
                    <p class="text-sm text-fg-2">
                        {format!("Delete {title}? Every place it was shown will say it was deleted.")}
                    </p>
                    <div class="flex justify-end gap-2">
                        <button type="button" class=BUTTON on:click=move |_| close()>"Keep"</button>
                        <button
                            type="button"
                            class=format!("{BUTTON} text-bad")
                            id="asset-delete-confirm"
                            on:click=move |_| {
                                if let Some(asset) = workspace.asset_deleting.get_untracked() {
                                    workspace.delete_asset(asset.store.clone(), asset.id);
                                }
                                refocus();
                            }
                        >
                            "Delete"
                        </button>
                    </div>
                </div>
            }
            .into_any()
        }
    };
    view! {
        <Show when=open>
            <div class="fixed inset-0 z-[70] bg-scrim dark:bg-black/50" on:click=move |_| close()></div>
            <div
                id="asset-dialog"
                role="dialog"
                aria-modal="true"
                aria-labelledby="asset-dialog-title"
                class="fixed inset-x-0 bottom-0 z-[70] flex flex-col rounded-t-xl p-4 pb-[max(1rem,env(safe-area-inset-bottom))] text-sm text-fg sm:inset-x-auto sm:top-1/2 sm:bottom-auto sm:left-1/2 sm:w-[26rem] sm:max-w-[calc(100vw-2rem)] sm:-translate-x-1/2 sm:-translate-y-1/2 sm:rounded-xl bg-raised ring-1 ring-line shadow-menu"
                on:keydown=move |event| {
                    if event.key() == "Escape" {
                        event.stop_propagation();
                        close();
                    }
                }
            >
                {body}
            </div>
        </Show>
    }
}
