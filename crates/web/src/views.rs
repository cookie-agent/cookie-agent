//! The connected page: the rail and its views. Chat is the sessions
//! sidebar, the title bar, the transcript, the queue strip, the composer,
//! the bottom bar, and the session's sheets; the other views are full
//! pages ([`shell`]). Wide windows show the sheets on the right, phones
//! from the bottom.

use std::{collections::HashMap, sync::Arc, time::Duration};

use cookie_agent_client_state::{
    EventLevel, ToolStatus,
    permissions::{EFFECTS, effect_label, permission_actions, permission_rows, source_label},
    session_actions::permission_mode_label,
};
use cookie_agent_protocol::{
    ApprovalUserDecision, InputDelivery, ModelSelection, PermissionAction, PermissionEffect,
    PermissionRuleSource, SessionId, SessionMeta,
};
use leptos::{ev, html, prelude::*};
use wasm_bindgen::JsCast as _;

use crate::{
    commands::{Command, completions},
    format::{basename, compact_cost, compact_count, relative_time},
    goal::{GoalCommand, action_label, goal_actions, status_name},
    icons::{self, Glyph},
    keys::{ComposerAction, ComposerButtons, KeyPress, composer_action, composer_buttons},
    render::{
        blocks::{BlockPlace, display_html},
        transcript::{Delegate, TranscriptView, item_html, item_key},
    },
    workspace::{Panel, Workspace},
};

mod depth;
mod install;
mod manage;
mod shell;

pub(crate) use install::{Install, InstallAction, InstallBanner};

use depth::{
    AttachButtons, BackgroundBar, DraftFiles, GoalBar, background_sheet, goal_sheet, message_sheet,
    permissions_sheet, skills_sheet,
};

use manage::runtimes_sheet;
use shell::{Drawer, PageView, Rail, SidebarButton};

use crate::{
    app::Connection,
    layout::{Press, View, session_menu},
    theme::ThemePreference,
    tree::{Activity, delegates_of, path, subagent_label, tree_rows},
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
/// A session row's delete button, which shows only over the hovered (or
/// focused) row: centred on its two lines, at rest in the row's hover
/// colour so it blends in (the open row's hover colour is the same), one
/// palette step darker on hover (lighter in dark), and further still when
/// pressed. 44 px square, over the row's right padding.
const ROW_DELETE: &str = "absolute top-1/2 right-0.5 min-h-11 min-w-11 -translate-y-1/2 items-center justify-center rounded-md bg-toasted text-ink hover:bg-crust hover:text-espresso active:bg-[color-mix(in_oklab,var(--color-crust),var(--color-espresso)_18%)] dark:bg-ember dark:text-sand dark:hover:bg-smoke dark:hover:text-latte dark:active:bg-[color-mix(in_oklab,var(--color-smoke),var(--color-latte)_18%)]";
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
    crate::browser::on_media_change(crate::browser::PHONE_QUERY, move |narrow| {
        workspace.narrow.set(narrow);
    });
    // A window that grows to a desktop's pins the sidebar instead.
    crate::browser::on_media_change(crate::browser::DESKTOP_QUERY, move |desktop| {
        if desktop {
            workspace.sidebar_open.set(false);
        }
    });
    workspace.follow_history();
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
                <Transcript workspace=workspace />
                <GoalBar workspace=workspace />
                <BackgroundBar workspace=workspace />
                <QueueStrip workspace=workspace />
                <DraftFiles workspace=workspace />
                <StatusLine workspace=workspace />
                // A short window (a phone on its side) puts the bottom bar
                // beside the composer.
                <div class="flex flex-col short:flex-row short:items-end">
                    <Composer workspace=workspace composing=composing />
                    <BottomBar workspace=workspace composing=composing />
                </div>
            </main>
            <ModeToast workspace=workspace />
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
    let directories = move || {
        let mut directories = workspace.sessions.with(|sessions| {
            sessions
                .iter()
                .map(|session| session.cwd_identity.to_string())
                .collect::<Vec<_>>()
        });
        directories.sort();
        directories.dedup();
        directories
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
                            .is_none_or(|filter| session.cwd_identity.as_str() == filter)
                })
                .cloned()
                .collect::<Vec<_>>()
        });
        sessions.sort_by_key(|session| std::cmp::Reverse(session.last_activity));
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
    // Deleting: the row a long press revealed the action of, and the row
    // whose delete waits for confirmation.
    let deletes = RowDeletes {
        hovered: RwSignal::new(None),
        revealed: RwSignal::new(None),
        confirming: RwSignal::new(None),
        press: StoredValue::new(Press::default()),
        timer: StoredValue::new(None),
    };
    view! {
        <div class="flex flex-col gap-2 p-3">
            <button type="button" class=PRIMARY data-new-session on:click=move |_| workspace.start_new_session()>
                <Glyph icon=icondata::FaPlusSolid />
                "New session"
            </button>
            <label class="flex flex-col gap-1 text-xs font-bold text-ink dark:text-sand">
                <span>"Directory"</span>
                <select
                    class="min-h-11 rounded-md border border-crust bg-cream px-2 text-sm font-normal text-espresso hover:border-caramel dark:border-smoke dark:bg-night dark:text-latte dark:hover:border-maple"
                    on:change=move |event| {
                        let value = event_target_value(&event);
                        workspace.directory_filter.set((!value.is_empty()).then_some(value));
                    }
                >
                    <option value="">"All directories"</option>
                    {move || {
                        directories()
                            .into_iter()
                            .map(|directory| {
                                let selected = workspace.directory_filter.get().as_deref()
                                    == Some(directory.as_str());
                                view! {
                                    <option value=directory.clone() selected=selected>
                                        {directory.clone()}
                                    </option>
                                }
                            })
                            .collect_view()
                    }}
                </select>
            </label>
        </div>
        <nav class="flex min-h-0 flex-1 flex-col gap-1 overflow-y-auto px-2 pb-3" aria-label="Sessions">
            {move || {
                let sessions = shown();
                if sessions.is_empty() {
                    return empty_state(icondata::FaFolderOpenSolid, "No sessions yet. Start one with New session.")
                        .into_any();
                }
                sessions
                    .into_iter()
                    .map(|session| session_row(workspace, session, now, deletes))
                    .collect_view()
                    .into_any()
            }}
        </nav>
    }
}

/// The session list's delete state, shared by its rows.
#[derive(Clone, Copy)]
struct RowDeletes {
    /// The row a mouse (or pen) is over, which shows its trash.
    hovered: RwSignal<Option<SessionId>>,
    revealed: RwSignal<Option<SessionId>>,
    confirming: RwSignal<Option<SessionId>>,
    /// The touch press under way, one at a time.
    press: StoredValue<Press>,
    timer: StoredValue<Option<TimeoutHandle>>,
}

impl RowDeletes {
    fn stop_timer(self) {
        if let Some(timer) = self.timer.get_value() {
            timer.clear();
        }
        self.timer.set_value(None);
    }
}

/// A root session's row: a click opens it; a trash button on hover (or a
/// long press on a touch screen) offers to delete it, behind an inline
/// confirmation. The open session's row has its subagents under it.
fn session_row(
    workspace: Workspace,
    session: SessionMeta,
    now: RwSignal<jiff::Timestamp>,
    deletes: RowDeletes,
) -> impl IntoView {
    let session_id = session.session_id;
    let current = move || workspace.selected.get() == Some(session_id);
    let mark = icons::session_status(session.status);
    let directory = session.cwd_identity.to_string();
    let last_activity = session.last_activity;
    let title = format!("{directory}\n{}", mark.label);
    let label = session_label(&session);
    let confirm_label = label.clone();
    let RowDeletes {
        hovered,
        revealed,
        confirming,
        press,
        ..
    } = deletes;
    let on_pointerdown = move |event: ev::PointerEvent| {
        if event.pointer_type() != "touch" || !workspace.can_delete.get_untracked() {
            return;
        }
        let (x, y) = (f64::from(event.client_x()), f64::from(event.client_y()));
        press.update_value(|press| press.start(x, y));
        deletes.stop_timer();
        let timer = set_timeout_with_handle(
            move || {
                let fired = press.try_update_value(Press::elapse).unwrap_or(false);
                if fired {
                    confirming.set(None);
                    revealed.set(Some(session_id));
                }
            },
            LONG_PRESS,
        )
        .ok();
        deletes.timer.set_value(timer);
    };
    let on_pointermove = move |event: ev::PointerEvent| {
        let (x, y) = (f64::from(event.client_x()), f64::from(event.client_y()));
        if press
            .try_update_value(|press| press.moved(x, y))
            .unwrap_or(false)
        {
            deletes.stop_timer();
        }
    };
    let release = move |_| {
        press.update_value(Press::cancel);
        deletes.stop_timer();
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
            <button
                type="button"
                // The row looks hovered while the pointer is on its delete
                // button too, which sits over it, or the keyboard is in it.
                class=format!("{ROW} no-callout select-none pr-12 group-hover:bg-toasted group-has-focus-visible:bg-toasted dark:group-hover:bg-ember dark:group-has-focus-visible:bg-ember")
                class:bg-toasted=current
                class:dark:bg-ember=current
                aria-current=move || current().then_some("true")
                data-session=session_id.to_string()
                title=title
                on:pointerdown=on_pointerdown
                on:pointermove=on_pointermove
                on:pointerup=release
                on:pointercancel=release
                on:contextmenu=move |event: ev::MouseEvent| {
                    // A touch screen's long press is the delete action, not
                    // the browser's menu.
                    if revealed.get_untracked() == Some(session_id) {
                        event.prevent_default();
                    }
                }
                on:click=move |_| {
                    if press.try_update_value(Press::click).unwrap_or(true) {
                        workspace.open_session(session_id);
                    }
                }
            >
                <span class="flex w-full items-center gap-2">
                    <Glyph icon=mark.icon class=mark.class("text-xs") />
                    <span class="sr-only">{mark.label}</span>
                    <span class="min-w-0 flex-1 truncate text-sm font-bold">{label}</span>
                </span>
                <span class=format!("flex w-full min-w-0 items-center gap-1 pl-5 {NOTE}")>
                    <Glyph icon=icondata::FaFolderSolid class="opacity-70" />
                    <span class="min-w-0 truncate">{basename(&directory).to_owned()}</span>
                    <span class="shrink-0">
                        {move || format!("· {}", relative_time(now.get(), last_activity))}
                    </span>
                </span>
            </button>
            <Show when=move || workspace.can_delete.get()>
                <button
                    type="button"
                    // Shown while a mouse is over the row, or the keyboard
                    // is in it; a touch screen's long press offers it below.
                    class=move || {
                        let shown = if hovered.get() == Some(session_id) {
                            "inline-flex"
                        } else {
                            "hidden group-has-focus-visible:inline-flex"
                        };
                        format!("{ROW_DELETE} {shown}")
                    }
                    aria-label="Delete the session"
                    title="Delete the session and its subagents"
                    data-delete=session_id.to_string()
                    on:click=move |_| {
                        revealed.set(None);
                        confirming.set(Some(session_id));
                    }
                >
                    <Glyph icon=icondata::FaTrashCanRegular />
                </button>
            </Show>
        </div>
        <Show when=move || revealed.get() == Some(session_id) && confirming.get() != Some(session_id)>
            <div class="flex gap-2 px-2 pb-1" data-delete-actions>
                <button
                    type="button"
                    class=BUTTON
                    data-delete-reveal=session_id.to_string()
                    on:click=move |_| confirming.set(Some(session_id))
                >
                    <Glyph icon=icondata::FaTrashCanRegular />
                    "Delete"
                </button>
                <button type="button" class=BUTTON on:click=move |_| revealed.set(None)>
                    "Cancel"
                </button>
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
        <Show when=move || workspace.tree_root.get() == Some(session_id) && workspace.subagents_shown()>
            <Subagents workspace=workspace />
        </Show>
    }
}

/// The open session's subagents, nested under its row: each with its
/// status, agent, handle, and task; a click opens one, the chevron folds
/// its own.
#[component]
fn Subagents(workspace: Workspace) -> impl IntoView {
    let rows = move || {
        let collapsed = workspace.collapsed.get();
        workspace
            .tree
            .with(|tree| tree.as_ref().map(|tree| tree_rows(tree, &collapsed)))
            .unwrap_or_default()
    };
    view! {
        <div role="group" aria-label="Subagents" data-subagents class="flex flex-col gap-0.5 pb-1">
            {move || {
                rows()
                    .into_iter()
                    .filter(|row| row.depth > 0)
                    .map(|row| {
                        let session_id = row.session_id;
                        let current = move || workspace.selected.get() == Some(session_id);
                        let mark = icons::session_status(row.status);
                        let indent = format!("{}rem", 0.75 * (row.depth - 1) as f64);
                        let fold = if row.has_children {
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
                        view! {
                            <div class="flex items-start" style:padding-left=indent>
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
                                        <Glyph icon=mark.icon class=mark.class("text-xs") />
                                        <span class="sr-only">{format!("{}: ", mark.label)}</span>
                                        <span class="truncate font-mono font-bold">{row.agent.clone()}</span>
                                        {row.handle.clone().map(|handle| view! {
                                            <span class=format!("shrink-0 font-mono {NOTE}")>{handle}</span>
                                        })}
                                    </span>
                                    <span class=format!("w-full truncate pl-5 {NOTE}")>{row.task.clone()}</span>
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

/// The chat's title bar: the sidebar button, the session's title, and its
/// ⋮ menu of session commands.
#[component]
fn TitleBar(workspace: Workspace) -> impl IntoView {
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
                <h1 class="min-w-0 truncate text-lg font-bold" id="session-title" title=title>
                    {title}
                </h1>
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

#[component]
fn Transcript(workspace: Workspace) -> impl IntoView {
    let cache = StoredValue::new(HashMap::<u64, Arc<str>>::new());
    let items = Memo::new(move |_| {
        let Some(session_id) = workspace
            .selected
            .get()
            .filter(|_| !workspace.new_session.get())
        else {
            return Vec::new();
        };
        let level = workspace.level.get();
        // What the user opened in this session: its own, as in the TUI.
        let expanded = workspace
            .expanded
            .with(|expanded| expanded.get(&session_id).cloned().unwrap_or_default());
        let names = workspace.with_runtime(|runtime| runtime.model_names().clone());
        let delegates = workspace.tree.with(|tree| {
            tree.as_ref()
                .map(|tree| delegates_of(tree, session_id))
                .unwrap_or_default()
                .into_iter()
                .map(|(call, child)| (call, child.clone()))
                .collect::<Vec<_>>()
        });
        workspace.with_store(|store| {
            let Some(state) = store.sessions.get(&session_id) else {
                return Vec::new();
            };
            // What each running subagent does, from its own projection
            // (the page follows a tree's live sessions).
            let delegates = delegates
                .into_iter()
                .map(|(call, child)| {
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
                        )
                    });
                    let delegate = Delegate {
                        session_id: child.session_id,
                        agent: child.creation_selection.agent.to_string(),
                        handle: child.short_id.clone(),
                        activity,
                    };
                    (call, delegate)
                })
                .collect::<HashMap<_, _>>();
            let view = TranscriptView {
                session_id,
                state,
                names: &names,
                minimum_level: level,
                expanded: &expanded,
                delegates: &delegates,
            };
            cache
                .try_update_value(|cache| {
                    let mut kept = HashMap::new();
                    let rows = state
                        .transcript
                        .iter()
                        .filter_map(|item| {
                            let key = item_key(&view, item);
                            let html = match cache.get(&key) {
                                Some(html) => Arc::clone(html),
                                None => Arc::from(item_html(&view, item)?),
                            };
                            kept.insert(key, Arc::clone(&html));
                            Some((key, html))
                        })
                        .collect::<Vec<_>>();
                    *cache = kept;
                    rows
                })
                .unwrap_or_default()
        })
    });
    let container = NodeRef::<html::Div>::new();
    let following = StoredValue::new(true);
    Effect::new(move |_| {
        items.track();
        let Some(element) = container.get() else {
            return;
        };
        let element: web_sys::Element = element.into();
        request_animation_frame(move || {
            if following.get_value() {
                element.set_scroll_top(element.scroll_height());
            }
            crate::highlight::highlight_within(&element);
            crate::media::show_within(&element, workspace);
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
            _ => {}
        }
    };
    // Following stops only when the user scrolls away from the bottom
    // (wheel, touch, or the scrollbar), never when an item's node is
    // replaced or the box shrinks under a bar.
    let near_bottom = move || {
        container.get().is_some_and(|element| {
            let element: web_sys::Element = element.into();
            element.scroll_height() - element.scroll_top() - element.client_height() < 80
        })
    };
    let user_scrolling = StoredValue::new(false);
    let on_scroll = move |_| {
        if near_bottom() {
            following.set_value(true);
        } else if user_scrolling.get_value() {
            following.set_value(false);
        }
    };
    view! {
        <div
            node_ref=container
            id="transcript"
            class="min-h-0 flex-1 overflow-y-auto px-3 py-2"
            on:click=on_click
            on:scroll=on_scroll
            on:wheel=move |event: ev::WheelEvent| {
                if event.delta_y() < 0.0 {
                    following.set_value(false);
                }
            }
            on:touchstart=move |_| user_scrolling.set_value(true)
            on:touchend=move |_| user_scrolling.set_value(false)
            on:pointerdown=move |_| user_scrolling.set_value(true)
            on:pointerup=move |_| user_scrolling.set_value(false)
        >
            <Show when=move || workspace.new_session.get()>
                <NewSessionCard workspace=workspace />
            </Show>
            <div class="mx-auto flex max-w-4xl flex-col gap-2">
                <For
                    each=move || items.get()
                    key=|(key, _)| *key
                    children=move |(_, html)| {
                        view! { <div class="transcript-item min-w-0" inner_html=html.to_string()></div> }
                    }
                />
            </div>
        </div>
    }
}

#[component]
fn NewSessionCard(workspace: Workspace) -> impl IntoView {
    view! {
        <div class="mx-auto my-4 flex max-w-xl flex-col gap-3 rounded-lg border border-toasted bg-parchment p-4 dark:border-ember dark:bg-char">
            <h2 class="flex items-center gap-2 text-lg font-bold">
                <Glyph icon=icondata::FaPlusSolid class="text-caramel dark:text-maple" />
                "New session"
            </h2>
            <label class="flex flex-col gap-1 text-sm">
                <span class="font-bold">"Directory"</span>
                <input
                    type="text"
                    class="min-h-11 rounded-md border border-crust bg-cream px-3 font-mono hover:border-caramel focus:border-caramel focus:outline-none dark:border-smoke dark:bg-night dark:hover:border-maple dark:focus:border-maple"
                    placeholder="the daemon's own directory"
                    prop:value=move || workspace.new_directory.get()
                    on:input=move |event| {
                        workspace.new_directory.set(event_target_value(&event));
                        // The draft's runtimes follow the directory again.
                        workspace.draft_runtimes.set(None);
                    }
                />
            </label>
            <p class=NOTE>
                "The first message creates the session with the agent and model below."
            </p>
        </div>
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

/// Sizes the composer to its text, from one line to five.
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
    let completions = move || completions(&workspace.composer.get());
    let attach_buttons = Memo::new(move |_| crate::views::depth::attach_buttons(workspace));
    let buttons = Memo::new(move |_| {
        let has_input = workspace.composer.with(|text| !text.trim().is_empty())
            || !workspace.attachments.with(Vec::is_empty);
        composer_buttons(workspace.stoppable(), has_input)
    });
    // The box follows its text: typed, pasted, recalled, or sent away; and
    // its buttons, whose number changes the room the text has.
    let fit = move || {
        if let Some(textarea) = textarea.get_untracked() {
            fit_composer(&textarea);
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
    view! {
        <div class="relative min-w-0 px-3 pt-2 pb-1 short:flex-1 short:pb-2">
            <Show when=move || !completions().is_empty()>
                <div class="absolute bottom-full left-3 z-20 mb-1 flex max-h-64 w-80 max-w-[calc(100%-1.5rem)] flex-col overflow-y-auto rounded-lg border border-crust bg-cream p-1 shadow-lg dark:border-smoke dark:bg-night">
                    {move || {
                        completions()
                            .into_iter()
                            .map(|spec| {
                                let command = spec.command;
                                let name = spec.name;
                                let takes_argument = spec.takes_argument;
                                view! {
                                    <button
                                        type="button"
                                        class=ROW
                                        on:click=move |_| {
                                            if takes_argument {
                                                // The argument follows, typed after the name.
                                                workspace.composer.set(format!("/{name} "));
                                                if let Some(textarea) = textarea.get() {
                                                    let _ = textarea.focus();
                                                }
                                            } else {
                                                workspace.composer.set(String::new());
                                                workspace.run_command(command, String::new());
                                            }
                                        }
                                    >
                                        <span class="font-mono text-sm font-bold">{format!("/{}", spec.name)}</span>
                                        <span class=NOTE>{spec.description}</span>
                                    </button>
                                }
                            })
                            .collect_view()
                    }}
                </div>
            </Show>
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
                        // reappears and moves it.
                        blurred.update_value(|blurred| *blurred += 1);
                        let current = blurred.get_value();
                        set_timeout(
                            move || {
                                if blurred.get_value() == current {
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
                <div class="absolute right-1 bottom-1 flex items-center gap-1.5" data-composer-actions>
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
                            {button(
                                "steer", "Steer", "Enter", icondata::FaPaperPlaneSolid, true,
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
    workspace.selected.get().is_some_and(|session_id| {
        workspace.with_store(|store| {
            store.sessions.get(&session_id).is_some_and(|state| {
                state.active_run.is_some()
                    || state
                        .tools
                        .values()
                        .any(|tool| tool.status == ToolStatus::Running)
            })
        })
    })
}

fn approvals_label(count: usize) -> String {
    match count {
        1 => "1 approval waiting".to_owned(),
        count => format!("{count} approvals waiting"),
    }
}

/// How long a phone's status line keeps a new status message.
const STATUS_LIVE: Duration = Duration::from_secs(8);

/// The bottom bar: the session's agent, model, permission mode, and cost.
/// A wide window adds the approvals, the working spinner, the status
/// message, and the context size; a phone shows those in the status line
/// above the composer, and hides the bar while the composer has focus.
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
        workspace.selected.get().and_then(|session_id| {
            workspace.with_store(|store| {
                store
                    .sessions
                    .get(&session_id)
                    .and_then(|state| state.context_tokens)
            })
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
            class="flex flex-wrap items-center gap-x-1 bg-parchment px-2 text-xs compact:flex-nowrap short:w-[22rem] short:shrink-0 short:bg-transparent dark:bg-char short:dark:bg-transparent"
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
                aria-label=move || format!("Permission mode: {}", permission_mode_label(mode()))
                title=move || {
                    let mode = mode();
                    format!(
                        "Permission mode: {}, {}. Click for the next mode.",
                        permission_mode_label(mode),
                        icons::permission_mode(mode).1
                    )
                }
                on:click=move |_| workspace.cycle_permission_mode()
            >
                <Glyph icon=Signal::derive(move || icons::permission_mode(mode()).0) class="text-caramel dark:text-maple" />
                // A toggle, not a picker: no chevron. A phone shows the
                // shield alone and says the new mode in a toast.
                <span class="min-w-0 truncate compact:sr-only">{move || permission_mode_label(mode())}</span>
            </button>
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
            <Show when=move || running(workspace)>
                <span class="inline-flex min-h-11 items-center gap-1.5 px-2 font-bold text-caramel compact:hidden dark:text-maple" role="status">
                    <Glyph icon=icondata::FaCircleNotchSolid class="animate-spin" />
                    "working…"
                </span>
            </Show>
            <span
                class="min-w-[8rem] flex-1 truncate px-2 text-ink compact:hidden dark:text-sand"
                id="status"
                role="status"
                title=move || workspace.status.get()
            >
                {move || workspace.status.get()}
            </span>
            {move || context().map(|tokens| view! {
                <span
                    class="inline-flex min-h-11 items-center gap-1.5 px-2 text-ink compact:hidden dark:text-sand"
                    title=format!("Context: {tokens} tokens in the last request")
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
                    on:click=move |_| workspace.run_command(Command::Usage, String::new())
                >
                    <Glyph icon=icondata::FaCoinsSolid class="text-caramel dark:text-maple compact:hidden" />
                    <span class="font-mono compact:hidden">{cost_label(cost)}</span>
                    <span class="hidden font-mono compact:inline">{compact_cost(cost)}</span>
                </button>
            })}
        </div>
    }
}

/// A phone's line above the composer: the approvals waiting, the working
/// spinner, and a new status message for a few seconds; it shows only
/// while one of them does. Wider windows have these in the bottom bar.
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
            set_timeout(
                move || {
                    if generation.get_value() == current {
                        live.set(false);
                    }
                },
                STATUS_LIVE,
            );
        }
        status
    });
    let approvals = Memo::new(move |_| workspace.approvals().len());
    let shown = move || running(workspace) || approvals.get() > 0 || live.get();
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
                <Show when=move || running(workspace)>
                    <span class="inline-flex shrink-0 items-center gap-1.5 px-2 font-bold text-caramel dark:text-maple" role="status">
                        <Glyph icon=icondata::FaCircleNotchSolid class="animate-spin" />
                        "working…"
                    </span>
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

/// How long the permission mode's toast shows.
const TOAST: Duration = Duration::from_millis(2000);

/// "now: <mode>" for a moment after the permission mode changes, on a phone
/// whose bottom bar shows only the mode's icon.
#[component]
fn ModeToast(workspace: Workspace) -> impl IntoView {
    let shown = RwSignal::new(None::<String>);
    let generation = StoredValue::new(0_u64);
    Effect::new(
        move |previous: Option<(
            Option<cookie_agent_protocol::SessionId>,
            bool,
            cookie_agent_protocol::PermissionMode,
        )>| {
            let now = (
                workspace.selected.get(),
                workspace.new_session.get(),
                workspace.permission_mode(),
            );
            // Only a change of the same session's mode: not a session switch.
            if let Some(previous) = previous
                && previous.0 == now.0
                && previous.1 == now.1
                && previous.2 != now.2
            {
                generation.update_value(|generation| *generation += 1);
                let current = generation.get_value();
                shown.set(Some(format!("now: {}", permission_mode_label(now.2))));
                set_timeout(
                    move || {
                        if generation.get_value() == current {
                            shown.set(None);
                        }
                    },
                    TOAST,
                );
            }
            now
        },
    );
    view! {
        {move || shown.get().map(|text| view! {
            <div
                id="mode-toast"
                role="status"
                class="pointer-events-none fixed inset-x-0 bottom-28 z-30 hidden justify-center px-4 compact:flex"
            >
                <span class="rounded-full bg-espresso px-4 py-2 font-mono text-sm text-cream shadow-lg dark:bg-latte dark:text-night">
                    {text}
                </span>
            </div>
        })}
    }
}

/// The session's cost as a wide window's bottom bar shows it.
fn cost_label(cost: f64) -> String {
    format!("${cost:.4}")
}

#[component]
fn Sheet(workspace: Workspace) -> impl IntoView {
    let close = move |_| workspace.panel.set(None);
    let title = move || match workspace.panel.get() {
        Some(Panel::Agents) => "Agent",
        Some(Panel::Models) => "Model",
        Some(Panel::Presets) => "Preset",
        Some(Panel::Approvals) => "Approvals",
        Some(Panel::Usage) => "Usage",
        Some(Panel::Events) => "Events",
        Some(Panel::Commands) => "Session commands",
        Some(Panel::Goal) => "Goal",
        Some(Panel::Background) => "Background calls",
        Some(Panel::Skills) => "Skills",
        Some(Panel::Permissions) => "Permissions",
        Some(Panel::Message) => "Message",
        Some(Panel::Runtimes) => "Runtimes",
        None => "",
    };
    view! {
        <Show when=move || workspace.panel.get().is_some()>
            <div class="fixed inset-0 z-10 bg-espresso/30 sm:bg-espresso/10 dark:bg-night/60 sm:dark:bg-night/30" on:click=close></div>
            // A phone's sheet rises from the bottom of what the keyboard
            // leaves; a wider window's stands on the right.
            <div class="sheet-frame pointer-events-none fixed inset-x-0 top-0 z-20 flex flex-col justify-end pt-12 sm:inset-y-0 sm:right-0 sm:left-auto sm:w-[28rem] sm:pt-0">
                <section
                    id="sheet"
                    role="dialog"
                    aria-label=title
                    class="pointer-events-auto flex max-h-full flex-col overflow-y-auto rounded-t-2xl border-t border-toasted bg-cream shadow-xl sm:h-full sm:rounded-none sm:border-t-0 sm:border-l dark:border-ember dark:bg-night"
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
                            Some(Panel::Presets) => presets_sheet(workspace).into_any(),
                            Some(Panel::Approvals) => approvals_sheet(workspace).into_any(),
                            Some(Panel::Usage) => usage_sheet(workspace).into_any(),
                            Some(Panel::Events) => events_sheet(workspace).into_any(),
                            Some(Panel::Commands) => commands_sheet(workspace).into_any(),
                            Some(Panel::Goal) => goal_sheet(workspace).into_any(),
                            Some(Panel::Background) => background_sheet(workspace).into_any(),
                            Some(Panel::Skills) => skills_sheet(workspace).into_any(),
                            Some(Panel::Permissions) => permissions_sheet(workspace).into_any(),
                            Some(Panel::Message) => message_sheet(workspace).into_any(),
                            Some(Panel::Runtimes) => runtimes_sheet(workspace).into_any(),
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

fn agents_sheet(workspace: Workspace) -> impl IntoView {
    let current = workspace.draft.get_untracked().map(|draft| draft.agent);
    let agents = workspace.selectable_agents();
    if agents.is_empty() {
        return empty_state(icondata::FaRobotSolid, "No agent can start a session.").into_any();
    }
    agents
        .into_iter()
        .map(|agent| {
            let picked = agent.clone();
            let chosen = current.as_ref() == Some(&agent.id);
            view! {
                <button
                    type="button"
                    class=ROW
                    class:bg-toasted=chosen
                    class:dark:bg-ember=chosen
                    on:click=move |_| {
                        workspace.pick_agent(&picked);
                        workspace.panel.set(None);
                    }
                >
                    {choice_line(agent.id.to_string(), chosen, "font-mono text-sm font-bold")}
                    <span class=NOTE>{agent.description.clone()}</span>
                </button>
            }
        })
        .collect_view()
        .into_any()
}

fn models_sheet(workspace: Workspace) -> impl IntoView {
    let current = workspace.draft.get_untracked().map(|draft| draft.model);
    let models = workspace.models();
    if models.is_empty() {
        return empty_state(
            icondata::FaMicrochipSolid,
            "No model is available. Connect a provider with /connect.",
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
                            on:click=move |_| {
                                workspace.pick_model(selection.clone());
                                workspace.panel.set(None);
                            }
                        >
                            {variant.display_name.clone()}
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
                        <span class=format!("font-mono {NOTE}")>{key.to_string()}</span>
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
            let rows = |usage: &cookie_agent_protocol::UsageRollup| {
                let mut rows = vec![
                    ("input tokens", usage.input_tokens.to_string()),
                    ("output tokens", usage.output_tokens.to_string()),
                    ("reasoning tokens", usage.reasoning_tokens.to_string()),
                    ("cache reads", usage.cache_read_tokens.to_string()),
                    ("cache writes", usage.cache_write_tokens.to_string()),
                    ("requests", usage.request_count.to_string()),
                ];
                if let Some(cost) = usage.estimated_cost_usd {
                    rows.push(("estimated cost", format!("${cost:.4}")));
                }
                rows.into_iter()
                    .map(|(name, value)| view! {
                        <dt class="border-b border-toasted py-1 text-ink dark:border-ember dark:text-sand">{name}</dt>
                        <dd class="border-b border-toasted py-1 text-right font-mono dark:border-ember">{value}</dd>
                    })
                    .collect_view()
            };
            let session = match &usage.session {
                Ok(rollup) => {
                    view! { <dl class="grid grid-cols-2 text-sm">{rows(rollup)}</dl> }.into_any()
                }
                Err(error) => view! { <p class="text-sm">{error.clone()}</p> }.into_any(),
            };
            let tree = match &usage.tree {
                Ok((rollup, count)) => view! {
                    <h3 class="mt-4 text-sm font-bold">
                        {if *count == 1 { "Session tree (1 session)".to_owned() } else { format!("Session tree ({count} sessions)") }}
                    </h3>
                    <dl class="grid grid-cols-2 text-sm">{rows(rollup)}</dl>
                }
                    .into_any(),
                Err(error) => view! { <p class="text-sm">{error.clone()}</p> }.into_any(),
            };
            // The bottom bar of a phone has no room for the context size.
            let context = workspace.selected.get_untracked().and_then(|session_id| {
                workspace.with_store(|store| {
                    store
                        .sessions
                        .get(&session_id)
                        .and_then(|state| state.context_tokens)
                })
            });
            view! {
                {context.map(|tokens| view! {
                    <p class="flex items-center gap-2 text-sm" id="usage-context">
                        <Glyph icon=icondata::FaGaugeSimpleHighSolid class="text-caramel dark:text-maple" />
                        {format!("Context: {tokens} tokens in the last request")}
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

/// The session's ⋮ menu: its own commands, in rows a finger can hit.
fn commands_sheet(workspace: Workspace) -> impl IntoView {
    session_menu()
        .into_iter()
        .map(|spec| {
            let command = spec.command;
            view! {
                <button
                    type="button"
                    class=format!("{ROW} min-h-12")
                    data-command=spec.name
                    on:click=move |_| {
                        workspace.panel.set(None);
                        workspace.run_command(command, String::new());
                    }
                >
                    <span class="font-mono text-sm font-bold">{format!("/{}", spec.name)}</span>
                    <span class=NOTE>{spec.description}</span>
                </button>
            }
        })
        .collect_view()
}
