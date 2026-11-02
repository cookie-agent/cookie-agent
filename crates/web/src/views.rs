//! The connected page: the rail and its views. Chat is the sessions
//! sidebar, the title bar, the transcript, the queue strip, the composer,
//! the bottom bar, and the session's sheets; the other views are full
//! pages ([`shell`]). Wide windows show the sheets on the right, phones
//! from the bottom.

use std::{collections::HashMap, sync::Arc, time::Duration};

use cookie_agent_client_state::{
    EventLevel, ToolStatus,
    badges::{badges_tooltip, session_badges},
    format::{compact_cost, format_cost, format_count, relative_time},
    permissions::{EFFECTS, effect_label, permission_actions, permission_rows, source_label},
    usage::{CacheHit, models_by_cost},
};
use cookie_agent_protocol::{
    ApprovalUserDecision, InputDelivery, ModelSelection, PermissionEffect, PermissionRuleSource,
    SessionId, SessionMeta, SessionRenameChange,
};
use leptos::{ev, html, prelude::*};
use wasm_bindgen::{JsCast as _, closure::Closure};

use crate::{
    commands::{Command, MENU},
    follow::{Follow, Metrics},
    goal::{GoalCommand, action_label, goal_actions, status_name},
    icons::{self, Glyph},
    keys::{ComposerAction, ComposerButtons, KeyPress, composer_action, composer_buttons},
    render::{
        blocks::{BlockPlace, display_html},
        transcript::{
            Delegate, Expanded, PartHtml, RowCache, RowHtml, Rows, TranscriptView, fills_in,
            part_element_id, row_element_id, to_fill_in, transcript_rows,
        },
    },
    workspace::{Panel, Workspace},
};

mod agents;
mod asset_panel;
mod assets;
mod cards;
mod cron;
mod depth;
mod install;
mod live;
mod machines;
mod manage;
mod media_viewer;
mod memory;
mod notifications;
mod onboarding;
mod projects;
mod questions;
mod restart;
mod row_menu;
mod session_rows;
mod shell;
mod sidebar;
mod skills;

pub(crate) use install::{Install, InstallAction, InstallBanner};
pub(crate) use media_viewer::MediaViewer;
pub(crate) use restart::{RestartBanner, RestartDialog};

use depth::{
    AttachButtons, DraftFiles, background_sheet, compact_sheet, deferred_prompt_sheet, goal_sheet,
    message_sheet, permissions_sheet, skills_sheet,
};

use manage::runtimes_sheet;
use shell::{PageView, SidebarButton};
use sidebar::Sidebar;

use crate::{
    app::Connection,
    layout::{Press, View},
    modes::{FOOTNOTE, MODES, mode_name},
    theme::ThemePreference,
    tree::{Activity, delegates_of, path, subagent_label},
};

/// How long a touch must hold a session row to offer its delete.
const LONG_PRESS: Duration = Duration::from_millis(500);

/// How long after the composer loses focus a phone's bottom bar returns.
const BAR_RETURN: Duration = Duration::from_millis(250);

/// Every button is at least 44 px square, with a hover and a pressed state.
/// A labelled button, bordered.
const BUTTON: &str = "inline-flex min-h-11 min-w-11 items-center justify-center gap-2 rounded-md border border-line-2 px-3 text-sm hover:bg-hover active:bg-press disabled:cursor-not-allowed disabled:opacity-50 disabled:hover:bg-transparent";
/// The main action of a form or the composer.
const PRIMARY: &str = "inline-flex min-h-11 min-w-11 items-center justify-center gap-2 rounded-md bg-accent px-3 text-sm font-bold text-on-accent hover:bg-accent active:bg-accent-press disabled:cursor-not-allowed disabled:opacity-50 disabled:hover:bg-accent";
/// A button that is only an icon: it has an `aria-label` and a `title`.
const ICON_BUTTON: &str = "inline-flex min-h-11 min-w-11 shrink-0 items-center justify-center rounded-md text-muted hover:bg-hover hover:text-fg active:bg-press disabled:cursor-not-allowed disabled:opacity-50 disabled:hover:bg-transparent";
/// A bottom bar control showing its current value: the agent, the model,
/// and the permission mode share it. 32 px to see, 44 px to hit.
const CHIP: &str = "hit-area inline-flex h-8 min-w-0 max-w-[16rem] items-center gap-2 rounded-md border border-line-2 bg-surface px-3 text-sm hover:border-accent hover:bg-hover active:bg-press";
/// A status bar indicator that does something when clicked.
/// A small borderless icon button of the sidebar's session tools.
const SIDEBAR_TOOL: &str = "hit-area inline-flex h-7 w-7 shrink-0 items-center justify-center rounded-md text-muted transition-colors hover:bg-hover hover:text-fg active:bg-press";
const STATUS_BUTTON: &str = "inline-flex min-h-11 min-w-11 items-center justify-center gap-1.5 rounded-md px-2 text-xs hover:bg-hover active:bg-press";
/// A session row's buttons (move, rename, delete), which show only over
/// the hovered (or focused) row: side by side, centred on its two lines,
/// laid over the row's right end on the row's hover colour, which fades in
/// from their left. The row keeps its layout underneath: only the part
/// they cover is hidden.
const ROW_ACTIONS: &str = "absolute inset-y-0 right-0 items-center gap-0.5 rounded-r-md pr-0.5 pl-6 bg-[linear-gradient(to_right,transparent,var(--color-hover-solid)_1.5rem)]";
/// One of them: at rest in the row's hover colour so it blends in (the
/// open row's hover colour is the same), one palette step darker on hover
/// (lighter in dark), and further still when pressed. 44 px square.
const ROW_ACTION: &str = "inline-flex min-h-11 min-w-11 items-center justify-center rounded-md bg-sunken text-muted hover:bg-press hover:text-fg active:bg-press";
/// A button of a row's actions a long press revealed: narrower than
/// [`BUTTON`], so that ⋮, Rename, Delete, and ✕ share a phone's line.
const REVEALED: &str = "inline-flex min-h-11 min-w-11 items-center justify-center gap-1.5 rounded-md border border-line-2 px-2 text-sm hover:bg-hover active:bg-press";
/// A session's title as a field: in a session row, and in the title bar.
const TITLE_FIELD: &str = "min-h-9 w-full min-w-0 rounded-md border border-accent bg-surface px-2 text-sm font-bold text-fg focus:outline-none";
/// "Use the automatic title", under an emptied title field.
const AUTO_TITLE: &str = "inline-flex min-h-9 items-center gap-1.5 rounded-md px-2 text-xs font-bold text-accent hover:bg-press active:bg-press";
/// A row of a list: two lines, the second a note.
const ROW: &str = "flex min-h-11 w-full shrink-0 flex-col items-start gap-0.5 rounded-md px-3 py-2 text-left hover:bg-hover active:bg-press";
const NOTE: &str = "text-xs text-muted";
/// A message about something that went wrong.
const ERROR: &str = "rounded-md border border-bad/30 bg-bad-soft px-3 py-2 text-sm whitespace-pre-wrap break-words text-bad";

/// What a sheet shows when it has nothing to list.
fn empty_state(icon: icons::Icon, text: &'static str) -> impl IntoView {
    view! {
        <div class="flex flex-col items-center gap-2 px-4 py-10 text-center text-sm text-muted">
            <Glyph icon=icon class="text-2xl opacity-70" />
            <p>{text}</p>
        </div>
    }
}

/// What a sheet shows while it reads what it lists.
fn loading(text: &'static str) -> impl IntoView {
    view! {
        <p class="flex items-center gap-2 py-4 text-sm text-muted">
            <Glyph icon=icondata::LuLoaderCircle class="animate-spin" />
            {text}
        </p>
    }
}

/// The connected page.
#[component]
pub fn WorkspaceView(
    workspace: Workspace,
    connection: Connection,
    theme: RwSignal<ThemePreference>,
) -> impl IntoView {
    // The view mounts again only when the page comes back from the connect
    // screen (another daemon, say), so its listeners go with it.
    let phone = crate::browser::on_media_change(crate::browser::PHONE_QUERY, move |narrow| {
        workspace.narrow.set(narrow);
    });
    // A window that grows to a desktop's pins the sidebar instead.
    let desktop = crate::browser::on_media_change(crate::browser::DESKTOP_QUERY, move |desktop| {
        workspace.desktop.set(desktop);
        if desktop {
            workspace.sidebar_open.set(false);
        }
    });
    let history = workspace.follow_history();
    let listeners = StoredValue::new_local(vec![phone, desktop, history]);
    on_cleanup(move || {
        for listener in listeners
            .try_update_value(std::mem::take)
            .into_iter()
            .flatten()
            .flatten()
        {
            listener.remove();
        }
    });
    let chat = move || workspace.view.get() == View::Chat;
    // Below desktop widths the sidebar is the page, over the view, which
    // keeps its place under it.
    let paged = move || workspace.sidebar_open.get();
    view! {
        <div class="relative flex min-h-0 flex-1">
            <Sidebar workspace=workspace connection=connection theme=theme />
            <div
                class="flex min-w-0 flex-1"
                class:hidden=move || !chat()
                class:invisible=paged
                inert=move || paged().then_some("")
            >
                <ChatView workspace=workspace />
                <asset_panel::AssetPanel workspace=workspace />
            </div>
            <Show when=move || !chat()>
                <div class="flex min-w-0 flex-1" class:invisible=paged inert=move || paged().then_some("")>
                    <PageView workspace=workspace connection=connection />
                </div>
            </Show>
            <Sheet workspace=workspace />
            <onboarding::OnboardingSheet workspace=workspace />
        </div>
    }
}

/// The chat view's column: the title bar, the transcript, the bars above
/// the composer, the composer, and the bottom bar.
#[component]
fn ChatView(workspace: Workspace) -> impl IntoView {
    // A phone's keyboard leaves room for the composer only: the bottom bar
    // hides while the composer has focus.
    let composing = RwSignal::new(false);
    view! {
            <main
                class="flex min-w-0 flex-1 flex-col"
                on:dragover=move |event: ev::DragEvent| event.prevent_default()
                on:drop=move |event: ev::DragEvent| {
                    event.prevent_default();
                    let files = crate::files::files_of(
                        event.data_transfer().and_then(|transfer| transfer.files()),
                    );
                    if !files.is_empty() {
                        workspace.attach_files(files);
                    }
                }
            >
                <TitleBar workspace=workspace />
                // A new session shows its runtime cards where the
                // transcript will be, above the composer.
                <Transcript workspace=workspace />
                <QueueStrip workspace=workspace />
                <DraftFiles workspace=workspace />
                // An agent's question waits above the composer, scrolling
                // within itself so the transcript keeps some room.
                <div class="max-h-[60dvh] shrink-0 overflow-y-auto px-2 empty:hidden">
                    <questions::QuestionCard workspace=workspace />
                </div>
                <StatusLine workspace=workspace />
                <cards::DraftNotice workspace=workspace />
                // A phone puts the bottom bar above the composer, and a
                // short window (a phone on its side) beside it.
                <div class="relative flex flex-col short:flex-row short:items-end">
                    <StatusTip workspace=workspace />
                    <Composer workspace=workspace composing=composing />
                    <BottomBar workspace=workspace composing=composing />
                </div>
            </main>
    }
}

/// A session's name in the list: its title, else its short ID.
pub(crate) fn session_label(session: &SessionMeta) -> String {
    session
        .title
        .as_ref()
        .map(ToString::to_string)
        .or_else(|| session.short_id.clone())
        .unwrap_or_else(|| session.session_id.to_string())
}

/// The session list's search, lowercased, while its box holds something.
fn search_query(workspace: Workspace) -> Option<String> {
    workspace.session_search.with(|search| {
        search
            .as_deref()
            .map(str::trim)
            .filter(|query| !query.is_empty())
            .map(str::to_lowercase)
    })
}

#[component]
fn SessionList(workspace: Workspace) -> impl IntoView {
    // Whether the search holds something, and the titles it looks in.
    let searching = Memo::new(move |_| search_query(workspace).is_some());
    let labels = session_rows::search_labels(workspace, searching);
    // The root rows a list without projects shows, in order.
    let shown = Memo::new(move |_| {
        let filter = workspace.directory_filter.get();
        let query = search_query(workspace);
        labels.with(|labels| {
            workspace.sessions.with(|sessions| {
                let mut shown = sessions
                    .iter()
                    .filter(|session| {
                        matches!(session.origin, cookie_agent_protocol::SessionOrigin::Root)
                            && filter
                                .as_deref()
                                .is_none_or(|filter| crate::places::matches(session, filter))
                            && workspace.filter_passes(session)
                            && session_rows::search_matches(
                                labels,
                                session.session_id,
                                query.as_deref(),
                            )
                    })
                    .collect::<Vec<_>>();
                // Running and pinned sessions first, then the rest; newest
                // first in each.
                cookie_agent_client_state::rows::order_sessions(&mut shown);
                shown
                    .into_iter()
                    .map(|session| session.session_id)
                    .collect::<Vec<_>>()
            })
        })
    });
    let nothing_shown = Memo::new(move |_| shown.with(Vec::is_empty));
    // The rows' "5m ago" moves on while the list shows.
    let now = RwSignal::new(jiff::Timestamp::now());
    let ticker = set_interval_with_handle(
        move || now.set(jiff::Timestamp::now()),
        Duration::from_secs(30),
    )
    .ok();
    on_cleanup(move || {
        if let Some(ticker) = ticker {
            ticker.clear();
        }
    });
    // Renaming and deleting: the row a long press revealed the actions
    // of, the row whose delete waits for confirmation, and the row whose
    // title is a field.
    let actions = RowActions {
        hovered: RwSignal::new(None),
        revealed: RwSignal::new(None),
        confirming: RwSignal::new(None),
        renaming: RwSignal::new(None),
        draft: RwSignal::new(String::new()),
        fresh: StoredValue::new(false),
        menu: RwSignal::new(None),
        context: RwSignal::new(None),
    };
    // Dragging a session onto a project's header, and the headers' own
    // buttons.
    let drag = projects::SessionDrag::new();
    drag.follow(workspace, actions);
    let headers = projects::HeaderActions::new();
    let list = drag.list;
    let rows = session_rows::SessionRows {
        workspace,
        slots: session_rows::SessionSlots::new(workspace),
        now,
        actions,
        drag,
    };
    // The project sections, worked out once for the list and Select all.
    let sections = projects::listed_sections(workspace, searching, labels);
    // The root rows the list shows, in page order, for Select all.
    let listed = Memo::new(move |_| {
        if workspace.has_projects.get() {
            projects::listed_rows(workspace, sections, searching)
        } else {
            shown.get()
        }
    });
    // A Shift+click ranges over these rows.
    workspace.picking.shown.set_value(Some(listed));
    on_cleanup(move || workspace.picking.shown.set_value(None));
    view! {
        // `relative`: a desktop's filter opens over the list, under its
        // button.
        <div class="relative flex shrink-0 flex-col gap-1 px-2 pt-3">
            <Show when=move || !workspace.picking.active.get() fallback=move || selection_bar(workspace, listed)>
                <SessionTools workspace=workspace />
            </Show>
            <SessionFilterPanel workspace=workspace />
        </div>
        <nav class="flex min-h-0 flex-1 flex-col gap-px overflow-x-hidden overflow-y-auto px-2 pb-3" aria-label="Sessions" node_ref=list>
            {move || {
                if workspace.has_projects.get() {
                    // The filter left every section empty: say so above
                    // them, with the way to change it.
                    let offer = !searching.get()
                        && projects::sections_hold_none(sections)
                        && (workspace.filter_changes() > 0 || workspace.has_older_sessions.get());
                    return view! {
                        {offer.then(|| filter_offer(workspace, icondata::LuListFilter, "No sessions in this time range or filter."))}
                        {projects::project_list(rows, sections, searching, headers)}
                    }
                    .into_any();
                }
                if nothing_shown.get() {
                    if searching.get() {
                        return empty_state(icondata::LuSearch, "No sessions match.").into_any();
                    }
                    if workspace.filter_changes() > 0 {
                        return filter_offer(workspace, icondata::LuListFilter, "No sessions match the filter.").into_any();
                    }
                    // Older sessions than the window reaches: a longer date
                    // in the filter lists them.
                    if workspace.has_older_sessions.get() {
                        return filter_offer(workspace, icondata::LuHistory, "No sessions in this time range; older ones are a longer date away.").into_any();
                    }
                    return empty_state(icondata::LuFolderOpen, "No sessions yet. Start one with New session.")
                        .into_any();
                }
                session_rows::keyed_rows(rows, move || shown.get()).into_any()
            }}
        </nav>
    }
}

/// The second row of the session list's header: Select, Search, and
/// Filter, with what the filter lists beside them. Search turns the row
/// into its search box, which grows out of the Search button as the other
/// buttons fade, and shrinks back into it as they return: the box stays in
/// the page, clipped to the button's place while closed. It takes the
/// focus as it opens; Escape or its × closes it, shows every session again,
/// and gives the focus back to Search.
#[component]
fn SessionTools(workspace: Workspace) -> impl IntoView {
    let row = NodeRef::<html::Div>::new();
    let search_button = NodeRef::<html::Button>::new();
    let field = NodeRef::<html::Input>::new();
    let searching = move || workspace.session_search.with(Option::is_some);
    // Where Search sits in the row, measured as the box opens and closes:
    // the box grows from and shrinks to it.
    let origin = RwSignal::new(String::new());
    let measure = move || {
        if let (Some(row), Some(button)) = (row.get_untracked(), search_button.get_untracked()) {
            let left = button.offset_left();
            let right = (row.client_width() - left - button.offset_width()).max(0);
            origin.set(format!(
                "--search-left: {left}px; --search-right: {right}px"
            ));
        }
    };
    let open = move |_| {
        measure();
        workspace.filter_open.set(false);
        workspace.session_search.set(Some(String::new()));
        // Focused at once, in the tap: a phone opens its keyboard only for
        // a focus the tap itself makes. The page shows the box open only
        // after the tap, so it is opened here first, as it will be.
        if let (Some(row), Some(input)) = (row.get_untracked(), field.get_untracked()) {
            let _ = row.set_attribute("data-searching", "true");
            if let Some(search) = row.query_selector(".session-search").ok().flatten() {
                let _ = search.remove_attribute("inert");
            }
            let _ = input.focus();
        }
    };
    let close = move || {
        measure();
        workspace.session_search.set(None);
        // Given back at once too, the button out of its inert first.
        if let Some(button) = search_button.get_untracked() {
            let _ = button.remove_attribute("inert");
            let _ = button.focus();
        }
    };
    let filter_changes = Memo::new(move |_| workspace.filter_changes());
    // The date, and how many other parts narrow the list: "Last 7 days",
    // or, short enough for the row, "7 days · 2 filters".
    let summary = move || {
        // Rolled back to the newest session's days: from when.
        if let Some(start) = workspace.rollback_start() {
            return format!(
                "Since {}",
                crate::workspace::date_label(start, crate::workspace::local_today())
            );
        }
        let within = workspace.session_filter.with(|filter| filter.within);
        let others = filter_changes.get()
            - usize::from(
                within != cookie_agent_client_state::session_filter::ActiveWithin::default(),
            );
        match others {
            0 => within.summary(crate::workspace::local_today()),
            1 => format!(
                "{} · 1 filter",
                within.short(crate::workspace::local_today())
            ),
            count => format!(
                "{} · {count} filters",
                within.short(crate::workspace::local_today())
            ),
        }
    };
    // A closed search lets go of what it held: it opens empty again.
    Effect::new(move |_| {
        if !searching()
            && let Some(input) = field.get()
        {
            input.set_value("");
        }
    });
    let tool_inert = move || searching().then_some("");
    view! {
        <div
            class="session-tools relative flex h-8 items-center gap-0.5 pl-2"
            data-searching=move || searching().to_string()
            style=move || origin.get()
            node_ref=row
        >
            <p class="session-tool shrink-0 text-[0.6875rem] font-semibold text-muted" data-tool="heading">"Sessions"</p>
            <span
                class="session-tool min-w-0 flex-1 truncate pr-1 pl-2 text-[0.6875rem] text-faint"
                data-tool="summary"
                data-filter-summary
                title=summary
            >
                {summary}
            </span>
            // Several sessions at once: mark them read or unread, or
            // delete them.
            <Show when=move || workspace.keeps_unread.get() || workspace.can_delete.get()>
                <button
                    type="button"
                    class=format!("{SIDEBAR_TOOL} session-tool")
                    data-tool="select"
                    id="select-sessions"
                    aria-label="Select sessions"
                    title="Select sessions"
                    inert=tool_inert
                    on:click=move |_| {
                        workspace.filter_open.set(false);
                        workspace.start_selecting();
                    }
                >
                    <Glyph icon=icondata::LuListChecks />
                </button>
            </Show>
            <button
                type="button"
                class=format!("{SIDEBAR_TOOL} session-tool")
                data-tool="search"
                id="search-sessions"
                aria-label="Search sessions"
                title="Search sessions"
                aria-expanded=move || searching().to_string()
                aria-controls="session-search"
                inert=tool_inert
                node_ref=search_button
                on:click=open
            >
                <Glyph icon=icondata::LuSearch />
            </button>
            <button
                type="button"
                class=format!("{SIDEBAR_TOOL} session-tool relative")
                data-tool="filter"
                id="filter-sessions"
                aria-label="Filter sessions"
                title="Filter sessions"
                aria-haspopup="dialog"
                aria-expanded=move || workspace.filter_open.get().to_string()
                aria-controls="session-filter"
                inert=tool_inert
                on:click=move |_| workspace.filter_open.update(|open| *open = !*open)
            >
                <Glyph icon=icondata::LuListFilter />
                // A dot while the filter narrows the list beyond its
                // default.
                <Show when=move || { filter_changes.get() > 0 }>
                    <span class="absolute top-1 right-1 h-1.5 w-1.5 rounded-full bg-accent ring-2 ring-surface-2" data-filter-dot></span>
                </Show>
            </button>
            <Show when=move || workspace.has_projects.get()>
                <button
                    type="button"
                    class=format!("{SIDEBAR_TOOL} session-tool")
                    data-tool="project"
                    id="new-project"
                    aria-label="New project…"
                    title="New project…"
                    inert=tool_inert
                    on:click=move |_| workspace.open_project_editor(None, false)
                >
                    <Glyph icon=icondata::LuFolderPlus />
                </button>
            </Show>
            <div
                class="session-search absolute inset-0 flex items-center gap-1.5"
                role="search"
                inert=move || (!searching()).then_some("")
            >
                <label class="relative flex min-w-0 flex-1 items-center">
                    <Glyph icon=icondata::LuSearch class="pointer-events-none absolute left-2.5 text-sm text-muted" />
                    <input
                        // Text, not `search`: the browser's own clear
                        // button would sit beside the ×.
                        type="text"
                        role="searchbox"
                        enterkeyhint="search"
                        id="session-search"
                        class="h-8 w-full min-w-0 rounded-lg bg-surface pr-3 pl-8 text-[0.8125rem] text-fg shadow-[inset_0_0_0_1px_var(--color-line-2)] placeholder:text-faint focus:shadow-[inset_0_0_0_1px_var(--color-accent),0_0_0_3px_var(--color-accent-soft)] focus:outline-none"
                        aria-label="Search sessions by title"
                        placeholder="Search by title"
                        node_ref=field
                        prop:value=move || workspace.session_search.get_untracked().unwrap_or_default()
                        on:input=move |event| workspace.session_search.set(Some(event_target_value(&event)))
                        on:keydown=move |event: ev::KeyboardEvent| {
                            if event.key() == "Escape" {
                                event.prevent_default();
                                event.stop_propagation();
                                close();
                            }
                        }
                    />
                </label>
                <button
                    type="button"
                    class=format!("{SIDEBAR_TOOL} session-search-cancel")
                    id="search-sessions-close"
                    aria-label="Close the search"
                    title="Close the search"
                    on:click=move |_| close()
                >
                    <Glyph icon=icondata::LuX />
                </button>
            </div>
        </div>
    }
}

/// The session list's filter: how far back the list reaches, and which
/// agents, machines, and (without projects) workspace its sessions run on.
/// A desktop opens it in place, over the list under its button, and a
/// click outside or Escape closes it; a narrower window drops it down
/// above the list, which it pushes down. The date always lists the pinned,
/// unread, and busy sessions too, whatever their age.
#[component]
fn SessionFilterPanel(workspace: Workspace) -> impl IntoView {
    use cookie_agent_client_state::session_filter::{
        ActiveWithin, DateRange, agent_options, machine_options,
    };
    let open = move || workspace.filter_open.get();
    let close = move || workspace.filter_open.set(false);
    let escape = window_event_listener(ev::keydown, move |event| {
        if event.key() == "Escape" && workspace.filter_open.get_untracked() {
            event.prevent_default();
            workspace.filter_open.set(false);
            if let Some(button) = document()
                .get_element_by_id("filter-sessions")
                .and_then(|button| button.dyn_into::<web_sys::HtmlElement>().ok())
            {
                let _ = button.focus();
            }
        }
    });
    let escape = StoredValue::new_local(Some(escape));
    on_cleanup(move || {
        if let Some(escape) = escape.try_update_value(Option::take).flatten() {
            escape.remove();
        }
    });
    let roots = move || {
        workspace.sessions.with(|sessions| {
            sessions
                .iter()
                .filter(|session| {
                    matches!(session.origin, cookie_agent_protocol::SessionOrigin::Root)
                })
                .cloned()
                .collect::<Vec<_>>()
        })
    };
    let agents = Memo::new(move |_| {
        let chosen = workspace
            .session_filter
            .with(|filter| filter.agents.clone());
        workspace
            .sessions
            .with(|sessions| agent_options(sessions, &chosen))
    });
    let machines = Memo::new(move |_| {
        let chosen = workspace
            .session_filter
            .with(|filter| filter.machines.clone());
        workspace.runtimes.with(|listing| {
            workspace.sessions.with(|sessions| {
                machine_options(
                    sessions
                        .iter()
                        .filter(|session| {
                            matches!(session.origin, cookie_agent_protocol::SessionOrigin::Root)
                        })
                        .map(|session| {
                            cookie_agent_client_state::badges::session_machines(
                                session,
                                listing.as_ref(),
                            )
                        }),
                    &chosen,
                )
            })
        })
    });
    // Each runtime at each workspace sessions work in, under its home.
    let places = move || {
        workspace.runtime_homes.with(|homes| {
            let roots = roots();
            crate::places::filter_options(&roots, homes)
        })
    };
    let chip = move |label: String,
                     chosen: Signal<bool>,
                     toggle: Callback<()>,
                     data: &'static str| {
        let value = label.clone();
        view! {
            <button
                type="button"
                class=move || format!(
                    "inline-flex min-h-11 max-w-full items-center gap-1.5 rounded-full border px-3 text-sm lg:min-h-9 {}",
                    if chosen.get() {
                        "border-fg bg-accent text-on-accent hover:bg-accent"
                    } else {
                        "border-line-2 hover:bg-hover active:bg-press"
                    },
                )
                aria-pressed=move || chosen.get().to_string()
                data-filter-chip=data
                data-value=value
                on:click=move |_| toggle.run(())
            >
                <Show when=move || chosen.get()>
                    <Glyph icon=icondata::LuCheck class="text-xs" />
                </Show>
                <span class="truncate">{label}</span>
            </button>
        }
    };
    let heading = "text-xs font-bold text-muted";
    // The range the filter lists, while it lists one.
    let range = move || {
        workspace.session_filter.with(|filter| match filter.within {
            ActiveWithin::Between(range) => Some(range),
            _ => None,
        })
    };
    let ranged = move || range().is_some();
    // A range's first or last day, as a date field: a change keeps the
    // other end, unless it passes it, which then moves to the same day.
    let date_field = move |label: &'static str, id: &'static str, first: bool| {
        let value = move || {
            range()
                .map(|range| if first { range.from } else { range.to }.to_string())
                .unwrap_or_default()
        };
        view! {
            <label class="flex min-w-36 flex-1 flex-col gap-1 text-xs font-bold text-muted">
                <span>{label}</span>
                <input
                    type="date"
                    id=id
                    class="min-h-11 w-full rounded-md border border-line-2 bg-surface px-2 text-sm font-normal text-fg hover:border-accent focus:border-accent focus:outline-none"
                    max=move || crate::workspace::local_today().to_string()
                    prop:value=value
                    on:change=move |event| {
                        let (Ok(day), Some(current)) =
                            (event_target_value(&event).parse::<jiff::civil::Date>(), range())
                        else {
                            return;
                        };
                        // The other end follows a day past it.
                        let range = if first {
                            DateRange::new(day, current.to.max(day))
                        } else {
                            DateRange::new(current.from.min(day), day)
                        };
                        workspace.set_active_within(ActiveWithin::Between(range));
                    }
                />
            </label>
        }
    };
    view! {
        // A desktop's click outside closes it.
        <Show when=open>
            <div class="fixed inset-0 z-30 hidden lg:block" data-filter-backdrop on:click=move |_| close()></div>
        </Show>
        <div
            class="session-filter grid lg:absolute lg:top-[7rem] lg:left-3 lg:z-40 lg:w-80"
            data-open=move || open().to_string()
            inert=move || (!open()).then_some("")
        >
            <div class="min-h-0 overflow-hidden">
                <div
                    id="session-filter"
                    role="dialog"
                    aria-label="Filter sessions"
                    class="flex flex-col gap-3 rounded-lg border border-line bg-surface p-3 text-fg lg:shadow-lg"
                >
                    <div class="flex flex-col gap-1.5">
                        <span class=heading id="filter-within">"Last active"</span>
                        <div class="flex flex-wrap gap-1.5" role="radiogroup" aria-labelledby="filter-within">
                            {ActiveWithin::ALL
                                .into_iter()
                                .map(|within| {
                                    let chosen = Signal::derive(move || {
                                        workspace.session_filter.with(|filter| filter.within == within)
                                    });
                                    view! {
                                        <button
                                            type="button"
                                            role="radio"
                                            class=move || format!(
                                                "inline-flex min-h-11 items-center rounded-full border px-3 text-sm lg:min-h-9 {}",
                                                if chosen.get() {
                                                    "border-fg bg-accent text-on-accent"
                                                } else {
                                                    "border-line-2 hover:bg-hover active:bg-press"
                                                },
                                            )
                                            aria-checked=move || chosen.get().to_string()
                                            data-within=within.id()
                                            on:click=move |_| workspace.set_active_within(within)
                                        >
                                            {within.label()}
                                        </button>
                                    }
                                })
                                .collect_view()}
                            // A range of days: the last week to start with,
                            // its dates below.
                            <button
                                type="button"
                                role="radio"
                                class=move || format!(
                                    "inline-flex min-h-11 items-center gap-1.5 rounded-full border px-3 text-sm lg:min-h-9 {}",
                                    if ranged() {
                                        "border-fg bg-accent text-on-accent"
                                    } else {
                                        "border-line-2 hover:bg-hover active:bg-press"
                                    },
                                )
                                aria-checked=move || ranged().to_string()
                                data-within="between"
                                on:click=move |_| {
                                    if !ranged() {
                                        workspace.set_active_within(ActiveWithin::Between(DateRange::ending(
                                            crate::workspace::local_today(),
                                            7,
                                        )));
                                    }
                                }
                            >
                                <Glyph icon=icondata::LuCalendarDays />
                                "Dates…"
                            </button>
                        </div>
                        <Show when=ranged>
                            <div class="flex flex-wrap gap-2" data-date-range>
                                {date_field("From", "filter-from", true)}
                                {date_field("To", "filter-to", false)}
                            </div>
                        </Show>
                        {move || workspace.session_rollback.get().map(|newest| {
                            let today = crate::workspace::local_today();
                            let newest = crate::workspace::date_label(crate::workspace::local_date(newest), today);
                            view! {
                                <p class=format!("flex items-center gap-1.5 {NOTE}") data-rolled-back>
                                    <Glyph icon=icondata::LuHistory class="text-accent" />
                                    {format!("Nothing in the last 7 days: listing the 7 days up to the newest session ({newest}), and on to now.")}
                                </p>
                            }
                        })}
                        <p class=format!("flex items-center gap-1.5 {NOTE}")>
                            <Glyph icon=icondata::LuPin class="text-accent" />
                            "Pinned, unread, and busy sessions always show."
                        </p>
                    </div>
                    <Show when=move || agents.with(|agents| !agents.is_empty())>
                        <div class="flex flex-col gap-1.5">
                            <span class=heading>"Agent"</span>
                            <div class="flex flex-wrap gap-1.5">
                                {move || agents.get().into_iter().map(|agent| {
                                    let key = agent.clone();
                                    let chosen = Signal::derive({
                                        let key = key.clone();
                                        move || workspace.session_filter.with(|filter| filter.agents.contains(&key))
                                    });
                                    let toggle = Callback::new(move |()| {
                                        workspace.session_filter.update(|filter| filter.toggle_agent(&key));
                                    });
                                    chip(agent, chosen, toggle, "agent")
                                }).collect_view()}
                            </div>
                        </div>
                    </Show>
                    <Show when=move || machines.with(|machines| !machines.is_empty())>
                        <div class="flex flex-col gap-1.5">
                            <span class=heading>"Machine"</span>
                            <div class="flex flex-wrap gap-1.5">
                                {move || machines.get().into_iter().map(|machine| {
                                    let key = machine.clone();
                                    let chosen = Signal::derive({
                                        let key = key.clone();
                                        move || workspace.session_filter.with(|filter| filter.machines.contains(&key))
                                    });
                                    let toggle = Callback::new(move |()| {
                                        workspace.session_filter.update(|filter| filter.toggle_machine(&key));
                                    });
                                    chip(machine, chosen, toggle, "machine")
                                }).collect_view()}
                            </div>
                        </div>
                    </Show>
                    // A daemon without projects: the workspace filter, as
                    // project sections stand in for it.
                    <Show when=move || !workspace.has_projects.get()>
                        <label class="flex flex-col gap-1.5">
                            <span class=heading>"Workspace"</span>
                            <select
                                id="place-filter"
                                class="min-h-11 rounded-md border border-line-2 bg-surface px-2 text-sm font-normal text-fg hover:border-accent"
                                on:change=move |event| {
                                    let value = event_target_value(&event);
                                    workspace.directory_filter.set((!value.is_empty()).then_some(value));
                                }
                            >
                                <option value="">"All workspaces"</option>
                                {move || {
                                    places()
                                        .into_iter()
                                        .map(|(value, label)| {
                                            let selected = workspace.directory_filter.get().as_deref()
                                                == Some(value.as_str());
                                            view! {
                                                <option value=value selected=selected>
                                                    {label}
                                                </option>
                                            }
                                        })
                                        .collect_view()
                                }}
                            </select>
                        </label>
                    </Show>
                    <div class="flex items-center justify-end gap-1.5 border-t border-line pt-2">
                        <button
                            type="button"
                            class=BUTTON
                            id="filter-reset"
                            disabled=move || workspace.filter_changes() == 0
                            on:click=move |_| workspace.reset_session_filter()
                        >
                            "Reset"
                        </button>
                        <button type="button" class=PRIMARY id="filter-done" on:click=move |_| close()>
                            "Done"
                        </button>
                    </div>
                </div>
            </div>
        </div>
    }
}

/// What the session list says when the filter leaves it empty, with the
/// button that opens the filter.
fn filter_offer(workspace: Workspace, icon: icons::Icon, text: &'static str) -> impl IntoView {
    view! {
        <div class="flex flex-col items-center gap-3 px-4 py-10 text-center text-sm text-muted" data-filter-offer>
            <Glyph icon=icon class="text-2xl opacity-70" />
            <p>{text}</p>
            <button
                type="button"
                class=BUTTON
                on:click=move |_| workspace.filter_open.set(true)
            >
                <Glyph icon=icondata::LuListFilter />
                "Change the filter"
            </button>
        </div>
    }
}

/// The session list's header in select mode: how many are selected,
/// Select all (Unselect all once every row the list shows is selected)
/// and Done, and what the selection can do: mark read, mark unread,
/// delete (behind a confirmation naming them).
fn selection_bar(workspace: Workspace, listed: Memo<Vec<SessionId>>) -> impl IntoView {
    let picking = workspace.picking;
    let count = move || picking.picked.with(crate::workspace::Picked::len);
    let all_picked =
        move || listed.with(|rows| workspace.pick_state(rows)) == crate::workspace::PickState::All;
    let idle = move || count() == 0 || picking.deleting.get();
    let actions = move || {
        view! {
            <div class="flex flex-wrap gap-2">
                <Show when=move || workspace.keeps_unread.get()>
                    <button type="button" class=BUTTON id="mark-picked-read" disabled=idle on:click=move |_| workspace.mark_picked(false)>
                        <Glyph icon=icondata::LuMailOpen />
                        "Mark read"
                    </button>
                    <button type="button" class=BUTTON id="mark-picked-unread" disabled=idle on:click=move |_| workspace.mark_picked(true)>
                        <Glyph icon=icondata::LuMail />
                        "Mark unread"
                    </button>
                </Show>
                <Show when=move || workspace.can_delete.get()>
                    <button type="button" class=BUTTON id="delete-picked" disabled=idle on:click=move |_| picking.confirming.set(true)>
                        <Glyph icon=icondata::LuTrash2 />
                        {move || if picking.deleting.get() { "Deleting…" } else { "Delete" }}
                    </button>
                </Show>
            </div>
        }
    };
    view! {
        <div class="flex flex-col gap-2" id="selection-bar">
            <div class="flex items-center gap-2">
                <span class="min-w-0 flex-1 truncate text-sm font-bold" data-picked-count>
                    {move || match count() {
                        0 => "Select sessions".to_owned(),
                        1 => "1 selected".to_owned(),
                        count => format!("{count} selected"),
                    }}
                </span>
                <button
                    type="button"
                    class=BUTTON
                    id="select-all"
                    on:click=move |_| {
                        if all_picked() {
                            workspace.unpick_all();
                        } else {
                            workspace.pick_all(listed.get_untracked());
                        }
                    }
                >
                    {move || if all_picked() { "Unselect all" } else { "Select all" }}
                </button>
                <button type="button" class=BUTTON id="select-done" on:click=move |_| workspace.stop_selecting()>
                    "Done"
                </button>
            </div>
            <Show when=move || picking.confirming.get() fallback=actions>
                <div class="flex flex-col gap-2 rounded-lg border border-accent bg-surface p-2 text-sm" role="alertdialog" data-delete-picked-box>
                    <p>
                        {move || {
                            let sessions = match count() {
                                1 => "1 session".to_owned(),
                                count => format!("{count} sessions"),
                            };
                            format!(
                                "Delete {sessions} and their subagents: {}? This cannot be undone. Running ones are skipped.",
                                workspace.picked_titles(),
                            )
                        }}
                    </p>
                    <div class="flex flex-wrap gap-2">
                        <button type="button" class=PRIMARY id="delete-picked-confirm" on:click=move |_| workspace.delete_picked()>
                            "Delete"
                        </button>
                        <button type="button" class=BUTTON id="delete-picked-cancel" on:click=move |_| picking.confirming.set(false)>
                            "Cancel"
                        </button>
                    </div>
                </div>
            </Show>
        </div>
    }
}

/// The session list's row actions, shared by its rows: rename, delete,
/// and the ⋮ menu (Move to project…).
#[derive(Clone, Copy)]
struct RowActions {
    /// The row a mouse (or pen) is over, which shows its buttons.
    hovered: RwSignal<Option<SessionId>>,
    /// The row a long press offered its actions under.
    revealed: RwSignal<Option<SessionId>>,
    confirming: RwSignal<Option<SessionId>>,
    /// The row whose title is a field, and what the field holds; kept
    /// here, so a list redrawn while the user types keeps it.
    renaming: RwSignal<Option<SessionId>>,
    draft: RwSignal<String>,
    /// The field just opened: its text is selected once it shows.
    fresh: StoredValue<bool>,
    /// The row whose ⋮ menu is open, and whether it is the ⋮ a long press
    /// revealed (or the one on hover).
    menu: RwSignal<Option<(SessionId, bool)>>,
    /// The row a right-click opened its menu over, and where.
    context: RwSignal<Option<(SessionId, f64, f64)>>,
}

impl RowActions {
    /// Turns `session`'s title into a field holding it.
    fn begin_rename(self, workspace: Workspace, session_id: SessionId) {
        self.menu.set(None);
        self.revealed.set(None);
        self.confirming.set(None);
        self.draft.set(workspace.title_draft(session_id));
        self.fresh.set_value(true);
        self.renaming.set(Some(session_id));
    }
}

/// A root session's row: a click opens it. On hover (or a long press on
/// a touch screen) its buttons offer to rename it, in place, and to
/// delete it, behind an inline confirmation. Its second line says where
/// it runs, as badges. The open session's row has its subagents under it.
fn session_row(
    workspace: Workspace,
    session: SessionMeta,
    now: RwSignal<jiff::Timestamp>,
    actions: RowActions,
    drag: projects::SessionDrag,
) -> impl IntoView {
    let session_id = session.session_id;
    let current = move || workspace.selected.get() == Some(session_id);
    // The root of the open tree, whether the root itself or one of its
    // subagents is open, and only when it has subagents: its row keeps the
    // chevron that folds and unfolds the tree.
    let tree_root =
        move || workspace.tree_root.get() == Some(session_id) && workspace.tree_has_subagents();
    let mark = icons::session_status(session.status);
    let activity = cookie_agent_client_state::activity::session_activity(&session);
    let status_mark = icons::row_status(&activity);
    // What the row's tooltip and a screen reader say of it: what it waits
    // on, else its status.
    let status_text = match &activity {
        cookie_agent_client_state::activity::SessionActivity::Waiting(kinds) => {
            cookie_agent_client_state::activity::waiting_label(kinds)
        }
        cookie_agent_client_state::activity::SessionActivity::Held(reason) => format!(
            "held: {}",
            cookie_agent_client_state::held::reason_label(reason)
        ),
        _ => mark.label.to_owned(),
    };
    let last_activity = session.last_activity;
    let unread = session.unread;
    let pinned = session.pinned;
    // A run session of a cron job: its clock opens the job.
    let cron = session.cron.clone();
    // Where it runs, once the runtimes are known; the tooltip lists every
    // workspace with its path.
    // Fitted to the line: machines whole, the last ones left out for a
    // `+N` when even the workspaces gone to `…` leave too little room.
    // A memo: a runtimes change that leaves this row's badges as they were
    // (a run starting elsewhere, say) does not draw them again.
    let shown_badges = {
        let session = session.clone();
        Memo::new(move |_| {
            let listed = workspace.lists_attachments.get();
            workspace.runtime_homes.with(|homes| {
                workspace
                    .runtimes
                    .with(|listing| session_badges(&session, listing.as_ref(), homes, listed))
            })
        })
    };
    let badges = move |line: NodeRef<html::Span>| {
        let shown = shown_badges.get();
        crate::badges::fit_soon(line);
        crate::badges::badge_line(shown)
    };
    let badge_line = NodeRef::<html::Span>::new();
    crate::badges::keep_fitted(badge_line);
    let title = {
        let session = session.clone();
        let status = status_text.clone();
        let title = Memo::new(move |_| {
            let listed = workspace.lists_attachments.get();
            let places = workspace.runtime_homes.with(|homes| {
                workspace
                    .runtimes
                    .with(|listing| badges_tooltip(&session, listing.as_ref(), homes, listed))
            });
            format!("{places}\n{status}")
        });
        move || title.get()
    };
    let label = session_label(&session);
    let confirm_label = label.clone();
    let menu_label = label.clone();
    let reveal_label = label.clone();
    let context_label = label.clone();
    let RowActions {
        hovered,
        revealed,
        confirming,
        renaming,
        menu,
        ..
    } = actions;
    let is_renaming = move || renaming.get() == Some(session_id);
    // In select mode a click selects the row, and its buttons stay away.
    let picking = move || workspace.picking.active.get();
    let picked = move || workspace.is_picked(session_id);
    // The hover buttons stay while their ⋮ menu is open.
    let shown = move || {
        (hovered.get() == Some(session_id) || menu.get() == Some((session_id, false)))
            && !is_renaming()
            && !picking()
    };
    // The ⋮ menu's actions; a long press's row lets go of its buttons
    // first.
    let menu_items = move |revealed_row: bool| {
        move || {
            let mut items = Vec::new();
            if workspace.keeps_pins.get() {
                items.push(row_menu::MenuItem {
                    id: if pinned { "unpin" } else { "pin" },
                    label: if pinned { "Unpin" } else { "Pin" },
                    icon: icondata::LuPin,
                    act: Callback::new(move |()| {
                        if revealed_row {
                            revealed.set(None);
                        }
                        workspace.set_pinned(vec![session_id], !pinned);
                    }),
                    danger: false,
                });
            }
            if workspace.has_projects.get() {
                items.push(row_menu::MenuItem {
                    id: "move",
                    label: "Move to project…",
                    icon: icondata::LuFolderOpen,
                    act: Callback::new(move |()| {
                        if revealed_row {
                            revealed.set(None);
                        }
                        workspace.ask_move(session_id);
                    }),
                    danger: false,
                });
            }
            if workspace.keeps_unread.get() {
                items.push(row_menu::MenuItem {
                    id: if unread { "mark-read" } else { "mark-unread" },
                    label: if unread { "Mark read" } else { "Mark unread" },
                    icon: if unread {
                        icondata::LuMailOpen
                    } else {
                        icondata::LuMail
                    },
                    act: Callback::new(move |()| {
                        if revealed_row {
                            revealed.set(None);
                        }
                        workspace.set_unread(vec![session_id], !unread);
                    }),
                    danger: false,
                });
            }
            items
        }
    };
    // A right-click's menu: everything the row offers, Rename first and
    // Delete last.
    let context_items = move || {
        let mut items = vec![row_menu::MenuItem {
            id: "rename",
            label: "Rename",
            icon: icondata::LuPencil,
            act: Callback::new(move |()| actions.begin_rename(workspace, session_id)),
            danger: false,
        }];
        items.extend(menu_items(false)());
        if workspace.can_delete.get() {
            items.push(row_menu::MenuItem {
                id: "delete",
                label: "Delete…",
                icon: icondata::LuTrash2,
                act: Callback::new(move |()| {
                    menu.set(None);
                    revealed.set(None);
                    confirming.set(Some(session_id));
                }),
                danger: true,
            });
        }
        items
    };
    // A mouse drags the row onto a project's header at once; a finger
    // after a long press, which let go in place reveals the buttons.
    let on_pointerdown = move |event: ev::PointerEvent| {
        if !is_renaming() && !picking() {
            drag.press(workspace, session_id, &event);
        }
    };
    let lifted = move || drag.lifts(session_id);
    view! {
        <div
            class=move || format!(
                "session-row group relative {}",
                if lifted() {
                    "pointer-events-none z-30 rounded-md bg-surface shadow-xl ring-2 ring-accent"
                } else {
                    ""
                },
            )
            style:transform=move || {
                let (dx, dy) = drag.offset.get();
                if lifted() { format!("translate({dx}px, {dy}px) scale(1.02)") } else { String::new() }
            }
            data-dragging=move || lifted().then_some("true")
            data-session-row=session_id.to_string()
            on:pointerenter=move |event: ev::PointerEvent| {
                if event.pointer_type() != "touch" {
                    hovered.set(Some(session_id));
                }
            }
            on:pointerleave=move |_| {
                if hovered.get_untracked() == Some(session_id) {
                    hovered.set(None);
                }
            }
        >
            <Show
                when=move || !is_renaming()
                fallback=move || rename_row(workspace, session_id, status_mark, actions, badges)
            >
            <button
                type="button"
                // The row looks hovered while the pointer is on its buttons
                // too, which sit over it, or the keyboard is in it.
                // One class closure for all of it: `class:` toggles beside
                // it would be dropped each time it changes.
                class=move || format!(
                    "{ROW} no-callout select-none pr-3 {} group-hover:bg-hover group-has-focus-visible:bg-hover",
                    if current() || (picking() && picked()) { "bg-sunken" } else { "" },
                )
                aria-current=move || current().then_some("true")
                aria-pressed=move || picking().then(|| picked().to_string())
                data-session=session_id.to_string()
                title=title
                on:pointerdown=on_pointerdown
                on:contextmenu=move |event: ev::MouseEvent| {
                    // A touch screen's long press lifts the row and offers
                    // its actions, not the browser's menu.
                    if lifted() || revealed.get_untracked() == Some(session_id) {
                        event.prevent_default();
                        return;
                    }
                    // A mouse's right-click opens the row's menu there.
                    if !crate::browser::from_touch(&event) && row_menu::wants_context_menu(&event) {
                        event.prevent_default();
                        menu.set(None);
                        actions.context.set(Some((
                            session_id,
                            f64::from(event.client_x()),
                            f64::from(event.client_y()),
                        )));
                    }
                }
                // The open session's row switches its subagent tree between
                // the running ones and all of them.
                aria-expanded=move || tree_root().then(|| workspace.tree_expanded.get().to_string())
                on:click=move |event: ev::MouseEvent| {
                    if picking() {
                        workspace.toggle_pick(session_id, event.shift_key());
                        return;
                    }
                    if !drag.take_click() {
                        if current() && workspace.sidebar_open.get_untracked() && !workspace.new_session.get_untracked() {
                            // The sessions as the page, over the open
                            // session: its row goes back to it.
                            workspace.sidebar_open.set(false);
                        } else if current() && tree_root() && !workspace.new_session.get_untracked() {
                            workspace.toggle_tree();
                        } else {
                            workspace.open_session(session_id);
                        }
                    }
                }
            >
                <span class="flex w-full items-center gap-2">
                    // In select mode, whether the row is selected.
                    <Show when=picking>
                        <span class="inline-flex shrink-0 text-accent" data-pick=move || picked().to_string()>
                            <Glyph icon=Signal::derive(move || if picked() { icondata::LuSquareCheck } else { icondata::LuSquare }) />
                        </span>
                    </Show>
                    // The status slot, only when there is a mark.
                    {status_mark.map(|mark| view! {
                        <span class="inline-flex shrink-0" data-session-activity=mark.label>
                            <Glyph icon=mark.icon class=mark.class("text-xs") />
                        </span>
                    })}
                    <span class="sr-only">{status_text.clone()}</span>
                    // Before the title: the row's buttons cover its right
                    // end on hover.
                    {unread.then(|| view! {
                        <span
                            class="h-2 w-2 shrink-0 rounded-full bg-accent"
                            data-unread
                            title="Unread: it finished, or asked for you, while you were away"
                        ></span>
                        <span class="sr-only">"unread"</span>
                    })}
                    {pinned.then(|| view! {
                        <span
                            class="inline-flex shrink-0 rotate-45 text-xs text-accent"
                            data-pinned-session
                            title="Pinned: listed with the sessions at work, whatever its age"
                        >
                            <Glyph icon=icondata::LuPin />
                        </span>
                        <span class="sr-only">"pinned"</span>
                    })}
                    {cron.clone().map(|cron| cron::clock_mark(workspace, cron))}
                    // The whole title on hover, as a long one is cut here.
                    <span class="min-w-0 flex-1 truncate text-sm font-bold" data-session-title title=label.clone()>{label.clone()}</span>
                    // The time sits beside the title, so the badges have
                    // the whole second line.
                    <span class=format!("shrink-0 {NOTE}") data-session-time>
                        {move || relative_time(now.get(), last_activity)}
                    </span>
                </span>
                <span class=format!("flex w-full min-w-0 items-center gap-1.5 {NOTE}")>
                    // The open session's row folds and unfolds its subagent
                    // tree: its chevron, before the badges, turns with it.
                    // With a subagent open, the row's click opens the root
                    // again, so the chevron folds and unfolds on its own.
                    <Show when=tree_root>
                        <span
                            class="hit-area relative inline-flex shrink-0 opacity-70 transition-transform hover:opacity-100"
                            class:rotate-90=move || workspace.tree_expanded.get()
                            data-tree-chevron
                            title="Show every subagent, or only the running ones"
                            // Its own tap always: on a phone, the open
                            // session's row goes back to it instead.
                            on:click=move |event: ev::MouseEvent| {
                                event.stop_propagation();
                                workspace.toggle_tree();
                            }
                        >
                            <Glyph icon=icondata::LuChevronRight class="text-[0.625rem]" />
                        </span>
                    </Show>
                    <span class="flex min-w-0 flex-1 items-center gap-1 overflow-hidden" node_ref=badge_line data-session-badges>
                        {move || badges(badge_line)}
                    </span>
                </span>
            </button>
            // Shown while a mouse is over the row, or the keyboard is in
            // it; a touch screen's long press offers them below. Their
            // group grows leftwards: the ⋮ goes before the pencil, and
            // rarer actions go in its menu.
            <div
                class=move || {
                    let shown = if shown() {
                        "flex"
                    } else {
                        "hidden group-has-focus-visible:flex"
                    };
                    format!("{ROW_ACTIONS} {shown}")
                }
                data-row-actions
            >
                {row_menu::row_menu(menu, (session_id, false), session_id.to_string(), menu_label.clone(), ROW_ACTION, menu_items(false))}
                <button
                    type="button"
                    class=ROW_ACTION
                    aria-label="Rename the session"
                    title="Rename the session"
                    data-rename=session_id.to_string()
                    on:click=move |_| actions.begin_rename(workspace, session_id)
                >
                    <Glyph icon=icondata::LuPencil />
                </button>
                <Show when=move || workspace.can_delete.get()>
                    <button
                        type="button"
                        class=ROW_ACTION
                        aria-label="Delete the session"
                        title="Delete the session and its subagents"
                        data-delete=session_id.to_string()
                        on:click=move |_| {
                            menu.set(None);
                            revealed.set(None);
                            confirming.set(Some(session_id));
                        }
                    >
                        <Glyph icon=icondata::LuTrash2 />
                    </button>
                </Show>
            </div>
            </Show>
        </div>
        {row_menu::context_menu(actions.context, session_id, context_label.clone(), context_items)}
        <Show when=move || revealed.get() == Some(session_id) && confirming.get() != Some(session_id)>
            // Labels only where the line fits them: on a narrow phone
            // Rename and Delete are icons, named by their aria-labels, so
            // the four buttons stay on one line. The ⋮ is always an icon.
            <div class="@container" data-delete-actions-frame>
            <div class="flex flex-wrap gap-1.5 px-2 pb-1" data-delete-actions>
                {row_menu::row_menu(menu, (session_id, true), session_id.to_string(), reveal_label.clone(), REVEALED, menu_items(true))}
                <button
                    type="button"
                    class=REVEALED
                    aria-label="Rename"
                    data-rename-reveal=session_id.to_string()
                    on:click=move |_| actions.begin_rename(workspace, session_id)
                >
                    <Glyph icon=icondata::LuPencil />
                    <span class="hidden @[22rem]:inline">"Rename"</span>
                </button>
                <Show when=move || workspace.can_delete.get()>
                    <button
                        type="button"
                        class=REVEALED
                        aria-label="Delete"
                        data-delete-reveal=session_id.to_string()
                        on:click=move |_| confirming.set(Some(session_id))
                    >
                        <Glyph icon=icondata::LuTrash2 />
                        <span class="hidden @[22rem]:inline">"Delete"</span>
                    </button>
                </Show>
                // An icon, so the four fit a phone's session row on one line.
                <button
                    type="button"
                    class=BUTTON.replace("px-3", "px-0")
                    aria-label="Cancel"
                    title="Cancel"
                    data-actions-cancel
                    on:click=move |_| revealed.set(None)
                >
                    <Glyph icon=icondata::LuX />
                </button>
            </div>
            </div>
        </Show>
        <Show when=move || confirming.get() == Some(session_id)>
            <div class="mx-2 mb-1 flex flex-col gap-2 rounded-lg border border-accent bg-surface p-2 text-sm" role="alertdialog" data-delete-confirm-box>
                <p>{format!("Delete “{confirm_label}” and its subagents? This cannot be undone.")}</p>
                <div class="flex flex-wrap gap-2">
                    <button
                        type="button"
                        class=PRIMARY
                        data-delete-confirm=session_id.to_string()
                        on:click=move |_| {
                            confirming.set(None);
                            revealed.set(None);
                            workspace.delete_session(session_id);
                        }
                    >
                        "Delete"
                    </button>
                    <button
                        type="button"
                        class=BUTTON
                        on:click=move |_| {
                            confirming.set(None);
                            revealed.set(None);
                        }
                    >
                        "Keep it"
                    </button>
                </div>
            </div>
        </Show>
        {move || {
            workspace
                .delete_error
                .get()
                .filter(|(failed, _)| *failed == session_id)
                .map(|(_, message)| view! { <p class=format!("{ERROR} mx-2 mb-1") data-delete-error>{message}</p> })
        }}
        {move || {
            workspace
                .rename_error
                .get()
                .filter(|(failed, _)| *failed == session_id)
                .map(|(_, message)| view! { <p class=format!("{ERROR} mx-2 mb-1") data-rename-error>{message}</p> })
        }}
        <Show when=move || workspace.tree_root.get() == Some(session_id)>
            <Subagents workspace=workspace />
        </Show>
    }
}

/// A row whose title is a field: Enter renames, Escape (or leaving it)
/// keeps the title, and an emptied field offers the automatic title.
fn rename_row<B: IntoView + 'static>(
    workspace: Workspace,
    session_id: SessionId,
    mark: Option<icons::StatusMark>,
    actions: RowActions,
    badges: impl Fn(NodeRef<html::Span>) -> B + Send + Sync + Clone + 'static,
) -> impl IntoView {
    let line = NodeRef::<html::Span>::new();
    crate::badges::keep_fitted(line);
    let RowActions {
        renaming,
        draft,
        fresh,
        ..
    } = actions;
    let field = NodeRef::<html::Input>::new();
    // The field takes the focus as it shows; its text is selected the
    // first time, so typing replaces the title.
    Effect::new(move |_| {
        if let Some(input) = field.get() {
            request_animation_frame(move || {
                let _ = input.focus();
                if fresh.try_get_value() == Some(true) {
                    fresh.set_value(false);
                    input.select();
                }
            });
        }
    });
    let cancel = move || {
        if renaming.get_untracked() == Some(session_id) {
            renaming.set(None);
        }
    };
    let save = move || {
        if workspace.submit_title(session_id, &draft.get_untracked()) {
            renaming.set(None);
        }
    };
    let empty = move || draft.with(|draft| draft.trim().is_empty());
    view! {
        <div
            class="flex min-h-11 w-full flex-col items-start gap-1 rounded-md bg-sunken px-3 py-2"
            data-renaming=session_id.to_string()
            on:focusout=move |event: ev::FocusEvent| {
                // Leaving the row keeps the title; moving within it does not.
                let inside = event
                    .related_target()
                    .and_then(|target| target.dyn_into::<web_sys::Node>().ok())
                    .zip(event.current_target().and_then(|target| target.dyn_into::<web_sys::Node>().ok()))
                    .is_some_and(|(target, row)| row.contains(Some(&target)));
                if !inside {
                    cancel();
                }
            }
        >
            <span class="flex w-full items-center gap-2">
                {mark.map(|mark| view! { <Glyph icon=mark.icon class=mark.class("text-xs") /> })}
                <input
                    type="text"
                    node_ref=field
                    class=TITLE_FIELD
                    aria-label="Session title"
                    placeholder="The automatic title"
                    autocomplete="off"
                    data-rename-field=session_id.to_string()
                    prop:value=move || draft.get()
                    on:input=move |event| draft.set(event_target_value(&event))
                    on:keydown=move |event: ev::KeyboardEvent| match event.key().as_str() {
                        "Enter" => {
                            event.prevent_default();
                            save();
                        }
                        "Escape" => {
                            event.prevent_default();
                            cancel();
                        }
                        _ => {}
                    }
                />
            </span>
            <Show
                when=empty
                fallback=move || view! {
                    <span class=format!("flex w-full min-w-0 items-center gap-1 {NOTE}")>
                        <span class="flex min-w-0 flex-1 items-center gap-1 overflow-hidden" node_ref=line>
                            {
                                let badges = badges.clone();
                                move || badges(line)
                            }
                        </span>
                    </span>
                }
            >
                <button
                    type="button"
                    class=format!("{AUTO_TITLE} -ml-2")
                    data-rename-reset=session_id.to_string()
                    // A tap must not take the focus from the field first,
                    // which would close it.
                    on:pointerdown=move |event: ev::PointerEvent| event.prevent_default()
                    on:click=move |_| {
                        renaming.set(None);
                        workspace.rename_session(session_id, SessionRenameChange::Reset);
                    }
                >
                    <Glyph icon=icondata::LuWandSparkles />
                    "Use the automatic title"
                </button>
            </Show>
        </div>
    }
}

/// The open session's subagents, nested under its row: each with its
/// status, agent, handle, and task; a click opens one. Collapsed, only the
/// running ones show, under their dimmed ancestors; expanded, all of them,
/// and a chevron folds a subagent's own.
#[component]
fn Subagents(workspace: Workspace) -> impl IntoView {
    let rows = Memo::new(move |_| workspace.subagent_rows());
    view! {
        // With no rows it takes no room, nor the list's gap: else opening
        // a row lower down would lift it by that room as the tree moves.
        <div
            role="group"
            aria-label="Subagents"
            data-subagents
            data-tree=move || if workspace.tree_expanded.get() { "expanded" } else { "collapsed" }
            class=move || if rows.with(Vec::is_empty) { "hidden" } else { "flex flex-col gap-0.5 pb-1" }
        >
            {move || {
                let expanded = workspace.tree_expanded.get();
                rows.get()
                    .into_iter()
                    .map(|row| {
                        let session_id = row.session_id;
                        let current = move || workspace.selected.get() == Some(session_id);
                        // As on session rows: a mark only while it runs or
                        // when its last run went wrong.
                        let mark = icons::row_status_of(row.status);
                        let task_indent = if mark.is_some() { "pl-5" } else { "" };
                        let indent = format!("{}rem", 0.75 * (row.depth - 1) as f64);
                        let fold = if expanded && row.has_children {
                            view! {
                                <button
                                    type="button"
                                    class=ICON_BUTTON
                                    aria-label="Fold"
                                    title=if row.collapsed { "Show its subagents" } else { "Hide its subagents" }
                                    aria-expanded=(!row.collapsed).to_string()
                                    on:click=move |_| workspace.toggle_folded(session_id)
                                >
                                    <Glyph
                                        icon=if row.collapsed { icondata::LuChevronRight } else { icondata::LuChevronDown }
                                        class="text-xs"
                                    />
                                </button>
                            }
                            .into_any()
                        } else {
                            view! { <span class="w-11 shrink-0" aria-hidden="true"></span> }.into_any()
                        };
                        let dimmed = row.dimmed;
                        view! {
                            <div class="flex items-start" class:opacity-60=dimmed data-dimmed=dimmed.then_some("true") style:padding-left=indent>
                                {fold}
                                <button
                                    type="button"
                                    class=ROW.replace("w-full", "min-w-0 flex-1")
                                    class:bg-sel=current
                                    aria-current=move || current().then_some("true")
                                    data-tree-session=session_id.to_string()
                                    on:click=move |_| workspace.open_session(session_id)
                                >
                                    <span class="flex w-full min-w-0 items-center gap-2 text-sm">
                                        {mark.map(|mark| view! {
                                            <Glyph icon=mark.icon class=mark.class("text-xs") />
                                            <span class="sr-only">{format!("{}: ", mark.label)}</span>
                                        })}
                                        // The handle gives way before the agent's name.
                                        <span class="max-w-[75%] shrink-0 truncate font-mono font-bold">{row.agent.clone()}</span>
                                        {row.handle.clone().map(|handle| view! {
                                            <span class=format!("min-w-0 truncate font-mono {NOTE}") data-handle>{handle}</span>
                                        })}
                                    </span>
                                    <span class=format!("w-full truncate {task_indent} {NOTE}")>{row.task.clone()}</span>
                                </button>
                            </div>
                        }
                    })
                    .collect_view()
            }}
        </div>
    }
}

/// The viewed session's name: the new session's draft, a listed session's
/// title, or a subagent's agent and title.
fn session_title(workspace: Workspace) -> String {
    if workspace.new_session.get() {
        return "New session".to_owned();
    }
    workspace
        .selected
        .get()
        .and_then(|session_id| {
            workspace
                .sessions
                .with(|sessions| {
                    sessions
                        .iter()
                        .find(|session| session.session_id == session_id)
                        .map(session_label)
                })
                .or_else(|| {
                    // A subagent's session is in the tree, not the list.
                    workspace.tree.with(|tree| {
                        tree.as_ref()
                            .and_then(|tree| crate::tree::find(tree, session_id))
                            .map(subagent_label)
                    })
                })
        })
        .unwrap_or_else(|| "No session".into())
}

/// The title bar's title as a field: Enter renames, Escape (or leaving
/// it) keeps the title, and an emptied field offers the automatic title.
#[component]
fn TitleField(workspace: Workspace) -> impl IntoView {
    let field = NodeRef::<html::Input>::new();
    Effect::new(move |_| {
        if let Some(input) = field.get() {
            request_animation_frame(move || {
                let _ = input.focus();
                input.select();
            });
        }
    });
    let empty = move || {
        workspace
            .title_edit
            .with(|draft| draft.as_deref().is_none_or(|draft| draft.trim().is_empty()))
    };
    let save = move || {
        let (Some(session_id), Some(draft)) = (
            workspace.selected.get_untracked(),
            workspace.title_edit.get_untracked(),
        ) else {
            return;
        };
        if workspace.submit_title(session_id, &draft) {
            workspace.title_edit.set(None);
        }
    };
    view! {
        <div
            class="flex min-w-0 flex-1 items-center gap-2"
            data-title-editor
            on:focusout=move |event: ev::FocusEvent| {
                // Leaving the field keeps the title; its button does not.
                let inside = event
                    .related_target()
                    .and_then(|target| target.dyn_into::<web_sys::Node>().ok())
                    .zip(event.current_target().and_then(|target| target.dyn_into::<web_sys::Node>().ok()))
                    .is_some_and(|(target, editor)| editor.contains(Some(&target)));
                if !inside {
                    workspace.title_edit.set(None);
                }
            }
        >
            <input
                type="text"
                id="session-title-field"
                node_ref=field
                class=format!("{TITLE_FIELD} max-w-xl text-base")
                aria-label="Session title"
                placeholder="The automatic title"
                autocomplete="off"
                prop:value=move || workspace.title_edit.get().unwrap_or_default()
                on:input=move |event| workspace.title_edit.set(Some(event_target_value(&event)))
                on:keydown=move |event: ev::KeyboardEvent| match event.key().as_str() {
                    "Enter" => {
                        event.prevent_default();
                        save();
                    }
                    "Escape" => {
                        event.prevent_default();
                        workspace.title_edit.set(None);
                    }
                    _ => {}
                }
            />
            <Show when=empty>
                <button
                    type="button"
                    class=format!("{AUTO_TITLE} shrink-0")
                    data-title-reset
                    on:pointerdown=move |event: ev::PointerEvent| event.prevent_default()
                    on:click=move |_| {
                        workspace.title_edit.set(None);
                        if let Some(session_id) = workspace.selected.get_untracked() {
                            workspace.rename_session(session_id, SessionRenameChange::Reset);
                        }
                    }
                >
                    <Glyph icon=icondata::LuWandSparkles />
                    <span class="max-sm:sr-only">"Use the automatic title"</span>
                </button>
            </Show>
        </div>
    }
}

/// The chat's title bar: the sidebar button, the session's title (a click
/// renames it), and its ⋮ menu of session commands.
#[component]
fn TitleBar(workspace: Workspace) -> impl IntoView {
    // Another session closes the title's field.
    Effect::new(move |_| {
        workspace.selected.track();
        workspace.new_session.track();
        workspace.title_edit.set(None);
    });
    let title = move || session_title(workspace);
    let read_only = move || {
        workspace.selected.get().is_some_and(|session_id| {
            !workspace.new_session.get()
                && workspace
                    .read_only
                    .with(|read_only| read_only.contains(&session_id))
        })
    };
    // A subagent's path from its root: each parent goes up to it.
    let crumbs = move || {
        if workspace.new_session.get() {
            return Vec::new();
        }
        let Some(session_id) = workspace.selected.get() else {
            return Vec::new();
        };
        workspace.tree.with(|tree| {
            let Some(tree) = tree.as_ref() else {
                return Vec::new();
            };
            let mut path = path(tree, session_id);
            path.pop();
            path.into_iter()
                .enumerate()
                .map(|(depth, session)| {
                    let label = if depth == 0 {
                        session_label(session)
                    } else {
                        subagent_label(session)
                    };
                    (session.session_id, label)
                })
                .collect::<Vec<_>>()
        })
    };
    view! {
        <div class="flex h-12 shrink-0 items-center gap-1 border-b border-line px-2">
            <SidebarButton workspace=workspace />
            {move || {
                crumbs().last().cloned().map(|(parent, label)| view! {
                    <button
                        type="button"
                        class=format!("{ICON_BUTTON} sm:hidden")
                        aria-label=format!("Back to {label}")
                        title=format!("Back to {label}")
                        data-crumb-back
                        on:click=move |_| workspace.open_session(parent)
                    >
                        <Glyph icon=icondata::LuChevronLeft class="text-lg" />
                    </button>
                })
            }}
            <Show when=move || !crumbs().is_empty()>
            <nav aria-label="Path" class="hidden min-w-0 shrink items-center sm:flex">
                {move || {
                    crumbs()
                        .into_iter()
                        .map(|(session_id, label)| {
                            let tip = label.clone();
                            view! {
                            <button
                                type="button"
                                class="inline-flex min-h-11 max-w-[14rem] min-w-0 items-center rounded-md px-2 text-sm text-muted hover:bg-hover hover:text-fg active:bg-press"
                                title=tip
                                data-crumb=session_id.to_string()
                                on:click=move |_| workspace.open_session(session_id)
                            >
                                <span class="truncate">{label}</span>
                            </button>
                            <Glyph icon=icondata::LuChevronRight class="text-[0.625rem] opacity-60" />
                            }
                        })
                        .collect_view()
                }}
            </nav>
            </Show>
            <div class="flex min-w-0 flex-1 items-center gap-2 px-1">
                <Show
                    when=move || workspace.title_edit.with(Option::is_some)
                    fallback=move || view! {
                        <h1 class="flex min-w-0 text-lg font-bold" id="session-title" title=title>
                            // A session's title is a button that renames it.
                            <Show when=move || workspace.title_editable() fallback=move || view! {
                                <span class="truncate">{title}</span>
                            }>
                                <button
                                    type="button"
                                    class="flex min-h-11 min-w-0 items-center rounded-md px-1 text-left hover:bg-hover active:bg-press"
                                    title=move || format!("{} (click to rename)", title())
                                    data-title-edit
                                    on:click=move |_| workspace.begin_title_edit()
                                >
                                    <span class="truncate">{title}</span>
                                </button>
                            </Show>
                        </h1>
                    }
                >
                    <TitleField workspace=workspace />
                </Show>
                <Show when=read_only>
                    <span class="inline-flex shrink-0 items-center gap-1 rounded-full bg-sunken px-2 py-1 text-xs" title="read-only snapshot">
                        <Glyph icon=icondata::LuLock />
                        <span class="max-sm:sr-only">"read-only snapshot"</span>
                    </span>
                </Show>
            </div>
            <Show when=move || !workspace.new_session.get()>
                <live::LiveBadges workspace=workspace />
            </Show>
            <button
                type="button"
                class=ICON_BUTTON
                aria-label="Commands"
                title="Session commands"
                aria-haspopup="dialog"
                on:click=move |_| workspace.panel.set(Some(Panel::Commands))
            >
                <Glyph icon=icondata::LuEllipsisVertical class="text-lg" />
            </button>
        </div>
    }
}

/// The subagent `child` a delegate call started, as its row shows it: what
/// it does comes from its own projection.
fn delegate(
    store: &cookie_agent_client_state::StateStore,
    child: &cookie_agent_protocol::SessionMeta,
) -> Delegate {
    let activity = store.sessions.get(&child.session_id).and_then(|child| {
        Activity::of(
            child.active_run.is_some(),
            child
                .tools
                .values()
                .any(|tool| tool.status == ToolStatus::Running),
            child
                .approvals
                .iter()
                .any(|approval| approval.is_visible_user_escalation()),
            !child.questions.is_empty(),
        )
    });
    Delegate {
        session_id: child.session_id,
        agent: child.creation_selection.agent.to_string(),
        handle: child.short_id.clone(),
        activity,
    }
}

#[component]
fn Transcript(workspace: Workspace) -> impl IntoView {
    let cache = StoredValue::new(RowCache::default());
    // Store changes reach the transcript at most once an animation frame: a
    // reply streaming dozens of deltas a second draws once a frame, and a
    // click between frames finds the page free. What the user changes (a
    // block opened, the event level) draws at once.
    let store_frame = Trigger::new();
    let frame_due = StoredValue::new(false);
    Effect::new(move |_| {
        workspace.revision.track();
        if frame_due.get_value() {
            return;
        }
        frame_due.set_value(true);
        request_animation_frame(move || {
            if frame_due.try_update_value(|due| *due = false).is_some() {
                store_frame.notify();
            }
        });
    });
    let homes = Memo::new(move |_| workspace.homes());
    // A reduced transcript's live work row counts its time each second
    // while the session runs.
    let now = RwSignal::new(jiff::Timestamp::now());
    let ticker = set_interval_with_handle(
        move || {
            let reduced = workspace.transcript.get_untracked()
                == cookie_agent_client_state::transcript_mode::TranscriptMode::Reduced;
            if reduced && workspace.viewed.with_untracked(|viewed| viewed.running) {
                now.set(jiff::Timestamp::now());
            }
        },
        Duration::from_secs(1),
    )
    .ok();
    on_cleanup(move || {
        if let Some(ticker) = ticker {
            ticker.clear();
        }
    });
    // Everything a pass reads it borrows where it lies: a frame copies
    // neither the expanded set, the model names, nor the tree.
    // Whether the run's latest work is a reduced transcript's live work row,
    // which spins in place of the trailing working row.
    let work_spins = Memo::new(move |_| {
        use cookie_agent_client_state::{
            ProducerMessageStatus, TranscriptItem, transcript_mode::TranscriptMode,
        };
        store_frame.track();
        if workspace.transcript.get() != TranscriptMode::Reduced {
            return false;
        }
        let Some(session_id) = workspace.selected.get() else {
            return false;
        };
        let level = workspace.level.get();
        untrack(|| {
            workspace.with_store(|store| {
                let state = store.sessions.get(&session_id)?;
                // The last row shown: a diagnostic the level hides, or a
                // message not shown yet, draws nothing.
                let last = state.transcript.iter().rev().find(|item| match item {
                    TranscriptItem::Event { level: shown, .. } => *shown >= level,
                    TranscriptItem::ProducerMessage { status, .. } => !matches!(
                        status,
                        ProducerMessageStatus::Pending | ProducerMessageStatus::Admitted
                    ),
                    _ => true,
                })?;
                // It is in a folded run whose last fold works.
                let plan = state.fold_plan();
                plan.get(last.id())?;
                Some(plan.live_keys().next().is_some())
            })
        })
        .unwrap_or(false)
    });
    let rows = Memo::new(move |_| {
        let Some(session_id) = workspace
            .selected
            .get()
            .filter(|_| !workspace.new_session.get())
        else {
            return Rows::default();
        };
        store_frame.track();
        let level = workspace.level.get();
        let transcript = workspace.transcript.get();
        // Each tick draws again; the time is read as the rows draw.
        now.track();
        let now = jiff::Timestamp::now();
        let draft_agent = workspace
            .draft
            .with(|draft| draft.as_ref().map(|draft| draft.agent.clone()));
        let nothing_open = Expanded::default();
        workspace
            .expanded
            .with(|expanded| {
                // What the user opened in this session: its own, as in the TUI.
                let expanded = expanded.get(&session_id).unwrap_or(&nothing_open);
                homes.with(|homes| {
                    workspace.with_runtime(|runtime| {
                        workspace.tree.with(|tree| {
                            let delegates = tree
                                .as_ref()
                                .map(|tree| delegates_of(tree, session_id))
                                .unwrap_or_default();
                            untrack(|| {
                                workspace.with_store(|store| {
                                    let state = store.sessions.get(&session_id)?;
                                    // What each running subagent does, from its
                                    // own projection (the page follows a tree's
                                    // live sessions).
                                    let delegates = delegates
                                        .into_iter()
                                        .map(|(call, child)| (call, delegate(store, child)))
                                        .collect::<HashMap<_, _>>();
                                    let view = TranscriptView {
                                        session_id,
                                        state,
                                        names: runtime.model_names(),
                                        minimum_level: level,
                                        expanded,
                                        delegates: &delegates,
                                        homes,
                                        transcript,
                                        now,
                                    };
                                    // The system prompt and the tools, pinned at
                                    // the top.
                                    let pinned = crate::render::transcript::pinned_rows(
                                        state,
                                        expanded,
                                        draft_agent.as_ref(),
                                    )
                                    .map(|(_, html)| html);
                                    cache.try_update_value(|cache| {
                                        transcript_rows(&view, pinned, cache)
                                    })
                                })
                            })
                        })
                    })
                })
            })
            .unwrap_or_default()
    });
    let row_slots = Slots::new();
    Effect::new(move |_| {
        rows.with(|rows| {
            row_slots.update(
                rows.order
                    .iter()
                    .filter_map(|id| Some((*id, rows.html.get(id)?))),
            );
        });
    });
    let container = NodeRef::<html::Div>::new();
    let content = NodeRef::<html::Div>::new();
    let follow = StoredValue::new(Follow::default());
    // Whether the view follows the end, for the jump-to-latest button.
    let following = RwSignal::new(true);
    let metrics = move || {
        container.try_get_untracked().flatten().map(|element| {
            let element: web_sys::Element = element.into();
            let at = Metrics {
                top: element.scroll_top(),
                height: element.scroll_height(),
                view: element.client_height(),
            };
            (element, at)
        })
    };
    // Whether the rows overflow the box: without, there is no older row to
    // jump back from.
    let overflows = RwSignal::new(false);
    // A copy of the open work row whose work the view's top is in, pinned
    // there, and the work row a click on that copy folded, to bring back
    // to the top once it drew folded.
    let pinned_work = NodeRef::<html::Div>::new();
    let reveal = StoredValue::new(None::<String>);
    // Only a reduced transcript with a work row open has one to pin.
    let pins_work = Memo::new(move |_| {
        let Some(session_id) = workspace.selected.get() else {
            return false;
        };
        workspace.transcript.get()
            == cookie_agent_client_state::transcript_mode::TranscriptMode::Reduced
            && workspace.expanded.with(|expanded| {
                expanded
                    .get(&session_id)
                    .is_some_and(crate::render::transcript::Expanded::any_work)
            })
    });
    // The open work rows and the end of each one's work, found once per
    // drawing of the rows rather than at every scroll; and the pinned
    // copy's HTML, patched in only when it changes.
    let open_heads = StoredValue::new_local(None::<Vec<OpenHead>>);
    let pinned_html = StoredValue::new(None::<String>);
    let pin_due = StoredValue::new(false);
    let draw_pin = move || {
        let (Some(column), Some(pinned)) = (
            content.try_get_untracked().flatten(),
            pinned_work.try_get_untracked().flatten(),
        ) else {
            return;
        };
        let pinned: web_sys::HtmlElement = pinned.into();
        let head = if pins_work.try_get_untracked() == Some(true) {
            if open_heads.with_value(Option::is_none) {
                open_heads.set_value(Some(open_work_heads(&column)));
            }
            open_heads
                .with_value(|heads| pinned_work_head(heads.as_deref().unwrap_or_default(), &pinned))
        } else {
            None
        };
        if let Some(head) = head {
            if pinned_html.with_value(|shown| shown.as_deref() != Some(head.html.as_str())) {
                crate::morph::patch(&pinned, &head.html);
                pinned_html.set_value(Some(head.html));
            }
            let _ = pinned
                .style()
                .set_property("transform", &format!("translateY({}px)", head.offset));
            let _ = pinned.remove_attribute("hidden");
        } else if !pinned.has_attribute("hidden") {
            let _ = pinned.set_attribute("hidden", "");
            pinned.set_inner_html("");
            pinned_html.set_value(None);
        }
    };
    // Every scroll, wheel, key, and touch asks; the copy is placed once a
    // frame, before it paints.
    let pin_work = move || {
        if pin_due.try_get_value() != Some(false) {
            return;
        }
        pin_due.set_value(true);
        request_animation_frame(move || {
            if pin_due.try_set_value(false).is_some() {
                return;
            }
            draw_pin();
        });
    };
    // The rows drew again, or work rows opened or closed: the heads are
    // found again at the next placing.
    Effect::new(move |_| {
        rows.track();
        pins_work.track();
        open_heads.set_value(None);
        pin_work();
    });
    // Mirrors `follow` into `following` once it changed, and whether the
    // rows overflow.
    let mirror = move || {
        if let Some(now) = follow.try_with_value(|follow| follow.following())
            && following.try_get_untracked() != Some(now)
        {
            following.set(now);
        }
        if let Some((_, at)) = metrics() {
            let now = at.height > at.view;
            if overflows.try_get_untracked() != Some(now) {
                overflows.set(now);
            }
        }
    };
    // After a scroll, a wheel, a key, or a touch: the pinned copy is placed
    // at the next frame.
    let shown = move || {
        pin_work();
        mirror();
    };
    // Pins the view to the end when following and the end moved away, and
    // places the pinned copy now: a resize and a drawing of the rows call
    // this as the frame is laid out.
    let pin = move || {
        let Some((element, at)) = metrics() else {
            return;
        };
        if let Some(end) = follow.try_update_value(|follow| follow.pin(at)).flatten() {
            element.set_scroll_top(end);
        }
        draw_pin();
        mirror();
    };
    // A session opens at its end.
    Effect::new(move |_| {
        workspace.selected.track();
        workspace.new_session.track();
        follow.set_value(Follow::default());
        shown();
    });
    // A message the user sends shows at the end, followed again however far
    // up they had scrolled.
    Effect::new(move |first: Option<()>| {
        workspace.sent.track();
        if first.is_some() {
            follow.update_value(Follow::jump);
            pin();
        }
    });
    // The content growing or the box shrinking moves the end: pinned before
    // the frame paints, and only then.
    let observer = StoredValue::new_local(None::<(web_sys::ResizeObserver, Closure<dyn Fn()>)>);
    Effect::new(move |_| {
        let (Some(scroller), Some(column)) = (container.get(), content.get()) else {
            return;
        };
        if observer.with_value(Option::is_some) {
            return;
        }
        let callback = Closure::<dyn Fn()>::new(pin);
        if let Ok(watch) = web_sys::ResizeObserver::new(callback.as_ref().unchecked_ref()) {
            watch.observe(&scroller);
            watch.observe(&column);
            observer.set_value(Some((watch, callback)));
        }
    });
    on_cleanup(move || {
        if let Some((watch, _)) = observer.try_update_value(Option::take).flatten() {
            watch.disconnect();
        }
    });
    // The rows the page last drew, to tell which changed.
    let drawn = StoredValue::new(Rows::default());
    Effect::new(move |_| {
        let fill_in = rows.with(|rows| {
            let fill_in = drawn.with_value(|drawn| to_fill_in(drawn, rows));
            drawn.set_value(rows.clone());
            fill_in
        });
        if container.get().is_none() {
            return;
        }
        // The transcript may be gone by the next frame (a new session's
        // cards took its place), `follow` with it: nothing to follow.
        request_animation_frame(move || {
            if follow.try_with_value(|_| ()).is_none() {
                return;
            }
            pin();
            if let Some((element, _)) = metrics() {
                load_history_in_view(&element, workspace);
            }
            if let Some(key) = reveal.try_update_value(Option::take).flatten() {
                reveal_work_row(container, content, pinned_work, &key);
                shown();
            }
            // Code and media in the rows and parts that changed, not the
            // whole page.
            let Some(document) = web_sys::window().and_then(|window| window.document()) else {
                return;
            };
            for id in fill_in {
                if let Some(changed) = document.get_element_by_id(&id) {
                    crate::highlight::highlight_within(&changed);
                    crate::media::show_within(&changed, workspace);
                    crate::assets::show_within(&changed, workspace);
                }
            }
        });
    });
    let on_click = move |event: ev::MouseEvent| {
        let Some(target) = event
            .target()
            .and_then(|target| target.dyn_into::<web_sys::Element>().ok())
            .and_then(|target| target.closest("[data-action]").ok().flatten())
        else {
            return;
        };
        let action = target.get_attribute("data-action").unwrap_or_default();
        let call = target.get_attribute("data-call");
        match action.as_str() {
            "toggle-tool" => {
                if let Some(call) = call {
                    workspace.toggle_expanded(call);
                }
            }
            "approvals" => workspace.panel.set(Some(Panel::Approvals)),
            "history" => {
                if let Some(before) = target
                    .get_attribute("data-history")
                    .and_then(|before| before.parse().ok())
                {
                    match metrics() {
                        Some((scroller, _)) => {
                            load_history(&scroller, before, workspace);
                        }
                        None => workspace.load_history(before),
                    }
                }
            }
            "cut-result" => {
                if let Some(call) = call
                    && let Ok(call) = call.parse::<cookie_agent_protocol::ToolCallId>()
                {
                    workspace.load_cut_result(call);
                }
            }
            "full" => {
                let block = target
                    .get_attribute("data-block")
                    .and_then(|block| block.parse().ok());
                if let (Some(call), Some(block)) = (call, block)
                    && let Ok(call) = call.parse::<cookie_agent_protocol::ToolCallId>()
                {
                    workspace.toggle_full_content(call, block);
                }
            }
            "toggle-block" => {
                if let Some(key) = target.get_attribute("data-block-key") {
                    // The pinned copy of a work row folds it and brings the
                    // row back to the top, where the view goes on after it.
                    if target
                        .closest("[data-work-pinned]")
                        .ok()
                        .flatten()
                        .is_some()
                    {
                        reveal.set_value(Some(key.clone()));
                    }
                    workspace.toggle_expanded(key);
                }
            }
            "user-menu" => {
                if let Some(seq) = target
                    .get_attribute("data-seq")
                    .and_then(|seq| seq.parse().ok())
                {
                    workspace.open_user_menu(seq);
                }
            }
            "background" => {
                if let Some(call) = call.and_then(|call| call.parse().ok()) {
                    workspace.request_background(call);
                }
            }
            "open-session" => {
                if let Some(session_id) = target
                    .get_attribute("data-session")
                    .and_then(|session| session.parse().ok())
                {
                    workspace.open_session(session_id);
                }
            }
            "open-memory" => {
                if let Some(url) = target.get_attribute("data-memory-url") {
                    workspace.open_memory_at(url);
                }
            }
            "deferred-prompt" => workspace.panel.set(Some(Panel::DeferredPrompt)),
            action if action.starts_with("asset-") || action.starts_with("attachment-") => {
                let clicked = event
                    .target()
                    .and_then(|target| target.dyn_into::<web_sys::Element>().ok());
                crate::assets::act(&target, clicked.as_ref(), action, workspace);
            }
            "open-media" => {
                if let Some(url) = target.get_attribute("data-src") {
                    workspace.media_group.set(Vec::new());
                    workspace.media_viewer.set(Some(crate::media::Viewing {
                        url,
                        media_type: target.get_attribute("data-media-type").unwrap_or_default(),
                        alt: target.get_attribute("data-alt").unwrap_or_default(),
                        handle: None,
                    }));
                }
            }
            _ => {}
        }
    };
    // The user message row the pointer is over, whose ⋯ shows.
    let marked_row = StoredValue::new_local(None::<web_sys::Element>);
    let message_press = MessagePress {
        workspace,
        press: StoredValue::new(Press::default()),
        timer: StoredValue::new(None),
        seq: StoredValue::new(None),
    };
    // Following stops the moment the user scrolls toward older rows (a
    // wheel, a key, a finger, the scrollbar, a flick's momentum) and
    // resumes only when they come back to the end.
    let back = move || {
        // Nothing to scroll back to: the view keeps following the end.
        if metrics().is_some_and(|(_, at)| at.height > at.view) {
            follow.update_value(Follow::back);
        }
        shown();
    };
    let forward = move || {
        if let Some((_, at)) = metrics() {
            follow.update_value(|follow| follow.forward(at));
            shown();
        }
    };
    let touch_y = StoredValue::new(None::<i32>);
    let on_touch_move = move |event: ev::TouchEvent| {
        let Some(touch) = event.touches().get(0) else {
            return;
        };
        let y = touch.client_y();
        if let Some(last) = touch_y.get_value() {
            // A finger dragging down pulls older rows into view.
            if y > last {
                back();
            } else if y < last {
                forward();
            }
        }
        touch_y.set_value(Some(y));
    };
    let jump = move |_| {
        follow.update_value(Follow::jump);
        pin();
    };
    view! {
        <div
            node_ref=container
            id="transcript"
            class="min-h-0 flex-1 overflow-y-auto px-3 py-2"
            on:click=on_click
            on:scroll=move |_| {
                if let Some((element, at)) = metrics() {
                    follow.update_value(|follow| follow.scrolled(at));
                    shown();
                    keep_off_top(&element, workspace);
                    load_history_in_view(&element, workspace);
                }
            }
            on:wheel=move |event: ev::WheelEvent| {
                if event.ctrl_key() {
                    return;
                }
                if event.delta_y() < 0.0 {
                    back();
                } else if event.delta_y() > 0.0 {
                    forward();
                }
            }
            on:keydown=move |event: ev::KeyboardEvent| match event.key().as_str() {
                // A deck or a chip is a button the keyboard presses too.
                "Enter" | " " if pressable(&event) => {
                    event.prevent_default();
                    if let Some(target) = event
                        .target()
                        .and_then(|target| target.dyn_into::<web_sys::HtmlElement>().ok())
                    {
                        target.click();
                    }
                }
                "ArrowUp" | "PageUp" | "Home" => back(),
                " " if event.shift_key() => back(),
                "ArrowDown" | "PageDown" | "End" | " " => forward(),
                _ => {}
            }
            on:touchstart=move |event: ev::TouchEvent| {
                touch_y.set_value(event.touches().get(0).map(|touch| touch.client_y()));
            }
            on:touchmove=on_touch_move
            on:touchend=move |event: ev::TouchEvent| {
                touch_y.set_value(None);
                // Chrome ends a long press with a click, which would land on
                // the menu's backdrop the press just opened and close it.
                if message_press.fired() {
                    event.prevent_default();
                }
            }
            on:pointerdown=move |event: ev::PointerEvent| message_press.down(&event)
            on:pointermove=move |event: ev::PointerEvent| message_press.moved(&event)
            on:pointerup=move |_| message_press.cancel()
            on:pointercancel=move |_| message_press.cancel()
            on:pointerover=move |event: ev::PointerEvent| mark_message_row(&event, marked_row)
            on:pointerleave=move |_| unmark_message_row(marked_row)
            on:contextmenu=move |event: ev::MouseEvent| {
                // A long press on a bubble is its menu, not the browser's.
                if message_press.held() {
                    event.prevent_default();
                }
            }
        >
            <Show when=move || workspace.new_session.get()>
                <cards::NewSessionCards workspace=workspace />
            </Show>
            // Over the top of the transcript, an open work row the view
            // scrolled past while its work is still in view; the row it
            // sits in takes no room.
            <div class="pointer-events-none sticky top-0 z-10 mx-auto h-0 max-w-4xl" data-work-pin>
                <div
                    node_ref=pinned_work
                    class="pointer-events-auto overflow-hidden rounded-md shadow-md"
                    data-work-pinned
                    hidden
                ></div>
            </div>
            <div node_ref=content class="mx-auto flex max-w-4xl flex-col gap-2">
                <For
                    each=move || rows.with(|rows| rows.order.to_vec())
                    key=|id| *id
                    children=move |id| {
                        let row = row_slots.slot(id, || {
                            rows.with_untracked(|rows| rows.html.get(&id).cloned())
                        });
                        on_cleanup(move || row_slots.remove(id));
                        view! { <TranscriptRow row=row id=id /> }
                    }
                />
                // The run going on, after the last row: the content grows
                // with it, so a view following the end keeps it in sight.
                <Show when=move || {
                    running(workspace)
                        && !workspace.new_session.get()
                        && !(work_spins.get() && workspace.questions().is_empty())
                }>
                    <div
                        id="working"
                        class="flex min-h-9 items-center gap-1.5 px-1 text-sm font-bold text-accent"
                        role="status"
                    >
                        <Glyph icon=icondata::LuLoaderCircle class="animate-spin" />
                        {move || working_label(workspace)}
                    </div>
                </Show>
                // A held session says why and what waits, until a resume.
                <Show when=move || !running(workspace) && workspace.held_line().is_some()>
                    <div
                        id="held"
                        class="flex min-h-9 items-center gap-1.5 px-1 text-sm font-bold text-accent"
                        role="status"
                        data-held
                    >
                        <Glyph icon=icondata::LuPause />
                        {move || workspace.held_line().unwrap_or_default()}
                    </div>
                </Show>
            </div>
            // Over the end of the transcript while it does not follow it;
            // the row it sits in takes no room.
            <div class="pointer-events-none sticky bottom-0 mx-auto h-0 max-w-4xl">
                <Show when=move || {
                    !following.get() && overflows.get() && workspace.selected.get().is_some()
                        && !workspace.new_session.get()
                }>
                    <button
                        type="button"
                        class="pointer-events-auto absolute right-0 bottom-1 inline-flex min-h-11 items-center gap-2 rounded-full border border-line-2 bg-surface px-3 text-sm shadow-md hover:bg-hover active:bg-press"
                        title="Jump to the latest and follow it"
                        data-jump-latest
                        on:click=jump
                    >
                        <Glyph icon=icondata::LuArrowDown />
                        "Latest"
                    </button>
                </Show>
            </div>
        </div>
    }
}

/// The open work row to pin over the top of the transcript: its button's
/// HTML, and how far up the end of its work pushes the copy.
struct PinnedHead {
    html: String,
    offset: f64,
}

/// An open work row of the transcript, and the last part marked as its
/// work.
struct OpenHead {
    head: web_sys::Element,
    last_work: web_sys::Element,
}

/// The open work rows in `column`, in page order, each with the end of its
/// work.
fn open_work_heads(column: &web_sys::Element) -> Vec<OpenHead> {
    let Ok(heads) = column.query_selector_all("[data-work][data-open]") else {
        return Vec::new();
    };
    (0..heads.length())
        .filter_map(|index| {
            let head = heads.item(index)?.dyn_into::<web_sys::Element>().ok()?;
            let key = head.get_attribute("data-block-key")?;
            let work = column
                .query_selector_all(&format!("[data-work-in=\"{key}\"]"))
                .ok()?;
            let last_work = work
                .item(work.length().checked_sub(1)?)?
                .dyn_into::<web_sys::Element>()
                .ok()?;
            Some(OpenHead { head, last_work })
        })
        .collect()
}

/// The open work row of `heads` that scrolled past the top `pinned`
/// sticks at while its work, the parts marked as its, still shows there.
fn pinned_work_head(heads: &[OpenHead], pinned: &web_sys::HtmlElement) -> Option<PinnedHead> {
    if heads.is_empty() {
        return None;
    }
    let top = pinned.parent_element()?.get_bounding_client_rect().top();
    let mut found = None;
    for OpenHead { head, last_work } in heads {
        let rect = head.get_bounding_client_rect();
        // In order: this one and every later one still show.
        if rect.bottom() > top {
            break;
        }
        // The copy rides up with the end of the work, and goes with it.
        let room = last_work.get_bounding_client_rect().bottom() - top;
        if room <= 0.0 {
            continue;
        }
        let Some(button) = head.first_element_child() else {
            continue;
        };
        found = Some(PinnedHead {
            html: button.outer_html(),
            offset: (room - rect.height()).min(0.0),
        });
    }
    found
}

/// Scrolls the transcript so the work row `key` sits at its top.
fn reveal_work_row(
    container: NodeRef<html::Div>,
    content: NodeRef<html::Div>,
    pinned: NodeRef<html::Div>,
    key: &str,
) {
    let (Some(scroller), Some(column), Some(pinned)) = (
        container.try_get_untracked().flatten(),
        content.try_get_untracked().flatten(),
        pinned.try_get_untracked().flatten(),
    ) else {
        return;
    };
    let Some(top) = pinned
        .parent_element()
        .map(|pin| pin.get_bounding_client_rect().top())
    else {
        return;
    };
    let Ok(Some(row)) = column.query_selector(&format!("[data-work][data-block-key=\"{key}\"]"))
    else {
        return;
    };
    let above = row.get_bounding_client_rect().top() - top;
    if above < 0.0 {
        scroller.set_scroll_top(scroller.scroll_top() + above.floor() as i32);
    }
}

/// Reads the history a row of history not loaded stands for once the row
/// comes within a screen above `scroller`'s view, the transcript, so it
/// mostly arrives before the user scrolls up to it. A row whose read
/// failed waits for a click.
fn load_history_in_view(scroller: &web_sys::Element, workspace: Workspace) {
    let Ok(rows) = scroller.query_selector_all("[data-history]:not([data-history-failed])") else {
        return;
    };
    let view = scroller.get_bounding_client_rect();
    for index in 0..rows.length() {
        let Some(row) = rows
            .item(index)
            .and_then(|row| row.dyn_into::<web_sys::Element>().ok())
        else {
            continue;
        };
        let at = row.get_bounding_client_rect();
        if at.bottom() > view.top() - view.height()
            && at.top() < view.bottom()
            && let Some(before) = row
                .get_attribute("data-history")
                .and_then(|before| before.parse().ok())
        {
            load_history(scroller, before, workspace);
            return;
        }
    }
}

/// Reads the history before `before`. The browser keeps the rows in view
/// where they are as it arrives above them (`overflow-anchor`), which also
/// keeps a fling going; see `keep_off_top`.
fn load_history(scroller: &web_sys::Element, before: u64, workspace: Workspace) {
    workspace.load_history(before);
    keep_off_top(scroller, workspace);
}

/// Moves the view a pixel off its top while history is read: the browser
/// anchors no row while the view rests at its top, and the history would
/// push the rows in view down.
fn keep_off_top(scroller: &web_sys::Element, workspace: Workspace) {
    if scroller.scroll_top() > 0 {
        return;
    }
    let loading = workspace
        .selected
        .get_untracked()
        .is_some_and(|session_id| {
            untrack(|| {
                workspace.with_store(|store| {
                    store
                        .sessions
                        .get(&session_id)
                        .is_some_and(|state| state.history.loading.is_some())
                })
            })
        });
    if loading {
        scroller.set_scroll_top(1);
    }
}

/// Values by ID, each in a signal of its own, for the transcript's rows and
/// a reply's parts. Were every row (or part) to read one shared memo, each
/// change would wake them all, and each one's resubscription costs the
/// number of readers: a frame of a long transcript would cost the square
/// of its rows. A change here wakes only the readers of what changed.
struct Slots<V: Send + Sync + 'static> {
    map: StoredValue<HashMap<u64, ArcRwSignal<Option<V>>>>,
}

impl<V: Send + Sync + 'static> Clone for Slots<V> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<V: Send + Sync + 'static> Copy for Slots<V> {}

impl<V: Clone + PartialEq + Send + Sync + 'static> Slots<V> {
    fn new() -> Self {
        Self {
            map: StoredValue::new(HashMap::new()),
        }
    }

    /// The slot of `id`, holding `initial` when it is new.
    fn slot(self, id: u64, initial: impl FnOnce() -> Option<V>) -> ArcRwSignal<Option<V>> {
        if let Some(slot) = self.map.with_value(|map| map.get(&id).cloned()) {
            return slot;
        }
        let slot = ArcRwSignal::new(initial());
        self.map.update_value(|map| {
            map.insert(id, slot.clone());
        });
        slot
    }

    /// Sets each slot whose value is not its entry's in `values` any more.
    fn update<'a>(self, values: impl Iterator<Item = (u64, &'a V)>)
    where
        V: 'a,
    {
        let changed = self.map.with_value(|map| {
            values
                .filter_map(|(id, value)| {
                    let slot = map.get(&id)?;
                    slot.with_untracked(|shown| shown.as_ref() != Some(value))
                        .then(|| (slot.clone(), value.clone()))
                })
                .collect::<Vec<_>>()
        });
        for (slot, value) in changed {
            slot.set(Some(value));
        }
    }

    /// Forgets the slot of an ID the page no longer shows.
    fn remove(self, id: u64) {
        self.map.try_update_value(|map| map.remove(&id));
    }
}

/// One transcript row, keyed by its item: its node stays while its content
/// changes, and only a change to its own HTML redraws it.
#[component]
fn TranscriptRow(row: ArcRwSignal<Option<RowHtml>>, id: u64) -> impl IntoView {
    let reply = row.with_untracked(|row| matches!(row, Some(RowHtml::Reply { .. })));
    if !reply {
        let html = move || {
            row.with(|row| match row {
                Some(RowHtml::Html(html)) => Arc::clone(html),
                _ => Arc::from(""),
            })
        };
        let node = NodeRef::<html::Div>::new();
        drawn_later(node);
        let first = untrack(&html);
        patched_later(node, first.clone(), html);
        return view! {
            <div
                class="transcript-item min-w-0"
                id=row_element_id(id)
                node_ref=node
                inner_html=first.to_string()
            ></div>
        }
        .into_any();
    }
    let node = NodeRef::<html::Div>::new();
    drawn_later(node);
    let header = Memo::new({
        let row = row.clone();
        move |_| {
            row.with(|row| match row {
                Some(RowHtml::Reply { header, .. }) => Arc::clone(header),
                _ => Arc::from(""),
            })
        }
    });
    let parts = Memo::new({
        let row = row.clone();
        move |_| {
            row.with(|row| match row {
                Some(RowHtml::Reply { parts, .. }) => Arc::clone(parts),
                _ => Arc::from([]),
            })
        }
    });
    // Which parts show as an open fold's work: settles to the same map
    // while the reply streams, so the parts' marks wake only on a change.
    let work = Memo::new(move |_| {
        row.with(|row| match row {
            Some(RowHtml::Reply { work, .. }) => Arc::clone(work),
            _ => Arc::default(),
        })
    });
    let part_slots = Slots::new();
    Effect::new(move |_| {
        parts.with(|parts| part_slots.update(parts.iter().map(|(part, html)| (*part, html))));
    });
    view! {
        <div class="transcript-item min-w-0" id=row_element_id(id) node_ref=node>
            <div class="flex flex-col gap-1">
                <div class="contents" inner_html=move || header.get().to_string()></div>
                <For
                    each=move || {
                        parts.with(|parts| {
                            parts
                                .iter()
                                .enumerate()
                                .map(|(at, (part, _))| (*part, at))
                                .collect::<Vec<_>>()
                        })
                    }
                    key=|(part, _)| *part
                    children=move |(part, at)| {
                        // Where the part is when it shows: looked at first.
                        let slot = part_slots.slot(part, || {
                            parts.with_untracked(|parts| {
                                parts
                                    .get(at)
                                    .filter(|(id, _)| *id == part)
                                    .or_else(|| parts.iter().find(|(id, _)| *id == part))
                                    .map(|(_, html)| html.clone())
                            })
                        });
                        on_cleanup(move || part_slots.remove(part));
                        let work = Memo::new(move |_| work.with(|work| work.get(&part).cloned()));
                        view! { <ReplyPart part=slot work=work row=id id=part /> }
                    }
                />
            </div>
        </div>
    }
    .into_any()
}

/// One part of a reply: a tool call, a thinking block, text. Text keeps the
/// nodes of its settled markdown blocks and redraws only its open tail.
/// Shown as an open fold's work, it says so (`work`, the work row's block
/// key), for the work row pinned while its work shows.
#[component]
fn ReplyPart(
    part: ArcRwSignal<Option<PartHtml>>,
    work: Memo<Option<Arc<str>>>,
    row: u64,
    id: u64,
) -> impl IntoView {
    let work = move || work.get().map(|key| key.to_string());
    let markdown = part.with_untracked(|part| matches!(part, Some(PartHtml::Markdown(_))));
    if !markdown {
        let html = move || {
            part.with(|part| match part {
                Some(PartHtml::Html(html)) => Arc::clone(html),
                _ => Arc::from(""),
            })
        };
        // A block of its own, not `contents`: the node stays while its
        // HTML changes, so it keeps the height it last showed at.
        let node = NodeRef::<html::Div>::new();
        drawn_later(node);
        let first = untrack(&html);
        patched_later(node, first.clone(), html);
        return view! {
            <div
                class="min-w-0"
                data-part
                data-work-in=work
                id=part_element_id(row, id)
                node_ref=node
                inner_html=first.to_string()
            ></div>
        }
        .into_any();
    }
    let node = NodeRef::<html::Div>::new();
    drawn_later(node);
    // The blocks on the page, each with the number of nodes it drew.
    let drawn = StoredValue::new(Vec::<(Arc<str>, u32)>::new());
    Effect::new(move |_| {
        let blocks = part.with(|part| match part {
            Some(PartHtml::Markdown(blocks)) => Arc::clone(blocks),
            _ => Arc::from([]),
        });
        let Some(element) = node.get() else {
            return;
        };
        drawn.update_value(|drawn| draw_blocks(&element, drawn, &blocks));
    });
    view! {
        <div
            class="markdown"
            data-part
            data-work-in=work
            id=part_element_id(row, id)
            node_ref=node
        ></div>
    }
    .into_any()
}

/// Whether `event` is a key on an element that acts as a button without
/// being one (`role="button"`).
fn pressable(event: &ev::KeyboardEvent) -> bool {
    event
        .target()
        .and_then(|target| target.dyn_into::<web_sys::Element>().ok())
        .is_some_and(|target| {
            target.get_attribute("role").as_deref() == Some("button")
                && target.has_attribute("data-action")
        })
}

/// Brings `node`, drawn with `first`, in step with each later `html` in
/// place ([`crate::morph`]): replaced whole, a block still streaming would
/// lose the header button under the pointer at every change, its hover
/// flickering and a click on it lost.
fn patched_later(node: NodeRef<html::Div>, first: Arc<str>, html: impl Fn() -> Arc<str> + 'static) {
    Effect::new(move |drawn: Option<Arc<str>>| {
        let drawn = drawn.unwrap_or_else(|| first.clone());
        let html = html();
        let Some(element) = node.get() else {
            return drawn;
        };
        if html != drawn {
            crate::morph::patch(&element, &html);
        }
        html
    });
}

/// Marks the transcript row or reply part `node` as drawn (`data-drawn`)
/// two frames after it shows, when the page has noted the height it laid
/// out at. Only a drawn one may skip its contents off screen
/// (style/main.css): a new one, skipped before its first frame, would
/// shrink to its estimate, and a reply part redrawn at the end of a turn
/// would pull the page up to the reply's top, taking the follow with it.
fn drawn_later(node: NodeRef<html::Div>) {
    Effect::new(move |_| {
        let Some(element) = node.get() else {
            return;
        };
        request_animation_frame(move || {
            request_animation_frame(move || {
                let _ = element.set_attribute("data-drawn", "");
            });
        });
    });
}

/// Brings the markdown blocks under `element` from `drawn` to `blocks`:
/// the blocks both start with stay, the rest go, and the new ones are added
/// after them.
fn draw_blocks(element: &web_sys::Element, drawn: &mut Vec<(Arc<str>, u32)>, blocks: &[Arc<str>]) {
    let kept = drawn
        .iter()
        .zip(blocks)
        .take_while(|((old, _), new)| old == *new)
        .count();
    let stale: u32 = drawn[kept..].iter().map(|(_, nodes)| nodes).sum();
    for _ in 0..stale {
        if let Some(last) = element.last_child() {
            let _ = element.remove_child(&last);
        }
    }
    drawn.truncate(kept);
    for block in &blocks[kept..] {
        let before = element.child_nodes().length();
        let _ = element.insert_adjacent_html("beforeend", block);
        let after = element.child_nodes().length();
        // A code block is highlighted as it is added, alone.
        if fills_in(block) {
            let nodes = element.child_nodes();
            for index in before..after {
                if let Some(node) = nodes
                    .item(index)
                    .and_then(|node| node.dyn_into::<web_sys::Element>().ok())
                {
                    crate::highlight::highlight_within(&node);
                }
            }
        }
        drawn.push((Arc::clone(block), after - before));
    }
}

/// Shows the ⋯ of the user message row a mouse or pen is over (the class
/// `pointer-over`, which style/main.css follows), and of no other row.
fn mark_message_row(
    event: &ev::PointerEvent,
    marked: StoredValue<Option<web_sys::Element>, LocalStorage>,
) {
    if event.pointer_type() == "touch" {
        return;
    }
    let Some(target) = event
        .target()
        .and_then(|target| target.dyn_into::<web_sys::Element>().ok())
    else {
        return;
    };
    let row = target.closest(".user-row").ok().flatten();
    // Only the row marked before can show its ⋯: no other is looked for.
    if !marked.with_value(|marked| *marked == row) {
        unmark_message_row(marked);
    }
    // Again on the same row too: a redraw of it may have dropped the class.
    if let Some(row) = &row {
        let _ = row.class_list().add_1("pointer-over");
    }
    marked.set_value(row);
}

/// Hides the ⋯ of the user message row the pointer was over.
fn unmark_message_row(marked: StoredValue<Option<web_sys::Element>, LocalStorage>) {
    if let Some(row) = marked.try_update_value(Option::take).flatten() {
        let _ = row.class_list().remove_1("pointer-over");
    }
}

/// A touch held on a user message's bubble: after [`LONG_PRESS`] without
/// moving it opens the message's menu, as the ⋯ does with a mouse.
#[derive(Clone, Copy)]
struct MessagePress {
    workspace: Workspace,
    press: StoredValue<Press>,
    timer: StoredValue<Option<TimeoutHandle>>,
    /// The message the press is on, until it ends.
    seq: StoredValue<Option<u64>>,
}

impl MessagePress {
    fn down(self, event: &ev::PointerEvent) {
        self.cancel();
        self.press.set_value(Press::default());
        self.seq.set_value(None);
        if event.pointer_type() != "touch" {
            return;
        }
        let Some(seq) = event
            .target()
            .and_then(|target| target.dyn_into::<web_sys::Element>().ok())
            .and_then(|target| target.closest("[data-bubble]").ok().flatten())
            .and_then(|bubble| bubble.closest(".user-row").ok().flatten())
            .and_then(|row| row.get_attribute("data-seq"))
            .and_then(|seq| seq.parse().ok())
        else {
            return;
        };
        let (x, y) = (f64::from(event.client_x()), f64::from(event.client_y()));
        self.press.update_value(|press| press.start(x, y));
        self.seq.set_value(Some(seq));
        let timer = set_timeout_with_handle(
            move || {
                if self.press.try_update_value(Press::elapse).unwrap_or(false) {
                    self.workspace.open_user_menu(seq);
                }
            },
            LONG_PRESS,
        )
        .ok();
        self.timer.set_value(timer);
    }

    fn moved(self, event: &ev::PointerEvent) {
        let (x, y) = (f64::from(event.client_x()), f64::from(event.client_y()));
        if self
            .press
            .try_update_value(|press| press.moved(x, y))
            .unwrap_or(false)
        {
            self.stop_timer();
        }
    }

    /// The finger lifted or the browser took the gesture over.
    fn cancel(self) {
        self.press.update_value(Press::cancel);
        self.stop_timer();
    }

    /// Whether the last press opened the menu, until the next one starts.
    fn fired(self) -> bool {
        self.press.with_value(Press::fired)
    }

    /// Whether a press is on a bubble now, held or just fired.
    fn held(self) -> bool {
        self.seq.get_value().is_some()
    }

    fn stop_timer(self) {
        if let Some(timer) = self.timer.get_value() {
            timer.clear();
        }
        self.timer.set_value(None);
    }
}

#[component]
fn QueueStrip(workspace: Workspace) -> impl IntoView {
    let queue = Memo::new(move |_| workspace.queue());
    view! {
        <Show when=move || !queue.with(Vec::is_empty)>
            <div class="flex max-h-32 flex-col gap-1 overflow-y-auto border-t border-line px-3 py-1" id="queue" aria-label="Pending and queued messages">
                {move || {
                    queue
                        .get()
                        .into_iter()
                        .map(|(seq, text, own, recallable)| {
                            if recallable {
                                view! {
                                    <button
                                        type="button"
                                        class=format!("{ROW} font-mono text-xs")
                                        title=if own {
                                            "Take back into the composer"
                                        } else {
                                            "Take back into your composer (another client sent it)"
                                        }
                                        on:click=move |_| workspace.recall(seq)
                                    >
                                        <span class="flex w-full items-center gap-2">
                                            <Glyph icon=icondata::LuClock class="text-accent" />
                                            <span class="min-w-0 flex-1 truncate">{text}</span>
                                            <Glyph icon=icondata::LuUndo2 class="opacity-60" />
                                        </span>
                                    </button>
                                }
                                    .into_any()
                            } else {
                                view! {
                                    <div class="flex items-center gap-2 px-3 font-mono text-xs text-muted">
                                        <Glyph icon=icondata::LuClock />
                                        <span class="min-w-0 truncate">{text}</span>
                                    </div>
                                }
                                    .into_any()
                            }
                        })
                        .collect_view()
                }}
            </div>
        </Show>
    }
}

/// A round button inside the composer: 36 px to see, 44 px to hit.
const COMPOSER_BUTTON: &str = "hit-area inline-flex h-9 w-9 shrink-0 items-center justify-center rounded-full disabled:cursor-not-allowed disabled:opacity-40";
/// The composer's main action: Send, Steer, or Stop.
const COMPOSER_PRIMARY: &str =
    "bg-accent text-on-accent hover:bg-accent active:bg-accent-press disabled:hover:bg-accent";
/// Its second action: Queue.
const COMPOSER_SECONDARY: &str =
    "border border-line-2 bg-surface-2 text-fg hover:border-accent hover:bg-hover active:bg-press";

/// The composer's text sizes: a line, and the padding above and below.
const COMPOSER_LINE: f64 = 20.0;
const COMPOSER_PADDING: f64 = 24.0;

/// The composer's left padding for the attach buttons in the box's
/// bottom left corner: 4 px in, 36 px a button, 2 px between, and 8 px of
/// air; 12 px without one. A phone also has the camera, which wider
/// windows (`md`) hide.
fn attach_padding(buttons: crate::attach::AttachButtons) -> &'static str {
    match buttons {
        crate::attach::AttachButtons::None => "pl-3",
        crate::attach::AttachButtons::File => "pl-12",
        crate::attach::AttachButtons::FileAndCamera => "pl-[5.375rem] md:pl-12",
    }
}

/// The composer's right padding for its buttons in the bottom right
/// corner: one round button (Send or Stop), or Queue and Steer, 6 px
/// apart.
fn actions_padding(buttons: ComposerButtons) -> &'static str {
    match buttons {
        ComposerButtons::SteerAndQueue => "pr-[5.625rem]",
        ComposerButtons::Send { .. } | ComposerButtons::Stop | ComposerButtons::Resume => "pr-12",
    }
}

/// [`actions_padding`] as a length, for a scrollbar's room on top of it.
fn actions_room(buttons: ComposerButtons) -> &'static str {
    match buttons {
        ComposerButtons::SteerAndQueue => "5.625rem",
        ComposerButtons::Send { .. } | ComposerButtons::Stop | ComposerButtons::Resume => "3rem",
    }
}

/// Sizes the composer to its text, from one line to five, and returns the
/// room its scrollbar takes once the text scrolls, in pixels: the buttons
/// in the box's corner move left of it. A scrollbar drawn over the text
/// (a phone's) takes none, and gets [`OVERLAY_SCROLLBAR`].
fn fit_composer(textarea: &web_sys::HtmlTextAreaElement) -> i32 {
    let style = web_sys::HtmlElement::style(textarea);
    let _ = style.set_property("height", "auto");
    let (height, scrolls) = crate::keys::composer_height(
        f64::from(textarea.scroll_height()),
        COMPOSER_LINE,
        COMPOSER_PADDING,
    );
    let _ = style.set_property("height", &format!("{height}px"));
    let _ = style.set_property("overflow-y", if scrolls { "auto" } else { "hidden" });
    if !scrolls {
        return 0;
    }
    let element: &web_sys::HtmlElement = textarea;
    let inner = web_sys::Element::client_width(element);
    (element.offset_width() - inner).max(OVERLAY_SCROLLBAR)
}

/// The room the composer's buttons leave a scrollbar drawn over the text.
const OVERLAY_SCROLLBAR: i32 = 6;

#[component]
fn Composer(workspace: Workspace, composing: RwSignal<bool>) -> impl IntoView {
    let textarea = NodeRef::<html::Textarea>::new();
    let blurred = StoredValue::new(0_u64);
    let attach_buttons = Memo::new(move |_| crate::views::depth::attach_buttons(workspace));
    let buttons = Memo::new(move |_| {
        let has_input = workspace.composer.with(|text| !text.trim().is_empty())
            || !workspace.attachments.with(Vec::is_empty);
        composer_buttons(
            workspace.stoppable(),
            has_input,
            workspace.held_line().is_some(),
        )
    });
    // The box follows its text: typed, pasted, recalled, or sent away; and
    // its buttons, whose number changes the room the text has. On the
    // next frame the composer may be gone, with the view it was in.
    // The room the box's scrollbar takes, which the buttons keep clear of.
    let gutter = RwSignal::new(0);
    let fit = move || {
        if let Some(textarea) = textarea.try_get_untracked().flatten() {
            let room = fit_composer(&textarea);
            if gutter.try_get_untracked() != Some(room) {
                gutter.set(room);
            }
        }
    };
    Effect::new(move |_| {
        workspace.composer.track();
        buttons.track();
        workspace.selected.track();
        request_animation_frame(fit);
    });
    let resize = window_event_listener(ev::resize, move |_| fit());
    on_cleanup(move || resize.remove());
    let insert_newline = move || {
        let Some(textarea) = textarea.get() else {
            return;
        };
        let start = textarea.selection_start().ok().flatten().unwrap_or(0);
        let end = textarea.selection_end().ok().flatten().unwrap_or(start);
        let _ = textarea.set_range_text_with_start_and_end_and_mode("\n", start, end, "end");
        workspace.composer.set(textarea.value());
    };
    let on_keydown = move |event: ev::KeyboardEvent| {
        let key = event.key();
        let action = composer_action(KeyPress {
            key: &key,
            alt: event.alt_key(),
            shift: event.shift_key(),
            ctrl: event.ctrl_key(),
            meta: event.meta_key(),
            empty: workspace.composer.with_untracked(String::is_empty),
        });
        match action {
            ComposerAction::Type => return,
            ComposerAction::Steer if buttons.get_untracked() == ComposerButtons::Resume => {
                workspace.resume_held();
            }
            ComposerAction::Steer => workspace.submit(InputDelivery::Steer),
            ComposerAction::Queue => workspace.submit(InputDelivery::Queue),
            ComposerAction::Newline if event.shift_key() => return,
            ComposerAction::Newline => insert_newline(),
            ComposerAction::Cancel => workspace.cancel(),
            ComposerAction::Recall => workspace.recall_newest(),
        }
        event.prevent_default();
    };
    let disabled = move || !workspace.writable();
    let button = move |id: &'static str,
                       label: &'static str,
                       key: &'static str,
                       icon: icons::Icon,
                       primary: bool,
                       action: fn(Workspace)| {
        view! {
            <button
                type="button"
                id=id
                class=format!("{COMPOSER_BUTTON} {}", if primary { COMPOSER_PRIMARY } else { COMPOSER_SECONDARY })
                data-composer-button=id
                aria-label=label
                title=format!("{label} · {key}")
                disabled=move || {
                    disabled() || buttons.get() == ComposerButtons::Send { enabled: false }
                }
                on:click=move |_| action(workspace)
            >
                <Glyph icon=icon class="text-sm" />
            </button>
        }
    };
    view! {
        <div class="relative min-w-0 px-3 pt-2 pb-1 short:flex-1 short:pb-2">
            // One border, the box's: the text field inside has none, and
            // the buttons sit in its bottom corners.
            <div
                id="composer-box"
                class="relative rounded-xl border border-line-2 bg-surface hover:border-accent focus-within:border-accent"
                class:opacity-60=disabled
            >
                <textarea
                    node_ref=textarea
                    id="composer"
                    rows="1"
                    // Its sides keep clear of the buttons in the box's corners,
                    // from the first render: padding by which buttons show,
                    // not by measuring them once they have.
                    class=move || {
                        format!(
                            "block w-full resize-none appearance-none border-0 bg-transparent py-3 font-mono text-sm leading-5 shadow-none outline-none focus:outline-none {} {}",
                            attach_padding(attach_buttons.get()),
                            actions_padding(buttons.get()),
                        )
                    }
                    // A scrollbar takes room from the text's side too.
                    style:padding-right=move || {
                        let room = gutter.get();
                        (room > 0).then(|| format!("calc({} + {room}px)", actions_room(buttons.get())))
                    }
                    placeholder="Message"
                    disabled=disabled
                    prop:value=move || workspace.composer.get()
                    on:input=move |event| {
                        workspace.composer.set(event_target_value(&event));
                        fit();
                    }
                    on:focus=move |_| {
                        blurred.update_value(|blurred| *blurred += 1);
                        composing.set(true);
                    }
                    on:blur=move |_| {
                        // Later, so a tap on a button lands before the bar
                        // reappears and moves it. The tap may take the
                        // composer away (New session), `blurred` with it.
                        // Removing the focused composer with its view (the
                        // page leaving for the connect screen) blurs it once
                        // `blurred` is gone.
                        let Some(current) = blurred.try_update_value(|blurred| {
                            *blurred += 1;
                            *blurred
                        }) else {
                            return;
                        };
                        set_timeout(
                            move || {
                                if blurred.try_get_value() == Some(current) {
                                    composing.set(false);
                                }
                            },
                            BAR_RETURN,
                        );
                    }
                    on:keydown=on_keydown
                    on:paste=move |event: ev::ClipboardEvent| {
                        let files = crate::files::files_of(
                            event.clipboard_data().and_then(|data| data.files()),
                        );
                        if !files.is_empty() {
                            event.prevent_default();
                            workspace.attach_files(files);
                        }
                    }
                ></textarea>
                <div class="absolute bottom-1 left-1 flex items-center gap-0.5" data-composer-left>
                    <AttachButtons workspace=workspace />
                </div>
                <div
                    class="absolute bottom-1 flex items-center gap-1.5"
                    style:right=move || format!("calc(0.25rem + {}px)", gutter.get())
                    data-composer-actions
                >
                    {move || match buttons.get() {
                        ComposerButtons::Send { .. } => button(
                            "send", "Send", "Enter", icondata::LuArrowUp, true,
                            |workspace| workspace.submit(InputDelivery::Steer),
                        )
                        .into_any(),
                        ComposerButtons::Stop => button(
                            "stop", "Stop", "Esc", icondata::LuSquare, true,
                            Workspace::cancel,
                        )
                        .into_any(),
                        ComposerButtons::Resume => button(
                            "resume", "Resume", "Enter", icondata::LuPlay, true,
                            Workspace::resume_held,
                        )
                        .into_any(),
                        ComposerButtons::SteerAndQueue => view! {
                            {button(
                                "queue-send", "Queue", "Alt+Enter", icondata::LuClock, false,
                                |workspace| workspace.submit(InputDelivery::Queue),
                            )}
                            // An ACP agent without the steering extension
                            // takes the message only with its next turn.
                            {button(
                                "steer",
                                if workspace.unsteerable() {
                                    "Queue (this agent can't be steered)"
                                } else {
                                    "Steer"
                                },
                                "Enter", icondata::LuArrowUp, true,
                                |workspace| workspace.submit(InputDelivery::Steer),
                            )}
                        }
                        .into_any(),
                    }}
                </div>
            </div>
        </div>
    }
}

/// Whether the viewed session runs: a run, or a call still running.
fn running(workspace: Workspace) -> bool {
    workspace.viewed.with(|viewed| viewed.running)
}

/// What a running session's spinner says: it waits on the user while an
/// agent's question does.
fn working_label(workspace: Workspace) -> &'static str {
    if workspace.questions().is_empty() {
        "working…"
    } else {
        "waiting for your answer"
    }
}

fn approvals_label(count: usize) -> String {
    match count {
        1 => "1 approval waiting".to_owned(),
        count => format!("{count} approvals waiting"),
    }
}

/// How long a new status message shows.
const STATUS_LIVE: Duration = Duration::from_secs(5);

/// The bottom bar: the session's agent, model, permission mode, and cost.
/// A wide window shows it under the composer and adds the approvals and the
/// context size; a phone shows it above the composer, on the composer's
/// background, with the approvals in the status line above it, and hides it
/// while the composer has focus. A status message shows over the composer
/// ([`StatusTip`]), not in the bar.
#[component]
fn BottomBar(workspace: Workspace, composing: RwSignal<bool>) -> impl IntoView {
    let cost = move || {
        workspace.selected.get().and_then(|session_id| {
            workspace
                .costs
                .with(|costs| costs.get(&session_id).copied())
        })
    };
    // An ACP agent names where it runs: `claude · on local`.
    let agent = move || {
        let Some(draft) = workspace.draft.get() else {
            return "agent".into();
        };
        match workspace.agent_place() {
            Some((runtime, _)) => format!("{} · on {runtime}", draft.agent),
            None => draft.agent.to_string(),
        }
    };
    let model = move || {
        // An ACP agent chooses its own model, one of its options.
        if workspace.draft_is_acp() {
            return workspace
                .acp_model_label()
                .unwrap_or_else(|| "agent's model".into());
        }
        let draft = workspace.draft.get();
        workspace.with_runtime(|runtime| {
            draft
                .map(|draft| runtime.model_names().selection_label(&draft.model))
                .unwrap_or_else(|| "model".into())
        })
    };
    let mode = move || workspace.permission_mode();
    // An ACP agent's own mode is its session's permission mode.
    let mode_label = move || {
        cookie_agent_client_state::acp::mode_option(&workspace.acp_options()).map_or_else(
            || mode_name(mode()).to_owned(),
            cookie_agent_client_state::acp::current_name,
        )
    };
    view! {
        <div
            id="status-bar"
            class="flex flex-wrap items-center gap-x-1 bg-surface-2 px-2 text-xs compact:-order-1 compact:flex-nowrap compact:bg-transparent short:order-none short:w-[22rem] short:shrink-0 compact:dark:bg-transparent"
            class:compact:hidden=move || composing.get()
        >
            <button
                type="button"
                id="agent-chip"
                class=CHIP.replace("min-w-0", "min-w-11 justify-center")
                aria-label=move || format!("Agent: {}", agent())
                title=move || format!("Agent: {} (click to change)", agent())
                on:click=move |_| workspace.panel.set(Some(Panel::Agents))
            >
                <Glyph icon=icondata::LuBot class="text-accent" />
                <span class="min-w-0 truncate max-[23.75rem]:sr-only">{agent}</span>
                <Glyph icon=icondata::LuChevronDown class="text-[0.625rem] opacity-60 compact:hidden" />
            </button>
            <button
                type="button"
                id="model-chip"
                class=format!("{CHIP} compact:max-w-none compact:flex-1")
                aria-label=move || format!("Model: {}", model())
                title=move || format!("Model: {} (click to change)", model())
                on:click=move |_| workspace.panel.set(Some(Panel::Models))
            >
                <Glyph icon=icondata::LuCpu class="text-accent" />
                <span class="min-w-0 flex-1 truncate text-left">{model}</span>
                <Glyph icon=icondata::LuChevronDown class="text-[0.625rem] opacity-60 compact:hidden" />
            </button>
            <button
                type="button"
                id="permission-mode"
                class=CHIP.replace("min-w-0", "min-w-11 justify-center")
                aria-label=move || format!("Permission mode: {}", mode_label())
                aria-haspopup="dialog"
                title=move || format!("Permission mode: {} (click to change)", mode_label())
                on:click=move |_| workspace.panel.set(Some(Panel::Modes))
            >
                <Glyph icon=Signal::derive(move || icons::permission_mode(mode()).0) class="text-accent" />
                // A phone shows the shield alone.
                <span class="min-w-0 truncate compact:sr-only">{mode_label}</span>
                <Glyph icon=icondata::LuChevronDown class="text-[0.625rem] opacity-60 compact:hidden" />
            </button>
            // Where the session works.
            {move || workspace.places_label().filter(|_| !workspace.new_session.get()).map(|places| view! {
                <button
                    type="button"
                    id="places"
                    class=format!("{STATUS_BUTTON} min-w-0 max-w-[12rem] compact:hidden")
                    aria-label=format!("Runtimes: {places}")
                    title=format!("Works in {places} (click for the runtimes)")
                    on:click=move |_| workspace.run_command(Command::Runtime)
                >
                    // Each runtime in its machine's colour, as on the
                    // session rows; the folder when there are none.
                    {move || {
                        let runtimes = workspace.places_runtimes();
                        if runtimes.is_empty() {
                            view! { <Glyph icon=icondata::LuFolderOpen class="text-accent" /> }.into_any()
                        } else {
                            view! {
                                <span class="inline-flex shrink-0 items-center gap-0.5" data-places-dots>
                                    {runtimes.iter().take(3).map(|runtime| {
                                        let key = workspace.runtimes.with(|listing| {
                                            cookie_agent_client_state::badges::hue_key(listing.as_ref(), runtime)
                                        });
                                        crate::badges::runtime_dot(&key, runtime)
                                    }).collect_view()}
                                </span>
                            }
                            .into_any()
                        }
                    }}
                    <span class="min-w-0 truncate font-mono">{places.clone()}</span>
                </button>
            })}
            <span class="flex-1 compact:hidden" aria-hidden="true"></span>
            {move || cost().map(|cost| view! {
                <button
                    type="button"
                    id="cost"
                    class=STATUS_BUTTON
                    aria-label=format!("Session cost {}: usage", cost_label(cost))
                    title="Session cost: click for usage"
                    on:click=move |_| workspace.run_command(Command::Usage)
                >
                    <Glyph icon=icondata::LuCoins class="text-accent compact:hidden" />
                    <span class="font-mono compact:hidden">{cost_label(cost)}</span>
                    <span class="hidden font-mono compact:inline">{compact_cost(cost)}</span>
                </button>
            })}
        </div>
    }
}

/// A phone's line above the composer: the approvals waiting; it shows only
/// while some do. Wider windows have them in the bottom bar. The working
/// spinner is the transcript's last row.
#[component]
fn StatusLine(workspace: Workspace) -> impl IntoView {
    let approvals = Memo::new(move |_| workspace.approvals().len());
    view! {
        <Show when=move || { approvals.get() > 0 }>
            <div
                id="status-line"
                class="hidden min-h-11 items-center gap-1 border-t border-line px-2 text-xs compact:flex"
            >
                <button
                    type="button"
                    data-approvals
                    class="inline-flex min-h-11 min-w-11 shrink-0 items-center justify-center gap-1.5 rounded-full bg-accent px-3 font-bold text-on-accent hover:bg-accent active:bg-accent-press"
                    title="Approvals waiting: tap to answer"
                    on:click=move |_| workspace.panel.set(Some(Panel::Approvals))
                >
                    <Glyph icon=icondata::LuBell />
                    {move || approvals_label(approvals.get())}
                </button>
            </div>
        </Show>
    }
}

/// Keeps [`Workspace::status_live`]: set by every status message, and
/// cleared [`STATUS_LIVE`] later, when [`StatusTip`] fades it out. A message
/// set again, such as the same error twice, shows again for as long.
/// The page calls it once: the views remount when the page comes back
/// from the connect screen, a message they would have to show among it.
pub(crate) fn follow_status(workspace: Workspace) {
    let generation = StoredValue::new(0_u64);
    Effect::new(move |previous: Option<()>| {
        let shown = workspace.status.with(|status| !status.is_empty());
        if !shown {
            workspace.status_live.set(false);
        } else if previous.is_some() {
            generation.update_value(|generation| *generation += 1);
            let current = generation.get_value();
            workspace.status_live.set(true);
            set_timeout(
                move || {
                    if generation.get_value() == current {
                        workspace.status_live.set(false);
                    }
                },
                STATUS_LIVE,
            );
        }
    });
}

/// A new status message, such as an error, as a tooltip over the composer
/// while [`Workspace::status_live`] says, fading out when it no longer does:
/// they come rarely, so they take no room of the bottom bar's. It never
/// takes a click from what it covers.
#[component]
fn StatusTip(workspace: Workspace) -> impl IntoView {
    let live = move || workspace.status_live.get();
    view! {
        <span
            id="status"
            role="status"
            aria-hidden=move || (!live()).then_some("true")
            class="status-tip pointer-events-none absolute bottom-full left-2 z-10 mb-1 line-clamp-3 max-w-[min(32rem,calc(100%-1rem))] rounded-md border border-line bg-surface-2 px-2 py-1 text-xs break-words text-muted shadow-md"
            class:status-tip-gone=move || !live()
        >
            {move || workspace.status.get()}
        </span>
    }
}

/// The session's cost as a wide window's bottom bar shows it.
fn cost_label(cost: f64) -> String {
    format!("${cost:.4}")
}

/// The session panel's tabs (docs/site/specs/quiet-ink.md, D5): the sheets
/// about the open session, each opened through its command as the
/// Session commands sheet opens it, with what it reads.
const SESSION_TABS: [(Panel, Command, &str); 6] = [
    (Panel::Usage, Command::Usage, "Usage"),
    (Panel::Permissions, Command::Permissions, "Permissions"),
    (Panel::Runtimes, Command::Runtime, "Runtimes"),
    (Panel::Goal, Command::Goal, "Goal"),
    (Panel::Background, Command::Background, "Background"),
    (Panel::Events, Command::Events, "Events"),
];

/// The session panel's tab strip, each tab with what is live in it.
fn session_tabs(workspace: Workspace) -> impl IntoView {
    let goal = Memo::new(move |_| {
        workspace
            .goal()
            .filter(|goal| cookie_agent_client_state::rows::goal_bar_shows(goal.status))
            .map(|goal| {
                (
                    goal.items.iter().filter(|item| item.finished).count(),
                    goal.items.len(),
                )
            })
    });
    let background = Memo::new(move |_| workspace.background_calls().len());
    let tabs = SESSION_TABS
        .into_iter()
        .map(|(panel, command, label)| {
            let current = move || workspace.panel.get() == Some(panel);
            let count = move || match panel {
                Panel::Goal => goal
                    .get()
                    .filter(|(_, total)| *total > 0)
                    .map(|(done, total)| format!("{done}/{total}")),
                Panel::Background => {
                    Some(background.get()).filter(|count| *count > 0).map(|count| count.to_string())
                }
                _ => None,
            };
            view! {
                <button
                    type="button"
                    role="tab"
                    class="relative inline-flex h-9 shrink-0 items-center gap-1.5 px-2 text-[0.8125rem] text-muted transition-colors hover:text-fg aria-selected:font-semibold aria-selected:text-fg"
                    aria-selected=move || current().to_string()
                    data-session-tab=label.to_lowercase()
                    on:click=move |_| workspace.run_command(command)
                >
                    {label}
                    {move || count().map(|count| view! {
                        <span class="text-xs font-normal text-muted tabular-nums">{count}</span>
                    })}
                    <span
                        class="absolute inset-x-2 -bottom-px h-0.5 rounded-full bg-accent transition-opacity"
                        class:opacity-0=move || !current()
                        aria-hidden="true"
                    ></span>
                </button>
            }
        })
        .collect_view();
    view! {
        <div role="tablist" aria-label="Session" class="flex overflow-x-auto px-2" data-session-tabs>
            {tabs}
        </div>
    }
}

#[component]
fn Sheet(workspace: Workspace) -> impl IntoView {
    let close = move |_| workspace.close_panel();
    let session_tab = move || {
        workspace
            .panel
            .get()
            .is_some_and(|panel| SESSION_TABS.iter().any(|(tab, _, _)| *tab == panel))
    };
    // The project editor fills a phone's screen.
    let tall = move || workspace.panel.get() == Some(Panel::Project);
    let title = move || match workspace.panel.get() {
        _ if session_tab() => "Session",
        Some(Panel::Agents) => "Agent",
        Some(Panel::Models) => "Model",
        Some(Panel::AgentOptions) => "Agent options",
        Some(Panel::Presets) => "Preset",
        Some(Panel::Approvals) => "Approvals",
        Some(Panel::Usage) => "Usage",
        Some(Panel::Events) => "Events",
        Some(Panel::Commands) => "Session commands",
        Some(Panel::Goal) => "Goal",
        Some(Panel::Compact) => "Compact context",
        Some(Panel::AgentCommand) => "The agent's command",
        Some(Panel::Modes) => "Permission mode",
        Some(Panel::Background) => "Background calls",
        Some(Panel::Skills) => "Skills",
        Some(Panel::Permissions) => "Permissions",
        Some(Panel::Message) => "Message",
        Some(Panel::Runtimes) => "Runtimes",
        Some(Panel::DeferredPrompt) => "Deferred tools prompt",
        Some(Panel::MoveSession) => "Move to project",
        Some(Panel::Project) => {
            if workspace.project_editor.with(|editor| {
                editor
                    .as_ref()
                    .is_some_and(|editor| editor.project.is_some())
            }) {
                "Project"
            } else {
                "New project"
            }
        }
        None => "",
    };
    view! {
        <Show when=move || workspace.panel.get().is_some()>
            <div class="scrim-in fixed inset-0 z-10 bg-scrim sm:bg-scrim/40" on:click=close></div>
            // A phone's sheet rises from the bottom of what the keyboard
            // leaves; a wider window's stands on the right.
            <div class=move || format!(
                "sheet-frame pointer-events-none fixed inset-x-0 top-0 z-20 flex flex-col justify-end sm:inset-y-0 sm:right-0 sm:left-auto sm:w-[28rem] sm:pt-0 {}",
                if tall() { "pt-0" } else { "pt-12" },
            )>
                <section
                    id="sheet"
                    role="dialog"
                    aria-label=title
                    class=move || format!(
                        "sheet-in pointer-events-auto flex max-h-full flex-col overflow-y-auto border-line bg-raised shadow-sheet sm:h-full sm:rounded-none sm:border-t-0 sm:border-l {}",
                        if tall() { "h-full" } else { "rounded-t-2xl border-t" },
                    )
                >
                    <div class="sticky top-0 z-10 flex flex-col border-b border-line bg-raised">
                        <span class="mx-auto mt-1.5 h-1 w-10 rounded-full bg-line-2 sm:hidden" aria-hidden="true"></span>
                        <div class="flex items-center gap-2 py-1 pr-1 pl-4">
                            <h2 class="flex-1 text-[0.9375rem] font-semibold">{title}</h2>
                            <button type="button" class=ICON_BUTTON aria-label="Close" title="Close" on:click=close>
                                <Glyph icon=icondata::LuX class="text-lg" />
                            </button>
                        </div>
                        <Show when=session_tab>
                            {session_tabs(workspace)}
                        </Show>
                    </div>
                    <div class="flex flex-col gap-2 p-3">
                        {move || match workspace.panel.get() {
                            Some(Panel::Agents) => agents_sheet(workspace).into_any(),
                            Some(Panel::Models) => models_sheet(workspace).into_any(),
                            Some(Panel::AgentOptions) => agent_options_sheet(workspace).into_any(),
                            Some(Panel::Presets) => presets_sheet(workspace).into_any(),
                            Some(Panel::Approvals) => approvals_sheet(workspace).into_any(),
                            Some(Panel::Usage) => usage_sheet(workspace).into_any(),
                            Some(Panel::Events) => events_sheet(workspace).into_any(),
                            Some(Panel::Commands) => commands_sheet(workspace).into_any(),
                            Some(Panel::Goal) => goal_sheet(workspace).into_any(),
                            Some(Panel::Compact) => compact_sheet(workspace).into_any(),
                            Some(Panel::AgentCommand) => agent_command_sheet(workspace).into_any(),
                            Some(Panel::Modes) => modes_sheet(workspace).into_any(),
                            Some(Panel::Background) => background_sheet(workspace).into_any(),
                            Some(Panel::Skills) => skills_sheet(workspace).into_any(),
                            Some(Panel::Permissions) => permissions_sheet(workspace).into_any(),
                            Some(Panel::Message) => message_sheet(workspace).into_any(),
                            Some(Panel::Runtimes) => runtimes_sheet(workspace).into_any(),
                            Some(Panel::DeferredPrompt) => deferred_prompt_sheet(workspace).into_any(),
                            Some(Panel::MoveSession) => projects::move_sheet(workspace).into_any(),
                            Some(Panel::Project) => projects::editor_sheet(workspace).into_any(),
                            None => ().into_any(),
                        }}
                    </div>
                </section>
            </div>
        </Show>
    }
}

/// A row's first line, with a check when it is the current choice.
fn choice_line(label: String, current: bool, class: &'static str) -> impl IntoView {
    view! {
        <span class="flex w-full items-center gap-2">
            <span class=format!("min-w-0 flex-1 truncate {class}")>{label}</span>
            {current.then(|| view! {
                <Glyph icon=icondata::LuCheck class="text-accent" />
                <span class="sr-only">"(current)"</span>
            })}
        </span>
    }
}

/// The ID an agent document's `models` entry writes for a choice named
/// `name` (a model's full key, a variant's ID, an ACP agent's option
/// value), muted and truncated, unless the name is that ID; `class` places
/// it (`DOC_ID_ROW` under a row's name, `DOC_ID_CHIP` in a chip).
fn document_id(name: &str, id: &str, class: &'static str) -> Option<impl IntoView + use<>> {
    cookie_agent_client_state::document_id(name, id).map(|id| {
        let id = id.to_owned();
        view! { <span class=class data-doc-id title=id.clone()>{id.clone()}</span> }
    })
}

/// A row's document ID: one muted line under its name, cut with an
/// ellipsis where the sheet is narrow.
const DOC_ID_ROW: &str = "block w-full truncate font-mono text-xs text-muted";
/// A chip's document ID, beside its name.
const DOC_ID_CHIP: &str = "min-w-0 max-w-[10rem] truncate font-mono text-xs text-muted";

fn agents_sheet(workspace: Workspace) -> impl IntoView {
    let current = workspace.draft.get_untracked().map(|draft| draft.agent);
    let agents = workspace.picker_agents();
    if agents.is_empty() {
        return empty_state(icondata::LuBot, "No agent can start a session.").into_any();
    }
    // The agents that cannot run come last, below a divider.
    let divider = agents
        .iter()
        .position(|agent| !agent.runnable_as_root && agent.unavailable.is_some());
    agents
        .into_iter()
        .enumerate()
        .map(|(index, agent)| {
            let divider = (divider == Some(index)).then(|| {
                view! {
                    <p class=format!("border-t border-line pt-2 {NOTE}") data-agents-divider>
                        "Cannot run now"
                    </p>
                }
            });
            let picked = agent.clone();
            let chosen = current.as_ref() == Some(&agent.id);
            // An ACP agent shows its runtime; one that is unavailable now is
            // dimmed and cannot be picked.
            let attached = workspace.agent_runtimes();
            // The draft's host stands only for the agent it was chosen for.
            let host = workspace.agent_host().filter(|_| chosen);
            let choices = cookie_agent_client_state::acp::runs_on_choices(&agent);
            // Several runtimes offer it: one option under it for each.
            if choices.len() > 1 {
                return view! {
                    {divider}
                    {agent_runtime_options(workspace, &agent, chosen, host, &attached, choices)}
                }
                .into_any();
            }
            let pickable = cookie_agent_client_state::acp::pickable(&agent, &attached, host.as_ref());
            let note = cookie_agent_client_state::acp::runtime_note(&agent, &attached, host.as_ref());
            let reason = (!pickable).then(|| {
                cookie_agent_client_state::acp::unpickable_reason(&agent, &attached, host.as_ref())
            });
            view! {
                {divider}
                <button
                    type="button"
                    class=ROW
                    class:bg-sel=chosen
                    class:opacity-50=!pickable
                    class:cursor-not-allowed=!pickable
                    data-agent=agent.id.to_string()
                    aria-disabled=(!pickable).to_string()
                    title=reason
                    on:click=move |_| {
                        if pickable {
                            workspace.pick_agent(&picked);
                            workspace.panel.set(None);
                        }
                    }
                >
                    {choice_line(agent.id.to_string(), chosen, "font-mono text-sm font-bold")}
                    {note.map(|note| view! { <span class=format!("font-mono {NOTE}") data-agent-runtime>{note}</span> })}
                    <span class=NOTE>{agent.description.clone()}</span>
                </button>
            }
            .into_any()
        })
        .collect_view()
        .into_any()
}

/// An ACP agent several runtimes offer, in the agent sheet: its name and
/// description, then an option for each runtime it runs on
/// (`docs/site/specs/acp-several-runtimes.md` section 3.7). Once a session
/// started it, the others are dimmed: it never moves.
fn agent_runtime_options(
    workspace: Workspace,
    agent: &cookie_agent_protocol::AgentDescriptor,
    chosen: bool,
    host: Option<cookie_agent_protocol::RuntimeId>,
    attached: &[cookie_agent_protocol::RuntimeId],
    choices: Vec<(cookie_agent_protocol::RuntimeId, bool)>,
) -> impl IntoView {
    let current = chosen
        .then(|| {
            cookie_agent_client_state::acp::placed_runtime(agent, attached, host.as_ref())
                .map(|offer| offer.runtime_id.clone())
        })
        .flatten();
    let settled = chosen && workspace.agent_settled(&agent.id);
    let options = choices
        .into_iter()
        .map(|(runtime, available)| {
            let here = current.as_ref() == Some(&runtime);
            let usable = agent.runnable_as_root && available && (!settled || here);
            let reason = if settled && !here {
                Some(format!(
                    "{} runs on {} for this session; start a new session to run it elsewhere",
                    agent.id,
                    current
                        .as_ref()
                        .map_or("another runtime".into(), ToString::to_string)
                ))
            } else if !available {
                Some(format!(
                    "{} is away, or the agent is disabled there",
                    runtime
                ))
            } else {
                None
            };
            let picked = agent.clone();
            let label = format!("on {runtime}");
            view! {
                <button
                    type="button"
                    class=CHIP
                    class:bg-sel=here
                    class:opacity-50=!usable
                    class:cursor-not-allowed=!usable
                    aria-pressed=here.to_string()
                    aria-disabled=(!usable).to_string()
                    data-runs-on=runtime.to_string()
                    title=reason
                    on:click=move |_| {
                        if usable {
                            workspace.pick_agent_on(&picked, runtime.clone());
                            workspace.panel.set(None);
                        }
                    }
                >
                    {choice_line(label, here, "font-mono text-xs")}
                </button>
            }
        })
        .collect_view();
    view! {
        <div
            class=format!("{ROW} cursor-default hover:bg-transparent active:bg-transparent dark:hover:bg-transparent dark:active:bg-transparent")
            class:bg-sel=chosen
            data-agent=agent.id.to_string()
        >
            {choice_line(agent.id.to_string(), chosen, "font-mono text-sm font-bold")}
            <span class=NOTE>{agent.description.clone()}</span>
            <div class="flex flex-wrap gap-2 pt-1">{options}</div>
        </div>
    }
}

/// What the ACP sheets say before the agent has offered any options.
const NO_AGENT_OPTIONS: &str =
    "The agent's options appear once it has started a session: send a first message.";

/// The ACP agent's options: each with its values, the current one pressed;
/// choosing a value sets it on the agent, or on a new session's draft.
fn agent_options_sheet(workspace: Workspace) -> impl IntoView {
    move || {
        let options = workspace.acp_options();
        if options.is_empty() {
            let message = if workspace.acp().is_some() {
                "The agent offers no options."
            } else {
                NO_AGENT_OPTIONS
            };
            return empty_state(icondata::LuSlidersHorizontal, message).into_any();
        }
        options
            .into_iter()
            .map(|option| {
                // A boolean is a switch that sets the other value.
                if option.kind == cookie_agent_protocol::AcpConfigOptionKind::Boolean {
                    let on = option.current_value == "true";
                    let option_id = option.id.clone();
                    let label = format!("{}: {}", option.name, if on { "off" } else { "on" });
                    return view! {
                        <div class="flex items-center gap-2 border-b border-line px-3 pb-2" data-agent-option=option.id.clone()>
                            <span class="flex min-w-0 flex-1 flex-col">
                                <span class="text-sm font-bold">{option.name.clone()}</span>
                                {option.description.clone().map(|description| view! { <span class=NOTE>{description}</span> })}
                            </span>
                            <button
                                type="button"
                                role="switch"
                                class=BUTTON
                                class:bg-sel=on
                                aria-checked=on.to_string()
                                aria-label=option.name.clone()
                                data-option-toggle=on.to_string()
                                on:click=move |_| {
                                    workspace.set_agent_option(
                                        option_id.clone(),
                                        (!on).to_string(),
                                        label.clone(),
                                    );
                                }
                            >
                                {if on { "On" } else { "Off" }}
                            </button>
                        </div>
                    }
                    .into_any();
                }
                let values = option
                    .values
                    .iter()
                    .map(|value| {
                        let chosen = value.value == option.current_value;
                        let (option_id, value_id) = (option.id.clone(), value.value.clone());
                        let label = format!("{}: {}", option.name, value.name);
                        view! {
                            <button
                                type="button"
                                class=BUTTON
                                class:bg-sel=chosen
                                aria-pressed=chosen.to_string()
                                data-option-value=value.value.clone()
                                title=value.description.clone().unwrap_or_default()
                                on:click=move |_| {
                                    if !chosen {
                                        workspace.choose_agent_option(
                                            option_id.clone(),
                                            value_id.clone(),
                                            label.clone(),
                                        );
                                    }
                                }
                            >
                                {value.name.clone()}
                            </button>
                        }
                    })
                    .collect_view();
                view! {
                    <div class="flex flex-col gap-1 border-b border-line pb-2" data-agent-option=option.id.clone()>
                        <span class="px-3 text-sm font-bold">{option.name.clone()}</span>
                        {option.description.clone().map(|description| view! { <span class=format!("px-3 {NOTE}")>{description}</span> })}
                        <div class="flex flex-wrap gap-2 px-3">{values}</div>
                    </div>
                }
                .into_any()
            })
            .collect_view()
            .into_any()
    }
}

/// The model sheet of an ACP agent (`docs/site/specs/acp-agents.md`
/// section 15, input and pickers): its model option's values as the models,
/// its effort option's as the variants, and the rest of its options a tap
/// away.
fn acp_models_sheet(workspace: Workspace) -> impl IntoView {
    move || {
        let options = workspace.acp_options();
        let model = cookie_agent_client_state::acp::model_option(&options).cloned();
        let effort = cookie_agent_client_state::acp::effort_option(&options).cloned();
        let choose = move |option: &cookie_agent_protocol::AcpConfigOption,
                           value: &cookie_agent_protocol::AcpConfigValue| {
            let (option_id, value_id) = (option.id.clone(), value.value.clone());
            let label = format!("{}: {}", option.name, value.name);
            move |_| {
                workspace.choose_agent_option(option_id.clone(), value_id.clone(), label.clone());
                workspace.panel.set(None);
            }
        };
        let models = model.map(|option| {
            option
                .values
                .iter()
                .map(|value| {
                    let chosen = value.value == option.current_value;
                    view! {
                        <button
                            type="button"
                            class=ROW
                            class:bg-sel=chosen
                            data-acp-model=value.value.clone()
                            on:click=choose(&option, value)
                        >
                            {choice_line(value.name.clone(), chosen, "text-sm font-bold")}
                            {document_id(&value.name, &value.value, DOC_ID_ROW)}
                            {value.description.clone().map(|description| view! { <span class=NOTE>{description}</span> })}
                        </button>
                    }
                })
                .collect_view()
        });
        let efforts = effort.map(|option| {
            let values = option
                .values
                .iter()
                .map(|value| {
                    let chosen = value.value == option.current_value;
                    view! {
                        <button
                            type="button"
                            class=BUTTON
                            class:bg-sel=chosen
                            aria-pressed=chosen.to_string()
                            data-acp-effort=value.value.clone()
                            title=value.description.clone().unwrap_or_default()
                            on:click=choose(&option, value)
                        >
                            <span class="min-w-0 truncate">{value.name.clone()}</span>
                            {document_id(&value.name, &value.value, DOC_ID_CHIP)}
                        </button>
                    }
                })
                .collect_view();
            view! {
                <div class="flex flex-col gap-1 border-b border-line pb-2">
                    <span class="px-3 text-sm font-bold">{option.name.clone()}</span>
                    <div class="flex flex-wrap gap-2 px-3">{values}</div>
                </div>
            }
        });
        if models.is_none() && efforts.is_none() {
            let message = if options.is_empty() {
                NO_AGENT_OPTIONS
            } else {
                "The agent offers no model option."
            };
            return view! {
                {empty_state(icondata::LuCpu, message)}
                {(!options.is_empty()).then(|| view! {
                    <button
                        type="button"
                        class=ROW
                        id="agent-options-more"
                        on:click=move |_| workspace.panel.set(Some(Panel::AgentOptions))
                    >
                        {choice_line("All agent options".to_owned(), false, "text-sm")}
                    </button>
                })}
            }
            .into_any();
        }
        view! {
            <div class="flex flex-col gap-1 border-b border-line pb-2">{models}</div>
            {efforts}
            <button
                type="button"
                class=ROW
                id="agent-options-more"
                on:click=move |_| workspace.panel.set(Some(Panel::AgentOptions))
            >
                {choice_line("All agent options".to_owned(), false, "text-sm")}
                <span class=NOTE>"Mode and the agent's other settings"</span>
            </button>
        }
        .into_any()
    }
}

fn models_sheet(workspace: Workspace) -> impl IntoView {
    if workspace.draft_is_acp() {
        return acp_models_sheet(workspace).into_any();
    }
    let current = workspace.draft.get_untracked().map(|draft| draft.model);
    let models = workspace.models();
    if models.is_empty() {
        return empty_state(
            icondata::LuCpu,
            "No model is available. Connect a provider in Settings › Providers.",
        )
        .into_any();
    }
    models
        .into_iter()
        .map(|model| {
            let key = model.key.clone();
            let base = ModelSelection {
                model: key.clone(),
                variant: model.default_variant.clone(),
            };
            let variants = model
                .variants
                .iter()
                .map(|variant| {
                    let selection = ModelSelection {
                        model: key.clone(),
                        variant: Some(variant.id.clone()),
                    };
                    let chosen = current.as_ref() == Some(&selection);
                    view! {
                        <button
                            type="button"
                            class=BUTTON
                            class:bg-sel=chosen
                            aria-pressed=chosen.to_string()
                            data-variant=variant.id.to_string()
                            on:click=move |_| {
                                workspace.pick_model(selection.clone());
                                workspace.panel.set(None);
                            }
                        >
                            <span class="min-w-0 truncate">{variant.display_name.clone()}</span>
                            {document_id(&variant.display_name, variant.id.as_str(), DOC_ID_CHIP)}
                        </button>
                    }
                })
                .collect_view();
            let chosen = current.as_ref().is_some_and(|current| current.model == key);
            view! {
                <div class="flex flex-col gap-1 border-b border-line pb-2">
                    <button
                        type="button"
                        class=ROW
                        class:bg-sel=chosen
                        on:click=move |_| {
                            workspace.pick_model(base.clone());
                            workspace.panel.set(None);
                        }
                    >
                        {choice_line(model.display_name.to_string(), chosen, "text-sm font-bold")}
                        {document_id(model.display_name.as_str(), &key.to_string(), DOC_ID_ROW)}
                    </button>
                    <div class="flex flex-wrap gap-2 px-3">{variants}</div>
                </div>
            }
        })
        .collect_view()
        .into_any()
}

fn presets_sheet(workspace: Workspace) -> impl IntoView {
    let current = workspace.preset.get_untracked();
    std::iter::once(None)
        .chain(workspace.presets().into_iter().map(Some))
        .map(|preset| {
            let label = preset.clone().unwrap_or_else(|| "default".into());
            let chosen = preset == current;
            view! {
                <button
                    type="button"
                    class=ROW
                    class:bg-sel=chosen
                    on:click=move |_| {
                        workspace.pick_preset(preset.clone());
                        workspace.panel.set(None);
                    }
                >
                    {choice_line(label, chosen, "text-sm")}
                </button>
            }
        })
        .collect_view()
}

fn approvals_sheet(workspace: Workspace) -> impl IntoView {
    move || {
        let approvals = workspace.approvals();
        if approvals.is_empty() {
            return empty_state(icondata::LuCircleCheck, "No approval is waiting.").into_any();
        }
        approvals
            .into_iter()
            .map(|approval| {
                let (title, preview, icon) = workspace.with_store(|store| {
                    store
                        .sessions
                        .get(&approval.session_id)
                        .and_then(|state| state.tools.get(&approval.tool_call_id))
                        .map(|tool| {
                            (
                                cookie_agent_client_state::acp::approval_title(&tool.header_text())
                                    .to_owned(),
                                display_html(&tool.presentation.preview, BlockPlace::plain),
                                icons::hint_icon(
                                    tool.presentation.hint.as_ref().map(|hint| hint.rendered()),
                                ),
                            )
                        })
                        .unwrap_or_else(|| ("a tool call".into(), String::new(), icons::hint_icon(None)))
                });
                let reviewer = approval
                    .reviewer
                    .as_ref()
                    .and_then(cookie_agent_client_state::ReviewerNote::escalation_line)
                    .map(|line| view! { <p class=NOTE data-reviewer>{line}</p> });
                // The note goes with a rejection, to the agent.
                let note = RwSignal::new(String::new());
                let decide = {
                    let approval = approval.clone();
                    move |decision: ApprovalUserDecision, option: Option<String>| {
                        let approval = approval.clone();
                        move |_| {
                            workspace.respond(
                                approval.clone(),
                                decision,
                                option.clone(),
                                Some(note.get_untracked()),
                            );
                        }
                    }
                };
                let buttons = if approval.agent_options.is_empty() {
                    let tree = approval.constraints.allow_tree_grant.then(|| {
                        view! {
                            <button type="button" class=BUTTON data-decision="approve_tree" on:click=decide(ApprovalUserDecision::ApproveTree, None)>
                                <Glyph icon=icondata::LuCheckCheck />
                                "Approve for this session tree"
                            </button>
                        }
                    });
                    view! {
                        <button type="button" class=PRIMARY data-decision="approve_once" on:click=decide(ApprovalUserDecision::ApproveOnce, None)>
                            <Glyph icon=icondata::LuCheck />
                            "Approve once"
                        </button>
                        {tree}
                        <button type="button" class=BUTTON data-decision="reject" on:click=decide(ApprovalUserDecision::Reject, None)>
                            <Glyph icon=icondata::LuX />
                            "Reject"
                        </button>
                    }
                    .into_any()
                } else {
                    // An ACP agent's own options, in its order.
                    approval
                        .agent_options
                        .iter()
                        .map(|option| {
                            let allows = option.kind.allows();
                            let decision = if allows {
                                ApprovalUserDecision::ApproveOnce
                            } else {
                                ApprovalUserDecision::Reject
                            };
                            let icon = if allows {
                                icondata::LuCheck
                            } else {
                                icondata::LuX
                            };
                            // The agent's allow-once option leads.
                            let class = if option.kind
                                == cookie_agent_protocol::ApprovalAgentOptionKind::AllowOnce
                            {
                                PRIMARY
                            } else {
                                BUTTON
                            };
                            view! {
                                <button
                                    type="button"
                                    class=class
                                    data-decision=if allows { "approve_once" } else { "reject" }
                                    data-agent-answer=option.option_id.clone()
                                    on:click=decide(decision, Some(option.option_id.clone()))
                                >
                                    <Glyph icon=icon />
                                    {option.name.clone()}
                                </button>
                            }
                        })
                        .collect_view()
                        .into_any()
                };
                let source = approval_source(workspace, &approval).map(|source| {
                    view! {
                        <span class="inline-flex shrink-0 items-center rounded-full bg-sunken px-2 py-0.5 text-xs font-normal" data-approval-source>
                            {source}
                        </span>
                    }
                });
                view! {
                    <div class="approval flex flex-col gap-2 rounded-lg border border-accent bg-surface-2 p-3">
                        <p class="flex items-center gap-2 font-mono text-sm font-bold">
                            <Glyph icon=icon class="text-accent" />
                            <span class="min-w-0 break-words">{title}</span>
                            {source}
                        </p>
                        <div class="min-w-0" inner_html=preview></div>
                        {reviewer}
                        <input
                            type="text"
                            class="min-h-9 w-full min-w-0 rounded-md border border-line-2 bg-surface px-2 text-sm focus:border-accent focus:outline-none"
                            data-approval-note
                            placeholder="Note for the agent, sent with a rejection"
                            prop:value=move || note.get()
                            on:input=move |event| note.set(event_target_value(&event))
                        />
                        <div class="flex flex-wrap gap-2">{buttons}</div>
                    </div>
                }
            })
            .collect_view()
            .into_any()
    }
}

/// What a waiting approval's call belongs to, when its session runs an ACP
/// agent: `cookie` for one of cookie's tools the agent asked to call, else
/// the server of an MCP tool the agent connected itself.
fn approval_source(
    workspace: Workspace,
    approval: &cookie_agent_client_state::ApprovalState,
) -> Option<String> {
    workspace.with_store(|store| {
        let state = store.sessions.get(&approval.session_id)?;
        let tool = state.tools.get(&approval.tool_call_id)?;
        cookie_agent_client_state::acp::tool_source_label(state, tool)
    })
}

fn usage_sheet(workspace: Workspace) -> impl IntoView {
    move || match workspace.usage.get() {
        None => loading("Reading usage…").into_any(),
        Some(usage) => {
            // The figures of a rollup, then its prompt-cache hit rate with
            // the tokens behind it in a note (a finger has no tooltip).
            let rows = |usage: &cookie_agent_protocol::UsageRollup, scope: &'static str| {
                let mut rows = vec![
                    ("input tokens", format_count(usage.input_tokens)),
                    ("output tokens", format_count(usage.output_tokens)),
                    ("reasoning tokens", format_count(usage.reasoning_tokens)),
                    ("cache reads", format_count(usage.cache_read_tokens)),
                    ("cache writes", format_count(usage.cache_write_tokens)),
                    ("requests", format_count(usage.request_count)),
                ];
                if let Some(cost) = usage.estimated_cost_usd {
                    rows.push(("estimated cost", format!("${cost:.4}")));
                }
                let hit = CacheHit::of(usage);
                let detail = hit.detail();
                view! {
                    <dl class="grid grid-cols-2 text-sm">
                        {rows.into_iter()
                            .map(|(name, value)| view! {
                                <dt class="border-b border-line py-1 text-muted">{name}</dt>
                                <dd class="border-b border-line py-1 text-right font-mono">{value}</dd>
                            })
                            .collect_view()}
                        <dt class="pt-1 text-muted">"cache hit rate"</dt>
                        <dd
                            class="pt-1 text-right font-mono"
                            class:opacity-60=hit.rate.is_none()
                            data-cache-hit=scope
                            title=detail.clone()
                        >
                            {hit.label()}
                        </dd>
                    </dl>
                    <p class=format!("{NOTE} pb-1") data-cache-detail=scope>{detail}</p>
                    {usage_models(usage, scope)}
                }
            };
            let session = match &usage.session {
                Ok(rollup) => rows(rollup, "session").into_any(),
                Err(error) => view! { <p class="text-sm">{error.clone()}</p> }.into_any(),
            };
            let tree = match &usage.tree {
                Ok((rollup, count)) => view! {
                    <h3 class="mt-4 text-sm font-bold">
                        {if *count == 1 { "Session tree (1 session)".to_owned() } else { format!("Session tree ({count} sessions)") }}
                    </h3>
                    {rows(rollup, "tree")}
                }
                    .into_any(),
                Err(error) => view! { <p class="text-sm">{error.clone()}</p> }.into_any(),
            };
            // The bottom bar of a phone has no room for the context size.
            let context = workspace
                .viewed
                .with_untracked(|viewed| viewed.context_tokens);
            view! {
                {context.map(|tokens| view! {
                    <p class="flex items-center gap-2 text-sm" id="usage-context">
                        <Glyph icon=icondata::LuGauge class="text-accent" />
                        {format!("Context: {} tokens in the last request", format_count(tokens))}
                    </p>
                })}
                <h3 class="text-sm font-bold">"This session"</h3>
                {session}
                {tree}
            }
            .into_any()
        }
    }
}

/// A rollup's figures by model, as the TUI's usage panel lists them: each
/// model's name on a line of its own, its figures under it, so a long name
/// does not squeeze them on a narrow sheet.
fn usage_models(
    usage: &cookie_agent_protocol::UsageRollup,
    scope: &'static str,
) -> Option<impl IntoView + use<>> {
    if usage.by_model.is_empty() {
        return None;
    }
    const CELL: &str = "whitespace-nowrap pb-1 pl-3 text-right font-mono first:pl-0";
    let models = models_by_cost(usage)
        .into_iter()
        .map(|(model, usage)| {
            let hit = CacheHit::of_model(usage);
            view! {
                <tbody class="border-b border-line">
                    <tr>
                        <td colspan="6" class="break-all pt-1 font-mono">{model.to_string()}</td>
                    </tr>
                    <tr data-model=model.to_string()>
                        <td class=CELL>{format_count(usage.request_count)}</td>
                        <td class=CELL>{format_count(usage.input_tokens)}</td>
                        <td class=CELL>{format_count(usage.output_tokens)}</td>
                        <td class=CELL>{format_count(usage.reasoning_tokens)}</td>
                        <td class=CELL title=hit.detail()>{hit.label()}</td>
                        <td class=CELL>{format_cost(usage.estimated_cost_usd)}</td>
                    </tr>
                </tbody>
            }
        })
        .collect_view();
    Some(view! {
        <div class="overflow-x-auto pb-1">
            <table class="w-full text-sm" data-usage-models=scope>
                <thead class="text-xs text-muted">
                    <tr class="border-b border-line">
                        {["requests", "input", "output", "reasoning", "cache hit", "cost"]
                            .map(|name| view! { <th class="whitespace-nowrap py-1 pl-3 text-right font-normal first:pl-0">{name}</th> })
                            .collect_view()}
                    </tr>
                </thead>
                {models}
            </table>
        </div>
    })
}

fn events_sheet(workspace: Workspace) -> impl IntoView {
    [
        (EventLevel::Debug, "Debug and above"),
        (EventLevel::Info, "Info and above"),
        (EventLevel::Warning, "Warnings and errors"),
        (EventLevel::Error, "Errors only"),
    ]
    .into_iter()
    .map(|(level, label)| {
        let chosen = move || workspace.level.get() == level;
        view! {
            <button
                type="button"
                class=ROW
                class:bg-sel=chosen
                aria-pressed=move || chosen().to_string()
                on:click=move |_| workspace.set_level(level)
            >
                {move || choice_line(label.to_owned(), chosen(), "text-sm")}
            </button>
        }
    })
    .collect_view()
}

/// The transcript modes to choose from, as Settings › Display offers them.
fn transcript_sheet(workspace: Workspace) -> impl IntoView {
    use cookie_agent_client_state::transcript_mode::TranscriptMode;
    [
        (
            TranscriptMode::Reduced,
            "Reduced: agent work folded into one row per reply",
        ),
        (
            TranscriptMode::Full,
            "Full: every thinking block and tool call",
        ),
    ]
    .into_iter()
    .map(|(mode, label)| {
        let chosen = move || workspace.transcript.get() == mode;
        view! {
            <button
                type="button"
                class=ROW
                class:bg-sel=chosen
                aria-pressed=move || chosen().to_string()
                data-transcript-mode=mode.name()
                on:click=move |_| workspace.set_transcript(mode)
            >
                {move || choice_line(label.to_owned(), chosen(), "text-sm")}
            </button>
        }
    })
    .collect_view()
}

/// The session's ⋮ menu: every command, by name, icon, and what it does,
/// in rows a finger can hit; each opens its sheet. The session's ACP
/// agent's commands follow, each badged as the agent's; one of them stands
/// in for cookie's command of its name. A filter on top narrows both.
fn commands_sheet(workspace: Workspace) -> impl IntoView {
    let has_crons = workspace.has_crons.get_untracked();
    let filter = RwSignal::new(String::new());
    let query = move || filter.get().trim().trim_start_matches('/').to_lowercase();
    let matches = |query: &str, texts: [&str; 2]| {
        query.is_empty() || texts.iter().any(|text| text.to_lowercase().contains(query))
    };
    let own = move || {
        let query = query();
        MENU.iter()
            .filter(|entry| crate::commands::offered(entry.command, has_crons))
            .filter(|entry| !workspace.agent_replaces(entry.id))
            .filter(|entry| matches(&query, [entry.label, entry.description]))
            .map(|entry| {
                let command = entry.command;
                view! {
                    <button
                        type="button"
                        class=format!("{ROW} min-h-12")
                        data-command=entry.id
                        on:click=move |_| {
                            workspace.panel.set(None);
                            workspace.run_command(command);
                        }
                    >
                        <span class="flex w-full items-center gap-3">
                            <Glyph icon=entry.icon class="w-5 justify-center text-accent" />
                            <span class="flex min-w-0 flex-1 flex-col">
                                <span class="text-sm font-bold">{entry.label}</span>
                                <span class=NOTE>{entry.description}</span>
                            </span>
                        </span>
                    </button>
                }
            })
            .collect_view()
    };
    let commands = StoredValue::new(workspace.agent_commands());
    let agent = move || {
        let query = query();
        let rows = commands.with_value(|commands| {
            commands
                .iter()
                .filter(|command| {
                    cookie_agent_client_state::acp::command_listed(
                        &command.name,
                        query.trim_start_matches('/'),
                    ) && matches(
                        &query,
                        [command.name.as_str(), command.description.as_str()],
                    )
                })
                .cloned()
                .collect::<Vec<_>>()
        });
        (!rows.is_empty()).then(|| {
            let rows = rows
                .into_iter()
                .map(|command| {
                    let name = command.name.clone();
                    let hint = command.input_hint.clone().unwrap_or_default();
                    let description = command.description.clone();
                    view! {
                        <button
                            type="button"
                            class=format!("{ROW} min-h-12")
                            data-agent-command=name.clone()
                            on:click=move |_| workspace.choose_agent_command(command.clone())
                        >
                            <span class="flex w-full min-w-0 flex-col">
                                <span class="flex flex-wrap items-baseline gap-x-2">
                                    <span class="agent-badge rounded border border-accent px-1 text-[0.625rem] uppercase">"agent"</span>
                                    <span class="font-mono text-sm font-bold">{cookie_agent_client_state::acp::command_label(&name)}</span>
                                    <span class=format!("font-mono {NOTE}")>{hint}</span>
                                </span>
                                <span class=NOTE>{description}</span>
                            </span>
                        </button>
                    }
                })
                .collect_view();
            view! {
                <h3 class="mt-2 px-1 text-sm font-bold">"The agent's commands"</h3>
                <div id="agent-commands" class="flex flex-col gap-2">{rows}</div>
            }
        })
    };
    view! {
        <input
            id="command-filter"
            type="search"
            class=depth::FIELD
            placeholder="Filter commands"
            aria-label="Filter commands"
            prop:value=move || filter.get()
            on:input=move |event| filter.set(event_target_value(&event))
        />
        {own}
        {agent}
    }
}

/// One of the agent's commands: what it does, a box for its optional
/// input, and **Send**, which sends `/<name> <input>` to the agent.
fn agent_command_sheet(workspace: Workspace) -> impl IntoView {
    let Some(command) = workspace.agent_command.get_untracked() else {
        return ().into_any();
    };
    let input = RwSignal::new(String::new());
    let name = StoredValue::new(command.name.clone());
    let send = move || workspace.run_agent_command(&name.get_value(), &input.get_untracked());
    let placeholder = command
        .input_hint
        .clone()
        .unwrap_or_else(|| "Input for the command".to_owned());
    view! {
        <p class="flex flex-wrap items-baseline gap-x-2">
            <span class="agent-badge rounded border border-accent px-1 text-[0.625rem] uppercase">"agent"</span>
            <span id="agent-command-name" class="font-mono text-sm font-bold">{cookie_agent_client_state::acp::command_label(&command.name)}</span>
        </p>
        <p class=NOTE>{command.description.clone()}</p>
        <label class="flex flex-col gap-1 text-sm">
            <span class="font-bold">"Input " <span class="font-normal text-muted">"(optional)"</span></span>
            <textarea
                id="agent-command-input"
                rows="2"
                class=depth::FIELD
                placeholder=placeholder
                prop:value=move || input.get()
                on:input=move |event| input.set(event_target_value(&event))
                on:keydown=move |event: web_sys::KeyboardEvent| {
                    if event.key() == "Enter" && !event.shift_key() {
                        event.prevent_default();
                        send();
                    }
                }
            ></textarea>
        </label>
        <div class="flex flex-wrap items-center gap-2">
            <button
                type="button"
                id="agent-command-send"
                class=PRIMARY
                disabled=move || !workspace.writable()
                on:click=move |_| send()
            >
                <Glyph icon=icondata::LuArrowUp />
                "Send to the agent"
            </button>
        </div>
    }
    .into_any()
}

/// The permission mode picker: every mode with its shield and what it
/// does, the current one checked; a click sets it.
fn modes_sheet(workspace: Workspace) -> impl IntoView {
    if let Some(option) =
        cookie_agent_client_state::acp::mode_option(&workspace.acp_options()).cloned()
    {
        return agent_modes_sheet(workspace, option).into_any();
    }
    view! {
        {MODES
            .into_iter()
            .map(|choice| {
                let chosen = move || workspace.permission_mode() == choice.mode;
                view! {
                    <button
                        type="button"
                        class=ROW
                        class:bg-sel=chosen
                        aria-pressed=move || chosen().to_string()
                        data-mode=choice.name
                        on:click=move |_| {
                            workspace.set_permission_mode(choice.mode);
                            workspace.panel.set(None);
                        }
                    >
                        <span class="flex w-full items-center gap-3">
                            <Glyph icon=icons::permission_mode(choice.mode).0 class="w-5 justify-center text-accent" />
                            <span class="flex min-w-0 flex-1 flex-col">
                                {move || choice_line(choice.name.to_owned(), chosen(), "font-mono text-sm font-bold")}
                                <span class=NOTE>{choice.description}</span>
                            </span>
                        </span>
                    </button>
                }
            })
            .collect_view()}
        <p class=format!("mt-2 flex items-start gap-2 border-t border-line px-3 pt-3 {NOTE}")>
            <Glyph icon=icondata::LuInfo class="mt-0.5" />
            {FOOTNOTE}
        </p>
    }
    .into_any()
}

/// The permission mode picker of a session an ACP agent runs: the agent's
/// own modes, since its harness decides what it asks the user, while
/// cookie applies its `deny` and `allow` rules
/// (`docs/site/specs/acp-permissions.md` section 4).
fn agent_modes_sheet(
    workspace: Workspace,
    option: cookie_agent_protocol::AcpConfigOption,
) -> impl IntoView {
    let id = option.id.clone();
    let current = option.current_value.clone();
    let rows = option
        .values
        .into_iter()
        .map(|value| {
            let chosen = value.value == current;
            let option_id = id.clone();
            let label = format!("{}: {}", option.name, value.name);
            let picked = value.value.clone();
            view! {
                <button
                    type="button"
                    class=ROW
                    class:bg-sel=chosen
                    aria-pressed=chosen.to_string()
                    data-agent-mode=value.value.clone()
                    on:click=move |_| {
                        workspace.choose_agent_option(option_id.clone(), picked.clone(), label.clone());
                        workspace.panel.set(None);
                    }
                >
                    <span class="flex min-w-0 flex-1 flex-col">
                        {choice_line(value.name.clone(), chosen, "font-mono text-sm font-bold")}
                        {value.description.clone().map(|description| view! { <span class=NOTE>{description}</span> })}
                    </span>
                </button>
            }
        })
        .collect_view();
    view! {
        {rows}
        <p class=format!("mt-2 flex items-start gap-2 border-t border-line px-3 pt-3 {NOTE}")>
            <Glyph icon=icondata::LuInfo class="mt-0.5" />
            "The agent decides in this mode what it asks you. Cookie still refuses what your deny rules deny and allows what your allow rules allow."
        </p>
    }
}
