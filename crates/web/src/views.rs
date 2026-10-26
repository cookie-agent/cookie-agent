//! The connected page: the rail and its views. Chat is the sessions
//! sidebar, the title bar, the transcript, the queue strip, the composer,
//! the bottom bar, and the session's sheets; the other views are full
//! pages ([`shell`]). Wide windows show the sheets on the right, phones
//! from the bottom.

use std::{collections::HashMap, sync::Arc, time::Duration};

use cookie_agent_client_state::{
    EventLevel, ToolStatus,
    badges::{badges_tooltip, session_badges},
    permissions::{EFFECTS, effect_label, permission_actions, permission_rows, source_label},
    usage::{CacheHit, format_count},
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
    format::{compact_cost, compact_count, relative_time},
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
mod cards;
mod cron;
mod depth;
mod install;
mod machines;
mod manage;
mod memory;
mod notifications;
mod onboarding;
mod projects;
mod questions;
mod restart;
mod row_menu;
mod shell;

pub(crate) use install::{Install, InstallAction, InstallBanner};
pub(crate) use restart::{RestartBanner, RestartDialog};

use depth::{
    AttachButtons, BackgroundBar, DraftFiles, GoalBar, background_sheet, compact_sheet, goal_sheet,
    message_sheet, permissions_sheet, skills_sheet,
};

use manage::runtimes_sheet;
use shell::{Drawer, PageView, Rail, SidebarButton};

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
const BUTTON: &str = "inline-flex min-h-11 min-w-11 items-center justify-center gap-2 rounded-md border border-crust px-3 text-sm hover:bg-toasted active:bg-crust disabled:cursor-not-allowed disabled:opacity-50 disabled:hover:bg-transparent dark:border-smoke dark:hover:bg-ember dark:active:bg-smoke";
/// The main action of a form or the composer.
const PRIMARY: &str = "inline-flex min-h-11 min-w-11 items-center justify-center gap-2 rounded-md bg-espresso px-3 text-sm font-bold text-cream hover:bg-caramel active:bg-ink disabled:cursor-not-allowed disabled:opacity-50 disabled:hover:bg-espresso dark:bg-latte dark:text-night dark:hover:bg-maple dark:active:bg-sand dark:disabled:hover:bg-latte";
/// A button that is only an icon: it has an `aria-label` and a `title`.
const ICON_BUTTON: &str = "inline-flex min-h-11 min-w-11 shrink-0 items-center justify-center rounded-md text-ink hover:bg-toasted hover:text-espresso active:bg-crust disabled:cursor-not-allowed disabled:opacity-50 disabled:hover:bg-transparent dark:text-sand dark:hover:bg-ember dark:hover:text-latte dark:active:bg-smoke";
/// A bottom bar control showing its current value: the agent, the model,
/// and the permission mode share it. 32 px to see, 44 px to hit.
const CHIP: &str = "hit-area inline-flex h-8 min-w-0 max-w-[16rem] items-center gap-2 rounded-md border border-crust bg-cream px-3 text-sm hover:border-caramel hover:bg-toasted active:bg-crust dark:border-smoke dark:bg-night dark:hover:border-maple dark:hover:bg-ember dark:active:bg-smoke";
/// A status bar indicator that does something when clicked.
const STATUS_BUTTON: &str = "inline-flex min-h-11 min-w-11 items-center justify-center gap-1.5 rounded-md px-2 text-xs hover:bg-toasted active:bg-crust dark:hover:bg-ember dark:active:bg-smoke";
/// A session row's buttons (move, rename, delete), which show only over
/// the hovered (or focused) row: side by side, centred on its two lines,
/// laid over the row's right end on the row's hover colour, which fades in
/// from their left. The row keeps its layout underneath: only the part
/// they cover is hidden.
const ROW_ACTIONS: &str = "absolute inset-y-0 right-0 items-center gap-0.5 rounded-r-md pr-0.5 pl-6 bg-[linear-gradient(to_right,transparent,var(--color-toasted)_1.5rem)] dark:bg-[linear-gradient(to_right,transparent,var(--color-ember)_1.5rem)]";
/// One of them: at rest in the row's hover colour so it blends in (the
/// open row's hover colour is the same), one palette step darker on hover
/// (lighter in dark), and further still when pressed. 44 px square.
const ROW_ACTION: &str = "inline-flex min-h-11 min-w-11 items-center justify-center rounded-md bg-toasted text-ink hover:bg-crust hover:text-espresso active:bg-[color-mix(in_oklab,var(--color-crust),var(--color-espresso)_18%)] dark:bg-ember dark:text-sand dark:hover:bg-smoke dark:hover:text-latte dark:active:bg-[color-mix(in_oklab,var(--color-smoke),var(--color-latte)_18%)]";
/// A button of a row's actions a long press revealed: narrower than
/// [`BUTTON`], so that ⋮, Rename, Delete, and ✕ share a phone's line.
const REVEALED: &str = "inline-flex min-h-11 min-w-11 items-center justify-center gap-1.5 rounded-md border border-crust px-2 text-sm hover:bg-toasted active:bg-crust dark:border-smoke dark:hover:bg-ember dark:active:bg-smoke";
/// A session's title as a field: in a session row, and in the title bar.
const TITLE_FIELD: &str = "min-h-9 w-full min-w-0 rounded-md border border-caramel bg-cream px-2 text-sm font-bold text-espresso focus:outline-none dark:border-maple dark:bg-night dark:text-latte";
/// "Use the automatic title", under an emptied title field.
const AUTO_TITLE: &str = "inline-flex min-h-9 items-center gap-1.5 rounded-md px-2 text-xs font-bold text-caramel hover:bg-crust active:bg-[color-mix(in_oklab,var(--color-crust),var(--color-espresso)_18%)] dark:text-maple dark:hover:bg-smoke";
/// A row of a list: two lines, the second a note.
const ROW: &str = "flex min-h-11 w-full shrink-0 flex-col items-start gap-0.5 rounded-md px-3 py-2 text-left hover:bg-toasted active:bg-crust dark:hover:bg-ember dark:active:bg-smoke";
const NOTE: &str = "text-xs text-ink dark:text-sand";
/// A message about something that went wrong.
const ERROR: &str = "rounded-md border border-red-700/30 bg-red-50 px-3 py-2 text-sm whitespace-pre-wrap break-words text-red-800 dark:border-red-400/30 dark:bg-red-950/40 dark:text-red-300";

/// What a sheet shows when it has nothing to list.
fn empty_state(icon: icons::Icon, text: &'static str) -> impl IntoView {
    view! {
        <div class="flex flex-col items-center gap-2 px-4 py-10 text-center text-sm text-ink dark:text-sand">
            <Glyph icon=icon class="text-2xl opacity-70" />
            <p>{text}</p>
        </div>
    }
}

/// What a sheet shows while it reads what it lists.
fn loading(text: &'static str) -> impl IntoView {
    view! {
        <p class="flex items-center gap-2 py-4 text-sm text-ink dark:text-sand">
            <Glyph icon=icondata::FaCircleNotchSolid class="animate-spin" />
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
    // The view remounts with every connection, so its listeners go with it.
    let phone = crate::browser::on_media_change(crate::browser::PHONE_QUERY, move |narrow| {
        workspace.narrow.set(narrow);
    });
    // A window that grows to a desktop's pins the sidebar instead.
    let desktop = crate::browser::on_media_change(crate::browser::DESKTOP_QUERY, move |desktop| {
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
    view! {
        <div class="flex min-h-0 flex-1">
            <div class="hidden sm:flex">
                <Rail workspace=workspace connection=connection theme=theme primary=true />
            </div>
            <div class="flex min-w-0 flex-1" class:hidden=move || !chat()>
                <aside
                    class="hidden w-72 shrink-0 flex-col border-r border-toasted bg-parchment dark:border-ember dark:bg-char"
                    class:lg:flex=move || workspace.sidebar_pinned.get()
                    id="sidebar"
                >
                    <SessionList workspace=workspace />
                </aside>
                <ChatView workspace=workspace />
            </div>
            <Show when=move || !chat()>
                <PageView workspace=workspace connection=connection />
            </Show>
            <Drawer workspace=workspace connection=connection theme=theme />
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
                <GoalBar workspace=workspace />
                <BackgroundBar workspace=workspace />
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
                <div class="flex flex-col short:flex-row short:items-end">
                    <Composer workspace=workspace composing=composing />
                    <BottomBar workspace=workspace composing=composing />
                </div>
            </main>
    }
}

/// A session's name in the list: its title, else its short ID.
fn session_label(session: &SessionMeta) -> String {
    session
        .title
        .as_ref()
        .map(ToString::to_string)
        .or_else(|| session.short_id.clone())
        .unwrap_or_else(|| session.session_id.to_string())
}

#[component]
fn SessionList(workspace: Workspace) -> impl IntoView {
    // Each runtime at each workspace sessions work in, under its home.
    let places = move || {
        let homes = workspace.homes();
        workspace.sessions.with(|sessions| {
            let roots = sessions
                .iter()
                .filter(|session| {
                    matches!(session.origin, cookie_agent_protocol::SessionOrigin::Root)
                })
                .cloned()
                .collect::<Vec<_>>();
            crate::places::filter_options(&roots, &homes)
        })
    };
    let shown = move || {
        let filter = workspace.directory_filter.get();
        let mut sessions = workspace.sessions.with(|sessions| {
            sessions
                .iter()
                .filter(|session| {
                    matches!(session.origin, cookie_agent_protocol::SessionOrigin::Root)
                        && filter
                            .as_deref()
                            .is_none_or(|filter| crate::places::matches(session, filter))
                })
                .cloned()
                .collect::<Vec<_>>()
        });
        // Running sessions first, then the rest; newest first in each.
        cookie_agent_client_state::rows::order_sessions(&mut sessions);
        sessions
    };
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
    };
    // Dragging a session onto a project's header, and the headers' own
    // buttons.
    let drag = projects::SessionDrag::new();
    drag.follow(workspace, actions);
    let headers = projects::HeaderActions::new();
    let list = drag.list;
    view! {
        <div class="flex flex-col gap-2 p-3">
            <div class="flex gap-2">
                <button type="button" class=format!("{PRIMARY} flex-1") data-new-session on:click=move |_| workspace.start_new_session()>
                    <Glyph icon=icondata::FaPlusSolid />
                    "New session"
                </button>
                <Show when=move || workspace.has_projects.get()>
                    <button
                        type="button"
                        class=BUTTON
                        id="new-project"
                        aria-label="New project…"
                        title="New project…"
                        on:click=move |_| workspace.open_project_editor(None, false)
                    >
                        <Glyph icon=icondata::FaFolderPlusSolid />
                    </button>
                </Show>
            </div>
            // A daemon without projects: the workspace filter instead of
            // project sections.
            <Show when=move || !workspace.has_projects.get()>
            <label class="flex flex-col gap-1 text-xs font-bold text-ink dark:text-sand">
                <span>"Workspace"</span>
                <select
                    id="place-filter"
                    class="min-h-11 rounded-md border border-crust bg-cream px-2 text-sm font-normal text-espresso hover:border-caramel dark:border-smoke dark:bg-night dark:text-latte dark:hover:border-maple"
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
        </div>
        <nav class="flex min-h-0 flex-1 flex-col gap-1 overflow-x-hidden overflow-y-auto px-2 pb-3" aria-label="Sessions" node_ref=list>
            {move || {
                if workspace.has_projects.get() {
                    return projects::project_list(workspace, now, actions, drag, headers).into_any();
                }
                let sessions = shown();
                if sessions.is_empty() {
                    return empty_state(icondata::FaFolderOpenSolid, "No sessions yet. Start one with New session.")
                        .into_any();
                }
                sessions
                    .into_iter()
                    .map(|session| session_row(workspace, session, now, actions, drag))
                    .collect_view()
                    .into_any()
            }}
        </nav>
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
        _ => mark.label.to_owned(),
    };
    let last_activity = session.last_activity;
    // A run session of a cron job: its clock opens the job.
    let cron = session.cron.clone();
    // Where it runs, once the runtimes are known; the tooltip lists every
    // workspace with its path.
    // Fitted to the line: machines whole, the last ones left out for a
    // `+N` when even the workspaces gone to `…` leave too little room.
    let badges = {
        let session = session.clone();
        move |line: NodeRef<html::Span>| {
            let homes = workspace.homes();
            let listed = workspace.lists_attachments.get();
            let shown = workspace
                .runtimes
                .with(|listing| session_badges(&session, listing.as_ref(), &homes, listed));
            crate::badges::fit_soon(line);
            crate::badges::badge_line(shown)
        }
    };
    let badge_line = NodeRef::<html::Span>::new();
    crate::badges::keep_fitted(badge_line);
    let title = {
        let session = session.clone();
        let status = status_text.clone();
        move || {
            let homes = workspace.homes();
            let listed = workspace.lists_attachments.get();
            let places = workspace
                .runtimes
                .with(|listing| badges_tooltip(&session, listing.as_ref(), &homes, listed));
            format!("{places}\n{status}")
        }
    };
    let label = session_label(&session);
    let confirm_label = label.clone();
    let menu_label = label.clone();
    let reveal_label = label.clone();
    let RowActions {
        hovered,
        revealed,
        confirming,
        renaming,
        menu,
        ..
    } = actions;
    let is_renaming = move || renaming.get() == Some(session_id);
    // The hover buttons stay while their ⋮ menu is open.
    let shown = move || {
        (hovered.get() == Some(session_id) || menu.get() == Some((session_id, false)))
            && !is_renaming()
    };
    // The ⋮ menu's actions; a long press's row lets go of its buttons
    // first.
    let menu_items = move |revealed_row: bool| {
        move || {
            let mut items = Vec::new();
            if workspace.has_projects.get() {
                items.push(row_menu::MenuItem {
                    id: "move",
                    label: "Move to project…",
                    icon: icondata::FaFolderOpenSolid,
                    act: Callback::new(move |()| {
                        if revealed_row {
                            revealed.set(None);
                        }
                        workspace.ask_move(session_id);
                    }),
                });
            }
            items
        }
    };
    // A mouse drags the row onto a project's header at once; a finger
    // after a long press, which let go in place reveals the buttons.
    let on_pointerdown = move |event: ev::PointerEvent| {
        if !is_renaming() {
            drag.press(workspace, session_id, &event);
        }
    };
    let lifted = move || drag.lifts(session_id);
    let field_badges = badges.clone();
    view! {
        <div
            class=move || format!(
                "session-row group relative {}",
                if lifted() {
                    "pointer-events-none z-30 rounded-md bg-cream shadow-xl ring-2 ring-caramel dark:bg-night dark:ring-maple"
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
                fallback=move || rename_row(workspace, session_id, status_mark, actions, field_badges.clone())
            >
            <button
                type="button"
                // The row looks hovered while the pointer is on its buttons
                // too, which sit over it, or the keyboard is in it.
                // One class closure for all of it: `class:` toggles beside
                // it would be dropped each time it changes.
                class=move || format!(
                    "{ROW} no-callout select-none pr-3 {} group-hover:bg-toasted group-has-focus-visible:bg-toasted dark:group-hover:bg-ember dark:group-has-focus-visible:bg-ember",
                    if current() { "bg-toasted dark:bg-ember" } else { "" },
                )
                aria-current=move || current().then_some("true")
                data-session=session_id.to_string()
                title=title.clone()
                on:pointerdown=on_pointerdown
                on:contextmenu=move |event: ev::MouseEvent| {
                    // A touch screen's long press lifts the row and offers
                    // its actions, not the browser's menu.
                    if lifted() || revealed.get_untracked() == Some(session_id) {
                        event.prevent_default();
                    }
                }
                // The open session's row switches its subagent tree between
                // the running ones and all of them.
                aria-expanded=move || tree_root().then(|| workspace.tree_expanded.get().to_string())
                on:click=move |_| {
                    if !drag.take_click() {
                        if current() && tree_root() && !workspace.new_session.get_untracked() {
                            workspace.toggle_tree();
                        } else {
                            workspace.open_session(session_id);
                        }
                    }
                }
            >
                <span class="flex w-full items-center gap-2">
                    // The status slot, only when there is a mark.
                    {status_mark.map(|mark| view! {
                        <span class="inline-flex shrink-0" data-session-activity=mark.label>
                            <Glyph icon=mark.icon class=mark.class("text-xs") />
                        </span>
                    })}
                    <span class="sr-only">{status_text.clone()}</span>
                    // Before the title: the row's buttons cover its right
                    // end on hover.
                    {cron.clone().map(|cron| cron::clock_mark(workspace, cron))}
                    <span class="min-w-0 flex-1 truncate text-sm font-bold" data-session-title>{label.clone()}</span>
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
                            on:click=move |event: ev::MouseEvent| {
                                if !current() {
                                    event.stop_propagation();
                                    workspace.toggle_tree();
                                }
                            }
                        >
                            <Glyph icon=icondata::FaChevronRightSolid class="text-[0.625rem]" />
                        </span>
                    </Show>
                    <span class="flex min-w-0 flex-1 items-center gap-1 overflow-hidden" node_ref=badge_line data-session-badges>
                        {
                            let badges = badges.clone();
                            move || badges(badge_line)
                        }
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
                    <Glyph icon=icondata::FaPenSolid />
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
                        <Glyph icon=icondata::FaTrashCanRegular />
                    </button>
                </Show>
            </div>
            </Show>
        </div>
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
                    <Glyph icon=icondata::FaPenSolid />
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
                        <Glyph icon=icondata::FaTrashCanRegular />
                        <span class="hidden @[22rem]:inline">"Delete"</span>
                    </button>
                </Show>
                // An icon, so the four fit a phone's drawer on one line.
                <button
                    type="button"
                    class=BUTTON.replace("px-3", "px-0")
                    aria-label="Cancel"
                    title="Cancel"
                    data-actions-cancel
                    on:click=move |_| revealed.set(None)
                >
                    <Glyph icon=icondata::FaXmarkSolid />
                </button>
            </div>
            </div>
        </Show>
        <Show when=move || confirming.get() == Some(session_id)>
            <div class="mx-2 mb-1 flex flex-col gap-2 rounded-lg border border-caramel bg-cream p-2 text-sm dark:border-maple dark:bg-night" role="alertdialog" data-delete-confirm-box>
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
            class="flex min-h-11 w-full flex-col items-start gap-1 rounded-md bg-toasted px-3 py-2 dark:bg-ember"
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
                    <Glyph icon=icondata::FaWandMagicSparklesSolid />
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
    let rows = move || workspace.subagent_rows();
    view! {
        <div
            role="group"
            aria-label="Subagents"
            data-subagents
            data-tree=move || if workspace.tree_expanded.get() { "expanded" } else { "collapsed" }
            class="flex flex-col gap-0.5 pb-1"
        >
            {move || {
                let expanded = workspace.tree_expanded.get();
                rows()
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
                                        icon=if row.collapsed { icondata::FaChevronRightSolid } else { icondata::FaChevronDownSolid }
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
                                    class:bg-toasted=current
                                    class:dark:bg-ember=current
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
                    <Glyph icon=icondata::FaWandMagicSparklesSolid />
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
        <div class="flex h-12 shrink-0 items-center gap-1 border-b border-toasted px-2 dark:border-ember">
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
                        <Glyph icon=icondata::FaChevronLeftSolid class="text-lg" />
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
                                class="inline-flex min-h-11 max-w-[14rem] min-w-0 items-center rounded-md px-2 text-sm text-ink hover:bg-toasted hover:text-espresso active:bg-crust dark:text-sand dark:hover:bg-ember dark:hover:text-latte dark:active:bg-smoke"
                                title=tip
                                data-crumb=session_id.to_string()
                                on:click=move |_| workspace.open_session(session_id)
                            >
                                <span class="truncate">{label}</span>
                            </button>
                            <Glyph icon=icondata::FaChevronRightSolid class="text-[0.625rem] opacity-60" />
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
                                    class="flex min-h-11 min-w-0 items-center rounded-md px-1 text-left hover:bg-toasted active:bg-crust dark:hover:bg-ember dark:active:bg-smoke"
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
                    <span class="inline-flex shrink-0 items-center gap-1 rounded-full bg-toasted px-2 py-1 text-xs dark:bg-ember" title="read-only snapshot">
                        <Glyph icon=icondata::FaLockSolid />
                        <span class="max-sm:sr-only">"read-only snapshot"</span>
                    </span>
                </Show>
            </div>
            <button
                type="button"
                class=ICON_BUTTON
                aria-label="Commands"
                title="Session commands"
                aria-haspopup="dialog"
                on:click=move |_| workspace.panel.set(Some(Panel::Commands))
            >
                <Glyph icon=icondata::FaEllipsisVerticalSolid class="text-lg" />
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
    // Everything a pass reads it borrows where it lies: a frame copies
    // neither the expanded set, the model names, nor the tree.
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
    // Mirrors `follow` into `following` once it changed, and whether the
    // rows overflow.
    let shown = move || {
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
    // Pins the view to the end when following and the end moved away.
    let pin = move || {
        let Some((element, at)) = metrics() else {
            return;
        };
        if let Some(end) = follow.try_update_value(|follow| follow.pin(at)).flatten() {
            element.set_scroll_top(end);
        }
        shown();
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
            // Code and media in the rows and parts that changed, not the
            // whole page.
            let Some(document) = web_sys::window().and_then(|window| window.document()) else {
                return;
            };
            for id in fill_in {
                if let Some(changed) = document.get_element_by_id(&id) {
                    crate::highlight::highlight_within(&changed);
                    crate::media::show_within(&changed, workspace);
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
            _ => {}
        }
    };
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
                if let Some((_, at)) = metrics() {
                    follow.update_value(|follow| follow.scrolled(at));
                    shown();
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
            on:pointerover=move |event: ev::PointerEvent| mark_message_row(&event)
            on:pointerleave=move |_| {
                if let Some(element) = container.get_untracked() {
                    unmark_message_rows(&element, None);
                }
            }
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
                <Show when=move || running(workspace) && !workspace.new_session.get()>
                    <div
                        id="working"
                        class="flex min-h-9 items-center gap-1.5 px-1 text-sm font-bold text-caramel dark:text-maple"
                        role="status"
                    >
                        <Glyph icon=icondata::FaCircleNotchSolid class="animate-spin" />
                        {move || working_label(workspace)}
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
                        class="pointer-events-auto absolute right-0 bottom-1 inline-flex min-h-11 items-center gap-2 rounded-full border border-crust bg-cream px-3 text-sm shadow-md hover:bg-toasted active:bg-crust dark:border-smoke dark:bg-night dark:hover:bg-ember dark:active:bg-smoke"
                        title="Jump to the latest and follow it"
                        data-jump-latest
                        on:click=jump
                    >
                        <Glyph icon=icondata::FaArrowDownSolid />
                        "Latest"
                    </button>
                </Show>
            </div>
        </div>
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
                Some(RowHtml::Html(html)) => html.to_string(),
                _ => String::new(),
            })
        };
        let node = NodeRef::<html::Div>::new();
        drawn_later(node);
        return view! {
            <div
                class="transcript-item min-w-0"
                id=row_element_id(id)
                node_ref=node
                inner_html=html
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
    let parts = Memo::new(move |_| {
        row.with(|row| match row {
            Some(RowHtml::Reply { parts, .. }) => Arc::clone(parts),
            _ => Arc::from([]),
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
                        view! { <ReplyPart part=slot row=id id=part /> }
                    }
                />
            </div>
        </div>
    }
    .into_any()
}

/// One part of a reply: a tool call, a thinking block, text. Text keeps the
/// nodes of its settled markdown blocks and redraws only its open tail.
#[component]
fn ReplyPart(part: ArcRwSignal<Option<PartHtml>>, row: u64, id: u64) -> impl IntoView {
    let markdown = part.with_untracked(|part| matches!(part, Some(PartHtml::Markdown(_))));
    if !markdown {
        let html = move || {
            part.with(|part| match part {
                Some(PartHtml::Html(html)) => html.to_string(),
                _ => String::new(),
            })
        };
        // A block of its own, not `contents`: the node stays while its
        // HTML changes, so it keeps the height it last showed at.
        let node = NodeRef::<html::Div>::new();
        drawn_later(node);
        return view! {
            <div
                class="min-w-0"
                data-part
                id=part_element_id(row, id)
                node_ref=node
                inner_html=html
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
    view! { <div class="markdown" data-part node_ref=node></div> }.into_any()
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
fn mark_message_row(event: &ev::PointerEvent) {
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
    if let Some(transcript) = target.closest("#transcript").ok().flatten() {
        unmark_message_rows(&transcript, row.as_ref());
    }
    if let Some(row) = row {
        let _ = row.class_list().add_1("pointer-over");
    }
}

/// Hides the ⋯ of every user message row under `within` but `keep`.
fn unmark_message_rows(within: &web_sys::Element, keep: Option<&web_sys::Element>) {
    let Ok(rows) = within.query_selector_all(".user-row.pointer-over") else {
        return;
    };
    for index in 0..rows.length() {
        if let Some(row) = rows
            .item(index)
            .and_then(|row| row.dyn_into::<web_sys::Element>().ok())
            && keep != Some(&row)
        {
            let _ = row.class_list().remove_1("pointer-over");
        }
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
            <div class="flex max-h-32 flex-col gap-1 overflow-y-auto border-t border-toasted px-3 py-1 dark:border-ember" id="queue" aria-label="Pending and queued messages">
                {move || {
                    queue
                        .get()
                        .into_iter()
                        .map(|(seq, text, own)| {
                            if own {
                                view! {
                                    <button
                                        type="button"
                                        class=format!("{ROW} font-mono text-xs")
                                        title="Take back into the composer"
                                        on:click=move |_| workspace.recall(seq)
                                    >
                                        <span class="flex w-full items-center gap-2">
                                            <Glyph icon=icondata::FaClockSolid class="text-caramel dark:text-maple" />
                                            <span class="min-w-0 flex-1 truncate">{text}</span>
                                            <Glyph icon=icondata::FaRotateLeftSolid class="opacity-60" />
                                        </span>
                                    </button>
                                }
                                    .into_any()
                            } else {
                                view! {
                                    <div class="flex items-center gap-2 px-3 font-mono text-xs text-ink dark:text-sand">
                                        <Glyph icon=icondata::FaClockSolid />
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
const COMPOSER_PRIMARY: &str = "bg-espresso text-cream hover:bg-caramel active:bg-ink disabled:hover:bg-espresso dark:bg-latte dark:text-night dark:hover:bg-maple dark:active:bg-sand dark:disabled:hover:bg-latte";
/// Its second action: Queue.
const COMPOSER_SECONDARY: &str = "border border-crust bg-parchment text-espresso hover:border-caramel hover:bg-toasted active:bg-crust dark:border-smoke dark:bg-char dark:text-latte dark:hover:border-maple dark:hover:bg-ember dark:active:bg-smoke";

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
        ComposerButtons::Send { .. } | ComposerButtons::Stop => "pr-12",
    }
}

/// [`actions_padding`] as a length, for a scrollbar's room on top of it.
fn actions_room(buttons: ComposerButtons) -> &'static str {
    match buttons {
        ComposerButtons::SteerAndQueue => "5.625rem",
        ComposerButtons::Send { .. } | ComposerButtons::Stop => "3rem",
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
        composer_buttons(workspace.stoppable(), has_input)
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
    // The session's ACP agent's commands a draft starting with `/` names:
    // choosing one that takes input puts `/<name> ` in the composer for it;
    // one that takes none is sent.
    let suggestions = Memo::new(move |_| {
        workspace
            .composer
            .with(|draft| workspace.agent_command_suggestions(draft))
    });
    let agent_commands = move || {
        let suggestions = suggestions.get();
        (!suggestions.is_empty()).then(|| {
            let rows = suggestions
                .into_iter()
                .map(|command| {
                    let name = command.name.clone();
                    let takes_input = command.input_hint.is_some();
                    let hint = command.input_hint.clone().unwrap_or_default();
                    view! {
                        <li>
                            <button
                                type="button"
                                class="flex min-h-11 w-full flex-wrap items-baseline gap-x-2 rounded px-2 py-1 text-left hover:bg-toasted focus-visible:bg-toasted dark:hover:bg-ember dark:focus-visible:bg-ember"
                                data-agent-command=name.clone()
                                on:click=move |_| {
                                    if takes_input {
                                        workspace.composer.set(format!("/{name} "));
                                        if let Some(textarea) = textarea.get_untracked() {
                                            let _ = textarea.focus();
                                        }
                                    } else {
                                        workspace.composer.set(format!("/{name}"));
                                        workspace.submit(InputDelivery::Steer);
                                    }
                                }
                            >
                                <span class="font-mono text-sm font-bold">{format!("/{}", command.name)}</span>
                                <span class=format!("font-mono {NOTE}")>{hint}</span>
                                <span class="rounded border border-caramel px-1 text-[0.625rem] uppercase dark:border-maple">"agent"</span>
                                <span class=format!("min-w-0 {NOTE}")>{command.description.clone()}</span>
                            </button>
                        </li>
                    }
                })
                .collect_view();
            view! {
                <ul
                    id="agent-commands"
                    class="mb-1 flex max-h-60 flex-col overflow-y-auto rounded-xl border border-crust bg-cream p-1 dark:border-smoke dark:bg-night"
                    aria-label="The agent's commands"
                >
                    {rows}
                </ul>
            }
        })
    };
    view! {
        <div class="relative min-w-0 px-3 pt-2 pb-1 short:flex-1 short:pb-2">
            {agent_commands}
            // One border, the box's: the text field inside has none, and
            // the buttons sit in its bottom corners.
            <div
                id="composer-box"
                class="relative rounded-xl border border-crust bg-cream hover:border-caramel focus-within:border-caramel dark:border-smoke dark:bg-night dark:hover:border-maple dark:focus-within:border-maple"
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
                        // Removing the focused composer with its view (a
                        // reconnect remounts the page) blurs it once
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
                            "send", "Send", "Enter", icondata::FaPaperPlaneSolid, true,
                            |workspace| workspace.submit(InputDelivery::Steer),
                        )
                        .into_any(),
                        ComposerButtons::Stop => button(
                            "stop", "Stop", "Esc", icondata::FaStopSolid, true,
                            Workspace::cancel,
                        )
                        .into_any(),
                        ComposerButtons::SteerAndQueue => view! {
                            {button(
                                "queue-send", "Queue", "Alt+Enter", icondata::FaClockRegular, false,
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
                                "Enter", icondata::FaPaperPlaneSolid, true,
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

/// The context chip's title: the tokens, and the window they fill when the
/// session says it (an ACP agent's).
fn context_title(tokens: u64, window: Option<u64>) -> String {
    match window {
        Some(window) => format!(
            "Context: {} of {} tokens",
            format_count(tokens),
            format_count(window)
        ),
        None => format!(
            "Context: {} tokens in the last request",
            format_count(tokens)
        ),
    }
}

/// How long a phone's status line keeps a new status message.
const STATUS_LIVE: Duration = Duration::from_secs(8);

/// The bottom bar: the session's agent, model, permission mode, and cost.
/// A wide window shows it under the composer and adds the approvals, the
/// status message, and the context size; a phone shows it above the
/// composer, on the composer's background, with the approvals and the
/// status in the status line above it, and hides it while the composer has
/// focus.
#[component]
fn BottomBar(workspace: Workspace, composing: RwSignal<bool>) -> impl IntoView {
    let cost = move || {
        workspace.selected.get().and_then(|session_id| {
            workspace
                .costs
                .with(|costs| costs.get(&session_id).copied())
        })
    };
    let context = move || {
        workspace.viewed.with(|viewed| {
            viewed
                .context_tokens
                .map(|tokens| (tokens, viewed.context_window))
        })
    };
    let agent = move || {
        workspace
            .draft
            .get()
            .map(|draft| draft.agent.to_string())
            .unwrap_or_else(|| "agent".into())
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
    let approvals = Memo::new(move |_| workspace.approvals().len());
    let mode = move || workspace.permission_mode();
    view! {
        <div
            id="status-bar"
            class="flex flex-wrap items-center gap-x-1 bg-parchment px-2 text-xs compact:-order-1 compact:flex-nowrap compact:bg-transparent short:order-none short:w-[22rem] short:shrink-0 dark:bg-char compact:dark:bg-transparent"
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
                <Glyph icon=icondata::FaRobotSolid class="text-caramel dark:text-maple" />
                <span class="min-w-0 truncate max-[23.75rem]:sr-only">{agent}</span>
                <Glyph icon=icondata::FaChevronDownSolid class="text-[0.625rem] opacity-60 compact:hidden" />
            </button>
            <button
                type="button"
                id="model-chip"
                class=format!("{CHIP} compact:max-w-none compact:flex-1")
                aria-label=move || format!("Model: {}", model())
                title=move || format!("Model: {} (click to change)", model())
                on:click=move |_| workspace.panel.set(Some(Panel::Models))
            >
                <Glyph icon=icondata::FaMicrochipSolid class="text-caramel dark:text-maple" />
                <span class="min-w-0 flex-1 truncate text-left">{model}</span>
                <Glyph icon=icondata::FaChevronDownSolid class="text-[0.625rem] opacity-60 compact:hidden" />
            </button>
            <button
                type="button"
                id="permission-mode"
                class=CHIP.replace("min-w-0", "min-w-11 justify-center")
                aria-label=move || format!("Permission mode: {}", mode_name(mode()))
                aria-haspopup="dialog"
                title=move || format!("Permission mode: {} (click to change)", mode_name(mode()))
                on:click=move |_| workspace.panel.set(Some(Panel::Modes))
            >
                <Glyph icon=Signal::derive(move || icons::permission_mode(mode()).0) class="text-caramel dark:text-maple" />
                // A phone shows the shield alone.
                <span class="min-w-0 truncate compact:sr-only">{move || mode_name(mode())}</span>
                <Glyph icon=icondata::FaChevronDownSolid class="text-[0.625rem] opacity-60 compact:hidden" />
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
                            view! { <Glyph icon=icondata::FaFolderOpenSolid class="text-caramel dark:text-maple" /> }.into_any()
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
            <Show when=move || { approvals.get() > 0 }>
                <button
                    type="button"
                    id="approvals-badge"
                    class="inline-flex min-h-11 min-w-11 items-center justify-center gap-1.5 rounded-full bg-caramel px-3 font-bold text-cream hover:bg-espresso active:bg-ink compact:hidden dark:bg-maple dark:text-night dark:hover:bg-latte dark:active:bg-sand"
                    title="Approvals waiting: click to answer"
                    on:click=move |_| workspace.panel.set(Some(Panel::Approvals))
                >
                    <Glyph icon=icondata::FaBellSolid />
                    {move || approvals_label(approvals.get())}
                </button>
            </Show>
            <span
                class="min-w-[4rem] flex-1 truncate px-2 text-ink compact:hidden dark:text-sand"
                id="status"
                role="status"
                title=move || workspace.status.get()
            >
                {move || workspace.status.get()}
            </span>
            {move || context().map(|(tokens, window)| view! {
                <span
                    id="context"
                    class="inline-flex min-h-11 items-center gap-1.5 px-2 text-ink compact:hidden dark:text-sand"
                    title=context_title(tokens, window)
                >
                    <Glyph icon=icondata::FaGaugeSimpleHighSolid />
                    <span class="font-mono">{format!("{} tokens", compact_count(tokens))}</span>
                </span>
            })}
            {move || cost().map(|cost| view! {
                <button
                    type="button"
                    id="cost"
                    class=STATUS_BUTTON
                    aria-label=format!("Session cost {}: usage", cost_label(cost))
                    title="Session cost: click for usage"
                    on:click=move |_| workspace.run_command(Command::Usage)
                >
                    <Glyph icon=icondata::FaCoinsSolid class="text-caramel dark:text-maple compact:hidden" />
                    <span class="font-mono compact:hidden">{cost_label(cost)}</span>
                    <span class="hidden font-mono compact:inline">{compact_cost(cost)}</span>
                </button>
            })}
        </div>
    }
}

/// A phone's line above the composer: the approvals waiting and a new
/// status message for a few seconds; it shows only while one of them does.
/// Wider windows have these in the bottom bar. The working spinner is the
/// transcript's last row.
#[component]
fn StatusLine(workspace: Workspace) -> impl IntoView {
    let live = RwSignal::new(false);
    let generation = StoredValue::new(0_u64);
    Effect::new(move |previous: Option<String>| {
        let status = workspace.status.get();
        if previous.is_some_and(|previous| previous != status) && !status.is_empty() {
            generation.update_value(|generation| *generation += 1);
            let current = generation.get_value();
            live.set(true);
            // The line may be gone by then, `generation` with it.
            set_timeout(
                move || {
                    if generation.try_get_value() == Some(current) {
                        live.set(false);
                    }
                },
                STATUS_LIVE,
            );
        }
        status
    });
    let approvals = Memo::new(move |_| workspace.approvals().len());
    let shown = move || approvals.get() > 0 || live.get();
    view! {
        <Show when=shown>
            <div
                id="status-line"
                class="hidden min-h-11 items-center gap-1 border-t border-toasted px-2 text-xs compact:flex dark:border-ember"
            >
                <Show when=move || { approvals.get() > 0 }>
                    <button
                        type="button"
                        data-approvals
                        class="inline-flex min-h-11 min-w-11 shrink-0 items-center justify-center gap-1.5 rounded-full bg-caramel px-3 font-bold text-cream hover:bg-espresso active:bg-ink dark:bg-maple dark:text-night dark:hover:bg-latte dark:active:bg-sand"
                        title="Approvals waiting: tap to answer"
                        on:click=move |_| workspace.panel.set(Some(Panel::Approvals))
                    >
                        <Glyph icon=icondata::FaBellSolid />
                        {move || approvals_label(approvals.get())}
                    </button>
                </Show>
                <Show when=move || live.get()>
                    <span class="min-w-0 flex-1 truncate px-2 text-ink dark:text-sand" title=move || workspace.status.get()>
                        {move || workspace.status.get()}
                    </span>
                </Show>
            </div>
        </Show>
    }
}

/// The session's cost as a wide window's bottom bar shows it.
fn cost_label(cost: f64) -> String {
    format!("${cost:.4}")
}

#[component]
fn Sheet(workspace: Workspace) -> impl IntoView {
    let close = move |_| workspace.close_panel();
    // The project editor fills a phone's screen.
    let tall = move || workspace.panel.get() == Some(Panel::Project);
    let title = move || match workspace.panel.get() {
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
        Some(Panel::Modes) => "Permission mode",
        Some(Panel::Background) => "Background calls",
        Some(Panel::Skills) => "Skills",
        Some(Panel::Permissions) => "Permissions",
        Some(Panel::Message) => "Message",
        Some(Panel::Runtimes) => "Runtimes",
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
            <div class="fixed inset-0 z-10 bg-espresso/30 sm:bg-espresso/10 dark:bg-night/60 sm:dark:bg-night/30" on:click=close></div>
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
                        "pointer-events-auto flex max-h-full flex-col overflow-y-auto border-toasted bg-cream shadow-xl sm:h-full sm:rounded-none sm:border-t-0 sm:border-l dark:border-ember dark:bg-night {}",
                        if tall() { "h-full" } else { "rounded-t-2xl border-t" },
                    )
                >
                    <div class="sticky top-0 z-10 flex flex-col border-b border-toasted bg-parchment dark:border-ember dark:bg-char">
                        <span class="mx-auto mt-1.5 h-1 w-10 rounded-full bg-crust sm:hidden dark:bg-smoke" aria-hidden="true"></span>
                        <div class="flex items-center gap-2 py-1 pr-1 pl-4">
                            <h2 class="flex-1 text-lg font-bold">{title}</h2>
                            <button type="button" class=ICON_BUTTON aria-label="Close" title="Close" on:click=close>
                                <Glyph icon=icondata::FaXmarkSolid class="text-lg" />
                            </button>
                        </div>
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
                            Some(Panel::Modes) => modes_sheet(workspace).into_any(),
                            Some(Panel::Background) => background_sheet(workspace).into_any(),
                            Some(Panel::Skills) => skills_sheet(workspace).into_any(),
                            Some(Panel::Permissions) => permissions_sheet(workspace).into_any(),
                            Some(Panel::Message) => message_sheet(workspace).into_any(),
                            Some(Panel::Runtimes) => runtimes_sheet(workspace).into_any(),
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
                <Glyph icon=icondata::FaCheckSolid class="text-caramel dark:text-maple" />
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
const DOC_ID_ROW: &str = "block w-full truncate font-mono text-xs text-ink dark:text-sand";
/// A chip's document ID, beside its name.
const DOC_ID_CHIP: &str =
    "min-w-0 max-w-[10rem] truncate font-mono text-xs text-ink dark:text-sand";

fn agents_sheet(workspace: Workspace) -> impl IntoView {
    let current = workspace.draft.get_untracked().map(|draft| draft.agent);
    let agents = workspace.picker_agents();
    if agents.is_empty() {
        return empty_state(icondata::FaRobotSolid, "No agent can start a session.").into_any();
    }
    agents
        .into_iter()
        .map(|agent| {
            let picked = agent.clone();
            let chosen = current.as_ref() == Some(&agent.id);
            // An ACP agent shows its runtime; one that is unavailable now is
            // dimmed and cannot be picked.
            let attached = workspace.agent_runtimes();
            let pickable = cookie_agent_client_state::acp::pickable(&agent, &attached);
            let note = cookie_agent_client_state::acp::runtime_note(&agent, &attached);
            let reason = (!pickable)
                .then(|| cookie_agent_client_state::acp::unpickable_reason(&agent, &attached));
            view! {
                <button
                    type="button"
                    class=ROW
                    class:bg-toasted=chosen
                    class:dark:bg-ember=chosen
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
        })
        .collect_view()
        .into_any()
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
            return empty_state(icondata::FaSlidersSolid, message).into_any();
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
                        <div class="flex items-center gap-2 border-b border-toasted px-3 pb-2 dark:border-ember" data-agent-option=option.id.clone()>
                            <span class="flex min-w-0 flex-1 flex-col">
                                <span class="text-sm font-bold">{option.name.clone()}</span>
                                {option.description.clone().map(|description| view! { <span class=NOTE>{description}</span> })}
                            </span>
                            <button
                                type="button"
                                role="switch"
                                class=BUTTON
                                class:bg-toasted=on
                                class:dark:bg-ember=on
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
                                class:bg-toasted=chosen
                                class:dark:bg-ember=chosen
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
                    <div class="flex flex-col gap-1 border-b border-toasted pb-2 dark:border-ember" data-agent-option=option.id.clone()>
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
                            class:bg-toasted=chosen
                            class:dark:bg-ember=chosen
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
                            class:bg-toasted=chosen
                            class:dark:bg-ember=chosen
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
                <div class="flex flex-col gap-1 border-b border-toasted pb-2 dark:border-ember">
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
                {empty_state(icondata::FaMicrochipSolid, message)}
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
            <div class="flex flex-col gap-1 border-b border-toasted pb-2 dark:border-ember">{models}</div>
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
            icondata::FaMicrochipSolid,
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
                            class:bg-toasted=chosen
                            class:dark:bg-ember=chosen
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
                <div class="flex flex-col gap-1 border-b border-toasted pb-2 dark:border-ember">
                    <button
                        type="button"
                        class=ROW
                        class:bg-toasted=chosen
                        class:dark:bg-ember=chosen
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
                    class:bg-toasted=chosen
                    class:dark:bg-ember=chosen
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
            return empty_state(icondata::FaCircleCheckRegular, "No approval is waiting.")
                .into_any();
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
                                tool.header_text(),
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
                let decide = move |decision: ApprovalUserDecision| {
                    let approval = approval.clone();
                    move |_| workspace.respond(approval.clone(), decision)
                };
                view! {
                    <div class="approval flex flex-col gap-2 rounded-lg border border-caramel bg-parchment p-3 dark:border-maple dark:bg-char">
                        <p class="flex items-center gap-2 font-mono text-sm font-bold">
                            <Glyph icon=icon class="text-caramel dark:text-maple" />
                            <span class="min-w-0 break-words">{title}</span>
                        </p>
                        <div class="min-w-0" inner_html=preview></div>
                        {reviewer}
                        <div class="flex flex-wrap gap-2">
                            <button type="button" class=PRIMARY data-decision="approve_once" on:click=decide(ApprovalUserDecision::ApproveOnce)>
                                <Glyph icon=icondata::FaCheckSolid />
                                "Approve once"
                            </button>
                            <button type="button" class=BUTTON data-decision="approve_tree" on:click=decide(ApprovalUserDecision::ApproveTree)>
                                <Glyph icon=icondata::FaCheckDoubleSolid />
                                "Approve for this session tree"
                            </button>
                            <button type="button" class=BUTTON data-decision="reject" on:click=decide(ApprovalUserDecision::Reject)>
                                <Glyph icon=icondata::FaXmarkSolid />
                                "Reject"
                            </button>
                        </div>
                    </div>
                }
            })
            .collect_view()
            .into_any()
    }
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
                                <dt class="border-b border-toasted py-1 text-ink dark:border-ember dark:text-sand">{name}</dt>
                                <dd class="border-b border-toasted py-1 text-right font-mono dark:border-ember">{value}</dd>
                            })
                            .collect_view()}
                        <dt class="pt-1 text-ink dark:text-sand">"cache hit rate"</dt>
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
                        <Glyph icon=icondata::FaGaugeSimpleHighSolid class="text-caramel dark:text-maple" />
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
                class:bg-toasted=chosen
                class:dark:bg-ember=chosen
                aria-pressed=move || chosen().to_string()
                on:click=move |_| workspace.set_level(level)
            >
                {move || choice_line(label.to_owned(), chosen(), "text-sm")}
            </button>
        }
    })
    .collect_view()
}

/// The session's ⋮ menu: every command, by name, icon, and what it does,
/// in rows a finger can hit; each opens its sheet.
fn commands_sheet(workspace: Workspace) -> impl IntoView {
    let has_crons = workspace.has_crons.get_untracked();
    MENU.iter()
        .filter(|entry| crate::commands::offered(entry.command, has_crons))
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
                        <Glyph icon=entry.icon class="w-5 justify-center text-caramel dark:text-maple" />
                        <span class="flex min-w-0 flex-1 flex-col">
                            <span class="text-sm font-bold">{entry.label}</span>
                            <span class=NOTE>{entry.description}</span>
                        </span>
                    </span>
                </button>
            }
        })
        .collect_view()
}

/// The permission mode picker: every mode with its shield and what it
/// does, the current one checked; a click sets it.
fn modes_sheet(workspace: Workspace) -> impl IntoView {
    view! {
        {MODES
            .into_iter()
            .map(|choice| {
                let chosen = move || workspace.permission_mode() == choice.mode;
                view! {
                    <button
                        type="button"
                        class=ROW
                        class:bg-toasted=chosen
                        class:dark:bg-ember=chosen
                        aria-pressed=move || chosen().to_string()
                        data-mode=choice.name
                        on:click=move |_| {
                            workspace.set_permission_mode(choice.mode);
                            workspace.panel.set(None);
                        }
                    >
                        <span class="flex w-full items-center gap-3">
                            <Glyph icon=icons::permission_mode(choice.mode).0 class="w-5 justify-center text-caramel dark:text-maple" />
                            <span class="flex min-w-0 flex-1 flex-col">
                                {move || choice_line(choice.name.to_owned(), chosen(), "font-mono text-sm font-bold")}
                                <span class=NOTE>{choice.description}</span>
                            </span>
                        </span>
                    </button>
                }
            })
            .collect_view()}
        <p class=format!("mt-2 flex items-start gap-2 border-t border-toasted px-3 pt-3 dark:border-ember {NOTE}")>
            <Glyph icon=icondata::FaCircleInfoSolid class="mt-0.5" />
            {FOOTNOTE}
        </p>
    }
}
