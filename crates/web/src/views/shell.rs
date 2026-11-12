//! The page's frame around the views: the sidebar button, and the
//! full-page views other than Chat. The sidebar itself is
//! [`super::sidebar`].
//!
//! Widths: below 640 px (Tailwind's `sm`) a phone, which gives an item
//! open in a view's list the whole screen; below 1024 px (`lg`) the
//! sidebar shows as the page, over the view, and from 1024 px it is pinned
//! beside every view.

use leptos::prelude::*;

use super::{
    ICON_BUTTON, NOTE,
    agents::agents_page,
    cron::cron_page,
    events_sheet,
    machines::machines_page,
    manage::{config_sheet, connect_sheet, mcp_sheet, plugins_sheet},
    memory::memory_sheet,
    transcript_sheet,
};
use crate::{
    app::{Connection, ForgetDaemon},
    icons::Glyph,
    layout::{Layer, SettingsSection, View},
    workspace::Workspace,
};

/// The icon of a view in the sidebar.
pub fn view_icon(view: View) -> crate::icons::Icon {
    match view {
        View::Chat => icondata::LuMessagesSquare,
        View::Agents => icondata::LuBot,
        View::Skills => icondata::LuBookOpen,
        View::Assets => icondata::LuImages,
        View::Sharing => icondata::LuShare2,
        View::Mcp => icondata::LuPlug,
        View::Settings => icondata::LuSettings,
        View::Machines => icondata::LuServer,
        View::Memory => icondata::LuBrain,
        View::Cron => icondata::LuClock,
    }
}

/// The sidebar button: it shows and hides the sidebar; on a phone's second
/// level (an item open in a view's list) it goes back up to the list. A
/// dot on it says a session is unread while the sidebar is hidden.
#[component]
pub fn SidebarButton(workspace: Workspace) -> impl IntoView {
    let shown = move || {
        workspace.sidebar_open.get() || (workspace.desktop.get() && workspace.sidebar_pinned.get())
    };
    let back_up = move || {
        workspace.view.get() != View::Chat && workspace.narrow.get() && !workspace.at_list()
    };
    let title = move || {
        if back_up() {
            format!("Back to {}", workspace.view.get().label())
        } else if workspace.sidebar_open.get() {
            "Hide the sidebar".to_owned()
        } else {
            "Show or hide the sidebar".to_owned()
        }
    };
    view! {
        <button
            type="button"
            class=format!("{ICON_BUTTON} relative")
            aria-label=move || if back_up() { "Back" } else { "Sidebar" }
            title=title
            data-sidebar-toggle
            aria-expanded=move || shown().to_string()
            on:click=move |_| workspace.toggle_sidebar()
        >
            <Glyph
                icon=Signal::derive(move || {
                    if back_up() { icondata::LuArrowLeft } else { icondata::LuPanelLeft }
                })
                class="text-lg"
            />
            // A session is unread and the sidebar, which would show it, is
            // hidden.
            <Show when=move || !shown() && workspace.any_unread()>
                <span
                    class="absolute top-2 right-2 h-2 w-2 rounded-full bg-accent ring-2 ring-surface"
                    data-unread-any
                    aria-hidden="true"
                ></span>
            </Show>
        </button>
    }
}

/// A view other than Chat, as a full page: its title bar, then its
/// content. Agents' title bar has the sidebar button, and so has a phone's
/// second level, to go back up to the list; a detail page has a back
/// arrow, which on a phone only a third level needs.
#[component]
pub fn PageView(workspace: Workspace, connection: Connection) -> impl IntoView {
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
            // An asset open on its page has a head of its own.
            <div
                class="flex h-12 shrink-0 items-center gap-1 border-b border-line px-2"
                class:hidden=move || workspace.view.get() == View::Assets && workspace.asset_page.with(Option::is_some)
            >
                <SidebarButton workspace=workspace />
                <Show when=back>
                    <button
                        type="button"
                        class=ICON_BUTTON
                        aria-label="Back"
                        title="Back"
                        data-page-back
                        on:click=move |_| workspace.go_back()
                    >
                        <Glyph icon=icondata::LuArrowLeft class="text-lg" />
                    </button>
                </Show>
                <h1 class="min-w-0 flex-1 truncate px-1 text-[0.9375rem] font-semibold tracking-tight" id="page-title">{title}</h1>
            </div>
            {move || match workspace.view.get() {
                View::Settings => settings_page(workspace, connection).into_any(),
                View::Agents => agents_page(workspace).into_any(),
                // The list stays, hidden, under an asset's page, so Back
                // finds it scrolled where it was.
                View::Assets => view! {
                    <div
                        class="flex min-h-0 flex-1 flex-col overflow-y-auto px-3 py-4 sm:px-8 sm:py-6"
                        class:hidden=move || workspace.asset_page.with(Option::is_some)
                        data-page-body
                    >
                        <div class="mx-auto flex w-full max-w-6xl flex-1 flex-col gap-3">
                            {super::assets::assets_page(workspace)}
                        </div>
                    </div>
                    <super::asset_page::AssetPage workspace />
                }
                .into_any(),
                view => view! {
                    <div class="flex min-h-0 flex-1 flex-col overflow-y-auto px-3 py-4 sm:px-8 sm:py-6" data-page-body>
                        // One width for every view (docs/site/specs/web-polish.md,
                        // P5).
                        <div class="mx-auto flex w-full max-w-[55rem] flex-1 flex-col gap-3">
                            {match view {
                                View::Machines => machines_page(workspace).into_any(),
                                View::Memory => memory_sheet(workspace).into_any(),
                                View::Cron => cron_page(workspace).into_any(),
                                View::Skills => super::skills::skills_page(workspace).into_any(),
                                View::Sharing => super::shares::sharing_page(workspace).into_any(),
                                View::Mcp => mcp_sheet(workspace).into_any(),
                                View::Chat | View::Settings | View::Agents | View::Assets => ().into_any(),
                            }}
                        </div>
                    </div>
                }
                .into_any(),
            }}
        </section>
    }
}

/// A Settings section's icon in the section list.
const fn section_icon(section: SettingsSection) -> crate::icons::Icon {
    match section {
        SettingsSection::Configuration => icondata::LuFileCode,
        SettingsSection::Providers => icondata::LuKeyRound,
        SettingsSection::Plugins => icondata::LuPuzzle,
        SettingsSection::Notifications => icondata::LuBell,
        SettingsSection::Display => icondata::LuEye,
        SettingsSection::Browser => icondata::LuGlobe,
        SettingsSection::About => icondata::LuInfo,
    }
}

/// A Settings section's row: its icon and name; a phone, whose list is the
/// page, says what it holds below.
const SECTION_ROW: &str = "group flex w-full shrink-0 flex-col items-start gap-0.5 rounded-lg px-2.5 py-1.5 text-left transition-colors hover:bg-hover active:bg-press aria-[current=page]:bg-sel compact:min-h-12 compact:px-3 compact:py-2";

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
                    class=SECTION_ROW
                    aria-current=move || current().then_some("page")
                    data-section=section.id()
                    on:click=move |_| workspace.open_section(Some(section))
                >
                    <span class="flex w-full items-center gap-2.5">
                        <Glyph icon=section_icon(section) class="text-base text-muted group-aria-[current=page]:text-accent" />
                        <span class="min-w-0 flex-1 text-[0.8125rem] text-fg-2 group-aria-[current=page]:font-semibold group-aria-[current=page]:text-fg">{section.label()}</span>
                        <Glyph icon=icondata::LuChevronRight class="text-xs opacity-60 sm:hidden" />
                    </span>
                    <span class="hidden pl-[1.625rem] text-xs text-muted compact:block">{section.note()}</span>
                </button>
            }
        })
        .collect_view();
    view! {
        <div class="flex min-h-0 flex-1">
            // On a desktop the section list is the sidebar: Back to the
            // sessions at its top, the connection at its foot
            // (docs/site/specs/web-polish.md, P6).
            <nav
                aria-label="Settings sections"
                class="flex w-full shrink-0 flex-col gap-px overflow-y-auto p-2 sm:w-56 sm:border-r sm:border-line lg:w-64 lg:bg-surface-2"
                class:hidden=move || workspace.shown_section().is_some() && workspace.narrow.get()
            >
                <button
                    type="button"
                    class=format!("{SECTION_ROW} mb-2 hidden lg:flex")
                    data-settings-back
                    on:click=move |_| workspace.open_view(View::Chat, None)
                >
                    <span class="flex w-full items-center gap-2.5">
                        <Glyph icon=icondata::LuArrowLeft class="text-base text-muted" />
                        <span class="min-w-0 flex-1 text-[0.8125rem] text-fg-2">"Back to sessions"</span>
                    </span>
                </button>
                <p class="hidden px-2.5 pt-1 pb-1 text-[0.6875rem] font-semibold text-muted lg:block">"Settings"</p>
                <Show when=move || workspace.onboarding.status.with(Option::is_some)>
                    <button
                        type="button"
                        id="settings-onboard"
                        class=SECTION_ROW
                        on:click=move |_| workspace.open_onboarding()
                    >
                        <span class="flex w-full items-center gap-2.5">
                            <Glyph icon=icondata::LuWandSparkles class="text-base text-accent" />
                            <span class="min-w-0 flex-1 text-[0.8125rem] font-medium text-fg-2">"Set up Cookie"</span>
                        </span>
                        <span class="hidden pl-[1.625rem] text-xs text-muted compact:block">"model providers or an ACP agent, and tools"</span>
                    </button>
                </Show>
                {entries}
                <div class="mt-auto hidden items-center gap-2 border-t border-line px-1 pt-2 lg:flex">
                    <crate::app::ConnectionStatus connection=connection compact=true primary=true />
                    <span class="text-xs text-muted">{concat!("cookie ", env!("CARGO_PKG_VERSION"))}</span>
                </div>
            </nav>
            {move || {
                workspace.shown_section().map(|section| view! {
                    <div class="flex min-h-0 min-w-0 flex-1 flex-col overflow-y-auto px-3 py-4 sm:px-8 sm:py-6" data-page-body data-section-body=section.id()>
                        <div class="mx-auto flex w-full max-w-3xl flex-1 flex-col gap-3">
                            {match section {
                                SettingsSection::Configuration => config_sheet(workspace).into_any(),
                                SettingsSection::Providers => connect_sheet(workspace).into_any(),
                                SettingsSection::Plugins => plugins_sheet(workspace).into_any(),
                                SettingsSection::Notifications => super::notifications::notifications_section(workspace, connection).into_any(),
                                SettingsSection::Display => display_section(workspace).into_any(),
                                SettingsSection::Browser => browser_section(connection).into_any(),
                                SettingsSection::About => crate::app::about_section(connection).into_any(),
                            }}
                        </div>
                    </div>
                })
            }}
        </div>
    }
}

/// Display: how the transcript shows agent work, and the diagnostics it
/// shows, each kept in this browser.
fn display_section(workspace: Workspace) -> impl IntoView {
    view! {
        <h2 class="text-sm font-bold">"Transcript"</h2>
        <p class=NOTE>"How replies show the agent's thinking and tool calls; it stays in this browser."</p>
        {transcript_sheet(workspace)}
        <h2 class="mt-4 text-sm font-bold">"Diagnostics in the transcript"</h2>
        <p class=NOTE>"The same choice as Diagnostics in the session's ⋮ menu; it stays in this browser."</p>
        {events_sheet(workspace)}
    }
}

/// This browser: installing the page as an app, and the daemon's token it
/// keeps.
fn browser_section(connection: Connection) -> impl IntoView {
    let confirming = RwSignal::new(false);
    view! {
        {super::install::install_section()}
        <h2 class="mt-4 text-sm font-bold">"This daemon"</h2>
        <p class=NOTE>"The browser keeps the daemon's token to connect again without a link."</p>
        <ForgetDaemon connection=connection confirming=confirming primary=false on_forget=|| {} />
    }
}
