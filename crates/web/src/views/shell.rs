//! The page's frame around the views: the rail that switches them, the
//! sidebar button, and the full-page views other than Chat.
//!
//! Widths: below 640 px (Tailwind's `sm`) a phone, which shows the rail
//! beside a view's first level (its list) only, and gives an item open
//! the whole screen; from 640 px the rail stays on the left; below
//! 1024 px (`lg`) the sessions (the agents, in Agents) show as the page in
//! place of the transcript (the agent open), and from 1024 px they are
//! pinned beside it.

use leptos::prelude::*;

use super::{
    ICON_BUTTON, NOTE, ROW,
    agents::agents_page,
    cron::cron_page,
    events_sheet,
    machines::machines_page,
    manage::{config_sheet, connect_sheet, mcp_sheet, plugins_sheet},
    memory::memory_sheet,
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
        View::Cron => icondata::FaClockSolid,
    }
}

/// The rail: the views, then the connection and the theme at its foot.
#[component]
pub fn Rail(
    workspace: Workspace,
    connection: Connection,
    theme: RwSignal<ThemePreference>,
) -> impl IntoView {
    let buttons = View::ALL
        .into_iter()
        .map(|view| {
            let current = move || workspace.view.get() == view;
            // Scheduled prompts need a daemon that keeps cron jobs.
            let offered = move || view != View::Cron || workspace.has_crons.get();
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
                    class:hidden=move || !offered()
                    on:click=move |_| workspace.open_from_rail(view)
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
            data-rail="primary"
        >
            <img src="icons/icon.svg" alt="" class="mb-1 h-7 w-7 short:hidden" />
            {buttons}
            <span class="flex-1"></span>
            <ConnectionStatus connection=connection compact=true primary=true />
            <ThemeToggle theme=theme class=RAIL_BUTTON in_rail=true />
        </nav>
    }
}

/// What the sidebar holds in `view`: the sessions in Chat, else the
/// view's list a phone's sidebar button goes back up to.
fn sidebar_label(view: View) -> &'static str {
    if view == View::Chat {
        "Sessions"
    } else {
        view.label()
    }
}

/// The sidebar button: it shows and hides the sessions, or the agents in
/// Agents; in the other views, on a phone's second level, it goes back up
/// to the view's list.
#[component]
pub fn SidebarButton(workspace: Workspace) -> impl IntoView {
    let shown = move || {
        workspace.sidebar_open.get()
            || (workspace.view.get().has_sidebar() && workspace.sidebar_pinned.get())
    };
    let title = move || {
        let view = workspace.view.get();
        let label = sidebar_label(view);
        if view.has_sidebar() {
            format!("Show or hide the {}", label.to_lowercase())
        } else {
            format!("Back to {label}")
        }
    };
    view! {
        <button
            type="button"
            class=ICON_BUTTON
            aria-label=move || sidebar_label(workspace.view.get())
            title=title
            data-sidebar-toggle
            aria-expanded=move || shown().to_string()
            on:click=move |_| workspace.toggle_sidebar()
        >
            <Glyph icon=icondata::FaTableColumnsSolid class="text-lg" />
        </button>
    }
}

/// A view other than Chat, as a full page: its title bar, then its
/// content. Agents' title bar has the sidebar button, and so has a phone's
/// second level, to go back up to the list; a detail page has a back
/// arrow, which on a phone only a third level needs.
#[component]
pub fn PageView(workspace: Workspace, connection: Connection) -> impl IntoView {
    let sidebar_button = move || {
        let phone = workspace.narrow.get();
        match workspace.view.get() {
            // A phone with no agent open shows the agents anyway.
            View::Agents => !phone || workspace.config_edit.with(Option::is_some),
            _ => phone && !workspace.at_list(),
        }
    };
    let back = move || {
        let depth = workspace
            .open_layers()
            .iter()
            .filter(|layer| matches!(layer, Layer::Section | Layer::Editor))
            .count();
        depth > usize::from(workspace.narrow.get())
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
                <Show when=sidebar_button>
                    <SidebarButton workspace=workspace />
                </Show>
                <Show when=back>
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
                View::Agents => agents_page(workspace).into_any(),
                view => view! {
                    <div class="flex min-h-0 flex-1 flex-col overflow-y-auto p-3" data-page-body>
                        <div class="mx-auto flex w-full max-w-3xl flex-1 flex-col gap-2">
                            {match view {
                                View::Machines => machines_page(workspace).into_any(),
                                View::Memory => memory_sheet(workspace).into_any(),
                                View::Cron => cron_page(workspace).into_any(),
                                View::Chat | View::Settings | View::Agents => ().into_any(),
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
                <Show when=move || workspace.onboarding.status.with(Option::is_some)>
                    <button
                        type="button"
                        id="settings-onboard"
                        class=format!("{ROW} min-h-12")
                        on:click=move |_| workspace.open_onboarding()
                    >
                        <span class="flex w-full items-center gap-2">
                            <Glyph icon=icondata::FaWandMagicSparklesSolid />
                            <span class="min-w-0 flex-1 text-sm font-bold">"Set up Cookie"</span>
                        </span>
                        <span class=NOTE>"a provider, the default and small models, the starter kit"</span>
                    </button>
                </Show>
                {entries}
            </nav>
            {move || {
                workspace.shown_section().map(|section| view! {
                    <div class="flex min-h-0 min-w-0 flex-1 flex-col overflow-y-auto p-3" data-page-body data-section-body=section.id()>
                        <div class="mx-auto flex w-full max-w-3xl flex-1 flex-col gap-2">
                            {match section {
                                SettingsSection::Configuration => config_sheet(workspace).into_any(),
                                SettingsSection::Providers => connect_sheet(workspace).into_any(),
                                SettingsSection::Mcp => mcp_sheet(workspace).into_any(),
                                SettingsSection::Plugins => plugins_sheet(workspace).into_any(),
                                SettingsSection::Notifications => super::notifications::notifications_section(workspace, connection).into_any(),
                                SettingsSection::Browser => browser_section(workspace, connection).into_any(),
                                SettingsSection::About => crate::app::about_section(connection).into_any(),
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
        <p class=NOTE>"The same choice as Diagnostics in the session's ⋮ menu; it stays in this browser."</p>
        {events_sheet(workspace)}
        <h2 class="mt-4 text-sm font-bold">"This daemon"</h2>
        <p class=NOTE>"The browser keeps the daemon's token to connect again without a link."</p>
        <ForgetDaemon connection=connection confirming=confirming primary=false on_forget=|| {} />
    }
}
