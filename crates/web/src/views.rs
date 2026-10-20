//! The connected page: the session sidebar, the transcript, the queue strip,
//! the composer, the status bar, and the side sheets. Wide screens show the
//! sidebar beside the transcript and the sheets on the right; narrow ones
//! show the sidebar and every sheet full screen.

use std::{collections::HashMap, sync::Arc, time::Duration};

use cookie_agent_client_state::{EventLevel, ToolStatus, session_actions::permission_mode_label};
use cookie_agent_protocol::{ApprovalUserDecision, InputDelivery, ModelSelection, SessionMeta};
use leptos::{ev, html, prelude::*};
use wasm_bindgen::JsCast as _;

use crate::{
    commands::{COMMANDS, completions},
    keys::{ComposerAction, KeyPress, composer_action},
    render::{
        blocks::{BlockPlace, display_html},
        transcript::{TranscriptView, item_html, item_key},
    },
    workspace::{Panel, Workspace},
};

/// How long a press on Send must last to queue instead of steer.
const LONG_PRESS: Duration = Duration::from_millis(500);

const BUTTON: &str = "min-h-11 rounded border border-crust px-3 py-1 text-sm hover:bg-toasted active:bg-crust dark:border-smoke dark:hover:bg-ember dark:active:bg-smoke";
const ROW: &str = "flex min-h-11 w-full flex-col items-start rounded px-3 py-2 text-left hover:bg-toasted active:bg-crust dark:hover:bg-ember dark:active:bg-smoke";

/// The connected page.
#[component]
pub fn WorkspaceView(workspace: Workspace) -> impl IntoView {
    view! {
        <div class="flex min-h-0 flex-1">
            <aside class="hidden w-72 shrink-0 flex-col border-r border-toasted bg-parchment md:flex dark:border-ember dark:bg-char">
                <SessionList workspace=workspace />
            </aside>
            <main class="flex min-w-0 flex-1 flex-col">
                <Toolbar workspace=workspace />
                <Transcript workspace=workspace />
                <QueueStrip workspace=workspace />
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
    view! {
        <div class="flex flex-col gap-2 p-3">
            <button type="button" class=BUTTON on:click=move |_| workspace.start_new_session()>
                "New session"
            </button>
            <label class="flex flex-col gap-1 text-xs">
                <span>"Directory"</span>
                <select
                    class="min-h-11 rounded border border-crust bg-cream px-2 text-sm hover:border-caramel dark:border-smoke dark:bg-night dark:hover:border-maple"
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
                shown()
                    .into_iter()
                    .map(|session| {
                        let session_id = session.session_id;
                        let current = move || workspace.selected.get() == Some(session_id);
                        view! {
                            <button
                                type="button"
                                class=ROW
                                class:bg-toasted=current
                                data-session=session_id.to_string()
                                on:click=move |_| workspace.open_session(session_id)
                            >
                                <span class="w-full truncate text-sm font-bold">
                                    {session_label(&session)}
                                </span>
                                <span class="w-full truncate font-mono text-xs text-ink dark:text-sand">
                                    {format!("{:?} · {}", session.status, session.cwd_identity)
                                        .to_lowercase()}
                                </span>
                            </button>
                        }
                    })
                    .collect_view()
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
                workspace.sessions.with(|sessions| {
                    sessions
                        .iter()
                        .find(|session| session.session_id == session_id)
                        .map(session_label)
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
        <div class="flex flex-wrap items-center gap-2 border-b border-toasted px-3 py-2 dark:border-ember">
            <button
                type="button"
                class=format!("{BUTTON} md:hidden")
                on:click=move |_| workspace.panel.set(Some(Panel::Sessions))
            >
                "Sessions"
            </button>
            <h1 class="min-w-0 flex-1 truncate font-bold" id="session-title">
                {title}
            </h1>
            <Show when=read_only>
                <span class="rounded bg-toasted px-2 py-1 text-xs dark:bg-ember">"read-only snapshot"</span>
            </Show>
            <button
                type="button"
                class=BUTTON
                on:click=move |_| workspace.panel.set(Some(Panel::Agents))
            >
                {agent}
            </button>
            <button
                type="button"
                class=BUTTON
                on:click=move |_| workspace.panel.set(Some(Panel::Models))
            >
                {model}
            </button>
            <button
                type="button"
                class=BUTTON
                aria-label="Commands"
                on:click=move |_| workspace.panel.set(Some(Panel::Commands))
            >
                "☰"
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
        let expanded = workspace.expanded.get();
        let names = workspace.with_runtime(|runtime| runtime.model_names().clone());
        workspace.with_store(|store| {
            let Some(state) = store.sessions.get(&session_id) else {
                return Vec::new();
            };
            let view = TranscriptView {
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
    let on_scroll = move |_| {
        if let Some(element) = container.get() {
            let element: web_sys::Element = element.into();
            following.set_value(
                element.scroll_height() - element.scroll_top() - element.client_height() < 80,
            );
        }
    };
    view! {
        <div
            node_ref=container
            id="transcript"
            class="min-h-0 flex-1 overflow-y-auto px-3 py-2"
            on:click=on_click
            on:scroll=on_scroll
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
        <div class="mx-auto my-4 flex max-w-xl flex-col gap-2 rounded-lg border border-toasted bg-parchment p-4 dark:border-ember dark:bg-char">
            <h2 class="font-bold">"New session"</h2>
            <label class="flex flex-col gap-1 text-sm">
                <span>"Directory"</span>
                <input
                    type="text"
                    class="min-h-11 rounded border border-crust bg-cream px-3 font-mono hover:border-caramel focus:border-caramel focus:outline-none dark:border-smoke dark:bg-night dark:hover:border-maple dark:focus:border-maple"
                    placeholder="the daemon's own directory"
                    prop:value=move || workspace.new_directory.get()
                    on:input=move |event| workspace.new_directory.set(event_target_value(&event))
                />
            </label>
            <p class="text-xs text-ink dark:text-sand">
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
            <div class="flex max-h-32 flex-col gap-1 overflow-y-auto border-t border-toasted px-3 py-1 dark:border-ember" id="queue">
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
                                        <span class="w-full truncate">{text}</span>
                                    </button>
                                }
                                    .into_any()
                            } else {
                                view! {
                                    <div class="truncate px-3 font-mono text-xs text-ink dark:text-sand">
                                        {text}
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
                <div class="absolute bottom-full left-3 mb-1 flex max-h-64 flex-col overflow-y-auto rounded border border-crust bg-cream shadow dark:border-smoke dark:bg-night">
                    {move || {
                        completions()
                            .into_iter()
                            .map(|spec| {
                                let command = spec.command;
                                view! {
                                    <button
                                        type="button"
                                        class=ROW
                                        on:click=move |_| {
                                            workspace.composer.set(String::new());
                                            workspace.run_command(command);
                                        }
                                    >
                                        <span class="font-mono text-sm">{format!("/{}", spec.name)}</span>
                                        <span class="text-xs text-ink dark:text-sand">{spec.description}</span>
                                    </button>
                                }
                            })
                            .collect_view()
                    }}
                </div>
            </Show>
            <div class="flex items-end gap-2">
                <textarea
                    node_ref=textarea
                    id="composer"
                    rows="2"
                    class="min-h-11 flex-1 resize-y rounded border border-crust bg-cream px-3 py-2 font-mono text-sm hover:border-caramel focus:border-caramel focus:outline-none disabled:opacity-60 dark:border-smoke dark:bg-night dark:hover:border-maple dark:focus:border-maple"
                    placeholder="Message · / for commands"
                    disabled=move || !workspace.writable()
                    prop:value=move || workspace.composer.get()
                    on:input=move |event| workspace.composer.set(event_target_value(&event))
                    on:keydown=on_keydown
                ></textarea>
                <div class="relative flex">
                    <button
                        type="button"
                        id="send"
                        class="min-h-11 rounded-l bg-espresso px-4 font-bold text-cream hover:bg-caramel active:bg-ink disabled:opacity-60 dark:bg-latte dark:text-night dark:hover:bg-maple dark:active:bg-sand"
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
                        "Send"
                    </button>
                    <button
                        type="button"
                        aria-label="More ways to send"
                        class="min-h-11 rounded-r border-l border-cream bg-espresso px-2 text-cream hover:bg-caramel active:bg-ink dark:border-night dark:bg-latte dark:text-night dark:hover:bg-maple dark:active:bg-sand"
                        on:click=move |_| menu_open.update(|open| *open = !*open)
                    >
                        "▾"
                    </button>
                    <Show when=move || menu_open.get()>
                        <div class="absolute right-0 bottom-full mb-1 flex w-48 flex-col rounded border border-crust bg-cream shadow dark:border-smoke dark:bg-night">
                            <button
                                type="button"
                                id="queue-send"
                                class=ROW
                                on:click=move |_| {
                                    menu_open.set(false);
                                    workspace.submit(InputDelivery::Queue);
                                }
                            >
                                "Queue behind the run"
                            </button>
                            <button
                                type="button"
                                id="cancel-run"
                                class=ROW
                                on:click=move |_| {
                                    menu_open.set(false);
                                    workspace.cancel();
                                }
                            >
                                "Cancel the run"
                            </button>
                        </div>
                    </Show>
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
    view! {
        <div id="status-bar" class="flex flex-wrap items-center gap-2 border-t border-toasted px-3 py-1 text-xs dark:border-ember">
            <button
                type="button"
                id="permission-mode"
                class=format!("{BUTTON} font-mono")
                title="Permission mode: click for the next"
                on:click=move |_| workspace.cycle_permission_mode()
            >
                {move || permission_mode_label(workspace.permission_mode())}
            </button>
            <Show when=move || { approvals.get() > 0 }>
                <button
                    type="button"
                    id="approvals-badge"
                    class="min-h-11 rounded bg-caramel px-3 font-bold text-cream hover:bg-espresso active:bg-ink dark:bg-maple dark:text-night dark:hover:bg-latte dark:active:bg-sand"
                    on:click=move |_| workspace.panel.set(Some(Panel::Approvals))
                >
                    {move || format!("{} approval(s) waiting", approvals.get())}
                </button>
            </Show>
            <Show when=running>
                <span class="text-caramel dark:text-maple">"working…"</span>
            </Show>
            {move || cost().map(|cost| view! {
                <button
                    type="button"
                    class=BUTTON
                    on:click=move |_| workspace.run_command(crate::commands::Command::Usage)
                >
                    {cost}
                </button>
            })}
            {move || context().map(|tokens| view! { <span class="font-mono">{format!("ctx {tokens}")}</span> })}
            <span class="min-w-0 flex-1 truncate text-ink dark:text-sand" id="status">
                {move || workspace.status.get()}
            </span>
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
        None => "",
    };
    view! {
        <Show when=move || workspace.panel.get().is_some()>
            <div class="fixed inset-0 z-10 bg-espresso/30 md:bg-transparent" on:click=close></div>
            <section
                id="sheet"
                class="fixed inset-0 z-20 flex flex-col overflow-y-auto bg-cream md:inset-y-0 md:right-0 md:left-auto md:w-[28rem] md:border-l md:border-toasted dark:bg-night md:dark:border-ember"
            >
                <div class="flex items-center gap-2 border-b border-toasted px-3 py-2 dark:border-ember">
                    <h2 class="flex-1 font-bold">{title}</h2>
                    <button type="button" class=BUTTON on:click=close>
                        "Close"
                    </button>
                </div>
                <div class="flex flex-col gap-1 p-3">
                    {move || match workspace.panel.get() {
                        Some(Panel::Sessions) => view! { <SessionList workspace=workspace /> }.into_any(),
                        Some(Panel::Agents) => agents_sheet(workspace).into_any(),
                        Some(Panel::Models) => models_sheet(workspace).into_any(),
                        Some(Panel::Presets) => presets_sheet(workspace).into_any(),
                        Some(Panel::Approvals) => approvals_sheet(workspace).into_any(),
                        Some(Panel::Usage) => usage_sheet(workspace).into_any(),
                        Some(Panel::Events) => events_sheet(workspace).into_any(),
                        Some(Panel::Commands) => commands_sheet(workspace).into_any(),
                        None => ().into_any(),
                    }}
                </div>
            </section>
        </Show>
    }
}

fn agents_sheet(workspace: Workspace) -> impl IntoView {
    workspace
        .selectable_agents()
        .into_iter()
        .map(|agent| {
            let picked = agent.clone();
            view! {
                <button
                    type="button"
                    class=ROW
                    on:click=move |_| {
                        workspace.pick_agent(&picked);
                        workspace.panel.set(None);
                    }
                >
                    <span class="font-mono text-sm font-bold">{agent.id.to_string()}</span>
                    <span class="text-xs text-ink dark:text-sand">{agent.description.clone()}</span>
                </button>
            }
        })
        .collect_view()
}

fn models_sheet(workspace: Workspace) -> impl IntoView {
    workspace
        .models()
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
                    view! {
                        <button
                            type="button"
                            class=BUTTON
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
            view! {
                <div class="flex flex-col gap-1 border-b border-toasted pb-2 dark:border-ember">
                    <button
                        type="button"
                        class=ROW
                        on:click=move |_| {
                            workspace.pick_model(base.clone());
                            workspace.panel.set(None);
                        }
                    >
                        <span class="text-sm font-bold">{model.display_name.clone()}</span>
                        <span class="font-mono text-xs text-ink dark:text-sand">{key.to_string()}</span>
                    </button>
                    <div class="flex flex-wrap gap-1 px-3">{variants}</div>
                </div>
            }
        })
        .collect_view()
}

fn presets_sheet(workspace: Workspace) -> impl IntoView {
    std::iter::once(None)
        .chain(workspace.presets().into_iter().map(Some))
        .map(|preset| {
            let label = preset.clone().unwrap_or_else(|| "default".into());
            view! {
                <button
                    type="button"
                    class=ROW
                    on:click=move |_| {
                        workspace.pick_preset(preset.clone());
                        workspace.panel.set(None);
                    }
                >
                    {label}
                </button>
            }
        })
        .collect_view()
}

fn approvals_sheet(workspace: Workspace) -> impl IntoView {
    move || {
        let approvals = workspace.approvals();
        if approvals.is_empty() {
            return view! { <p class="text-sm">"No approval is waiting."</p> }.into_any();
        }
        approvals
            .into_iter()
            .map(|approval| {
                let (title, preview) = workspace.with_store(|store| {
                    store
                        .sessions
                        .get(&approval.session_id)
                        .and_then(|state| state.tools.get(&approval.tool_call_id))
                        .map(|tool| {
                            (
                                tool.compact_title(),
                                display_html(&tool.presentation.preview, BlockPlace::plain),
                            )
                        })
                        .unwrap_or_else(|| ("a tool call".into(), String::new()))
                });
                let decide = move |decision: ApprovalUserDecision| {
                    let approval = approval.clone();
                    move |_| workspace.respond(approval.clone(), decision)
                };
                view! {
                    <div class="approval flex flex-col gap-2 rounded border border-caramel p-3 dark:border-maple">
                        <p class="font-mono text-sm">{title}</p>
                        <div class="min-w-0" inner_html=preview></div>
                        <div class="flex flex-wrap gap-2">
                            <button type="button" class=BUTTON data-decision="approve_once" on:click=decide(ApprovalUserDecision::ApproveOnce)>
                                "Approve once"
                            </button>
                            <button type="button" class=BUTTON data-decision="approve_tree" on:click=decide(ApprovalUserDecision::ApproveTree)>
                                "Approve for this session tree"
                            </button>
                            <button type="button" class=BUTTON data-decision="reject" on:click=decide(ApprovalUserDecision::Reject)>
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
        None => view! { <p class="text-sm">"Reading usage…"</p> }.into_any(),
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
                    .map(|(name, value)| view! { <dt>{name}</dt><dd class="text-right font-mono">{value}</dd> })
                    .collect_view()
            };
            let session = match &usage.session {
                Ok(rollup) => {
                    view! { <dl class="grid grid-cols-2 gap-x-3 text-sm">{rows(rollup)}</dl> }
                        .into_any()
                }
                Err(error) => view! { <p class="text-sm">{error.clone()}</p> }.into_any(),
            };
            let tree = match &usage.tree {
                Ok((rollup, count)) => view! {
                    <p class="mt-3 text-sm font-bold">{format!("Session tree ({count} sessions)")}</p>
                    <dl class="grid grid-cols-2 gap-x-3 text-sm">{rows(rollup)}</dl>
                }
                    .into_any(),
                Err(error) => view! { <p class="text-sm">{error.clone()}</p> }.into_any(),
            };
            view! {
                <p class="text-sm font-bold">"This session"</p>
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
        view! {
            <button
                type="button"
                class=ROW
                class:font-bold=move || workspace.level.get() == level
                on:click=move |_| workspace.set_level(level)
            >
                {label}
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
                        workspace.run_command(command);
                    }
                >
                    <span class="font-mono text-sm">{format!("/{}", spec.name)}</span>
                    <span class="text-xs text-ink dark:text-sand">{spec.description}</span>
                </button>
            }
        })
        .collect_view()
}
