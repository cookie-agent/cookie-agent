//! The Scheduled prompts view (`/cron`, client protocol 1.15): every job
//! with its title, schedule and its plain reading, next fire, target,
//! state, and last outcome; the selected job's details, actions (edit,
//! enable or disable, run now, delete), and history; and the job form,
//! which the ⋮ menu's "Schedule a prompt" opens for the open session.

use std::time::Duration;

use cookie_agent_client_state::{
    cron::{
        CronDraft, CronDraftTarget, delivery_label, detail_lines, fire_outcome, job_title,
        last_outcome, local_minute, parse_max_runs, parse_schedule, schedule_reading,
        schedule_text, state_label, target_label, until,
    },
    session_actions::permission_mode_label,
};
use cookie_agent_protocol::{
    CronFire, CronJob, CronJobId, CronMaxRuns, PermissionMode, ProducerDeliveryMode,
};
use leptos::prelude::*;

use super::{
    BUTTON, ERROR, NOTE, PRIMARY, ROW, empty_state, loading,
    manage::{CONFIRM, EDITOR_ACTIONS, EDITOR_PAGE, FIELD, STATE},
};
use crate::{
    cron::{clock_title, model_pins, session_name, zone_at},
    icons::Glyph,
    modes::MODES,
    workspace::{CronForm, Workspace},
};

/// A select, as the form's fields.
const SELECT: &str = "min-h-11 w-full rounded-md border border-crust bg-cream px-2 text-sm hover:border-caramel focus:border-caramel focus:outline-none dark:border-smoke dark:bg-night dark:hover:border-maple dark:focus:border-maple";

/// A field's label above it.
const LABEL: &str = "flex flex-col gap-1 text-sm";

/// A detail that opens a session.
const LINK: &str = "text-left font-bold text-caramel underline-offset-2 hover:underline focus-visible:underline dark:text-maple";

pub fn cron_page(workspace: Workspace) -> impl IntoView {
    // "in 5 minutes" moves on while the view shows.
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
    let confirm_delete = RwSignal::new(None::<CronJobId>);
    // The form renders once per opening: its fields keep what is typed.
    let form = Memo::new(move |_| workspace.cron_form.get());
    move || {
        if let Some(form) = form.get() {
            return cron_form(workspace, form).into_any();
        }
        let listing = match workspace.crons.get() {
            None => return loading("Reading scheduled prompts…").into_any(),
            Some(Err(error)) => return view! { <p class=ERROR>{error}</p> }.into_any(),
            Some(Ok(listing)) => listing,
        };
        let rows = if listing.jobs.is_empty() {
            empty_state(
                icondata::FaClockRegular,
                "No scheduled prompts. Schedule one here, or from a session's ⋮ menu.",
            )
            .into_any()
        } else {
            listing
                .jobs
                .into_iter()
                .map(|job| job_row(workspace, job, now, confirm_delete))
                .collect_view()
                .into_any()
        };
        view! {
            <div class="flex flex-wrap items-center gap-2">
                <p class=format!("min-w-0 flex-1 {NOTE}")>
                    "Cron times are the daemon host's local time; the times shown here are this browser's."
                </p>
                <button type="button" id="cron-new" class=PRIMARY on:click=move |_| workspace.new_cron_job()>
                    <Glyph icon=icondata::FaPlusSolid />
                    "New"
                </button>
            </div>
            {(!listing.firing).then(|| view! {
                <p class=ERROR id="cron-firing-off">
                    "Firing is off ([cron] enabled = false): jobs stay, and each fire is recorded as skipped."
                </p>
            })}
            {move || workspace.cron_error.get().map(|error| view! { <p class=ERROR id="cron-error">{error}</p> })}
            {rows}
        }
        .into_any()
    }
}

/// A job's row: a click shows or hides its details.
fn job_row(
    workspace: Workspace,
    job: CronJob,
    now: RwSignal<jiff::Timestamp>,
    confirm_delete: RwSignal<Option<CronJobId>>,
) -> impl IntoView {
    let id = job.id.clone();
    let selected = {
        let id = id.clone();
        Memo::new(move |_| {
            workspace
                .cron_selected
                .with(|selected| selected.as_ref() == Some(&id))
        })
    };
    let zone = zone_at(jiff::Timestamp::now());
    let title = job_title(&job);
    let reading = schedule_reading(&job.schedule, job.max_runs, &zone);
    let schedule = schedule_text(&job.schedule, &zone);
    let target = target_label(&job.target, |session| {
        workspace
            .sessions
            .with_untracked(|sessions| session_name(sessions, session))
    });
    let state = state_label(&job);
    let outcome = last_outcome(&job);
    let next = job.next_fire;
    let enabled = job.enabled;
    let toggle = {
        let id = id.clone();
        move |_| {
            workspace.cron_selected.update(|selected| {
                *selected = if selected.as_ref() == Some(&id) {
                    None
                } else {
                    Some(id.clone())
                };
            });
        }
    };
    let details = {
        let job = job.clone();
        move || {
            selected
                .get()
                .then(|| job_details(workspace, job.clone(), now, confirm_delete))
        }
    };
    view! {
        <div
            class=move || format!(
                "cron-job flex flex-col gap-1 rounded-lg border p-1 {}",
                if selected.get() { "border-caramel dark:border-maple" } else { "border-toasted dark:border-ember" }
            )
            data-cron-job=id.to_string()
            data-title=title.clone()
            data-enabled=enabled.to_string()
        >
            <button
                type="button"
                class=ROW
                aria-expanded=move || selected.get().to_string()
                on:click=toggle
            >
                <span class="flex w-full items-center gap-2">
                    <Glyph
                        icon=if enabled { icondata::FaClockSolid } else { icondata::FaClockRegular }
                        class=if enabled { "text-caramel dark:text-maple" } else { "opacity-50" }
                    />
                    <span class="min-w-0 flex-1 truncate text-sm font-bold" data-cron-title>{title.clone()}</span>
                    <span class=STATE data-cron-state>{state}</span>
                </span>
                <span class=format!("pl-6 {NOTE}") data-cron-reading>
                    <span class="font-mono">{schedule}</span>
                    " · "
                    {reading}
                </span>
                <span class=format!("flex w-full flex-wrap gap-x-3 pl-6 {NOTE}")>
                    <span data-cron-next>
                        {move || match next {
                            Some(next) => format!("next {} ({})", local_minute(next, &zone_at(next)), until(next, now.get())),
                            None => "no next fire".to_owned(),
                        }}
                    </span>
                    <span data-cron-target>{target}</span>
                    <span data-cron-outcome>{format!("last: {outcome}")}</span>
                </span>
            </button>
            {details}
        </div>
    }
}

/// The selected job: its details, actions, and history, newest first. The
/// target and the session that created the job link to their sessions.
fn job_details(
    workspace: Workspace,
    job: CronJob,
    now: RwSignal<jiff::Timestamp>,
    confirm_delete: RwSignal<Option<CronJobId>>,
) -> impl IntoView {
    let zone = zone_at(jiff::Timestamp::now());
    let title = |session| {
        workspace
            .sessions
            .with_untracked(|sessions| session_name(sessions, session))
    };
    let lines = detail_lines(&job, now.get_untracked(), &zone, title)
        .into_iter()
        .map(|detail| {
            let value = match detail.session {
                Some(session) => view! {
                    <button
                        type="button"
                        class=LINK
                        data-cron-session=session.to_string()
                        title="Open this session"
                        on:click=move |_| {
                            workspace.open_view(crate::layout::View::Chat, None);
                            workspace.open_session(session);
                        }
                    >
                        {detail.value}
                    </button>
                }
                .into_any(),
                None => detail.value.into_any(),
            };
            view! {
                <dt class="text-xs font-bold text-ink dark:text-sand">{detail.label}</dt>
                <dd class="min-w-0 text-sm break-words" data-cron-detail=detail.label>{value}</dd>
            }
        })
        .collect_view();
    let prompt = job.prompt.clone();
    let history = if job.history.is_empty() {
        view! { <p class=NOTE>"It has not fired yet."</p> }.into_any()
    } else {
        job.history
            .iter()
            .rev()
            .map(|fire| fire_line(workspace, fire))
            .collect_view()
            .into_any()
    };
    let title = job_title(&job);
    let edited = job.clone();
    let (toggled, enabled) = (job.id.clone(), job.enabled);
    let ran = job.id.clone();
    let asked = job.id.clone();
    let confirming = {
        let id = job.id.clone();
        move || confirm_delete.get().as_ref() == Some(&id)
    };
    let deleted = (job.id.clone(), title.clone());
    view! {
        <div class="flex flex-col gap-3 px-3 pb-2" data-cron-details>
            <dl class="grid grid-cols-[auto_1fr] items-baseline gap-x-3 gap-y-1">{lines}</dl>
            <div class="flex flex-col gap-1">
                <span class="text-xs font-bold text-ink dark:text-sand">"Prompt"</span>
                <p class="rounded-md bg-toasted px-3 py-2 text-sm whitespace-pre-wrap break-words dark:bg-ember" data-cron-prompt>
                    {prompt}
                </p>
            </div>
            <div class="flex flex-wrap gap-2" data-cron-actions>
                <button type="button" class=BUTTON data-cron-action="edit" on:click=move |_| workspace.edit_cron(edited.clone())>
                    <Glyph icon=icondata::FaPenSolid />
                    "Edit"
                </button>
                <button
                    type="button"
                    class=BUTTON
                    data-cron-action=if enabled { "disable" } else { "enable" }
                    on:click=move |_| workspace.set_cron_enabled(toggled.clone(), !enabled)
                >
                    <Glyph icon=if enabled { icondata::FaPauseSolid } else { icondata::FaPlaySolid } />
                    {if enabled { "Disable" } else { "Enable" }}
                </button>
                <button type="button" class=BUTTON data-cron-action="run" on:click=move |_| workspace.run_cron(ran.clone())>
                    <Glyph icon=icondata::FaForwardStepSolid />
                    "Run now"
                </button>
                <button
                    type="button"
                    class=BUTTON
                    data-cron-action="delete"
                    on:click=move |_| confirm_delete.set(Some(asked.clone()))
                >
                    <Glyph icon=icondata::FaTrashCanRegular />
                    "Delete"
                </button>
            </div>
            <Show when=confirming.clone()>
                <div class=CONFIRM role="alertdialog" data-cron-confirm>
                    <p class="text-sm">{format!("Delete {title}? The sessions it started stay.")}</p>
                    <div class="flex gap-2">
                        <button type="button" id="cron-delete-confirm" class=PRIMARY on:click={
                            let deleted = deleted.clone();
                            move |_| {
                                confirm_delete.set(None);
                                workspace.delete_cron(deleted.0.clone(), deleted.1.clone());
                            }
                        }>
                            "Delete"
                        </button>
                        <button type="button" class=BUTTON on:click=move |_| confirm_delete.set(None)>
                            "Keep it"
                        </button>
                    </div>
                </div>
            </Show>
            <div class="flex flex-col gap-1" data-cron-history>
                <span class="text-xs font-bold text-ink dark:text-sand">"History"</span>
                {history}
            </div>
        </div>
    }
}

/// One fire of the history: when, what it did, and the session it went
/// to, which a click opens.
fn fire_line(workspace: Workspace, fire: &CronFire) -> impl IntoView {
    let at = fire.fired_at;
    let when = local_minute(at, &zone_at(at));
    let outcome = fire_outcome(fire);
    let session = fire.session_id;
    view! {
        <div class="flex min-h-11 flex-wrap items-center gap-x-3 text-sm" data-cron-fire>
            <span class="font-mono text-xs">{when}</span>
            <span class="min-w-0 flex-1 break-words" data-cron-fire-outcome>{outcome}</span>
            {session.map(|session| view! {
                <button
                    type="button"
                    class=BUTTON
                    data-cron-fire-session=session.to_string()
                    on:click=move |_| {
                        workspace.open_view(crate::layout::View::Chat, None);
                        workspace.open_session(session);
                    }
                >
                    <Glyph icon=icondata::FaArrowUpRightFromSquareSolid />
                    "Open session"
                </button>
            })}
        </div>
    }
}

/// The job form: a new job, or an edit of one.
fn cron_form(workspace: Workspace, form: CronForm) -> impl IntoView {
    let CronForm {
        editing,
        draft,
        session,
        template,
    } = form;
    let name = RwSignal::new(draft.name.clone());
    let prompt = RwSignal::new(draft.prompt.clone());
    let schedule = RwSignal::new(draft.schedule.clone());
    let max_runs = RwSignal::new(draft.max_runs.clone());
    let delivery = RwSignal::new(draft.delivery);
    let new = RwSignal::new(draft.target.is_new());
    let agent = RwSignal::new(if draft.agent.is_empty() {
        template.agent.to_string()
    } else {
        draft.agent.clone()
    });
    let model = RwSignal::new(draft.model.clone());
    let mode = RwSignal::new(draft.permission_mode);
    let creating = editing.is_none();
    // A plain reading of what is typed, or why it is not a schedule.
    let reading = move || {
        let zone = zone_at(jiff::Timestamp::now());
        match parse_schedule(&schedule.get()) {
            Err(message) => Err(message),
            Ok(parsed) => parse_max_runs(&max_runs.get()).map(|runs| {
                schedule_reading(&parsed, runs.unwrap_or(CronMaxRuns::Count(1)), &zone)
            }),
        }
    };
    let target_note = editing.as_ref().map(|job| {
        target_label(&job.target, |id| {
            workspace
                .sessions
                .with_untracked(|sessions| session_name(sessions, id))
        })
    });
    let this_session = session.map(|(id, subagent)| {
        let kind = if subagent {
            "this subagent"
        } else {
            "this session"
        };
        workspace
            .sessions
            .with_untracked(|sessions| session_name(sessions, id))
            .map_or_else(|| kind.to_owned(), |name| format!("{kind}: {name}"))
    });
    let agents = {
        let mut agents: Vec<String> = untrack(|| workspace.selectable_agents())
            .into_iter()
            .map(|agent| agent.id.to_string())
            .collect();
        if !agents.contains(&agent.get_untracked()) {
            agents.insert(0, agent.get_untracked());
        }
        agents
    };
    let pins = {
        let mut pins = model_pins(&untrack(|| workspace.models()));
        let current = model.get_untracked();
        if !current.is_empty() && !pins.contains(&current) {
            pins.insert(0, current);
        }
        pins
    };
    let build = {
        let draft = draft.clone();
        let template = template.clone();
        move || {
            let mut draft = CronDraft {
                name: name.get_untracked(),
                prompt: prompt.get_untracked(),
                schedule: schedule.get_untracked(),
                max_runs: max_runs.get_untracked(),
                delivery: delivery.get_untracked(),
                permission_mode: mode.get_untracked(),
                model: model.get_untracked(),
                agent: agent.get_untracked(),
                ..draft.clone()
            };
            if creating {
                draft.target = match (new.get_untracked(), session) {
                    (false, Some((session, subagent))) => {
                        CronDraftTarget::Session { session, subagent }
                    }
                    _ => CronDraftTarget::New {
                        template: template.clone(),
                    },
                };
            }
            draft
        }
    };
    let heading = match &editing {
        Some(job) => format!("Edit {}", job_title(job)),
        None => "Schedule a prompt".to_owned(),
    };
    view! {
        <div class=EDITOR_PAGE id="cron-form">
            <h2 class="text-sm font-bold">{heading}</h2>
            <label class=LABEL>
                <span class="font-bold">"Name"</span>
                <input
                    type="text"
                    id="cron-name"
                    class=FIELD
                    maxlength="80"
                    placeholder="optional; lists show the prompt's start without one"
                    prop:value=move || name.get()
                    on:input=move |event| name.set(event_target_value(&event))
                />
            </label>
            <label class=LABEL>
                <span class="font-bold">"Prompt"</span>
                <textarea
                    id="cron-prompt"
                    class="min-h-[8rem] w-full resize-y rounded-md border border-crust bg-cream p-2 text-sm hover:border-caramel focus:border-caramel focus:outline-none dark:border-smoke dark:bg-night dark:hover:border-maple dark:focus:border-maple"
                    prop:value=move || prompt.get()
                    on:input=move |event| prompt.set(event_target_value(&event))
                ></textarea>
            </label>
            <div class="grid gap-2 sm:grid-cols-[2fr_1fr]">
                <label class=LABEL>
                    <span class="font-bold">"Schedule"</span>
                    <input
                        type="text"
                        id="cron-schedule"
                        class=FIELD
                        placeholder="0 3 * * *  or  in 20m"
                        prop:value=move || schedule.get()
                        on:input=move |event| schedule.set(event_target_value(&event))
                    />
                </label>
                <label class=LABEL>
                    <span class="font-bold">"Runs"</span>
                    <input
                        type="text"
                        id="cron-max-runs"
                        class=FIELD
                        placeholder="1"
                        prop:value=move || max_runs.get()
                        on:input=move |event| max_runs.set(event_target_value(&event))
                    />
                </label>
            </div>
            <p class=NOTE id="cron-reading" data-valid=move || reading().is_ok().to_string()>
                {move || {
                    if schedule.with(|text| text.trim().is_empty()) {
                        return "Five cron fields (minute hour day month weekday) in the daemon host's time, or a delay such as in 20m. Runs: a whole number, 0 or less for no limit; 1 when empty.".to_owned();
                    }
                    reading().unwrap_or_else(|message| message)
                }}
            </p>
            {match target_note {
                Some(target) => view! {
                    <p class="text-sm" id="cron-target">{format!("Target: {target}")}</p>
                }
                .into_any(),
                None => view! {
                    <div class="flex flex-col gap-1" role="radiogroup" aria-label="Target">
                        <span class="text-sm font-bold">"Target"</span>
                        <div class="flex flex-wrap gap-2">
                            {this_session.clone().map(|label| view! {
                                <button
                                    type="button"
                                    id="cron-target-session"
                                    role="radio"
                                    class=move || format!("{BUTTON} {}", if new.get() { "" } else { "bg-toasted dark:bg-ember" })
                                    aria-checked=move || (!new.get()).to_string()
                                    on:click=move |_| new.set(false)
                                >
                                    {label}
                                </button>
                            })}
                            <button
                                type="button"
                                id="cron-target-new"
                                role="radio"
                                class=move || format!("{BUTTON} {}", if new.get() { "bg-toasted dark:bg-ember" } else { "" })
                                aria-checked=move || new.get().to_string()
                                on:click=move |_| new.set(true)
                            >
                                "A new session each run"
                            </button>
                        </div>
                    </div>
                }
                .into_any(),
            }}
            <Show when=move || !new.get()>
                <label class=LABEL>
                    <span class="font-bold">"Delivery"</span>
                    <select
                        id="cron-delivery"
                        class=SELECT
                        on:change=move |event| delivery.set(if event_target_value(&event) == "steer" {
                            ProducerDeliveryMode::Steer
                        } else {
                            ProducerDeliveryMode::Queue
                        })
                    >
                        {[ProducerDeliveryMode::Queue, ProducerDeliveryMode::Steer].map(|choice| view! {
                            <option value=delivery_label(choice) selected=move || delivery.get() == choice>
                                {match choice {
                                    ProducerDeliveryMode::Queue => "queue: after the current turn",
                                    ProducerDeliveryMode::Steer => "steer: into the running turn",
                                }}
                            </option>
                        })}
                    </select>
                </label>
            </Show>
            <Show when=move || new.get()>
                <div class="grid gap-2 sm:grid-cols-2">
                    <label class=LABEL>
                        <span class="font-bold">"Agent"</span>
                        <select id="cron-agent" class=SELECT on:change=move |event| agent.set(event_target_value(&event))>
                            {agents.clone().into_iter().map(|id| {
                                let (chosen, value) = (id.clone(), id.clone());
                                view! { <option value=value selected=move || agent.get() == chosen>{id}</option> }
                            }).collect_view()}
                        </select>
                    </label>
                    <label class=LABEL>
                        <span class="font-bold">"Model"</span>
                        <select id="cron-model" class=SELECT on:change=move |event| model.set(event_target_value(&event))>
                            <option value="" selected=move || model.with(String::is_empty)>"the agent's own models"</option>
                            {pins.clone().into_iter().map(|pin| {
                                let (chosen, value) = (pin.clone(), pin.clone());
                                view! { <option value=value selected=move || model.get() == chosen>{pin}</option> }
                            }).collect_view()}
                        </select>
                    </label>
                    <label class=LABEL>
                        <span class="font-bold">"Permission mode"</span>
                        <select
                            id="cron-mode"
                            class=SELECT
                            on:change=move |event| {
                                let value = event_target_value(&event);
                                mode.set(MODES.iter().map(|choice| choice.mode).find(|choice| permission_mode_label(*choice) == value));
                            }
                        >
                            <option value="" selected=move || mode.get().is_none()>
                                {format!("{} (the default)", permission_mode_label(PermissionMode::AutoApproveN))}
                            </option>
                            {MODES.iter().map(|choice| {
                                let value = choice.mode;
                                view! {
                                    <option value=permission_mode_label(value) selected=move || mode.get() == Some(value)>
                                        {format!("{} — {}", permission_mode_label(value), choice.description)}
                                    </option>
                                }
                            }).collect_view()}
                        </select>
                    </label>
                </div>
                <p class=NOTE>"Each run starts a session of this agent; only you can choose its model and permission mode, agents' jobs always start in auto-n."</p>
            </Show>
            {move || workspace.cron_error.get().map(|error| view! { <p class=ERROR id="cron-error">{error}</p> })}
            <div class=EDITOR_ACTIONS>
                <button type="button" id="cron-save" class=PRIMARY on:click=move |_| workspace.save_cron(build())>
                    {if creating { "Schedule" } else { "Save" }}
                </button>
                <button type="button" id="cron-cancel" class=BUTTON on:click=move |_| workspace.close_cron_form()>
                    "Cancel"
                </button>
            </div>
        </div>
    }
}

/// A run session's clock mark in the session list: a click opens the job
/// that started the session, while a press that moves drags the row like
/// the rest of it.
pub fn clock_mark(workspace: Workspace, cron: cookie_agent_protocol::SessionCron) -> impl IntoView {
    let job = cron.job_id.clone();
    let title = {
        let cron = cron.clone();
        move || {
            workspace.crons.with(|crons| {
                let jobs = crons
                    .as_ref()
                    .and_then(|crons| crons.as_ref().ok())
                    .map(|crons| crons.jobs.as_slice())
                    .unwrap_or_default();
                clock_title(&cron, jobs)
            })
        }
    };
    view! {
        <span
            class="hit-area relative inline-flex shrink-0 text-caramel opacity-80 hover:opacity-100 dark:text-maple"
            role="link"
            tabindex="0"
            aria-label=title.clone()
            title=title
            data-session-cron=cron.job_id.to_string()
            on:click=move |event: leptos::ev::MouseEvent| {
                event.stop_propagation();
                workspace.open_cron_job(job.clone());
            }
        >
            <Glyph icon=icondata::FaClockSolid class="text-xs" />
        </span>
    }
}
