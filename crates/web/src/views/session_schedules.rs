//! The session panel's Schedules and Webhooks tabs
//! (`docs/site/specs/session-schedules.md`, D2 and D3): the open session's
//! cron jobs and webhooks in three groups (what fires or delivers into it,
//! what an agent made in it, what started it), with their actions, and
//! the buttons that add one for the session.

use cookie_agent_client_state::{
    cron::{actor_label, job_title, schedule_reading, until},
    session_schedules::{SessionGroups, session_jobs, session_webhooks},
};
use cookie_agent_protocol::{CronJob, CronJobId, CronTarget, SessionMeta, WebhookId};
use leptos::prelude::*;

use super::{
    BUTTON, NOTE, PRIMARY,
    manage::CONFIRM,
    row_menu::{MenuItem, row_menu},
};
use crate::{cron::zone_at, icons::Glyph, layout::View, workspace::Workspace};

/// A group's heading.
const GROUP: &str = "pt-1 text-[0.6875rem] font-semibold tracking-wide text-muted uppercase";

/// A job's ⋯, kept clear of nothing: the row has no open button.
const MORE: &str = "inline-flex min-h-11 min-w-11 shrink-0 items-center justify-center rounded-lg text-muted transition-colors hover:bg-hover hover:text-fg active:bg-press lg:min-h-8 lg:min-w-8";

/// The open session's metadata, while one is open.
fn open_session(workspace: Workspace) -> Option<SessionMeta> {
    if workspace.new_session.get() {
        return None;
    }
    let id = workspace.selected.get()?;
    workspace
        .sessions
        .with(|sessions| sessions.iter().find(|meta| meta.session_id == id).cloned())
}

/// A group's heading and rows, nothing when it has none.
fn group<T: Clone + 'static, V: IntoView + 'static>(
    heading: &'static str,
    items: Vec<T>,
    row: impl Fn(T) -> V + 'static,
) -> Option<impl IntoView> {
    (!items.is_empty()).then(|| {
        view! {
            <span class=GROUP>{heading}</span>
            {items.into_iter().map(row).collect_view()}
        }
    })
}

/// The Schedules tab (D2).
pub(super) fn schedules_sheet(workspace: Workspace) -> impl IntoView {
    let confirm = RwSignal::new(None::<CronJobId>);
    let menu = RwSignal::new(None::<CronJobId>);
    let body = move || {
        let Some(session) = open_session(workspace) else {
            return view! { <p class=NOTE>"Open a session to see its schedules."</p> }.into_any();
        };
        let SessionGroups {
            into,
            made_here,
            started_by,
        } = workspace
            .crons
            .with(|_| session_jobs(&session, &workspace.cron_jobs()));
        let empty = into.is_empty() && made_here.is_empty() && started_by.is_none();
        let row = move |job: CronJob| job_row(workspace, job, confirm, menu);
        view! {
            {empty.then(|| view! {
                <p class=NOTE data-session-schedules-empty>"No cron job fires into this session, and none was made here."</p>
            })}
            {group("Fires into this session", into, row)}
            {group("Made here, starting sessions of their own", made_here, row)}
            {group("Started this session", started_by.into_iter().collect(), row)}
        }
        .into_any()
    };
    view! {
        <div class="flex flex-col gap-2" data-session-schedules>
            {body}
            <div class="flex flex-wrap gap-2 pt-1">
                <button type="button" class=PRIMARY data-session-schedule on:click=move |_| workspace.schedule_prompt()>
                    <Glyph icon=icondata::LuPlus />
                    "Schedule a prompt"
                </button>
                <button type="button" class=BUTTON on:click=move |_| workspace.open_view(View::Cron, None)>
                    "Open Schedules"
                </button>
            </div>
        </div>
    }
}

/// A job: its name, when it fires, its next fire, its runs and maker, its
/// switch, and a ⋯ with Run now, Edit, and Delete.
fn job_row(
    workspace: Workspace,
    job: CronJob,
    confirm: RwSignal<Option<CronJobId>>,
    menu: RwSignal<Option<CronJobId>>,
) -> impl IntoView {
    let zone = zone_at(jiff::Timestamp::now());
    let id = job.id.clone();
    let title = job_title(&job);
    let reading = schedule_reading(&job.schedule, job.max_runs, &zone);
    let next = job
        .next_fire
        .map(|at| format!("next {}", until(at, jiff::Timestamp::now())));
    let mut facts = Vec::new();
    if matches!(job.target, CronTarget::New(_)) {
        facts.push("a new session each run".to_owned());
    }
    if job.runs > 0 {
        facts.push(format!("{} runs so far", job.runs));
    }
    facts.push(format!("by {}", actor_label(&job.created_by)));
    let enabled = job.enabled;
    let items = {
        let job = job.clone();
        move || {
            let (ran, edited, asked) = (job.id.clone(), job.clone(), job.id.clone());
            vec![
                MenuItem {
                    id: "run",
                    label: "Run now",
                    icon: icondata::LuPlay,
                    act: Callback::new(move |()| workspace.run_cron(ran.clone())),
                    danger: false,
                },
                MenuItem {
                    id: "edit",
                    label: "Edit",
                    icon: icondata::LuPencil,
                    act: Callback::new(move |()| {
                        workspace.open_view(View::Cron, None);
                        workspace.edit_cron(edited.clone());
                    }),
                    danger: false,
                },
                MenuItem {
                    id: "delete",
                    label: "Delete…",
                    icon: icondata::LuTrash2,
                    act: Callback::new(move |()| confirm.set(Some(asked.clone()))),
                    danger: true,
                },
            ]
        }
    };
    let asking = {
        let id = id.clone();
        move || confirm.with(|asked| asked.as_ref() == Some(&id))
    };
    let (switched, deleted) = (id.clone(), (id.clone(), title.clone()));
    view! {
        <div class="flex flex-col gap-2 rounded-xl bg-raised p-2 pl-3 shadow-[0_0_0_1px_var(--color-line)]" data-session-job=id.to_string()>
            <div class="flex items-start gap-2">
                <span class="mt-1.5 inline-flex h-7 w-7 shrink-0 items-center justify-center rounded-lg bg-sunken text-fg-2">
                    <Glyph icon=icondata::LuClock />
                </span>
                <span class="flex min-w-0 flex-1 flex-col py-1">
                    <span class="truncate text-sm font-semibold">{title.clone()}</span>
                    <span class="text-[0.8125rem] text-fg-2">
                        {reading}
                        {next.map(|next| format!(" · {next}"))}
                    </span>
                    <span class=NOTE>{facts.join(" · ")}</span>
                </span>
                <button
                    type="button"
                    role="switch"
                    class="inline-flex min-h-11 min-w-11 shrink-0 items-center justify-center rounded-full lg:min-h-9"
                    aria-checked=enabled.to_string()
                    aria-label=if enabled { "Turn the job off" } else { "Turn the job on" }
                    data-job-switch
                    on:click=move |_| workspace.set_cron_enabled(switched.clone(), !enabled)
                >
                    <span class="effort-switch" class:effort-switch-on=enabled aria-hidden="true"></span>
                </button>
                {row_menu(menu, id.clone(), id.to_string(), title, MORE, items)}
            </div>
            <Show when=asking.clone()>
                {
                    let (id, title) = deleted.clone();
                    view! {
                        <div class=CONFIRM role="alertdialog" data-job-confirm>
                            <p class="text-sm">{format!("Delete {title}? The sessions it started stay.")}</p>
                            <div class="flex gap-2">
                                <button type="button" class=PRIMARY data-job-delete on:click=move |_| {
                                    confirm.set(None);
                                    workspace.delete_cron(id.clone(), title.clone());
                                }>
                                    "Delete"
                                </button>
                                <button type="button" class=BUTTON on:click=move |_| confirm.set(None)>"Keep it"</button>
                            </div>
                        </div>
                    }
                }
            </Show>
        </div>
    }
}

/// The Webhooks tab (D3): each webhook as the Webhooks view's row.
pub(super) fn webhooks_sheet(workspace: Workspace) -> impl IntoView {
    let confirm = workspace.webhook_confirm;
    let menu = RwSignal::new(None::<WebhookId>);
    let context = RwSignal::new(None::<(WebhookId, f64, f64)>);
    let body = move || {
        let Some(session) = open_session(workspace) else {
            return view! { <p class=NOTE>"Open a session to see its webhooks."</p> }.into_any();
        };
        let SessionGroups {
            into,
            made_here,
            started_by,
        } = workspace
            .webhooks
            .with(|_| session_webhooks(&session, &workspace.webhook_list()));
        let empty = into.is_empty() && made_here.is_empty() && started_by.is_none();
        let row =
            move |webhook| super::webhooks::webhook_row(workspace, webhook, confirm, menu, context);
        view! {
            {empty.then(|| view! {
                <p class=NOTE data-session-webhooks-empty>"No webhook delivers into this session, and none was made here."</p>
            })}
            {group("Delivers into this session", into, row)}
            {group("Made here, starting sessions of their own", made_here, row)}
            {group("Started this session", started_by.into_iter().collect(), row)}
        }
        .into_any()
    };
    view! {
        <div class="flex flex-col gap-2" data-session-webhooks>
            {body}
            <div class="flex flex-wrap gap-2 pt-1">
                <button type="button" class=PRIMARY data-session-add-webhook on:click=move |_| workspace.add_webhook()>
                    <Glyph icon=icondata::LuPlus />
                    "Add a webhook"
                </button>
                <button type="button" class=BUTTON on:click=move |_| workspace.open_view(View::Webhooks, None)>
                    "Open Webhooks"
                </button>
            </div>
        </div>
    }
}
