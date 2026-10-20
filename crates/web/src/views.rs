//! The connected page: the session sidebar, the transcript, the queue strip,
//! the composer, the status bar, and the side sheets. Wide screens show the
//! sidebar beside the transcript and the sheets on the right; narrow ones
//! show the sidebar and every sheet full screen.

use std::{collections::HashMap, sync::Arc, time::Duration};

use cookie_agent_client_state::{
    EventLevel, ToolStatus,
    permissions::{EFFECTS, effect_label, permission_actions, permission_rows, source_label},
    session_actions::permission_mode_label,
};
use cookie_agent_protocol::{
    ApprovalUserDecision, InputDelivery, ModelSelection, PermissionAction, PermissionEffect,
    PermissionRuleSource, SessionMeta,
};
use leptos::{ev, html, prelude::*};
use wasm_bindgen::JsCast as _;

use crate::{
    commands::{COMMANDS, Command, completions},
    format::{basename, compact_count, relative_time},
    goal::{GoalCommand, action_label, goal_actions, status_name},
    icons::{self, Glyph},
    keys::{ComposerAction, KeyPress, composer_action},
    render::{
        blocks::{BlockPlace, display_html},
        transcript::{TranscriptView, item_html, item_key},
    },
    workspace::{Panel, Workspace},
};

mod depth;
mod manage;

use depth::{
    AttachButtons, BackgroundBar, DraftFiles, GoalBar, TreePanel, background_sheet, goal_sheet,
    message_sheet, permissions_sheet, skills_sheet,
};

use manage::{config_sheet, connect_sheet, mcp_sheet, memory_sheet, plugins_sheet, runtimes_sheet};

/// How long a press on Send must last to queue instead of steer.
const LONG_PRESS: Duration = Duration::from_millis(500);

/// Every button is at least 44 px square, with a hover and a pressed state.
/// A labelled button, bordered.
const BUTTON: &str = "inline-flex min-h-11 min-w-11 items-center justify-center gap-2 rounded-md border border-crust px-3 text-sm hover:bg-toasted active:bg-crust disabled:cursor-not-allowed disabled:opacity-50 disabled:hover:bg-transparent dark:border-smoke dark:hover:bg-ember dark:active:bg-smoke";
/// The main action of a form or the composer.
const PRIMARY: &str = "inline-flex min-h-11 min-w-11 items-center justify-center gap-2 rounded-md bg-espresso px-3 text-sm font-bold text-cream hover:bg-caramel active:bg-ink disabled:cursor-not-allowed disabled:opacity-50 disabled:hover:bg-espresso dark:bg-latte dark:text-night dark:hover:bg-maple dark:active:bg-sand dark:disabled:hover:bg-latte";
/// A button that is only an icon: it has an `aria-label` and a `title`.
const ICON_BUTTON: &str = "inline-flex min-h-11 min-w-11 shrink-0 items-center justify-center rounded-md text-ink hover:bg-toasted hover:text-espresso active:bg-crust disabled:cursor-not-allowed disabled:opacity-50 disabled:hover:bg-transparent dark:text-sand dark:hover:bg-ember dark:hover:text-latte dark:active:bg-smoke";
/// A picker showing its current value: the agent and the model.
const CHIP: &str = "inline-flex min-h-11 min-w-0 max-w-[16rem] items-center gap-2 rounded-full border border-crust bg-cream px-3 text-sm hover:border-caramel hover:bg-toasted active:bg-crust dark:border-smoke dark:bg-night dark:hover:border-maple dark:hover:bg-ember dark:active:bg-smoke";
/// A status bar indicator that does something when clicked.
const STATUS_BUTTON: &str = "inline-flex min-h-11 min-w-11 items-center justify-center gap-1.5 rounded-md px-2 text-xs hover:bg-toasted active:bg-crust dark:hover:bg-ember dark:active:bg-smoke";
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
pub fn WorkspaceView(workspace: Workspace) -> impl IntoView {
    view! {
        <div class="flex min-h-0 flex-1">
            <aside class="hidden w-72 shrink-0 flex-col border-r border-toasted bg-parchment md:flex dark:border-ember dark:bg-char">
                <TreePanel workspace=workspace />
                <SessionList workspace=workspace />
            </aside>
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
                <Toolbar workspace=workspace />
                <Transcript workspace=workspace />
                <GoalBar workspace=workspace />
                <BackgroundBar workspace=workspace />
                <QueueStrip workspace=workspace />
                <DraftFiles workspace=workspace />
                <Composer workspace=workspace />
                <StatusBar workspace=workspace />
            </main>
            <Sheet workspace=workspace />
        </div>
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
                    .map(|session| {
                        let session_id = session.session_id;
                        let current = move || workspace.selected.get() == Some(session_id);
                        let mark = icons::session_status(session.status);
                        let directory = session.cwd_identity.to_string();
                        let last_activity = session.last_activity;
                        let title = format!("{directory}\n{}", mark.label);
                        view! {
                            <button
                                type="button"
                                class=ROW
                                class:bg-toasted=current
                                class:dark:bg-ember=current
                                aria-current=move || current().then_some("true")
                                data-session=session_id.to_string()
                                title=title
                                on:click=move |_| workspace.open_session(session_id)
                            >
                                <span class="flex w-full items-center gap-2">
                                    <Glyph icon=mark.icon class=mark.class("text-xs") />
                                    <span class="sr-only">{mark.label}</span>
                                    <span class="min-w-0 flex-1 truncate text-sm font-bold">
                                        {session_label(&session)}
                                    </span>
                                </span>
                                <span class=format!("flex w-full min-w-0 items-center gap-1 pl-5 {NOTE}")>
                                    <Glyph icon=icondata::FaFolderSolid class="opacity-70" />
                                    <span class="min-w-0 truncate">{basename(&directory).to_owned()}</span>
                                    <span class="shrink-0">
                                        {move || format!("· {}", relative_time(now.get(), last_activity))}
                                    </span>
                                </span>
                            </button>
                        }
                    })
                    .collect_view()
                    .into_any()
            }}
        </nav>
    }
}

#[component]
fn Toolbar(workspace: Workspace) -> impl IntoView {
    let title = move || {
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
                                .map(|session| {
                                    format!(
                                        "{} · {}",
                                        session.creation_selection.agent,
                                        session_label(session)
                                    )
                                })
                        })
                    })
            })
            .unwrap_or_else(|| "No session".into())
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
    let read_only = move || {
        workspace.selected.get().is_some_and(|session_id| {
            !workspace.new_session.get()
                && workspace
                    .read_only
                    .with(|read_only| read_only.contains(&session_id))
        })
    };
    view! {
        <div class="flex flex-wrap items-center gap-x-2 gap-y-1 border-b border-toasted px-3 py-2 dark:border-ember">
            <button
                type="button"
                class=format!("{ICON_BUTTON} md:hidden")
                aria-label="Sessions"
                title="Sessions"
                on:click=move |_| workspace.panel.set(Some(Panel::Sessions))
            >
                <Glyph icon=icondata::FaBarsSolid class="text-lg" />
            </button>
            <div class="flex min-w-0 flex-1 basis-40 items-center gap-2">
                <h1 class="min-w-0 truncate text-lg font-bold" id="session-title" title=title>
                    {title}
                </h1>
                <Show when=read_only>
                    <span class="inline-flex shrink-0 items-center gap-1 rounded-full bg-toasted px-2 py-1 text-xs dark:bg-ember">
                        <Glyph icon=icondata::FaLockSolid />
                        "read-only snapshot"
                    </span>
                </Show>
            </div>
            <div class="flex min-w-0 items-center gap-2">
                <button
                    type="button"
                    class=CHIP
                    title=move || format!("Agent: {} (click to change)", agent())
                    on:click=move |_| workspace.panel.set(Some(Panel::Agents))
                >
                    <Glyph icon=icondata::FaRobotSolid class="text-caramel dark:text-maple" />
                    <span class="sr-only">"Agent: "</span>
                    <span class="min-w-0 truncate">{agent}</span>
                    <Glyph icon=icondata::FaChevronDownSolid class="text-[0.625rem] opacity-60" />
                </button>
                <button
                    type="button"
                    class=CHIP
                    title=move || format!("Model: {} (click to change)", model())
                    on:click=move |_| workspace.panel.set(Some(Panel::Models))
                >
                    <Glyph icon=icondata::FaMicrochipSolid class="text-caramel dark:text-maple" />
                    <span class="sr-only">"Model: "</span>
                    <span class="min-w-0 truncate">{model}</span>
                    <Glyph icon=icondata::FaChevronDownSolid class="text-[0.625rem] opacity-60" />
                </button>
                <button
                    type="button"
                    class=ICON_BUTTON
                    aria-label="Commands"
                    title="Commands"
                    on:click=move |_| workspace.panel.set(Some(Panel::Commands))
                >
                    <Glyph icon=icondata::FaEllipsisVerticalSolid class="text-lg" />
                </button>
            </div>
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
        let expanded = workspace.expanded.get();
        let names = workspace.with_runtime(|runtime| runtime.model_names().clone());
        workspace.with_store(|store| {
            let Some(state) = store.sessions.get(&session_id) else {
                return Vec::new();
            };
            let view = TranscriptView {
                session_id,
                state,
                names: &names,
                minimum_level: level,
                expanded: &expanded,
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
                    workspace.expanded.update(|expanded| {
                        if !expanded.remove(&call) {
                            expanded.insert(call);
                        }
                    });
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
                    workspace.expanded.update(|expanded| {
                        if !expanded.remove(&key) {
                            expanded.insert(key);
                        }
                    });
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
                "The first message creates the session with the agent and model above."
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

#[component]
fn Composer(workspace: Workspace) -> impl IntoView {
    let textarea = NodeRef::<html::Textarea>::new();
    let long_pressed = StoredValue::new(false);
    let press_timer = StoredValue::new(None::<TimeoutHandle>);
    let menu_open = RwSignal::new(false);
    let completions = move || completions(&workspace.composer.get());
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
    view! {
        <div class="relative border-t border-toasted px-3 py-2 dark:border-ember">
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
            // On a phone the buttons go below a text field too narrow to
            // write in.
            <div class="flex flex-wrap items-end gap-2">
                <textarea
                    node_ref=textarea
                    id="composer"
                    rows="2"
                    class="min-h-11 min-w-[12rem] flex-1 resize-y rounded-md border border-crust bg-cream px-3 py-2 font-mono text-sm hover:border-caramel focus:border-caramel focus:outline-none disabled:opacity-60 dark:border-smoke dark:bg-night dark:hover:border-maple dark:focus:border-maple"
                    placeholder="Message · / for commands"
                    disabled=move || !workspace.writable()
                    prop:value=move || workspace.composer.get()
                    on:input=move |event| workspace.composer.set(event_target_value(&event))
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
                <div class="ml-auto flex items-end gap-1">
                <AttachButtons workspace=workspace />
                <div class="relative flex">
                    <button
                        type="button"
                        id="send"
                        aria-label="Send"
                        title="Send (Enter); hold to queue behind the run"
                        class="inline-flex min-h-11 min-w-12 items-center justify-center rounded-l-md bg-espresso px-3 text-cream hover:bg-caramel active:bg-ink disabled:cursor-not-allowed disabled:opacity-50 disabled:hover:bg-espresso dark:bg-latte dark:text-night dark:hover:bg-maple dark:active:bg-sand dark:disabled:hover:bg-latte"
                        disabled=move || !workspace.writable()
                        on:pointerdown=move |_| {
                            long_pressed.set_value(false);
                            let handle = set_timeout_with_handle(
                                move || {
                                    long_pressed.set_value(true);
                                    workspace.submit(InputDelivery::Queue);
                                },
                                LONG_PRESS,
                            )
                            .ok();
                            press_timer.set_value(handle);
                        }
                        on:pointerup=move |_| {
                            if let Some(handle) = press_timer.get_value() {
                                handle.clear();
                            }
                        }
                        on:pointerleave=move |_| {
                            if let Some(handle) = press_timer.get_value() {
                                handle.clear();
                            }
                        }
                        on:click=move |_| {
                            if !long_pressed.get_value() {
                                workspace.submit(InputDelivery::Steer);
                            }
                            long_pressed.set_value(false);
                        }
                    >
                        <Glyph icon=icondata::FaPaperPlaneSolid class="text-base" />
                    </button>
                    <button
                        type="button"
                        aria-label="More ways to send"
                        title="More ways to send"
                        aria-haspopup="menu"
                        aria-expanded=move || menu_open.get().to_string()
                        class="inline-flex min-h-11 min-w-11 items-center justify-center rounded-r-md border-l border-cream/40 bg-espresso text-cream hover:bg-caramel active:bg-ink dark:border-night/40 dark:bg-latte dark:text-night dark:hover:bg-maple dark:active:bg-sand"
                        on:click=move |_| menu_open.update(|open| *open = !*open)
                    >
                        <Glyph
                            icon=Signal::derive(move || {
                                if menu_open.get() { icondata::FaChevronDownSolid } else { icondata::FaChevronUpSolid }
                            })
                            class="text-xs"
                        />
                    </button>
                    <Show when=move || menu_open.get()>
                        <div class="fixed inset-0 z-10" on:click=move |_| menu_open.set(false)></div>
                        <div role="menu" class="absolute right-0 bottom-full z-20 mb-1 flex w-60 flex-col rounded-lg border border-crust bg-cream p-1 shadow-lg dark:border-smoke dark:bg-night">
                            <button
                                type="button"
                                id="queue-send"
                                role="menuitem"
                                class=ROW
                                on:click=move |_| {
                                    menu_open.set(false);
                                    workspace.submit(InputDelivery::Queue);
                                }
                            >
                                <span class="flex w-full items-center gap-2 text-sm">
                                    <Glyph icon=icondata::FaClockSolid />
                                    <span class="flex-1">"Queue behind the run"</span>
                                    <kbd class="font-mono text-xs text-ink dark:text-sand">"Alt+Enter"</kbd>
                                </span>
                            </button>
                            <button
                                type="button"
                                id="cancel-run"
                                role="menuitem"
                                class=ROW
                                on:click=move |_| {
                                    menu_open.set(false);
                                    workspace.cancel();
                                }
                            >
                                <span class="flex w-full items-center gap-2 text-sm">
                                    <Glyph icon=icondata::FaCircleStopSolid />
                                    <span class="flex-1">"Cancel the run"</span>
                                    <kbd class="font-mono text-xs text-ink dark:text-sand">"Esc"</kbd>
                                </span>
                            </button>
                        </div>
                    </Show>
                </div>
                </div>
            </div>
        </div>
    }
}

#[component]
fn StatusBar(workspace: Workspace) -> impl IntoView {
    let running = move || {
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
    };
    let cost = move || {
        workspace
            .selected
            .get()
            .and_then(|session_id| {
                workspace
                    .costs
                    .with(|costs| costs.get(&session_id).copied())
            })
            .map(|cost| format!("${cost:.4}"))
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
    let approvals = Memo::new(move |_| workspace.approvals().len());
    let mode = move || workspace.permission_mode();
    view! {
        <div id="status-bar" class="flex flex-wrap items-center gap-x-1 border-t border-toasted bg-parchment px-2 text-xs dark:border-ember dark:bg-char">
            <button
                type="button"
                id="permission-mode"
                class=STATUS_BUTTON
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
                <span class="font-mono">{move || permission_mode_label(mode())}</span>
            </button>
            <Show when=move || { approvals.get() > 0 }>
                <button
                    type="button"
                    id="approvals-badge"
                    class="inline-flex min-h-11 min-w-11 items-center justify-center gap-1.5 rounded-full bg-caramel px-3 font-bold text-cream hover:bg-espresso active:bg-ink dark:bg-maple dark:text-night dark:hover:bg-latte dark:active:bg-sand"
                    title="Approvals waiting: click to answer"
                    on:click=move |_| workspace.panel.set(Some(Panel::Approvals))
                >
                    <Glyph icon=icondata::FaBellSolid />
                    {move || match approvals.get() {
                        1 => "1 approval waiting".to_owned(),
                        count => format!("{count} approvals waiting"),
                    }}
                </button>
            </Show>
            <Show when=running>
                <span class="inline-flex min-h-11 items-center gap-1.5 px-2 font-bold text-caramel dark:text-maple" role="status">
                    <Glyph icon=icondata::FaCircleNotchSolid class="animate-spin" />
                    "working…"
                </span>
            </Show>
            <span
                class="min-w-[8rem] flex-1 truncate px-2 text-ink dark:text-sand"
                id="status"
                role="status"
                title=move || workspace.status.get()
            >
                {move || workspace.status.get()}
            </span>
            {move || context().map(|tokens| view! {
                <span
                    class="inline-flex min-h-11 items-center gap-1.5 px-2 text-ink dark:text-sand"
                    title=format!("Context: {tokens} tokens in the last request")
                >
                    <Glyph icon=icondata::FaGaugeSimpleHighSolid />
                    <span class="font-mono">{format!("{} tokens", compact_count(tokens))}</span>
                </span>
            })}
            {move || cost().map(|cost| view! {
                <button
                    type="button"
                    class=STATUS_BUTTON
                    title="Session cost: click for usage"
                    on:click=move |_| workspace.run_command(Command::Usage, String::new())
                >
                    <Glyph icon=icondata::FaCoinsSolid class="text-caramel dark:text-maple" />
                    <span class="font-mono">{cost}</span>
                </button>
            })}
        </div>
    }
}

#[component]
fn Sheet(workspace: Workspace) -> impl IntoView {
    let close = move |_| workspace.panel.set(None);
    let title = move || match workspace.panel.get() {
        Some(Panel::Sessions) => "Sessions",
        Some(Panel::Agents) => "Agent",
        Some(Panel::Models) => "Model",
        Some(Panel::Presets) => "Preset",
        Some(Panel::Approvals) => "Approvals",
        Some(Panel::Usage) => "Usage",
        Some(Panel::Events) => "Events",
        Some(Panel::Commands) => "Commands",
        Some(Panel::Goal) => "Goal",
        Some(Panel::Background) => "Background calls",
        Some(Panel::Skills) => "Skills",
        Some(Panel::Permissions) => "Permissions",
        Some(Panel::Message) => "Message",
        Some(Panel::Runtimes) => "Runtimes",
        Some(Panel::Mcp) => "MCP servers",
        Some(Panel::Connect) => "Providers",
        Some(Panel::Config) => "Configuration",
        Some(Panel::Memory) => "Memory",
        Some(Panel::Plugins) => "Plugins",
        None => "",
    };
    view! {
        <Show when=move || workspace.panel.get().is_some()>
            <div class="fixed inset-0 z-10 bg-espresso/30 md:bg-espresso/10 dark:bg-night/60 md:dark:bg-night/30" on:click=close></div>
            <section
                id="sheet"
                class=move || {
                    // The editors get the room a document needs.
                    if matches!(workspace.panel.get(), Some(Panel::Config | Panel::Memory)) {
                        "sheet-frame fixed inset-x-0 top-0 z-20 flex flex-col overflow-y-auto bg-cream md:inset-y-0 md:right-0 md:left-auto md:w-[48rem] md:border-l md:border-toasted md:shadow-xl dark:bg-night md:dark:border-ember"
                    } else {
                        "sheet-frame fixed inset-x-0 top-0 z-20 flex flex-col overflow-y-auto bg-cream md:inset-y-0 md:right-0 md:left-auto md:w-[28rem] md:border-l md:border-toasted md:shadow-xl dark:bg-night md:dark:border-ember"
                    }
                }
            >
                <div class="sticky top-0 z-10 flex items-center gap-2 border-b border-toasted bg-parchment py-1 pr-1 pl-4 dark:border-ember dark:bg-char">
                    <h2 class="flex-1 text-lg font-bold">{title}</h2>
                    <button type="button" class=ICON_BUTTON aria-label="Close" title="Close" on:click=close>
                        <Glyph icon=icondata::FaXmarkSolid class="text-lg" />
                    </button>
                </div>
                <div class="flex flex-col gap-2 p-3">
                    {move || match workspace.panel.get() {
                        // The list keeps its own padding, as in the sidebar.
                        Some(Panel::Sessions) => view! {
                            <div class="-m-3 flex flex-col">
                                <TreePanel workspace=workspace />
                                <SessionList workspace=workspace />
                            </div>
                        }
                            .into_any(),
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
                        Some(Panel::Mcp) => mcp_sheet(workspace).into_any(),
                        Some(Panel::Connect) => connect_sheet(workspace).into_any(),
                        Some(Panel::Config) => config_sheet(workspace).into_any(),
                        Some(Panel::Memory) => memory_sheet(workspace).into_any(),
                        Some(Panel::Plugins) => plugins_sheet(workspace).into_any(),
                        None => ().into_any(),
                    }}
                </div>
            </section>
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
            view! {
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

fn commands_sheet(workspace: Workspace) -> impl IntoView {
    COMMANDS
        .iter()
        .map(|spec| {
            let command = spec.command;
            view! {
                <button
                    type="button"
                    class=ROW
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
