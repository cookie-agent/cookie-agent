//! The page's one sidebar (docs/site/specs/quiet-ink.md, D3): the brand,
//! New session, the views under *Workspace*, the sessions by project, and
//! Settings, the connection, and the theme at its foot.
//!
//! A desktop pins it beside every view, or hides it (the sidebar button,
//! remembered in the browser). A narrower window shows it as the page,
//! over the view, while the sidebar button or Back has it open.

use leptos::prelude::*;

use super::{SessionList, shell::view_icon};
use crate::{
    app::{Connection, ConnectionStatus, ThemeToggle},
    icons::Glyph,
    layout::View,
    theme::ThemePreference,
    workspace::Workspace,
};

/// One entry of the sidebar's navigation: an icon and a name in a 32 px
/// row, tinted and marked while it is the view shown.
pub(super) const NAV_ROW: &str = "hit-area group relative flex h-8 w-full items-center gap-2.5 rounded-lg px-2 text-left text-[0.8125rem] text-fg-2 transition-colors hover:bg-hover hover:text-fg active:bg-press aria-[current=page]:bg-sel aria-[current=page]:font-semibold aria-[current=page]:text-fg";

/// An icon-only button of the sidebar's foot.
const FOOT_BUTTON: &str = "inline-flex h-11 w-11 shrink-0 items-center justify-center rounded-lg text-muted transition-colors hover:bg-hover hover:text-fg active:bg-press lg:h-8 lg:w-8";

/// A view's entry on a phone, where the views are a grid of tiles so the
/// sessions keep the room.
const NAV_TILE: &str = "group flex h-14 min-w-0 flex-col items-center justify-center gap-1 rounded-xl text-[0.6875rem] text-fg-2 transition-colors hover:bg-hover active:bg-press aria-[current=page]:bg-sel aria-[current=page]:text-fg";

/// The views the sidebar lists under *Workspace*: every view but Chat,
/// which is the session list itself, and Settings, at the foot.
const WORKSPACE_VIEWS: [View; 8] = [
    View::Agents,
    View::Skills,
    View::Memory,
    View::Assets,
    View::Sharing,
    View::Machines,
    View::Mcp,
    View::Cron,
];

/// The sidebar entry of `view`.
fn nav_entry(workspace: Workspace, view: View) -> impl IntoView {
    let current = move || workspace.view.get() == view;
    // Scheduled prompts need a daemon that keeps cron jobs.
    let offered = move || {
        (view != View::Cron || workspace.has_crons.get())
            && (view != View::Sharing || workspace.has_shares.get())
    };
    let label = if view == View::Machines {
        "Machines"
    } else if view == View::Cron {
        "Schedules"
    } else {
        view.label()
    };
    view! {
        <button
            type="button"
            class=move || if workspace.narrow.get() { NAV_TILE } else { NAV_ROW }
            data-view=view.id()
            aria-current=move || current().then_some("page")
            class:hidden=move || !offered()
            on:click=move |_| workspace.open_from_sidebar(view)
        >
            <Glyph
                icon=view_icon(view)
                class="text-base text-muted transition-colors group-hover:text-fg-2 group-aria-[current=page]:text-accent"
            />
            <span class="max-w-full truncate px-1">{label}</span>
        </button>
    }
}

/// The sidebar: what [`WorkspaceView`](super::WorkspaceView) shows beside
/// every view, or as the page below desktop widths.
#[component]
pub fn Sidebar(
    workspace: Workspace,
    connection: Connection,
    theme: RwSignal<ThemePreference>,
) -> impl IntoView {
    // Below desktop widths the sidebar is the page, over the view.
    let paged = move || workspace.sidebar_open.get();
    let nav = WORKSPACE_VIEWS
        .into_iter()
        .map(|view| nav_entry(workspace, view))
        .collect_view();
    view! {
        <aside
            id="sidebar"
            class="z-10 w-64 shrink-0 flex-col border-r border-line bg-surface-2"
            class:hidden=move || !paged()
            class:flex=paged
            class:absolute=paged
            class:inset-0=paged
            class:w-full=paged
            class:lg:flex=move || workspace.sidebar_pinned.get()
            aria-label="Sidebar"
        >
            <div class="flex h-12 shrink-0 items-center gap-2 px-3">
                <img src="icons/icon.svg" alt="" class="h-6 w-6 shrink-0" />
                <span class="min-w-0 flex-1 truncate text-sm font-semibold tracking-tight">"Cookie Agent"</span>
                <Show when=paged>
                    <super::shell::SidebarButton workspace=workspace />
                </Show>
            </div>
            <nav aria-label="Views" class="flex shrink-0 flex-col gap-px px-2" data-sidebar-views>
                <button
                    type="button"
                    class=NAV_ROW
                    data-new-session
                    aria-current=move || {
                        (workspace.view.get() == View::Chat && workspace.new_session.get())
                            .then_some("page")
                    }
                    on:click=move |_| workspace.start_new_session()
                >
                    <Glyph icon=icondata::LuSquarePen class="text-base text-muted group-hover:text-fg-2 group-aria-[current=page]:text-accent" />
                    <span class="truncate">"New session"</span>
                </button>
                <p class="px-2 pt-4 pb-1 text-[0.6875rem] font-semibold text-muted">"Workspace"</p>
                <div class=move || if workspace.narrow.get() { "grid grid-cols-4 gap-1" } else { "flex flex-col gap-px" }>{nav}</div>
            </nav>
            <SessionList workspace=workspace />
            <div class="flex shrink-0 items-center gap-1 border-t border-line px-2 py-1.5">
                <button
                    type="button"
                    class=format!("{NAV_ROW} min-w-0 flex-1")
                    data-view=View::Settings.id()
                    aria-current=move || (workspace.view.get() == View::Settings).then_some("page")
                    on:click=move |_| workspace.open_from_sidebar(View::Settings)
                >
                    <Glyph icon=view_icon(View::Settings) class="text-base text-muted group-hover:text-fg-2 group-aria-[current=page]:text-accent" />
                    <span class="truncate">"Settings"</span>
                </button>
                <ConnectionStatus connection=connection compact=true primary=true />
                <ThemeToggle theme=theme class=FOOT_BUTTON />
            </div>
        </aside>
    }
}
