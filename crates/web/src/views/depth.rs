//! The session-depth parts of the page: the subagent tree, the goal and
//! background bars, the draft's files and the ways to attach them, and the
//! goal, background, skills, permissions, and message sheets.

use std::time::Duration;

use cookie_agent_client_state::attachment_chip;
use leptos::{ev, prelude::*};

use super::{
    BUTTON, Command, EFFECTS, GoalCommand, PermissionAction, PermissionEffect,
    PermissionRuleSource, ROW, action_label, effect_label, goal_actions, permission_actions,
    permission_rows, source_label, status_name,
};
use crate::{
    attach::{accept_attribute, accepts_images},
    files::{CAMERA_INPUT, FILE_INPUT, files_of, pick_files},
    tree::tree_rows,
    workspace::Workspace,
};

const BAR: &str =
    "flex flex-wrap items-center gap-2 border-t border-toasted px-3 py-1 text-sm dark:border-ember";
const FIELD: &str = "min-h-11 rounded border border-crust bg-cream px-3 font-mono text-sm hover:border-caramel focus:border-caramel focus:outline-none dark:border-smoke dark:bg-night dark:hover:border-maple dark:focus:border-maple";

/// The viewed session's tree, folded where the user folded it, shown as
/// `/agent-panel` says.
#[component]
pub fn TreePanel(workspace: Workspace) -> impl IntoView {
    let rows = move || {
        let collapsed = workspace.collapsed.get();
        workspace
            .tree
            .with(|tree| tree.as_ref().map(|tree| tree_rows(tree, &collapsed)))
            .unwrap_or_default()
    };
    view! {
        <Show when=move || workspace.tree_panel_visible()>
            <nav id="agent-tree" class="flex max-h-72 flex-col gap-1 overflow-y-auto border-b border-toasted p-2 dark:border-ember" aria-label="Subagents">
                <p class="px-2 text-xs font-bold">"Agents"</p>
                {move || {
                    rows()
                        .into_iter()
                        .map(|row| {
                            let session_id = row.session_id;
                            let current = move || workspace.selected.get() == Some(session_id);
                            let indent = format!("{}rem", row.depth);
                            let fold = row.has_children.then(|| {
                                view! {
                                    <button
                                        type="button"
                                        class="min-h-11 min-w-11 rounded hover:bg-toasted active:bg-crust dark:hover:bg-ember dark:active:bg-smoke"
                                        aria-label="Fold"
                                        aria-expanded=(!row.collapsed).to_string()
                                        on:click=move |_| workspace.toggle_folded(session_id)
                                    >
                                        {if row.collapsed { "▸" } else { "▾" }}
                                    </button>
                                }
                            });
                            view! {
                                <div class="flex items-center" style:padding-left=indent>
                                    {fold}
                                    <button
                                        type="button"
                                        class=ROW
                                        class:bg-toasted=current
                                        data-tree-session=session_id.to_string()
                                        on:click=move |_| workspace.open_session(session_id)
                                    >
                                        <span class="w-full truncate text-sm">
                                            {if row.running { "● " } else { "" }}
                                            {row.label.clone()}
                                        </span>
                                    </button>
                                </div>
                            }
                        })
                        .collect_view()
                }}
            </nav>
        </Show>
    }
}

/// The root session's goal, with the controls its status allows.
#[component]
pub fn GoalBar(workspace: Workspace) -> impl IntoView {
    let goal = Memo::new(move |_| workspace.goal());
    view! {
        {move || {
            goal.get()
                .map(|goal| {
                    let read_only = !workspace.writable();
                    let mut summary = goal.objective.split_whitespace().collect::<Vec<_>>().join(" ");
                    if matches!(
                        goal.status,
                        cookie_agent_protocol::GoalStatus::Completed
                            | cookie_agent_protocol::GoalStatus::Cancelled
                    ) {
                        summary.push_str(&format!(" · {}", status_name(goal.status)));
                    }
                    let controls = goal_actions(goal.status, read_only)
                        .iter()
                        .map(|action| {
                            let action = *action;
                            view! {
                                <button
                                    type="button"
                                    class=BUTTON
                                    data-goal-action=action_label(action).to_lowercase()
                                    on:click=move |_| {
                                        workspace.goal_command(GoalCommand::Lifecycle(action))
                                    }
                                >
                                    {action_label(action)}
                                </button>
                            }
                        })
                        .collect_view();
                    view! {
                        <div id="goal-bar" class=BAR>
                            <button
                                type="button"
                                class=format!("{ROW} min-w-0 flex-1 justify-center")
                                on:click=move |_| workspace.panel.set(Some(crate::workspace::Panel::Goal))
                            >
                                <span class="w-full truncate">{format!("🎯 {summary}")}</span>
                            </button>
                            {controls}
                        </div>
                    }
                })
        }}
    }
}

/// The calls running in the background, while there are any.
#[component]
pub fn BackgroundBar(workspace: Workspace) -> impl IntoView {
    let calls = Memo::new(move |_| workspace.background_calls());
    view! {
        <Show when=move || !calls.with(Vec::is_empty)>
            <div id="background-bar" class=BAR>
                <button
                    type="button"
                    class=format!("{ROW} min-w-0 flex-1 justify-center")
                    on:click=move |_| workspace.run_command(Command::Background, String::new())
                >
                    <span class="w-full truncate">
                        {move || {
                            let calls = calls.get();
                            let summary = calls
                                .iter()
                                .map(|call| match &call.display {
                                    Some(display) => format!(
                                        "{}: {}",
                                        call.tool,
                                        display.split_whitespace().collect::<Vec<_>>().join(" ")
                                    ),
                                    None => call.tool.clone(),
                                })
                                .collect::<Vec<_>>()
                                .join(" · ");
                            format!("⏳ {} in background: {summary}", calls.len())
                        }}
                    </span>
                </button>
            </div>
        </Show>
    }
}

/// The draft's files as chips, and an offer to downscale an image too
/// large to attach.
#[component]
pub fn DraftFiles(workspace: Workspace) -> impl IntoView {
    view! {
        {move || {
            workspace
                .downscale_offer
                .get()
                .map(|offer| {
                    view! {
                        <div id="downscale-offer" class=format!("{BAR} bg-parchment dark:bg-char")>
                            <span class="min-w-0 flex-1">
                                {format!("{}. Downscale and attach?", offer.reason.trim_end_matches('.'))}
                            </span>
                            <button
                                type="button"
                                id="downscale-accept"
                                class=BUTTON
                                on:click=move |_| workspace.accept_downscale()
                            >
                                "Downscale and attach"
                            </button>
                            <button type="button" class=BUTTON on:click=move |_| workspace.dismiss_downscale()>
                                "Dismiss"
                            </button>
                        </div>
                    }
                })
        }}
        <Show when=move || !workspace.attachments.with(Vec::is_empty)>
            <div id="draft-files" class="flex flex-wrap gap-1 border-t border-toasted px-3 py-1 dark:border-ember">
                {move || {
                    workspace
                        .attachments
                        .get()
                        .into_iter()
                        .map(|artifact| {
                            let sha256 = artifact.sha256.clone();
                            view! {
                                <span class="attachment-chip flex items-center gap-1 rounded bg-toasted pl-2 font-mono text-xs dark:bg-ember">
                                    {attachment_chip(&artifact)}
                                    <button
                                        type="button"
                                        class="min-h-11 min-w-11 rounded hover:bg-crust active:bg-smoke dark:hover:bg-smoke dark:active:bg-night"
                                        aria-label="Remove"
                                        on:click=move |_| workspace.remove_attachment(&sha256)
                                    >
                                        "×"
                                    </button>
                                </span>
                            }
                        })
                        .collect_view()
                }}
            </div>
        </Show>
    }
}

/// The attach and camera buttons and their hidden file inputs, offered
/// only for the media the selected model accepts.
#[component]
pub fn AttachButtons(workspace: Workspace) -> impl IntoView {
    let media = Memo::new(move |_| {
        workspace
            .accepted_media()
            .map(|accepted| accepted.media)
            .unwrap_or_default()
    });
    let accept = move || accept_attribute(&media.get());
    let camera_accept = move || {
        let media = media.get();
        accept_attribute(
            &media
                .into_iter()
                .filter(|entry| entry.kind.as_str() == "image")
                .collect::<Vec<_>>(),
        )
    };
    let on_change = move |event: ev::Event| {
        let Some(input) = event.target().and_then(|target| {
            wasm_bindgen::JsCast::dyn_into::<web_sys::HtmlInputElement>(target).ok()
        }) else {
            return;
        };
        let files = files_of(input.files());
        input.set_value("");
        workspace.attach_files(files);
    };
    view! {
        <Show when=move || !media.with(Vec::is_empty)>
            <input
                type="file"
                id=FILE_INPUT
                class="hidden"
                multiple
                accept=accept
                on:change=on_change
            />
            <button
                type="button"
                id="attach"
                class=format!("{BUTTON} disabled:opacity-60")
                aria-label="Attach a file"
                title="Attach a file"
                disabled=move || !workspace.writable()
                on:click=move |_| pick_files(false)
            >
                "📎"
            </button>
            <Show when=move || accepts_images(&media.get())>
                <input
                    type="file"
                    id=CAMERA_INPUT
                    class="hidden"
                    accept=camera_accept
                    capture="environment"
                    on:change=on_change
                />
                <button
                    type="button"
                    class=format!("{BUTTON} disabled:opacity-60 md:hidden")
                    aria-label="Take a photo"
                    disabled=move || !workspace.writable()
                    on:click=move |_| pick_files(true)
                >
                    "📷"
                </button>
            </Show>
        </Show>
    }
}

pub fn goal_sheet(workspace: Workspace) -> impl IntoView {
    move || {
        let goal = workspace.goal();
        let current = match &goal {
            Some(goal) => {
                let items = goal
                    .items
                    .iter()
                    .map(|item| {
                        let mark = if item.finished { "☑" } else { "☐" };
                        view! { <li>{format!("{mark} {}", item.description)}</li> }
                    })
                    .collect_view();
                view! {
                    <p class="text-sm font-bold">{format!("Goal ({})", status_name(goal.status))}</p>
                    <p id="goal-objective" class="text-sm whitespace-pre-wrap">{goal.objective.clone()}</p>
                    <ul class="list-none text-xs">{items}</ul>
                }
                    .into_any()
            }
            None => view! { <p class="text-sm">"No goal is set."</p> }.into_any(),
        };
        let controls = goal.map(|goal| {
            goal_actions(goal.status, !workspace.writable())
                .iter()
                .map(|action| {
                    let action = *action;
                    view! {
                        <button
                            type="button"
                            class=BUTTON
                            on:click=move |_| workspace.goal_command(GoalCommand::Lifecycle(action))
                        >
                            {action_label(action)}
                        </button>
                    }
                })
                .collect_view()
        });
        view! {
            {current}
            <div class="flex flex-wrap gap-2">{controls}</div>
            <Show when=move || workspace.viewing_root() && workspace.writable()>
                <label class="mt-3 flex flex-col gap-1 text-sm">
                    <span>"New objective"</span>
                    <textarea
                        id="goal-input"
                        rows="3"
                        class=FIELD
                        prop:value=move || workspace.goal_draft.get()
                        on:input=move |event| workspace.goal_draft.set(event_target_value(&event))
                    ></textarea>
                </label>
                <button
                    type="button"
                    id="goal-set"
                    class=BUTTON
                    on:click=move |_| {
                        let objective = workspace.goal_draft.get_untracked();
                        if objective.trim().is_empty() {
                            workspace.status.set("goal objective must not be empty".into());
                            return;
                        }
                        workspace.goal_draft.set(String::new());
                        workspace.panel.set(None);
                        workspace.goal_command(GoalCommand::Objective(objective.trim().to_owned()));
                    }
                >
                    "Set the goal"
                </button>
            </Show>
            <Show when=move || !workspace.viewing_root()>
                <p class="text-xs">"Goals belong to root sessions."</p>
            </Show>
        }
    }
}

/// How long ago a background call was detached, as the TUI says it.
fn elapsed_label(seconds: i64) -> String {
    match seconds {
        ..60 => format!("{}s", seconds.max(0)),
        60..3600 => format!("{}m{:02}s", seconds / 60, seconds % 60),
        _ => format!("{}h{:02}m", seconds / 3600, seconds % 3600 / 60),
    }
}

pub fn background_sheet(workspace: Workspace) -> impl IntoView {
    // The elapsed times tick while the sheet is open.
    let now = RwSignal::new(jiff::Timestamp::now());
    let ticker = set_interval_with_handle(
        move || now.set(jiff::Timestamp::now()),
        Duration::from_secs(1),
    )
    .ok();
    on_cleanup(move || {
        if let Some(ticker) = ticker {
            ticker.clear();
        }
    });
    move || {
        let calls = workspace.background_calls();
        if calls.is_empty() {
            return view! { <p class="text-sm">"No calls are running in the background."</p> }
                .into_any();
        }
        calls
            .into_iter()
            .map(|call| {
                let job = call.job.clone();
                let elapsed = now.get().duration_since(call.detached_at).as_secs();
                view! {
                    <div class="background-call flex items-center gap-2 border-b border-toasted py-2 dark:border-ember">
                        <div class="min-w-0 flex-1">
                            <p class="truncate font-mono text-sm">{format!("{} · {}", call.job, call.tool)}</p>
                            <p class="truncate text-xs">
                                {call.display.clone().unwrap_or_default()}
                                {format!(" · {}", elapsed_label(elapsed))}
                            </p>
                        </div>
                        <button
                            type="button"
                            class=BUTTON
                            data-stop=call.job.clone()
                            on:click=move |_| workspace.stop_background(job.clone())
                        >
                            "Stop"
                        </button>
                    </div>
                }
            })
            .collect_view()
            .into_any()
    }
}

pub fn skills_sheet(workspace: Workspace) -> impl IntoView {
    let list = move || match workspace.skills.get() {
        None => view! { <p class="text-sm">"Reading skills…"</p> }.into_any(),
        Some(Err(error)) => view! { <p class="text-sm">{error}</p> }.into_any(),
        Some(Ok(skills)) => {
            let query = workspace.skill_query.get();
            let shown = crate::skills::matching_skills(&skills, &query)
                .into_iter()
                .cloned()
                .collect::<Vec<_>>();
            if shown.is_empty() {
                return view! { <p class="text-sm">"No matching skill."</p> }.into_any();
            }
            shown
                .into_iter()
                .map(|skill| {
                    let picked = skill.clone();
                    let name = skill.name.clone();
                    let current = move || {
                        workspace
                            .skill_pick
                            .with(|pick| pick.as_ref().is_some_and(|pick| pick.name == name))
                    };
                    view! {
                        <button
                            type="button"
                            class=ROW
                            class:bg-toasted=current
                            data-skill=skill.name.clone()
                            on:click=move |_| workspace.skill_pick.set(Some(picked.clone()))
                        >
                            <span class="font-mono text-sm font-bold">{skill.name.clone()}</span>
                            <span class="text-xs text-ink dark:text-sand">{skill.description.clone()}</span>
                        </button>
                    }
                })
                .collect_view()
                .into_any()
        }
    };
    view! {
        <input
            type="search"
            id="skill-search"
            class=FIELD
            placeholder="Search skills"
            prop:value=move || workspace.skill_query.get()
            on:input=move |event| workspace.skill_query.set(event_target_value(&event))
        />
        {list}
        {move || {
            workspace
                .skill_pick
                .get()
                .map(|skill| {
                    let hint = skill.argument_hint.clone().unwrap_or_else(|| "arguments".into());
                    view! {
                        <div class="mt-3 flex flex-col gap-2 border-t border-toasted pt-3 dark:border-ember">
                            <p class="font-mono text-sm">{format!("/{}", skill.name)}</p>
                            <input
                                type="text"
                                id="skill-args"
                                class=FIELD
                                placeholder=hint
                                prop:value=move || workspace.skill_args.get()
                                on:input=move |event| workspace.skill_args.set(event_target_value(&event))
                                on:keydown=move |event: ev::KeyboardEvent| {
                                    if event.key() == "Enter" {
                                        event.prevent_default();
                                        workspace.run_skill();
                                    }
                                }
                            />
                            <button type="button" id="skill-run" class=BUTTON on:click=move |_| workspace.run_skill()>
                                "Run the skill"
                            </button>
                        </div>
                    }
                })
        }}
    }
}

pub fn permissions_sheet(workspace: Workspace) -> impl IntoView {
    let new_action = RwSignal::new(None::<PermissionAction>);
    let new_pattern = RwSignal::new(String::new());
    let new_effect = RwSignal::new(PermissionEffect::Ask);
    let table = move || match workspace.permissions.get() {
        None => view! { <p class="text-sm">"Reading permissions…"</p> }.into_any(),
        Some(Err(error)) => view! { <p class="text-sm">{error}</p> }.into_any(),
        Some(Ok(permissions)) => {
            let editable = workspace.writable();
            let rows = permission_rows(&permissions)
                .into_iter()
                .map(|row| {
                    let segments = EFFECTS
                        .iter()
                        .map(|effect| {
                            let effect = *effect;
                            let action = row.action.clone();
                            let resource = row.resource.clone();
                            let active = effect == row.effect;
                            view! {
                                <button
                                    type="button"
                                    class="min-h-11 min-w-11 px-2 text-xs hover:bg-toasted active:bg-crust disabled:opacity-60 dark:hover:bg-ember dark:active:bg-smoke"
                                    class:font-bold=active
                                    class:underline=active
                                    aria-pressed=active.to_string()
                                    data-effect=effect_label(effect)
                                    disabled=!editable
                                    on:click=move |_| {
                                        if !active {
                                            workspace.set_permission(action.clone(), &resource, effect);
                                        }
                                    }
                                >
                                    {effect_label(effect)}
                                </button>
                            }
                        })
                        .collect_view();
                    let remove = (row.source == PermissionRuleSource::SessionOverlay && editable).then(|| {
                        let action = row.action.clone();
                        let resource = row.resource.clone();
                        view! {
                            <button
                                type="button"
                                class="min-h-11 min-w-11 rounded text-xs hover:bg-toasted active:bg-crust dark:hover:bg-ember dark:active:bg-smoke"
                                aria-label="Remove the override"
                                data-remove=format!("{} {}", row.action, row.resource)
                                on:click=move |_| workspace.clear_permission(action.clone(), &resource)
                            >
                                "×"
                            </button>
                        }
                    });
                    view! {
                        <tr class="permission-row border-b border-toasted dark:border-ember" data-rule=format!("{} {}", row.action, row.resource)>
                            <td class="py-1 pr-2 font-mono text-xs">{row.action.to_string()}</td>
                            <td class="py-1 pr-2 font-mono text-xs break-all">{row.resource.clone()}</td>
                            <td class="py-1"><div class="flex">{segments}</div></td>
                            <td class="py-1 text-xs">{source_label(row.source)}</td>
                            <td>{remove}</td>
                        </tr>
                    }
                })
                .collect_view();
            view! {
                <table class="w-full text-left">
                    <thead>
                        <tr class="text-xs">
                            <th>"Action"</th>
                            <th>"Pattern"</th>
                            <th>"Effect"</th>
                            <th>"From"</th>
                            <th></th>
                        </tr>
                    </thead>
                    <tbody>{rows}</tbody>
                </table>
            }
            .into_any()
        }
    };
    let actions = move || {
        workspace.permissions.with(|permissions| {
            permission_actions(
                permissions
                    .as_ref()
                    .and_then(|permissions| permissions.as_ref().ok())
                    .map(Vec::as_slice),
            )
        })
    };
    view! {
        <Show when=move || workspace.permissions_draft()>
            <p class="text-xs">"Rules for the new session, previewed against its agent; the first message creates the session with them."</p>
        </Show>
        {table}
        <Show when=move || workspace.writable()>
            <div id="permission-form" class="mt-3 flex flex-col gap-2 border-t border-toasted pt-3 dark:border-ember">
                <p class="text-sm font-bold">"New rule"</p>
                <select
                    id="permission-action"
                    class=FIELD
                    on:change=move |event| {
                        new_action.set(PermissionAction::new(event_target_value(&event)).ok());
                    }
                >
                    {move || {
                        let actions = actions();
                        if new_action.get_untracked().is_none() {
                            new_action.set(actions.first().cloned());
                        }
                        actions
                            .into_iter()
                            .map(|action| {
                                let selected = new_action.get().as_ref() == Some(&action);
                                view! {
                                    <option value=action.to_string() selected=selected>
                                        {action.to_string()}
                                    </option>
                                }
                            })
                            .collect_view()
                    }}
                </select>
                <input
                    type="text"
                    id="permission-pattern"
                    class=FIELD
                    placeholder="pattern, such as git *"
                    prop:value=move || new_pattern.get()
                    on:input=move |event| new_pattern.set(event_target_value(&event))
                />
                <div class="flex">
                    {EFFECTS
                        .iter()
                        .map(|effect| {
                            let effect = *effect;
                            view! {
                                <button
                                    type="button"
                                    class=BUTTON
                                    class:font-bold=move || new_effect.get() == effect
                                    aria-pressed=move || (new_effect.get() == effect).to_string()
                                    data-new-effect=effect_label(effect)
                                    on:click=move |_| new_effect.set(effect)
                                >
                                    {effect_label(effect)}
                                </button>
                            }
                        })
                        .collect_view()}
                </div>
                <button
                    type="button"
                    id="permission-add"
                    class=BUTTON
                    on:click=move |_| {
                        let Some(action) = new_action.get_untracked() else {
                            return;
                        };
                        let pattern = new_pattern.get_untracked();
                        if pattern.trim().is_empty() {
                            workspace.status.set("a rule needs a pattern".into());
                            return;
                        }
                        new_pattern.set(String::new());
                        workspace.set_permission(action, &pattern, new_effect.get_untracked());
                    }
                >
                    "Add the rule"
                </button>
            </div>
        </Show>
    }
}

pub fn message_sheet(workspace: Workspace) -> impl IntoView {
    move || {
        let Some(menu) = workspace.user_menu.get() else {
            return ().into_any();
        };
        let writable = workspace.writable();
        let actions = if menu.confirming_revert {
            view! {
                <p class="text-sm font-bold">"Revert to before this message?"</p>
                <p class="text-xs">
                    "This message and everything after it will be removed from the conversation. Its text goes back into the composer so you can edit and resend. To keep both versions, fork instead."
                </p>
                <div class="flex flex-wrap gap-2">
                    <button type="button" id="revert-confirm" class=BUTTON on:click=move |_| workspace.revert()>
                        "Revert"
                    </button>
                    <button type="button" class=BUTTON on:click=move |_| workspace.confirm_revert(false)>
                        "Back"
                    </button>
                </div>
            }
                .into_any()
        } else {
            view! {
                <button type="button" id="message-copy" class=ROW on:click=move |_| workspace.copy_message()>
                    <span class="text-sm font-bold">"Copy"</span>
                    <span class="text-xs">"copy the message text"</span>
                </button>
                <Show when=move || writable>
                    <button type="button" id="message-revert" class=ROW on:click=move |_| workspace.confirm_revert(true)>
                        <span class="text-sm font-bold">"Revert"</span>
                        <span class="text-xs">"remove this message and every later turn; edit and resend it"</span>
                    </button>
                    <button type="button" id="message-fork" class=ROW on:click=move |_| workspace.fork()>
                        <span class="text-sm font-bold">"Fork"</span>
                        <span class="text-xs">"a new session with the conversation up to this message"</span>
                    </button>
                </Show>
            }
                .into_any()
        };
        view! {
            <textarea id="message-text" readonly rows="4" class=FIELD>{menu.text.clone()}</textarea>
            {actions}
        }
        .into_any()
    }
}
