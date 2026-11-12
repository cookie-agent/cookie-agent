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
    history_scroll::HistoryScroll,
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
mod asset_details;
mod asset_page;
mod asset_panel;
mod assets;
mod cards;
mod config_form;
mod cron;
mod depth;
mod effort;
mod install;
mod live;
mod loader;
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
mod shares;
mod shell;
mod sidebar;
mod skills;
mod tool_groups;

pub(crate) use assets::AssetDialogs;
pub(crate) use install::{Install, InstallAction, InstallBanner};
pub(crate) use media_viewer::MediaViewer;
pub(crate) use restart::{RestartBanner, RestartDialog};
pub(crate) use shares::{SessionShareDialogView, ShareDialogView};

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

/// How long a finger holds a session row, or a message, for its actions.
const LONG_PRESS: Duration = Duration::from_millis(500);

/// How long after the composer loses focus a phone's bottom bar returns.
const BAR_RETURN: Duration = Duration::from_millis(250);

/// Every button is at least 44 px square, with a hover and a pressed state.
/// A labelled button, bordered.
const BUTTON: &str = "inline-flex min-h-11 min-w-11 items-center justify-center gap-2 rounded-lg bg-raised px-3 text-[0.8125rem] font-medium text-fg shadow-[0_0_0_1px_var(--color-line-2),0_1px_2px_rgb(0_0_0/0.04)] transition-colors hover:bg-sunken active:bg-press disabled:cursor-not-allowed disabled:opacity-50 disabled:hover:bg-raised lg:min-h-9 lg:min-w-9";
/// The main action of a form or the composer.
const PRIMARY: &str = "inline-flex min-h-11 min-w-11 items-center justify-center gap-2 rounded-lg bg-accent px-3 text-[0.8125rem] font-semibold text-on-accent shadow-[inset_0_1px_0_rgb(255_255_255/0.14),0_1px_2px_rgb(0_0_0/0.12)] transition-[filter,background-color] hover:brightness-110 active:bg-accent-press disabled:cursor-not-allowed disabled:opacity-50 disabled:hover:brightness-100 lg:min-h-9 lg:min-w-9";
/// A button that is only an icon: it has an `aria-label` and a `title`.
const ICON_BUTTON: &str = "inline-flex min-h-11 min-w-11 shrink-0 items-center justify-center rounded-lg text-muted transition-colors hover:bg-hover hover:text-fg active:bg-press disabled:cursor-not-allowed disabled:opacity-50 disabled:hover:bg-transparent lg:min-h-9 lg:min-w-9";
/// A bottom bar control showing its current value: the agent, the model,
/// and the permission mode share it. 32 px to see, 44 px to hit.
// The focus ring is drawn inside the chip: the composer's row of chips
// clips what reaches past it, which cut an outside ring to "( Auto )"
// (docs/site/specs/web-polish.md, B2).
const CHIP: &str = "hit-area inline-flex h-8 min-w-0 max-w-[14rem] shrink-0 items-center gap-1.5 rounded-lg px-2 text-[0.8125rem] whitespace-nowrap text-fg-2 transition-colors hover:bg-hover hover:text-fg active:bg-press aria-expanded:bg-hover focus-visible:-outline-offset-2";
/// A status bar indicator that does something when clicked.
/// A small borderless icon button of the sidebar's session tools.
const SIDEBAR_TOOL: &str = "hit-area inline-flex h-7 w-7 shrink-0 items-center justify-center rounded-md text-muted transition-colors hover:bg-hover hover:text-fg active:bg-press";
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
/// A button of the row's actions a long press laid out, two to a line:
/// its icon, then its name, on two lines where a narrow phone needs them.
const REVEALED: &str = "flex min-h-11 min-w-0 items-center gap-2 rounded-lg bg-raised px-3 py-1.5 text-left text-sm shadow-[0_0_0_1px_var(--color-line-2)] hover:bg-sunken active:bg-press";
/// A session's title as a field: in a session row, and in the title bar.
const TITLE_FIELD: &str = "min-h-9 w-full min-w-0 rounded-lg bg-surface px-2 text-sm font-semibold text-fg shadow-[inset_0_0_0_1px_var(--color-accent),0_0_0_3px_var(--color-accent-soft)] focus:outline-none";
/// "Use the automatic title", under an emptied title field.
const AUTO_TITLE: &str = "inline-flex min-h-9 items-center gap-1.5 rounded-md px-2 text-xs font-bold text-accent hover:bg-press active:bg-press";
/// A row of a list: two lines, the second a note.
const ROW: &str = "flex min-h-11 w-full shrink-0 flex-col items-start gap-0.5 rounded-lg px-3 py-2 text-left transition-colors hover:bg-hover active:bg-press";
const NOTE: &str = "text-xs text-muted";
/// An item of a menu: an icon, a name, and what is live behind it at the
/// end; a finger's height below desktop widths.
const MENU_ROW: &str = "flex min-h-11 w-full shrink-0 items-center gap-2.5 rounded-lg px-2.5 text-left text-[0.8125rem] transition-colors hover:bg-hover active:bg-press lg:min-h-9";
/// A message about something that went wrong.
const ERROR: &str =
    "rounded-lg bg-bad-soft px-3 py-2 text-[0.8125rem] whitespace-pre-wrap break-words text-bad";

/// Something to look at, though nothing failed: a configuration
/// diagnostic. An amber callout with its icon (docs/site/specs/web-polish.md,
/// B5).
const WARNING: &str = "flex gap-2 rounded-lg bg-warn-soft px-3 py-2 text-[0.8125rem] text-fg";
/// Inline code, as a Markdown reply draws it.
const INLINE_CODE: &str =
    "rounded-[0.3rem] bg-sunken px-[0.3em] py-[0.1em] font-mono text-[0.86em]";

/// A line of text with its code spans drawn as code instead of showing
/// their backticks (`format::code_spans`).
fn with_code(line: &str) -> impl IntoView + use<> {
    use cookie_agent_client_state::format::{Span, code_spans};
    code_spans(line)
        .into_iter()
        .map(|span| match span {
            Span::Text(text) => text.to_owned().into_any(),
            Span::Code(code) => {
                view! { <code class=INLINE_CODE>{code.to_owned()}</code> }.into_any()
            }
        })
        .collect_view()
}

/// What a sheet shows when it has nothing to list.
fn empty_state(icon: icons::Icon, text: &'static str) -> impl IntoView {
    view! {
        <div class="flex flex-col items-center gap-3 px-4 py-12 text-center text-[0.8125rem] text-muted">
            <span class="inline-flex h-11 w-11 items-center justify-center rounded-xl bg-sunken text-muted shadow-[inset_0_0_0_1px_var(--color-line)]">
                <Glyph icon=icon class="text-xl" />
            </span>
            <p class="max-w-xs">{text}</p>
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
    provide_context(workspace);
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
            <Show when=move || !workspace.is_guest()>
                <Sidebar workspace=workspace connection=connection theme=theme />
            </Show>
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
                <shares::GuestBar workspace=workspace />
                <TitleBar workspace=workspace />
                // A new session shows its runtime cards where the
                // transcript will be, above the composer; a session that
                // loads shows a skeleton there until it has.
                <div class="relative flex min-h-0 flex-1 flex-col">
                    <Transcript workspace=workspace />
                    <loader::TranscriptLoader workspace=workspace />
                </div>
                <DraftFiles workspace=workspace />
                // An agent's question waits above the composer, scrolling
                // within itself so the transcript keeps some room.
                <div class="max-h-[60dvh] shrink-0 overflow-y-auto px-2 empty:hidden">
                    <questions::QuestionCard workspace=workspace />
                </div>
                <cards::DraftNotice workspace=workspace />
                // An approval waiting takes the composer's place until it
                // is answered.
                <div class="relative">
                    <StatusTip workspace=workspace />
                    <Show
                        when=move || !workspace.approvals().is_empty()
                        fallback=move || view! { <Composer workspace=workspace composing=composing /> }
                    >
                        <ApprovalDock workspace=workspace />
                    </Show>
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
    let press = session_rows::RowPress::new();
    let headers = projects::HeaderActions::new();
    let list = NodeRef::<html::Nav>::new();
    let rows = session_rows::SessionRows {
        workspace,
        slots: session_rows::SessionSlots::new(workspace),
        now,
        actions,
        press,
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
    // The filter left every section empty: the list says so above them,
    // with the way to change it. A memo of its own, so the sections, which
    // change as a session opens, never draw the list anew: that would lose
    // the list's scroll, which the browser keeps on a row it shows.
    let offer = Memo::new(move |_| {
        !searching.get()
            && projects::sections_hold_none(sections)
            && (workspace.filter_changes() > 0 || workspace.has_older_sessions.get())
    });
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
                    return view! {
                        {move || offer.get().then(|| filter_offer(workspace, icondata::LuListFilter, "No sessions in this time range or filter."))}
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
/// A desktop opens it beside the sidebar, near the top of the window and
/// never past its foot, and a click outside or Escape closes it; a narrower window drops it down
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
                    class="min-h-11 w-full rounded-lg bg-surface px-2 text-sm font-normal text-fg hover:shadow-[inset_0_0_0_1px_var(--color-faint)] focus:shadow-[inset_0_0_0_1px_var(--color-accent),0_0_0_3px_var(--color-accent-soft)] focus:outline-none shadow-[inset_0_0_0_1px_var(--color-line-2)]"
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
            class="session-filter grid lg:fixed lg:top-24 lg:left-[16.5rem] lg:z-40 lg:w-80"
            data-open=move || open().to_string()
            inert=move || (!open()).then_some("")
        >
            <div class="min-h-0 overflow-hidden">
                <div
                    id="session-filter"
                    role="dialog"
                    aria-label="Filter sessions"
                    class="flex flex-col gap-3 rounded-xl p-3 text-fg bg-raised ring-1 ring-line lg:max-h-[calc(100dvh-7rem)] lg:overflow-y-auto lg:shadow-menu"
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
                                class="min-h-11 rounded-lg bg-surface px-2 text-sm font-normal text-fg hover:shadow-[inset_0_0_0_1px_var(--color-faint)] shadow-[inset_0_0_0_1px_var(--color-line-2)]"
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
                <div class="flex flex-col gap-2 rounded-xl p-2 text-sm bg-raised shadow-[0_0_0_1px_var(--color-accent),0_0_0_4px_var(--color-accent-soft)]" role="alertdialog" data-delete-picked-box>
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
    /// The row whose ⋮ menu is open.
    menu: RwSignal<Option<SessionId>>,
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
    press: session_rows::RowPress,
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
    // The time the list orders by (the user's last input or delegation),
    // not the last event, which a working session moves on its own, so
    // that "1d" never sits above "22h" (docs/site/specs/web-polish.md, B8).
    let listed_at = cookie_agent_client_state::rows::ordering_activity(&session);
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
        (hovered.get() == Some(session_id) || menu.get() == Some(session_id))
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
            if workspace.has_shares.get() {
                items.push(row_menu::MenuItem {
                    id: "share",
                    label: "Share conversation…",
                    icon: icondata::LuShare2,
                    act: Callback::new(move |()| {
                        if revealed_row {
                            revealed.set(None);
                        }
                        workspace.open_session_share_dialog_of(session_id);
                    }),
                    danger: false,
                });
            }
            items
        }
    };
    // Everything the row offers, Rename first and Delete last: a
    // right-click's menu, and the buttons a long press lays out
    // (`revealed_row`), which let go of the row once one is pressed.
    let all_items = move |revealed_row: bool| {
        let mut items = vec![row_menu::MenuItem {
            id: "rename",
            label: "Rename",
            icon: icondata::LuPencil,
            act: Callback::new(move |()| actions.begin_rename(workspace, session_id)),
            danger: false,
        }];
        items.extend(menu_items(revealed_row)());
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
    // A finger's long press shows the row's buttons.
    let on_pointerdown = move |event: ev::PointerEvent| {
        if !is_renaming() && !picking() {
            press.down(session_id, &event, actions);
        }
    };
    view! {
        <div
            class="session-row group relative"
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
                on:pointermove=move |event: ev::PointerEvent| press.moved(&event)
                on:pointerup=move |_| press.up()
                on:pointercancel=move |_| press.up()
                on:touchend=move |event: ev::TouchEvent| {
                    // Chrome ends a long press with a click, which would
                    // land on the buttons it just showed under the finger.
                    if !press.click() {
                        event.prevent_default();
                    }
                }
                on:contextmenu=move |event: ev::MouseEvent| {
                    // A touch screen's long press offers the row's actions,
                    // not the browser's menu.
                    if crate::browser::from_touch(&event) {
                        event.prevent_default();
                        if !is_renaming() && !picking() {
                            session_rows::RowPress::held(session_id, actions);
                        }
                        return;
                    }
                    if revealed.get_untracked() == Some(session_id) {
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
                    if press.click() {
                        // From another view, the open session's row goes
                        // back to its conversation.
                        if current() && workspace.view.get_untracked() != View::Chat {
                            workspace.open_session(session_id);
                        } else if current() && workspace.sidebar_open.get_untracked() && !workspace.new_session.get_untracked() {
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
                    <span
                        class=format!("shrink-0 {NOTE}")
                        data-session-time
                        title=move || format!("last event {}", relative_time(now.get(), last_activity))
                    >
                        {move || relative_time(now.get(), listed_at)}
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
                {row_menu::row_menu(menu, session_id, session_id.to_string(), menu_label.clone(), ROW_ACTION, menu_items(false))}
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
        {row_menu::context_menu(actions.context, session_id, context_label.clone(), move || all_items(false))}
        <Show when=move || revealed.get() == Some(session_id) && confirming.get() != Some(session_id)>
            // Every action at once, two to a line, then Cancel.
            <div class="grid grid-cols-2 gap-1.5 px-2 pb-1" data-delete-actions>
                {move || {
                    all_items(true)
                        .into_iter()
                        .map(|item| {
                            let act = item.act;
                            view! {
                                <button
                                    type="button"
                                    class=format!("{REVEALED} {}", if item.danger { "text-bad" } else { "" })
                                    data-reveal-item=item.id
                                    on:click=move |_| act.run(())
                                >
                                    <Glyph
                                        icon=item.icon
                                        class=if item.danger { "shrink-0 text-bad" } else { "shrink-0 text-muted" }
                                    />
                                    <span class="leading-tight">{item.label}</span>
                                </button>
                            }
                        })
                        .collect_view()
                }}
                <button
                    type="button"
                    class=REVEALED
                    data-actions-cancel
                    on:click=move |_| revealed.set(None)
                >
                    <Glyph icon=icondata::LuX class="shrink-0 text-muted" />
                    <span class="leading-tight">"Cancel"</span>
                </button>
            </div>
        </Show>
        <Show when=move || confirming.get() == Some(session_id)>
            <div class="mx-2 mb-1 flex flex-col gap-2 rounded-xl p-2 text-sm bg-raised shadow-[0_0_0_1px_var(--color-accent),0_0_0_4px_var(--color-accent-soft)]" role="alertdialog" data-delete-confirm-box>
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
            <Show when=move || !workspace.is_guest()>
                <SidebarButton workspace=workspace />
            </Show>
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
                        <h1 class="flex min-w-0 text-[0.9375rem] font-semibold tracking-tight" id="session-title" title=title>
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
            <shares::LinkBadge workspace=workspace />
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
    parent: Option<&cookie_agent_protocol::SessionMeta>,
    listing: Option<&cookie_agent_protocol::RuntimesListResult>,
) -> Delegate {
    // Its machines, unless it stays on the only one its parent is on.
    let names = cookie_agent_client_state::badges::session_machines;
    let machines = names(child, listing);
    let own = parent
        .map(|parent| names(parent, listing))
        .unwrap_or_default();
    let elsewhere = own.len() > 1 || machines.iter().any(|machine| !own.contains(machine));
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
        machines: if elsewhere { machines } else { Vec::new() },
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
    // while the session runs. An ended background command's row settles for
    // a while with no run to draw it, and draws once more when that ends.
    let now = RwSignal::new(jiff::Timestamp::now());
    let settling = StoredValue::new(false);
    let ticker = set_interval_with_handle(
        move || {
            let reduced = workspace.transcript.get_untracked()
                == cookie_agent_client_state::transcript_mode::TranscriptMode::Reduced;
            let settles = workspace.selected.get_untracked().is_some_and(|session| {
                untrack(|| {
                    workspace.with_store(|store| {
                        store.sessions.get(&session).is_some_and(|state| {
                            state
                                .background_settles_at(jiff::Timestamp::now())
                                .is_some()
                        })
                    })
                })
            });
            let settled = settling.get_value() && !settles;
            settling.set_value(settles);
            if (reduced && workspace.viewed.with_untracked(|viewed| viewed.running))
                || settles
                || settled
            {
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
                            let parent = tree
                                .as_ref()
                                .and_then(|tree| path(tree, session_id).pop().cloned());
                            let listing = workspace.runtimes.get();
                            let machines = workspace.runtime_machines.get();
                            untrack(|| {
                                workspace.with_store(|store| {
                                    let state = store.sessions.get(&session_id)?;
                                    // What each running subagent does, from its
                                    // own projection (the page follows a tree's
                                    // live sessions).
                                    let delegates = delegates
                                        .into_iter()
                                        .map(|(call, child)| {
                                            let row = delegate(
                                                store,
                                                child,
                                                parent.as_ref(),
                                                listing.as_ref(),
                                            );
                                            (call, row)
                                        })
                                        .collect::<HashMap<_, _>>();
                                    let labels =
                                        cookie_agent_client_state::call_machines::MachineLabels::of_session(
                                            state,
                                            machines.as_ref(),
                                        );
                                    let view = TranscriptView {
                                        session_id,
                                        state,
                                        names: runtime.model_names(),
                                        minimum_level: level,
                                        expanded,
                                        delegates: &delegates,
                                        homes,
                                        machines: &labels,
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
    let history = StoredValue::new(HistoryScroll::default());
    // Whether the view follows the end, for the jump-to-latest button.
    let following = RwSignal::new(true);
    // The column's width the work rows' machines were last fitted to.
    let fitted_width = StoredValue::new(0);
    let metrics = move || {
        container.try_get_untracked().flatten().map(|element| {
            let element: web_sys::Element = element.into();
            let (height, view) = (element.scroll_height(), element.client_height());
            // The new session's cards read from their top; a transcript's
            // position counts from its end.
            let from_end = if workspace.new_session.get_untracked() {
                height - view - element.scroll_top()
            } else {
                -element.scroll_top()
            };
            let at = Metrics {
                from_end,
                height,
                view,
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
        // A new width fits the work rows' machines again; a drawing fits
        // the rows it changed (`fill_row`).
        if let Some(column) = content.try_get_untracked().flatten() {
            let column: web_sys::Element = column.into();
            let width = column.client_width();
            if fitted_width.try_get_value() != Some(width) {
                fitted_width.set_value(width);
                crate::badges::fit_work_machines(&column);
            }
        }
        history.update_value(|history| history.resized(at));
        if follow.try_update_value(|follow| follow.pin(at)) == Some(true) {
            element.set_scroll_top(if workspace.new_session.get_untracked() {
                at.height
            } else {
                0
            });
        }
        draw_pin();
        mirror();
    };
    // A session opens at its end.
    Effect::new(move |_| {
        workspace.selected.track();
        workspace.new_session.track();
        follow.set_value(Follow::default());
        history.set_value(HistoryScroll::default());
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
    // Code and media fill in as their rows near the view.
    let watching = StoredValue::new_local(None::<web_sys::Element>);
    Effect::new(move |_| {
        let Some(scroller) = container.get() else {
            return;
        };
        let scroller: web_sys::Element = scroller.into();
        if watching.with_value(|watching| watching.as_ref() == Some(&scroller)) {
            return;
        }
        crate::near_view::start(&scroller, move |row| fill_row(row, workspace));
        watching.set_value(Some(scroller));
    });
    on_cleanup(move || {
        if let Some(scroller) = watching.try_update_value(Option::take).flatten() {
            crate::near_view::stop(&scroller);
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
            if let Some((element, at)) = metrics()
                && history
                    .try_update_value(|history| history.read_on(at, reading_history(workspace)))
                    == Some(true)
            {
                if load_history_near(&element, workspace, history) {
                    history.update_value(HistoryScroll::counted);
                } else {
                    // The store takes a page before its rows draw: these
                    // may still be the ones before it, whose row of history
                    // names what was read. Their next drawing asks again.
                    history.update_value(|history| history.started(at.height));
                }
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
                if let Some(changed) = document.get_element_by_id(&id)
                    && !crate::near_view::watch(&changed)
                {
                    fill_row(&changed, workspace);
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
                    history.update_value(HistoryScroll::prompted);
                    match metrics() {
                        Some((scroller, _)) => {
                            load_history(&scroller, before, workspace, history);
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
            // The icons under a message or a reply.
            action
                if matches!(
                    action.split_once('-'),
                    Some(("message" | "reply", "copy" | "revert" | "fork"))
                ) =>
            {
                let (kind, name) = action.split_once('-').unwrap_or_default();
                let anchor = crate::workspace::MenuAnchor::of(&target);
                let Some(action) = crate::workspace::PointAction::parse(name) else {
                    return;
                };
                if kind == "message" {
                    if let Some(seq) = target
                        .get_attribute("data-seq")
                        .and_then(|seq| seq.parse().ok())
                    {
                        workspace.message_action(seq, action, anchor);
                    }
                } else if let Some(item) = target
                    .get_attribute("data-item")
                    .and_then(|item| item.parse().ok())
                {
                    workspace.reply_action(item, action, anchor);
                }
            }
            "compaction-revert" => {
                if let Some(seq) = target
                    .get_attribute("data-seq")
                    .and_then(|seq| seq.parse().ok())
                {
                    workspace
                        .open_compaction_revert(seq, crate::workspace::MenuAnchor::of(&target));
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
                        asset: None,
                    }));
                }
            }
            _ => {}
        }
    };
    // The row the pointer is over, whose icons show.
    let marked_row = StoredValue::new_local(None::<web_sys::Element>);
    let message_press = MessagePress {
        workspace,
        press: StoredValue::new(Press::default()),
        timer: StoredValue::new(None),
        on: StoredValue::new(None),
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
        // At the very top no scroll follows to ask for older rows.
        if let Some((element, _)) = metrics() {
            history.update_value(HistoryScroll::prompted);
            load_history_near(&element, workspace, history);
        }
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
            // Reversed, so that the position counts from the end and rows
            // arriving above the view move nothing, even at its very top,
            // where a browser anchors no row. Its one child keeps the DOM's
            // order for selection, tabbing, and find in page, and sits at
            // the top when it is short. The new session's cards read from
            // their top instead.
            class="flex min-h-0 flex-1 overflow-y-auto px-3 py-2 sm:px-6"
            class:flex-col-reverse=move || !workspace.new_session.get()
            class:flex-col=move || workspace.new_session.get()
            // A session another client owns offers no revert or fork.
            data-read-only=move || (!workspace.writable()).then_some("")
            data-guest=move || workspace.is_guest().then_some("")
            on:click=on_click
            on:scroll=move |_| {
                if workspace.new_session.get_untracked() {
                    workspace.draft_at_skills.set(crate::browser::scrolled_to(
                        "#transcript",
                        "#new-session-skills",
                    ));
                }
                if let Some((element, at)) = metrics() {
                    follow.update_value(|follow| follow.scrolled(at));
                    shown();
                    if history.try_update_value(|history| history.scrolled(at)) == Some(true) {
                        load_history_near(&element, workspace, history);
                    }
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
                // A long press on a message or a reply is its menu, not the
                // browser's.
                if message_press.held() {
                    event.prevent_default();
                    return;
                }
                // A mouse's right-click on one of your messages opens its
                // menu too, outside its text's selection.
                if crate::browser::from_touch(&event) || !row_menu::wants_context_menu(&event) {
                    return;
                }
                let seq = event
                    .target()
                    .and_then(|target| target.dyn_into::<web_sys::Element>().ok())
                    .and_then(|target| target.closest(".user-message[data-seq]").ok().flatten())
                    .and_then(|message| message.get_attribute("data-seq"))
                    .and_then(|seq| seq.parse().ok());
                if let Some(seq) = seq {
                    event.prevent_default();
                    workspace.open_user_menu(seq);
                }
            }
        >
            <div class="mb-auto w-full shrink-0">
                <Show when=move || workspace.new_session.get()>
                    <cards::NewSessionCards workspace=workspace />
                </Show>
                // Over the top of the transcript, an open work row the view
                // scrolled past while its work is still in view; the row it
                // sits in takes no room.
                <div class="pointer-events-none sticky top-0 z-10 mx-auto h-0 max-w-[45rem]" data-work-pin>
                    <div
                        node_ref=pinned_work
                        class="pointer-events-auto overflow-hidden rounded-md shadow-md"
                        data-work-pinned
                        hidden
                    ></div>
                </div>
                <div node_ref=content class="mx-auto flex max-w-[45rem] flex-col gap-3">
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
                <div class="pointer-events-none sticky bottom-0 mx-auto h-0 max-w-[45rem]">
                    <Show when=move || {
                        !following.get() && overflows.get() && workspace.selected.get().is_some()
                            && !workspace.new_session.get()
                    }>
                        <button
                            type="button"
                            class="pointer-events-auto absolute right-0 bottom-1 inline-flex min-h-11 items-center gap-2 rounded-full px-3 text-sm shadow-md hover:bg-sunken active:bg-press bg-raised shadow-[0_0_0_1px_var(--color-line-2)]"
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
        </div>
    }
}

/// How many screens above the view a row of history not loaded starts being
/// read, so that a page mostly arrives before the user scrolls up to it.
const HISTORY_SCREENS: f64 = 1.5;

/// Reads the history the row of history not loaded nearest above the view of
/// `scroller`, the transcript, stands for, when that row is within
/// [`HISTORY_SCREENS`]: whether a read started. A row whose read failed waits
/// for a click.
fn load_history_near(
    scroller: &web_sys::Element,
    workspace: Workspace,
    history: StoredValue<HistoryScroll>,
) -> bool {
    let Ok(rows) = scroller.query_selector_all("[data-history]:not([data-history-failed])") else {
        return false;
    };
    let view = scroller.get_bounding_client_rect();
    // From the bottom: a gap inside the newest run comes before older history.
    for index in (0..rows.length()).rev() {
        let Some(row) = rows
            .item(index)
            .and_then(|row| row.dyn_into::<web_sys::Element>().ok())
        else {
            continue;
        };
        let at = row.get_bounding_client_rect();
        if at.bottom() > view.top() - HISTORY_SCREENS * view.height()
            && at.top() < view.bottom()
            && let Some(before) = row
                .get_attribute("data-history")
                .and_then(|before| before.parse().ok())
        {
            return load_history(scroller, before, workspace, history);
        }
    }
    false
}

/// Reads the history before `before`: whether a read started, which it does
/// not while another is under way.
fn load_history(
    scroller: &web_sys::Element,
    before: u64,
    workspace: Workspace,
    history: StoredValue<HistoryScroll>,
) -> bool {
    if reading_history(workspace) {
        return false;
    }
    workspace.load_history(before);
    let started = reading_history(workspace);
    if started {
        history.update_value(|history| history.started(scroller.scroll_height()));
    }
    started
}

/// Whether the open session's transcript is reading a page of history.
fn reading_history(workspace: Workspace) -> bool {
    workspace
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
        })
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
    let block = row.with_untracked(|row| match row {
        Some(RowHtml::Reply { block, .. }) => Some(*block),
        _ => None,
    });
    let Some(block) = block else {
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
    };
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
        <div
            class="transcript-item min-w-0"
            id=row_element_id(id)
            data-reply-block=block.to_string()
            node_ref=node
        >
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
        // A code block is highlighted alone, once it is added and near the view.
        if fills_in(block) {
            let nodes = element.child_nodes();
            for index in before..after {
                if let Some(node) = nodes
                    .item(index)
                    .and_then(|node| node.dyn_into::<web_sys::Element>().ok())
                    && !crate::near_view::watch(&node)
                {
                    crate::highlight::highlight_within(&node);
                }
            }
        }
        drawn.push((Arc::clone(block), after - before));
    }
}

/// Shows the icons under the message or reply a mouse or pen is over (the
/// class `pointer-over` on its row, which style/main.css follows), and
/// under no other.
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
    let row = target.closest(".transcript-item").ok().flatten();
    // Only the row marked before can show its icons: no other is looked for.
    if !marked.with_value(|marked| *marked == row) {
        unmark_message_row(marked);
    }
    // Again on the same row too: a redraw of it may have dropped the class.
    if let Some(row) = &row {
        let _ = row.class_list().add_1("pointer-over");
    }
    marked.set_value(row);
}

/// Hides the icons of the row the pointer was over.
fn unmark_message_row(marked: StoredValue<Option<web_sys::Element>, LocalStorage>) {
    if let Some(row) = marked.try_update_value(Option::take).flatten() {
        let _ = row.class_list().remove_1("pointer-over");
    }
}

/// What a finger's long press in the transcript opens the menu of.
#[derive(Clone, Copy)]
enum Pressed {
    /// A user message's bubble, by the message's sequence.
    Message(u64),
    /// A finished reply's text or footer, by the reply's last block.
    Reply(u64),
}

/// A touch held on a user message's bubble or a finished reply's text:
/// after [`LONG_PRESS`] without moving it opens the message's or the
/// reply's menu, the actions a mouse finds in the icons under them.
#[derive(Clone, Copy)]
struct MessagePress {
    workspace: Workspace,
    press: StoredValue<Press>,
    timer: StoredValue<Option<TimeoutHandle>>,
    /// What the press is on, until it ends.
    on: StoredValue<Option<Pressed>>,
}

impl MessagePress {
    fn down(self, event: &ev::PointerEvent) {
        self.cancel();
        self.press.set_value(Press::default());
        self.on.set_value(None);
        if event.pointer_type() != "touch" {
            return;
        }
        let Some(on) = event
            .target()
            .and_then(|target| target.dyn_into::<web_sys::Element>().ok())
            .and_then(|target| self.pressed(&target))
        else {
            return;
        };
        let (x, y) = (f64::from(event.client_x()), f64::from(event.client_y()));
        self.press.update_value(|press| press.start(x, y));
        self.on.set_value(Some(on));
        let timer = set_timeout_with_handle(
            move || {
                if self.press.try_update_value(Press::elapse).unwrap_or(false) {
                    match on {
                        Pressed::Message(seq) => self.workspace.open_user_menu(seq),
                        Pressed::Reply(block) => self.workspace.open_reply_menu(block),
                    }
                }
            },
            LONG_PRESS,
        )
        .ok();
        self.timer.set_value(timer);
    }

    /// What a press starting at `target` is on: a user message's bubble, or
    /// a finished reply's text or footer, but not a link, button, or player
    /// in it, which keep their own press.
    fn pressed(self, target: &web_sys::Element) -> Option<Pressed> {
        let closest = |selector: &str| target.closest(selector).ok().flatten();
        if let Some(bubble) = closest("[data-bubble]") {
            return bubble
                .closest(".user-row")
                .ok()
                .flatten()?
                .get_attribute("data-seq")?
                .parse()
                .ok()
                .map(Pressed::Message);
        }
        if closest("a, button, [data-action], video, audio").is_some() {
            return None;
        }
        let block = closest(".markdown[data-part], [data-reply-footer]")?
            .closest("[data-reply-block]")
            .ok()
            .flatten()?
            .get_attribute("data-reply-block")?
            .parse()
            .ok()?;
        self.workspace.reply_end(block).map(Pressed::Reply)
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

    /// Whether a press is on a message or reply now, held or just fired.
    fn held(self) -> bool {
        self.on.get_value().is_some()
    }

    fn stop_timer(self) {
        if let Some(timer) = self.timer.get_value() {
            timer.clear();
        }
        self.timer.set_value(None);
    }
}

/// The messages waiting for the agent, as the top section of the
/// composer's card (`#queue`): a steer its run takes at its next step,
/// then those queued behind it, each taken back into the composer by its
/// Edit, which shows with the pointer over it (and always on a touch
/// screen). One another client sent says so when it cannot be taken back.
#[component]
fn WaitingRows(workspace: Workspace) -> impl IntoView {
    let waiting = Memo::new(move |_| workspace.queue());
    view! {
        <Show when=move || !waiting.with(Vec::is_empty)>
            <div id="queue" aria-label="Messages waiting for the agent">
                <div class="flex max-h-36 flex-col gap-0.5 overflow-y-auto px-2 pt-2 pb-1.5">
                    <For
                        each=move || waiting.get()
                        key=|waiting| (waiting.seq, waiting.steer)
                        children=move |waiting| waiting_row(workspace, waiting)
                    />
                </div>
                <div class="mx-3.5 h-px bg-line" aria-hidden="true"></div>
            </div>
        </Show>
    }
}

/// One waiting message: what it is, its text, and how to take it back.
fn waiting_row(workspace: Workspace, waiting: crate::workspace::Waiting) -> impl IntoView {
    let crate::workspace::Waiting {
        seq,
        steer,
        text,
        own,
        recallable,
    } = waiting;
    let full = text.clone();
    let (icon, icon_class, tag, tag_class) = if steer {
        (
            icondata::LuCornerDownRight,
            "shrink-0 text-accent",
            "Steer",
            "text-accent bg-accent-soft shadow-[inset_0_0_0_1px_color-mix(in_srgb,var(--color-accent)_35%,transparent)]",
        )
    } else {
        (
            icondata::LuClock,
            "shrink-0 text-muted",
            "Queued",
            "text-muted bg-sunken shadow-[inset_0_0_0_1px_var(--color-line-2)]",
        )
    };
    let take_back = if recallable {
        view! {
            <button
                type="button"
                class="hit-area inline-flex h-7 shrink-0 items-center gap-1 rounded-md px-2 text-xs text-muted opacity-0 transition-opacity hover:bg-hover hover:text-fg focus-visible:opacity-100 group-hover/waiting:opacity-100 [@media(hover:none)]:opacity-100"
                title=if own {
                    "Take back into the composer"
                } else {
                    "Take back into your composer (another client sent it)"
                }
                data-recall=seq
                on:click=move |_| workspace.recall(seq)
            >
                <Glyph icon=icondata::LuUndo2 />
                "Edit"
            </button>
        }
        .into_any()
    } else {
        view! {
            <span class="inline-flex shrink-0 items-center gap-1 pr-2 text-xs text-faint" title="Another client sent it">
                <Glyph icon=icondata::LuMonitorSmartphone />
                <span class="hidden sm:inline">"other client"</span>
            </span>
        }
        .into_any()
    };
    view! {
        <div
            class="group/waiting rise-in flex min-h-8 items-center gap-2 rounded-lg pr-1 pl-2 text-[0.8125rem] text-fg-2 transition-colors hover:bg-hover"
            data-waiting=if steer { "steer" } else { "queued" }
        >
            <Glyph icon=icon class=icon_class />
            <span class=format!("shrink-0 rounded-full px-1.5 text-[0.6875rem] leading-[1.125rem] font-semibold {tag_class}")>{tag}</span>
            <span class="min-w-0 flex-1 truncate" title=full>{text}</span>
            {take_back}
        </div>
    }
}

/// A round button inside the composer: 36 px to see, 44 px to hit.
const COMPOSER_BUTTON: &str = "hit-area inline-flex h-9 w-9 shrink-0 items-center justify-center rounded-full disabled:cursor-not-allowed disabled:opacity-40";
/// The composer's main action: Send, Steer, or Stop.
const COMPOSER_PRIMARY: &str =
    "bg-accent text-on-accent hover:bg-accent active:bg-accent-press disabled:hover:bg-accent";
/// Its second action: Queue.
const COMPOSER_SECONDARY: &str = "bg-surface-2 text-fg hover:border-accent hover:bg-sunken active:bg-press bg-raised shadow-[0_0_0_1px_var(--color-line-2)]";

/// The composer's text sizes: a line, and the padding above and below,
/// which keep one line a finger's 44 px tall.
const COMPOSER_LINE: f64 = 24.0;
const COMPOSER_PADDING: f64 = 20.0;

/// Sizes the composer to its text, from one line to five, scrolling past
/// that.
fn fit_composer(textarea: &web_sys::HtmlTextAreaElement) {
    let style = web_sys::HtmlElement::style(textarea);
    let _ = style.set_property("height", "auto");
    let (height, scrolls) = crate::keys::composer_height(
        f64::from(textarea.scroll_height()),
        COMPOSER_LINE,
        COMPOSER_PADDING,
    );
    let _ = style.set_property("height", &format!("{height}px"));
    let _ = style.set_property("overflow-y", if scrolls { "auto" } else { "hidden" });
}

#[component]
fn Composer(workspace: Workspace, composing: RwSignal<bool>) -> impl IntoView {
    let textarea = NodeRef::<html::Textarea>::new();
    let blurred = StoredValue::new(0_u64);
    let buttons = Memo::new(move |_| {
        let has_input = workspace.composer.with(|text| !text.trim().is_empty())
            || !workspace.attachments.with(Vec::is_empty);
        composer_buttons(
            workspace.stoppable(),
            has_input,
            workspace.held_line().is_some(),
        )
    });
    // The box follows its text: typed, pasted, recalled, or sent away. On
    // the next frame the composer may be gone, with the view it was in.
    let fit = move || {
        if let Some(textarea) = textarea.try_get_untracked().flatten() {
            fit_composer(&textarea);
        }
    };
    Effect::new(move |_| {
        workspace.composer.track();
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
            // With a sheet or menu open, Escape closes that instead (the
            // sheet's own listener), never the run behind it.
            ComposerAction::Cancel if workspace.panel.get_untracked().is_some() => return,
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
        <div class="relative min-w-0 px-3 pt-1 pb-3 sm:px-6">
            // One card: the text, then a row with the attach buttons, the
            // session's chips, and the actions.
            <div
                id="composer-box"
                class="relative mx-auto flex max-w-[45rem] flex-col rounded-[1.125rem] bg-raised shadow-[0_0_0_1px_var(--color-line-2),0_10px_32px_-12px_rgb(0_0_0/0.16)] transition-shadow duration-200 focus-within:shadow-[0_0_0_1px_var(--color-accent),0_0_0_4px_var(--color-accent-soft),0_10px_32px_-12px_rgb(0_0_0/0.16)]"
                class:opacity-60=disabled
            >
                <WaitingRows workspace=workspace />
                <textarea
                    node_ref=textarea
                    id="composer"
                    rows="1"
                    // Its sides keep clear of the buttons in the box's corners,
                    // from the first render: padding by which buttons show,
                    // not by measuring them once they have.
                    class="block w-full resize-none appearance-none border-0 bg-transparent px-4 pt-3 pb-2 text-[0.9375rem] leading-6 text-fg shadow-none outline-none placeholder:text-faint focus:outline-none"
                    placeholder="Ask cookie to…"
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
                <div class="flex min-w-0 items-center gap-1 px-1.5 pb-1.5">
                <div class="flex shrink-0 items-center gap-0.5" data-composer-left>
                    <AttachButtons workspace=workspace />
                </div>
                <ComposerChips workspace=workspace composing=composing />
                // Pinned to the right also while a phone hides the chips.
                <div
                    class="ml-auto flex shrink-0 items-center gap-1.5"
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
fn ComposerChips(workspace: Workspace, composing: RwSignal<bool>) -> impl IntoView {
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
        let Some(draft) = workspace.draft.get() else {
            return "model".into();
        };
        // The model's and its level's display names (D2): the variant, else
        // the model's default.
        let level = workspace
            .models()
            .into_iter()
            .find(|model| model.key == draft.model.model)
            .and_then(|model| {
                let id = draft
                    .model
                    .variant
                    .clone()
                    .or_else(|| model.default_variant.clone())?;
                model
                    .variants
                    .into_iter()
                    .find(|variant| variant.id == id)
                    .map(|variant| variant.display_name)
            });
        let name =
            workspace.with_runtime(|runtime| runtime.model_names().short_label(&draft.model.model));
        cookie_agent_client_state::effort::chip_label(&name, level.as_deref())
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
            class="flex min-w-0 flex-1 items-center gap-0.5 overflow-x-auto [scrollbar-width:none]"
            class:compact:hidden=move || composing.get()
        >
            <span class="mx-1 h-4 w-px shrink-0 bg-line-2" aria-hidden="true"></span>
            <button
                type="button"
                id="agent-chip"
                class=CHIP
                disabled=move || workspace.is_guest()
                aria-label=move || format!("Agent: {}", agent())
                title=move || if workspace.is_guest() {
                    format!("Agent: {}. Only the owner can change this.", agent())
                } else {
                    format!("Agent: {} (click to change)", agent())
                }
                on:click=move |_| workspace.panel.set(Some(Panel::Agents))
            >
                <Glyph icon=icondata::LuBot class="text-sm text-muted" />
                <span class="min-w-0 truncate max-[23.75rem]:sr-only">{agent}</span>
                <Glyph icon=icondata::LuChevronDown class="text-[0.625rem] opacity-50 compact:hidden" />
            </button>
            <button
                type="button"
                id="model-chip"
                class=CHIP
                disabled=move || workspace.is_guest()
                aria-label=move || format!("Model: {}", model())
                title=move || if workspace.is_guest() {
                    format!("Model: {}. Only the owner can change this.", model())
                } else {
                    format!("Model: {} (click to change)", model())
                }
                on:click=move |_| workspace.panel.set(Some(Panel::Models))
            >
                <Glyph icon=icondata::LuSparkles class="text-sm text-muted" />
                <span class="min-w-0 flex-1 truncate text-left">{model}</span>
                <Glyph icon=icondata::LuChevronDown class="text-[0.625rem] opacity-50 compact:hidden" />
            </button>
            <button
                type="button"
                id="permission-mode"
                class=CHIP
                disabled=move || workspace.is_guest()
                aria-label=move || format!("Permission mode: {}", mode_label())
                aria-haspopup="dialog"
                title=move || if workspace.is_guest() {
                    format!("Permission mode: {}. Only the owner can change this.", mode_label())
                } else {
                    format!("Permission mode: {} (click to change)", mode_label())
                }
                on:click=move |_| workspace.panel.set(Some(Panel::Modes))
            >
                <Glyph icon=Signal::derive(move || icons::permission_mode(mode()).0) class="text-sm text-muted" />
                // A phone shows the shield alone.
                <span class="min-w-0 truncate compact:sr-only">{mode_label}</span>
                <Glyph icon=icondata::LuChevronDown class="text-[0.625rem] opacity-50 compact:hidden" />
            </button>
            // Where the session works.
            {move || workspace.places_label().filter(|_| !workspace.new_session.get()).map(|places| view! {
                <button
                    type="button"
                    id="places"
                    class=format!("{CHIP} compact:hidden")
                    aria-label=format!("Runtimes: {places}")
                    title=format!("Works in {places} (click for the runtimes)")
                    on:click=move |_| workspace.run_command(Command::Runtime)
                >
                    // Each runtime in its machine's colour, as on the
                    // session rows; the folder when there are none.
                    {move || {
                        let runtimes = workspace.places_runtimes();
                        if runtimes.is_empty() {
                            view! { <Glyph icon=icondata::LuFolder class="text-sm text-muted" /> }.into_any()
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
                    <span class="min-w-0 truncate">{places.clone()}</span>
                </button>
            })}
            <span class="flex-1" aria-hidden="true"></span>
            {move || cost().map(|cost| view! {
                <button
                    type="button"
                    id="cost"
                    class=format!("{CHIP} text-muted")
                    aria-label=format!("Session cost {}: usage", cost_label(cost))
                    title="Session cost: click for usage"
                    on:click=move |_| workspace.run_command(Command::Usage)
                >
                    <span class="tabular-nums compact:hidden">{cost_label(cost)}</span>
                    <span class="hidden tabular-nums compact:inline">{compact_cost(cost)}</span>
                </button>
            })}
        </div>
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
            class="status-tip pointer-events-none absolute bottom-full left-2 z-10 mb-1 line-clamp-3 max-w-[min(32rem,calc(100%-1rem))] rounded-lg bg-raised px-2.5 py-1.5 text-xs break-words text-muted shadow-menu ring-1 ring-line"
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

/// The composer chip whose picker `panel` is, which it opens at.
const fn chip_of(panel: Panel) -> Option<&'static str> {
    match panel {
        Panel::Agents => Some("agent-chip"),
        Panel::Models => Some("model-chip"),
        Panel::Modes => Some("permission-mode"),
        _ => None,
    }
}

/// The session panel's tabs (docs/site/specs/quiet-ink.md, D5): the sheets
/// about the open session, each opened through its command as the
/// Session commands sheet opens it, with what it reads.
const SESSION_TABS: [(Panel, Command, &str, icons::Icon); 7] = [
    (Panel::Usage, Command::Usage, "Usage", icondata::LuCoins),
    (
        Panel::Permissions,
        Command::Permissions,
        "Permissions",
        icondata::LuList,
    ),
    (
        Panel::Runtimes,
        Command::Runtime,
        "Runtimes",
        icondata::LuServer,
    ),
    (
        Panel::Skills,
        Command::Skills,
        "Skills",
        icondata::LuWandSparkles,
    ),
    (Panel::Goal, Command::Goal, "Goal", icondata::LuTarget),
    (
        Panel::Background,
        Command::Background,
        "Background",
        icondata::LuHourglass,
    ),
    (
        Panel::Events,
        Command::Events,
        "Events",
        icondata::LuListFilter,
    ),
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
        .map(|(panel, command, label, icon)| {
            let current = move || workspace.panel.get() == Some(panel);
            let count = move || match panel {
                Panel::Goal => goal
                    .get()
                    .filter(|(_, total)| *total > 0)
                    .map(|(done, total)| format!("{done}/{total}")),
                Panel::Background => {
                    Some(background.get()).filter(|count| *count > 0).map(|count| count.to_string())
                }
                // The groups and the skills that are off.
                Panel::Runtimes | Panel::Skills => workspace.session_tools.with(|tools| {
                    let Some(Ok(tools)) = tools else {
                        return None;
                    };
                    let off = if panel == Panel::Runtimes {
                        tools
                            .groups
                            .iter()
                            .filter(|group| group.state == cookie_agent_protocol::ToolGroupState::Off)
                            .count()
                    } else {
                        tools
                            .skills
                            .iter()
                            .filter(|skill| skill.state == cookie_agent_protocol::SkillState::Off)
                            .count()
                    };
                    (off > 0).then(|| off.to_string())
                }),
                _ => None,
            };
            view! {
                // An icon each, the label on the current tab only, so the
                // seven fit on one line (docs/site/specs/web-polish.md, B10).
                <button
                    type="button"
                    role="tab"
                    class="relative inline-flex h-11 min-w-11 shrink-0 items-center justify-center gap-1.5 px-2 text-[0.8125rem] text-muted transition-colors hover:text-fg aria-selected:font-semibold aria-selected:text-fg lg:h-9 lg:min-w-9"
                    aria-selected=move || current().to_string()
                    aria-label=label
                    title=label
                    data-session-tab=label.to_lowercase()
                    on:click=move |_| workspace.run_command(command)
                >
                    <Glyph icon=icon class="text-sm" />
                    <Show when=current>
                        <span aria-hidden="true">{label}</span>
                    </Show>
                    {move || count().map(|count| view! {
                        <span class="rounded-full bg-sunken px-1.5 text-[0.6875rem] leading-4 font-normal text-muted tabular-nums">{count}</span>
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
        <div role="tablist" aria-label="Session" class="flex overflow-x-auto px-2 [scrollbar-width:none]" data-session-tabs>
            {tabs}
        </div>
    }
}

#[component]
fn Sheet(workspace: Workspace) -> impl IntoView {
    let close = move |_| workspace.close_panel();
    let open_panel = Memo::new(move |_| workspace.panel.get());
    // Escape closes the sheet, as × does (docs/site/specs/web-polish.md,
    // B1), unless a field in it already took the key (a rename field
    // cancels its edit) or a smaller overlay is open over it (a row's
    // menu, which closes itself first). A chip's menu gives the focus back
    // to its chip.
    let escape = window_event_listener(ev::keydown, move |event| {
        if event.key() != "Escape" || event.default_prevented() {
            return;
        }
        let Some(panel) = workspace.panel.get_untracked() else {
            return;
        };
        if workspace.transient_open() {
            return;
        }
        event.prevent_default();
        workspace.close_panel();
        if let Some(chip) = chip_of(panel)
            .and_then(|id| document().get_element_by_id(id))
            .and_then(|chip| chip.dyn_into::<web_sys::HtmlElement>().ok())
        {
            let _ = chip.focus();
        }
    });
    let escape = StoredValue::new_local(Some(escape));
    on_cleanup(move || {
        if let Some(escape) = escape.try_update_value(Option::take).flatten() {
            escape.remove();
        }
    });
    let session_tab = move || {
        workspace
            .panel
            .get()
            .is_some_and(|panel| SESSION_TABS.iter().any(|(tab, _, _, _)| *tab == panel))
    };
    // The project editor fills a phone's screen.
    let tall = move || workspace.panel.get() == Some(Panel::Project);
    // A chip's picker opens at its chip on a wider window, as a menu over
    // the composer, instead of a sheet: where, as CSS (D7).
    let anchor = RwSignal::new(None::<String>);
    Effect::new(move |_| {
        // A revert asked from an icon under a message opens at the icon,
        // below it when there is room, else above it.
        let icon = (workspace.panel.get() == Some(Panel::Message) && !workspace.narrow.get())
            .then(|| {
                workspace
                    .user_menu
                    .with(|menu| menu.as_ref().and_then(|menu| menu.anchor))
            })
            .flatten();
        if let Some(icon) = icon {
            let (width, height) = window()
                .inner_width()
                .ok()
                .zip(window().inner_height().ok())
                .and_then(|(width, height)| width.as_f64().zip(height.as_f64()))
                .unwrap_or((0.0, 0.0));
            // The sheet is 22rem wide: it starts at the icon (a reply's, at
            // the left), or where there is no room for that, ends at it (a
            // message's, at the right), inside the window.
            let sheet = 352.0;
            let left = if icon.left - 6.0 + sheet <= width - 12.0 {
                icon.left - 6.0
            } else {
                icon.right + 6.0 - sheet
            }
            .min(width - sheet - 12.0)
            .max(12.0);
            anchor.set(Some(if height - icon.bottom >= 220.0 {
                format!(
                    "left: {left:.0}px; top: {:.0}px; max-height: {:.0}px",
                    icon.bottom + 6.0,
                    height - icon.bottom - 24.0,
                )
            } else {
                format!(
                    "left: {left:.0}px; bottom: {:.0}px; max-height: {:.0}px",
                    height - icon.top + 6.0,
                    (icon.top - 24.0).max(160.0),
                )
            }));
            return;
        }
        // The ⋮ menu opens under the ⋮, at the window's right
        // (docs/site/specs/web-polish.md, P4).
        if workspace.panel.get() == Some(Panel::Commands) && !workspace.narrow.get() {
            let at = document()
                .query_selector("main button[aria-label=Commands]")
                .ok()
                .flatten()
                .map(|button| {
                    let frame = button.get_bounding_client_rect();
                    let (width, height) = window()
                        .inner_width()
                        .ok()
                        .zip(window().inner_height().ok())
                        .and_then(|(width, height)| width.as_f64().zip(height.as_f64()))
                        .unwrap_or((0.0, 0.0));
                    format!(
                        "right: {:.0}px; top: {:.0}px; max-height: {:.0}px",
                        (width - frame.right()).max(12.0),
                        frame.bottom() + 6.0,
                        (height - frame.bottom() - 24.0).max(200.0),
                    )
                });
            anchor.set(at);
            return;
        }
        let at = workspace
            .panel
            .get()
            .and_then(chip_of)
            .filter(|_| !workspace.narrow.get())
            .and_then(|id| document().get_element_by_id(id))
            .map(|chip| {
                let frame = chip.get_bounding_client_rect();
                let height = window()
                    .inner_height()
                    .ok()
                    .and_then(|height| height.as_f64())
                    .unwrap_or(0.0);
                format!(
                    "left: max(0.75rem, {:.0}px); bottom: {:.0}px; max-height: {:.0}px",
                    frame.left() - 8.0,
                    height - frame.top() + 8.0,
                    (frame.top() - 24.0).max(160.0),
                )
            });
        anchor.set(at);
    });
    let anchored = move || anchor.with(Option::is_some);
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
        Some(Panel::Message) => {
            workspace
                .user_menu
                .with(|menu| match menu.as_ref().map(|menu| &menu.point) {
                    Some(cookie_agent_client_state::cut_points::CutPoint::ReplyEnd { .. }) => {
                        "Reply"
                    }
                    Some(cookie_agent_client_state::cut_points::CutPoint::Compaction {
                        ..
                    }) => "Compaction",
                    _ => "Message",
                })
        }
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
            <div
                class="scrim-in fixed inset-0 z-10 bg-scrim sm:bg-scrim/40"
                class:sm:bg-transparent=anchored
                on:click=close
            ></div>
            // A phone's sheet rises from the bottom of what the keyboard
            // leaves; a wider window's stands on the right.
            <div class=move || {
                if anchored() {
                    "pointer-events-none fixed inset-0 z-20".to_owned()
                } else {
                    format!(
                        "sheet-frame pointer-events-none fixed inset-x-0 top-0 z-20 flex flex-col justify-end sm:inset-y-0 sm:right-0 sm:left-auto sm:w-[28rem] sm:pt-0 {}",
                        if tall() { "pt-0" } else { "pt-12" },
                    )
                }
            }>
                <section
                    id="sheet"
                    role="dialog"
                    aria-label=title
                    data-anchored=move || anchored().to_string()
                    style=move || anchor.get().unwrap_or_default()
                    class=move || {
                        if anchored() {
                            "menu-in pointer-events-auto fixed flex w-[22rem] flex-col overflow-y-auto rounded-xl bg-raised shadow-menu ring-1 ring-line".to_owned()
                        } else {
                            format!(
                                "sheet-in pointer-events-auto flex max-h-full flex-col overflow-y-auto border-line bg-raised shadow-sheet sm:h-full sm:rounded-none sm:border-t-0 sm:border-l {}",
                                if tall() { "h-full" } else { "rounded-t-2xl border-t" },
                            )
                        }
                    }
                >
                    <div class="sticky top-0 z-10 flex flex-col border-b border-line bg-raised" class:border-b-0=anchored>
                        <span class="mx-auto mt-1.5 h-1 w-10 rounded-full bg-line-2 sm:hidden" aria-hidden="true"></span>
                        <div class="flex items-center gap-2 py-1 pr-1 pl-4" class:pl-3=anchored class:py-0=anchored>
                            <h2 class=move || if anchored() { "flex-1 text-xs font-semibold text-muted" } else { "flex-1 text-[0.9375rem] font-semibold" }>{title}</h2>
                            <button type="button" class=ICON_BUTTON aria-label="Close" title="Close" on:click=close>
                                <Glyph icon=icondata::LuX class="text-lg" />
                            </button>
                        </div>
                        <Show when=session_tab>
                            {session_tabs(workspace)}
                        </Show>
                    </div>
                    <div class=move || if anchored() { "flex flex-col gap-1 p-1.5" } else { "flex flex-col gap-2 p-3" }>
                        {move || {
                            // Built again only for another sheet: what a
                            // builder reads to build it (the store, which
                            // changes with every event of a working session)
                            // must not build it again, or every row starts
                            // over un-hovered and its hover blinks. A sheet
                            // that shows live data reads it in a closure of
                            // its own, or through `built_on`.
                            let panel = open_panel.get();
                            untrack(|| match panel {
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
                            })
                        }}
                    </div>
                </section>
            </div>
        </Show>
    }
}

/// What `build` builds, built again only when what `key` reads comes out
/// different, not on whatever else `build` reads: a sheet over the session
/// store would otherwise be built again on each of its revisions, every
/// event of a working session, its rows starting over un-hovered each time,
/// so that their hover blinks.
pub(super) fn built_on<K, V>(
    key: impl Fn() -> K + Send + Sync + 'static,
    build: impl Fn() -> V + Send + Sync + 'static,
) -> impl IntoView
where
    K: PartialEq + Send + Sync + 'static,
    V: IntoView + 'static,
{
    let key = Memo::new(move |_| key());
    move || {
        // Read, not tracked: `track` subscribes without running the memo,
        // which then never reads, nor follows, what it is made of.
        key.with(|_| ());
        untrack(&build)
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

/// The agents to start the session with, built again as they or their
/// runtimes change.
fn agents_sheet(workspace: Workspace) -> impl IntoView {
    built_on(
        move || {
            (
                workspace.picker_agents(),
                workspace.agent_runtimes(),
                workspace.agent_host(),
            )
        },
        move || agents_sheet_rows(workspace),
    )
}

fn agents_sheet_rows(workspace: Workspace) -> impl IntoView {
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
                    {choice_line(agent.id.to_string(), chosen, "text-sm font-semibold")}
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
            {choice_line(agent.id.to_string(), chosen, "text-sm font-semibold")}
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
    built_on(
        move || (workspace.acp_options(), workspace.acp().is_some()),
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
        },
    )
}

/// The model sheet of an ACP agent (`docs/site/specs/acp-agents.md`
/// section 15, input and pickers): its model option's values as the models,
/// its effort option's as the variants, and the rest of its options a tap
/// away.
fn acp_models_sheet(workspace: Workspace) -> impl IntoView {
    built_on(
        move || workspace.acp_options(),
        move || {
            let options = workspace.acp_options();
            let model = cookie_agent_client_state::acp::model_option(&options).cloned();
            let effort = cookie_agent_client_state::acp::effort_option(&options).cloned();
            let choose =
                move |option: &cookie_agent_protocol::AcpConfigOption,
                      value: &cookie_agent_protocol::AcpConfigValue| {
                    let (option_id, value_id) = (option.id.clone(), value.value.clone());
                    let label = format!("{}: {}", option.name, value.name);
                    move |_| {
                        workspace.choose_agent_option(
                            option_id.clone(),
                            value_id.clone(),
                            label.clone(),
                        );
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
                                title=value.value.clone()
                                data-acp-model=value.value.clone()
                                on:click=choose(&option, value)
                            >
                                {choice_line(value.name.clone(), chosen, "text-sm font-semibold")}
                                {value.description.clone().map(|description| view! { <span class=NOTE>{description}</span> })}
                            </button>
                        }
                    })
                    .collect_view()
            });
            // The agent's effort as the effort control, in its order
            // (docs/site/specs/web-polish.md, section 6); a level stays
            // chosen without closing the menu.
            let efforts = effort.map(|option| {
                let levels = cookie_agent_client_state::effort::acp(&option);
                let current = Some(option.current_value.clone());
                let on_choose = Callback::new(move |id: String| {
                    let Some(value) = option.values.iter().find(|value| value.value == id) else {
                        return;
                    };
                    workspace.choose_agent_option(
                        option.id.clone(),
                        value.value.clone(),
                        format!("{}: {}", option.name, value.name),
                    );
                });
                view! {
                    <div class="px-1 pt-2 pb-1">
                        <effort::EffortControl levels current on_choose large=workspace.narrow.get_untracked() />
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
                <div class="flex flex-col gap-0.5 border-b border-line pb-2">{models}</div>
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
        },
    )
}

/// The models to start the session with, built again as they change.
fn models_sheet(workspace: Workspace) -> impl IntoView {
    built_on(
        move || (workspace.draft_is_acp(), workspace.models()),
        move || models_sheet_rows(workspace),
    )
}

fn models_sheet_rows(workspace: Workspace) -> impl IntoView {
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
    // The level the current selection runs: its variant, else its model's
    // default.
    let current_level = current.as_ref().and_then(|current| {
        current
            .variant
            .as_ref()
            .map(ToString::to_string)
            .or_else(|| {
                models
                    .iter()
                    .find(|model| model.key == current.model)
                    .and_then(|model| model.default_variant.as_ref().map(ToString::to_string))
            })
    });
    let rows = models
        .iter()
        .map(|model| {
            let key = model.key.clone();
            let chosen = current.as_ref().is_some_and(|current| current.model == key);
            // Another model keeps the level when it offers it, else takes
            // its own default (docs/site/specs/web-polish.md, section 6).
            let selection = ModelSelection {
                model: key.clone(),
                variant: cookie_agent_client_state::effort::native(model)
                    .carried(current_level.as_deref())
                    .and_then(|id| cookie_agent_protocol::VariantId::new(id).ok()),
            };
            view! {
                <button
                    type="button"
                    class=ROW
                    class:bg-sel=chosen
                    title=key.to_string()
                    data-model=key.to_string()
                    on:click=move |_| {
                        workspace.pick_model(selection.clone());
                        workspace.panel.set(None);
                    }
                >
                    {choice_line(model.display_name.to_string(), chosen, "text-sm font-semibold")}
                </button>
            }
        })
        .collect_view();
    // The current model's levels, at the foot.
    let foot = current.and_then(|current| {
        let model = models.iter().find(|model| model.key == current.model)?;
        let levels = cookie_agent_client_state::effort::native(model);
        if levels.form() == cookie_agent_client_state::effort::Form::Hidden {
            return None;
        }
        let on_choose = Callback::new(move |id: String| {
            if let Ok(variant) = cookie_agent_protocol::VariantId::new(id) {
                workspace.pick_model(ModelSelection {
                    model: current.model.clone(),
                    variant: Some(variant),
                });
            }
        });
        Some(view! {
            <div class="mt-1 border-t border-line px-1 pt-3 pb-1">
                <effort::EffortControl levels current=current_level.clone() on_choose large=workspace.narrow.get_untracked() />
            </div>
        })
    });
    view! {
        <div class="flex flex-col gap-0.5">{rows}</div>
        {foot}
    }
    .into_any()
}

/// The presets to start the session with, built again as they change.
fn presets_sheet(workspace: Workspace) -> impl IntoView {
    built_on(
        move || workspace.presets(),
        move || presets_sheet_rows(workspace),
    )
}

fn presets_sheet_rows(workspace: Workspace) -> impl IntoView {
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
            .map(|approval| approval_card(workspace, approval))
            .collect_view()
            .into_any()
    }
}

/// One approval: what the call wants, where, the reviewer's reason, a note
/// for the agent, and the answers.
fn approval_card(
    workspace: Workspace,
    approval: cookie_agent_client_state::ApprovalState,
) -> impl IntoView {
    let (title, preview, icon) = workspace.with_store(|store| {
        store
            .sessions
            .get(&approval.session_id)
            .and_then(|state| state.tools.get(&approval.tool_call_id))
            .map(|tool| {
                (
                    cookie_agent_client_state::acp::approval_title(&tool.header_text()).to_owned(),
                    display_html(&tool.presentation.preview, BlockPlace::plain),
                    icons::hint_icon(tool.presentation.hint.as_ref().map(|hint| hint.rendered())),
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
                let class =
                    if option.kind == cookie_agent_protocol::ApprovalAgentOptionKind::AllowOnce {
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
    let machine = approval_machine(workspace, &approval).map(|label| {
            view! {
                <span class="inline-flex shrink-0 items-center gap-1.5 rounded-full bg-sunken px-2 py-0.5 text-xs font-normal" title=label.name.clone() data-approval-machine>
                    <span class=format!("inline-block size-2 shrink-0 rounded-full {}", crate::badges::dot_class(label.hue)) aria-hidden="true"></span>
                    {label.name.clone()}
                </span>
            }
        });
    let source = approval_source(workspace, &approval).map(|source| {
            view! {
                <span class="inline-flex shrink-0 items-center rounded-full bg-sunken px-2 py-0.5 text-xs font-normal" data-approval-source>
                    {source}
                </span>
            }
        });
    view! {
        <div class="approval rise-in flex flex-col gap-2.5 rounded-[1.125rem] bg-raised p-4 shadow-[0_0_0_1px_color-mix(in_srgb,var(--color-warn)_45%,transparent),0_0_0_4px_var(--color-warn-soft),0_14px_40px_-12px_rgb(0_0_0/0.2)]">
            <p class="flex items-center gap-2.5 text-sm font-semibold">
                <span class="inline-flex h-7 w-7 shrink-0 items-center justify-center rounded-lg bg-warn-soft text-warn">
                    <Glyph icon=icon />
                </span>
                <span class="min-w-0 font-mono text-[0.8125rem] break-words">{title}</span>
                {machine}
                {source}
            </p>
            <div class="min-w-0" inner_html=preview></div>
            {reviewer}
            <input
                type="text"
                class="h-9 w-full min-w-0 rounded-lg bg-surface px-3 text-[0.8125rem] shadow-[inset_0_0_0_1px_var(--color-line-2)] placeholder:text-faint focus:shadow-[inset_0_0_0_1px_var(--color-accent),0_0_0_3px_var(--color-accent-soft)] focus:outline-none"
                data-approval-note
                placeholder="Note for the agent, sent with a rejection"
                prop:value=move || note.get()
                on:input=move |event| note.set(event_target_value(&event))
            />
            <div class="flex flex-wrap gap-2">{buttons}</div>
        </div>
    }
}

/// The approval that takes the composer's place while it waits
/// (docs/site/specs/quiet-ink.md, D8): the oldest of the session's, and
/// how many more wait behind it, which the approvals sheet lists.
#[component]
fn ApprovalDock(workspace: Workspace) -> impl IntoView {
    view! {
        <div class="px-3 pt-1 pb-3 sm:px-6" id="approval-dock">
            <div class="mx-auto flex max-w-[45rem] flex-col gap-1.5">
                {move || {
                    let waiting = workspace.approvals();
                    let more = waiting.len().saturating_sub(1);
                    let first = waiting.into_iter().next();
                    view! {
                        {first.map(|approval| approval_card(workspace, approval))}
                        {(more > 0).then(|| view! {
                            <button
                                type="button"
                                class="self-start rounded-md px-2 py-1 text-xs text-muted hover:bg-hover hover:text-fg"
                                data-approvals-more
                                on:click=move |_| workspace.panel.set(Some(Panel::Approvals))
                            >
                                {format!("{more} more waiting")}
                            </button>
                        })}
                    }
                }}
            </div>
        </div>
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

/// The machine an approval's call will run on, while its session labels them.
fn approval_machine(
    workspace: Workspace,
    approval: &cookie_agent_client_state::ApprovalState,
) -> Option<cookie_agent_client_state::call_machines::MachineLabel> {
    let machines = workspace.runtime_machines.get();
    workspace.with_store(|store| {
        let state = store.sessions.get(&approval.session_id)?;
        let tool = state.tools.get(&approval.tool_call_id)?;
        cookie_agent_client_state::call_machines::MachineLabels::of_session(
            state,
            machines.as_ref(),
        )
        .call(tool)
        .cloned()
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
            // Three tiles lead: the context, the cost, and the cache hits
            // (docs/site/specs/web-polish.md, P4).
            let (cost, requests, hit) = match &usage.session {
                Ok(rollup) => (
                    rollup
                        .estimated_cost_usd
                        .map_or_else(|| "unpriced".to_owned(), |cost| format!("${cost:.2}")),
                    format!("{} requests", format_count(rollup.request_count)),
                    CacheHit::of(rollup).label(),
                ),
                Err(_) => ("—".to_owned(), String::new(), "—".to_owned()),
            };
            let tile = |id: &'static str, name: &'static str, figure: String, note: String| {
                view! {
                    <div class="flex min-w-0 flex-col rounded-xl bg-sunken px-3 py-2" id=id>
                        <span class="text-[0.6875rem] text-muted">{name}</span>
                        <span class="truncate text-lg leading-6 font-semibold tabular-nums" data-figure>{figure}</span>
                        <span class="truncate text-[0.6875rem] text-faint">{note}</span>
                    </div>
                }
            };
            view! {
                <div class="grid grid-cols-3 gap-2" data-usage-tiles>
                    {tile(
                        "usage-context",
                        "Context",
                        context.map_or_else(|| "—".to_owned(), format_count),
                        "tokens, last request".to_owned(),
                    )}
                    {tile("usage-cost", "Cost", cost, requests)}
                    {tile("usage-cache", "Cache hits", hit, "of input tokens".to_owned())}
                </div>
                {cache_card(workspace)}
                <h3 class="text-sm font-bold">"This session"</h3>
                {session}
                {tree}
            }
            .into_any()
        }
    }
}

/// The usage tab's prompt cache card (`docs/site/specs/cache-aware-idle.md`
/// section 4.5): how much of the TTL is left, and what the badge's tooltip
/// says.
fn cache_card(workspace: Workspace) -> Option<impl IntoView + use<>> {
    let viewed = workspace.viewed.get_untracked();
    let clock = viewed.cache_clock?;
    let session = workspace.selected.get_untracked()?;
    let plan = workspace.sessions.with_untracked(|sessions| {
        sessions
            .iter()
            .find(|meta| meta.session_id == session)
            .and_then(|meta| meta.cache_compaction.clone())
    });
    let now = jiff::Timestamp::now();
    let badge = clock.badge(now, viewed.running, plan.as_ref());
    let lines = cookie_agent_client_state::cache::detail_lines(
        &clock,
        &badge,
        plan.as_ref(),
        now,
        &crate::cron::zone_at(now),
    );
    let left = match &badge {
        cookie_agent_client_state::cache::CacheBadge::Counting { left, .. } => {
            let ttl = clock.stamp.ttl_secs.max(1) as f64;
            (left.as_secs_f64() / ttl).clamp(0.0, 1.0)
        }
        _ => 1.0,
    };
    Some(view! {
        <section class="mb-3 rounded-lg border border-line p-3" id="usage-cache">
            <h3 class="mb-2 flex items-center gap-2 text-sm font-bold">
                <Glyph icon=icondata::LuFlame class="text-hot" />
                "Prompt cache"
            </h3>
            <div class="h-1.5 overflow-hidden rounded-full bg-sunken" aria-hidden="true">
                <div class="h-full rounded-full bg-hot" style=format!("width: {:.1}%", left * 100.0)></div>
            </div>
            <ul class="mt-2 flex flex-col gap-0.5 text-sm text-fg-2">
                {lines.into_iter().map(|line| view! { <li>{line}</li> }).collect_view()}
            </ul>
        </section>
    })
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
    // The agent's commands and which of cookie's they stand in for, as
    // values: read from the store, which changes with every event of a
    // working session, they would build every row again with each, and
    // the hover of the one under the pointer would blink.
    let replaced = Memo::new(move |_| {
        MENU.iter()
            .filter(|entry| workspace.agent_replaces(entry.id))
            .map(|entry| entry.id)
            .collect::<Vec<_>>()
    });
    let commands = Memo::new(move |_| workspace.agent_commands());
    // What is live behind a panel entry, at its end: the cost, the goal's
    // progress, the calls in the background.
    let live = move |command: Command| -> Option<String> {
        match command {
            Command::Usage => workspace.selected.get().and_then(|session_id| {
                workspace
                    .costs
                    .with(|costs| costs.get(&session_id).copied())
                    .map(cost_label)
            }),
            Command::Goal => workspace
                .goal()
                .filter(|goal| cookie_agent_client_state::rows::goal_bar_shows(goal.status))
                .filter(|goal| !goal.items.is_empty())
                .map(|goal| {
                    format!(
                        "{}/{}",
                        goal.items.iter().filter(|item| item.finished).count(),
                        goal.items.len()
                    )
                }),
            Command::Background => Some(workspace.background_calls().len())
                .filter(|count| *count > 0)
                .map(|count| count.to_string()),
            _ => None,
        }
    };
    let row = move |entry: &'static crate::commands::MenuEntry| {
        let command = entry.command;
        view! {
            <button
                type="button"
                class=MENU_ROW
                title=entry.description
                data-command=entry.id
                on:click=move |_| {
                    workspace.panel.set(None);
                    workspace.run_command(command);
                }
            >
                <Glyph icon=entry.icon class="w-4 shrink-0 justify-center text-muted" />
                <span class="min-w-0 flex-1 truncate">{entry.label}</span>
                {move || live(command).map(|value| view! { <span class="shrink-0 text-xs text-muted tabular-nums">{value}</span> })}
            </button>
        }
    };
    // The session's actions, then what opens in the session panel
    // (docs/site/specs/web-polish.md, P4).
    let own = move || {
        let query = query();
        let replaced = replaced.get();
        let offered: Vec<&'static crate::commands::MenuEntry> = MENU
            .iter()
            .filter(|entry| crate::commands::offered(entry.command, has_crons))
            .filter(|entry| {
                !workspace.is_guest() || crate::commands::offered_to_guest(entry.command)
            })
            .filter(|entry| !replaced.contains(&entry.id))
            .filter(|entry| matches(&query, [entry.label, entry.description]))
            .collect();
        let in_panel = |entry: &&&crate::commands::MenuEntry| {
            SESSION_TABS
                .iter()
                .any(|(_, command, _, _)| *command == entry.command)
        };
        let actions = offered
            .iter()
            .filter(|entry| !in_panel(entry))
            .map(|entry| row(entry))
            .collect_view();
        let panel: Vec<_> = offered.iter().filter(|entry| in_panel(entry)).collect();
        view! {
            <div class="flex flex-col">{actions}</div>
            {(!panel.is_empty()).then(|| view! {
                <h3 class="mt-1 border-t border-line px-2.5 pt-2 pb-0.5 text-xs font-semibold text-muted">"Open in the session panel"</h3>
                <div class="flex flex-col">{panel.into_iter().map(|entry| row(entry)).collect_view()}</div>
            })}
        }
    };
    let agent = move || {
        let query = query();
        let rows = commands.with(|commands| {
            commands
                .iter()
                .filter(|command| {
                    // A guest's agent commands are the ones that stand in for
                    // compact and the goal.
                    (!workspace.is_guest() || matches!(command.name.as_str(), "compact" | "goal"))
                        && cookie_agent_client_state::acp::command_listed(
                            &command.name,
                            query.trim_start_matches('/'),
                        )
                        && matches(
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
                                    <span class="agent-badge rounded px-1 text-[0.625rem] uppercase bg-raised shadow-[0_0_0_1px_var(--color-accent),0_0_0_4px_var(--color-accent-soft)]">"agent"</span>
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
                <h3 class="mt-1 border-t border-line px-2.5 pt-2 pb-0.5 text-xs font-semibold text-muted">"The agent's commands"</h3>
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
            <span class="agent-badge rounded px-1 text-[0.625rem] uppercase bg-raised shadow-[0_0_0_1px_var(--color-accent),0_0_0_4px_var(--color-accent-soft)]">"agent"</span>
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
/// The permission modes, or the ACP agent's own modes, built again as
/// the agent's change.
fn modes_sheet(workspace: Workspace) -> impl IntoView {
    built_on(
        move || cookie_agent_client_state::acp::mode_option(&workspace.acp_options()).cloned(),
        move || modes_sheet_rows(workspace),
    )
}

fn modes_sheet_rows(workspace: Workspace) -> impl IntoView {
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
                                {move || choice_line(choice.name.to_owned(), chosen(), "text-sm font-semibold")}
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
                        {choice_line(value.name.clone(), chosen, "text-sm font-semibold")}
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

/// Fills in the code and media of `row`, a transcript row or part of one.
fn fill_row(row: &web_sys::Element, workspace: Workspace) {
    crate::badges::fit_work_machines(row);
    crate::highlight::highlight_within(row);
    crate::media::show_within(row, workspace);
    crate::assets::show_within(row, workspace);
}
