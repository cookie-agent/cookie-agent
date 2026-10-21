//! The page's frame around the views: the rail that switches them, the
//! drawer that carries the rail and the sessions on a phone (and the
//! sessions over a tablet's transcript), and the full-page views other
//! than Chat.
//!
//! Widths: below 640 px (Tailwind's `sm`) a phone, whose rail is in the
//! drawer; from 640 px the rail stays on the left; from 1024 px (`lg`) the
//! sessions sidebar is pinned beside the transcript.

use leptos::prelude::*;

use super::{
    ICON_BUTTON, NOTE, ROW, SessionList, events_sheet,
    manage::{
        ConfigScope, config_sheet, connect_sheet, machines_page, mcp_sheet, memory_sheet,
        plugins_sheet,
    },
};
use crate::{
    app::{Connection, ConnectionStatus, ForgetDaemon, ThemeToggle},
    icons::Glyph,
    layout::{Layer, SettingsSection, View},
    theme::ThemePreference,
    workspace::Workspace,
};

/// A rail button: an icon in a 44 px square, its name in a tooltip beside
/// it (`data-tip`).
const RAIL_BUTTON: &str = "relative inline-flex h-11 w-11 shrink-0 items-center justify-center rounded-lg text-ink hover:bg-toasted hover:text-espresso active:bg-crust dark:text-sand dark:hover:bg-ember dark:hover:text-latte dark:active:bg-smoke";

/// The icon of a view on the rail.
pub fn view_icon(view: View) -> crate::icons::Icon {
    match view {
        View::Chat => icondata::FaCommentsSolid,
        View::Agents => icondata::FaRobotSolid,
        View::Settings => icondata::FaGearSolid,
        View::Machines => icondata::FaServerSolid,
        View::Memory => icondata::FaBrainSolid,
    }
}

/// The rail: the views, then the connection and the theme at its foot.
/// `primary` is the one always in the page (the drawer has a second).
#[component]
pub fn Rail(
    workspace: Workspace,
    connection: Connection,
    theme: RwSignal<ThemePreference>,
    primary: bool,
) -> impl IntoView {
    let buttons = View::ALL
        .into_iter()
        .map(|view| {
            let current = move || workspace.view.get() == view;
            view! {
                <button
                    type="button"
                    class=RAIL_BUTTON
                    class:bg-toasted=current
                    class:text-espresso=current
                    class:dark:bg-ember=current
                    class:dark:text-latte=current
                    aria-label=view.label()
                    data-tip=view.label()
                    data-view=view.id()
                    aria-current=move || current().then_some("page")
                    on:click=move |_| workspace.open_view(view, None)
                >
                    // The active view's mark: a bar at the rail's edge.
                    <span
                        class="absolute top-2 bottom-2 -left-1.5 w-1 rounded-r bg-caramel dark:bg-maple"
                        class:hidden=move || !current()
                        aria-hidden="true"
                    ></span>
                    <Glyph icon=view_icon(view) class="text-lg" />
                </button>
            }
        })
        .collect_view();
    view! {
        <nav
            aria-label="Views"
            class="rail flex w-14 shrink-0 flex-col items-center gap-1 border-r border-toasted bg-parchment py-2 dark:border-ember dark:bg-char"
            data-rail=if primary { "primary" } else { "drawer" }
        >
            <img src="icons/icon.svg" alt="" class="mb-1 h-7 w-7 short:hidden" />
            {buttons}
            <span class="flex-1"></span>
            <ConnectionStatus connection=connection compact=true primary=primary />
            <ThemeToggle theme=theme class=RAIL_BUTTON in_rail=true />
        </nav>
    }
}

/// The sessions over the page: a phone's drawer (the rail beside the
/// sessions, 85 % wide), or the sessions sliding over a tablet's
/// transcript. A desktop pins them instead.
#[component]
pub fn Drawer(
    workspace: Workspace,
    connection: Connection,
    theme: RwSignal<ThemePreference>,
) -> impl IntoView {
    view! {
        <Show when=move || workspace.sidebar_open.get()>
            <div
                class="fixed inset-0 z-30 bg-espresso/30 lg:hidden dark:bg-night/60"
                data-drawer-backdrop
                on:click=move |_| workspace.sidebar_open.set(false)
            ></div>
            <div
                id="drawer"
                role="dialog"
                aria-label="Sessions"
                class="sheet-frame fixed top-0 left-0 z-40 flex w-[85%] max-w-sm shadow-xl sm:bottom-0 sm:left-14 sm:w-80 lg:hidden"
            >
                <div class="flex sm:hidden">
                    <Rail workspace=workspace connection=connection theme=theme primary=false />
                </div>
                <div class="flex min-w-0 flex-1 flex-col overflow-y-auto border-r border-toasted bg-parchment dark:border-ember dark:bg-char">
                    <SessionList workspace=workspace />
                </div>
            </div>
        </Show>
    }
}

/// The sidebar button: it shows and hides the sessions.
#[component]
pub fn SidebarButton(workspace: Workspace, #[prop(optional)] class: &'static str) -> impl IntoView {
    let shown = move || {
        workspace.sidebar_open.get()
            || (workspace.view.get() == View::Chat && workspace.sidebar_pinned.get())
    };
    view! {
        <button
            type="button"
            class=format!("{ICON_BUTTON} {class}")
            aria-label="Sessions"
            title="Show or hide the sessions"
            data-sidebar-toggle
            aria-expanded=move || shown().to_string()
            on:click=move |_| workspace.toggle_sidebar()
        >
            <Glyph icon=icondata::FaTableColumnsSolid class="text-lg" />
        </button>
    }
}

/// A view other than Chat, as a full page: its title bar, then its
/// content. A phone's title bar has the sidebar button, and a back arrow
/// on a detail page.
#[component]
pub fn PageView(workspace: Workspace, connection: Connection) -> impl IntoView {
    let detail = move || {
        workspace
            .open_layers()
            .iter()
            .any(|layer| matches!(layer, Layer::Section | Layer::Editor))
    };
    let title = move || match workspace.view.get() {
        View::Settings if workspace.narrow.get() => workspace
            .settings_section
            .get()
            .map_or("Settings", SettingsSection::label),
        view => view.label(),
    };
    view! {
        <section
            id="page"
            class="flex min-w-0 flex-1 flex-col"
            data-view=move || workspace.view.get().id()
        >
            <div class="flex h-12 shrink-0 items-center gap-1 border-b border-toasted px-2 dark:border-ember">
                <SidebarButton workspace=workspace class="sm:hidden" />
                <Show when=detail>
                    <button
                        type="button"
                        class=ICON_BUTTON
                        aria-label="Back"
                        title="Back"
                        data-page-back
                        on:click=move |_| workspace.go_back()
                    >
                        <Glyph icon=icondata::FaArrowLeftSolid class="text-lg" />
                    </button>
                </Show>
                <h1 class="min-w-0 flex-1 truncate px-1 text-lg font-bold" id="page-title">{title}</h1>
            </div>
            {move || match workspace.view.get() {
                View::Settings => settings_page(workspace, connection).into_any(),
                view => view! {
                    <div class="flex min-h-0 flex-1 flex-col overflow-y-auto p-3" data-page-body>
                        <div class="mx-auto flex w-full max-w-3xl flex-1 flex-col gap-2">
                            {match view {
                                View::Agents => config_sheet(workspace, ConfigScope::Agents).into_any(),
                                View::Machines => machines_page(workspace).into_any(),
                                View::Memory => memory_sheet(workspace).into_any(),
                                View::Chat | View::Settings => ().into_any(),
                            }}
                        </div>
                    </div>
                }
                .into_any(),
            }}
        </section>
    }
}

/// Settings: the section list beside the open section; on a phone the
/// list, then one section at a time.
fn settings_page(workspace: Workspace, connection: Connection) -> impl IntoView {
    let entries = SettingsSection::ALL
        .into_iter()
        .map(|section| {
            let current = move || workspace.shown_section() == Some(section);
            view! {
                <button
                    type="button"
                    class=format!("{ROW} min-h-12")
                    class:bg-toasted=current
                    class:dark:bg-ember=current
                    aria-current=move || current().then_some("page")
                    data-section=section.id()
                    on:click=move |_| workspace.open_section(Some(section))
                >
                    <span class="flex w-full items-center gap-2">
                        <span class="min-w-0 flex-1 text-sm font-bold">{section.label()}</span>
                        <Glyph icon=icondata::FaChevronRightSolid class="text-xs opacity-60 sm:hidden" />
                    </span>
                    <span class=NOTE>{section.note()}</span>
                </button>
            }
        })
        .collect_view();
    view! {
        <div class="flex min-h-0 flex-1">
            <nav
                aria-label="Settings sections"
                class="flex w-full shrink-0 flex-col gap-1 overflow-y-auto p-2 sm:w-60 sm:border-r sm:border-toasted sm:dark:border-ember"
                class:hidden=move || workspace.shown_section().is_some() && workspace.narrow.get()
            >
                {entries}
            </nav>
            {move || {
                workspace.shown_section().map(|section| view! {
                    <div class="flex min-h-0 min-w-0 flex-1 flex-col overflow-y-auto p-3" data-page-body data-section-body=section.id()>
                        <div class="mx-auto flex w-full max-w-3xl flex-1 flex-col gap-2">
                            {match section {
                                SettingsSection::Configuration => config_sheet(workspace, ConfigScope::Configuration).into_any(),
                                SettingsSection::Providers => connect_sheet(workspace).into_any(),
                                SettingsSection::Mcp => mcp_sheet(workspace).into_any(),
                                SettingsSection::Plugins => plugins_sheet(workspace).into_any(),
                                SettingsSection::Browser => browser_section(workspace, connection).into_any(),
                            }}
                        </div>
                    </div>
                })
            }}
        </div>
    }
}

/// This browser: installing the page as an app, and what it keeps for the
/// daemon: the diagnostics the transcript shows, and the token.
fn browser_section(workspace: Workspace, connection: Connection) -> impl IntoView {
    let confirming = RwSignal::new(false);
    view! {
        {super::install::install_section()}
        <h2 class="mt-4 text-sm font-bold">"Diagnostics in the transcript"</h2>
        <p class=NOTE>"The same choice as /events; it stays in this browser."</p>
        {events_sheet(workspace)}
        <h2 class="mt-4 text-sm font-bold">"This daemon"</h2>
        <p class=NOTE>"The browser keeps the daemon's token to connect again without a link."</p>
        <ForgetDaemon connection=connection confirming=confirming primary=false on_forget=|| {} />
    }
}
