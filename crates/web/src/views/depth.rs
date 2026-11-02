//! The session-depth parts of the page: the goal and
//! background bars, the draft's files and the ways to attach them, and the
//! goal, background, skills, permissions, and message sheets.

use std::time::Duration;

use cookie_agent_client_state::{
    attachment_chip,
    format::format_clock_elapsed,
    permissions::{PermissionRow, parse_rule_key, rule_runtimes, takes_runtime},
};
use leptos::{ev, prelude::*};

use super::{
    BUTTON, EFFECTS, GoalCommand, ICON_BUTTON, NOTE, PRIMARY, PermissionEffect,
    PermissionRuleSource, ROW, action_label, effect_label, empty_state, goal_actions, loading,
    permission_actions, permission_rows, source_label, status_name,
};
use crate::{
    attach::{accept_attribute, accepts_images, camera_accept_attribute},
    files::{CAMERA_INPUT, FILE_INPUT, files_of, pick_files},
    icons::Glyph,
    workspace::{RuleRef, Workspace},
};

/// The attach and camera buttons, round inside the composer's box: 36 px
/// to see, 44 px to hit.
const ATTACH_BUTTON: &str = "hit-area inline-flex h-9 w-9 shrink-0 items-center justify-center rounded-full text-muted hover:bg-hover hover:text-fg active:bg-press disabled:cursor-not-allowed disabled:opacity-40";
const BAR: &str = "flex flex-wrap items-center gap-2 border-t border-line px-3 py-1 text-sm";
pub(super) const FIELD: &str = "min-h-11 rounded-lg bg-surface px-3 font-mono text-[0.8125rem] shadow-[inset_0_0_0_1px_var(--color-line-2)] transition-shadow placeholder:text-faint hover:shadow-[inset_0_0_0_1px_var(--color-faint)] focus:shadow-[inset_0_0_0_1px_var(--color-accent),0_0_0_3px_var(--color-accent-soft)] focus:outline-none lg:min-h-9";

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
                        <div id="downscale-offer" class=format!("{BAR} bg-surface-2")>
                            <Glyph icon=icondata::LuTriangleAlert class="text-accent" />
                            <span class="min-w-0 flex-1">
                                {format!("{}. Downscale and attach?", offer.reason.trim_end_matches('.'))}
                            </span>
                            <button
                                type="button"
                                id="downscale-accept"
                                class=PRIMARY
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
            <div id="draft-files" class="flex flex-wrap gap-1 border-t border-line px-3 py-1">
                {move || {
                    workspace
                        .attachments
                        .get()
                        .into_iter()
                        .map(|artifact| {
                            let sha256 = artifact.sha256.clone();
                            view! {
                                <span class="attachment-chip flex items-center gap-1 rounded-full bg-sunken pl-3 font-mono text-xs">
                                    <Glyph icon=icondata::LuPaperclip class="opacity-70" />
                                    {attachment_chip(&artifact)}
                                    <button
                                        type="button"
                                        class="inline-flex min-h-11 min-w-11 items-center justify-center rounded-full hover:bg-press active:bg-press"
                                        aria-label="Remove"
                                        title="Remove"
                                        on:click=move |_| workspace.remove_attachment(&sha256)
                                    >
                                        <Glyph icon=icondata::LuX />
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

/// The attach and camera buttons and their hidden file inputs, once a model
/// is selected: any file, and the camera for a model that takes images.
#[component]
pub fn AttachButtons(workspace: Workspace) -> impl IntoView {
    let media = Memo::new(move |_| {
        workspace
            .accepted_media()
            .map(|accepted| accepted.media)
            .unwrap_or_default()
    });
    let accept = move || accept_attribute(&media.get());
    let camera_accept = move || camera_accept_attribute(&media.get());
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
        <Show when=move || workspace.accepted_media().is_some()>
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
                class=ATTACH_BUTTON
                aria-label="Attach a file"
                title="Attach a file"
                disabled=move || !workspace.writable()
                on:click=move |_| pick_files(false)
            >
                <Glyph icon=icondata::LuPaperclip class="text-base" />
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
                    class=format!("{ATTACH_BUTTON} md:hidden")
                    aria-label="Take a photo"
                    title="Take a photo"
                    disabled=move || !workspace.writable()
                    on:click=move |_| pick_files(true)
                >
                    <Glyph icon=icondata::LuCamera class="text-base" />
                </button>
            </Show>
        </Show>
    }
}

/// Compacting the context: an optional focus, **Compact**, and, while it
/// compacts, the way to cancel it (as the composer's Stop does).
pub fn compact_sheet(workspace: Workspace) -> impl IntoView {
    let focus = RwSignal::new(String::new());
    let started = RwSignal::new(false);
    let compacting = move || workspace.compacting_now();
    view! {
        <p class=NOTE>
            "Replaces the history so far with a summary, so the session has room to go on. A focus says what the summary must keep."
        </p>
        <label class="flex flex-col gap-1 text-sm">
            <span class="font-bold">"Focus " <span class="font-normal text-muted">"(optional)"</span></span>
            <textarea
                id="compact-focus"
                rows="2"
                class=FIELD
                placeholder="e.g. keep the API decisions"
                prop:value=move || focus.get()
                on:input=move |event| focus.set(event_target_value(&event))
            ></textarea>
        </label>
        <div class="flex flex-wrap items-center gap-2">
            <button
                type="button"
                id="compact-run"
                class=PRIMARY
                disabled=move || compacting() || !workspace.writable()
                on:click=move |_| {
                    started.set(true);
                    workspace.compact(focus.get_untracked());
                }
            >
                <Glyph icon=icondata::LuMinimize2 />
                "Compact"
            </button>
            <Show when=compacting>
                <span class="inline-flex min-h-11 items-center gap-2 text-sm" role="status">
                    <Glyph icon=icondata::LuLoaderCircle class="animate-spin text-accent" />
                    "Compacting…"
                </span>
                <button
                    type="button"
                    id="compact-cancel"
                    class=BUTTON
                    on:click=move |_| workspace.cancel_compaction()
                >
                    "Cancel compaction"
                </button>
            </Show>
        </div>
        <Show when=move || started.get() && !compacting()>
            <p id="compact-outcome" class="text-sm" role="status">{move || workspace.status.get()}</p>
        </Show>
    }
}

/// The selected session's deferred tools prompt, as the model reads it.
pub fn deferred_prompt_sheet(workspace: Workspace) -> impl IntoView {
    move || {
        let prompt = workspace.selected.get().and_then(|session_id| {
            workspace.with_store(|store| {
                store
                    .sessions
                    .get(&session_id)
                    .and_then(cookie_agent_client_state::rows::deferred_tools_prompt)
            })
        });
        match prompt {
            Some(prompt) => view! {
                <pre
                    class="max-h-[70vh] overflow-auto whitespace-pre-wrap break-words rounded-md bg-surface-2 p-2 font-mono text-xs"
                    data-deferred-prompt
                >
                    {prompt}
                </pre>
            }
            .into_any(),
            None => view! { <p class=NOTE>"Nothing is deferred in this session."</p> }.into_any(),
        }
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
                        view! {
                            <li class="flex items-start gap-2">
                                <Glyph
                                    icon=if item.finished { icondata::LuSquareCheck } else { icondata::LuSquare }
                                    class="mt-0.5"
                                />
                                <span class="sr-only">{if item.finished { "finished: " } else { "open: " }}</span>
                                <span>{item.description.clone()}</span>
                            </li>
                        }
                    })
                    .collect_view();
                view! {
                    <p class="flex items-center gap-2 text-sm font-bold">
                        <Glyph icon=icondata::LuTarget class="text-accent" />
                        {format!("Goal ({})", status_name(goal.status))}
                    </p>
                    <p id="goal-objective" class="text-sm whitespace-pre-wrap">{goal.objective.clone()}</p>
                    <ul class="flex list-none flex-col gap-1 text-xs">{items}</ul>
                }
                    .into_any()
            }
            None => empty_state(icondata::LuTarget, "No goal is set.").into_any(),
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
                            data-goal-action=action_label(action).to_lowercase()
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
                    class=PRIMARY
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
                <p class=NOTE>"Goals belong to root sessions."</p>
            </Show>
        }
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
            return empty_state(
                icondata::LuHourglass,
                "No calls are running in the background.",
            )
            .into_any();
        }
        calls
            .into_iter()
            .map(|call| {
                let job = call.job.clone();
                let elapsed = u64::try_from(now.get().duration_since(call.detached_at).as_secs()).unwrap_or(0);
                view! {
                    <div class="background-call flex items-center gap-2 border-b border-line py-2">
                        <div class="min-w-0 flex-1">
                            <p class="truncate font-mono text-sm">{format!("{} · {}", call.job, call.tool)}</p>
                            <p class=format!("truncate {NOTE}")>
                                {call.display.clone().unwrap_or_default()}
                                {format!(" · {}", format_clock_elapsed(elapsed))}
                            </p>
                        </div>
                        // An ACP agent's own command: the agent runs it, and
                        // only the agent can stop it.
                        {if call.agent_task {
                            view! {
                                <span class=NOTE data-agent-task=call.job.clone()>"run by the agent"</span>
                            }
                            .into_any()
                        } else {
                            view! {
                                <button
                                    type="button"
                                    class=BUTTON
                                    data-stop=call.job.clone()
                                    on:click=move |_| workspace.stop_background(job.clone())
                                >
                                    <Glyph icon=icondata::LuCircleStop />
                                    "Stop"
                                </button>
                            }
                            .into_any()
                        }}
                    </div>
                }
            })
            .collect_view()
            .into_any()
    }
}

pub fn skills_sheet(workspace: Workspace) -> impl IntoView {
    let list = move || match workspace.skills.get() {
        None => loading("Reading skills…").into_any(),
        Some(Err(error)) => view! { <p class=super::ERROR>{error}</p> }.into_any(),
        Some(Ok(skills)) => {
            let query = workspace.skill_query.get();
            let shown = crate::skills::matching_skills(&skills, &query)
                .into_iter()
                .cloned()
                .collect::<Vec<_>>();
            if shown.is_empty() {
                return empty_state(icondata::LuWandSparkles, "No matching skill.").into_any();
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
                            class:bg-sel=current.clone()
                            data-skill=skill.name.clone()
                            on:click=move |_| workspace.skill_pick.set(Some(picked.clone()))
                        >
                            <span class="font-mono text-sm font-bold">{skill.name.clone()}</span>
                            {(skill.source == cookie_agent_protocol::SkillSource::Official)
                                .then(|| {
                                    view! {
                                        <span class="ml-2 text-xs" data-official-skill="">
                                            {cookie_agent_client_state::onboarding::skill_origin(&skill)}
                                        </span>
                                    }
                                })}
                            <span class=NOTE>{skill.description.clone()}</span>
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
                        <div class="mt-3 flex flex-col gap-2 border-t border-line pt-3">
                            <p class="font-mono text-sm font-bold">{skill.name.clone()}</p>
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
                            <button type="button" id="skill-run" class=PRIMARY on:click=move |_| workspace.run_skill()>
                                <Glyph icon=icondata::LuPlay />
                                "Run the skill"
                            </button>
                        </div>
                    }
                })
        }}
    }
}

/// Why the runtime field is off for an action that takes no runtime.
const NO_RUNTIME: &str =
    "message and skill run in the daemon itself, on no runtime, so their rules name none";

/// The new-rule form's fields, which an edited rule fills.
#[derive(Clone, Copy)]
struct RuleForm {
    action: RwSignal<String>,
    runtime: RwSignal<String>,
    pattern: RwSignal<String>,
    effect: RwSignal<PermissionEffect>,
    /// The session rule being edited, which the form's rule replaces.
    editing: RwSignal<Option<RuleRef>>,
}

impl RuleForm {
    fn new() -> Self {
        Self {
            action: RwSignal::new(String::new()),
            runtime: RwSignal::new(String::new()),
            pattern: RwSignal::new(String::new()),
            effect: RwSignal::new(PermissionEffect::Ask),
            editing: RwSignal::new(None),
        }
    }

    fn edit(self, row: &PermissionRow) {
        self.action.set(row.action.to_string());
        self.runtime.set(
            row.runtime
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_default(),
        );
        self.pattern.set(row.resource.clone());
        self.effect.set(row.effect);
        self.editing.set(RuleRef::of(row));
    }

    fn reset(self) {
        self.action.set(String::new());
        self.runtime.set(String::new());
        self.pattern.set(String::new());
        self.effect.set(PermissionEffect::Ask);
        self.editing.set(None);
    }

    /// Why the typed key is refused; none while the action is empty.
    fn key_error(self) -> Option<String> {
        let action = self.action.get();
        if action.trim().is_empty() {
            return None;
        }
        parse_rule_key(&action, &self.runtime.get()).err()
    }

    fn submit(self, workspace: Workspace) {
        let (action, runtime) =
            match parse_rule_key(&self.action.get_untracked(), &self.runtime.get_untracked()) {
                Ok(key) => key,
                Err(error) => {
                    workspace.status.set(error);
                    return;
                }
            };
        let pattern = self.pattern.get_untracked();
        if pattern.trim().is_empty() {
            workspace.status.set("a rule needs a pattern".into());
            return;
        }
        let replacing = self.editing.get_untracked();
        let effect = self.effect.get_untracked();
        self.reset();
        workspace.set_permission(action, runtime, &pattern, effect, replacing);
    }
}

/// A rule's key as the table shows it: the action, then `@runtime` when the
/// rule names one.
fn rule_key_view(row: &PermissionRow) -> impl IntoView + use<> {
    let runtime = row.runtime.as_ref().map(|runtime| {
        view! { <span class="text-muted" data-rule-runtime>{format!("@{runtime}")}</span> }
    });
    view! {
        <span class="font-bold">{row.action.to_string()}</span>
        {runtime}
    }
}

pub fn permissions_sheet(workspace: Workspace) -> impl IntoView {
    let form = RuleForm::new();
    let table = move || match workspace.permissions.get() {
        None => loading("Reading permissions…").into_any(),
        Some(Err(error)) => view! { <p class=super::ERROR>{error}</p> }.into_any(),
        Some(Ok(permissions)) => {
            let editable = workspace.writable();
            let rows = permission_rows(&permissions)
                .into_iter()
                .map(|row| {
                    let name = format!("{} {}", row.key(), row.resource);
                    let segments = EFFECTS
                        .iter()
                        .map(|effect| {
                            let effect = *effect;
                            let action = row.action.clone();
                            let runtime = row.runtime.clone();
                            let resource = row.resource.clone();
                            let active = effect == row.effect;
                            view! {
                                <button
                                    type="button"
                                    class="min-h-11 min-w-11 px-2 text-xs hover:bg-hover active:bg-press disabled:cursor-not-allowed disabled:opacity-50"
                                    class:font-bold=active
                                    class:bg-sel=active
                                    aria-pressed=active.to_string()
                                    data-effect=effect_label(effect)
                                    disabled=!editable
                                    on:click=move |_| {
                                        if !active {
                                            workspace.set_permission(
                                                action.clone(),
                                                runtime.clone(),
                                                &resource,
                                                effect,
                                                None,
                                            );
                                        }
                                    }
                                >
                                    {effect_label(effect)}
                                </button>
                            }
                        })
                        .collect_view();
                    let session_rule = (row.source == PermissionRuleSource::SessionOverlay && editable)
                        .then(|| RuleRef::of(&row))
                        .flatten();
                    let controls = session_rule.map(|rule| {
                        let edited = row.clone();
                        view! {
                            <button
                                type="button"
                                class=ICON_BUTTON
                                aria-label="Edit the override"
                                title="Edit the override"
                                data-edit=name.clone()
                                on:click=move |_| form.edit(&edited)
                            >
                                <Glyph icon=icondata::LuPencil />
                            </button>
                            <button
                                type="button"
                                class=ICON_BUTTON
                                aria-label="Remove the override"
                                title="Remove the override"
                                data-remove=name.clone()
                                on:click=move |_| workspace.clear_permission(rule.clone())
                            >
                                <Glyph icon=icondata::LuX />
                            </button>
                        }
                    });
                    // Rows wrap rather than scroll: the rule's name takes
                    // the room left, and its controls go below it when a
                    // narrow screen or a long name needs.
                    view! {
                        <div
                            class="permission-row flex min-w-0 flex-wrap items-center gap-x-3 gap-y-1 border-b border-line py-2"
                            data-rule=name.clone()
                        >
                            <span class="min-w-0 flex-1 basis-32 font-mono text-xs wrap-anywhere">
                                {rule_key_view(&row)}
                                " "
                                <span>{row.resource.clone()}</span>
                            </span>
                            <span class="flex min-w-0 flex-wrap items-center gap-2">
                                <span class="inline-flex shrink-0 overflow-hidden rounded-md border border-line-2" role="group" aria-label="Effect">
                                    {segments}
                                </span>
                                <span class=format!("min-w-14 wrap-anywhere {NOTE}")>{source_label(row.source)}</span>
                                {controls}
                            </span>
                        </div>
                    }
                })
                .collect_view();
            view! { <div class="flex min-w-0 flex-col">{rows}</div> }.into_any()
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
    let runtimes = move || {
        workspace
            .runtimes
            .with(|listing| listing.as_ref().map(rule_runtimes).unwrap_or_default())
    };
    let takes = move || takes_runtime(&form.action.get());
    view! {
        <Show when=move || workspace.permissions_draft()>
            <p class=NOTE>"Rules for the new session, previewed against its agent; the first message creates the session with them."</p>
        </Show>
        {table}
        <Show when=move || workspace.writable()>
            <div id="permission-form" class="mt-3 flex flex-col gap-2 border-t border-line pt-3">
                <p class="text-sm font-bold">
                    {move || if form.editing.get().is_some() { "Edit the rule" } else { "New rule" }}
                </p>
                <div class="flex min-w-0 items-center gap-2">
                    <input
                        type="text"
                        id="permission-action"
                        class=format!("{FIELD} w-full min-w-0 flex-1")
                        list="permission-action-options"
                        placeholder="action, such as bash"
                        aria-label="Action"
                        autocomplete="off"
                        autocapitalize="off"
                        spellcheck="false"
                        aria-invalid=move || form.key_error().is_some().to_string()
                        prop:value=move || form.action.get()
                        on:input=move |event| {
                            let action = event_target_value(&event);
                            if !takes_runtime(&action) {
                                form.runtime.set(String::new());
                            }
                            form.action.set(action);
                        }
                    />
                    <span class="shrink-0 font-mono text-sm" aria-hidden="true">"@"</span>
                    <input
                        type="text"
                        id="permission-runtime"
                        class=format!("{FIELD} w-full min-w-0 flex-1 disabled:cursor-not-allowed disabled:opacity-50")
                        list="permission-runtime-options"
                        placeholder=move || if takes() { "any runtime" } else { "no runtime" }
                        aria-label="Runtime (empty for every runtime)"
                        title=move || if takes() { "The runtime the rule is limited to; empty for every runtime" } else { NO_RUNTIME }
                        autocomplete="off"
                        autocapitalize="off"
                        spellcheck="false"
                        disabled=move || !takes()
                        prop:value=move || form.runtime.get()
                        on:input=move |event| form.runtime.set(event_target_value(&event))
                    />
                </div>
                <datalist id="permission-action-options">
                    {move || actions().into_iter().map(|action| view! { <option value=action.to_string()></option> }).collect_view()}
                </datalist>
                <datalist id="permission-runtime-options">
                    {move || runtimes().into_iter().map(|runtime| view! { <option value=runtime.to_string()></option> }).collect_view()}
                </datalist>
                {move || form.key_error().map(|error| view! { <p class=NOTE data-key-error>{error}</p> })}
                <input
                    type="text"
                    id="permission-pattern"
                    class=FIELD
                    placeholder="pattern, such as git *"
                    aria-label="Pattern"
                    prop:value=move || form.pattern.get()
                    on:input=move |event| form.pattern.set(event_target_value(&event))
                    on:keydown=move |event: ev::KeyboardEvent| {
                        if event.key() == "Enter" {
                            event.prevent_default();
                            form.submit(workspace);
                        }
                    }
                />
                <div class="inline-flex self-start overflow-hidden rounded-md border border-line-2" role="group" aria-label="Effect">
                    {EFFECTS
                        .iter()
                        .map(|effect| {
                            let effect = *effect;
                            view! {
                                <button
                                    type="button"
                                    class="min-h-11 min-w-11 px-3 text-sm hover:bg-hover active:bg-press"
                                    class:font-bold=move || form.effect.get() == effect
                                    class:bg-sel=move || form.effect.get() == effect
                                    aria-pressed=move || (form.effect.get() == effect).to_string()
                                    data-new-effect=effect_label(effect)
                                    on:click=move |_| form.effect.set(effect)
                                >
                                    {effect_label(effect)}
                                </button>
                            }
                        })
                        .collect_view()}
                </div>
                <div class="flex flex-wrap gap-2">
                    <button
                        type="button"
                        id="permission-add"
                        class=PRIMARY
                        disabled=move || form.key_error().is_some()
                        on:click=move |_| form.submit(workspace)
                    >
                        {move || if form.editing.get().is_some() {
                            view! { <Glyph icon=icondata::LuCheck />"Save the rule" }.into_any()
                        } else {
                            view! { <Glyph icon=icondata::LuPlus />"Add the rule" }.into_any()
                        }}
                    </button>
                    <Show when=move || form.editing.get().is_some()>
                        <button type="button" id="permission-cancel" class=BUTTON on:click=move |_| form.reset()>
                            "Cancel"
                        </button>
                    </Show>
                </div>
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
                <p class=NOTE>
                    "This message and everything after it will be removed from the conversation. Its text goes back into the composer so you can edit and resend. To keep both versions, fork instead."
                </p>
                <div class="flex flex-wrap gap-2">
                    <button type="button" id="revert-confirm" class=PRIMARY on:click=move |_| workspace.revert()>
                        <Glyph icon=icondata::LuRotateCcw />
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
                    <span class="flex items-center gap-2 text-sm font-bold">
                        <Glyph icon=icondata::LuCopy />
                        "Copy"
                    </span>
                    <span class=format!("pl-6 {NOTE}")>"copy the message text"</span>
                </button>
                <Show when=move || writable>
                    <button type="button" id="message-revert" class=ROW on:click=move |_| workspace.confirm_revert(true)>
                        <span class="flex items-center gap-2 text-sm font-bold">
                            <Glyph icon=icondata::LuRotateCcw />
                            "Revert"
                        </span>
                        <span class=format!("pl-6 {NOTE}")>"remove this message and every later turn; edit and resend it"</span>
                    </button>
                    <button type="button" id="message-fork" class=ROW on:click=move |_| workspace.fork()>
                        <span class="flex items-center gap-2 text-sm font-bold">
                            <Glyph icon=icondata::LuGitBranch />
                            "Fork"
                        </span>
                        <span class=format!("pl-6 {NOTE}")>"a new session with the conversation up to this message"</span>
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
